//! macOS capture backend driving `ScreenCaptureKit` directly.
//!
//! A single `SCStream` per capture target (display or window) delivers BGRA
//! frames on a `ScreenCaptureKit` dispatch queue; each frame is copied out of
//! the locked `CVPixelBuffer` and handed to the pipeline through a bounded
//! [`FrameSender`]. Output sizes are configured explicitly so
//! `ScreenCaptureKit` performs any downscaling to fit the encoder natively
//! (zero CPU cost in our pipeline).
//!
//! Screen-recording permission is gated with CoreGraphics'
//! `CGPreflight/RequestScreenCaptureAccess` (the same flow the previous
//! backend used): the first run registers the binary in System Settings, and
//! a denial surfaces as [`CaptureError::PermissionDenied`] with remediation
//! text — never a panic.

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use core_graphics::access::ScreenCaptureAccess;
use core_graphics::display::CGDisplay;
use lumen_core::{Dimensions, RawFrame};
use screencapturekit::cg::{CGPoint, CGRect};
use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::prelude::*;

use crate::capture::{
  CaptureError, CaptureSource, DisplayInfo, FrameSender, SendOutcome, WindowInfo, bgra_to_raw,
  fit_encodable,
};

/// Backpressure budget for the capture → pipeline hop. Beyond it the newest
/// frame is dropped; the pipeline keeps only the latest frame anyway.
const FRAME_QUEUE: usize = 8;

/// System display capture backed by a `ScreenCaptureKit` stream.
pub struct PlatformCapture {
  stream: SCStream,
  bridge: Arc<FrameSender<RawFrame>>,
  rx: Receiver<RawFrame>,
  dimensions: Dimensions,
  started: bool,
}

/// Copies BGRA pixels out of each sample buffer and forwards them to the
/// pipeline. Runs on a `ScreenCaptureKit` dispatch queue.
struct ScreenOutput {
  bridge: Arc<FrameSender<RawFrame>>,
}

impl SCStreamOutputTrait for ScreenOutput {
  fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
    if !matches!(of_type, SCStreamOutputType::Screen) {
      return;
    }
    let Some(frame) = sample_to_raw(&sample) else {
      return;
    };
    match self.bridge.send(frame) {
      SendOutcome::Full => tracing::trace!("frame dropped (pipeline queue full)"),
      SendOutcome::Closed => tracing::trace!("frame dropped (consumer gone)"),
      SendOutcome::Sent => {}
    }
  }
}

/// Convert one `ScreenCaptureKit` sample into a [`RawFrame`].
///
/// Returns `None` for anything undeliverable: zero-size placeholder frames
/// (idle display), planar buffers, or inconsistent geometry. Never panics on
/// a malformed buffer.
fn sample_to_raw(sample: &CMSampleBuffer) -> Option<RawFrame> {
  let pixel_buffer = sample.pixel_buffer()?;
  let guard = pixel_buffer.lock_read_only().ok()?;
  let width = u32::try_from(guard.width()).ok()?;
  let height = u32::try_from(guard.height()).ok()?;
  let stride = u32::try_from(guard.bytes_per_row()).ok()?;
  if width == 0 || height == 0 {
    // ScreenCaptureKit emits zero-size frames while the target is idle.
    return None;
  }
  // SAFETY: the guard holds a read-only lock for the slice's lifetime, so
  // the mapping stays allocated, initialized, and immutable; the bytes are
  // copied out immediately below.
  #[expect(unsafe_code, reason = "reading a locked CVPixelBuffer's pixel bytes")]
  let pixels = unsafe { guard.as_slice() }?;
  bgra_to_raw(width, height, stride, pixels.to_vec()).ok()
}

/// Fetch shareable content behind the screen-recording permission gate.
fn shareable_content() -> Result<SCShareableContent, CaptureError> {
  require_permission()?;
  // Any CoreGraphics display call establishes the window-server connection;
  // ScreenCaptureKit's desktop-independent window filter aborts inside CGS
  // when the process never made one (a plain CLI binary never does).
  let _ = CGDisplay::main();
  SCShareableContent::get().map_err(|error| {
    tracing::debug!("SCShareableContent::get failed: {error}");
    CaptureError::PermissionDenied
  })
}

/// Human-readable display name.
///
/// `SCDisplay` carries no user-facing name and reading
/// `NSScreen.localizedName` would require `AppKit`/`objc` bindings, so the
/// title is synthesized positionally.
fn display_title(index: usize) -> String {
  format!("Display {}", index + 1)
}

/// Native pixel size of a display (`SCDisplay::width/height` report pixels,
/// `frame` reports points).
fn display_dimensions(display: &SCDisplay) -> Dimensions {
  Dimensions::new(display.width(), display.height())
}

/// Points-to-pixels scale factor of a display, from its pixel size and its
/// frame in points.
fn display_scale(display: &SCDisplay) -> f64 {
  let points = display.frame().size.width;
  if points > 0.0 {
    f64::from(display.width()) / points
  } else {
    1.0
  }
}

fn contains(rect: CGRect, point: CGPoint) -> bool {
  point.x >= rect.origin.x
    && point.x < rect.origin.x + rect.size.width
    && point.y >= rect.origin.y
    && point.y < rect.origin.y + rect.size.height
}

/// Native pixel size of a window capture.
///
/// Window frames are reported in points; their pixels are rendered on the
/// display the window sits on, so the scale factor is taken from the display
/// containing the window's center (falling back to the first display).
fn window_pixel_size(window: &SCWindow, displays: &[SCDisplay]) -> Dimensions {
  let frame = window.frame();
  let center = CGPoint::new(
    frame.origin.x + frame.size.width / 2.0,
    frame.origin.y + frame.size.height / 2.0,
  );
  let scale = displays
    .iter()
    .find(|display| contains(display.frame(), center))
    .or_else(|| displays.first())
    .map_or(1.0, display_scale);
  #[expect(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "negative or oversized frames floor to 0 and are rejected upstream"
  )]
  let dimension =
    |points: f64| -> u32 { u32::try_from((points * scale).max(0.0) as u64).unwrap_or(0) };
  Dimensions::new(dimension(frame.size.width), dimension(frame.size.height))
}

impl PlatformCapture {
  /// Build a full-display capturer.
  ///
  /// `display_id = None` selects the main display.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_display(display_id: Option<u32>, fps: u32) -> Result<Self, CaptureError> {
    let content = shareable_content()?;
    let displays = content.displays();
    if displays.is_empty() {
      return Err(CaptureError::NoDisplay);
    }
    let display = if let Some(id) = display_id {
      displays
        .into_iter()
        .find(|display| display.display_id() == id)
        .ok_or(CaptureError::DisplayNotFound(id))?
    } else {
      let main = CGDisplay::main().id;
      displays
        .iter()
        .find(|display| display.display_id() == main)
        .cloned()
        .unwrap_or_else(|| displays[0].clone())
    };
    let dimensions = fit_encodable(display_dimensions(&display));
    let filter = SCContentFilter::create()
      .with_display(&display)
      .with_excluding_windows(&[])
      .build();
    Ok(Self::build(&filter, dimensions, fps))
  }

  /// Build a window capturer.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_window(window_id: u32, fps: u32) -> Result<Self, CaptureError> {
    let content = shareable_content()?;
    let window = content
      .windows()
      .into_iter()
      .find(|window| window.window_id() == window_id)
      .ok_or(CaptureError::WindowNotFound(window_id))?;
    let native = window_pixel_size(&window, &content.displays());
    if native.is_empty() {
      return Err(CaptureError::StartFailed(format!(
        "window {window_id} has no capturable size (is it minimized?)"
      )));
    }
    let dimensions = fit_encodable(native);
    let filter = SCContentFilter::create().with_window(&window).build();
    Ok(Self::build(&filter, dimensions, fps))
  }

  fn build(filter: &SCContentFilter, dimensions: Dimensions, fps: u32) -> Self {
    let (bridge, rx) = FrameSender::bounded(FRAME_QUEUE);
    let bridge = Arc::new(bridge);

    let config = SCStreamConfiguration::new()
      .with_width(dimensions.width)
      .with_height(dimensions.height)
      .with_pixel_format(PixelFormat::BGRA)
      .with_shows_cursor(true)
      .with_fps(fps.clamp(1, 240));

    // ScreenCaptureKit can stop the stream itself (display unplugged,
    // permission revoked, window closed); closing the frame channel turns
    // that into `CaptureError::Stopped` for the consumer.
    let error_bridge = Arc::clone(&bridge);
    let mut stream = SCStream::new_with_delegate(
      filter,
      &config,
      ErrorHandler::new(move |error| {
        tracing::warn!("ScreenCaptureKit stream stopped with error: {error}");
        error_bridge.close();
      }),
    );
    stream.add_output_handler(
      ScreenOutput {
        bridge: Arc::clone(&bridge),
      },
      SCStreamOutputType::Screen,
    );

    tracing::debug!(
      width = dimensions.width,
      height = dimensions.height,
      fps,
      "configured ScreenCaptureKit capture"
    );
    Self {
      stream,
      bridge,
      rx,
      dimensions,
      started: false,
    }
  }

  /// Start the underlying capture engine.
  ///
  /// # Errors
  ///
  /// See [`CaptureError::StartFailed`].
  pub fn start(&mut self) -> Result<(), CaptureError> {
    self.stream.start_capture().map_err(|error| match error {
      SCError::PermissionDenied(_) | SCError::NoShareableContent(_) => {
        CaptureError::PermissionDenied
      }
      other => CaptureError::StartFailed(other.to_string()),
    })?;
    self.started = true;
    Ok(())
  }
}

impl Drop for PlatformCapture {
  fn drop(&mut self) {
    // Never stop a stream that never started: stopping an unstarted stream
    // is an error, and panicking in a destructor aborts the process.
    if self.started {
      if let Err(error) = self.stream.stop_capture() {
        tracing::debug!(error = %error, "failed to stop ScreenCaptureKit capture");
      }
    }
    self.bridge.close();
  }
}

impl CaptureSource for PlatformCapture {
  fn dimensions(&self) -> Dimensions {
    self.dimensions
  }

  fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
    // The ScreenCaptureKit queues deliver frames; a disconnected channel
    // means the stream died (see `build`'s error delegate) or this source
    // is being dropped.
    self.rx.recv().map_err(|_| CaptureError::Stopped)
  }
}

pub(crate) fn require_permission() -> Result<(), CaptureError> {
  // ScreenCaptureKit is purely TCC-gated: preflight first, then request so
  // the first run registers the binary in System Settings. A denial must
  // never abort the process — `CGRequestScreenCaptureAccess` only shows the
  // prompt and reports the outcome.
  let access = ScreenCaptureAccess;
  if access.preflight() || access.request() {
    return Ok(());
  }
  Err(CaptureError::PermissionDenied)
}

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
  let content = shareable_content()?;
  Ok(
    content
      .displays()
      .into_iter()
      .enumerate()
      .map(|(index, display)| DisplayInfo {
        id: display.display_id(),
        title: display_title(index),
        // What `lumen serve` would actually capture, encoder-fit included.
        dimensions: fit_encodable(display_dimensions(&display)),
      })
      .collect(),
  )
}

pub(crate) fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
  let content = shareable_content()?;
  // Untitled windows (desktop elements, overlays) are not capturable
  // targets, matching the previous backend's listing.
  Ok(
    content
      .windows()
      .into_iter()
      .filter_map(|window| {
        let title = window.title()?;
        Some(WindowInfo {
          id: window.window_id(),
          title,
        })
      })
      .collect(),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(x, y, width, height)
  }

  #[test]
  fn contains_picks_the_display_holding_a_point() {
    let left = rect(0.0, 0.0, 1728.0, 1117.0);
    let right = rect(1728.0, 0.0, 1920.0, 1080.0);
    assert!(contains(left, CGPoint::new(100.0, 100.0)));
    assert!(!contains(left, CGPoint::new(1728.0, 100.0)));
    assert!(contains(right, CGPoint::new(1728.0, 100.0)));
    assert!(!contains(right, CGPoint::new(3648.0, 1080.0)));
  }
}
