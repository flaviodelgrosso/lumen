//! Reusable Lumen sharing session: capture → encode → WebRTC fan-out plus
//! the HTTP/signaling server, with an explicit lifecycle state machine.
//!
//! [`session::start_session`] runs the exact pipeline `lumen serve` uses,
//! without any terminal presentation; [`session::Session::shutdown`] tears
//! it down in the same deterministic order. [`controller::SessionController`]
//! wraps that engine in an `Idle → Starting → Running → Stopping → Idle`
//! state machine so multiple frontends (the CLI, the desktop tray app) can
//! drive the same session implementation without duplicating the pipeline
//! or racing each other.

pub mod controller;
pub mod error;
pub mod mdns;
pub mod network;
pub mod pipeline;
pub mod session;

pub use controller::{
  ControlError, Engine, LiveEngine, RunningSession, SessionController, SessionState, SessionStatus,
};
pub use error::SessionError;
pub use session::{DEFAULT_PORT, Session, SessionConfig, SessionInfo, start_session};
