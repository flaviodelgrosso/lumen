//! Ephemeral post-approval signaling grants.
//!
//! A grant is the short-lived capability that turns an approved join
//! request into a live signaling socket. It is issued only after the host
//! allowed the viewer, is bound to exactly one peer, dies on first use or
//! after a short TTL, and never appears in a manually entered URL.
//! Candidates are compared in constant time against the stored token.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::peer::PeerInfo;
use crate::token::{SessionToken, TokenError};

/// Default lifetime of an unredeemed grant.
pub const DEFAULT_GRANT_TTL: Duration = Duration::from_secs(15);

/// One issued, unredeemed grant.
#[derive(Debug)]
struct Grant {
  token: SessionToken,
  peer: PeerInfo,
  expires: Instant,
}

/// Registry of issued, unredeemed grants.
///
/// Cheap to clone: every clone sees the same live set. The join manager
/// issues through it; the WebSocket upgrade redeems from it.
#[derive(Clone, Debug)]
pub struct GrantStore {
  grants: Arc<Mutex<Vec<Grant>>>,
  ttl: Duration,
}

impl GrantStore {
  /// New store with the default TTL.
  #[must_use]
  pub fn new() -> Self {
    Self::with_ttl(DEFAULT_GRANT_TTL)
  }

  /// New store with an explicit TTL (tests inject short lifetimes).
  #[must_use]
  pub fn with_ttl(ttl: Duration) -> Self {
    Self {
      grants: Arc::new(Mutex::new(Vec::new())),
      ttl,
    }
  }

  fn lock(&self) -> MutexGuard<'_, Vec<Grant>> {
    self.grants.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Issue a fresh single-use grant for `peer`; returns the secret.
  ///
  /// # Errors
  ///
  /// Returns [`TokenError::Random`] when the OS CSPRNG fails.
  pub fn issue(&self, peer: PeerInfo) -> Result<String, TokenError> {
    let token = SessionToken::generate()?;
    let secret = token.as_str().to_owned();
    let mut grants = self.lock();
    let now = Instant::now();
    grants.retain(|grant| grant.expires > now);
    grants.push(Grant {
      token,
      peer,
      expires: now + self.ttl,
    });
    Ok(secret)
  }

  /// Redeem `candidate`: returns the bound peer exactly once per grant.
  ///
  /// Constant-time comparison against every live grant; an expired grant
  /// is dropped and never returned.
  #[must_use]
  pub fn redeem(&self, candidate: &str) -> Option<PeerInfo> {
    let mut grants = self.lock();
    let index = grants
      .iter()
      .position(|grant| grant.token.matches(candidate))?;
    let grant = grants.remove(index);
    let now = Instant::now();
    if grant.expires <= now {
      return None;
    }
    let mut info = grant.peer;
    // The registry clock starts when the grant is redeemed, not when the
    // viewer first joined the approval queue.
    info.joined_at = now;
    Some(info)
  }

  /// `true` when `candidate` matches a live, unredeemed grant. Does not
  /// consume; prunes expired entries on the way through.
  #[must_use]
  pub fn contains(&self, candidate: &str) -> bool {
    let grants = self.lock();
    let now = Instant::now();
    grants
      .iter()
      .any(|grant| grant.expires > now && grant.token.matches(candidate))
  }

  /// Live (unredeemed, unexpired) grant count.
  #[must_use]
  pub fn len(&self) -> usize {
    let now = Instant::now();
    self
      .lock()
      .iter()
      .filter(|grant| grant.expires > now)
      .count()
  }

  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

impl Default for GrantStore {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use std::time::Instant;

  use super::*;
  use crate::token::PeerId;

  fn peer() -> PeerInfo {
    PeerInfo {
      id: PeerId::generate().unwrap(),
      address: Some("192.168.1.73".parse().unwrap()),
      user_agent: Some("Safari/iPad".to_owned()),
      joined_at: Instant::now(),
    }
  }

  #[test]
  fn grant_redeems_once() {
    let store = GrantStore::new();
    let info = peer();
    let secret = store.issue(info.clone()).unwrap();
    let redeemed = store.redeem(&secret).expect("first redeem");
    assert_eq!(redeemed.id, info.id);
    assert_eq!(redeemed.user_agent.as_deref(), Some("Safari/iPad"));
    assert!(store.redeem(&secret).is_none(), "grant is single-use");
  }

  #[test]
  fn wrong_candidate_never_redeems() {
    let store = GrantStore::new();
    let secret = store.issue(peer()).unwrap();
    assert!(store.redeem("").is_none());
    assert!(store.redeem("not-a-grant").is_none());
    assert!(store.redeem(&format!("{secret}x")).is_none());
    // The real grant survives unrelated probing.
    assert!(store.redeem(&secret).is_some());
  }

  #[test]
  fn expired_grant_is_rejected_and_dropped() {
    let store = GrantStore::with_ttl(Duration::from_millis(20));
    let secret = store.issue(peer()).unwrap();
    assert!(store.redeem(&secret).is_some(), "fresh grant redeems");

    let secret = store.issue(peer()).unwrap();
    std::thread::sleep(Duration::from_millis(40));
    assert!(
      store.redeem(&secret).is_none(),
      "expired grant must not redeem"
    );
    assert!(store.is_empty(), "expiry prunes on redeem miss");
  }

  #[test]
  fn grants_are_scoped_to_their_peer() {
    let store = GrantStore::new();
    let first = peer();
    let second = peer();
    let secret_a = store.issue(first.clone()).unwrap();
    let secret_b = store.issue(second.clone()).unwrap();
    assert_ne!(secret_a, secret_b);
    assert_eq!(store.redeem(&secret_a).unwrap().id, first.id);
    assert_eq!(store.redeem(&secret_b).unwrap().id, second.id);
  }

  #[test]
  fn contains_tracks_the_live_grant_without_consuming() {
    let store = GrantStore::with_ttl(Duration::from_millis(30));
    let secret = store.issue(peer()).unwrap();
    assert!(store.contains(&secret));
    assert!(store.contains(&secret), "contains never consumes");
    assert!(!store.contains("wrong"));
    assert!(store.redeem(&secret).is_some());
    assert!(!store.contains(&secret), "redeemed grant is not live");
    let secret = store.issue(peer()).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(!store.contains(&secret), "expired grant is not live");
  }

  #[test]
  fn debug_does_not_leak_grants() {
    let store = GrantStore::new();
    let secret = store.issue(peer()).unwrap();
    assert!(!format!("{store:?}").contains(&secret));
  }
}
