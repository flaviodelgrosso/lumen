//! Frame conversion and encoding behind portable traits.
//!
//! Video: [`OpenH264Encoder`] wraps the bundled Cisco `OpenH264` library
//! (no system `FFmpeg`) and is the software baseline on every platform;
//! on macOS [`VideoToolboxEncoder`] wraps the native `VideoToolbox` H.264
//! encoder. [`create_video_encoder`] selects between them from an
//! [`lumen_core::EncoderPreference`] and reports the chosen backend.
//!
//! Audio: [`OpusAudioEncoder`] wraps `libopus` (bundled via the `opus`
//! crate's `bundled` feature). PCM arrives interleaved `f32` at 48 kHz, so
//! no resampling or conversion happens before encoding.
//!
//! All video backends normalize their output to the same contract: Annex-B
//! access units, correct `EncodedFrame.keyframe`, monotonic sequence values
//! and the source frame's capture timestamp.

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use lumen_core::{Bitrate, Dimensions, EncodedFrame, EncoderPreference, RawFrame};
use thiserror::Error;

pub mod audio;
mod avcc;
mod openh264;

#[cfg(target_os = "macos")]
mod videotoolbox;

pub use audio::{AudioEncoder, FakeAudioEncoder, OpusAudioEncoder};
pub use openh264::OpenH264Encoder;
#[cfg(target_os = "macos")]
pub use videotoolbox::VideoToolboxEncoder;

/// Backend name reported for the bundled Cisco `OpenH264` software encoder.
pub const BACKEND_OPENH264: &str = "openh264";
/// Backend name reported for the macOS `VideoToolbox` hardware encoder.
pub const BACKEND_VIDEOTOOLBOX: &str = "videotoolbox";

/// Errors from encoding.
#[derive(Debug, Error)]
pub enum EncodeError {
  /// The native encoder failed to initialize.
  #[error("failed to initialize H.264 encoder: {0}")]
  Init(String),
  /// Encoding a frame failed.
  #[error("H.264 encode failed: {0}")]
  Encode(String),
  /// A hardware encoder was required but is unavailable or failed to
  /// initialize.
  #[error("hardware H.264 encoder unavailable: {0}")]
  HardwareUnavailable(String),
  /// The frame size does not match the configured encoder size.
  #[error("frame {found} does not match configured encoder size {expected}")]
  DimensionMismatch {
    /// Configured size.
    expected: Dimensions,
    /// Observed frame size.
    found: Dimensions,
  },
  /// Capture dimensions are not encodable (too small or too large).
  #[error("display size {0} is not encodable (need 16x16..3840x2160)")]
  UnsupportedDimensions(Dimensions),
  /// The native Opus encoder failed to initialize.
  #[error("failed to initialize Opus encoder: {0}")]
  AudioInit(String),
  /// Opus encoding failed.
  #[error("Opus encode failed: {0}")]
  AudioEncode(String),
  /// The PCM buffer is not a whole number of interleaved stereo frames.
  #[error("audio buffer is malformed: {0}")]
  MalformedAudio(String),
  /// The audio format is not encodable (only 48 kHz stereo is accepted).
  #[error("audio format {sample_rate} Hz / {channels} ch is not encodable (need 48000 Hz / 2 ch)")]
  UnsupportedAudio {
    /// Observed sample rate.
    sample_rate: u32,
    /// Observed channel count.
    channels: u16,
  },
}

/// Encode raw frames into H.264 Annex-B access units.
pub trait VideoEncoder: Send {
  /// Encode one frame; returns the encoded access unit, or `None` when
  /// the encoder skipped the frame.
  ///
  /// # Errors
  ///
  /// See [`EncodeError`].
  fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, EncodeError>;

  /// Force the next encoded frame to be an IDR keyframe.
  fn request_keyframe(&mut self);
}

/// A chosen video encoder plus the backend name it should be reported as
/// (e.g. `openh264`, `videotoolbox`).
pub struct VideoEncoderSetup {
  /// The selected encoder.
  pub encoder: Box<dyn VideoEncoder>,
  /// Stable backend identifier for logs and the host dashboard.
  pub backend: &'static str,
}

impl core::fmt::Debug for VideoEncoderSetup {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_struct("VideoEncoderSetup")
      .field("encoder", &"Box<dyn VideoEncoder>")
      .field("backend", &self.backend)
      .finish()
  }
}

/// Create the video encoder selected by `preference`.
///
/// * `software` always uses `OpenH264`.
/// * `auto` prefers the platform-native hardware encoder and falls back to
///   `OpenH264` (with a warning) when it is unavailable or fails to
///   initialize.
/// * `hardware` requires the native encoder and never falls back: startup
///   fails with [`EncodeError::HardwareUnavailable`] when it cannot
///   initialize.
///
/// # Errors
///
/// See [`EncodeError`].
pub fn create_video_encoder(
  preference: EncoderPreference,
  dimensions: Dimensions,
  fps: u32,
  bitrate: Bitrate,
  keyframe_interval_frames: u32,
) -> Result<VideoEncoderSetup, EncodeError> {
  match preference {
    EncoderPreference::Software => {
      new_openh264_encoder(dimensions, fps, bitrate, keyframe_interval_frames)
    }
    EncoderPreference::Auto => {
      match new_hardware_encoder(dimensions, fps, bitrate, keyframe_interval_frames) {
        Ok(setup) => Ok(setup),
        Err(error) => {
          tracing::warn!(
            "hardware H.264 encoder unavailable ({error}); falling back to `OpenH264`"
          );
          new_openh264_encoder(dimensions, fps, bitrate, keyframe_interval_frames)
        }
      }
    }
    EncoderPreference::Hardware => {
      new_hardware_encoder(dimensions, fps, bitrate, keyframe_interval_frames)
    }
  }
}

fn new_openh264_encoder(
  dimensions: Dimensions,
  fps: u32,
  bitrate: Bitrate,
  keyframe_interval_frames: u32,
) -> Result<VideoEncoderSetup, EncodeError> {
  Ok(VideoEncoderSetup {
    encoder: Box::new(OpenH264Encoder::new(
      dimensions,
      fps,
      bitrate,
      keyframe_interval_frames,
    )?),
    backend: BACKEND_OPENH264,
  })
}

#[cfg(target_os = "macos")]
fn new_hardware_encoder(
  dimensions: Dimensions,
  fps: u32,
  bitrate: Bitrate,
  keyframe_interval_frames: u32,
) -> Result<VideoEncoderSetup, EncodeError> {
  Ok(VideoEncoderSetup {
    encoder: Box::new(VideoToolboxEncoder::new(
      dimensions,
      fps,
      bitrate,
      keyframe_interval_frames,
    )?),
    backend: BACKEND_VIDEOTOOLBOX,
  })
}

#[cfg(not(target_os = "macos"))]
fn new_hardware_encoder(
  dimensions: Dimensions,
  fps: u32,
  bitrate: Bitrate,
  keyframe_interval_frames: u32,
) -> Result<VideoEncoderSetup, EncodeError> {
  let _ = (dimensions, fps, bitrate, keyframe_interval_frames);
  // The Media Foundation backend (Windows) is a separate milestone; until
  // it exists there is no native hardware encoder on this platform.
  Err(EncodeError::HardwareUnavailable(
    "no native hardware H.264 encoder exists on this platform".to_owned(),
  ))
}

/// NAL type of an Annex-B NAL (skips 3- or 4-byte start codes).
pub(crate) fn nal_type(nal: &[u8]) -> Option<u8> {
  let first = if nal.starts_with(&[0, 0, 0, 1]) {
    nal.get(4)?
  } else if nal.starts_with(&[0, 0, 1]) {
    nal.get(3)?
  } else {
    nal.first()?
  };
  Some(*first & 0x1f)
}

/// Strip leading SPS/PPS NALs from an Annex-B buffer (used to avoid
/// duplicating our cached parameter sets).
pub(crate) fn annex_b_without_param_sets(data: &[u8]) -> Bytes {
  let mut out = Vec::with_capacity(data.len());
  for nal in split_annex_b(data) {
    match nal_type(nal) {
      Some(7 | 8) => {}
      _ => out.extend_from_slice(nal),
    }
  }
  Bytes::from(out)
}

/// Split an Annex-B stream into NAL slices including their start codes.
pub(crate) fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
  fn start_code_len(d: &[u8], i: usize) -> Option<usize> {
    if d.len() >= i + 4 && d[i..i + 4] == [0, 0, 0, 1] {
      Some(4)
    } else if d.len() >= i + 3 && d[i..i + 3] == [0, 0, 1] {
      Some(3)
    } else {
      None
    }
  }
  let mut starts = Vec::new();
  let mut i = 0;
  while i < data.len() {
    if let Some(len) = start_code_len(data, i) {
      starts.push(i);
      i += len;
    } else {
      i += 1;
    }
  }
  let mut nals: Vec<&[u8]> = starts
    .iter()
    .enumerate()
    .map(|(idx, &start)| {
      let end = starts.get(idx + 1).copied().unwrap_or(data.len());
      &data[start..end]
    })
    .collect();
  // Trim trailing zero padding from the final NAL.
  if let Some(last) = nals.last_mut() {
    let mut trimmed = *last;
    while trimmed.last() == Some(&0) {
      trimmed = &trimmed[..trimmed.len() - 1];
    }
    *last = trimmed;
  }
  nals
}

/// A trivial encoder for tests: echoes synthetic access units without a
/// native codec.
#[derive(Debug, Default)]
pub struct FakeVideoEncoder {
  sequence: u64,
  keyframes_every: u64,
  keyframe_requested: bool,
  ever_encoded: std::sync::Arc<AtomicBool>,
}

impl FakeVideoEncoder {
  /// Emit a keyframe every `n` frames (default every 4).
  #[must_use]
  pub fn with_keyframes_every(n: u64) -> Self {
    Self {
      keyframes_every: n.max(1),
      ..Self::default()
    }
  }

  /// Flag that flips once `encode` has been called at least once.
  #[must_use]
  pub fn ever_encoded(&self) -> std::sync::Arc<AtomicBool> {
    std::sync::Arc::clone(&self.ever_encoded)
  }
}

impl VideoEncoder for FakeVideoEncoder {
  fn encode(&mut self, _frame: &RawFrame) -> Result<Option<EncodedFrame>, EncodeError> {
    self.ever_encoded.store(true, Ordering::Relaxed);
    let keyframe = self.keyframe_requested || self.sequence % self.keyframes_every == 0;
    self.keyframe_requested = false;
    let seq = self.sequence;
    self.sequence += 1;
    // Minimal Annex-B: start code + fake NAL byte.
    let mut data = vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }];
    data.extend_from_slice(&seq.to_be_bytes());
    Ok(Some(EncodedFrame {
      data: Bytes::from(data),
      keyframe,
      captured_at: std::time::Instant::now(),
      sequence: seq,
    }))
  }

  fn request_keyframe(&mut self) {
    self.keyframe_requested = true;
  }
}

/// Shared test fixtures for the encoder submodules.
#[cfg(test)]
pub(crate) mod tests_util {
  use bytes::Bytes;
  use lumen_core::{PixelFormat, RawFrame};
  use std::time::Instant;

  #[expect(
    clippy::cast_possible_truncation,
    reason = "pattern values are reduced modulo 256"
  )]
  pub(crate) fn gradient_frame(width: u32, height: u32) -> RawFrame {
    let mut pixels =
      vec![0_u8; usize::try_from(width).unwrap_or(0) * usize::try_from(height).unwrap_or(0) * 4];
    for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
      px[0] = (i % 256) as u8;
      px[1] = (i / 7 % 256) as u8;
      px[2] = (i / 13 % 256) as u8;
      px[3] = 255;
    }
    RawFrame {
      width,
      height,
      stride: width * 4,
      format: PixelFormat::Bgra8,
      pixels: Bytes::from(pixels),
      captured_at: Instant::now(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::tests_util::gradient_frame;
  use super::*;
  use lumen_core::Bitrate;

  #[test]
  fn fake_encoder_flags_keyframes() {
    let mut enc = FakeVideoEncoder::with_keyframes_every(3);
    let f = gradient_frame(8, 8);
    assert!(enc.encode(&f).unwrap().unwrap().keyframe); // seq 0
    assert!(!enc.encode(&f).unwrap().unwrap().keyframe);
    assert!(!enc.encode(&f).unwrap().unwrap().keyframe);
    assert!(enc.encode(&f).unwrap().unwrap().keyframe); // seq 3
    enc.request_keyframe();
    assert!(enc.encode(&f).unwrap().unwrap().keyframe); // forced
  }

  #[test]
  fn annex_b_splitting_handles_3_and_4_byte_codes() {
    let mut data = vec![0, 0, 0, 1, 0x67, 1, 2, 3];
    data.extend_from_slice(&[0, 0, 1, 0x68, 4, 5]);
    let nals = split_annex_b(&data);
    assert_eq!(nals.len(), 2);
    assert_eq!(nal_type(nals[0]), Some(7));
    assert_eq!(nal_type(nals[1]), Some(8));
  }

  #[test]
  fn software_preference_always_selects_openh264() {
    let setup = create_video_encoder(
      EncoderPreference::Software,
      Dimensions::new(64, 64),
      30,
      Bitrate(1_000_000),
      30,
    )
    .expect("software encoder must always initialize");
    assert_eq!(setup.backend, BACKEND_OPENH264);
  }

  #[test]
  fn auto_always_yields_a_working_encoder() {
    // Auto must never fail: hardware when supported, `OpenH264` otherwise.
    let mut setup = create_video_encoder(
      EncoderPreference::Auto,
      Dimensions::new(64, 64),
      30,
      Bitrate(1_000_000),
      30,
    )
    .expect("auto selection must always succeed");
    assert!(matches!(
      setup.backend,
      BACKEND_OPENH264 | BACKEND_VIDEOTOOLBOX
    ));
    let frame = gradient_frame(64, 64);
    let first = setup
      .encoder
      .encode(&frame)
      .expect("encode")
      .expect("first frame output");
    assert!(first.keyframe, "first frame must be an IDR");
    assert!(first.data.starts_with(&[0, 0, 0, 1]));
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn hardware_preference_selects_videotoolbox_on_macos() {
    let setup = create_video_encoder(
      EncoderPreference::Hardware,
      Dimensions::new(64, 64),
      30,
      Bitrate(1_000_000),
      30,
    )
    .expect("`VideoToolbox` must initialize on macOS");
    assert_eq!(setup.backend, BACKEND_VIDEOTOOLBOX);
  }

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn hardware_preference_fails_clearly_without_native_encoder() {
    let err = create_video_encoder(
      EncoderPreference::Hardware,
      Dimensions::new(64, 64),
      30,
      Bitrate(1_000_000),
      30,
    )
    .expect_err("hardware must be required to fail on this platform");
    assert!(matches!(err, EncodeError::HardwareUnavailable(_)), "{err}");
  }

  #[test]
  fn auto_falls_back_on_unencodable_dimensions() {
    // Odd width fails every backend, so auto must surface the error only
    // after the fallback also fails — and never report a hardware backend.
    let err = create_video_encoder(
      EncoderPreference::Auto,
      Dimensions::new(63, 64),
      30,
      Bitrate(1_000_000),
      30,
    )
    .expect_err("odd width is not encodable");
    assert!(
      matches!(err, EncodeError::UnsupportedDimensions(_)),
      "{err}"
    );
  }
}
