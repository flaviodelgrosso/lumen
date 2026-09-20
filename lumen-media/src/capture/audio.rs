//! System-audio capture behind a portable trait.
//!
//! The native backends open the platform's system-audio path and normalize
//! captured buffers to 48 kHz stereo interleaved `f32` — Opus' native rate
//! and format — via the resampler in [`crate::capture::resample`]:
//!
//! * macOS (13+): a dedicated audio-only `ScreenCaptureKit` stream through
//!   the `screencapturekit` crate.
//! * Windows: WASAPI loopback capture of the default render endpoint.
//!
//! Other platforms report [`CaptureError::AudioNotSupported`]. Tests use
//! [`FakeAudioCapture`] so they never need audio hardware.

use std::time::{Duration, Instant};

use bytes::Bytes;
use lumen_core::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, OPUS_FRAME_SAMPLES, RawAudioFrame};

use crate::capture::CaptureError;

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
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Backpressure budget for the capture → pipeline hop (~2 s of buffers);
/// beyond it the newest buffer is dropped — for live audio, latency beats
/// backlog.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
const QUEUE_BUFFERS: usize = 64;

#[cfg(target_os = "macos")]
mod macos {
  use std::sync::Arc;
  use std::sync::Mutex;
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::time::Instant;

  use bytes::Bytes;
  use lumen_core::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, RawAudioFrame};
  use screencapturekit::cm::CMSampleBuffer;
  use screencapturekit::prelude::*;

  use super::{POLL_INTERVAL, QUEUE_BUFFERS, deinterleave, interleave, mix_to_stereo};
  use crate::capture::resample::LaneResampler;
  use crate::capture::{CaptureError, FrameSender, SendOutcome, require_permission};

  /// `kLinearPCMFormatFlagIsPacked` / `kLinearPCMFormatFlagIsNonInterleaved`
  /// from `CoreAudio/AudioFormat.h`.
  const PCM_IS_PACKED: u32 = 1 << 3;
  const PCM_IS_NON_INTERLEAVED: u32 = 1 << 5;

  /// System-audio capture backed by a `ScreenCaptureKit` audio-only stream.
  pub struct PlatformAudioCapture {
    rx: std::sync::mpsc::Receiver<RawAudioFrame>,
    stream: SCStream,
    bridge: Arc<FrameSender<RawAudioFrame>>,
  }

  /// SCK → pipeline bridge; runs on the SCK dispatch queue.
  struct PcmOutput {
    bridge: Arc<FrameSender<RawAudioFrame>>,
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
    fn new(bridge: Arc<FrameSender<RawAudioFrame>>) -> Self {
      Self {
        bridge,
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
    fn sample_to_raw(&self, sample: &CMSampleBuffer) -> Option<RawAudioFrame> {
      let captured_at = Instant::now();
      let description = sample.format_description()?;
      if !description.is_pcm()
        || !description.audio_is_float()
        || description.audio_is_big_endian()
        || description
          .audio_format_flags()
          .is_none_or(|flags| flags & PCM_IS_PACKED == 0)
        || description.audio_bits_per_channel() != Some(32)
      {
        self.warn_format("not packed 32-bit float little-endian PCM");
        return None;
      }
      let rate = description.audio_sample_rate()?;
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
      let channels = usize::try_from(description.audio_channel_count()?).unwrap_or(0);
      if !(1..=8).contains(&channels) {
        self.warn_format("channel count out of range");
        return None;
      }

      let interleaved = description
        .audio_format_flags()
        .is_some_and(|flags| flags & PCM_IS_NON_INTERLEAVED == 0);
      let expected_frame_bytes = if interleaved {
        channels.checked_mul(std::mem::size_of::<f32>())?
      } else {
        std::mem::size_of::<f32>()
      };
      if description.audio_bytes_per_frame() != Some(expected_frame_bytes as u32) {
        self.warn_format("inconsistent bytes per frame");
        return None;
      }

      let list = sample.audio_buffer_list()?;
      let format = InputFormat {
        rate: in_rate,
        channels,
        interleaved,
      };
      let native = interleaved && channels == 2 && in_rate == AUDIO_SAMPLE_RATE;
      let mut state = self.state.lock().ok()?;
      state.prepare(format, !native);
      let samples = if native {
        let buffer = list.get(0)?;
        if buffer.data().len() % expected_frame_bytes != 0 {
          self.warn_format("partial native audio frame");
          return None;
        }
        buffer.data().to_vec()
      } else {
        let Some(planes) = decode_planes(&list, channels, interleaved) else {
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

  impl SCStreamOutputTrait for PcmOutput {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
      if !matches!(of_type, SCStreamOutputType::Audio) {
        return;
      }
      let Some(frame) = self.sample_to_raw(&sample) else {
        return;
      };
      match self.bridge.send(frame) {
        SendOutcome::Full => tracing::trace!("audio buffer dropped"),
        SendOutcome::Closed => tracing::trace!("audio buffer dropped (consumer gone)"),
        SendOutcome::Sent => {}
      }
    }
  }

  impl PlatformAudioCapture {
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
      require_permission()?;
      let content = SCShareableContent::get().map_err(|e| {
        tracing::debug!("SCShareableContent failed: {e}");
        CaptureError::PermissionDenied
      })?;
      let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or(CaptureError::NoDisplay)?;

      let filter = SCContentFilter::create().with_display(&display).build();
      let config = SCStreamConfiguration::new()
        .with_width(16)
        .with_height(16)
        .with_captures_audio(true)
        .with_sample_rate(i32::try_from(AUDIO_SAMPLE_RATE).unwrap_or(48_000))
        .with_channel_count(i32::from(AUDIO_CHANNELS));

      let (bridge, rx) = FrameSender::bounded(QUEUE_BUFFERS);
      let bridge = Arc::new(bridge);

      // SCK can stop the stream itself (device gone, permission revoked);
      // closing the buffer channel turns that into `CaptureError::Stopped`.
      let error_bridge = Arc::clone(&bridge);
      let mut stream = SCStream::new_with_delegate(
        &filter,
        &config,
        ErrorHandler::new(move |error| {
          tracing::warn!("ScreenCaptureKit audio stream stopped with error: {error}");
          error_bridge.close();
        }),
      );
      stream.add_output_handler(
        PcmOutput::new(Arc::clone(&bridge)),
        SCStreamOutputType::Audio,
      );
      stream.start_capture().map_err(|e| match e {
        SCError::PermissionDenied(_) | SCError::NoShareableContent(_) => {
          CaptureError::PermissionDenied
        }
        other => CaptureError::AudioStart(other.to_string()),
      })?;

      tracing::debug!("configured ScreenCaptureKit audio capture (48 kHz stereo output)");
      Ok(Self { rx, stream, bridge })
    }
  }

  impl Drop for PlatformAudioCapture {
    fn drop(&mut self) {
      if let Err(error) = self.stream.stop_capture() {
        tracing::warn!("failed to stop ScreenCaptureKit audio capture: {error}");
      }
      self.bridge.close();
    }
  }

  impl crate::capture::audio::AudioCaptureSource for PlatformAudioCapture {
    fn next_audio(&mut self) -> Result<Option<RawAudioFrame>, CaptureError> {
      match self.rx.recv_timeout(POLL_INTERVAL) {
        Ok(frame) => Ok(Some(frame)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(CaptureError::Stopped),
      }
    }
  }

  /// Decode the sample's buffers into one `f32` plane per channel.
  fn decode_planes(
    list: &screencapturekit::cm::AudioBufferList,
    channels: usize,
    interleaved: bool,
  ) -> Option<Vec<Vec<f32>>> {
    if interleaved {
      if list.num_buffers() != 1 {
        return None;
      }
      let buffer = list.get(0)?;
      return deinterleave(buffer.data(), channels);
    }
    if list.num_buffers() != channels {
      return None;
    }
    let buffers: Vec<&_> = (0..channels)
      .map(|index| list.get(index))
      .collect::<Option<_>>()?;
    if buffers.iter().any(|buffer| buffer.data().len() % 4 != 0) {
      return None;
    }
    let frames = buffers.first()?.data().len();
    if buffers.iter().any(|buffer| buffer.data().len() != frames) {
      return None;
    }
    Some(
      buffers
        .iter()
        .map(|buffer| bytes_to_f32(buffer.data()))
        .collect(),
    )
  }

  /// Decode LE `f32` bytes into samples.
  fn bytes_to_f32(data: &[u8]) -> Vec<f32> {
    data
      .chunks_exact(4)
      .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("4 bytes")))
      .collect()
  }
}

/// Split interleaved LE `f32` bytes into one `f32` plane per channel.
fn deinterleave(data: &[u8], channels: usize) -> Option<Vec<Vec<f32>>> {
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

/// Downmix any channel count to stereo: 2 passes through, 1 duplicates,
/// more averages even channels into L and odd into R (5.1 → stereo).
#[expect(
  clippy::cast_possible_truncation,
  reason = "the mean of at most 8 f32 samples; error is far below one \
                  audio LSB"
)]
fn mix_to_stereo(planes: &[Vec<f32>]) -> (Vec<f32>, Vec<f32>) {
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
fn interleave(left: &[f32], right: &[f32]) -> Vec<u8> {
  let count = left.len().min(right.len());
  let mut out = Vec::with_capacity(count * 8);
  for (l, r) in left.iter().zip(right).take(count) {
    out.extend_from_slice(&l.to_le_bytes());
    out.extend_from_slice(&r.to_le_bytes());
  }
  out
}

#[cfg(target_os = "macos")]
pub use macos::PlatformAudioCapture;

/// WASAPI loopback backend for Windows system-audio capture.
#[cfg(target_os = "windows")]
#[expect(
  unsafe_code,
  reason = "WASAPI is a COM API: every interface call is an unsafe vtable \
            dispatch, and capture buffers arrive as raw pointers"
)]
#[path = "wasapi.rs"]
mod wasapi;

#[cfg(target_os = "windows")]
pub use wasapi::PlatformAudioCapture;

/// System-audio capture on platforms without a native backend.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub struct PlatformAudioCapture;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl PlatformAudioCapture {
  /// Always fails with [`CaptureError::AudioNotSupported`].
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn new() -> Result<Self, CaptureError> {
    Err(CaptureError::AudioNotSupported)
  }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl AudioCaptureSource for PlatformAudioCapture {
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
  use super::*;

  /// On platforms without a system-audio backend, the video-only fallback
  /// in `lumen serve` depends on this exact error.
  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  #[test]
  fn unsupported_platform_reports_audio_not_supported() {
    let err = PlatformAudioCapture::new()
      .err()
      .expect("stub always fails");
    assert!(matches!(err, CaptureError::AudioNotSupported), "{err}");
  }

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

  #[cfg(any(target_os = "macos", target_os = "windows"))]
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

  #[cfg(any(target_os = "macos", target_os = "windows"))]
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

  #[cfg(any(target_os = "macos", target_os = "windows"))]
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
