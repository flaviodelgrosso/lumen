//! System-audio capture behind a portable trait.
//!
//! The production implementation opens a dedicated audio-only
//! `ScreenCaptureKit` stream (macOS 13+). The `screencapturekit` facade
//! crate is deliberately *not* used here: its output bridge calls
//! `get_frame_info()` on every sample buffer, which panics on audio buffers
//! (they carry no sample-attachment array), so this module drives the
//! `screencapturekit-sys` bindings directly.
//!
//! SCK delivers audio in the output device's format, so buffers are
//! normalized to 48 kHz stereo interleaved `f32` — Opus' native rate and
//! format — via the resampler in [`crate::resample`]. Other platforms
//! report [`CaptureError::AudioNotSupported`]. Tests use
//! [`FakeAudioCapture`] so they never need audio hardware.

use std::time::{Duration, Instant};

use bytes::Bytes;
use lumen_core::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, OPUS_FRAME_SAMPLES, RawAudioFrame};

use crate::CaptureError;

/// Abstraction over anything that yields raw PCM audio buffers.
///
/// `next_audio` blocks for at most [`POLL_INTERVAL`]; run it on a blocking
/// thread (see the audio task in `lumen-cli`) and re-check shutdown after
/// every `Ok(None)`.
pub trait AudioCaptureSource: Send {
  /// Poll for the next PCM buffer; `Ok(None)` means "nothing yet".
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  fn next_audio(&mut self) -> Result<Option<RawAudioFrame>, CaptureError>;
}

/// How long [`AudioCaptureSource::next_audio`] waits before reporting
/// "nothing yet"; keeps shutdown responsive without a wake-up channel.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Backpressure budget for the capture → pipeline hop (~2 s of buffers);
/// beyond it the newest buffer is dropped — for live audio, latency beats
/// backlog.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const QUEUE_BUFFERS: usize = 64;

#[cfg(target_os = "macos")]
mod macos {
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::time::Instant;

  use bytes::Bytes;
  use screencapturekit_sys::{
    cm_sample_buffer_ref::CMSampleBufferRef,
    content_filter::{UnsafeContentFilter, UnsafeInitParams},
    os_types::rc::Id,
    shareable_content::UnsafeSCShareableContent,
    stream::UnsafeSCStream,
    stream_configuration::{UnsafeStreamConfiguration, UnsafeStreamConfigurationRef},
    stream_error_handler::UnsafeSCStreamError,
    stream_output_handler::UnsafeSCStreamOutput,
  };

  use super::{
    AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, CaptureError, POLL_INTERVAL, QUEUE_BUFFERS, RawAudioFrame,
  };
  use crate::resample::LaneResampler;

  /// `SCStreamOutputTypeAudio` as the sys bindings spell it.
  const OUTPUT_TYPE_AUDIO: u8 = 1;

  /// System-audio capture backed by a `ScreenCaptureKit` audio-only stream.
  pub struct ScapAudioCapture {
    rx: std::sync::mpsc::Receiver<RawAudioFrame>,
    stream: Id<UnsafeSCStream>,
  }

  /// Logs stream errors; SCK surfaces no detail through the delegate.
  struct LoggingErrorHandler;

  impl UnsafeSCStreamError for LoggingErrorHandler {
    fn handle_error(&self) {
      tracing::warn!("ScreenCaptureKit audio stream reported an error");
    }
  }

  /// SCK → pipeline bridge; runs on the SCK dispatch queue.
  struct PcmOutput {
    tx: std::sync::mpsc::SyncSender<RawAudioFrame>,
    /// Resampler lanes; the SCK queue is concurrent, so the state is
    /// mutex-guarded.
    state: Mutex<PcmState>,
    /// Format problems are logged once, not per buffer.
    warned: AtomicBool,
  }

  #[derive(Clone, Copy, Eq, PartialEq)]
  struct InputFormat {
    rate: u32,
    channels: usize,
    interleaved: bool,
  }

  #[derive(Default)]
  struct PcmState {
    lanes: Option<(LaneResampler, LaneResampler)>,
    format: Option<InputFormat>,
  }

  impl PcmState {
    /// Reset the streaming filter whenever any part of the source format
    /// changes. Reusing its carry would otherwise replay samples from the
    /// old channel layout after a device switch.
    fn prepare(&mut self, format: InputFormat, needs_resampling: bool) {
      if self.format == Some(format) {
        return;
      }
      if let Some(previous) = self.format {
        if previous.rate != format.rate {
          tracing::warn!(
            "audio device rate changed {} → {}; resyncing",
            previous.rate,
            format.rate
          );
        }
      }
      self.lanes = needs_resampling.then(|| {
        (
          LaneResampler::new(format.rate, AUDIO_SAMPLE_RATE),
          LaneResampler::new(format.rate, AUDIO_SAMPLE_RATE),
        )
      });
      self.format = Some(format);
    }
  }

  impl PcmOutput {
    fn new(tx: std::sync::mpsc::SyncSender<RawAudioFrame>) -> Self {
      Self {
        tx,
        state: Mutex::new(PcmState::default()),
        warned: AtomicBool::new(false),
      }
    }

    fn warn_format(&self, reason: &str) {
      if !self.warned.swap(true, Ordering::Relaxed) {
        tracing::warn!("dropping unsupported system audio buffers ({reason})");
      }
    }

    #[expect(
      clippy::cast_possible_truncation,
      clippy::cast_sign_loss,
      reason = "the sample rate is range-validated (positive, in range) \
                      before the cast"
    )]
    fn sample_to_raw(&self, sample: &CMSampleBufferRef) -> Option<RawAudioFrame> {
      let captured_at = Instant::now();
      let description = sample.get_format_description()?;
      let Some(asbd) = description.audio_format_description_get_stream_basic_description() else {
        self.warn_format("no stream basic description");
        return None;
      };
      if asbd.format_id != screencapturekit_sys::cm_format_description_ref::kAudioFormatLinearPCM
        || asbd.format_flags
          & screencapturekit_sys::cm_format_description_ref::kLinearPCMFormatFlagIsFloat
          == 0
        || asbd.format_flags
          & screencapturekit_sys::cm_format_description_ref::kLinearPCMFormatFlagIsPacked
          == 0
        || asbd.format_flags
          & screencapturekit_sys::cm_format_description_ref::kLinearPCMFormatFlagIsBigEndian
          != 0
        || asbd.bits_per_channel != 32
      {
        self.warn_format("not packed 32-bit float little-endian PCM");
        return None;
      }
      let rate = asbd.sample_rate;
      if !(4_000.0..=384_000.0).contains(&rate) {
        self.warn_format("sample rate out of range");
        return None;
      }
      let rounded_rate = rate.round();
      if (rate - rounded_rate).abs() > f64::EPSILON * rate {
        self.warn_format("fractional sample rate");
        return None;
      }
      let in_rate = rounded_rate as u32;
      let channels = usize::try_from(asbd.channels_per_frame).unwrap_or(0);
      if !(1..=8).contains(&channels) {
        self.warn_format("channel count out of range");
        return None;
      }

      let interleaved = asbd.format_flags
        & screencapturekit_sys::cm_format_description_ref::kLinearPCMFormatFlagIsNonInterleaved
        == 0;
      let expected_frame_bytes = if interleaved {
        channels.checked_mul(std::mem::size_of::<f32>())?
      } else {
        std::mem::size_of::<f32>()
      };
      if usize::try_from(asbd.bytes_per_frame).ok() != Some(expected_frame_bytes) {
        self.warn_format("inconsistent bytes per frame");
        return None;
      }

      let buffers = sample.get_av_audio_buffer_list();
      let format = InputFormat {
        rate: in_rate,
        channels,
        interleaved,
      };
      let native = interleaved && channels == 2 && in_rate == AUDIO_SAMPLE_RATE;
      let mut state = self.state.lock().ok()?;
      state.prepare(format, !native);
      let samples = if native {
        let [buffer] = buffers.as_slice() else {
          self.warn_format("unexpected native audio buffer list");
          return None;
        };
        if buffer.data.len() % expected_frame_bytes != 0 {
          self.warn_format("partial native audio frame");
          return None;
        }
        buffer.data.clone()
      } else {
        let Some(planes) = decode_planes(&buffers, channels, interleaved) else {
          self.warn_format("inconsistent audio planes");
          return None;
        };
        let (left, right) = mix_to_stereo(&planes);
        let (left_lane, right_lane) = state.lanes.as_mut()?;
        let mut resampled_left = Vec::with_capacity(left.len());
        let mut resampled_right = Vec::with_capacity(right.len());
        left_lane.push(&left, &mut resampled_left);
        right_lane.push(&right, &mut resampled_right);
        interleave(&resampled_left, &resampled_right)
      };
      if samples.is_empty() {
        return None;
      }
      Some(RawAudioFrame {
        samples: Bytes::from(samples),
        sample_rate: AUDIO_SAMPLE_RATE,
        channels: AUDIO_CHANNELS,
        captured_at,
      })
    }
  }

  impl UnsafeSCStreamOutput for PcmOutput {
    fn did_output_sample_buffer(&self, sample: Id<CMSampleBufferRef>, of_type: u8) {
      if of_type != OUTPUT_TYPE_AUDIO {
        return;
      }
      let Some(frame) = self.sample_to_raw(&sample) else {
        return;
      };
      if self.tx.try_send(frame).is_err() {
        // Queue full (consumer stalled) or receiver gone: drop the
        // newest buffer rather than block the capture queue.
        tracing::trace!("audio buffer dropped");
      }
    }
  }

  impl ScapAudioCapture {
    /// Open an audio-only `ScreenCaptureKit` stream.
    ///
    /// The content filter pins a display (SCK requires one); system
    /// audio is global regardless of which display is picked. The
    /// configured size is a placeholder: no video output is registered,
    /// so the surface is never filled.
    ///
    /// # Errors
    ///
    /// See [`CaptureError`] (notably [`CaptureError::PermissionDenied`]
    /// and [`CaptureError::AudioStart`]).
    pub fn new() -> Result<Self, CaptureError> {
      crate::require_permission()?;
      let content = UnsafeSCShareableContent::get().map_err(|e| {
        tracing::debug!("SCShareableContent failed: {e}");
        CaptureError::PermissionDenied
      })?;
      let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or(CaptureError::NoDisplay)?;

      let filter = UnsafeContentFilter::init(UnsafeInitParams::Display(display));
      let config: Id<UnsafeStreamConfigurationRef> = UnsafeStreamConfiguration {
        width: 16,
        height: 16,
        captures_audio: 1,
        ..Default::default()
      }
      .into();

      let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_BUFFERS);
      let stream = UnsafeSCStream::init(filter, config, LoggingErrorHandler);
      stream.add_stream_output(PcmOutput::new(tx), OUTPUT_TYPE_AUDIO);
      stream.start_capture().map_err(CaptureError::AudioStart)?;

      tracing::debug!("configured ScreenCaptureKit audio capture (48 kHz stereo output)");
      Ok(Self { rx, stream })
    }
  }

  impl Drop for ScapAudioCapture {
    fn drop(&mut self) {
      // `objc_id::Id` releases the Objective-C object but does not run
      // the Rust `Drop` impl on the zero-sized sys wrapper.
      if let Err(error) = self.stream.stop_capture() {
        tracing::warn!("failed to stop ScreenCaptureKit audio capture: {error}");
      }
    }
  }

  impl crate::audio::AudioCaptureSource for ScapAudioCapture {
    fn next_audio(&mut self) -> Result<Option<RawAudioFrame>, CaptureError> {
      match self.rx.recv_timeout(POLL_INTERVAL) {
        Ok(frame) => Ok(Some(frame)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(CaptureError::Stopped),
      }
    }
  }

  /// Decode copied SCK buffers into one `f32` plane per channel.
  fn decode_planes(
    buffers: &[screencapturekit_sys::audio_buffer::CopiedAudioBuffer],
    channels: usize,
    interleaved: bool,
  ) -> Option<Vec<Vec<f32>>> {
    if interleaved {
      let [buffer] = buffers else {
        return None;
      };
      return deinterleave(&buffer.data, channels);
    }
    if buffers.len() != channels || buffers.iter().any(|buffer| buffer.data.len() % 4 != 0) {
      return None;
    }
    let frames = buffers.first()?.data.len();
    if buffers.iter().any(|buffer| buffer.data.len() != frames) {
      return None;
    }
    Some(
      buffers
        .iter()
        .map(|buffer| bytes_to_f32(&buffer.data))
        .collect(),
    )
  }

  /// Split interleaved LE `f32` bytes into one `f32` plane per channel.
  pub(super) fn deinterleave(data: &[u8], channels: usize) -> Option<Vec<Vec<f32>>> {
    let frame_bytes = channels.checked_mul(4)?;
    if frame_bytes == 0 || data.len() % frame_bytes != 0 {
      return None;
    }
    let frames = data.len() / frame_bytes;
    let mut planes = vec![Vec::with_capacity(frames); channels];
    for chunk in data.chunks_exact(frame_bytes) {
      for (plane, sample) in planes.iter_mut().zip(chunk.chunks_exact(4)) {
        plane.push(f32::from_le_bytes(sample.try_into().expect("4 bytes")));
      }
    }
    Some(planes)
  }

  /// Decode LE `f32` bytes into samples.
  fn bytes_to_f32(data: &[u8]) -> Vec<f32> {
    data
      .chunks_exact(4)
      .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("4 bytes")))
      .collect()
  }

  /// Downmix any channel count to stereo: 2 passes through, 1 duplicates,
  /// more averages even channels into L and odd into R (5.1 → stereo).
  #[expect(
    clippy::cast_possible_truncation,
    reason = "the mean of at most 8 f32 samples; error is far below one \
                  audio LSB"
  )]
  pub(super) fn mix_to_stereo(planes: &[Vec<f32>]) -> (Vec<f32>, Vec<f32>) {
    match planes.len() {
      0 => (Vec::new(), Vec::new()),
      1 => (planes[0].clone(), planes[0].clone()),
      2 => (planes[0].clone(), planes[1].clone()),
      _ => {
        let frames = planes.iter().map(Vec::len).min().unwrap_or(0);
        let mut left = vec![0.0_f32; frames];
        let mut right = vec![0.0_f32; frames];
        for (frame, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
          let mut sum_l = 0.0_f64;
          let mut sum_r = 0.0_f64;
          let mut count_l = 0.0_f64;
          let mut count_r = 0.0_f64;
          for (c, plane) in planes.iter().enumerate() {
            let value = f64::from(plane[frame]);
            if c % 2 == 0 {
              sum_l += value;
              count_l += 1.0;
            } else {
              sum_r += value;
              count_r += 1.0;
            }
          }
          *l = (sum_l / count_l) as f32;
          *r = (sum_r / count_r) as f32;
        }
        (left, right)
      }
    }
  }

  /// Interleave two equal-length sample slices into LE `f32` bytes.
  pub(super) fn interleave(left: &[f32], right: &[f32]) -> Vec<u8> {
    let count = left.len().min(right.len());
    let mut out = Vec::with_capacity(count * 8);
    for (l, r) in left.iter().zip(right).take(count) {
      out.extend_from_slice(&l.to_le_bytes());
      out.extend_from_slice(&r.to_le_bytes());
    }
    out
  }
}

#[cfg(target_os = "macos")]
pub use macos::ScapAudioCapture;

/// System-audio capture on platforms without a ScreenCaptureKit backend.
#[cfg(not(target_os = "macos"))]
pub struct ScapAudioCapture;

#[cfg(not(target_os = "macos"))]
impl ScapAudioCapture {
  /// Always fails with [`CaptureError::AudioNotSupported`].
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn new() -> Result<Self, CaptureError> {
    Err(CaptureError::AudioNotSupported)
  }
}

#[cfg(not(target_os = "macos"))]
impl AudioCaptureSource for ScapAudioCapture {
  fn next_audio(&mut self) -> Result<Option<RawAudioFrame>, CaptureError> {
    Err(CaptureError::AudioNotSupported)
  }
}

/// A synthetic audio source for tests and headless runs.
///
/// Emits a 20 ms chunk of a quiet 440 Hz sine every 20 ms.
#[derive(Debug, Default)]
pub struct FakeAudioCapture {
  phase: usize,
}

impl FakeAudioCapture {
  #[must_use]
  pub fn new() -> Self {
    Self::default()
  }
}

#[expect(
  clippy::cast_precision_loss,
  reason = "tone phase stays below 48 000, exact in f32"
)]
impl AudioCaptureSource for FakeAudioCapture {
  fn next_audio(&mut self) -> Result<Option<RawAudioFrame>, CaptureError> {
    std::thread::sleep(Duration::from_millis(20));
    let rate = AUDIO_SAMPLE_RATE as usize;
    let mut samples = Vec::with_capacity(OPUS_FRAME_SAMPLES * usize::from(AUDIO_CHANNELS) * 4);
    for i in 0..OPUS_FRAME_SAMPLES {
      let t = ((self.phase + i) % rate) as f32;
      let value = (2.0 * std::f32::consts::PI * 440.0 * t / AUDIO_SAMPLE_RATE as f32).sin() * 0.2;
      samples.extend_from_slice(&value.to_le_bytes());
      samples.extend_from_slice(&value.to_le_bytes());
    }
    self.phase = (self.phase + OPUS_FRAME_SAMPLES) % rate;
    Ok(Some(RawAudioFrame {
      samples: Bytes::from(samples),
      sample_rate: AUDIO_SAMPLE_RATE,
      channels: AUDIO_CHANNELS,
      captured_at: Instant::now(),
    }))
  }
}

#[cfg(test)]
mod tests {
  #[cfg(target_os = "macos")]
  use super::macos::*;
  use super::*;

  #[test]
  fn fake_source_emits_interleaved_frames() {
    let mut source = FakeAudioCapture::new();
    let frame = source
      .next_audio()
      .expect("fake source never fails")
      .expect("fake source always yields");
    assert_eq!(frame.sample_rate, AUDIO_SAMPLE_RATE);
    assert_eq!(frame.channels, AUDIO_CHANNELS);
    assert_eq!(
      frame.samples.len(),
      OPUS_FRAME_SAMPLES * usize::from(AUDIO_CHANNELS) * 4
    );
    // L == R for a mono tone: interleaving puts identical pairs side by side.
    let pair = frame.samples.chunks_exact(8).next().expect("one pair");
    assert_eq!(&pair[..4], &pair[4..]);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn deinterleave_splits_channels() {
    let interleaved: Vec<u8> = [1.0_f32, -1.0, 2.0, -2.0]
      .iter()
      .flat_map(|v| v.to_le_bytes())
      .collect();
    let planes = deinterleave(&interleaved, 2).expect("stereo");
    assert_eq!(planes[0], [1.0, 2.0]);
    assert_eq!(planes[1], [-1.0, -2.0]);
    assert!(deinterleave(&[0; 6], 2).is_none());
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn downmix_averages_surround_into_stereo() {
    let planes: Vec<Vec<f32>> = [[1.0_f32, 1.0], [0.0, 0.0], [1.0, 1.0], [0.0, 0.0]]
      .into_iter()
      .map(Vec::from)
      .collect();
    let (left, right) = mix_to_stereo(&planes);
    assert_eq!(left, [1.0, 1.0]);
    assert_eq!(right, [0.0, 0.0]);
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn interleave_roundtrips_deinterleave() {
    let interleaved: Vec<u8> = [3.0_f32, 4.0, 5.0, 6.0]
      .iter()
      .flat_map(|v| v.to_le_bytes())
      .collect();
    let planes = deinterleave(&interleaved, 2).expect("stereo");
    assert_eq!(interleave(&planes[0], &planes[1]), interleaved);
  }
}
