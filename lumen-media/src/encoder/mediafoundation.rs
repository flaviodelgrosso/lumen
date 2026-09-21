//! Windows hardware H.264 encoding backed by `Media Foundation`.
//!
//! Only hardware-backed H.264 encoder MFTs are considered: enumeration
//! passes `MFT_ENUM_FLAG_HARDWARE`, so the built-in software Microsoft
//! encoder never satisfies this backend (a software MFT must not masquerade
//! as hardware). [`super::OpenH264Encoder`] remains the explicit software
//! fallback.
//!
//! Captured frames are BGRA in system memory; the encoder converts them to
//! `NV12` (the input subtype every hardware MFT accepts) on the CPU before
//! submission. The MFT is unlocked from its mandatory asynchronous model
//! (`MF_TRANSFORM_ASYNC_UNLOCK`) and driven strictly
//! one-frame-in/one-frame-out — like the `VideoToolbox` backend, each
//! `encode` drains its output before returning, so sequence numbers and
//! capture timestamps map 1:1 to inputs.
//!
//! Configuration follows the documented real-time contract: `NV12` input →
//! `H.264` output, `MF_LOW_LATENCY`, no B-frames (no reordering), GOP size
//! from the keyframe interval, and IDR forcing through the certified
//! `ICodecAPI` (`CODECAPI_AVEncVideoForceKeyFrame`), which every hardware
//! encoder MFT must implement. Encoders without `ICodecAPI` are rejected:
//! without it, keyframe requests could not be honored.
//!
//! Output access units are normalized into the pipeline's Annex-B contract
//! (see [`normalize_access_unit`]): start-code payloads pass through,
//! length-prefixed payloads are converted, and every IDR is guaranteed to
//! carry SPS/PPS ahead of the slice.

use std::cell::Cell;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr;
use std::slice;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use lumen_core::{Bitrate, Dimensions, EncodedFrame, RawFrame};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{
  CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
  CODECAPI_AVEncVideoForceKeyFrame, ICodecAPI, IMFActivate, IMFMediaBuffer, IMFMediaType,
  IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
  MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
  MF_MT_FIXED_SIZE_SAMPLES, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
  MF_MT_MAJOR_TYPE, MF_MT_MAX_KEYFRAME_SPACING, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE,
  MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType,
  MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup, MFT_ENUM_FLAG,
  MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
  MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
  MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
  MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_UI4};
use windows::core::{GUID, Interface as _};

use super::avcc::avcc_to_annex_b;
use super::{EncodeError, VideoEncoder, nal_type, nv12, split_annex_b};

/// `MFT_CATEGORY_VIDEO_ENCODER`
/// (`{f79eac7d-e545-4387-bdee-d647d7bde42a}`).
const MFT_CATEGORY_VIDEO_ENCODER: GUID = GUID::from_u128(0xf79eac7d_e545_4387_bdee_d647d7bde42a);

/// `REFERENCE_TIME` ticks per second (`Media Foundation` timestamps).
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Safety cap on `ProcessOutput` calls per input frame; a healthy
/// zero-latency encoder emits at most one access unit.
const MAX_DRAIN_PER_FRAME: usize = 8;

/// `windows-rs` conservatively leaves COM interfaces `!Send + !Sync`.
/// `Media Foundation` MFTs live in the COM MTA, where direct calls from
/// any COM-initialized thread are supported by design, and Lumen drives
/// the encoder from a single pipeline task; this newtype asserts the
/// auto-traits so the encoder can satisfy `VideoEncoder: Send`.
struct ThreadSafe<T>(T);

// SAFETY: the wrapped objects are MTA COM interfaces (the activation, the
// transform, the codec control, and the staging sample/buffer). MTA
// interfaces are free-threaded: any COM-initialized thread may call them
// without marshaling. The pipeline confines all encoder calls to one task
// at a time — even when the task migrates between runtime worker threads,
// every call is a complete, serialized COM invocation, and `ensure_com`
// guarantees the calling thread is COM-initialized.
#[expect(
  unsafe_code,
  reason = "audited auto-trait assertion for free-threaded MTA COM interfaces that windows-rs leaves !Send/!Sync"
)]
unsafe impl<T> Send for ThreadSafe<T> {}
// SAFETY: as above; shared references handed out by `Deref` are only used
// from the single task that owns the encoder.
#[expect(
  unsafe_code,
  reason = "audited auto-trait assertion for free-threaded MTA COM interfaces that windows-rs leaves !Send/!Sync"
)]
unsafe impl<T> Sync for ThreadSafe<T> {}

impl<T> std::ops::Deref for ThreadSafe<T> {
  type Target = T;
  fn deref(&self) -> &T {
    &self.0
  }
}

/// Process-wide `MFStartup`/`MFShutdown` refcount guard.
struct MediaFoundation;

impl MediaFoundation {
  #[expect(unsafe_code, reason = "MFStartup is a process-global COM call")]
  fn startup() -> Result<Self, EncodeError> {
    unsafe { MFStartup(MF_VERSION, 0) }
      .map_err(|err| EncodeError::Init(format!("MFStartup failed: {err}")))?;
    Ok(Self)
  }
}

impl Drop for MediaFoundation {
  #[expect(
    unsafe_code,
    reason = "MFShutdown decrements the process-global MFStartup refcount"
  )]
  fn drop(&mut self) {
    if let Err(err) = unsafe { MFShutdown() } {
      tracing::debug!("MFShutdown failed: {err}");
    }
  }
}

thread_local! {
  /// Whether COM has already been initialized on the current thread. The
  /// encoder task may run on different runtime worker threads over time,
  /// so initialization is tracked per thread and never torn down (worker
  /// threads live for the process lifetime).
  static COM_INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

/// Initialize COM (MTA) on the calling thread if it has not been yet.
#[expect(
  unsafe_code,
  reason = "CoInitializeEx is a per-thread COM call; repeated calls are reference-counted and guarded by the thread-local"
)]
fn ensure_com() -> Result<(), EncodeError> {
  COM_INITIALIZED.with(|initialized| {
    if initialized.get() {
      return Ok(());
    }
    let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // `S_OK`/`S_FALSE` mean initialized (freshly or already on this
    // thread); `RPC_E_CHANGED_MODE` means COM is up in another apartment
    // model, which still supports direct MTA-object calls.
    if status.is_ok() || status == RPC_E_CHANGED_MODE {
      initialized.set(true);
      Ok(())
    } else {
      Err(EncodeError::Init(format!(
        "CoInitializeEx failed for the encoder thread: {status}"
      )))
    }
  })
}

/// Build a `VARIANT` holding a `VT_UI4` scalar for `ICodecAPI::SetValue`.
fn ui4_variant(value: u32) -> VARIANT {
  VARIANT {
    Anonymous: VARIANT_0 {
      Anonymous: ManuallyDrop::new(VARIANT_0_0 {
        vt: VT_UI4,
        wReserved1: 0,
        wReserved2: 0,
        wReserved3: 0,
        Anonymous: VARIANT_0_0_0 { ulVal: value },
      }),
    },
  }
}

/// Reusable NV12 input sample with its backing memory buffer.
struct Staging {
  sample: IMFSample,
  buffer: IMFMediaBuffer,
}

impl Staging {
  #[expect(
    unsafe_code,
    reason = "MFCreateSample/MFCreateMemoryBuffer/AddBuffer are COM factory calls with valid local out-parameters"
  )]
  fn new(size: u32) -> Result<Self, EncodeError> {
    let buffer = unsafe { MFCreateMemoryBuffer(size) }
      .map_err(|err| EncodeError::Encode(format!("MFCreateMemoryBuffer failed: {err}")))?;
    let sample = unsafe { MFCreateSample() }
      .map_err(|err| EncodeError::Encode(format!("MFCreateSample failed: {err}")))?;
    unsafe { sample.AddBuffer(&buffer) }
      .map_err(|err| EncodeError::Encode(format!("IMFSample::AddBuffer failed: {err}")))?;
    Ok(Self { sample, buffer })
  }
}

/// Every candidate-MFT configuration failure means "this machine has no
/// usable hardware encoder", which is exactly what `hardware` mode must
/// surface.
fn unavailable(message: String) -> EncodeError {
  EncodeError::HardwareUnavailable(message)
}

/// A hardware H.264 encoder backed by a `Media Foundation` MFT.
pub struct MediaFoundationEncoder {
  /// The activated MFT; retained so `Drop` can shut the object down.
  activate: ThreadSafe<IMFActivate>,
  transform: ThreadSafe<IMFTransform>,
  /// Certified codec control (forced keyframes, GOP); `configure` rejects
  /// encoders that do not expose it, so this is always present.
  codec_api: ThreadSafe<ICodecAPI>,
  /// Reusable NV12 staging sample; `None` after a frame the MFT accepted
  /// without emitting output — the MFT may still hold the input buffer.
  staging: Option<ThreadSafe<Staging>>,
  dimensions: Dimensions,
  /// Timescale for presentation timestamps (one tick per frame).
  fps: u32,
  /// Capture timestamps of frames submitted but not yet emitted, FIFO.
  pending: VecDeque<Instant>,
  /// Monotonic sequence counter assigned at emission time.
  sequence: u64,
  /// Frames submitted (presentation timestamp ticks).
  frame_count: u64,
  keyframe_requested: bool,
  /// Cached Annex-B SPS+PPS from the most recent keyframe the encoder
  /// emitted in-band, so later IDRs can be made self-contained.
  param_sets: Option<Bytes>,
  /// Declared last so `MFShutdown` runs after the COM references drop.
  _startup: MediaFoundation,
}

impl MediaFoundationEncoder {
  /// Activate the first configurable hardware H.264 encoder MFT for a
  /// fixed capture size.
  ///
  /// # Errors
  ///
  /// See [`EncodeError`]. [`EncodeError::HardwareUnavailable`] means no
  /// hardware-backed encoder MFT could be activated — the software
  /// Microsoft encoder is deliberately not accepted.
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

    ensure_com()?;
    let startup = MediaFoundation::startup()?;
    let encoders = Self::enumerate_hardware_encoders()?;
    if encoders.is_empty() {
      return Err(EncodeError::HardwareUnavailable(
        "no hardware-backed H.264 encoder MFT is registered on this machine".to_owned(),
      ));
    }

    let mut last_error = String::from("no hardware-backed H.264 encoder MFT could be configured");
    for activate in encoders {
      match Self::configure(
        &activate,
        dimensions,
        fps,
        bitrate,
        keyframe_interval_frames,
      ) {
        Ok((transform, codec_api)) => {
          return Ok(Self {
            activate: ThreadSafe(activate),
            transform: ThreadSafe(transform),
            codec_api: ThreadSafe(codec_api),
            staging: None,
            dimensions,
            fps,
            pending: VecDeque::new(),
            sequence: 0,
            frame_count: 0,
            // The first frame must be a self-contained IDR.
            keyframe_requested: true,
            param_sets: None,
            _startup: startup,
          });
        }
        Err(err) => {
          tracing::debug!("rejecting hardware encoder MFT: {err}");
          last_error = err.to_string();
        }
      }
    }
    Err(EncodeError::HardwareUnavailable(last_error))
  }

  /// Enumerate hardware video-encoder MFTs that accept `NV12` and produce
  /// `H.264`, stealing each returned activation out of the COM task
  /// allocator's array.
  #[expect(
    unsafe_code,
    reason = "MFTEnumEx fills a CoTaskMem array; every entry is moved out before the array itself is freed"
  )]
  fn enumerate_hardware_encoders() -> Result<Vec<IMFActivate>, EncodeError> {
    let input = MFT_REGISTER_TYPE_INFO {
      guidMajorType: MFMediaType_Video,
      guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
      guidMajorType: MFMediaType_Video,
      guidSubtype: MFVideoFormat_H264,
    };
    let mut array: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0_u32;
    unsafe {
      MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
        Some(&raw const input),
        Some(&raw const output),
        &raw mut array,
        &raw mut count,
      )
    }
    .map_err(|err| EncodeError::HardwareUnavailable(format!("MFTEnumEx failed: {err}")))?;
    if array.is_null() || count == 0 {
      return Ok(Vec::new());
    }
    let mut activates = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    unsafe {
      for index in 0..usize::try_from(count).unwrap_or(0) {
        if let Some(activate) = (*array.add(index)).take() {
          activates.push(activate);
        }
      }
      CoTaskMemFree(Some(array.cast_const().cast()));
    }
    Ok(activates)
  }

  /// Activate `activate` and drive it to a configured, streaming encoder.
  /// Any failure means this MFT is unsuitable and the next candidate is
  /// tried, so all errors are [`EncodeError::HardwareUnavailable`].
  #[expect(
    unsafe_code,
    reason = "COM activation and media-type configuration on a freshly activated MFT; every interface is a valid local and every attribute is a framework constant"
  )]
  fn configure(
    activate: &IMFActivate,
    dimensions: Dimensions,
    fps: u32,
    bitrate: Bitrate,
    keyframe_interval_frames: u32,
  ) -> Result<(IMFTransform, ICodecAPI), EncodeError> {
    let transform: IMFTransform = unsafe { activate.ActivateObject() }
      .map_err(|err| unavailable(format!("IMFActivate::ActivateObject failed: {err}")))?;

    // Hardware MFTs are registered as asynchronous; unlocking the async
    // model switches them to the synchronous ProcessInput/ProcessOutput
    // path the one-in/one-out driver needs.
    let attributes = unsafe { transform.GetAttributes() }
      .map_err(|err| unavailable(format!("IMFTransform::GetAttributes failed: {err}")))?;
    let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1;
    if is_async {
      unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }.map_err(|err| {
        unavailable(format!(
          "could not unlock the MFT async model for sync driving: {err}"
        ))
      })?;
    }
    if let Err(err) = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) } {
      tracing::debug!("MFT rejected the MF_LOW_LATENCY hint: {err}");
    }

    // The certified codec control is mandatory: without it, neither GOP
    // scheduling nor forced keyframes can be honored.
    let codec_api: ICodecAPI = transform
      .cast()
      .map_err(|err| unavailable(format!("MFT exposes no ICodecAPI: {err}")))?;
    let gop = keyframe_interval_frames.max(2);
    let gop_value = ui4_variant(gop);
    if let Err(err) = unsafe { codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &raw const gop_value) }
    {
      tracing::debug!("MFT rejected CODECAPI_AVEncMPVGOPSize: {err}");
    }
    // No B-frames: screen sharing must not reorder frames. The baseline
    // default is already zero; this pins the contract on hybrid MFTs.
    let no_b_frames = ui4_variant(0);
    if let Err(err) = unsafe {
      codec_api.SetValue(
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        &raw const no_b_frames,
      )
    } {
      tracing::debug!("MFT rejected CODECAPI_AVEncMPVDefaultBPictureCount: {err}");
    }

    Self::set_media_types(&transform, dimensions, fps, bitrate, gop)?;
    Ok((transform, codec_api))
  }

  /// Set the output (`H.264`) and input (`NV12`) media types — output
  /// first, per the encoder-MFT contract — verify the output stream
  /// provides its own samples, and move the transform into streaming.
  #[expect(
    unsafe_code,
    reason = "media-type attribute calls and MFT messages; every attribute is a framework constant and the media types are valid locals"
  )]
  fn set_media_types(
    transform: &IMFTransform,
    dimensions: Dimensions,
    fps: u32,
    bitrate: Bitrate,
    gop: u32,
  ) -> Result<(), EncodeError> {
    let (width, height) = (dimensions.width, dimensions.height);
    let interlace: u32 = u32::try_from(MFVideoInterlace_Progressive.0).unwrap_or(2);
    let output_type: IMFMediaType = unsafe { MFCreateMediaType() }
      .map_err(|err| unavailable(format!("MFCreateMediaType failed: {err}")))?;
    unsafe {
      output_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|err| unavailable(format!("output type MF_MT_MAJOR_TYPE failed: {err}")))?;
      output_type
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
        .map_err(|err| unavailable(format!("output type MF_MT_SUBTYPE failed: {err}")))?;
      output_type
        .SetUINT32(&MF_MT_AVG_BITRATE, bitrate.bps())
        .map_err(|err| unavailable(format!("output type MF_MT_AVG_BITRATE failed: {err}")))?;
      output_type
        .SetUINT64(
          &MF_MT_FRAME_SIZE,
          (u64::from(width) << 32) | u64::from(height),
        )
        .map_err(|err| unavailable(format!("output type MF_MT_FRAME_SIZE failed: {err}")))?;
      output_type
        .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)
        .map_err(|err| unavailable(format!("output type MF_MT_FRAME_RATE failed: {err}")))?;
      output_type
        .SetUINT32(&MF_MT_INTERLACE_MODE, interlace)
        .map_err(|err| unavailable(format!("output type MF_MT_INTERLACE_MODE failed: {err}")))?;
      output_type
        .SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, gop)
        .map_err(|err| {
          unavailable(format!(
            "output type MF_MT_MAX_KEYFRAME_SPACING failed: {err}"
          ))
        })?;
      transform
        .SetOutputType(0, &output_type, 0)
        .map_err(|err| unavailable(format!("SetOutputType failed: {err}")))?;

      let input_type: IMFMediaType = MFCreateMediaType()
        .map_err(|err| unavailable(format!("MFCreateMediaType failed: {err}")))?;
      input_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|err| unavailable(format!("input type MF_MT_MAJOR_TYPE failed: {err}")))?;
      input_type
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|err| unavailable(format!("input type MF_MT_SUBTYPE failed: {err}")))?;
      input_type
        .SetUINT64(
          &MF_MT_FRAME_SIZE,
          (u64::from(width) << 32) | u64::from(height),
        )
        .map_err(|err| unavailable(format!("input type MF_MT_FRAME_SIZE failed: {err}")))?;
      input_type
        .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1)
        .map_err(|err| unavailable(format!("input type MF_MT_FRAME_RATE failed: {err}")))?;
      input_type
        .SetUINT32(&MF_MT_INTERLACE_MODE, interlace)
        .map_err(|err| unavailable(format!("input type MF_MT_INTERLACE_MODE failed: {err}")))?;
      let nv12_size = u32::try_from(nv12::buffer_size(
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
      ))
      .map_err(|_| unavailable(format!("frame size {dimensions} overflows NV12 sizing")))?;
      input_type
        .SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)
        .map_err(|err| unavailable(format!("input type MF_MT_FIXED_SIZE_SAMPLES failed: {err}")))?;
      input_type
        .SetUINT32(&MF_MT_SAMPLE_SIZE, nv12_size)
        .map_err(|err| unavailable(format!("input type MF_MT_SAMPLE_SIZE failed: {err}")))?;
      input_type
        .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
        .map_err(|err| unavailable(format!("input type MF_MT_DEFAULT_STRIDE failed: {err}")))?;
      transform
        .SetInputType(0, &input_type, 0)
        .map_err(|err| unavailable(format!("SetInputType failed: {err}")))?;

      // The one-in/one-out pump passes no output samples, so the MFT must
      // provide its own.
      let stream_info = transform
        .GetOutputStreamInfo(0)
        .map_err(|err| unavailable(format!("GetOutputStreamInfo failed: {err}")))?;
      let provides_samples = u32::try_from(MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0)
        .is_ok_and(|flag| stream_info.dwFlags & flag != 0);
      if !provides_samples {
        return Err(unavailable(
          "MFT expects caller-provided output samples; unsupported".to_owned(),
        ));
      }

      // BEGIN_STREAMING is optional per the MFT contract; START_OF_STREAM
      // is required before the first ProcessInput.
      if let Err(err) = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) {
        tracing::debug!("MFT rejected BEGIN_STREAMING: {err}");
      }
      transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .map_err(|err| unavailable(format!("START_OF_STREAM failed: {err}")))?;
    }
    Ok(())
  }

  /// Ask the encoder to make the next submitted frame an IDR.
  #[expect(
    unsafe_code,
    reason = "ICodecAPI::SetValue is a COM call; the VARIANT is a valid local scalar"
  )]
  fn force_keyframe(&self) {
    let value = ui4_variant(1);
    if let Err(err) = unsafe {
      self
        .codec_api
        .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &raw const value)
    } {
      tracing::warn!("forcing an IDR via CODECAPI_AVEncVideoForceKeyFrame failed: {err}");
    }
  }

  /// Pull every available encoded access unit into `outputs`.
  #[expect(
    unsafe_code,
    reason = "MFT ProcessOutput and the ManuallyDrop interface slots it fills; each interface is taken out exactly once and released"
  )]
  fn drain(&mut self, outputs: &mut Vec<(Bytes, bool)>) -> Result<(), EncodeError> {
    for _ in 0..MAX_DRAIN_PER_FRAME {
      let mut buffer = MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(None),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
      };
      let mut status = 0_u32;
      let result = unsafe {
        self
          .transform
          .ProcessOutput(0, slice::from_mut(&mut buffer), &raw mut status)
      };
      // SAFETY: the `ManuallyDrop` interface slots are taken out exactly
      // once so their COM references are released (the struct itself never
      // drops them).
      let sample = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
      drop(unsafe { ManuallyDrop::take(&mut buffer.pEvents) });

      if let Err(err) = result {
        if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
          return Ok(());
        }
        if err.code() == MF_E_TRANSFORM_STREAM_CHANGE {
          tracing::warn!("hardware encoder MFT requested a stream change; pending output skipped");
          return Ok(());
        }
        return Err(EncodeError::Encode(format!(
          "IMFTransform::ProcessOutput failed: {err}"
        )));
      }
      let Some(payload) = sample.as_ref().and_then(sample_payload) else {
        tracing::warn!("hardware encoder MFT emitted a sample without a readable payload");
        continue;
      };
      if let Some(normalized) = normalize_access_unit(&payload, &mut self.param_sets) {
        outputs.push(normalized);
      }
    }
    tracing::warn!(
      "hardware encoder MFT emitted more than {MAX_DRAIN_PER_FRAME} access units in one drain; stopping"
    );
    Ok(())
  }

  /// Convert the frame's BGRA pixels into the staging NV12 buffer.
  #[expect(
    unsafe_code,
    reason = "locked IMFMediaBuffer is a raw pointer; writes stay within the locked `size` bytes the buffer was created with"
  )]
  fn fill_staging(
    staging: &Staging,
    frame: &RawFrame,
    expected: Dimensions,
    size: usize,
  ) -> Result<(), EncodeError> {
    let mut pointer: *mut u8 = ptr::null_mut();
    unsafe {
      staging
        .buffer
        .Lock(&raw mut pointer, None, None)
        .map_err(|err| EncodeError::Encode(format!("IMFMediaBuffer::Lock failed: {err}")))?;
    }
    let result = (|| {
      if pointer.is_null() {
        return Err(EncodeError::Encode(
          "IMFMediaBuffer::Lock returned a null pointer".to_owned(),
        ));
      }
      // SAFETY: the buffer was created with exactly `size` bytes; Lock
      // hands out that whole region, writable, until Unlock.
      let dst = unsafe { slice::from_raw_parts_mut(pointer, size) };
      let (w, h) = (
        usize::try_from(frame.width).unwrap_or(0),
        usize::try_from(frame.height).unwrap_or(0),
      );
      let stride = usize::try_from(frame.stride).unwrap_or(0);
      if nv12::from_bgra(&frame.pixels, stride, w, h, dst) {
        Ok(())
      } else {
        Err(EncodeError::DimensionMismatch {
          expected,
          found: frame.dimensions(),
        })
      }
    })();
    if let Err(err) = unsafe { staging.buffer.Unlock() } {
      tracing::debug!("IMFMediaBuffer::Unlock failed: {err}");
    }
    result?;
    let size = u32::try_from(size)
      .map_err(|_| EncodeError::Encode("NV12 buffer overflows u32".to_owned()))?;
    unsafe {
      staging.buffer.SetCurrentLength(size).map_err(|err| {
        EncodeError::Encode(format!("IMFMediaBuffer::SetCurrentLength failed: {err}"))
      })
    }
  }
}

impl VideoEncoder for MediaFoundationEncoder {
  #[expect(
    unsafe_code,
    reason = "MFT ProcessInput and IMFSample timestamp calls; the staging sample is owned by this frame and the MFT serializes access"
  )]
  fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, EncodeError> {
    if frame.width != self.dimensions.width || frame.height != self.dimensions.height {
      return Err(EncodeError::DimensionMismatch {
        expected: self.dimensions,
        found: frame.dimensions(),
      });
    }
    ensure_com()?;

    let (w, h) = (
      usize::try_from(self.dimensions.width).unwrap_or(0),
      usize::try_from(self.dimensions.height).unwrap_or(0),
    );
    let nv12_size = nv12::buffer_size(w, h);
    let staging = if let Some(existing) = self.staging.take() {
      existing
    } else {
      let size = u32::try_from(nv12_size)
        .map_err(|_| EncodeError::UnsupportedDimensions(self.dimensions))?;
      ThreadSafe(Staging::new(size)?)
    };
    Self::fill_staging(&staging, frame, self.dimensions, nv12_size)?;

    let ticks = TICKS_PER_SECOND / u64::from(self.fps);
    unsafe {
      staging
        .sample
        .SetSampleTime(i64::try_from(self.frame_count * ticks).unwrap_or(i64::MAX))
        .map_err(|err| EncodeError::Encode(format!("IMFSample::SetSampleTime failed: {err}")))?;
      staging
        .sample
        .SetSampleDuration(i64::try_from(ticks).unwrap_or(i64::MAX))
        .map_err(|err| {
          EncodeError::Encode(format!("IMFSample::SetSampleDuration failed: {err}"))
        })?;
    }

    if self.keyframe_requested {
      self.force_keyframe();
      self.keyframe_requested = false;
    }
    self.frame_count += 1;
    self.pending.push_back(frame.captured_at);

    let mut outputs: Vec<(Bytes, bool)> = Vec::new();
    let mut input_status = unsafe { self.transform.ProcessInput(0, &staging.sample, 0) };
    if input_status
      .as_ref()
      .err()
      .is_some_and(|err| err.code() == MF_E_NOTACCEPTING)
    {
      // The MFT's input queue is full: drain whatever it has already
      // produced, then hand it the frame once more.
      self.drain(&mut outputs)?;
      input_status = unsafe { self.transform.ProcessInput(0, &staging.sample, 0) };
    }
    if let Err(err) = input_status {
      // The frame will never be encoded: drop its timeline entry. The
      // rejected sample was not taken, so staging stays reusable.
      self.pending.pop_front();
      self.staging = Some(staging);
      return Err(EncodeError::Encode(format!(
        "IMFTransform::ProcessInput failed: {err}"
      )));
    }
    self.drain(&mut outputs)?;

    if outputs.is_empty() {
      // The MFT accepted the frame but emitted nothing: it may still hold
      // a reference to the input buffer, so discard the staging sample
      // instead of rewriting it under the encoder.
      self.pending.pop_front();
      return Ok(None);
    }
    if outputs.len() > 1 {
      tracing::warn!(
        "hardware encoder MFT emitted {} access units for one input frame; extra units dropped",
        outputs.len()
      );
    }
    self.staging = Some(staging);

    let (data, keyframe) = outputs.remove(0);
    let captured_at = self.pending.pop_front().unwrap_or_else(Instant::now);
    self.pending.clear();
    let sequence = self.sequence;
    self.sequence += 1;
    Ok(Some(EncodedFrame::new(
      data,
      keyframe,
      sequence,
      captured_at,
    )))
  }

  fn request_keyframe(&mut self) {
    self.keyframe_requested = true;
  }
}

impl Drop for MediaFoundationEncoder {
  #[expect(
    unsafe_code,
    reason = "IMFActivate::ShutdownObject releases the activated MFT instance; the interface references drop afterwards"
  )]
  fn drop(&mut self) {
    if let Err(err) = unsafe { self.activate.ShutdownObject() } {
      tracing::debug!("IMFActivate::ShutdownObject failed: {err}");
    }
  }
}

/// Copy one emitted sample's contiguous payload out of COM memory.
#[expect(
  unsafe_code,
  reason = "IMFMediaBuffer Lock/Unlock bracket; the pointer is valid only between them and the bytes are copied out immediately"
)]
fn sample_payload(sample: &IMFSample) -> Option<Vec<u8>> {
  let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
  let mut pointer: *mut u8 = ptr::null_mut();
  let mut length = 0_u32;
  if unsafe { buffer.Lock(&raw mut pointer, None, Some(&raw mut length)) }.is_err() {
    return None;
  }
  let payload = if pointer.is_null() || length == 0 {
    Vec::new()
  } else {
    // SAFETY: Lock hands out `length` readable bytes until Unlock.
    unsafe { slice::from_raw_parts(pointer, usize::try_from(length).unwrap_or(0)).to_vec() }
  };
  if let Err(err) = unsafe { buffer.Unlock() } {
    tracing::debug!("IMFMediaBuffer::Unlock failed: {err}");
  }
  (!payload.is_empty()).then_some(payload)
}

/// Whether a payload begins with an Annex-B start code (as opposed to a
/// length prefix).
fn starts_with_start_code(data: &[u8]) -> bool {
  data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

/// Rebuild `SPS + PPS` (with 4-byte start codes) from an Annex-B buffer,
/// or `None` when it contains no parameter sets.
fn param_sets_from_annex_b(data: &[u8]) -> Option<Bytes> {
  let mut out = Vec::new();
  for nal in split_annex_b(data) {
    if matches!(nal_type(nal), Some(7 | 8)) {
      let body = if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
      } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
      } else {
        nal
      };
      out.extend_from_slice(&[0, 0, 0, 1]);
      out.extend_from_slice(body);
    }
  }
  (!out.is_empty()).then(|| Bytes::from(out))
}

/// Normalize one encoder output payload into the pipeline's Annex-B
/// contract: convert length-prefixed (AVCC) payloads, classify the access
/// unit by its NAL types, and guarantee every IDR carries SPS/PPS ahead of
/// the slice (caching in-band parameter sets from keyframes the encoder
/// emits them on). Returns `(annex_b, keyframe)`, or `None` when the
/// payload is unusable.
fn normalize_access_unit(payload: &[u8], param_sets: &mut Option<Bytes>) -> Option<(Bytes, bool)> {
  if payload.is_empty() {
    return None;
  }
  let annex_b = if starts_with_start_code(payload) {
    Bytes::copy_from_slice(payload)
  } else {
    match avcc_to_annex_b(payload) {
      Ok(Some(bytes)) => bytes,
      Ok(None) => return None,
      Err(err) => {
        tracing::error!("dropping malformed hardware encoder sample: {err}");
        return None;
      }
    }
  };
  let keyframe = split_annex_b(&annex_b)
    .iter()
    .any(|&nal| nal_type(nal) == Some(5));
  if !keyframe {
    return Some((annex_b, false));
  }
  if let Some(fresh) = param_sets_from_annex_b(&annex_b) {
    // The encoder emitted its parameter sets in-band: refresh the cache
    // (a reconfiguration would show up here as new sets) and pass the
    // access unit through untouched.
    *param_sets = Some(fresh);
    return Some((annex_b, true));
  }
  let Some(cached) = param_sets.as_ref() else {
    tracing::error!("hardware encoder IDR carries no SPS/PPS and none is cached yet");
    return Some((annex_b, true));
  };
  let mut out = BytesMut::with_capacity(cached.len() + annex_b.len());
  out.extend_from_slice(cached);
  out.extend_from_slice(&annex_b);
  Some((out.freeze(), true))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::encoder::tests_util::gradient_frame;

  fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
      out.extend_from_slice(&[0, 0, 0, 1]);
      out.extend_from_slice(nal);
    }
    out
  }

  #[test]
  fn annex_b_passthrough_classifies_by_nal_type() {
    let mut param_sets = None;
    let idr = annex_b(&[&[0x67, 0x01], &[0x68, 0x02], &[0x65, 0x03]]);
    let (data, keyframe) =
      normalize_access_unit(&idr, &mut param_sets).expect("IDR must normalize");
    assert!(keyframe);
    assert_eq!(data.as_ref(), idr.as_slice(), "in-band AU passes through");
    assert!(
      param_sets.is_some(),
      "in-band parameter sets must be cached"
    );

    let inter = annex_b(&[&[0x41, 0x04]]);
    let (data, keyframe) =
      normalize_access_unit(&inter, &mut param_sets).expect("inter frame must normalize");
    assert!(!keyframe);
    assert_eq!(data.as_ref(), inter.as_slice());
  }

  #[test]
  fn idr_without_in_band_parameter_sets_gets_the_cached_pair() {
    let mut param_sets = Some(Bytes::from_static(&[
      0, 0, 0, 1, 0x67, 0x09, 0, 0, 0, 1, 0x68, 0x08,
    ]));
    let bare_idr = annex_b(&[&[0x65, 0x03]]);
    let (data, keyframe) =
      normalize_access_unit(&bare_idr, &mut param_sets).expect("bare IDR must normalize");
    assert!(keyframe);
    let types: Vec<u8> = split_annex_b(&data)
      .iter()
      .map(|&nal| nal_type(nal).expect("non-empty NAL"))
      .collect();
    assert_eq!(
      types,
      vec![7, 8, 5],
      "cached SPS/PPS must precede the IDR slice"
    );
  }

  #[test]
  fn length_prefixed_payloads_are_converted() {
    // Defensive path: a MFT emitting AVCC-style length-prefixed payloads.
    let mut payload = Vec::new();
    for nal in [[0x67_u8, 0x01], [0x65, 0x02]] {
      let len = u32::try_from(nal.len()).expect("test NALs fit in u32");
      payload.extend_from_slice(&len.to_be_bytes());
      payload.extend_from_slice(&nal);
    }
    let mut param_sets = None;
    let (data, keyframe) =
      normalize_access_unit(&payload, &mut param_sets).expect("AVCC must convert");
    assert!(keyframe);
    assert!(data.starts_with(&[0, 0, 0, 1]), "output must be Annex-B");
    let types: Vec<u8> = split_annex_b(&data)
      .iter()
      .map(|&nal| nal_type(nal).expect("non-empty NAL"))
      .collect();
    assert_eq!(types, vec![7, 5]);
  }

  #[test]
  fn malformed_payloads_are_dropped() {
    let mut param_sets = None;
    assert!(normalize_access_unit(&[], &mut param_sets).is_none());
    // A length prefix claiming more bytes than the payload holds.
    let bogus = vec![0xff, 0xff, 0xff, 0xff, 0x65];
    assert!(normalize_access_unit(&bogus, &mut param_sets).is_none());
  }

  #[test]
  fn param_set_extraction_keeps_only_sps_and_pps() {
    let mixed = annex_b(&[&[0x67, 0x01], &[0x68, 0x02], &[0x65, 0x03], &[0x67, 0x04]]);
    let sets = param_sets_from_annex_b(&mixed).expect("must find parameter sets");
    let types: Vec<u8> = split_annex_b(&sets)
      .iter()
      .map(|&nal| nal_type(nal).expect("non-empty NAL"))
      .collect();
    assert_eq!(types, vec![7, 8, 7]);
    assert!(param_sets_from_annex_b(&annex_b(&[&[0x41, 0x05]])).is_none());
  }

  #[test]
  fn initialization_is_clean_without_hardware_mft() {
    // On CI runners (and any GPU-less Windows machine) no hardware MFT is
    // registered; initialization must fail with HardwareUnavailable — never
    // a panic, never a silent software encoder. On machines with a GPU the
    // only acceptable success is the media-foundation encoder itself.
    let result = MediaFoundationEncoder::new(Dimensions::new(64, 64), 30, Bitrate(1_000_000), 30);
    match result {
      Ok(_) => tracing::info!("hardware encoder MFT activated on this machine"),
      Err(err) => assert!(
        matches!(err, EncodeError::HardwareUnavailable(_)),
        "non-hardware machines must report HardwareUnavailable, got {err}"
      ),
    }
  }

  #[test]
  fn unsupported_dimensions_are_rejected_before_com() {
    for bad in [
      Dimensions::new(0, 64),
      Dimensions::new(15, 64),
      Dimensions::new(64, 63),
      Dimensions::new(Dimensions::MAX_ENCODABLE.width + 2, 64),
    ] {
      let Err(err) = MediaFoundationEncoder::new(bad, 30, Bitrate(1_000_000), 30) else {
        panic!("{bad} must not be encodable");
      };
      assert!(
        matches!(err, EncodeError::UnsupportedDimensions(_)),
        "{err}"
      );
    }
  }

  #[test]
  fn dimension_mismatch_is_rejected() {
    let Ok(mut encoder) =
      MediaFoundationEncoder::new(Dimensions::new(64, 64), 30, Bitrate(1_000_000), 30)
    else {
      // No hardware MFT (CI): the mismatch check still runs first for any
      // configured encoder, so there is nothing to test here.
      return;
    };
    let frame = gradient_frame(32, 32);
    let err = encoder
      .encode(&frame)
      .expect_err("32x32 must not encode into a 64x64 encoder");
    assert!(
      matches!(err, EncodeError::DimensionMismatch { .. }),
      "{err}"
    );
  }
}
