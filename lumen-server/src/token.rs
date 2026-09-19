//! Cryptographically secure session tokens and peer identities.

use std::fmt;

use base64::Engine as _;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use thiserror::Error;

/// Errors from secure randomness.
#[derive(Debug, Error)]
pub enum TokenError {
  /// The OS CSPRNG failed.
  #[error("failed to obtain secure randomness: {0}")]
  Random(String),
}

/// Number of random bytes backing a session token (256 bits).
const TOKEN_BYTES: usize = 32;

/// A per-process, URL-safe session token.
///
/// Generated once per `lumen serve` invocation with an OS-backed CSPRNG;
/// it lives only in memory and dies with the process.
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
  /// Generate a fresh 256-bit token encoded as URL-safe base64 (no padding).
  ///
  /// # Errors
  ///
  /// Returns [`TokenError::Random`] when the OS CSPRNG fails.
  pub fn generate() -> Result<Self, TokenError> {
    let mut buf = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut buf).map_err(|e| TokenError::Random(e.to_string()))?;
    Ok(Self(BASE64_URL_SAFE_NO_PAD.encode(buf)))
  }

  #[must_use]
  pub fn as_str(&self) -> &str {
    &self.0
  }

  /// Constant-time comparison against an untrusted candidate string.
  #[must_use]
  pub fn matches(&self, candidate: &str) -> bool {
    let ours = self.0.as_bytes();
    let theirs = candidate.as_bytes();
    if ours.len() != theirs.len() {
      return false;
    }
    // Simple fold; token material is short and both sides are public to
    // the caller, but avoid early-exit length probing.
    let mut diff = 0_u8;
    for (a, b) in ours.iter().zip(theirs) {
      diff |= a ^ b;
    }
    diff == 0
  }
}

impl fmt::Display for SessionToken {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl fmt::Debug for SessionToken {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "SessionToken(<redacted {} chars>)", self.0.len())
  }
}

/// A short, per-process pairing secret for unattended sharing.
///
/// It is deliberately distinct from every URL-capable token: it only proves
/// physical proximity to the host's displayed terminal during join creation.
#[derive(Clone)]
pub struct PairingCode(String);

impl PairingCode {
  /// Generate a uniformly distributed six-digit code from the OS CSPRNG.
  ///
  /// # Errors
  ///
  /// Returns [`TokenError::Random`] when the OS CSPRNG fails.
  pub fn generate() -> Result<Self, TokenError> {
    const SPACE: u32 = 1_000_000;
    const CEILING: u32 = u32::MAX - u32::MAX % SPACE;
    loop {
      let mut buf = [0_u8; 4];
      getrandom::fill(&mut buf).map_err(|e| TokenError::Random(e.to_string()))?;
      let value = u32::from_be_bytes(buf);
      if value < CEILING {
        return Ok(Self(format!("{:06}", value % SPACE)));
      }
    }
  }

  /// Constant-time comparison against a viewer-supplied candidate.
  #[must_use]
  pub fn matches(&self, candidate: &str) -> bool {
    let ours = self.0.as_bytes();
    let theirs = candidate.as_bytes();
    if ours.len() != theirs.len() {
      return false;
    }
    let mut diff = 0_u8;
    for (a, b) in ours.iter().zip(theirs) {
      diff |= a ^ b;
    }
    diff == 0
  }
}

impl fmt::Display for PairingCode {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.0)
  }
}

impl fmt::Debug for PairingCode {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("PairingCode(<redacted>)")
  }
}

/// Stable identity for one connected viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(u64);

impl PeerId {
  /// Draw a fresh random peer id.
  ///
  /// # Errors
  ///
  /// Returns [`TokenError::Random`] when the OS CSPRNG fails.
  pub fn generate() -> Result<Self, TokenError> {
    let mut buf = [0_u8; 8];
    getrandom::fill(&mut buf).map_err(|e| TokenError::Random(e.to_string()))?;
    Ok(Self(u64::from_be_bytes(buf)))
  }

  /// Reconstruct from a raw value (e.g. parsed from an API path).
  #[must_use]
  pub const fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  #[must_use]
  pub const fn raw(self) -> u64 {
    self.0
  }
}

impl fmt::Display for PeerId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:016x}", self.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tokens_are_unique_and_urlsafe() {
    let a = SessionToken::generate().unwrap();
    let b = SessionToken::generate().unwrap();
    assert_ne!(a.as_str(), b.as_str());
    assert!(a.as_str().len() >= 43); // 256 bits in base64url
    assert!(
      a.as_str()
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
      "token must be URL-safe"
    );
  }

  #[test]
  fn token_matches_only_exact_value() {
    let t = SessionToken::generate().unwrap();
    assert!(t.matches(t.as_str()));
    assert!(!t.matches(""));
    assert!(!t.matches(&format!("{}x", t.as_str())));
    let other = SessionToken::generate().unwrap();
    assert!(!t.matches(other.as_str()));
  }

  #[test]
  fn debug_does_not_leak_token() {
    let t = SessionToken::generate().unwrap();
    assert!(!format!("{t:?}").contains(t.as_str()));
  }

  #[test]
  fn peer_ids_distinct() {
    let a = PeerId::generate().unwrap();
    let b = PeerId::generate().unwrap();
    assert_ne!(a, b);
    assert!(!a.to_string().is_empty());
  }
}
