//! Join requests: the tokenless entry path into the approval queue.
//!
//! Viewers enter through `/` and `POST /api/join` — no secret appears in the
//! manually entered URL. Each response has a public [`PeerId`] plus an
//! independent secret [`SessionToken`] required to poll
//! `GET /api/join/<id>`. The host approves through the shared
//! [`ApprovalQueue`], then the manager issues an ephemeral [`GrantStore`]
//! capability for signaling. A viewer that stops polling is pruned after a
//! TTL. Global, per-IP-pending, and per-IP-interval bounds keep cheap POSTs
//! from accumulating approval state.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::oneshot;

use crate::auth::{AuthDecision, Authorizer};
use crate::grant::GrantStore;
use crate::peer::PeerInfo;
use crate::token::{PeerId, SessionToken};

/// Default cap on simultaneously live join sessions.
pub const DEFAULT_MAX_JOINS: usize = 32;
/// Default lifetime of a join session that stopped polling.
pub const DEFAULT_JOIN_TTL: Duration = Duration::from_secs(60);
/// Maximum pending joins that one source address may hold.
pub const DEFAULT_MAX_PENDING_PER_IP: usize = 2;
/// Minimum time between new joins from one source address.
pub const DEFAULT_JOIN_INTERVAL: Duration = Duration::from_secs(1);

/// Errors creating a join request.
#[derive(Debug, Error)]
pub enum JoinError {
  /// Too many live join sessions: the caller must back off (rate limit).
  #[error("too many pending join requests")]
  Full,
  /// One source address exceeded its pending or creation-rate bound.
  #[error("join request rate limited")]
  RateLimited,
  /// The approval queue is saturated: no host can answer (fail closed).
  #[error("no host approver available")]
  NoApprover,
  /// The OS CSPRNG failed.
  #[error(transparent)]
  Random(#[from] crate::token::TokenError),
}

/// A saturated approval queue is exactly the fail-closed denial the join
/// surface reports.
impl From<crate::auth::AuthError> for JoinError {
  fn from(_: crate::auth::AuthError) -> Self {
    Self::NoApprover
  }
}

/// The independent identity and polling capability issued for one join.
#[derive(Clone, Debug)]
pub struct JoinRequest {
  /// Public peer identity shown to host approval surfaces.
  pub id: PeerId,
  /// Secret capability required to inspect this join's status.
  pub token: SessionToken,
}

/// What a polling viewer is told about its join request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinStatus {
  /// The host has not decided yet.
  Pending,
  /// The host allowed: redeem `grant` on the signaling socket.
  Approved {
    /// Ephemeral single-use grant secret.
    grant: String,
  },
  /// The host refused.
  Denied,
  /// The request is gone, expired, or inaccessible: join again.
  Gone,
}

/// Where a join session stands with the host.
#[derive(Debug)]
enum JoinState {
  Pending,
  Approved { grant: String },
  Denied,
}

#[derive(Debug)]
struct JoinSession {
  info: PeerInfo,
  token: SessionToken,
  /// Host verdict receiver; taken once observed. Dropping the session
  /// drops it, which prunes the entry from the approval queue.
  verdict: Option<oneshot::Receiver<AuthDecision>>,
  state: JoinState,
  /// Last authorized poll; abandoned sessions expire after the join TTL.
  last_poll: Instant,
}

#[derive(Debug, Default)]
struct JoinStateTable {
  sessions: HashMap<PeerId, JoinSession>,
  last_create: HashMap<IpAddr, Instant>,
}

/// Live join sessions keyed by peer id.
///
/// Cheap to clone: every clone sees the same session set. The HTTP
/// handlers drive it; the [`GrantStore`] carries the capability out.
#[derive(Clone, Debug)]
pub struct JoinManager {
  state: Arc<Mutex<JoinStateTable>>,
  authorizer: Authorizer,
  grants: GrantStore,
  max_joins: usize,
  join_ttl: Duration,
  max_pending_per_ip: usize,
  join_interval: Duration,
}

impl JoinManager {
  /// New manager with the default bounds.
  #[must_use]
  pub fn new(authorizer: Authorizer, grants: GrantStore) -> Self {
    Self::with_abuse_limits(
      authorizer,
      grants,
      DEFAULT_MAX_JOINS,
      DEFAULT_JOIN_TTL,
      DEFAULT_MAX_PENDING_PER_IP,
      DEFAULT_JOIN_INTERVAL,
    )
  }

  /// New manager with explicit global bounds (tests inject tiny limits).
  #[must_use]
  pub fn with_limits(
    authorizer: Authorizer,
    grants: GrantStore,
    max_joins: usize,
    join_ttl: Duration,
  ) -> Self {
    Self::with_abuse_limits(
      authorizer,
      grants,
      max_joins,
      join_ttl,
      DEFAULT_MAX_PENDING_PER_IP,
      DEFAULT_JOIN_INTERVAL,
    )
  }

  /// New manager with explicit global and per-address bounds.
  #[must_use]
  pub fn with_abuse_limits(
    authorizer: Authorizer,
    grants: GrantStore,
    max_joins: usize,
    join_ttl: Duration,
    max_pending_per_ip: usize,
    join_interval: Duration,
  ) -> Self {
    Self {
      state: Arc::new(Mutex::new(JoinStateTable::default())),
      authorizer,
      grants,
      max_joins: max_joins.max(1),
      join_ttl,
      max_pending_per_ip: max_pending_per_ip.max(1),
      join_interval,
    }
  }

  fn lock(&self) -> MutexGuard<'_, JoinStateTable> {
    self.state.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Park a new join request in the approval queue.
  ///
  /// # Errors
  ///
  /// - [`JoinError::Full`] when the live-session bound is reached.
  /// - [`JoinError::RateLimited`] when one address exceeds its bounds.
  /// - [`JoinError::NoApprover`] when the approval queue is saturated.
  /// - [`JoinError::Random`] when the OS CSPRNG fails.
  pub fn create(&self, info: PeerInfo) -> Result<JoinRequest, JoinError> {
    let mut state = self.lock();
    let now = Instant::now();
    prune(&mut state, now, self.join_ttl);
    if state.sessions.len() >= self.max_joins {
      return Err(JoinError::Full);
    }
    if let Some(ip) = info.address {
      let pending = state
        .sessions
        .values()
        .filter(|session| {
          session.info.address == Some(ip) && matches!(session.state, JoinState::Pending)
        })
        .count();
      if pending >= self.max_pending_per_ip
        || state
          .last_create
          .get(&ip)
          .is_some_and(|previous| now.duration_since(*previous) < self.join_interval)
      {
        return Err(JoinError::RateLimited);
      }
    }
    let token = SessionToken::generate()?;
    let verdict = self.authorizer.park(info.clone())?;
    let id = info.id;
    if let Some(ip) = info.address {
      state.last_create.insert(ip, now);
    }
    state.sessions.insert(
      id,
      JoinSession {
        info,
        token: token.clone(),
        verdict: Some(verdict),
        state: JoinState::Pending,
        last_poll: now,
      },
    );
    Ok(JoinRequest { id, token })
  }

  /// Record a rejected pairing attempt against the same per-IP throttle.
  ///
  /// `true` means this is the first attempt in the current interval; `false`
  /// means the caller must answer 429. No join session is created.
  #[must_use]
  pub fn record_failed_attempt(&self, ip: IpAddr) -> bool {
    let mut state = self.lock();
    let now = Instant::now();
    prune(&mut state, now, self.join_ttl);
    if state
      .last_create
      .get(&ip)
      .is_some_and(|previous| now.duration_since(*previous) < self.join_interval)
    {
      return false;
    }
    state.last_create.insert(ip, now);
    true
  }

  /// Poll a join request with its secret; issues the grant after approval.
  ///
  /// Repeated authorized polls before redemption return the same grant. An
  /// unknown id or wrong token is deliberately indistinguishable from gone.
  pub fn poll(&self, id: PeerId, token: &str) -> JoinStatus {
    let mut state = self.lock();
    let now = Instant::now();
    prune(&mut state, now, self.join_ttl);
    let Some(session) = state.sessions.get_mut(&id) else {
      return JoinStatus::Gone;
    };
    if !session.token.matches(token) {
      return JoinStatus::Gone;
    }
    session.last_poll = now;
    let status = match &session.state {
      JoinState::Denied => JoinStatus::Denied,
      JoinState::Approved { grant } => {
        if self.grants.contains(grant) {
          JoinStatus::Approved {
            grant: grant.clone(),
          }
        } else {
          JoinStatus::Gone
        }
      }
      JoinState::Pending => {
        let verdict = session.verdict.as_mut().map_or(
          Err(oneshot::error::TryRecvError::Closed),
          oneshot::Receiver::try_recv,
        );
        match verdict {
          Ok(AuthDecision::Allow) => {
            session.verdict = None;
            match self.grants.issue(session.info.clone()) {
              Ok(grant) => {
                session.state = JoinState::Approved {
                  grant: grant.clone(),
                };
                JoinStatus::Approved { grant }
              }
              Err(e) => {
                tracing::error!("grant issuance failed: {e}");
                session.state = JoinState::Denied;
                JoinStatus::Denied
              }
            }
          }
          Ok(AuthDecision::Deny) | Err(oneshot::error::TryRecvError::Closed) => {
            session.verdict = None;
            session.state = JoinState::Denied;
            JoinStatus::Denied
          }
          Err(oneshot::error::TryRecvError::Empty) => JoinStatus::Pending,
        }
      }
    };
    if matches!(status, JoinStatus::Gone) {
      state.sessions.remove(&id);
    }
    status
  }

  /// Live session count.
  #[must_use]
  pub fn len(&self) -> usize {
    let now = Instant::now();
    self
      .lock()
      .sessions
      .values()
      .filter(|session| now.duration_since(session.last_poll) < self.join_ttl)
      .count()
  }

  /// Number of retained per-IP throttle entries.
  #[must_use]
  pub fn limiter_len(&self) -> usize {
    let now = Instant::now();
    let mut state = self.lock();
    prune(&mut state, now, self.join_ttl);
    state.last_create.len()
  }

  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

/// Drop abandoned sessions and stale rate entries. Removing a session drops
/// its verdict receiver, so the approval queue prunes it on its next touch.
fn prune(state: &mut JoinStateTable, now: Instant, ttl: Duration) {
  state
    .sessions
    .retain(|_, session| now.duration_since(session.last_poll) < ttl);
  state
    .last_create
    .retain(|_, created| now.duration_since(*created) < ttl);
}

#[cfg(test)]
mod tests {
  use std::time::Instant;

  use super::*;
  use crate::auth::ApprovalQueue;

  fn channel() -> (Authorizer, ApprovalQueue) {
    Authorizer::channel(16)
  }

  fn peer(ip: &str) -> PeerInfo {
    PeerInfo {
      id: PeerId::generate().unwrap(),
      address: Some(ip.parse().unwrap()),
      user_agent: Some("Safari/iPad".to_owned()),
      joined_at: Instant::now(),
    }
  }

  fn poll(joins: &JoinManager, request: &JoinRequest) -> JoinStatus {
    joins.poll(request.id, request.token.as_str())
  }

  fn decide(queue: &ApprovalQueue, decision: AuthDecision) {
    let pending = queue.poll().into_iter().next().expect("pending");
    assert!(queue.decide(pending.id, decision));
  }

  #[test]
  fn new_join_is_pending_with_a_separate_token() {
    let (auth, queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    let request = joins.create(peer("192.168.1.73")).unwrap();
    assert_eq!(poll(&joins, &request), JoinStatus::Pending);
    assert_eq!(queue.poll().len(), 1, "the join parked in the queue");
  }

  #[test]
  fn peer_id_or_wrong_token_cannot_retrieve_an_approved_grant() {
    let (auth, queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    let request = joins.create(peer("192.168.1.73")).unwrap();
    decide(&queue, AuthDecision::Allow);
    assert_eq!(
      joins.poll(request.id, "not-the-join-token"),
      JoinStatus::Gone
    );
    assert!(matches!(
      poll(&joins, &request),
      JoinStatus::Approved { .. }
    ));
  }

  #[test]
  fn join_token_is_scoped_to_one_request() {
    let (auth, _queue) = channel();
    let joins = JoinManager::with_abuse_limits(
      auth,
      GrantStore::new(),
      8,
      DEFAULT_JOIN_TTL,
      2,
      Duration::ZERO,
    );
    let first = joins.create(peer("192.168.1.73")).unwrap();
    let second = joins.create(peer("192.168.1.74")).unwrap();
    assert_eq!(
      joins.poll(second.id, first.token.as_str()),
      JoinStatus::Gone
    );
    assert_eq!(poll(&joins, &first), JoinStatus::Pending);
  }

  #[test]
  fn approved_join_issues_a_stable_single_use_grant() {
    let (auth, queue) = channel();
    let grants = GrantStore::new();
    let joins = JoinManager::new(auth, grants.clone());
    let request = joins.create(peer("192.168.1.73")).unwrap();
    decide(&queue, AuthDecision::Allow);
    let JoinStatus::Approved { grant } = poll(&joins, &request) else {
      panic!("expected approved");
    };
    assert_eq!(
      poll(&joins, &request),
      JoinStatus::Approved {
        grant: grant.clone()
      }
    );
    assert_eq!(grants.redeem(&grant).unwrap().id, request.id);
    assert!(grants.redeem(&grant).is_none(), "the grant is single-use");
    assert_eq!(poll(&joins, &request), JoinStatus::Gone);
  }

  #[test]
  fn expired_grant_retires_the_session() {
    let (auth, queue) = channel();
    let grants = GrantStore::with_ttl(Duration::from_millis(30));
    let joins = JoinManager::new(auth, grants.clone());
    let request = joins.create(peer("192.168.1.73")).unwrap();
    decide(&queue, AuthDecision::Allow);
    let JoinStatus::Approved { grant } = poll(&joins, &request) else {
      panic!("expected approved");
    };
    std::thread::sleep(Duration::from_millis(60));
    assert!(grants.redeem(&grant).is_none());
    assert_eq!(poll(&joins, &request), JoinStatus::Gone);
  }

  #[test]
  fn denied_join_stays_denied() {
    let (auth, queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    let request = joins.create(peer("192.168.1.73")).unwrap();
    decide(&queue, AuthDecision::Deny);
    assert_eq!(poll(&joins, &request), JoinStatus::Denied);
    assert_eq!(poll(&joins, &request), JoinStatus::Denied);
  }

  #[test]
  fn abandoned_join_and_limiter_entry_expire() {
    let (auth, queue) = channel();
    let joins = JoinManager::with_abuse_limits(
      auth,
      GrantStore::new(),
      8,
      Duration::from_millis(30),
      2,
      Duration::ZERO,
    );
    let request = joins.create(peer("192.168.1.73")).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(poll(&joins, &request), JoinStatus::Gone);
    assert!(queue.poll().is_empty());
    assert_eq!(joins.limiter_len(), 0);
  }

  #[test]
  fn one_ip_cannot_fill_the_global_queue() {
    let (auth, _queue) = Authorizer::channel(16);
    let joins = JoinManager::with_abuse_limits(
      auth,
      GrantStore::new(),
      16,
      DEFAULT_JOIN_TTL,
      2,
      Duration::ZERO,
    );
    joins.create(peer("192.168.1.73")).unwrap();
    joins.create(peer("192.168.1.73")).unwrap();
    assert!(matches!(
      joins.create(peer("192.168.1.73")),
      Err(JoinError::RateLimited)
    ));
  }

  #[test]
  fn different_ips_can_join_concurrently() {
    let (auth, _queue) = Authorizer::channel(16);
    let joins = JoinManager::new(auth, GrantStore::new());
    assert!(joins.create(peer("192.168.1.73")).is_ok());
    assert!(joins.create(peer("192.168.1.74")).is_ok());
  }

  #[test]
  fn repeated_attempts_from_one_ip_are_throttled() {
    let (auth, _queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    let ip = "192.168.1.73".parse().unwrap();
    assert!(joins.record_failed_attempt(ip));
    assert!(!joins.record_failed_attempt(ip));
  }
}
