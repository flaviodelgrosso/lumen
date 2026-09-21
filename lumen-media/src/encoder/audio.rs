//! Opus audio encoding behind a portable trait.
//!
//! PCM arrives interleaved `f32` at 48 kHz, so no resampling or conversion
//! happens before encoding (bundled `libopus` via the `opus` crate's
//! `bundled` feature).

use bytes::Bytes;
use lumen_core::{
  AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, Bitrate, EncodedAudioFrame, OPUS_FRAME_SAMPLES, RawAudioFrame,
};

use super::EncodeError;

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
    if !frame.samples.len().is_multiple_of(frame_bytes) {
      return Err(EncodeError::MalformedAudio(format!(
        "{} B is not a whole number of stereo f32 frames",
        frame.samples.len()
      )));
    }
    self.staging.extend(
      frame
        .samples
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes)),
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
  use std::time::Instant;

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
