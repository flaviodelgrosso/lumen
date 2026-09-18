//! Host approval of incoming viewers.
//!
//! The server side holds an [`Authorizer`]; the CLI (or a test) consumes
//! [`AuthRequest`]s and answers each via its embedded oneshot. When no
//! approver is listening the outcome is **deny** (fail closed).

use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::peer::PeerInfo;

/// Host's verdict on a connection attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthDecision {
  /// Admit the viewer; WebRTC negotiation may start.
  Allow,
  /// Refuse the viewer.
  Deny,
}

/// A pending connection awaiting host approval.
#[derive(Debug)]
pub struct AuthRequest {
  /// Metadata of the requesting viewer.
  pub peer: PeerInfo,
  /// Answer the request here.
  pub respond: oneshot::Sender<AuthDecision>,
}

/// Errors while requesting approval.
#[derive(Debug, Error)]
pub enum AuthError {
  /// No host approver is attached (or it shut down).
  #[error("no host approver available")]
  NoApprover,
}

/// Server-side handle for requesting host approval of new viewers.
#[derive(Clone, Debug)]
pub struct Authorizer {
  tx: mpsc::Sender<AuthRequest>,
}

impl Authorizer {
  /// Create an `(authorizer, requests)` channel pair.
  #[must_use]
  pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<AuthRequest>) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (Self { tx }, rx)
  }

  /// Ask the host to approve `peer`, awaiting the verdict.
  ///
  /// # Errors
  ///
  /// Returns [`AuthError::NoApprover`] when the approver is gone; callers
  /// should treat that as a denial.
  pub async fn request(&self, peer: PeerInfo) -> Result<AuthDecision, AuthError> {
    let (respond, verdict) = oneshot::channel();
    self
      .tx
      .send(AuthRequest { peer, respond })
      .await
      .map_err(|_| AuthError::NoApprover)?;
    verdict.await.map_err(|_| AuthError::NoApprover)
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
    let (auth, mut requests) = Authorizer::channel(4);
    let host = tokio::spawn(async move {
      let req = requests.recv().await.expect("request");
      assert_eq!(req.peer.user_agent.as_deref(), Some("Safari/iPad"));
      req.respond.send(AuthDecision::Allow).expect("respond");
    });
    assert!(auth.is_allowed(peer()).await);
    host.await.unwrap();
  }

  #[tokio::test]
  async fn denied_viewer_is_denied() {
    let (auth, mut requests) = Authorizer::channel(4);
    let host = tokio::spawn(async move {
      let req = requests.recv().await.expect("request");
      req.respond.send(AuthDecision::Deny).expect("respond");
    });
    assert!(!auth.is_allowed(peer()).await);
    host.await.unwrap();
  }

  #[tokio::test]
  async fn missing_approver_denies() {
    let (auth, requests) = Authorizer::channel(4);
    drop(requests);
    assert!(!auth.is_allowed(peer()).await);
  }

  #[tokio::test]
  async fn approver_dropping_reply_denies() {
    let (auth, mut requests) = Authorizer::channel(4);
    let host = tokio::spawn(async move {
      let req = requests.recv().await.expect("request");
      drop(req.respond); // host vanished mid-approval
    });
    assert!(!auth.is_allowed(peer()).await);
    host.await.unwrap();
  }
}
