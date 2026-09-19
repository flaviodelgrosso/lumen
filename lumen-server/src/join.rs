//! Join requests: the tokenless entry path into the approval queue.
//!
//! Viewers enter through `/` and `POST /api/join` — no secret, no URL
//! token. A join parks the viewer in the shared [`ApprovalQueue`] (the
//! same pool the terminal prompt and the admin dashboard poll), so the
//! host approval flow stays the sole authority. The viewer then polls
//! `GET /api/join/<id>`; when the host allows, the manager issues an
//! ephemeral [`GrantStore`] grant the viewer redeems on the signaling
//! socket. A viewer that stops polling is pruned after a TTL, which also
//! drops its verdict and removes it from the pending queue. The number of
//! live sessions is bounded: saturated requests answer fail-closed, so a
//! cheap `POST` cannot pile up unbounded state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::oneshot;

use crate::auth::{AuthDecision, Authorizer};
use crate::grant::GrantStore;
use crate::peer::PeerInfo;
use crate::token::PeerId;

/// Default cap on simultaneously live join sessions.
pub const DEFAULT_MAX_JOINS: usize = 32;
/// Default lifetime of a join session that stopped polling.
pub const DEFAULT_JOIN_TTL: Duration = Duration::from_secs(60);

/// Errors creating a join request.
#[derive(Debug, Error)]
pub enum JoinError {
  /// Too many live join sessions: the caller must back off (rate limit).
  #[error("too many pending join requests")]
  Full,
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
  /// The request is gone (expired or unknown): join again.
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
  /// Host verdict receiver; taken once observed. Dropping the session
  /// drops it, which prunes the entry from the approval queue.
  verdict: Option<oneshot::Receiver<AuthDecision>>,
  state: JoinState,
  /// Last poll; abandoned sessions expire after the join TTL.
  last_poll: Instant,
}

/// Live join sessions keyed by peer id.
///
/// Cheap to clone: every clone sees the same session set. The HTTP
/// handlers drive it; the [`GrantStore`] carries the capability out.
#[derive(Clone, Debug)]
pub struct JoinManager {
  sessions: Arc<Mutex<HashMap<PeerId, JoinSession>>>,
  authorizer: Authorizer,
  grants: GrantStore,
  max_joins: usize,
  join_ttl: Duration,
}

impl JoinManager {
  /// New manager with the default bounds.
  #[must_use]
  pub fn new(authorizer: Authorizer, grants: GrantStore) -> Self {
    Self::with_limits(authorizer, grants, DEFAULT_MAX_JOINS, DEFAULT_JOIN_TTL)
  }

  /// New manager with explicit bounds (tests inject tiny limits).
  #[must_use]
  pub fn with_limits(
    authorizer: Authorizer,
    grants: GrantStore,
    max_joins: usize,
    join_ttl: Duration,
  ) -> Self {
    Self {
      sessions: Arc::new(Mutex::new(HashMap::new())),
      authorizer,
      grants,
      max_joins: max_joins.max(1),
      join_ttl,
    }
  }

  fn lock(&self) -> MutexGuard<'_, HashMap<PeerId, JoinSession>> {
    self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Park a new join request in the approval queue.
  ///
  /// # Errors
  ///
  /// - [`JoinError::Full`] when the live-session bound is reached; the
  ///   caller must back off.
  /// - [`JoinError::NoApprover`] when the approval queue is saturated.
  /// - [`JoinError::Random`] when the OS CSPRNG fails.
  pub fn create(&self, info: PeerInfo) -> Result<PeerId, JoinError> {
    let mut sessions = self.lock();
    let now = Instant::now();
    prune(&mut sessions, now, self.join_ttl);
    if sessions.len() >= self.max_joins {
      return Err(JoinError::Full);
    }
    let verdict = self.authorizer.park(info.clone())?;
    let id = info.id;
    sessions.insert(
      id,
      JoinSession {
        info,
        verdict: Some(verdict),
        state: JoinState::Pending,
        last_poll: now,
      },
    );
    Ok(id)
  }

  /// Poll a join request; issues the grant once the host has allowed.
  ///
  /// Repeated polls before redemption return the same grant; once the
  /// grant is redeemed or expired the session is gone and the viewer must
  /// join again.
  pub fn poll(&self, id: PeerId) -> JoinStatus {
    let mut sessions = self.lock();
    let now = Instant::now();
    prune(&mut sessions, now, self.join_ttl);
    let Some(session) = sessions.get_mut(&id) else {
      return JoinStatus::Gone;
    };
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
                // Fail closed: without randomness there is no grant.
                tracing::error!("grant issuance failed: {e}");
                session.state = JoinState::Denied;
                JoinStatus::Denied
              }
            }
          }
          Ok(AuthDecision::Deny) | Err(oneshot::error::TryRecvError::Closed) => {
            // A vanished verdict channel means the queue entry was pruned
            // without a decision: deny (fail closed).
            session.verdict = None;
            session.state = JoinState::Denied;
            JoinStatus::Denied
          }
          Err(oneshot::error::TryRecvError::Empty) => JoinStatus::Pending,
        }
      }
    };
    if matches!(status, JoinStatus::Gone) {
      sessions.remove(&id);
    }
    status
  }

  /// Live session count.
  #[must_use]
  pub fn len(&self) -> usize {
    let now = Instant::now();
    self
      .lock()
      .values()
      .filter(|session| now.duration_since(session.last_poll) < self.join_ttl)
      .count()
  }

  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

/// Drop sessions whose viewer stopped polling: the verdict receiver goes
/// with them, which prunes the approval queue on its next touch.
fn prune(sessions: &mut HashMap<PeerId, JoinSession>, now: Instant, ttl: Duration) {
  sessions.retain(|_, session| now.duration_since(session.last_poll) < ttl);
}

#[cfg(test)]
mod tests {
  use std::time::Instant;

  use super::*;
  use crate::auth::ApprovalQueue;

  fn channel() -> (Authorizer, ApprovalQueue) {
    Authorizer::channel(16)
  }

  fn peer() -> PeerInfo {
    PeerInfo {
      id: PeerId::generate().unwrap(),
      address: Some("192.168.1.73".parse().unwrap()),
      user_agent: Some("Safari/iPad".to_owned()),
      joined_at: Instant::now(),
    }
  }

  /// Answer the first pending approval with `decision`.
  fn decide(queue: &ApprovalQueue, decision: AuthDecision) {
    let pending = queue.poll().into_iter().next().expect("pending");
    assert!(queue.decide(pending.id, decision));
  }

  #[test]
  fn new_join_is_pending() {
    let (auth, queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    let id = joins.create(peer()).unwrap();
    assert_eq!(joins.poll(id), JoinStatus::Pending);
    assert_eq!(queue.poll().len(), 1, "the join parked in the queue");
  }

  #[test]
  fn approved_join_issues_a_stable_single_use_grant() {
    let (auth, queue) = channel();
    let grants = GrantStore::new();
    let joins = JoinManager::new(auth, grants.clone());
    let id = joins.create(peer()).unwrap();
    decide(&queue, AuthDecision::Allow);
    let JoinStatus::Approved { grant } = joins.poll(id) else {
      panic!("expected approved");
    };
    assert!(!grant.is_empty());
    assert_eq!(
      joins.poll(id),
      JoinStatus::Approved {
        grant: grant.clone()
      },
      "the same grant is returned until redeemed"
    );
    assert_eq!(grants.redeem(&grant).unwrap().id, id);
    assert!(
      grants.redeem(&grant).is_none(),
      "the grant is single-use at the store"
    );
    assert_eq!(
      joins.poll(id),
      JoinStatus::Gone,
      "a redeemed grant retires the session"
    );
  }

  #[test]
  fn expired_grant_retires_the_session() {
    let (auth, queue) = channel();
    let grants = GrantStore::with_ttl(Duration::from_millis(30));
    let joins = JoinManager::new(auth, grants.clone());
    let id = joins.create(peer()).unwrap();
    decide(&queue, AuthDecision::Allow);
    let JoinStatus::Approved { grant } = joins.poll(id) else {
      panic!("expected approved");
    };
    std::thread::sleep(Duration::from_millis(60));
    assert!(
      grants.redeem(&grant).is_none(),
      "an expired grant never redeems"
    );
    assert_eq!(joins.poll(id), JoinStatus::Gone);
  }
  #[test]
  fn denied_join_stays_denied() {
    let (auth, queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    let id = joins.create(peer()).unwrap();
    decide(&queue, AuthDecision::Deny);
    assert_eq!(joins.poll(id), JoinStatus::Denied);
    assert_eq!(joins.poll(id), JoinStatus::Denied);
  }

  #[test]
  fn unknown_join_is_gone() {
    let (auth, _queue) = channel();
    let joins = JoinManager::new(auth, GrantStore::new());
    assert_eq!(joins.poll(PeerId::generate().unwrap()), JoinStatus::Gone);
  }

  #[test]
  fn abandoned_join_expires() {
    let (auth, queue) = channel();
    let joins = JoinManager::with_limits(auth, GrantStore::new(), 8, Duration::from_millis(30));
    let id = joins.create(peer()).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(joins.poll(id), JoinStatus::Gone);
    assert!(
      queue.poll().is_empty(),
      "dropping the session must clear the approval queue"
    );
  }

  #[test]
  fn live_polls_keep_a_join_alive() {
    let (auth, _queue) = channel();
    let joins = JoinManager::with_limits(auth, GrantStore::new(), 8, Duration::from_millis(60));
    let id = joins.create(peer()).unwrap();
    for _ in 0..4 {
      std::thread::sleep(Duration::from_millis(20));
      assert_eq!(joins.poll(id), JoinStatus::Pending, "polling keeps it live");
    }
  }

  #[test]
  fn bounded_sessions_reject_new_joins() {
    let (auth, _queue) = channel();
    let joins = JoinManager::with_limits(auth, GrantStore::new(), 2, Duration::from_secs(60));
    joins.create(peer()).unwrap();
    joins.create(peer()).unwrap();
    assert!(matches!(joins.create(peer()), Err(JoinError::Full)));
  }

  #[test]
  fn saturated_approval_queue_is_no_approver() {
    let (auth, _queue) = Authorizer::channel(1);
    let joins = JoinManager::new(auth, GrantStore::new());
    joins.create(peer()).unwrap();
    assert!(matches!(joins.create(peer()), Err(JoinError::NoApprover)));
  }
}
