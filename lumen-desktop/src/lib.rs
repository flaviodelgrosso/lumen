//! Native menu-bar / system-tray frontend for Lumen.
//!
//! The tray drives the shared `lumen-session` state machine directly; it
//! never spawns `lumen serve`. The calling binary owns the main thread while
//! a dedicated Tokio runtime owns capture, encoding, HTTP, and WebRTC work.

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod app;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform;

/// Launch the native Lumen tray application.
///
/// This function must run on the process main thread. It returns after the
/// user selects **Quit**, once an active sharing session has shut down.
///
/// # Errors
///
/// Returns an error if the native tray cannot be initialized or the async
/// runtime cannot be created. Linux desktop support is intentionally out of
/// scope; on unsupported platforms it returns an explanatory error.
pub fn run() -> anyhow::Result<()> {
  #[cfg(any(target_os = "macos", target_os = "windows"))]
  return run_native();

  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  anyhow::bail!("`lumen desktop` is supported on macOS and Windows only")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_native() -> anyhow::Result<()> {
  use anyhow::Context;
  let filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| "warn,lumen=info".into());
  tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_target(false)
    .try_init()
    .map_err(|e| anyhow::anyhow!("failed to initialize desktop logging: {e}"))?;

  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .context("failed to start async runtime")?;

  #[cfg(target_os = "macos")]
  platform::init_app()?;

  #[cfg(target_os = "macos")]
  {
    let _pump = app::App::install(runtime.handle().clone())?;
    // AppKit owns the main thread until Quit ends the run loop.
    platform::run()?;
  }

  #[cfg(target_os = "windows")]
  {
    use app::UiEvent;

    let pump = app::App::install(runtime.handle().clone())?
      .context("the Windows UI event pump was not installed")?;
    while let Ok(event) = pump.recv() {
      match event {
        UiEvent::Menu(event) => {
          let _ = app::with_app(|app| app.handle_menu(&event));
        }
        UiEvent::Status(status) => {
          let _ = app::with_app(|app| app.apply_status(status));
        }
        UiEvent::Exit => break,
      }
    }
  }

  // The native loop only ends after the session stopped (Quit path);
  // release the tray first, then the runtime.
  app::App::detach();
  runtime.shutdown_timeout(std::time::Duration::from_secs(5));
  tracing::info!("Lumen Desktop stopped");
  Ok(())
}
