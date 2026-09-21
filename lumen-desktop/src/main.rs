//! Lumen Desktop — a native menu-bar / system-tray frontend for Lumen.
//!
//! Thin on purpose: the tray drives the shared `lumen-session` state
//! machine directly (no `lumen serve` subprocess, no Tauri). The native UI
//! loop owns the main thread; capture, encoding and the HTTP/WebRTC server
//! run on an independent Tokio runtime.

#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]

mod app;
mod platform;

use anyhow::Context;

use app::App;

fn main() -> anyhow::Result<()> {
  let filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| "warn,lumen=info".into());
  tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_target(false)
    .init();

  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .context("failed to start async runtime")?;

  platform::init_app();

  #[cfg(target_os = "macos")]
  {
    let _pump = App::install(runtime.handle().clone());
    // AppKit owns the main thread until Quit ends the run loop.
    platform::run();
  }

  #[cfg(not(target_os = "macos"))]
  {
    use app::UiEvent;
    let pump =
      App::install(runtime.handle().clone()).context("the UI event pump must be installed")?;
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
  App::detach();
  runtime.shutdown_timeout(std::time::Duration::from_secs(5));
  tracing::info!("Lumen Desktop stopped");
  Ok(())
}
