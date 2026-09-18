//! Bounded streaming band-limited resampling for one PCM lane.
//!
//! `ScreenCaptureKit` delivers audio in the output device's format, so
//! Bluetooth or USB audio commonly arrives at 44.1 kHz (or 96 kHz) while
//! Opus needs 48 kHz. Input arrives in irregular buffers; this resampler
//! preserves a short filter history across calls without keeping absolute
//! sample counters.

/// Half-width of the windowed-sinc filter in input samples.
///
/// At the largest supported downsampling ratio (384 kHz → 48 kHz), this
/// still spans four lobes of the cutoff-scaled sinc on each side.
const HALF_WIDTH: usize = 32;
const HALF_WIDTH_F64: f64 = 32.0;

/// One channel lane of the resampler.
pub(crate) struct LaneResampler {
  /// Input samples consumed per output sample.
  step: f64,
  /// Low-pass cutoff relative to the input Nyquist frequency.
  cutoff: f64,
  /// Pending samples, including the filter's left-hand history.
  samples: Vec<f32>,
  /// Center of the next output sample, relative to `samples[0]`.
  next: f64,
}

impl LaneResampler {
  pub(crate) fn new(in_rate: u32, out_rate: u32) -> Self {
    let step = f64::from(in_rate) / f64::from(out_rate);
    Self {
      step,
      cutoff: (1.0 / step).min(1.0),
      samples: vec![0.0; HALF_WIDTH],
      next: HALF_WIDTH_F64,
    }
  }

  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "positions are non-negative and kept within the small pending buffer"
  )]
  pub(crate) fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
    self.samples.extend_from_slice(input);

    // A center is ready once every non-zero tap on its right is present.
    while self.next + HALF_WIDTH_F64 <= self.samples.len() as f64 {
      let first = (self.next - HALF_WIDTH_F64).floor() as usize + 1;
      let last = (self.next + HALF_WIDTH_F64).ceil() as usize - 1;
      let mut value = 0.0_f64;
      let mut normalization = 0.0_f64;
      for index in first..=last {
        let distance = self.next - index as f64;
        let weight = low_pass_kernel(distance, self.cutoff);
        value += f64::from(self.samples[index]) * weight;
        normalization += weight;
      }
      out.push(if normalization.abs() > f64::EPSILON {
        (value / normalization) as f32
      } else {
        0.0
      });
      self.next += self.step;
    }

    // Keep only history that can contribute to the next output. Rebasing
    // here bounds both the buffer and floating-point position forever.
    let discard = (self.next.floor() as usize).saturating_sub(HALF_WIDTH);
    if discard != 0 {
      let remaining = self.samples.len() - discard;
      self.samples.copy_within(discard.., 0);
      self.samples.truncate(remaining);
      self.next -= discard as f64;
    }
  }
}

/// Cutoff-scaled sinc low-pass filter with a Lanczos window.
fn low_pass_kernel(distance: f64, cutoff: f64) -> f64 {
  cutoff * sinc(cutoff * distance) * sinc(distance / HALF_WIDTH_F64)
}

fn sinc(value: f64) -> f64 {
  if value.abs() < f64::EPSILON {
    1.0
  } else {
    let x = std::f64::consts::PI * value;
    x.sin() / x
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[expect(
    clippy::cast_precision_loss,
    reason = "test inputs stay below 2^23, exact in f32"
  )]
  fn sine(count: usize, frequency: f32, rate: f32) -> Vec<f32> {
    (0..count)
      .map(|index| (2.0 * std::f32::consts::PI * frequency * index as f32 / rate).sin())
      .collect()
  }

  #[test]
  fn same_rate_is_the_identity() {
    let input = sine(500, 440.0, 48_000.0);
    let mut resampler = LaneResampler::new(48_000, 48_000);
    let mut out = Vec::new();
    resampler.push(&input, &mut out);
    for (index, sample) in out.iter().enumerate() {
      assert!(
        (*sample - input[index]).abs() < 1e-6,
        "sample {index} differs"
      );
    }
  }

  #[test]
  fn chunked_streaming_equals_whole_buffer() {
    let input = sine(2_000, 1_000.0, 44_100.0);
    let mut whole = LaneResampler::new(44_100, 48_000);
    let mut expected = Vec::new();
    whole.push(&input, &mut expected);

    let mut chunked = LaneResampler::new(44_100, 48_000);
    let mut got = Vec::new();
    for chunk in input.chunks(137) {
      chunked.push(chunk, &mut got);
    }
    assert_eq!(got.len(), expected.len());
    for (index, (actual, expected)) in got.iter().zip(&expected).enumerate() {
      assert!(
        (actual - expected).abs() < 1e-6,
        "sample {index} differs across chunking"
      );
    }
    assert!(chunked.samples.len() <= HALF_WIDTH * 2 + 2);
  }

  #[test]
  fn upsample_yields_the_expected_length() {
    let input = sine(4_410, 440.0, 44_100.0);
    let mut resampler = LaneResampler::new(44_100, 48_000);
    let mut out = Vec::new();
    resampler.push(&input, &mut out);
    let delayed = (HALF_WIDTH * 48_000_usize).div_ceil(44_100);
    assert!(
      (out.len() + delayed).abs_diff(4_800) <= 1,
      "got {}",
      out.len()
    );
  }

  #[test]
  fn downsample_preserves_passband_and_rejects_aliases() {
    let low = sine(9_600, 1_000.0, 96_000.0);
    let mut low_resampler = LaneResampler::new(96_000, 48_000);
    let mut low_out = Vec::new();
    low_resampler.push(&low, &mut low_out);
    let low_peak = low_out
      .iter()
      .skip(100)
      .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    assert!(low_peak > 0.95 && low_peak <= 1.01, "peak {low_peak}");

    let above_output_nyquist = sine(9_600, 30_000.0, 96_000.0);
    let mut high_resampler = LaneResampler::new(96_000, 48_000);
    let mut high_out = Vec::new();
    high_resampler.push(&above_output_nyquist, &mut high_out);
    let high_peak = high_out
      .iter()
      .skip(100)
      .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    assert!(high_peak < 0.05, "aliased peak {high_peak}");
  }
}
