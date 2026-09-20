//! H.264 software encoding backed by the bundled Cisco `OpenH264` library.
//!
//! BGRA → I420 conversion uses the crate's own `from_bgra8_source` path;
//! parameter sets are cached from the first IDR so every later IDR stays
//! independently decodable even if the native encoder omits them.

use bytes::{BufMut, Bytes, BytesMut};
use lumen_core::{Bitrate, Dimensions, EncodedFrame, RawFrame};
use openh264::OpenH264API;
use openh264::encoder::{
  BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
  SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};

use super::{EncodeError, VideoEncoder, annex_b_without_param_sets, nal_type};

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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::encoder::split_annex_b;
  use crate::encoder::tests_util::gradient_frame;
  use lumen_core::Bitrate;

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
}
