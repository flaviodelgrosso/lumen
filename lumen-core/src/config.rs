//! Streaming configuration: quality presets and bitrate parsing.

use std::str::FromStr;

/// Maximum length, in characters, of the `--name` session name.
pub const MAX_SESSION_NAME_LEN: usize = 64;

/// Normalize a `--name` session name for display.
///
/// The name is untrusted host text: control characters (including
/// terminal escapes and newlines) are replaced with spaces, whitespace
/// runs are collapsed and the result trimmed, so the value stays a safe
/// single-line label. Blank input normalizes to `None` (no name). The
/// name is display metadata only: it never participates in URLs, tokens
/// or any authorization decision, and UIs must render it through text
/// APIs (`textContent`), never as markup.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidSessionName`] when the normalized name
/// exceeds [`MAX_SESSION_NAME_LEN`] characters.
///
/// # Examples
///
/// ```
/// use lumen_core::normalize_session_name;
///
/// assert_eq!(
///   normalize_session_name("  Architecture\u{1} Workshop \n").unwrap().as_deref(),
///   Some("Architecture Workshop")
/// );
/// assert_eq!(normalize_session_name("   ").unwrap(), None);
/// ```
pub fn normalize_session_name(raw: &str) -> Result<Option<String>, ConfigError> {
  let collapsed: String = raw
    .chars()
    .map(|c| if c.is_control() { ' ' } else { c })
    .collect();
  let name = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
  if name.is_empty() {
    return Ok(None);
  }
  let len = name.chars().count();
  if len > MAX_SESSION_NAME_LEN {
    return Err(ConfigError::InvalidSessionName {
      value: name,
      max: MAX_SESSION_NAME_LEN,
    });
  }
  Ok(Some(name))
}

use serde::{Deserialize, Serialize};

use crate::Dimensions;
use crate::error::ConfigError;

/// Target video bitrate in bits per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Bitrate(pub u32);

impl Bitrate {
  #[must_use]
  pub const fn bps(self) -> u32 {
    self.0
  }
}

impl std::fmt::Display for Bitrate {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    if self.0 % 1_000_000 == 0 {
      write!(f, "{}M", self.0 / 1_000_000)
    } else if self.0 % 1_000 == 0 {
      write!(f, "{}k", self.0 / 1_000)
    } else {
      write!(f, "{}", self.0)
    }
  }
}

impl FromStr for Bitrate {
  type Err = ConfigError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let s = s.trim();
    let invalid = || ConfigError::InvalidBitrate {
      value: s.to_owned(),
    };
    let (digits, multiplier) = match s.chars().last() {
      Some('k' | 'K') => (&s[..s.len() - 1], 1_000_u64),
      Some('m' | 'M') => (&s[..s.len() - 1], 1_000_000_u64),
      _ => (s, 1),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
      return Err(invalid());
    }
    let value: u64 = digits.parse().map_err(|_| invalid())?;
    let bits = u32::try_from(value * multiplier).map_err(|_| invalid())?;
    if bits == 0 {
      return Err(invalid());
    }
    Ok(Self(bits))
  }
}

/// Encoder/quality preset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
  Low,
  Medium,
  #[default]
  Auto,
  High,
}

impl Quality {
  /// Approximate bits-per-pixel-per-frame used to derive a target bitrate
  /// from a resolution and frame rate.
  const fn bits_per_pixel(self) -> f64 {
    match self {
      Self::Low => 0.06,
      Self::High => 0.15,
      // `auto` follows `medium`; adaptive logic is a future extension.
      Self::Medium | Self::Auto => 0.10,
    }
  }

  #[must_use]
  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "bitrate math is non-negative and clamped to 0.8–20 Mbps"
  )]
  pub fn target_bitrate(self, dimensions: Dimensions, fps: u32) -> Bitrate {
    let pixels = f64::from(dimensions.width) * f64::from(dimensions.height);
    let raw = pixels * f64::from(fps.max(1)) * self.bits_per_pixel();
    // Round to the nearest 100 kbps, then clamp to a sane encoder range.
    let rounded = (raw / 100_000.0).round() * 100_000.0;
    Bitrate(rounded.clamp(800_000.0, 20_000_000.0) as u32)
  }
}

impl FromStr for Quality {
  type Err = ConfigError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s.trim().to_ascii_lowercase().as_str() {
      "low" => Ok(Self::Low),
      "medium" => Ok(Self::Medium),
      "high" => Ok(Self::High),
      "auto" => Ok(Self::Auto),
      _ => Err(ConfigError::InvalidQuality {
        value: s.to_owned(),
      }),
    }
  }
}

impl std::fmt::Display for Quality {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let name = match self {
      Self::Low => "low",
      Self::Medium => "medium",
      Self::High => "high",
      Self::Auto => "auto",
    };
    f.write_str(name)
  }
}

/// Video encoder backend preference (`--encoder`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderPreference {
  /// Prefer the platform-native hardware encoder; fall back to `OpenH264`.
  #[default]
  Auto,
  /// Always use the bundled `OpenH264` software encoder.
  Software,
  /// Require a platform-native hardware encoder; never fall back.
  Hardware,
}

impl FromStr for EncoderPreference {
  type Err = ConfigError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    match s.trim().to_ascii_lowercase().as_str() {
      "auto" => Ok(Self::Auto),
      "software" => Ok(Self::Software),
      "hardware" => Ok(Self::Hardware),
      _ => Err(ConfigError::InvalidEncoder {
        value: s.to_owned(),
      }),
    }
  }
}

impl std::fmt::Display for EncoderPreference {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let name = match self {
      Self::Auto => "auto",
      Self::Software => "software",
      Self::Hardware => "hardware",
    };
    f.write_str(name)
  }
}

/// Fully resolved streaming configuration for one `serve` run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
  /// Target capture/encode frame rate.
  pub fps: u32,
  /// Quality preset used to derive encoder bitrate when unset.
  pub quality: Quality,
  /// Explicit bitrate override.
  pub max_bitrate: Option<Bitrate>,
  /// Seconds between forced periodic keyframes.
  pub keyframe_interval_secs: u64,
  /// Whether system audio is captured and streamed (Opus).
  pub audio: bool,
  /// Target Opus audio bitrate.
  pub audio_bitrate: Bitrate,
  /// Video encoder backend preference.
  pub encoder: EncoderPreference,
  /// Optional human-readable session name (`--name`), normalized for
  /// display. Public presentation metadata; never a capability.
  pub session_name: Option<String>,
}

impl Default for StreamConfig {
  fn default() -> Self {
    Self {
      fps: 60,
      quality: Quality::Auto,
      max_bitrate: None,
      keyframe_interval_secs: 2,
      audio: true,
      audio_bitrate: Bitrate(128_000),
      encoder: EncoderPreference::default(),
      session_name: None,
    }
  }
}

impl StreamConfig {
  /// Resolve the effective encoder bitrate for a capture resolution.
  #[must_use]
  pub fn effective_bitrate(&self, dimensions: Dimensions) -> Bitrate {
    self
      .max_bitrate
      .unwrap_or_else(|| self.quality.target_bitrate(dimensions, self.fps))
  }

  /// Validate configuration values.
  ///
  /// # Errors
  ///
  /// Returns [`ConfigError::InvalidFps`] when `fps` is out of range.
  pub fn validate(&self) -> Result<(), ConfigError> {
    if self.fps == 0 || self.fps > 240 {
      return Err(ConfigError::InvalidFps { value: self.fps });
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_plain_bitrate() {
    assert_eq!("1500000".parse::<Bitrate>().unwrap().bps(), 1_500_000);
  }

  #[test]
  fn parses_kilobit_suffixes() {
    assert_eq!("8000k".parse::<Bitrate>().unwrap().bps(), 8_000_000);
    assert_eq!("8000K".parse::<Bitrate>().unwrap().bps(), 8_000_000);
    assert_eq!("400k".parse::<Bitrate>().unwrap().bps(), 400_000);
  }

  #[test]
  fn parses_megabit_suffixes() {
    assert_eq!("2M".parse::<Bitrate>().unwrap().bps(), 2_000_000);
    assert_eq!("2m".parse::<Bitrate>().unwrap().bps(), 2_000_000);
  }

  #[test]
  fn rejects_invalid_bitrates() {
    for bad in ["", "k", "M", "abc", "10x", "0", "1.5M", "-5", "99999999999"] {
      assert!(bad.parse::<Bitrate>().is_err(), "{bad:?} should not parse");
    }
  }

  #[test]
  fn bitrate_display_rounds() {
    assert_eq!(Bitrate(8_000_000).to_string(), "8M");
    assert_eq!(Bitrate(2_500_000).to_string(), "2500k");
    assert_eq!(Bitrate(1234).to_string(), "1234");
  }

  #[test]
  fn parses_quality_values() {
    assert_eq!("low".parse::<Quality>().unwrap(), Quality::Low);
    assert_eq!("Medium".parse::<Quality>().unwrap(), Quality::Medium);
    assert_eq!("HIGH".parse::<Quality>().unwrap(), Quality::High);
    assert_eq!("auto".parse::<Quality>().unwrap(), Quality::Auto);
    assert!("ultra".parse::<Quality>().is_err());
  }

  #[test]
  fn quality_target_bitrate_scales_with_resolution() {
    let hd = Quality::High.target_bitrate(Dimensions::new(1920, 1080), 30);
    let low_res = Quality::High.target_bitrate(Dimensions::new(1280, 720), 30);
    assert!(hd.bps() > low_res.bps());
    assert!(
      Quality::Low
        .target_bitrate(Dimensions::new(1920, 1080), 30)
        .bps()
        < hd.bps()
    );
    assert!(hd.bps() <= 20_000_000);
  }

  #[test]
  fn quality_target_bitrate_clamped() {
    let tiny = Quality::Low.target_bitrate(Dimensions::new(64, 64), 1);
    assert_eq!(tiny.bps(), 800_000);
    let huge = Quality::High.target_bitrate(Dimensions::new(7680, 4320), 60);
    assert_eq!(huge.bps(), 20_000_000);
  }

  #[test]
  fn explicit_bitrate_wins() {
    let cfg = StreamConfig {
      max_bitrate: Some(Bitrate(3_000_000)),
      ..StreamConfig::default()
    };
    assert_eq!(
      cfg.effective_bitrate(Dimensions::new(1920, 1080)).bps(),
      3_000_000
    );
  }

  #[test]
  fn fps_validation() {
    assert!(
      StreamConfig {
        fps: 0,
        ..StreamConfig::default()
      }
      .validate()
      .is_err()
    );
    assert!(
      StreamConfig {
        fps: 241,
        ..StreamConfig::default()
      }
      .validate()
      .is_err()
    );
    assert!(
      StreamConfig {
        fps: 30,
        ..StreamConfig::default()
      }
      .validate()
      .is_ok()
    );
  }

  #[test]
  fn parses_encoder_preferences() {
    assert_eq!(
      "auto".parse::<EncoderPreference>().unwrap(),
      EncoderPreference::Auto
    );
    assert_eq!(
      "Software".parse::<EncoderPreference>().unwrap(),
      EncoderPreference::Software
    );
    assert_eq!(
      "HARDWARE".parse::<EncoderPreference>().unwrap(),
      EncoderPreference::Hardware
    );
    assert!("gpu".parse::<EncoderPreference>().is_err());
  }

  #[test]
  fn encoder_preference_display_rounds() {
    for pref in [
      EncoderPreference::Auto,
      EncoderPreference::Software,
      EncoderPreference::Hardware,
    ] {
      assert_eq!(pref.to_string().parse::<EncoderPreference>().unwrap(), pref);
    }
  }

  #[test]
  fn stream_config_defaults_to_auto_encoder() {
    assert_eq!(StreamConfig::default().encoder, EncoderPreference::Auto);
  }

  #[test]
  fn session_name_trims_and_collapses_whitespace() {
    assert_eq!(
      normalize_session_name("  Architecture   Workshop  ")
        .unwrap()
        .as_deref(),
      Some("Architecture Workshop")
    );
  }

  #[test]
  fn session_name_replaces_control_characters() {
    // Newlines/tabs keep word boundaries; other control characters
    // (SOH, ESC, C1) collapse into plain spaces.
    assert_eq!(
      normalize_session_name("My\u{1} \tSession\nName\u{1b}[31m")
        .unwrap()
        .as_deref(),
      Some("My Session Name [31m")
    );
    assert_eq!(
      normalize_session_name("A\u{82}\u{9f}B").unwrap().as_deref(),
      Some("A B")
    );
  }

  #[test]
  fn blank_session_name_is_no_name() {
    assert_eq!(normalize_session_name("").unwrap(), None);
    assert_eq!(normalize_session_name("   \n\t\u{0} ").unwrap(), None);
  }

  #[test]
  fn session_name_length_is_counted_in_characters() {
    let at_max = "é".repeat(MAX_SESSION_NAME_LEN);
    assert_eq!(
      normalize_session_name(&at_max)
        .unwrap()
        .unwrap()
        .chars()
        .count(),
      MAX_SESSION_NAME_LEN
    );
    let too_long = "é".repeat(MAX_SESSION_NAME_LEN + 1);
    assert!(matches!(
      normalize_session_name(&too_long),
      Err(ConfigError::InvalidSessionName { max, .. })
        if max == MAX_SESSION_NAME_LEN
    ));
  }

  #[test]
  fn session_name_keeps_markup_as_inert_text() {
    // Normalization does not strip markup: the value is inert display
    // text (UIs render it with textContent, never innerHTML).
    let hostile = "<script>alert(1)</script>";
    assert_eq!(
      normalize_session_name(hostile).unwrap().as_deref(),
      Some(hostile)
    );
    let img = r#"<img src=x onerror="alert(1)">"#;
    assert_eq!(normalize_session_name(img).unwrap().as_deref(), Some(img));
  }

  #[test]
  fn stream_config_defaults_to_no_session_name() {
    assert_eq!(StreamConfig::default().session_name, None);
  }
}
