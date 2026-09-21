//! CPU-side BGRA → NV12 conversion for encoders that reject RGB input.
//!
//! The Windows `Media Foundation` H.264 encoder MFTs accept planar YUV
//! (`NV12` among them) but no RGB subtype, so every captured BGRA frame is
//! converted here before submission. The math is fixed-point ITU-R BT.601
//! limited-range (`studio swing`) — the coefficients `x264`/`OpenH264`
//! assume when the stream carries an explicit 601 matrix.
//!
//! Kept hardware-free so the conversion is unit-tested on every platform.

/// Byte length of an NV12 buffer for `width × height` (Y plane + 4:2:0 UV).
#[must_use]
pub(crate) const fn buffer_size(width: usize, height: usize) -> usize {
  width * height + width * height / 2
}

/// Convert a stride-padded BGRA image into an NV12 buffer.
///
/// `width` and `height` must be even (chroma is subsampled 2×2). Returns
/// `false` — writing nothing — when the geometry is invalid or `src` / `dst`
/// are too short for the declared size and stride.
///
/// Luma is computed per pixel; chroma averages each 2×2 RGB block before the
/// color transform, which is cheaper than averaging four transformed samples
/// and visually equivalent for screen content.
#[must_use]
pub(crate) fn from_bgra(
  src: &[u8],
  src_stride: usize,
  width: usize,
  height: usize,
  dst: &mut [u8],
) -> bool {
  if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
    return false;
  }
  let row_bytes = width * 4;
  let stride = src_stride.max(row_bytes);
  if src.len() < (height - 1) * stride + row_bytes {
    return false;
  }
  let need = buffer_size(width, height);
  if dst.len() < need {
    return false;
  }
  let luma_len = width * height;
  let (y_plane, uv_plane) = dst.split_at_mut(luma_len);
  let chroma_width = width / 2;

  // Luma: Y = ((66R + 129G + 25B + 128) >> 8) + 16, clamped to 0..=255.
  for (row, y_row) in y_plane.chunks_exact_mut(width).enumerate() {
    let Some(line) = src.get(row * stride..row * stride + row_bytes) else {
      return false;
    };
    for (x, y) in y_row.iter_mut().enumerate() {
      let px = &line[x * 4..x * 4 + 4];
      let (pb, pg, pr) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
      let value = ((66 * pr + 129 * pg + 25 * pb + 128) >> 8) + 16;
      *y = u8::try_from(value.clamp(0, 255)).unwrap_or(0);
    }
  }

  // Chroma from the 2×2 block averages:
  //   U = ((-38R - 74G + 112B + 128) >> 8) + 128
  //   V = ((112R - 94G - 18B + 128) >> 8) + 128
  // With summed (not averaged) block pixels the >>8 becomes >>10 and the
  // +128 rounding term becomes +512.
  for (block_row, uv_row) in uv_plane.chunks_exact_mut(chroma_width * 2).enumerate() {
    let top = block_row * 2;
    for (block_col, uv) in uv_row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
      let left = block_col * 2;
      let mut sums = [0_i32; 3]; // B, G, R over the 2×2 block.
      for row in [top, top + 1] {
        let Some(line) = src.get(row * stride..row * stride + row_bytes) else {
          return false;
        };
        for col in [left, left + 1] {
          let px = &line[col * 4..col * 4 + 4];
          sums[0] += i32::from(px[0]);
          sums[1] += i32::from(px[1]);
          sums[2] += i32::from(px[2]);
        }
      }
      let (sb, sg, sr) = (sums[0], sums[1], sums[2]);
      let u = ((-38 * sr - 74 * sg + 112 * sb + 512) >> 10) + 128;
      let v = ((112 * sr - 94 * sg - 18 * sb + 512) >> 10) + 128;
      uv[0] = u8::try_from(u.clamp(0, 255)).unwrap_or(0);
      uv[1] = u8::try_from(v.clamp(0, 255)).unwrap_or(0);
    }
  }
  true
}

#[cfg(test)]
mod tests {
  use super::*;

  fn bgra_solid(width: usize, height: usize, rgba: [u8; 4], stride: usize) -> Vec<u8> {
    let mut buf = vec![0_u8; stride * height];
    for row in buf.chunks_exact_mut(stride) {
      for px in row[..width * 4].as_chunks_mut::<4>().0 {
        px.copy_from_slice(&rgba);
      }
    }
    buf
  }

  fn convert(src: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let mut dst = vec![0_u8; buffer_size(width, height)];
    assert!(from_bgra(src, stride, width, height, &mut dst));
    dst
  }

  #[test]
  fn black_maps_to_limited_black() {
    let src = bgra_solid(2, 2, [0, 0, 0, 255], 8);
    let dst = convert(&src, 8, 2, 2);
    assert!(dst[..4].iter().all(|&y| y == 16), "Y must be 16: {dst:?}");
    assert_eq!(&dst[4..], &[128, 128], "U/V must be 128");
  }

  #[test]
  fn white_maps_to_limited_white() {
    let src = bgra_solid(2, 2, [255, 255, 255, 255], 8);
    let dst = convert(&src, 8, 2, 2);
    assert!(dst[..4].iter().all(|&y| y == 235), "Y must be 235: {dst:?}");
    assert_eq!(&dst[4..], &[128, 128]);
  }

  #[test]
  fn primary_red_matches_bt601() {
    // Pure red (B=0, G=0, R=255): Y=82, U=90, V=240 per BT.601 studio swing.
    let src = bgra_solid(2, 2, [0, 0, 255, 255], 8);
    let dst = convert(&src, 8, 2, 2);
    assert!(dst[..4].iter().all(|&y| y == 82), "Y must be 82: {dst:?}");
    assert_eq!(&dst[4..], &[90, 240]);
  }

  #[test]
  fn chroma_averages_the_2x2_block() {
    // One red and one blue pixel per block: chroma is transformed from the
    // 2×2 block averages (B=G=R sums: 510, 0, 510), not from either primary.
    let mut src = vec![0_u8; 2 * 2 * 8];
    for (i, px) in src.as_chunks_mut::<4>().0.iter_mut().enumerate() {
      if i % 2 == 0 {
        px.copy_from_slice(&[0, 0, 255, 255]); // red
      } else {
        px.copy_from_slice(&[255, 0, 0, 255]); // blue
      }
    }
    let dst = convert(&src, 8, 2, 2);
    // U = ((-38·510 + 112·510 + 512) >> 10) + 128 = 165 (red: 90, blue: 240).
    // V = ((112·510 - 18·510 + 512) >> 10) + 128 = 175 (red: 240, blue: 110).
    assert_eq!(&dst[4..], &[165, 175]);
  }

  #[test]
  fn invalid_geometry_and_short_buffers_are_rejected() {
    let mut dst = vec![0_u8; 64];
    // Odd dimensions.
    assert!(!from_bgra(&[], 4, 3, 2, &mut dst));
    assert!(!from_bgra(&[], 4, 2, 3, &mut dst));
    // Source shorter than the declared frame.
    let src = bgra_solid(2, 2, [0, 0, 0, 255], 8);
    assert!(!from_bgra(&src[..6], 8, 2, 2, &mut dst));
    // Destination too short for NV12.
    let mut tiny = vec![0_u8; buffer_size(2, 2) - 1];
    assert!(!from_bgra(&src, 8, 2, 2, &mut tiny));
  }
}
