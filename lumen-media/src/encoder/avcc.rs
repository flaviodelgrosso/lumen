//! AVCC (length-prefixed) ↔ Annex-B (start-code) bitstream utilities.
//!
//! `VideoToolbox` emits H.264 access units in AVCC form: each NAL unit is
//! preceded by a 4-byte big-endian length. The WebRTC pipeline expects
//! Annex-B, so every encoded sample is rewritten here. The Windows
//! `Media Foundation` backend uses the same conversion defensively for
//! encoders that emit length-prefixed instead of start-code payloads.
//! Kept hardware-free so the conversion is unit-tested on every platform.

use bytes::{BufMut, Bytes, BytesMut};

/// Size of the NAL length prefix `VideoToolbox` uses (always 4 bytes for
/// `kCMVideoCodecType_H264` samples).
const NAL_LENGTH_PREFIX: usize = 4;

/// H.264 IDR slice NAL type.
#[cfg(any(target_os = "macos", test))]
pub(crate) const NAL_TYPE_IDR: u8 = 5;

/// Convert one AVCC access unit to Annex-B.
///
/// Returns `Ok(None)` for an empty input (a sample with no payload) and
/// `Err` with a human-readable reason when the length prefixes are
/// malformed or truncated — callers must never emit a partial access unit.
pub(crate) fn avcc_to_annex_b(avcc: &[u8]) -> Result<Option<Bytes>, String> {
  if avcc.is_empty() {
    return Ok(None);
  }
  let mut out = BytesMut::with_capacity(avcc.len() + NAL_LENGTH_PREFIX);
  let mut pos = 0usize;
  let mut nals = 0usize;
  while pos < avcc.len() {
    let Some(prefix) = avcc.get(pos..pos + NAL_LENGTH_PREFIX) else {
      return Err(format!(
        "truncated NAL length prefix at byte {pos} ({} B left)",
        avcc.len() - pos
      ));
    };
    let nal_len = u32::from_be_bytes(prefix.try_into().expect("4-byte slice")) as usize;
    if nal_len == 0 {
      return Err(format!("zero-length NAL at byte {pos}"));
    }
    let start = pos + NAL_LENGTH_PREFIX;
    let Some(end) = start.checked_add(nal_len) else {
      return Err(format!("NAL length {nal_len} overflows at byte {pos}"));
    };
    if end > avcc.len() {
      return Err(format!(
        "NAL at byte {pos} declares {nal_len} B but only {} B remain",
        avcc.len() - start
      ));
    }
    out.put_slice(&[0, 0, 0, 1]);
    out.put_slice(&avcc[start..end]);
    pos = end;
    nals += 1;
  }
  debug_assert!(nals > 0, "non-empty input always yields at least one NAL");
  Ok(Some(out.freeze()))
}

/// NAL type of the first length-prefixed NAL in an AVCC access unit.
///
/// `None` when the buffer is empty or the first length prefix is
/// incomplete/zero.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn first_avcc_nal_type(avcc: &[u8]) -> Option<u8> {
  let prefix = avcc.get(..NAL_LENGTH_PREFIX)?;
  let nal_len = u32::from_be_bytes(prefix.try_into().expect("4-byte slice")) as usize;
  if nal_len == 0 {
    return None;
  }
  let first_byte = avcc.get(NAL_LENGTH_PREFIX)?;
  Some(*first_byte & 0x1f)
}

/// True when the access unit starts with an IDR slice (NAL type 5).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn avcc_starts_with_idr(avcc: &[u8]) -> bool {
  first_avcc_nal_type(avcc) == Some(NAL_TYPE_IDR)
}

/// Prepend the cached Annex-B SPS/PPS to an IDR access unit so every
/// keyframe is self-contained for mid-session joiners.
///
/// Returns the body unchanged when no parameter sets are cached yet or
/// the access unit already begins with them (the encoder emitted them
/// inline).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn idr_with_param_sets(param_sets: Option<&Bytes>, body: Bytes) -> Bytes {
  let Some(sets) = param_sets else {
    return body;
  };
  if body.starts_with(sets) {
    return body;
  }
  let mut buf = BytesMut::with_capacity(sets.len() + body.len());
  buf.put_slice(sets);
  buf.put_slice(&body);
  buf.freeze()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build an AVCC access unit from NAL payloads.
  fn avcc_of(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
      out.extend_from_slice(
        &u32::try_from(nal.len())
          .expect("test NAL payloads fit in u32")
          .to_be_bytes(),
      );
      out.extend_from_slice(nal);
    }
    out
  }

  #[test]
  fn converts_single_nal() {
    // IDR slice header byte: 0x65 → type 5.
    let input = avcc_of(&[&[0x65, 0xAA, 0xBB]]);
    let out = avcc_to_annex_b(&input).unwrap().expect("output");
    assert_eq!(&out[..], &[0, 0, 0, 1, 0x65, 0xAA, 0xBB]);
  }

  #[test]
  fn converts_multiple_nals_in_order() {
    // SPS (7) + PPS (8) + IDR (5), as `VideoToolbox` may emit them inline.
    let input = avcc_of(&[
      &[0x67, 0x01], // SPS
      &[0x68, 0x02], // PPS
      &[0x65, 0x03], // IDR
    ]);
    let out = avcc_to_annex_b(&input).unwrap().expect("output");
    assert_eq!(
      &out[..],
      &[
        0, 0, 0, 1, 0x67, 0x01, //
        0, 0, 0, 1, 0x68, 0x02, //
        0, 0, 0, 1, 0x65, 0x03, //
      ]
    );
  }

  #[test]
  fn empty_input_yields_no_output() {
    assert!(avcc_to_annex_b(&[]).unwrap().is_none());
  }

  #[test]
  fn truncated_length_prefix_is_rejected() {
    // A declared NAL longer than the remaining payload.
    let input = avcc_of(&[&[0x65, 0xAA, 0xBB]]);
    let truncated = &input[..input.len() - 2];
    assert!(avcc_to_annex_b(truncated).is_err());
  }

  #[test]
  fn partial_length_prefix_is_rejected() {
    assert!(avcc_to_annex_b(&[0, 0, 0]).is_err());
  }

  #[test]
  fn zero_length_nal_is_rejected() {
    let input = [0, 0, 0, 0, 0, 0, 0, 3, 0x65, 0x01, 0x02];
    assert!(avcc_to_annex_b(&input).is_err());
  }

  #[test]
  fn huge_length_prefix_is_rejected() {
    let input = [0xFF, 0xFF, 0xFF, 0xFF, 0x65];
    assert!(avcc_to_annex_b(&input).is_err());
  }

  #[test]
  fn trailing_bytes_are_rejected() {
    // Second NAL's length prefix promises more than exists.
    let mut input = avcc_of(&[&[0x41, 0x01]]);
    input.extend_from_slice(&[0, 0, 0, 9, 0x41]);
    assert!(avcc_to_annex_b(&input).is_err());
  }

  #[test]
  fn idr_detection() {
    assert!(avcc_starts_with_idr(&avcc_of(&[&[0x65, 0x01]])));
    // Non-IDR slice (type 1) after an SPS-prefixed P frame.
    assert!(!avcc_starts_with_idr(&avcc_of(&[&[0x41, 0x01]])));
    assert!(!avcc_starts_with_idr(&[]));
    assert!(!avcc_starts_with_idr(&[0, 0, 0, 0]));
  }

  #[test]
  fn first_nal_type_masks_forbidden_bits() {
    // 0x05 with the F/NRI bits set still reports type 5.
    assert_eq!(first_avcc_nal_type(&avcc_of(&[&[0xE5, 0x01]])), Some(5));
  }

  #[test]
  fn idr_gets_param_sets_prepended() {
    let sets = Bytes::from_static(&[0, 0, 0, 1, 0x67, 0x01, 0, 0, 0, 1, 0x68, 0x02]);
    let body = Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x03]);
    let out = idr_with_param_sets(Some(&sets), body.clone());
    assert!(out.starts_with(&sets), "IDR must lead with SPS+PPS");
    assert!(
      out.ends_with(&body),
      "original body must follow the parameter sets"
    );
  }

  #[test]
  fn inline_param_sets_are_not_duplicated() {
    let sets = Bytes::from_static(&[0, 0, 0, 1, 0x67, 0x01, 0, 0, 0, 1, 0x68, 0x02]);
    let mut vec = sets.to_vec();
    vec.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x03]);
    let body = Bytes::from(vec);
    let out = idr_with_param_sets(Some(&sets), body.clone());
    assert_eq!(
      out, body,
      "already self-contained access units pass through"
    );
  }

  #[test]
  fn missing_param_sets_pass_the_body_through() {
    let body = Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x03]);
    assert_eq!(idr_with_param_sets(None, body.clone()), body);
  }
}
