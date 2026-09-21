//! Explicit session state machine shared by desktop and CLI frontends.
//!
//! [`SessionController`] wraps [`crate::session::start_session`] in one
//! serialized phase lock and an observable state:
//!
//! ```text
//! Idle ──start()──▶ Starting ──ok──▶ Running ──stop()──▶ Stopping ──▶ Idle
//!                     │                                      ▲
//!                     └──fail──▶ Error ──start() (retry)─────┘
//! ```
//!
//! Invalid concurrent operations (`start` while starting/running/stopping,
//! `stop` while idle/stopping) are rejected by the phase itself, not by
//! menu enablement. All transitions happen under one lock; long-running
//! start/stop work runs on spawned tasks so callers never block on
//! capture, media or network work.
//!
//! All methods must run inside a Tokio runtime (the controller spawns the
//! engine work onto the current runtime).

use std::sync::Arc;

use parking_lot::Mutex;

use futures_util::future::BoxFuture;
use tokio::sync::watch;

use crate::error::SessionError;
use crate::session::{SessionConfig, start_session};

/// Observable lifecycle state of a managed session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
  /// No session; `start` is allowed.
  Idle,
  /// A session is being brought up; `stop` cancels the attempt.
  Starting,
  /// A session is streaming; `stop` is allowed.
  Running,
  /// A session (or a canceled start) is being torn down.
  Stopping,
  /// The last start failed; `start` retries.
  Error,
}

impl SessionState {
  /// Whether the state machine currently accepts `start`.
  #[must_use]
  pub fn can_start(self) -> bool {
    matches!(self, Self::Idle | Self::Error)
  }

  /// Whether the state machine currently accepts `stop`.
  #[must_use]
  pub fn can_stop(self) -> bool {
    matches!(self, Self::Starting | Self::Running)
  }

  /// Whether live session URLs (viewer, dashboard) are available.
  #[must_use]
  pub fn has_urls(self) -> bool {
    self == Self::Running
  }
}

/// Snapshot of the controller's observable state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStatus {
  /// Current lifecycle state.
  pub state: SessionState,
  /// Message of the last startup failure (`Error` state only).
  pub error: Option<String>,
}

/// Rejected control operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
  /// A session is already being brought up.
  #[error("a session is already starting")]
  Starting,
  /// A session is already streaming.
  #[error("a session is already running")]
  Running,
  /// Teardown is already in progress.
  #[error("the session is already stopping")]
  Stopping,
  /// No session to stop.
  #[error("no session is active")]
  NotRunning,
}

/// A live session as the state machine sees it: its URLs plus a one-shot
/// graceful stop.
pub struct RunningSession {
  /// Canonical viewer URL (no secret).
  pub viewer_url: String,
  /// Admin dashboard URL including the session token (capability URL).
  pub dashboard_url: String,
  stop: Box<dyn FnOnce() -> BoxFuture<'static, Result<(), SessionError>> + Send>,
}

impl RunningSession {
  /// Wrap a live session: its URLs and a stop future that runs exactly once.
  #[must_use]
  pub fn new(
    viewer_url: String,
    dashboard_url: String,
    stop: impl Future<Output = Result<(), SessionError>> + Send + 'static,
  ) -> Self {
    Self {
      viewer_url,
      dashboard_url,
      stop: Box::new(move || Box::pin(stop)),
    }
  }

  /// Stop the session gracefully. Consumes the handle: stop is one-shot.
  ///
  /// # Errors
  ///
  /// Propagates the underlying teardown failure.
  pub async fn stop(self) -> Result<(), SessionError> {
    (self.stop)().await
  }
}

/// The machinery behind one sharing session.
#[async_trait::async_trait]
pub trait Engine: Send + Sync + 'static {
  /// Bring up a session; the returned handle carries its URLs and teardown.
  ///
  /// # Errors
  ///
  /// See [`SessionError`].
  async fn start(&self, cfg: &SessionConfig) -> Result<RunningSession, SessionError>;
}

/// The production engine: the real Lumen capture → encode → server pipeline.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveEngine;

#[async_trait::async_trait]
impl Engine for LiveEngine {
  async fn start(&self, cfg: &SessionConfig) -> Result<RunningSession, SessionError> {
    let session = start_session(cfg.clone()).await?;
    let (viewer_url, dashboard_url) = (
      session.info.viewer_url.clone(),
      session.info.admin_url.clone(),
    );
    Ok(RunningSession::new(
      viewer_url,
      dashboard_url,
      session.shutdown(),
    ))
  }
}

/// Internal phase; the failure message lives in the watch status.
enum Phase {
  Idle,
  Starting,
  Running(RunningSession),
  Stopping,
  Error,
}

/// Shared controller handle driving one session at a time.
///
/// Cheap to clone; all clones share the phase and the state watch.
#[derive(Clone)]
pub struct SessionController {
  phase: Arc<Mutex<Phase>>,
  status_tx: watch::Sender<SessionStatus>,
  /// Internal subscriber: keeps the watch channel open so publishes land
  /// (a `send` on a channel with zero receivers is dropped by tokio) and
  /// `borrow()` stays authoritative.
  _status_rx: watch::Receiver<SessionStatus>,
  engine: Arc<dyn Engine>,
  config: SessionConfig,
}

impl SessionController {
  /// Create a controller bound to `engine` and a fixed `config`.
  #[must_use]
  pub fn new(engine: Arc<dyn Engine>, config: SessionConfig) -> Self {
    let (status_tx, status_rx) = watch::channel(SessionStatus {
      state: SessionState::Idle,
      error: None,
    });
    Self {
      phase: Arc::new(Mutex::new(Phase::Idle)),
      status_tx,
      _status_rx: status_rx,
      engine,
      config,
    }
  }

  /// Current observable status.
  #[must_use]
  pub fn status(&self) -> SessionStatus {
    self.status_tx.borrow().clone()
  }

  /// Current lifecycle state.
  #[must_use]
  pub fn state(&self) -> SessionState {
    self.status_tx.borrow().state
  }

  /// Message of the last startup failure, if any.
  #[must_use]
  pub fn error(&self) -> Option<String> {
    self.status_tx.borrow().error.clone()
  }

  /// Subscribe to state changes (coalesced watch channel).
  #[must_use]
  pub fn subscribe(&self) -> watch::Receiver<SessionStatus> {
    self.status_tx.subscribe()
  }

  /// Viewer URL of the running session, `None` unless [`SessionState::Running`].
  #[must_use]
  pub fn viewer_url(&self) -> Option<String> {
    let phase = self.phase.lock();
    match &*phase {
      Phase::Running(running) => Some(running.viewer_url.clone()),
      _ => None,
    }
  }

  /// Admin dashboard URL (with token) of the running session.
  #[must_use]
  pub fn dashboard_url(&self) -> Option<String> {
    let phase = self.phase.lock();
    match &*phase {
      Phase::Running(running) => Some(running.dashboard_url.clone()),
      _ => None,
    }
  }

  /// Bring up a session with the configured settings.
  ///
  /// Returns as soon as the phase flips to `Starting`: the actual capture /
  /// server bring-up runs on a spawned task, so UI loops never block on
  /// media or network work. Outcome (or failure) is observable via
  /// [`SessionController::subscribe`] / [`SessionController::state`].
  ///
  /// # Errors
  ///
  /// [`ControlError`] when a start is invalid for the current phase
  /// (already starting, running or stopping). Startup failures are *not*
  /// returned here — they land in the `Error` state.
  pub fn start(&self) -> Result<(), ControlError> {
    let mut phase = self.phase.lock();
    match &*phase {
      Phase::Idle | Phase::Error => {}
      Phase::Starting => return Err(ControlError::Starting),
      Phase::Running(_) => return Err(ControlError::Running),
      Phase::Stopping => return Err(ControlError::Stopping),
    }
    *phase = Phase::Starting;
    drop(phase);
    self.publish(SessionState::Starting, None);

    let engine = Arc::clone(&self.engine);
    let cfg = self.config.clone();
    let phase = Arc::clone(&self.phase);
    let status_tx = self.status_tx.clone();
    tokio::spawn(async move {
      let outcome = engine.start(&cfg).await;
      let canceled = {
        let mut guard = phase.lock();
        if matches!(&*guard, Phase::Starting) {
          match outcome {
            Ok(running) => {
              *guard = Phase::Running(running);
              drop(guard);
              publish(&status_tx, SessionState::Running, None);
            }
            Err(error) => {
              tracing::error!("session start failed: {error}");
              let message = error.to_string();
              *guard = Phase::Error;
              drop(guard);
              publish(&status_tx, SessionState::Error, Some(message));
            }
          }
          return;
        }
        // `stop` raced the startup: the session is no longer wanted.
        outcome.ok()
      };
      if let Some(running) = canceled
        && let Err(error) = running.stop().await
      {
        tracing::error!("canceled session teardown failed: {error}");
      }
      let mut guard = phase.lock();
      if matches!(&*guard, Phase::Stopping) {
        *guard = Phase::Idle;
        drop(guard);
        publish(&status_tx, SessionState::Idle, None);
      }
    });
    Ok(())
  }

  /// Tear the session (or a canceled startup) down gracefully.
  ///
  /// Returns as soon as the phase flips to `Stopping`; teardown runs on a
  /// spawned task and the state reaches `Idle` only after the engine has
  /// fully stopped.
  ///
  /// # Errors
  ///
  /// [`ControlError`] when nothing is running or teardown is already under
  /// way.
  pub fn stop(&self) -> Result<(), ControlError> {
    let mut guard = self.phase.lock();
    let running = match &*guard {
      Phase::Running(_) => match std::mem::replace(&mut *guard, Phase::Stopping) {
        Phase::Running(running) => Some(running),
        _ => unreachable!("phase checked as Running"),
      },
      // A startup is in flight: mark the intent; the start task tears the
      // session down (or just resolves) and lands in `Idle`.
      Phase::Starting => {
        *guard = Phase::Stopping;
        drop(guard);
        publish(&self.status_tx, SessionState::Stopping, None);
        return Ok(());
      }
      Phase::Stopping => return Err(ControlError::Stopping),
      Phase::Idle | Phase::Error => return Err(ControlError::NotRunning),
    };
    drop(guard);
    publish(&self.status_tx, SessionState::Stopping, None);

    let phase = Arc::clone(&self.phase);
    let status_tx = self.status_tx.clone();
    tokio::spawn(async move {
      if let Some(running) = running
        && let Err(error) = running.stop().await
      {
        tracing::error!("session stop failed: {error}");
      }
      let mut guard = phase.lock();
      if matches!(&*guard, Phase::Stopping) {
        *guard = Phase::Idle;
        drop(guard);
        publish(&status_tx, SessionState::Idle, None);
      }
    });
    Ok(())
  }

  /// Stop whatever is running and wait until the controller is fully idle.
  ///
  /// Used by quit paths: no session can outlive this call.
  pub async fn stop_and_wait(&self) {
    if self.state().can_stop() {
      // A concurrent stop that beats us is fine: we wait for `Idle` below.
      let _ = self.stop();
    }
    let mut rx = self.subscribe();
    loop {
      {
        let status = rx.borrow_and_update();
        if status.state == SessionState::Idle || status.state == SessionState::Error {
          return;
        }
      }
      if rx.changed().await.is_err() {
        return;
      }
    }
  }

  fn publish(&self, state: SessionState, error: Option<String>) {
    publish(&self.status_tx, state, error);
  }
}

fn publish(status_tx: &watch::Sender<SessionStatus>, state: SessionState, error: Option<String>) {
  let _ = status_tx.send(SessionStatus { state, error });
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

  use tokio::sync::watch;

  use super::*;
  use lumen_core::ConfigError;

  /// Engine whose starts can be gated and failed on demand, counting
  /// successful starts and completed stops.
  struct FakeEngine {
    gate: Mutex<Option<watch::Receiver<bool>>>,
    fail_starts: AtomicUsize,
    started: AtomicUsize,
    stopped: Arc<AtomicUsize>,
    stop_delay_us: AtomicU64,
  }

  impl FakeEngine {
    fn new() -> Arc<Self> {
      Arc::new(Self {
        gate: Mutex::new(None),
        fail_starts: AtomicUsize::new(0),
        started: AtomicUsize::new(0),
        stopped: Arc::new(AtomicUsize::new(0)),
        stop_delay_us: AtomicU64::new(0),
      })
    }

    /// Engine whose *next* start waits until the returned sender is set.
    fn gated() -> (Arc<Self>, watch::Sender<bool>) {
      let engine = Self::new();
      let (tx, rx) = watch::channel(false);
      *engine.gate.lock() = Some(rx);
      (engine, tx)
    }
  }

  #[async_trait::async_trait]
  impl Engine for FakeEngine {
    async fn start(&self, _cfg: &SessionConfig) -> Result<RunningSession, SessionError> {
      let gated = { self.gate.lock().take() };
      if let Some(mut rx) = gated {
        while !*rx.borrow_and_update() {
          if rx.changed().await.is_err() {
            break;
          }
        }
      }
      if self.fail_starts.load(Ordering::Relaxed) > 0 {
        self.fail_starts.fetch_sub(1, Ordering::Relaxed);
        return Err(SessionError::Config(ConfigError::InvalidFps { value: 0 }));
      }
      self.started.fetch_add(1, Ordering::Relaxed);
      let stopped = Arc::clone(&self.stopped);
      let delay = std::time::Duration::from_micros(self.stop_delay_us.load(Ordering::Relaxed));
      Ok(RunningSession::new(
        "http://lumen.local:3131".to_owned(),
        "http://127.0.0.1:3131/admin/secret".to_owned(),
        async move {
          tokio::time::sleep(delay).await;
          stopped.fetch_add(1, Ordering::Relaxed);
          Ok(())
        },
      ))
    }
  }

  fn controller(engine: Arc<FakeEngine>) -> SessionController {
    SessionController::new(engine, SessionConfig::default())
  }

  async fn wait_state(c: &SessionController, want: SessionState) {
    let mut rx = c.subscribe();
    while {
      let status = rx.borrow_and_update();
      status.state != want
    } {
      rx.changed().await.expect("controller dropped");
    }
  }

  #[tokio::test]
  async fn idle_to_running_to_idle() {
    let engine = FakeEngine::new();
    let c = controller(Arc::clone(&engine));
    assert_eq!(c.state(), SessionState::Idle);

    c.start().expect("start from idle");
    assert_eq!(c.state(), SessionState::Starting);
    wait_state(&c, SessionState::Running).await;
    assert_eq!(engine.started.load(Ordering::Relaxed), 1);

    c.stop().expect("stop from running");
    assert_eq!(c.state(), SessionState::Stopping);
    wait_state(&c, SessionState::Idle).await;
    assert_eq!(engine.stopped.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn startup_failure_lands_in_error_and_retry_recovers() {
    let engine = FakeEngine::new();
    engine.fail_starts.store(1, Ordering::Relaxed);
    let c = controller(Arc::clone(&engine));

    c.start().unwrap();
    wait_state(&c, SessionState::Error).await;
    assert!(c.error().is_some(), "error state must carry a message");
    assert_eq!(engine.started.load(Ordering::Relaxed), 0);

    // `Error` is recoverable: another start succeeds.
    c.start().expect("retry from Error");
    wait_state(&c, SessionState::Running).await;
    assert_eq!(c.error(), None);
    assert_eq!(engine.started.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn repeated_start_stop_cycles() {
    let engine = FakeEngine::new();
    let c = controller(Arc::clone(&engine));
    for round in 1..=3 {
      c.start().unwrap();
      wait_state(&c, SessionState::Running).await;
      assert_eq!(engine.started.load(Ordering::Relaxed), round);
      c.stop().unwrap();
      wait_state(&c, SessionState::Idle).await;
      assert_eq!(engine.stopped.load(Ordering::Relaxed), round);
    }
  }

  #[tokio::test]
  async fn duplicate_start_is_rejected() {
    let (engine, release) = FakeEngine::gated();
    let c = controller(Arc::clone(&engine));

    c.start().unwrap();
    assert_eq!(c.start().err(), Some(ControlError::Starting));
    release.send(true).unwrap();
    wait_state(&c, SessionState::Running).await;
    assert_eq!(c.start().err(), Some(ControlError::Running));
    assert_eq!(engine.started.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn duplicate_stop_is_rejected() {
    let engine = FakeEngine::new();
    engine.stop_delay_us.store(40_000, Ordering::Relaxed);
    let c = controller(Arc::clone(&engine));

    assert_eq!(c.stop().err(), Some(ControlError::NotRunning));
    c.start().unwrap();
    wait_state(&c, SessionState::Running).await;
    c.stop().unwrap();
    assert_eq!(c.stop().err(), Some(ControlError::Stopping));
    wait_state(&c, SessionState::Idle).await;
    assert_eq!(c.stop().err(), Some(ControlError::NotRunning));
  }

  #[tokio::test]
  async fn stop_during_starting_tears_down_the_session() {
    let (engine, release) = FakeEngine::gated();
    let c = controller(Arc::clone(&engine));

    c.start().unwrap();
    c.stop().expect("stopping a starting session is legal");
    assert_eq!(c.state(), SessionState::Stopping);
    // The start completes after the stop request: the session must be
    // created and then torn down again, landing in `Idle`.
    release.send(true).unwrap();
    wait_state(&c, SessionState::Idle).await;
    assert_eq!(engine.started.load(Ordering::Relaxed), 1);
    assert_eq!(engine.stopped.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn failed_start_during_pending_stop_lands_idle() {
    let (engine, release) = FakeEngine::gated();
    engine.fail_starts.store(1, Ordering::Relaxed);
    let c = controller(Arc::clone(&engine));

    c.start().unwrap();
    c.stop().unwrap();
    release.send(true).unwrap();
    wait_state(&c, SessionState::Idle).await;
    assert_eq!(engine.stopped.load(Ordering::Relaxed), 0);
  }

  #[tokio::test]
  async fn urls_track_the_running_session() {
    let engine = FakeEngine::new();
    let c = controller(Arc::clone(&engine));
    assert_eq!(c.viewer_url(), None);
    assert_eq!(c.dashboard_url(), None);

    c.start().unwrap();
    wait_state(&c, SessionState::Running).await;
    assert_eq!(c.viewer_url().as_deref(), Some("http://lumen.local:3131"));
    assert!(c.dashboard_url().unwrap().contains("/admin/"));

    c.stop().unwrap();
    wait_state(&c, SessionState::Idle).await;
    assert_eq!(c.viewer_url(), None);
    assert_eq!(c.dashboard_url(), None);
  }

  #[tokio::test]
  async fn idle_state_observed_only_after_teardown_completes() {
    let engine = FakeEngine::new();
    engine.stop_delay_us.store(60_000, Ordering::Relaxed);
    let c = controller(Arc::clone(&engine));
    c.start().unwrap();
    wait_state(&c, SessionState::Running).await;

    c.stop().unwrap();
    assert_eq!(engine.stopped.load(Ordering::Relaxed), 0);
    assert_eq!(c.state(), SessionState::Stopping);
    wait_state(&c, SessionState::Idle).await;
    assert_eq!(engine.stopped.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn stop_and_wait_settles_from_starting() {
    let (engine, release) = FakeEngine::gated();
    let c = controller(Arc::clone(&engine));
    c.start().unwrap();

    let waiter = {
      let c = c.clone();
      tokio::spawn(async move {
        c.stop_and_wait().await;
      })
    };
    // Give `stop_and_wait` a moment to request the stop, then let the
    // startup finish; teardown must resolve the waiter into `Idle`.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    release.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
      .await
      .expect("stop_and_wait must settle")
      .unwrap();
    assert_eq!(c.state(), SessionState::Idle);
    assert_eq!(engine.stopped.load(Ordering::Relaxed), 1);
  }

  #[tokio::test]
  async fn stop_and_wait_returns_immediately_when_idle() {
    let c = controller(FakeEngine::new());
    tokio::time::timeout(std::time::Duration::from_secs(1), c.stop_and_wait())
      .await
      .expect("idle controller settles instantly");
  }

  #[test]
  fn state_capability_predicates() {
    assert!(SessionState::Idle.can_start());
    assert!(SessionState::Error.can_start());
    assert!(!SessionState::Starting.can_start());
    assert!(!SessionState::Stopping.can_start());
    assert!(SessionState::Starting.can_stop());
    assert!(SessionState::Running.can_stop());
    assert!(!SessionState::Stopping.can_stop());
    assert!(!SessionState::Idle.can_stop());
    assert!(SessionState::Running.has_urls());
    assert!(!SessionState::Starting.has_urls());
  }
}
