//! Platform glue for the native UI loop.
//!
//! macOS owns the strictest rule: all AppKit/event-loop code must run on
//! the main thread, so the main thread runs `NSApplication`'s run loop and
//! async completions hop back onto it via the main GCD queue. Windows has
//! no main-thread requirement for tray/menu objects (muda and tray-icon
//! pump their own message thread), so the main thread simply drains a
//! channel — all UI mutations still happen there.

#[cfg(target_os = "macos")]
mod macos {
  use objc2::MainThreadMarker;
  use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

  fn shared() -> objc2::rc::Retained<NSApplication> {
    let marker = MainThreadMarker::new().expect("Lumen Desktop UI must run on the main thread");
    NSApplication::sharedApplication(marker)
  }

  /// Accessory activation policy: menu-bar-only app, no Dock icon — the
  /// shape a future `.app` bundle keeps via `LSUIElement`.
  pub fn init_app() {
    shared().setActivationPolicy(NSApplicationActivationPolicy::Accessory);
  }

  /// Block on the `AppKit` run loop until [`stop_app`] is called.
  pub fn run() {
    shared().run();
  }

  /// End the [`run`] loop (must be called from the main thread).
  pub fn stop_app() {
    shared().stop(None);
  }

  /// Run `task` on the main thread (via the main GCD queue).
  pub fn post_to_main<F: FnOnce() + Send + 'static>(task: F) {
    dispatch::Queue::main().exec_async(task);
  }
}

#[cfg(target_os = "macos")]
pub use macos::{init_app, post_to_main, run, stop_app};

#[cfg(not(target_os = "macos"))]
pub fn init_app() {}

#[cfg(not(target_os = "macos"))]
pub fn post_to_main<F: FnOnce() + Send + 'static>(task: F) {
  // Only used on the macOS path; run inline elsewhere.
  task();
}
