//! Session tokens, authorization, peer registry and the typed signaling
//! protocol.
//!
//! The signaling enums are the single source of truth for the WebSocket
//! JSON format shared by [`lumen_server`](https://docs.rs/lumen-server) and
//! the embedded browser viewer.

pub mod auth;
pub mod peer;
pub mod signal;
pub mod token;
pub mod ua;
pub use auth::{ApprovalQueue, AuthDecision, AuthError, Authorizer, PendingView};
pub use peer::{PeerInfo, PeerRegistry};
pub use signal::{HostMessage, SignalMessage, ViewerMessage};
pub use token::{PeerId, SessionToken};
pub use ua::describe_user_agent;
