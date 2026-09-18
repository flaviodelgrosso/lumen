//! Windows capture backend driving `Windows.Graphics.Capture` directly.
//!
//! `windows-capture`'s engine needs a dedicated thread with a `WinRT`
//! dispatcher queue and a Win32 message loop, so the capture runs on the
//! engine thread that `start_free_threaded` spawns. Completed frames are
//! copied out of the mapped D3D11 staging texture and handed to the
//! pipeline through a bounded [`FrameSender`], keeping the public
//! [`CaptureSource::next_frame`] API blocking. The native engine objects
//! stay on the engine thread — no `unsafe impl Send` is needed anywhere.
//!
//! Windows Graphics Capture delivers updates when content changes rather
//! than at a fixed rate, so the `fps` knob is not enforced here (the
//! previous backend behaved the same way); the pipeline's latest-frame
//! watch channel paces the encoder.

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use lumen_core::{Dimensions, RawFrame};
use windows_capture::capture::{
  CaptureControl, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{
  Error as GraphicsCaptureError, GraphicsCaptureApi, InternalCaptureControl,
};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
  ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
  MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::{
  CaptureError, CaptureSource, DisplayInfo, FrameSender, SendOutcome, WindowInfo, bgra_to_raw,
  fits_encodable,
};

/// Backpressure budget for the engine → pipeline hop. Beyond it the newest
/// frame is dropped; the pipeline keeps only the latest frame anyway.
const FRAME_QUEUE: usize = 8;

/// `E_ACCESSDENIED` — WGC reports this when the capture target is an
/// elevated window or a policy disables screen capture.
#[expect(
  clippy::cast_possible_wrap,
  reason = "HRESULTs are 32-bit two's-complement values; the bit pattern is \
            the value"
)]
const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;

/// Full-screen/window capture backed by `Windows.Graphics.Capture`.
pub struct PlatformCapture {
  /// `None` once dropped; `stop()` joins the engine thread.
  control: Option<CaptureControl<FramePump, PumpError>>,
  rx: Receiver<RawFrame>,
  dimensions: Dimensions,
}

/// Errors surfaced by the frame pump to the engine (stops the session).
#[derive(Debug, thiserror::Error)]
enum PumpError {
  /// The pipeline consumer is gone; stop the engine.
  #[error("capture consumer stopped")]
  Closed,
}

/// Engine-thread handler: converts each WGC frame and forwards it.
struct FramePump {
  bridge: Arc<FrameSender<RawFrame>>,
}

impl GraphicsCaptureApiHandler for FramePump {
  type Flags = Arc<FrameSender<RawFrame>>;
  type Error = PumpError;

  fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
    Ok(Self { bridge: ctx.flags })
  }

  fn on_frame_arrived(
    &mut self,
    frame: &mut Frame,
    _capture_control: InternalCaptureControl,
  ) -> Result<(), Self::Error> {
    let Ok(mut buffer) = frame.buffer() else {
      // Transient D3D failure: skip the frame, keep the session alive.
      tracing::debug!("discarding frame: buffer mapping failed");
      return Ok(());
    };
    // D3D11 aligns rows, so the row pitch is the authoritative stride; the
    // encoder repacks padded rows.
    let (width, height, stride) = (buffer.width(), buffer.height(), buffer.row_pitch());
    if width == 0 || height == 0 {
      return Ok(());
    }
    let Some(len) = u64::from(stride)
      .checked_mul(u64::from(height))
      .and_then(|bytes| usize::try_from(bytes).ok())
    else {
      tracing::warn!("discarding frame with impossible geometry {width}x{height} pitch {stride}");
      return Ok(());
    };
    let pixels = buffer.as_raw_buffer();
    if pixels.len() < len {
      tracing::warn!(
        "discarding frame: {} bytes below {len} for {height} rows",
        pixels.len()
      );
      return Ok(());
    }
    let Ok(raw) = bgra_to_raw(width, height, stride, pixels[..len].to_vec()) else {
      tracing::warn!("discarding malformed frame");
      return Ok(());
    };
    match self.bridge.send(raw) {
      SendOutcome::Full => {
        tracing::trace!("frame dropped (pipeline queue full)");
        Ok(())
      }
      // The consumer is gone: stop the engine, which drops the sender and
      // wakes `next_frame` with `CaptureError::Stopped`.
      SendOutcome::Closed => Err(PumpError::Closed),
      SendOutcome::Sent => Ok(()),
    }
  }

  fn on_closed(&mut self) -> Result<(), Self::Error> {
    // The capture target vanished (window closed, monitor unplugged). Close
    // the channel so `next_frame` reports `CaptureError::Stopped` even if
    // the engine thread lingers.
    self.bridge.close();
    Ok(())
  }
}

/// The OS window handle is 32-bit-significant (the kernel guarantees the
/// high bits are zero), so truncating it yields the stable `u32` id shown by
/// `lumen windows`.
#[expect(
  clippy::cast_possible_truncation,
  reason = "HWND values are 32-bit-significant on all supported Windows versions"
)]
fn window_id_of(window: &Window) -> u32 {
  window.as_raw_hwnd() as usize as u32
}

fn monitor_dimensions(monitor: Monitor) -> Result<Dimensions, CaptureError> {
  let width = monitor
    .width()
    .map_err(|e| CaptureError::StartFailed(e.to_string()))?;
  let height = monitor
    .height()
    .map_err(|e| CaptureError::StartFailed(e.to_string()))?;
  Ok(Dimensions::new(width, height))
}

fn monitor_title(monitor: Monitor, index: usize) -> String {
  monitor
    .name()
    .or_else(|_| monitor.device_name())
    .unwrap_or_else(|_| format!("Monitor {}", index + 1))
}

fn too_large_error(target: Dimensions, advice: &str) -> CaptureError {
  CaptureError::StartFailed(format!(
    "display {target} exceeds the 3840x2160 encoder limit and Windows \
     capture cannot scale it; {advice}"
  ))
}

/// Map an engine startup failure onto [`CaptureError`].
///
/// Elevated (Run as administrator) windows and enterprise screen-capture
/// policy make `CreateForWindow`/`CreateForMonitor` fail with
/// `E_ACCESSDENIED`; that is the Windows analogue of a permission denial.
fn start_error(error: &GraphicsCaptureApiError<PumpError>) -> CaptureError {
  if let GraphicsCaptureApiError::GraphicsCaptureApiError(inner) = &error {
    match inner {
      GraphicsCaptureError::WindowsError(w) if w.code().0 == E_ACCESSDENIED => {
        return CaptureError::PermissionDenied;
      }
      GraphicsCaptureError::Unsupported => return CaptureError::NotSupported,
      _ => {}
    }
  }
  CaptureError::StartFailed(error.to_string())
}

impl PlatformCapture {
  /// Build a full-display capturer.
  ///
  /// `display_id = None` selects the primary display; otherwise the id is
  /// the display's position in the `lumen displays` listing.
  ///
  /// WGC cannot scale capture output, so a display larger than
  /// [`lumen_core::Dimensions::MAX_ENCODABLE`] fails at startup.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_display(display_id: Option<u32>, _fps: u32) -> Result<Self, CaptureError> {
    require_permission()?;
    let monitors = Monitor::enumerate().map_err(|e| CaptureError::StartFailed(e.to_string()))?;
    if monitors.is_empty() {
      return Err(CaptureError::NoDisplay);
    }
    let monitor = match display_id {
      Some(id) => monitors
        .into_iter()
        .nth(usize::try_from(id).unwrap_or(usize::MAX))
        .ok_or(CaptureError::DisplayNotFound(id))?,
      None => Monitor::primary().map_err(|_| CaptureError::NoDisplay)?,
    };
    let dimensions = monitor_dimensions(monitor)?;
    if !fits_encodable(dimensions) {
      return Err(too_large_error(
        dimensions,
        "capture a window or lower the display resolution",
      ));
    }
    Self::build(monitor, dimensions)
  }

  /// Build a window capturer.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_window(window_id: u32, _fps: u32) -> Result<Self, CaptureError> {
    require_permission()?;
    let window = Window::enumerate()
      .map_err(|e| CaptureError::StartFailed(e.to_string()))?
      .into_iter()
      .find(|window| window_id_of(window) == window_id)
      .ok_or(CaptureError::WindowNotFound(window_id))?;
    let width = u32::try_from(
      window
        .width()
        .map_err(|e| CaptureError::StartFailed(e.to_string()))?,
    )
    .unwrap_or(0);
    let height = u32::try_from(
      window
        .height()
        .map_err(|e| CaptureError::StartFailed(e.to_string()))?,
    )
    .unwrap_or(0);
    let dimensions = Dimensions::new(width, height);
    if dimensions.is_empty() {
      return Err(CaptureError::StartFailed(format!(
        "window {window_id} has no capturable size (is it minimized?)"
      )));
    }
    if !fits_encodable(dimensions) {
      return Err(too_large_error(
        dimensions,
        "capture a smaller window or a display",
      ));
    }
    Self::build(window, dimensions)
  }

  fn build(
    item: impl TryInto<windows_capture::settings::GraphicsCaptureItemType> + Send + 'static,
    dimensions: Dimensions,
  ) -> Result<Self, CaptureError> {
    crate::require_permission()?;
    let (bridge, rx) = FrameSender::bounded(FRAME_QUEUE);
    let settings = Settings::new(
      item,
      CursorCaptureSettings::WithCursor,
      DrawBorderSettings::Default,
      SecondaryWindowSettings::Default,
      MinimumUpdateIntervalSettings::Default,
      DirtyRegionSettings::Default,
      ColorFormat::Bgra8,
      Arc::new(bridge),
    );
    // The engine thread starts capturing immediately; `start()` below is a
    // formality to keep the shared startup flow uniform.
    let control = FramePump::start_free_threaded(settings).map_err(|error| start_error(&error))?;
    tracing::debug!(
      width = dimensions.width,
      height = dimensions.height,
      "configured Windows.Graphics.Capture"
    );
    Ok(Self {
      control: Some(control),
      rx,
      dimensions,
    })
  }

  /// The engine is already running (started with the constructor); this
  /// keeps the startup flow identical across platforms.
  ///
  /// # Errors
  ///
  /// See [`CaptureError::StartFailed`].
  pub fn start(&mut self) -> Result<(), CaptureError> {
    Ok(())
  }
}

impl Drop for PlatformCapture {
  fn drop(&mut self) {
    // Request WM_QUIT and join the engine thread. The frame pump — the sole
    // holder of the frame sender — dies with the thread, so the consumer's
    // `recv` wakes with `Disconnected` (`CaptureError::Stopped`).
    if let Some(control) = self.control.take() {
      if let Err(error) = control.stop() {
        tracing::debug!(error = %error, "failed to stop Windows capture engine");
      }
    }
  }
}

impl CaptureSource for PlatformCapture {
  fn dimensions(&self) -> Dimensions {
    self.dimensions
  }

  fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
    self.rx.recv().map_err(|_| CaptureError::Stopped)
  }
}

pub(crate) fn require_permission() -> Result<(), CaptureError> {
  // WGC has no per-app permission gate; the realistic failure modes are an
  // OS that predates the API (`NotSupported`) and elevated/policy-blocked
  // targets (surfaced at start-up as `PermissionDenied`).
  match GraphicsCaptureApi::is_supported() {
    Ok(true) => Ok(()),
    Ok(false) | Err(_) => Err(CaptureError::NotSupported),
  }
}

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
  require_permission()?;
  let monitors = Monitor::enumerate().map_err(|e| CaptureError::StartFailed(e.to_string()))?;
  Ok(
    monitors
      .into_iter()
      .enumerate()
      .map(|(index, monitor)| DisplayInfo {
        // Enumerate order is the stable per-session id accepted by
        // `--display`.
        id: u32::try_from(index).unwrap_or(u32::MAX),
        title: monitor_title(monitor, index),
        dimensions: monitor_dimensions(monitor).unwrap_or(Dimensions::new(0, 0)),
      })
      .collect(),
  )
}

pub(crate) fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
  require_permission()?;
  let windows = Window::enumerate().map_err(|e| CaptureError::StartFailed(e.to_string()))?;
  Ok(
    windows
      .into_iter()
      .map(|window| WindowInfo {
        id: window_id_of(&window),
        title: window.title().unwrap_or_default(),
      })
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn access_denied_maps_to_permission_denied() {
    let error = GraphicsCaptureApiError::GraphicsCaptureApiError(
      GraphicsCaptureError::WindowsError(windows_error(E_ACCESSDENIED)),
    );
    assert!(matches!(
      start_error(&error),
      CaptureError::PermissionDenied
    ));
  }

  #[test]
  fn other_startup_errors_map_to_start_failed() {
    let error = GraphicsCaptureApiError::FailedToInitWinRT;
    assert!(matches!(start_error(&error), CaptureError::StartFailed(_)));
  }

  #[test]
  fn unsupported_maps_to_not_supported() {
    let error = GraphicsCaptureApiError::GraphicsCaptureApiError(GraphicsCaptureError::Unsupported);
    assert!(matches!(start_error(&error), CaptureError::NotSupported));
  }

  fn windows_error(code: i32) -> windows::core::Error {
    windows::core::Error::from_hresult(windows::core::HRESULT(code))
  }
}
