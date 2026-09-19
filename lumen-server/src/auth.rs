//! Host approval of incoming viewers.
//!
//! The server side holds an [`Authorizer`]; incoming viewers are parked in
//! a shared [`ApprovalQueue`] until a host surface answers them. Approval
//! consumers — the CLI terminal loop and the admin dashboard API — poll the
//! same queue and decide by peer id; the first decision wins. Entries whose
//! viewer socket vanished (oneshot receiver dropped) are pruned on every
//! queue touch. A saturated queue answers **deny** immediately (fail closed).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::futures::Notified;
use tokio::sync::{Notify, oneshot};

use crate::peer::PeerInfo;
use crate::token::PeerId;

/// Host's verdict on a connection attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthDecision {
  /// Admit the viewer; WebRTC negotiation may start.
  Allow,
  /// Refuse the viewer.
  Deny,
}

/// Errors while requesting approval.
#[derive(Debug, Error)]
pub enum AuthError {
  /// The approval queue is saturated: no host can answer in time.
  #[error("no host approver available")]
  NoApprover,
}

/// A pending approval as observed by pollers (terminal loop, dashboard).
#[derive(Clone, Debug)]
pub struct PendingView {
  /// Requesting viewer's identity.
  pub id: PeerId,
  /// Metadata of the requesting viewer.
  pub peer: PeerInfo,
  /// How long the request has been waiting.
  pub waited: Duration,
}

#[derive(Debug)]
struct PendingEntry {
  info: PeerInfo,
  since: Instant,
  respond: Option<oneshot::Sender<AuthDecision>>,
}

#[derive(Debug)]
struct QueueInner {
  pending: Vec<PendingEntry>,
  max_pending: usize,
}

/// Shared pool of viewers awaiting host approval.
///
/// Cheap to clone: every clone sees the same pending set. The server holds
/// one via [`Authorizer`]; approval surfaces hold another.
#[derive(Clone, Debug)]
pub struct ApprovalQueue {
  inner: Arc<Mutex<QueueInner>>,
  changed: Arc<Notify>,
}

impl ApprovalQueue {
  fn new(max_pending: usize) -> Self {
    Self {
      inner: Arc::new(Mutex::new(QueueInner {
        pending: Vec::new(),
        max_pending: max_pending.max(1),
      })),
      changed: Arc::new(Notify::new()),
    }
  }

  fn lock(&self) -> MutexGuard<'_, QueueInner> {
    self.inner.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Park `peer` and return the verdict receiver.
  ///
  /// `Err(NoApprover)` when the queue is saturated; the caller must treat
  /// that as a denial.
  fn submit(&self, peer: PeerInfo) -> Result<oneshot::Receiver<AuthDecision>, AuthError> {
    let (respond, verdict) = oneshot::channel();
    {
      let mut inner = self.lock();
      prune(&mut inner);
      if inner.pending.len() >= inner.max_pending {
        return Err(AuthError::NoApprover);
      }
      inner.pending.push(PendingEntry {
        info: peer,
        since: Instant::now(),
        respond: Some(respond),
      });
    }
    self.changed.notify_waiters();
    Ok(verdict)
  }

  /// Snapshot the pending requests (oldest first), pruning vanished ones.
  #[must_use]
  pub fn poll(&self) -> Vec<PendingView> {
    let mut inner = self.lock();
    prune(&mut inner);
    inner
      .pending
      .iter()
      .map(|entry| PendingView {
        id: entry.info.id,
        peer: entry.info.clone(),
        waited: entry.since.elapsed(),
      })
      .collect()
  }

  /// Answer the pending request for `id`.
  ///
  /// Returns `false` when the entry is gone (already decided, or the
  /// viewer vanished).
  #[must_use]
  pub fn decide(&self, id: PeerId, decision: AuthDecision) -> bool {
    let respond = {
      let mut inner = self.lock();
      let index = inner.pending.iter().position(|entry| entry.info.id == id);
      match index {
        Some(index) => inner.pending.remove(index).respond,
        None => None,
      }
    };
    let Some(respond) = respond else {
      return false;
    };
    self.changed.notify_waiters();
    respond.send(decision).is_ok()
  }

  /// Resolves when the pending set changes (new request or decision).
  ///
  /// Enable the waiter *before* checking [`poll`](Self::poll) /
  /// [`decide`](Self::decide) to avoid missing a wake between check and
  /// await.
  pub fn wait_for_change(&self) -> Notified<'_> {
    self.changed.notified()
  }
}

/// Drop entries whose viewer stopped awaiting the verdict.
fn prune(inner: &mut QueueInner) {
  inner
    .pending
    .retain(|entry| entry.respond.as_ref().is_some_and(|tx| !tx.is_closed()));
}

/// Server-side handle for requesting host approval of new viewers.
#[derive(Clone, Debug)]
pub struct Authorizer {
  queue: ApprovalQueue,
}

impl Authorizer {
  /// Create an `(authorizer, queue)` pair. `max_pending` bounds the number
  /// of simultaneously waiting viewers; beyond it new requests are denied
  /// (fail closed).
  #[must_use]
  pub fn channel(max_pending: usize) -> (Self, ApprovalQueue) {
    let queue = ApprovalQueue::new(max_pending);
    (
      Self {
        queue: queue.clone(),
      },
      queue,
    )
  }

  /// Ask the host to approve `peer`, awaiting the verdict.
  ///
  /// # Errors
  ///
  /// Returns [`AuthError::NoApprover`] when the queue is saturated; callers
  /// should treat that as a denial.
  pub async fn request(&self, peer: PeerInfo) -> Result<AuthDecision, AuthError> {
    let verdict = self.queue.submit(peer)?;
    verdict.await.map_err(|_| AuthError::NoApprover)
  }

  /// Park `peer` in the approval queue without awaiting the verdict.
  ///
  /// The returned receiver resolves with the host's decision; dropping
  /// it (abandoned viewer) removes the request from the queue on the
  /// next prune. Used by the join flow, where the verdict is observed
  /// by repeated polls instead of one long-lived socket.
  ///
  /// # Errors
  ///
  /// Returns [`AuthError::NoApprover`] when the queue is saturated;
  /// callers must treat that as a denial.
  pub fn park(&self, peer: PeerInfo) -> Result<oneshot::Receiver<AuthDecision>, AuthError> {
    self.queue.submit(peer)
  }

  /// Approval helper: `Allow` only when the host explicitly allows.
  pub async fn is_allowed(&self, peer: PeerInfo) -> bool {
    matches!(self.request(peer).await, Ok(AuthDecision::Allow))
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

  #[tokio::test]
  async fn allowed_viewer_is_allowed() {
    let (auth, queue) = Authorizer::channel(4);
    let host = tokio::spawn(async move {
      for _ in 0..50 {
        if let Some(pending) = queue.poll().into_iter().next() {
          assert_eq!(pending.peer.user_agent.as_deref(), Some("Safari/iPad"));
          assert!(queue.decide(pending.id, AuthDecision::Allow));
          return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
      panic!("no pending request observed");
    });
    assert!(auth.is_allowed(peer()).await);
    host.await.unwrap();
  }

  #[tokio::test]
  async fn denied_viewer_is_denied() {
    let (auth, queue) = Authorizer::channel(4);
    let host = tokio::spawn(async move {
      for _ in 0..50 {
        if let Some(pending) = queue.poll().into_iter().next() {
          assert!(queue.decide(pending.id, AuthDecision::Deny));
          return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
      }
      panic!("no pending request observed");
    });
    assert!(!auth.is_allowed(peer()).await);
    host.await.unwrap();
  }

  #[tokio::test]
  async fn first_decision_wins() {
    let (_auth, queue) = Authorizer::channel(4);
    let peer = peer();
    let id = peer.id;
    let verdict = queue.submit(peer).expect("parks");
    assert!(queue.decide(id, AuthDecision::Allow));
    assert!(!queue.decide(id, AuthDecision::Deny), "gone after decision");
    assert_eq!(verdict.await.expect("verdict"), AuthDecision::Allow);
  }

  #[tokio::test]
  async fn saturated_queue_denies() {
    let (_auth, queue) = Authorizer::channel(1);
    let _parked = queue.submit(peer()).expect("first parks");
    let second = queue.submit(peer());
    assert!(matches!(second, Err(AuthError::NoApprover)));
  }

  #[tokio::test]
  async fn vanished_viewer_is_pruned() {
    let (_auth, queue) = Authorizer::channel(4);
    let verdict = queue.submit(peer()).expect("parks");
    drop(verdict); // viewer socket closed mid-wait
    assert!(queue.poll().is_empty(), "closed verdict is pruned on poll");
  }

  #[tokio::test]
  async fn waiter_wakes_on_new_request() {
    let (_auth, queue) = Authorizer::channel(4);
    let waiter = queue.wait_for_change();
    tokio::pin!(waiter);
    waiter.as_mut().enable();
    let _parked = queue.submit(peer()).expect("parks");
    tokio::select! {
      () = waiter => {}
      () = tokio::time::sleep(Duration::from_secs(1)) => panic!("not woken"),
    }
  }
}
