//! Media types flowing through the capture → encode → publish pipeline.

use std::time::Instant;

use bytes::Bytes;

/// Frame dimensions in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Dimensions {
  pub width: u32,
  pub height: u32,
}

impl Dimensions {
  /// Largest frame size the H.264 encoder accepts.
  pub const MAX_ENCODABLE: Self = Self::new(3840, 2160);

  #[must_use]
  pub const fn new(width: u32, height: u32) -> Self {
    Self { width, height }
  }

  #[must_use]
  pub const fn is_empty(self) -> bool {
    self.width == 0 || self.height == 0
  }
}

impl std::fmt::Display for Dimensions {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}x{}", self.width, self.height)
  }
}

/// Pixel layout of a [`RawFrame`] buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
  /// 8-bit blue, green, red, alpha, packed little-endian per pixel.
  Bgra8,
}

/// A raw, uncompressed frame as produced by a capture source.
#[derive(Clone, Debug)]
pub struct RawFrame {
  pub width: u32,
  pub height: u32,
  /// Row stride in bytes (may exceed `width * 4`).
  pub stride: u32,
  pub format: PixelFormat,
  pub pixels: Bytes,
  pub captured_at: Instant,
}

impl RawFrame {
  #[must_use]
  pub const fn dimensions(&self) -> Dimensions {
    Dimensions {
      width: self.width,
      height: self.height,
    }
  }
}
/// A single encoded H.264 access unit in Annex-B format.
#[derive(Clone, Debug)]
pub struct EncodedFrame {
  /// Annex-B byte stream for one frame; always starts with a start code.
  /// Keyframes include SPS/PPS ahead of the IDR slice.
  pub data: Bytes,
  /// True when the access unit starts with an IDR picture.
  pub keyframe: bool,
  /// Monotonic frame sequence number (encoder-local).
  pub sequence: u64,
  /// Capture time of the source frame (drives RTP timestamps).
  pub captured_at: Instant,
}

/// Sample rate of the Lumen audio pipeline (Opus' native rate; capture
/// delivers it directly, so no resampling happens).
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

/// Channel count of the Lumen audio pipeline.
pub const AUDIO_CHANNELS: u16 = 2;

/// Samples per channel in one Opus frame (20 ms at [`AUDIO_SAMPLE_RATE`]).
pub const OPUS_FRAME_SAMPLES: usize = 960;

/// A raw, uncompressed PCM buffer as produced by an audio capture source.
///
/// `samples` is interleaved little-endian `f32` in the range `[-1.0, 1.0]`
/// with [`AUDIO_CHANNELS`] channels at [`AUDIO_SAMPLE_RATE`].
#[derive(Clone, Debug)]
pub struct RawAudioFrame {
  pub samples: Bytes,
  /// Sample rate; must equal [`AUDIO_SAMPLE_RATE`] for the encoder.
  pub sample_rate: u32,
  /// Channel count; must equal [`AUDIO_CHANNELS`] for the encoder.
  pub channels: u16,
  pub captured_at: Instant,
}

/// A single encoded Opus packet (one 20 ms frame).
#[derive(Clone, Debug)]
pub struct EncodedAudioFrame {
  /// One complete Opus packet, ready for RTP payload.
  pub data: Bytes,
  /// Monotonic packet sequence number (encoder-local).
  pub sequence: u64,
  /// Capture time of the source buffer this packet closes.
  pub captured_at: Instant,
}

impl EncodedAudioFrame {
  #[must_use]
  pub fn new(data: Bytes, sequence: u64, captured_at: Instant) -> Self {
    Self {
      data,
      sequence,
      captured_at,
    }
  }
}

impl EncodedFrame {
  #[must_use]
  pub fn new(data: Bytes, keyframe: bool, sequence: u64, captured_at: Instant) -> Self {
    Self {
      data,
      keyframe,
      sequence,
      captured_at,
    }
  }
}
