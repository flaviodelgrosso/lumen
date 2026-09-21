//! Errors covering one full session lifecycle.

use lumen_core::ConfigError;
use lumen_media::capture::CaptureError;
use lumen_media::encoder::EncodeError;
use lumen_server::ServerError;
use lumen_server::token::TokenError;

use crate::network::NetworkError;
use crate::pipeline::PipelineError;

/// Failure starting, serving, or stopping a sharing session.
///
/// mDNS advertisement failures are deliberately absent: discovery is
/// best-effort and only ever degrades the viewer URL to the LAN IP.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
  /// Resolved stream configuration is invalid.
  #[error(transparent)]
  Config(#[from] ConfigError),
  /// Display/window capture failed.
  #[error(transparent)]
  Capture(#[from] CaptureError),
  /// The video encoder could not be created.
  #[error(transparent)]
  Encoder(#[from] EncodeError),
  /// The capture→encode pipeline failed before the first frame.
  #[error(transparent)]
  Pipeline(#[from] PipelineError),
  /// LAN interface discovery or selection failed.
  #[error(transparent)]
  Network(#[from] NetworkError),
  /// OS randomness for session secrets failed.
  #[error(transparent)]
  Token(#[from] TokenError),
  /// The HTTP/signaling server failed to start or stopped abnormally.
  #[error(transparent)]
  Server(#[from] ServerError),
}
