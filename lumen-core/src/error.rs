//! Errors surfaced by shared core types.

use thiserror::Error;

/// Errors produced while parsing or validating streaming configuration.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
  /// A `--max-bitrate` value could not be parsed.
  #[error("invalid bitrate {value:?}: expected a number with optional k/M suffix (e.g. 8000k, 2M)")]
  InvalidBitrate {
    /// The offending raw value.
    value: String,
  },
  /// A quality value was not one of `low|medium|high|auto`.
  #[error("invalid quality {value:?}: expected one of low, medium, high, auto")]
  InvalidQuality {
    /// The offending raw value.
    value: String,
  },
  /// A frame rate was out of the supported range.
  #[error("invalid fps {value}: expected 1..=240")]
  InvalidFps {
    /// The offending raw value.
    value: u32,
  },
}
