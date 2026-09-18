//! Frame conversion and encoding behind portable traits.
//!
//! Video: production implementation wraps [`openh264`] (bundled Cisco
//! `OpenH264`, no system `FFmpeg`). BGRA → I420 conversion uses the crate's
//! own `from_bgra8_source` path.
//!
//! Audio: production implementation wraps [`opus`] (libopus bundled and
//! built by the crate's `bundled` feature). PCM arrives interleaved `f32`
//! at 48 kHz, so no resampling or conversion happens before encoding.

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::{BufMut, Bytes, BytesMut};
use lumen_core::{
  AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, Bitrate, Dimensions, EncodedAudioFrame, EncodedFrame,
  OPUS_FRAME_SAMPLES, RawAudioFrame, RawFrame,
};
use openh264::OpenH264API;
use openh264::encoder::{
  BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
  SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use thiserror::Error;

/// Errors from encoding.
#[derive(Debug, Error)]
pub enum EncodeError {
  /// The native encoder failed to initialize.
  #[error("failed to initialize H.264 encoder: {0}")]
  Init(String),
  /// Encoding a frame failed.
  #[error("H.264 encode failed: {0}")]
  Encode(String),
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

/// H.264 encoder backed by the bundled Cisco `OpenH264` library.
pub struct OpenH264Encoder {
  encoder: Encoder,
  dimensions: Dimensions,
  yuv: YUVBuffer,
  packed: Vec<u8>,
  sequence: u64,
  /// Cached SPS/PPS NALs (Annex-B) from the first IDR.
  param_sets: Option<Bytes>,
  keyframe_requested: bool,
}

impl OpenH264Encoder {
  /// Create an encoder for a fixed capture size.
  ///
  /// # Errors
  ///
  /// See [`EncodeError`].
  #[expect(
    clippy::cast_precision_loss,
    reason = "fps is clamped to ≤ 240, exact in f32"
  )]
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

    let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate.bps()))
            .max_frame_rate(FrameRate::from_hz(fps.clamp(1, 240) as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                keyframe_interval_frames.max(2),
            ))
            // We manage frame dropping for latency ourselves; never let the
            // encoder silently skip input frames.
            .skip_frames(false);

    let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
      .map_err(|e| EncodeError::Init(e.to_string()))?;

    Ok(Self {
      encoder,
      dimensions,
      yuv: YUVBuffer::new(width as usize, height as usize),
      packed: Vec::new(),
      sequence: 0,
      param_sets: None,
      keyframe_requested: true, // first frame must be an IDR
    })
  }
}

impl VideoEncoder for OpenH264Encoder {
  fn encode(&mut self, frame: &RawFrame) -> Result<Option<EncodedFrame>, EncodeError> {
    if frame.width != self.dimensions.width || frame.height != self.dimensions.height {
      return Err(EncodeError::DimensionMismatch {
        expected: self.dimensions,
        found: frame.dimensions(),
      });
    }

    if self.keyframe_requested {
      self.encoder.force_intra_frame();
      self.keyframe_requested = false;
    }

    // Feed BGRA into the crate's conversion path, reusing buffers.
    let (w, h) = (
      self.dimensions.width as usize,
      self.dimensions.height as usize,
    );
    let need = w * h * 4;
    if usize::try_from(frame.stride).unwrap_or(0) == w * 4 {
      if frame.pixels.len() < need {
        return Err(EncodeError::DimensionMismatch {
          expected: self.dimensions,
          found: frame.dimensions(),
        });
      }
      self
        .yuv
        .read_bgra8(BgraSliceU8::new(&frame.pixels[..need], (w, h)));
    } else {
      // Stride-padded capture: repack to contiguous rows.
      self.packed.clear();
      self.packed.reserve(need);
      let stride = frame.stride as usize;
      for row in 0..h {
        let start = row * stride;
        let end = (start + w * 4).min(frame.pixels.len());
        if start >= end {
          return Err(EncodeError::DimensionMismatch {
            expected: self.dimensions,
            found: frame.dimensions(),
          });
        }
        self.packed.extend_from_slice(&frame.pixels[start..end]);
      }
      self.yuv.read_bgra8(BgraSliceU8::new(&self.packed, (w, h)));
    }

    let stream = self
      .encoder
      .encode(&self.yuv)
      .map_err(|e| EncodeError::Encode(e.to_string()))?;

    let frame_type = stream.frame_type();
    if frame_type == FrameType::Invalid || frame_type == FrameType::Skip {
      return Ok(None);
    }
    let keyframe = frame_type == FrameType::IDR;

    let mut buf = BytesMut::new();
    if keyframe {
      // Cache parameter sets from the first IDR; guarantee every later
      // IDR is self-contained even if the native encoder omits them.
      if self.param_sets.is_none() {
        self.param_sets = Some(extract_param_sets(&stream));
      }
      let ps = self.param_sets.clone().unwrap_or_default();
      let body = annex_b_without_param_sets(&stream.to_vec());
      buf.put(ps);
      buf.put(body);
    } else {
      buf.put(stream.to_vec().as_slice());
    }
    if buf.is_empty() {
      return Ok(None);
    }

    self.sequence += 1;
    Ok(Some(EncodedFrame {
      data: buf.freeze(),
      keyframe,
      sequence: self.sequence - 1,
      captured_at: frame.captured_at,
    }))
  }

  fn request_keyframe(&mut self) {
    self.keyframe_requested = true;
  }
}

/// Extract Annex-B SPS (type 7) + PPS (type 8) NALs from a stream.
fn extract_param_sets(stream: &openh264::encoder::EncodedBitStream<'_>) -> Bytes {
  let mut out = Vec::new();
  for l in 0..stream.num_layers() {
    let Some(layer) = stream.layer(l) else {
      continue;
    };
    for n in 0..layer.nal_count() {
      let Some(nal) = layer.nal_unit(n) else {
        continue;
      };
      if let Some(t) = nal_type(nal) {
        if t == 7 || t == 8 {
          out.extend_from_slice(nal);
        }
      }
    }
  }
  Bytes::from(out)
}

/// NAL type of an Annex-B NAL (skips 3- or 4-byte start codes).
fn nal_type(nal: &[u8]) -> Option<u8> {
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
fn annex_b_without_param_sets(data: &[u8]) -> Bytes {
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
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
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

/// Largest an Opus packet can be (RFC 6716); caps the encoder's output buffer.
const MAX_OPUS_PACKET: usize = 1275;

/// Encode interleaved `f32` PCM into Opus packets.
pub trait AudioEncoder: Send {
  /// Feed one PCM buffer; returns the Opus packets it completed (usually
  /// one; zero when the buffer did not close a 20 ms frame, more when it
  /// carried multiple frames).
  ///
  /// # Errors
  ///
  /// See [`EncodeError`].
  fn encode_audio(&mut self, frame: &RawAudioFrame) -> Result<Vec<EncodedAudioFrame>, EncodeError>;
}

/// Opus encoder backed by the bundled `libopus`.
///
/// Opus encodes fixed 20 ms frames; capture buffers arrive in arbitrary
/// chunk sizes, so the encoder owns the 20 ms framing/staging buffer.
pub struct OpusAudioEncoder {
  encoder: opus::Encoder,
  /// Interleaved `f32` samples not yet forming a full 20 ms frame.
  staging: Vec<f32>,
  sequence: u64,
}

impl OpusAudioEncoder {
  /// Create a 48 kHz stereo encoder at the given target bitrate.
  ///
  /// # Errors
  ///
  /// See [`EncodeError`].
  pub fn new(bitrate: Bitrate) -> Result<Self, EncodeError> {
    let bits = i32::try_from(bitrate.bps())
      .map_err(|_| EncodeError::AudioInit("bitrate out of range".to_owned()))?;
    let mut encoder = opus::Encoder::new(
      AUDIO_SAMPLE_RATE,
      opus::Channels::Stereo,
      opus::Application::Audio,
    )
    .map_err(|e| EncodeError::AudioInit(e.to_string()))?;
    // Constrained VBR: modulates quality but honors the target closely,
    // which keeps SRTP pacing steady without hard-CBR quality cliffs.
    for (result, name) in [
      (encoder.set_bitrate(opus::Bitrate::Bits(bits)), "bitrate"),
      (encoder.set_vbr(true), "vbr"),
      (encoder.set_vbr_constraint(true), "vbr-constraint"),
      (encoder.set_signal(opus::Signal::Music), "signal"),
    ] {
      result.map_err(|e| EncodeError::AudioInit(format!("set_{name}: {e}")))?;
    }
    Ok(Self {
      encoder,
      staging: Vec::with_capacity(2 * OPUS_FRAME_SAMPLES),
      sequence: 0,
    })
  }
}

impl AudioEncoder for OpusAudioEncoder {
  fn encode_audio(&mut self, frame: &RawAudioFrame) -> Result<Vec<EncodedAudioFrame>, EncodeError> {
    if frame.sample_rate != AUDIO_SAMPLE_RATE || frame.channels != AUDIO_CHANNELS {
      return Err(EncodeError::UnsupportedAudio {
        sample_rate: frame.sample_rate,
        channels: frame.channels,
      });
    }
    let frame_bytes = usize::from(AUDIO_CHANNELS) * size_of::<f32>();
    if frame.samples.len() % frame_bytes != 0 {
      return Err(EncodeError::MalformedAudio(format!(
        "{} B is not a whole number of stereo f32 frames",
        frame.samples.len()
      )));
    }
    self.staging.extend(
      frame
        .samples
        .chunks_exact(size_of::<f32>())
        .map(|b| f32::from_le_bytes(b.try_into().expect("chunk_exact(4)"))),
    );

    let need = OPUS_FRAME_SAMPLES * usize::from(AUDIO_CHANNELS);
    let mut packets = Vec::new();
    while self.staging.len() >= need {
      let data = self
        .encoder
        .encode_vec_float(&self.staging[..need], MAX_OPUS_PACKET)
        .map_err(|e| EncodeError::AudioEncode(e.to_string()))?;
      self.staging.drain(..need);
      packets.push(EncodedAudioFrame::new(
        Bytes::from(data),
        self.sequence,
        frame.captured_at,
      ));
      self.sequence += 1;
    }
    Ok(packets)
  }
}

/// A trivial audio encoder for tests: frames PCM and echoes synthetic
/// packets without a native codec.
#[derive(Debug, Default)]
pub struct FakeAudioEncoder {
  staged_pairs: usize,
  sequence: u64,
}

impl AudioEncoder for FakeAudioEncoder {
  fn encode_audio(&mut self, frame: &RawAudioFrame) -> Result<Vec<EncodedAudioFrame>, EncodeError> {
    self.staged_pairs += frame.samples.len() / (usize::from(AUDIO_CHANNELS) * 4);
    let mut packets = Vec::new();
    while self.staged_pairs >= OPUS_FRAME_SAMPLES {
      self.staged_pairs -= OPUS_FRAME_SAMPLES;
      // Minimal Opus-ish payload: a config byte + sequence tag.
      let mut data = vec![0xF8_u8];
      data.extend_from_slice(&self.sequence.to_be_bytes());
      packets.push(EncodedAudioFrame::new(
        Bytes::from(data),
        self.sequence,
        frame.captured_at,
      ));
      self.sequence += 1;
    }
    Ok(packets)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use lumen_core::PixelFormat;
  use std::time::Instant;

  #[expect(
    clippy::cast_possible_truncation,
    reason = "pattern values are reduced modulo 256"
  )]
  fn gradient_frame(width: u32, height: u32) -> RawFrame {
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
  fn openh264_encodes_idr_with_param_sets() {
    let dims = Dimensions::new(64, 64);
    let mut enc = OpenH264Encoder::new(dims, 30, Bitrate(1_000_000), 30).expect("encoder init");
    let frame = gradient_frame(64, 64);

    let first = enc
      .encode(&frame)
      .expect("first encode")
      .expect("first frame output");
    assert!(first.keyframe, "first frame must be IDR");
    assert!(first.data.starts_with(&[0, 0, 0, 1]));

    let types: Vec<u8> = split_annex_b(&first.data)
      .iter()
      .filter_map(|n| nal_type(n))
      .collect();
    assert!(types.contains(&7), "SPS missing in keyframe");
    assert!(types.contains(&8), "PPS missing in keyframe");
    assert!(types.contains(&5), "IDR slice missing");

    // P frame: no parameter sets.
    let second = enc
      .encode(&frame)
      .expect("second encode")
      .expect("second frame output");
    assert!(!second.keyframe);
    let p_types: Vec<u8> = split_annex_b(&second.data)
      .iter()
      .filter_map(|n| nal_type(n))
      .collect();
    assert!(!p_types.contains(&7));
    assert!(!p_types.contains(&8));
    assert!(p_types.contains(&1), "P slice missing");
  }

  #[test]
  fn openh264_forced_keyframe_contains_param_sets() {
    let mut enc = OpenH264Encoder::new(Dimensions::new(64, 64), 30, Bitrate(1_000_000), 300)
      .expect("encoder init");
    let frame = gradient_frame(64, 64);
    // Encode several P frames first.
    for _ in 0..5 {
      enc.encode(&frame).unwrap();
    }
    enc.request_keyframe();
    let kf = enc.encode(&frame).unwrap().expect("keyframe output");
    assert!(kf.keyframe);
    let types: Vec<u8> = split_annex_b(&kf.data)
      .iter()
      .filter_map(|n| nal_type(n))
      .collect();
    assert!(types.contains(&7) && types.contains(&8) && types.contains(&5));
    assert!(split_annex_b(&kf.data).len() >= 3);
  }

  #[test]
  fn dimension_mismatch_is_rejected() {
    let mut enc = OpenH264Encoder::new(Dimensions::new(64, 64), 30, Bitrate(1_000_000), 30)
      .expect("encoder init");
    let err = enc.encode(&gradient_frame(32, 32)).unwrap_err();
    assert!(matches!(err, EncodeError::DimensionMismatch { .. }));
  }

  #[test]
  fn odd_or_tiny_dimensions_rejected() {
    assert!(OpenH264Encoder::new(Dimensions::new(63, 64), 30, Bitrate(1_000_000), 30).is_err());
    assert!(OpenH264Encoder::new(Dimensions::new(8, 8), 30, Bitrate(1_000_000), 30).is_err());
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

  /// `n` stereo 48 kHz `f32` frames of silence as interleaved LE bytes.
  fn pcm_buffer(frames: usize) -> RawAudioFrame {
    RawAudioFrame {
      samples: Bytes::from(vec![0_u8; frames * usize::from(AUDIO_CHANNELS) * 4]),
      sample_rate: AUDIO_SAMPLE_RATE,
      channels: AUDIO_CHANNELS,
      captured_at: Instant::now(),
    }
  }

  #[test]
  fn opus_encoder_yields_one_packet_per_20ms() {
    let mut enc = OpusAudioEncoder::new(Bitrate(128_000)).expect("opus init");
    for i in 0..5_u64 {
      let packets = enc
        .encode_audio(&pcm_buffer(OPUS_FRAME_SAMPLES))
        .expect("encode");
      assert_eq!(packets.len(), 1, "a 20 ms buffer closes exactly one frame");
      assert_eq!(packets[0].sequence, i);
      assert!(!packets[0].data.is_empty());
    }
  }

  #[test]
  fn opus_encoder_stages_sub_frame_buffers() {
    let mut enc = OpusAudioEncoder::new(Bitrate(128_000)).expect("opus init");
    let half = OPUS_FRAME_SAMPLES / 2;
    assert!(
      enc
        .encode_audio(&pcm_buffer(half))
        .expect("10 ms")
        .is_empty()
    );
    assert_eq!(
      enc.encode_audio(&pcm_buffer(half)).expect("10 ms").len(),
      1,
      "the second 10 ms half closes the 20 ms frame"
    );
  }

  #[test]
  fn opus_encoder_rejects_unsupported_format() {
    let mut enc = OpusAudioEncoder::new(Bitrate(128_000)).expect("opus init");
    let mut frame = pcm_buffer(OPUS_FRAME_SAMPLES);
    frame.sample_rate = 44_100;
    assert!(matches!(
      enc.encode_audio(&frame).unwrap_err(),
      EncodeError::UnsupportedAudio {
        sample_rate: 44_100,
        ..
      }
    ));
  }

  #[test]
  fn fake_audio_encoder_frames_pcm() {
    let mut enc = FakeAudioEncoder::default();
    assert_eq!(enc.encode_audio(&pcm_buffer(2400)).expect("50 ms").len(), 2);
    // 10 ms leftover carries into the next buffer: 50 ms + 10 ms → 3.
    assert_eq!(enc.encode_audio(&pcm_buffer(2400)).expect("50 ms").len(), 3);
  }
}
