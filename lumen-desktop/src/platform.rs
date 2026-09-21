//! Platform glue for the native UI loop.
//!
//! macOS owns the strictest rule: all AppKit/event-loop code must run on the
//! main thread, so the main thread runs `NSApplication`'s run loop and async
//! completions hop back onto it via the main GCD queue. Windows uses the pump
//! in [`crate::run`] to serialize UI mutations onto the main thread.

#[cfg(target_os = "macos")]
mod macos {
  use anyhow::Context;
  use objc2::MainThreadMarker;
  use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

  fn shared() -> anyhow::Result<objc2::rc::Retained<NSApplication>> {
    let marker = MainThreadMarker::new().context("Lumen Desktop must run on the main thread")?;
    Ok(NSApplication::sharedApplication(marker))
  }

  /// Accessory activation policy: menu-bar-only app, no Dock icon — the
  /// shape a future `.app` bundle keeps via `LSUIElement`.
  pub fn init_app() -> anyhow::Result<()> {
    shared()?.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    Ok(())
  }

  /// Block on the `AppKit` run loop until [`stop_app`] is called.
  pub fn run() -> anyhow::Result<()> {
    shared()?.run();
    Ok(())
  }

  /// End the [`run`] loop (must be called from the main thread).
  pub fn stop_app() -> anyhow::Result<()> {
    shared()?.stop(None);
    Ok(())
  }

  /// Run `task` on the main thread via the main GCD queue.
  pub fn post_to_main<F: FnOnce() + Send + 'static>(task: F) {
    dispatch::Queue::main().exec_async(task);
  }
}

#[cfg(target_os = "macos")]
pub use macos::{init_app, post_to_main, run, stop_app};
