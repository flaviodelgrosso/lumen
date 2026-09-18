//! Live pipeline counters shared between the capture/encode pipeline and
//! the server's admin surface.
//!
//! Counters are `Arc` cells: the pipeline holds the owning `Arc<PipelineStats>`
//! and any observer (HTTP status endpoint, terminal stats loop) reads the
//! same atomics through cheap clones.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};

/// Live counters for diagnostics.
#[derive(Debug, Default)]
pub struct PipelineStats {
  /// Frames produced by the capture source.
  pub captured: Arc<AtomicU64>,
  /// Access units produced by the encoder.
  pub encoded: Arc<AtomicU64>,
  /// Mean encode latency in microseconds (EMA).
  pub encode_latency_us: Arc<AtomicU64>,
  /// Broadcast fan-out capacity.
  pub fanout_capacity: Arc<AtomicU32>,
  /// Subscribers currently attached to the fan-out.
  pub subscribers: Arc<AtomicUsize>,
  /// PCM buffers produced by the audio capture source.
  pub audio_captured: Arc<AtomicU64>,
  /// Opus packets produced by the audio encoder.
  pub audio_encoded: Arc<AtomicU64>,
}
