//! macOS hardware H.264 encoding backed by `VideoToolbox`.
//!
//! A `VTCompressionSession` configured for real-time screen sharing
//! (hardware preferred, no frame reordering, constrained-baseline profile)
//! consumes BGRA `CVPixelBuffer`s. `VideoToolbox` emits AVCC access units,
//! which [`avcc`](super::avcc) normalizes into the pipeline's Annex-B
//! contract; SPS/PPS are cached from the sample's format description and
//! re-extracted whenever `VideoToolbox` reports a format change, so every
//! IDR is self-contained (`SPS + PPS + IDR`).
//!
//! The session is driven strictly one-frame-in/one-frame-out: each
//! `encode` completes its pending frames before returning, so output
//! order, sequence numbers and capture timestamps map 1:1 to inputs and
//! a forced keyframe surfaces on the very next frame.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use lumen_core::{Bitrate, Dimensions, EncodedFrame, RawFrame};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
  CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
  CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_H264,
};
use objc2_core_video::{
  CVBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
  CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
  kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
  VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
  kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
  kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
  kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
  kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
  kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
  kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
};

use super::avcc::{avcc_starts_with_idr, avcc_to_annex_b, idr_with_param_sets};
use super::{EncodeError, VideoEncoder};

/// objc2 conservatively leaves opaque, mutable CoreMedia/CoreVideo CF
/// types (`VTCompressionSession`, `CVBuffer`) `!Send + !Sync`. Apple
/// documents their APIs as thread-safe, and Lumen drives the encoder
/// from a single pipeline thread; this newtype asserts the auto-traits
/// so the encoder can satisfy `VideoEncoder: Send`.
struct ThreadSafe<T>(T);

// SAFETY: the wrapped objects are only ever the session and the staging
// pixel buffer. Apple documents `VTSession` calls as thread-safe, and
// the pipeline confines all encoder calls to one task; the output
// callback only touches the separate `Arc<Mutex<CallbackState>>`, never
// these objects (the pixel buffer is only read by `VideoToolbox` itself
// while the encoder is mid-call, which the single-threaded driver
// serializes).
#[expect(
  unsafe_code,
  reason = "audited auto-trait assertion for thread-safe Apple CF objects that objc2 leaves !Send/!Sync"
)]
unsafe impl<T> Send for ThreadSafe<T> {}
// SAFETY: as above; shared references handed out by `Deref` are only
// used from the single thread that owns the encoder.
#[expect(
  unsafe_code,
  reason = "audited auto-trait assertion for thread-safe Apple CF objects that objc2 leaves !Send/!Sync"
)]
unsafe impl<T> Sync for ThreadSafe<T> {}

impl<T> std::ops::Deref for ThreadSafe<T> {
  type Target = T;
  fn deref(&self) -> &T {
    &self.0
  }
}

/// Callback-state shared with the (possibly asynchronous) `VideoToolbox`
/// output callback via the session's `outputCallbackRefCon`.
#[derive(Default)]
struct CallbackState {
  /// Capture timestamps of frames submitted but not yet emitted, FIFO.
  pending: VecDeque<Instant>,
  /// Encoded outputs produced since the last drain, in emission order.
  outputs: Vec<EncodedFrame>,
  /// Monotonic sequence counter assigned at emission time.
  sequence: u64,
  /// Cached Annex-B SPS+PPS from the active format description.
  param_sets: Option<Bytes>,
  /// Format description the cached parameter sets belong to; a change
  /// (e.g. after a reconfiguration) re-extracts them.
  format_desc: Option<CFRetained<CMFormatDescription>>,
}

/// H.264 encoder backed by the native macOS `VideoToolbox` framework.
pub struct VideoToolboxEncoder {
  session: ThreadSafe<CFRetained<VTCompressionSession>>,
  /// Reusable BGRA staging buffer; pixels are copied in per frame.
  pixel_buffer: ThreadSafe<CFRetained<CVBuffer>>,
  dimensions: Dimensions,
  /// Timescale for presentation timestamps (one tick per frame).
  fps: u32,
  /// Refcon target for the output callback. The `Arc` allocation is
  /// address-stable, so the raw pointer handed to `VideoToolbox` stays
  /// valid regardless of where the encoder struct itself moves.
  state: Arc<Mutex<CallbackState>>,
  sequence: u64,
  /// Frames submitted (presentation timestamp ticks).
  frame_count: u64,
  keyframe_requested: bool,
}

impl VideoToolboxEncoder {
  /// Create a `VideoToolbox` session for a fixed capture size.
  ///
  /// # Errors
  ///
  /// See [`EncodeError`].
  pub fn new(
    dimensions: Dimensions,
    fps: u32,
    bitrate: Bitrate,
    keyframe_interval_frames: u32,
  ) -> Result<Self, EncodeError> {
    let (width, height) = (dimensions.width, dimensions.height);
    if width < 16
      || height < 16
      || width > Dimensions::MAX_ENCODABLE.width
      || height > Dimensions::MAX_ENCODABLE.height
      || width % 2 != 0
      || height % 2 != 0
    {
      return Err(EncodeError::UnsupportedDimensions(dimensions));
    }
    let fps = fps.clamp(1, 240);

    let state = Arc::new(Mutex::new(CallbackState::default()));
    let session = Self::create_session(dimensions, &state)?;
    Self::configure_session(
      &session,
      fps,
      bitrate,
      u64::from(keyframe_interval_frames.max(2)),
    );
    let pixel_buffer = Self::create_pixel_buffer(dimensions)?;

    Ok(Self {
      session: ThreadSafe(session),
      pixel_buffer: ThreadSafe(pixel_buffer),
      dimensions,
      fps,
      state,
      sequence: 0,
      frame_count: 0,
      keyframe_requested: true, // first frame must be an IDR
    })
  }

  #[expect(
    unsafe_code,
    reason = "raw C session creation: the callback is a plain extern fn, the refcon is the address-stable Arc allocation (kept alive until invalidate() drains callbacks in Drop), and the out-pointer is a valid local"
  )]
  #[expect(
    clippy::cast_possible_wrap,
    reason = "dimensions are validated against MAX_ENCODABLE (≤ 3840), so the u32→i32 casts cannot wrap"
  )]
  fn create_session(
    dimensions: Dimensions,
    state: &Arc<Mutex<CallbackState>>,
  ) -> Result<CFRetained<VTCompressionSession>, EncodeError> {
    // Prefer the hardware encoder and low-latency rate control; do not
    // *require* hardware — where no hardware encoder exists `VideoToolbox`
    // still supplies its own (still `videotoolbox`) implementation.
    // Extern statics are raw pointers to the framework's CFStrings.
    let (enable_hardware, low_latency) = unsafe {
      (
        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
        kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
      )
    };
    let encoder_spec = CFDictionary::from_slices(
      &[enable_hardware, low_latency],
      &[CFBoolean::new(true), CFBoolean::new(true)],
    );

    let mut raw: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
      VTCompressionSession::create(
        None,
        dimensions.width as i32,
        dimensions.height as i32,
        kCMVideoCodecType_H264,
        Some(encoder_spec.as_ref()),
        None,
        None,
        Some(output_callback),
        Arc::as_ptr(state).cast::<c_void>().cast_mut(),
        NonNull::new(&raw mut raw)
          .ok_or_else(|| EncodeError::Init("null compression session out-pointer".to_owned()))?,
      )
    };
    if status != 0 {
      return Err(EncodeError::HardwareUnavailable(format!(
        "VTCompressionSessionCreate failed with status {status}"
      )));
    }
    // SAFETY: status == 0 guarantees a valid +1 session reference.
    let session = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("non-null by status")) };
    Ok(session)
  }

  #[expect(
    unsafe_code,
    reason = "VTSessionSetProperty is a raw C call; keys are the framework's own CFString constants and values their documented CFNumber/CFBoolean types"
  )]
  fn configure_session(
    session: &CFRetained<VTCompressionSession>,
    fps: u32,
    bitrate: Bitrate,
    keyframe_interval_frames: u64,
  ) {
    let set = |key: &CFString, value: &CFType| {
      let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
      if status != 0 {
        // Some encoders reject individual properties; the keyframe-flag
        // and forced-IDR paths keep the stream contract valid either
        // way, so surface it at debug level instead of failing startup.
        tracing::debug!("VTSessionSetProperty failed with status {status}");
      }
    };
    // Extern statics: raw references to the framework's CFString keys.
    set(
      unsafe { kVTCompressionPropertyKey_RealTime },
      CFBoolean::new(true),
    );
    set(
      unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
      CFBoolean::new(false),
    );
    set(
      unsafe { kVTCompressionPropertyKey_AverageBitRate },
      &CFNumber::new_i64(i64::from(bitrate.bps())),
    );
    set(
      unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
      &CFNumber::new_f64(f64::from(fps)),
    );
    set(
      unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
      &CFNumber::new_i64(i64::try_from(keyframe_interval_frames).unwrap_or(i64::MAX)),
    );
    set(unsafe { kVTCompressionPropertyKey_ProfileLevel }, unsafe {
      kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel
    });
  }

  #[expect(
    unsafe_code,
    reason = "raw C pixel buffer creation; the out-pointer is a valid local and the BGRA fourcc is a framework constant"
  )]
  fn create_pixel_buffer(dimensions: Dimensions) -> Result<CFRetained<CVBuffer>, EncodeError> {
    let mut raw: *mut CVBuffer = ptr::null_mut();
    let status = unsafe {
      CVPixelBufferCreate(
        None,
        usize::try_from(dimensions.width).unwrap_or(0),
        usize::try_from(dimensions.height).unwrap_or(0),
        kCVPixelFormatType_32BGRA,
        None,
        NonNull::new(&raw mut raw)
          .ok_or_else(|| EncodeError::Init("null pixel buffer out-pointer".to_owned()))?,
      )
    };
    if status != 0 {
      return Err(EncodeError::Init(format!(
        "CVPixelBufferCreate failed with status {status}"
      )));
    }
    // SAFETY: status == 0 guarantees a valid +1 buffer reference.
    let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("non-null by status")) };
    Ok(buffer)
  }

  /// Copy the frame's BGRA rows into the staging pixel buffer, honoring
  /// both the source capture stride and the buffer's row alignment.
  #[expect(
    unsafe_code,
    reason = "locked CVPixelBuffer base address is a raw pointer; every write stays within the locked mapping of `height` rows of `bytes_per_row`"
  )]
  fn copy_pixels(&mut self, frame: &RawFrame) -> Result<(), EncodeError> {
    let (w, h) = (
      usize::try_from(self.dimensions.width).unwrap_or(0),
      usize::try_from(self.dimensions.height).unwrap_or(0),
    );
    let row_bytes = w * 4;
    let lock_status =
      unsafe { CVPixelBufferLockBaseAddress(&self.pixel_buffer, CVPixelBufferLockFlags::empty()) };
    if lock_status != 0 {
      return Err(EncodeError::Encode(format!(
        "CVPixelBufferLockBaseAddress failed with status {lock_status}"
      )));
    }
    let result = (|| {
      let dst = CVPixelBufferGetBaseAddress(&self.pixel_buffer);
      let dst_stride = CVPixelBufferGetBytesPerRow(&self.pixel_buffer);
      if dst.is_null() || dst_stride < row_bytes {
        return Err(EncodeError::Encode(
          "locked pixel buffer has no usable base address".to_owned(),
        ));
      }
      let src_stride = usize::try_from(frame.stride).unwrap_or(0).max(row_bytes);
      if frame.pixels.len() < (h - 1) * src_stride + row_bytes {
        return Err(EncodeError::DimensionMismatch {
          expected: self.dimensions,
          found: frame.dimensions(),
        });
      }
      // SAFETY: the buffer is locked for `h` rows of `dst_stride` bytes;
      // each copy writes exactly `row_bytes` (≤ `dst_stride`) at the row
      // start, and the source bound is checked above.
      unsafe {
        let dst_base = dst.cast::<u8>();
        for row in 0..h {
          let src = frame.pixels.as_ptr().add(row * src_stride);
          ptr::copy_nonoverlapping(src, dst_base.add(row * dst_stride), row_bytes);
        }
      }
      Ok(())
    })();
    let unlock_status = unsafe {
      CVPixelBufferUnlockBaseAddress(&self.pixel_buffer, CVPixelBufferLockFlags::empty())
    };
    if unlock_status != 0 {
      tracing::warn!("CVPixelBufferUnlockBaseAddress failed with status {unlock_status}");
    }
    result
  }
}

impl VideoEncoder for VideoToolboxEncoder {
  fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, EncodeError> {
    if frame.width != self.dimensions.width || frame.height != self.dimensions.height {
      return Err(EncodeError::DimensionMismatch {
        expected: self.dimensions,
        found: frame.dimensions(),
      });
    }

    self.copy_pixels(frame)?;

    let force_keyframe = self.keyframe_requested;
    self.keyframe_requested = false;

    {
      let mut state = lock_state(&self.state);
      state.pending.push_back(frame.captured_at);
      state.outputs.clear();
    }

    #[expect(
      unsafe_code,
      reason = "reading the framework's ForceKeyFrame CFString extern static"
    )]
    let keyframe_props = force_keyframe.then(|| {
      let force = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
      CFDictionary::from_slices(&[force], &[CFBoolean::new(true)])
    });

    let status = unsafe_encode(
      &self.session,
      &self.pixel_buffer,
      i64::try_from(self.frame_count).unwrap_or(i64::MAX),
      i32::try_from(self.fps).unwrap_or(1),
      keyframe_props.as_ref().map(AsRef::as_ref),
    );
    self.frame_count += 1;
    if status != 0 {
      // The frame will never be emitted: drop its timeline entry.
      lock_state(&self.state).pending.pop_back();
      return Err(EncodeError::Encode(format!(
        "VTCompressionSessionEncodeFrame failed with status {status}"
      )));
    }

    // Drive the session to zero output latency: every pending frame
    // (always exactly this one) is emitted before returning.
    let status = unsafe_complete(&self.session.0);
    if status != 0 {
      tracing::debug!("VTCompressionSessionCompleteFrames failed with status {status}");
    }

    let mut state = lock_state(&self.state);
    if state.outputs.is_empty() {
      // Synchronously dropped frame (kVTEncodeInfo_FrameDropped) or a
      // failed encode callback: no access unit this round.
      state.pending.pop_front();
      return Ok(None);
    }
    if state.outputs.len() > 1 {
      tracing::warn!(
        "VideoToolbox emitted {} access units for one input frame; extra units dropped",
        state.outputs.len()
      );
    }
    let mut output = state.outputs.remove(0);
    state.pending.clear();
    output.sequence = self.sequence;
    self.sequence += 1;
    Ok(Some(output))
  }

  fn request_keyframe(&mut self) {
    self.keyframe_requested = true;
  }
}

impl Drop for VideoToolboxEncoder {
  #[expect(
    unsafe_code,
    reason = "deterministic session teardown; invalidate() blocks until all output callbacks have returned, so the Arc state is still alive here"
  )]
  fn drop(&mut self) {
    unsafe { self.session.0.invalidate() };
  }
}

#[expect(
  unsafe_code,
  reason = "raw C encode call; the pixel buffer is alive for the call, the frame-properties dictionary is owned by the caller, and the info-flags pointer is a valid local"
)]
fn unsafe_encode(
  session: &CFRetained<VTCompressionSession>,
  pixel_buffer: &CFRetained<CVBuffer>,
  frame_ticks: i64,
  fps: i32,
  frame_properties: Option<&CFDictionary>,
) -> i32 {
  let presentation_time_stamp = unsafe { CMTime::new(frame_ticks, fps) };
  let mut info_flags = VTEncodeInfoFlags::empty();
  unsafe {
    session.encode_frame(
      pixel_buffer,
      presentation_time_stamp,
      kCMTimeInvalid,
      frame_properties,
      ptr::null_mut(),
      &raw mut info_flags,
    )
  }
}

#[expect(
  unsafe_code,
  reason = "raw C call with a non-numeric CMTime, documented to complete all pending frames"
)]
fn unsafe_complete(session: &CFRetained<VTCompressionSession>) -> i32 {
  unsafe { session.complete_frames(kCMTimeInvalid) }
}

fn lock_state(state: &Mutex<CallbackState>) -> MutexGuard<'_, CallbackState> {
  state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `VideoToolbox` output callback. May run on a `VideoToolbox` worker thread;
/// all cross-thread state flows through the `Arc<Mutex<..>>` refcon.
#[expect(
  unsafe_code,
  reason = "C callback boundary: the refcon is the address-stable Arc allocation owned by the encoder and alive for the whole session lifetime; the sample buffer pointer is valid for the call"
)]
unsafe extern "C-unwind" fn output_callback(
  output_callback_ref_con: *mut c_void,
  _source_frame_ref_con: *mut c_void,
  output_status: i32,
  _output_flags: VTEncodeInfoFlags,
  video_data_sample_buffer: *mut CMSampleBuffer,
) {
  if output_callback_ref_con.is_null() {
    return;
  }
  // SAFETY: the refcon was created with `Arc::as_ptr` on the encoder's
  // `Arc<Mutex<CallbackState>>` — a pointer to the mutex inside the
  // address-stable Arc allocation. The encoder (and thus the Arc)
  // outlives every callback because `invalidate()` on Drop drains
  // pending callbacks before the state is released.
  let state_mutex = unsafe { &*output_callback_ref_con.cast::<Mutex<CallbackState>>() };
  let mut state = lock_state(state_mutex);

  if video_data_sample_buffer.is_null() || output_status != 0 {
    // End-of-stream marker or a failed frame: no sample to emit. The
    // encoder side reconciles the pending timeline after completion.
    return;
  }

  // SAFETY: `VideoToolbox` guarantees the sample buffer pointer is valid
  // and its data ready for the duration of the callback.
  let sample = unsafe { &*video_data_sample_buffer };
  let Some(avcc) = sample_payload(sample) else {
    state.pending.pop_front();
    return;
  };

  let keyframe = avcc_starts_with_idr(&avcc);
  if keyframe {
    refresh_param_sets(&mut state, sample);
  }
  let annex_b = match avcc_to_annex_b(&avcc) {
    Ok(Some(body)) => {
      if keyframe {
        // Guarantee every independently decodable IDR carries SPS+PPS.
        idr_with_param_sets(state.param_sets.as_ref(), body)
      } else {
        body
      }
    }
    Ok(None) => Bytes::new(),
    Err(error) => {
      tracing::error!("dropping malformed VideoToolbox sample: {error}");
      state.pending.pop_front();
      return;
    }
  };
  if annex_b.is_empty() {
    state.pending.pop_front();
    return;
  }

  let captured_at = state.pending.pop_front().unwrap_or_else(Instant::now);
  let sequence = state.sequence;
  state.sequence += 1;
  state
    .outputs
    .push(EncodedFrame::new(annex_b, keyframe, sequence, captured_at));
}

#[expect(
  unsafe_code,
  reason = "raw C block-buffer access; the block buffer is borrowed from the valid sample and copied out immediately"
)]
fn sample_payload(sample: &CMSampleBuffer) -> Option<Vec<u8>> {
  let block: CFRetained<CMBlockBuffer> = unsafe { sample.data_buffer() }?;
  let len = unsafe { block.data_length() };
  if len == 0 {
    return None;
  }
  let mut buf = vec![0_u8; len];
  let dst = NonNull::new(buf.as_mut_ptr().cast::<c_void>())?;
  let status = unsafe { block.copy_data_bytes(0, len, dst) };
  if status != 0 {
    tracing::error!("CMBlockBufferCopyDataBytes failed with status {status}");
    return None;
  }
  Some(buf)
}

/// Cache SPS/PPS from the sample's format description, re-extracting
/// when `VideoToolbox` reports a different (changed) description.
#[expect(
  unsafe_code,
  reason = "raw C parameter-set access; the format description is retained from the valid sample and the returned pointers stay valid for the call"
)]
fn refresh_param_sets(state: &mut CallbackState, sample: &CMSampleBuffer) {
  let Some(desc) = (unsafe { sample.format_description() }) else {
    if state.param_sets.is_none() {
      tracing::error!("VideoToolbox sample carries no format description; IDRs will lack SPS/PPS");
    }
    return;
  };

  let unchanged = state
    .format_desc
    .as_ref()
    .is_some_and(|cached| unsafe { CMFormatDescription::equal(Some(&desc), Some(cached)) });
  if unchanged {
    return;
  }

  let mut out = Vec::new();
  let mut index = 0_usize;
  loop {
    let mut nal_ptr: *const u8 = ptr::null();
    let mut nal_len = 0_usize;
    let mut nal_count = 0_usize;
    let mut nalu_length = 0_i32;
    let status = unsafe {
      CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        &desc,
        index,
        &raw mut nal_ptr,
        &raw mut nal_len,
        &raw mut nal_count,
        &raw mut nalu_length,
      )
    };
    if status != 0 || index >= nal_count {
      break;
    }
    // SAFETY: status == 0 guarantees `nal_ptr..nal_ptr + nal_len` is a
    // valid parameter-set NAL (without start code) for this call.
    let nal = unsafe { slice::from_raw_parts(nal_ptr, nal_len) };
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
    index += 1;
  }
  if out.is_empty() {
    tracing::error!("VideoToolbox format description exposed no H.264 parameter sets");
    return;
  }

  state.format_desc = Some(desc);
  state.param_sets = Some(Bytes::from(out));
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::encoder::nal_type;
  use crate::encoder::split_annex_b;
  use crate::encoder::tests_util::gradient_frame;

  fn encoder() -> VideoToolboxEncoder {
    VideoToolboxEncoder::new(Dimensions::new(64, 64), 30, Bitrate(1_000_000), 30)
      .expect("VideoToolbox must initialize on macOS")
  }

  /// Encode until an output arrives (defensive against rare drops).
  fn encode_output(enc: &mut VideoToolboxEncoder, frame: &RawFrame) -> EncodedFrame {
    for _ in 0..8 {
      if let Some(out) = enc.encode(frame).expect("encode") {
        return out;
      }
    }
    panic!("encoder produced no output after 8 attempts");
  }

  #[test]
  fn first_output_is_a_self_contained_annex_b_idr() {
    let mut enc = encoder();
    let frame = gradient_frame(64, 64);
    let out = encode_output(&mut enc, &frame);

    assert!(out.keyframe, "first frame must be an IDR");
    assert!(out.data.starts_with(&[0, 0, 0, 1]), "must be Annex-B");
    let types: Vec<u8> = split_annex_b(&out.data)
      .iter()
      .map(|nal| nal_type(nal).expect("non-empty NAL"))
      .collect();
    assert!(types.contains(&7), "IDR must carry SPS, got {types:?}");
    assert!(types.contains(&8), "IDR must carry PPS, got {types:?}");
    assert!(
      types.contains(&5),
      "IDR must carry an IDR slice, got {types:?}"
    );
    assert_eq!(types[0], 7, "SPS must come first");
    assert_eq!(types[1], 8, "PPS must follow the SPS");
  }

  #[test]
  fn forced_keyframe_is_emitted_on_the_next_frame() {
    let mut enc = encoder();
    let frame = gradient_frame(64, 64);
    let first = encode_output(&mut enc, &frame);
    assert!(first.keyframe);

    // One inter frame (may be omitted by the encoder; tolerate either).
    let _ = enc.encode(&frame);

    enc.request_keyframe();
    let forced = encode_output(&mut enc, &frame);
    assert!(forced.keyframe, "forced frame must be an IDR");
    let types: Vec<u8> = split_annex_b(&forced.data)
      .iter()
      .map(|nal| nal_type(nal).expect("non-empty NAL"))
      .collect();
    assert!(
      types.contains(&7) && types.contains(&8) && types.contains(&5),
      "forced IDR must be self-contained, got {types:?}"
    );
  }

  #[test]
  fn sequence_numbers_increase_by_one_per_output() {
    let mut enc = encoder();
    let frame = gradient_frame(64, 64);
    let mut sequences = Vec::new();
    for _ in 0..12 {
      if let Some(out) = enc.encode(&frame).expect("encode") {
        sequences.push(out.sequence);
      }
    }
    assert!(sequences.len() >= 10, "expected outputs, got {sequences:?}");
    for pair in sequences.windows(2) {
      assert_eq!(
        pair[1],
        pair[0] + 1,
        "sequence must be dense: {sequences:?}"
      );
    }
  }

  #[test]
  fn dimension_mismatch_is_rejected() {
    let mut enc = encoder();
    let frame = gradient_frame(32, 32);
    let err = enc
      .encode(&frame)
      .expect_err("32x32 must not encode into a 64x64 session");
    assert!(
      matches!(err, EncodeError::DimensionMismatch { .. }),
      "{err}"
    );
  }
}
