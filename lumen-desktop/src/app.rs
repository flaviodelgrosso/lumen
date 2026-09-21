//! The tray application: menu, state projection and actions.
//!
//! The UI never blocks: `start`/`stop` hand work to the controller (which
//! spawns onto the Tokio runtime) and return; menu enablement follows the
//! observed [`SessionState`], and the state machine — not the menu — is
//! the concurrency authority.
//!
//! `muda` menu items and `tray-icon` handles are main-thread objects
//! (`Rc`-backed), so the [`App`] lives on the main thread behind a
//! thread-local and every UI mutation runs there: directly on macOS (the
//! `AppKit` run loop and main-queue dispatch), or via the Windows pump.

use std::cell::RefCell;
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::sync::OnceLock;

use anyhow::Context;
use muda::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, accelerator::Accelerator};
use tokio::runtime::Handle;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use lumen_session::{LiveEngine, SessionConfig, SessionController, SessionState, SessionStatus};

#[cfg(target_os = "macos")]
use crate::platform;
/// Event delivered to the Windows main-thread pump.
#[allow(dead_code, reason = "constructed only by the Windows pump")]
pub enum UiEvent {
  Menu(MenuEvent),
  Status(SessionStatus),
  Exit,
}

/// The Windows menu handler and session watcher run off the main thread, so
/// they publish UI work to the main thread through this one-time pump.
#[cfg(target_os = "windows")]
static PUMP: OnceLock<std::sync::mpsc::Sender<UiEvent>> = OnceLock::new();

thread_local! {
  /// The app, owned by the main thread from startup until exit.
  static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` on the main thread's [`App`], if it is still alive.
#[cfg(target_os = "windows")]
pub(crate) fn with_app<R>(f: impl FnOnce(&App) -> R) -> Option<R> {
  APP.with(|slot| slot.borrow().as_ref().map(f))
}

#[cfg(target_os = "macos")]
fn with_app<R>(f: impl FnOnce(&App) -> R) -> Option<R> {
  APP.with(|slot| slot.borrow().as_ref().map(f))
}

/// The tray application state: controller, menu items and tray handle.
pub struct App {
  controller: SessionController,
  runtime: Handle,
  start: MenuItem,
  stop: MenuItem,
  copy: MenuItem,
  dashboard: MenuItem,
  tray: TrayIcon,
}

impl App {
  /// Build the tray, install event handlers, and publish it to the main
  /// thread. On Windows, returns the receiver for the main-thread pump.
  ///
  /// # Errors
  ///
  /// Returns a recoverable initialization error rather than panicking when
  /// an icon, menu, system tray, or UI pump cannot be created.
  pub fn install(runtime: Handle) -> anyhow::Result<Option<std::sync::mpsc::Receiver<UiEvent>>> {
    ensure_uninstalled()?;
    #[cfg(target_os = "windows")]
    let (tx, rx) = std::sync::mpsc::channel();

    let app = App {
      controller: SessionController::new(Arc::new(LiveEngine), SessionConfig::default()),
      runtime,
      start: MenuItem::with_id("start", "Start Sharing", true, None::<Accelerator>),
      stop: MenuItem::with_id("stop", "Stop Sharing", false, None::<Accelerator>),
      copy: MenuItem::with_id("copy", "Copy Viewer Link", false, None::<Accelerator>),
      dashboard: MenuItem::with_id("dashboard", "Open Dashboard", false, None::<Accelerator>),
      tray: TrayIconBuilder::new()
        .with_tooltip("Lumen — idle")
        .with_icon(tray_icon_image()?)
        .with_icon_as_template(cfg!(target_os = "macos"))
        .build()
        .context("failed to create the system tray icon")?,
    };
    app.attach_menu()?;
    app.apply_status(SessionStatus {
      state: SessionState::Idle,
      error: None,
    });

    #[cfg(target_os = "windows")]
    PUMP
      .set(tx)
      .map_err(|_| anyhow::anyhow!("the Windows UI event pump is already installed"))?;

    APP.with(|slot| {
      slot.borrow_mut().replace(app);
    });
    with_app(App::install_handlers);

    #[cfg(target_os = "windows")]
    return Ok(Some(rx));
    #[cfg(target_os = "macos")]
    return Ok(None);
  }

  /// Take the app off the main thread (removes the tray icon).
  pub fn detach() {
    let _ = APP.with(|slot| slot.borrow_mut().take());
  }

  fn attach_menu(&self) -> anyhow::Result<()> {
    let menu = Menu::new();
    let header = MenuItem::with_id("lumen", "Lumen", false, None::<Accelerator>);
    let quit = MenuItem::with_id("quit", "Quit", true, None::<Accelerator>);
    let sep1 = PredefinedMenuItem::separator();
    let sep2 = PredefinedMenuItem::separator();
    let sep3 = PredefinedMenuItem::separator();
    let parts: Vec<&dyn IsMenuItem> = vec![
      &header,
      &sep1,
      &self.start,
      &self.stop,
      &sep2,
      &self.copy,
      &self.dashboard,
      &sep3,
      &quit,
    ];
    menu
      .append_items(&parts)
      .context("failed to build the tray menu")?;
    self.tray.set_menu(Some(Box::new(menu)));
    Ok(())
  }

  /// Install the global menu-event handler and the state watcher.
  fn install_handlers(&self) {
    // Menu events: macOS fires the handler on the main thread inside the
    // AppKit run loop; Windows fires it on muda's message thread, so the
    // event is marshalled to the main pump.
    MenuEvent::set_event_handler(Some(move |event| {
      #[cfg(target_os = "macos")]
      let _ = with_app(|app| app.handle_menu(&event));
      #[cfg(target_os = "windows")]
      if let Some(tx) = PUMP.get() {
        let _ = tx.send(UiEvent::Menu(event));
      }
    }));

    // State changes: the watcher never blocks the runtime worker and hops
    // back to the UI thread to mutate the menu.
    let mut status_rx = self.controller.subscribe();
    let _guard = self.runtime.enter();
    tokio::spawn(async move {
      loop {
        if status_rx.changed().await.is_err() {
          break;
        }
        let status = status_rx.borrow_and_update().clone();
        #[cfg(target_os = "macos")]
        platform::post_to_main(move || {
          let _ = with_app(|app| app.apply_status(status));
        });
        #[cfg(target_os = "windows")]
        if let Some(tx) = PUMP.get() {
          let _ = tx.send(UiEvent::Status(status));
        }
      }
    });
  }

  /// Route a menu activation. Runs on the main thread on both platforms.
  #[cfg(target_os = "windows")]
  pub fn handle_menu(&self, event: &MenuEvent) {
    self.handle_menu_impl(event);
  }

  #[cfg(target_os = "macos")]
  fn handle_menu(&self, event: &MenuEvent) {
    self.handle_menu_impl(event);
  }

  fn handle_menu_impl(&self, event: &MenuEvent) {
    match event.id.as_ref() {
      "start" => {
        let _guard = self.runtime.enter();
        if let Err(e) = self.controller.start() {
          tracing::warn!("start rejected: {e}");
        }
      }
      "stop" => {
        let _guard = self.runtime.enter();
        if let Err(e) = self.controller.stop() {
          tracing::warn!("stop rejected: {e}");
        }
      }
      "copy" => self.copy_viewer_link(),
      "dashboard" => self.open_dashboard(),
      "quit" => self.quit(),
      _ => {}
    }
  }

  /// Project the session state onto the menu and tooltip.
  #[cfg(target_os = "windows")]
  pub fn apply_status(&self, status: SessionStatus) {
    self.apply_status_impl(status);
  }

  #[cfg(target_os = "macos")]
  fn apply_status(&self, status: SessionStatus) {
    self.apply_status_impl(status);
  }

  fn apply_status_impl(&self, status: SessionStatus) {
    self.start.set_enabled(status.state.can_start());
    self.stop.set_enabled(status.state.can_stop());
    self.copy.set_enabled(status.state.has_urls());
    self.dashboard.set_enabled(status.state.has_urls());
    let tooltip = match status.state {
      SessionState::Idle => "Lumen — idle".to_owned(),
      SessionState::Starting => "Lumen — starting…".to_owned(),
      SessionState::Running => "Lumen — sharing".to_owned(),
      SessionState::Stopping => "Lumen — stopping…".to_owned(),
      SessionState::Error => format!(
        "Lumen — failed: {}",
        status.error.unwrap_or_else(|| "unknown error".to_owned())
      ),
    };
    if let Err(e) = self.tray.set_tooltip(Some(&tooltip)) {
      tracing::warn!("could not update the tray tooltip: {e}");
    }
  }

  fn copy_viewer_link(&self) {
    let Some(url) = self.controller.viewer_url() else {
      tracing::warn!("copy viewer link requested with no active session");
      return;
    };
    // Clipboard init can touch OS APIs that block; never on the UI thread.
    std::thread::spawn(move || {
      let result = arboard::Clipboard::new().and_then(|mut clip| clip.set_text(url));
      if let Err(e) = result {
        tracing::error!("could not copy the viewer link: {e}");
      }
    });
  }

  fn open_dashboard(&self) {
    // The URL (with its token) comes from the running session; the desktop
    // never reconstructs admin secrets.
    let Some(url) = self.controller.dashboard_url() else {
      tracing::warn!("open dashboard requested with no active session");
      return;
    };
    std::thread::spawn(move || {
      if let Err(e) = open::that(&url) {
        tracing::error!("could not open the dashboard: {e}");
      }
    });
  }

  /// Graceful quit: fully stop the session, then end the native loop.
  fn quit(&self) {
    let controller = self.controller.clone();
    let _guard = self.runtime.enter();
    tokio::spawn(async move {
      controller.stop_and_wait().await;
      #[cfg(target_os = "macos")]
      platform::post_to_main(|| {
        if let Err(e) = platform::stop_app() {
          tracing::error!("could not stop the AppKit run loop: {e}");
        }
      });
      #[cfg(target_os = "windows")]
      if let Some(tx) = PUMP.get() {
        let _ = tx.send(UiEvent::Exit);
      }
    });
  }
}

/// Refuse a second UI instance before allocating native resources.
fn ensure_uninstalled() -> anyhow::Result<()> {
  if APP.with(|slot| slot.borrow().is_some()) {
    anyhow::bail!("the tray application is already running")
  }
  #[cfg(target_os = "windows")]
  if PUMP.get().is_some() {
    anyhow::bail!("the Windows UI event pump is already installed")
  }
  Ok(())
}

/// The embedded tray icon: a monochrome template on macOS (the menu bar
/// tints it for light/dark), full-color on Windows.
fn tray_icon_image() -> anyhow::Result<Icon> {
  #[cfg(target_os = "macos")]
  let bytes: &[u8] = include_bytes!("../assets/tray-template.png");
  #[cfg(target_os = "windows")]
  let bytes: &[u8] = include_bytes!("../assets/tray.png");
  let image = image::load_from_memory(bytes)
    .context("failed to decode the embedded tray icon")?
    .to_rgba8();
  let (width, height) = image.dimensions();
  Icon::from_rgba(image.into_raw(), width, height)
    .context("the embedded tray icon is not valid RGBA")
}
