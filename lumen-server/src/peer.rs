//! Live peer registry with per-viewer disconnect control.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::token::PeerId;

/// Metadata about one connected viewer.
#[derive(Clone, Debug)]
pub struct PeerInfo {
  /// Stable viewer identity.
  pub id: PeerId,
  /// Peer socket address, when known.
  pub address: Option<IpAddr>,
  /// `User-Agent` header, when present.
  pub user_agent: Option<String>,
  /// When the viewer attached to the registry.
  pub joined_at: Instant,
}

/// A registered viewer. Dropping the handle removes nothing; call
/// [`PeerRegistry::leave`] when the viewer's transport closes.
#[derive(Debug)]
pub struct PeerHandle {
  /// Viewer metadata.
  pub info: PeerInfo,
  /// Fired when the host asks for this viewer to be disconnected.
  pub disconnect: CancellationToken,
}

/// In-memory registry of connected viewers.
#[derive(Debug, Default)]
pub struct PeerRegistry {
  peers: Mutex<HashMap<PeerId, Arc<PeerHandle>>>,
}

impl PeerRegistry {
  #[must_use]
  pub fn new() -> Self {
    Self::default()
  }

  /// Register a viewer and return its handle.
  pub fn join(&self, info: PeerInfo) -> Arc<PeerHandle> {
    let handle = Arc::new(PeerHandle {
      info,
      disconnect: CancellationToken::new(),
    });
    if let Ok(mut peers) = self.peers.lock() {
      peers.insert(handle.info.id, Arc::clone(&handle));
    }
    handle
  }

  /// Remove a viewer from the registry (transport closed).
  pub fn leave(&self, id: PeerId) {
    if let Ok(mut peers) = self.peers.lock() {
      peers.remove(&id);
    }
  }

  /// Snapshot of registered viewers.
  pub fn list(&self) -> Vec<PeerInfo> {
    self
      .peers
      .lock()
      .map(|peers| peers.values().map(|h| h.info.clone()).collect())
      .unwrap_or_default()
  }

  #[must_use]
  pub fn count(&self) -> usize {
    self.peers.lock().map_or(0, |peers| peers.len())
  }

  /// Signal a viewer to disconnect without stopping the server.
  ///
  /// Returns `true` when the peer was present.
  pub fn disconnect(&self, id: PeerId) -> bool {
    let handle = self.peers.lock().ok().and_then(|p| p.get(&id).cloned());
    match handle {
      Some(handle) => {
        handle.disconnect.cancel();
        true
      }
      None => false,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn peer() -> PeerInfo {
    PeerInfo {
      id: PeerId::generate().unwrap(),
      address: Some("192.168.1.73".parse().unwrap()),
      user_agent: Some("Safari/iPad".to_owned()),
      joined_at: Instant::now(),
    }
  }

  #[tokio::test]
  async fn join_list_leave() {
    let reg = PeerRegistry::new();
    assert_eq!(reg.count(), 0);
    let a = reg.join(peer());
    let b = reg.join(peer());
    assert_eq!(reg.count(), 2);
    assert_eq!(reg.list().len(), 2);
    reg.leave(a.info.id);
    assert_eq!(reg.count(), 1);
    assert!(reg.list().iter().any(|p| p.id == b.info.id));
    reg.leave(b.info.id);
    assert_eq!(reg.count(), 0);
  }

  #[tokio::test]
  async fn disconnect_cancels_only_target_peer() {
    let reg = PeerRegistry::new();
    let a = reg.join(peer());
    let b = reg.join(peer());
    assert!(reg.disconnect(a.info.id));
    assert!(a.disconnect.is_cancelled());
    assert!(!b.disconnect.is_cancelled());
    assert!(!reg.disconnect(PeerId::generate().unwrap()));
  }
}
