//! Display/window enumeration, screen capture, and system-audio capture
//! behind portable traits.
//!
//! The production implementation wraps [`scap`] (`ScreenCaptureKit` /
//! Windows.Graphics.Capture / `PipeWire`). Tests use [`FakeCaptureSource`] so
//! they never need display hardware; [`FakeAudioCapture`] plays the same
//! role for audio.
//!
//! Note: the `scap` dependency is `scap-vc`, a patched fork of `scap` 0.0.8
//! that captures windows across processes (upstream sizes windows through
//! `NSApp`, which only sees the calling process's own windows) and returns
//! `Result` from start/stop instead of panicking. `scap` keeps its `targets`
//! module private and exposes only the `Target` enum; display/window metadata
//! is therefore read through pattern matching on the re-exported variants,
//! and output sizes come from the public `get_output_frame_size`.

use std::time::{Duration, Instant};

use bytes::Bytes;
use lumen_core::{Dimensions, PixelFormat, RawFrame};
use scap::{Target, capturer::Options, capturer::Resolution, frame::Frame, frame::FrameType};
use thiserror::Error;

pub mod audio;
mod resample;

pub use audio::{AudioCaptureSource, FakeAudioCapture, ScapAudioCapture};

/// Errors from capture enumeration or frame delivery.
#[derive(Debug, Error)]
pub enum CaptureError {
  /// This platform/OS version cannot capture screens.
  #[error("screen capture is not supported on this platform")]
  NotSupported,
  /// The OS denied the screen-recording permission.
  #[error(
    "screen-recording permission not granted; on macOS open System Settings \u{2192} Privacy & Security \u{2192} Screen Recording, enable your terminal app, then rerun lumen"
  )]
  PermissionDenied,
  /// No capturable display exists.
  #[error("no capturable display found")]
  NoDisplay,
  /// The requested `--display` id does not exist.
  #[error("display {0} not found; run `lumen displays` to list available displays")]
  DisplayNotFound(u32),
  /// The frame channel closed (capture engine died).
  #[error("capture engine stopped")]
  Stopped,
  /// A frame arrived with an inconsistent buffer.
  #[error("captured frame is malformed: {0}")]
  MalformedFrame(String),
  /// The requested `--window` id does not exist.
  #[error("window {0} not found; run `lumen windows` to list capturable windows")]
  WindowNotFound(u32),
  /// This platform/OS version cannot capture system audio.
  #[error("system audio capture is not supported on this platform (macOS 13+ required)")]
  AudioNotSupported,
  /// The audio capture engine failed to start.
  #[error("audio capture failed to start: {0}")]
  AudioStart(String),
  /// The capture engine refused to start (e.g. the target vanished or the
  /// OS rejected the stream configuration).
  #[error("capture failed to start: {0}")]
  StartFailed(String),
}

/// Abstraction over anything that yields raw frames.
///
/// `next_frame` blocks until a frame is available; run it on a blocking
/// thread (see the capture task in `lumen-cli`).
pub trait CaptureSource: Send {
  /// Output dimensions of frames produced by this source.
  fn dimensions(&self) -> Dimensions;

  /// Block for the next frame.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  fn next_frame(&mut self) -> Result<RawFrame, CaptureError>;
}

/// Metadata about a capturable display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
  /// OS display id (stable for the session).
  pub id: u32,
  /// Human-readable name.
  pub title: String,
  /// Pixel dimensions.
  pub dimensions: Dimensions,
}

/// Metadata about a capturable window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
  /// OS window id.
  pub id: u32,
  /// Window title.
  pub title: String,
}

fn output_size_for(target: &Target) -> Dimensions {
  let options = Options {
    fps: 30,
    target: Some(target.clone()),
    output_type: FrameType::BGRAFrame,
    ..Default::default()
  };
  let [width, height] = scap::capturer::get_output_frame_size(&options);
  Dimensions::new(width, height)
}

/// List capturable displays.
///
/// # Errors
///
/// See [`CaptureError`] (notably [`CaptureError::PermissionDenied`] with
/// corrective guidance on macOS).
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
  require_permission()?;
  Ok(
    scap::get_all_targets()
      .into_iter()
      .filter_map(|target| match &target {
        Target::Display(display) => Some(DisplayInfo {
          id: display.id,
          title: display.title.clone(),
          dimensions: output_size_for(&target),
        }),
        Target::Window(_) => None,
      })
      .collect(),
  )
}

/// List capturable windows.
///
/// # Errors
///
/// See [`CaptureError`] (notably [`CaptureError::PermissionDenied`]).
pub fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
  require_permission()?;
  Ok(
    scap::get_all_targets()
      .into_iter()
      .filter_map(|target| match target {
        Target::Window(window) => Some(WindowInfo {
          id: window.id,
          title: window.title,
        }),
        Target::Display(_) => None,
      })
      .collect(),
  )
}

/// Guard `scap` calls that abort the process when TCC denies capture.
pub(crate) fn require_permission() -> Result<(), CaptureError> {
  if !scap::is_supported() {
    return Err(CaptureError::NotSupported);
  }
  if !scap::has_permission() && !scap::request_permission() {
    return Err(CaptureError::PermissionDenied);
  }
  Ok(())
}

/// Full-display capture backed by `scap`.
pub struct ScapCapture {
  capturer: scap::capturer::Capturer,
  dimensions: Dimensions,
  started: bool,
}

impl ScapCapture {
  /// Build a full-display capturer.
  ///
  /// `display_id = None` selects the primary display.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_display(display_id: Option<u32>, fps: u32) -> Result<Self, CaptureError> {
    require_permission()?;
    let targets = scap::get_all_targets();
    if !targets.iter().any(|t| matches!(t, Target::Display(_))) {
      return Err(CaptureError::NoDisplay);
    }

    let target = match display_id {
      Some(id) => targets
        .into_iter()
        .find(|t| matches!(t, Target::Display(d) if d.id == id))
        .ok_or(CaptureError::DisplayNotFound(id))?,
      // None → scap captures the primary display.
      None => return Self::build(None, fps),
    };
    Self::build(Some(target), fps)
  }

  /// Build a window capturer.
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_window(window_id: u32, fps: u32) -> Result<Self, CaptureError> {
    require_permission()?;
    let target = scap::get_all_targets()
      .into_iter()
      .find(|t| matches!(t, Target::Window(w) if w.id == window_id))
      .ok_or(CaptureError::WindowNotFound(window_id))?;
    Self::build(Some(target), fps)
  }

  fn build(target: Option<Target>, fps: u32) -> Result<Self, CaptureError> {
    let options = Options {
      fps: fps.clamp(1, 240),
      target,
      show_cursor: true,
      show_highlight: false,
      output_type: FrameType::BGRAFrame,
      ..Default::default()
    };
    let options = Self::fit_encodable(options);
    let [width, height] = scap::capturer::get_output_frame_size(&options);
    let capturer = scap::capturer::Capturer::build(options).map_err(|e| match e {
      scap::capturer::CapturerBuildError::NotSupported => CaptureError::NotSupported,
      scap::capturer::CapturerBuildError::PermissionNotGranted => CaptureError::PermissionDenied,
    })?;
    tracing::debug!(width, height, "configured scap display capture");
    Ok(Self {
      capturer,
      dimensions: Dimensions::new(width, height),
      started: false,
    })
  }

  /// Pick the largest `scap` output preset that keeps the capture within
  /// [`Dimensions::MAX_ENCODABLE`].
  ///
  /// Retina displays exceed the encoder's 4K ceiling (e.g. 3456x2234);
  /// `ScreenCaptureKit` then scales the frames natively, so downscaling
  /// costs no CPU in our pipeline. `get_output_frame_size` is the exact
  /// sizing function the scap engine uses, so the probe cannot drift.
  fn fit_encodable(mut options: Options) -> Options {
    let max = Dimensions::MAX_ENCODABLE;
    for resolution in [
      Resolution::Captured,
      Resolution::_2160p,
      Resolution::_1440p,
      Resolution::_1080p,
      Resolution::_720p,
      Resolution::_480p,
    ] {
      options.output_resolution = resolution;
      let [w, h] = scap::capturer::get_output_frame_size(&options);
      if w <= max.width && h <= max.height {
        if !matches!(resolution, Resolution::Captured) {
          tracing::info!(
              resolution = ?resolution,
              width = w,
              height = h,
              "scaling capture down to fit the encoder"
          );
        }
        return options;
      }
    }
    options
  }

  /// Start the underlying capture engine.
  ///
  /// # Errors
  ///
  /// See [`CaptureError::StartFailed`].
  pub fn start(&mut self) -> Result<(), CaptureError> {
    self
      .capturer
      .start_capture()
      .map_err(CaptureError::StartFailed)?;
    self.started = true;
    Ok(())
  }
}

impl Drop for ScapCapture {
  fn drop(&mut self) {
    // Never stop a stream that never started: the engine panics on a
    // missing stream, and a panic in a destructor aborts the process.
    if self.started {
      if let Err(error) = self.capturer.stop_capture() {
        tracing::debug!(error, "failed to stop scap capture");
      }
    }
  }
}

impl CaptureSource for ScapCapture {
  fn dimensions(&self) -> Dimensions {
    self.dimensions
  }

  fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
    loop {
      match self.capturer.get_next_frame() {
        Ok(Frame::BGRA(frame)) => {
          // scap emits zero-size placeholder frames when the
          // display is idle; skip them.
          if frame.width == 0 || frame.height == 0 || frame.data.is_empty() {
            continue;
          }
          return bgra_to_raw(frame);
        }
        Ok(other) => {
          tracing::trace!(?other, "discarding non-BGRA frame");
        }
        Err(std::sync::mpsc::RecvError) => return Err(CaptureError::Stopped),
      }
    }
  }
}

/// Convert a scap BGRA frame, deriving the true stride from the buffer
/// length (scap's reported width can disagree with `bytes_per_row`).
fn bgra_to_raw(frame: scap::frame::BGRAFrame) -> Result<RawFrame, CaptureError> {
  let height = u32::try_from(frame.height).unwrap_or(0).max(1);
  let data_len = frame.data.len();
  let height_usize = usize::try_from(height).unwrap_or(1);
  if data_len == 0 || data_len % height_usize != 0 {
    return Err(CaptureError::MalformedFrame(format!(
      "{data_len} bytes for {height} rows"
    )));
  }
  let row_bytes = data_len / height_usize;
  if row_bytes == 0 || row_bytes % 4 != 0 {
    return Err(CaptureError::MalformedFrame(format!(
      "row size {row_bytes} not a multiple of 4"
    )));
  }
  let width = u32::try_from(row_bytes / 4)
    .map_err(|_| CaptureError::MalformedFrame(format!("row of {row_bytes} B too wide")))?;
  Ok(RawFrame {
    width,
    height,
    stride: u32::try_from(row_bytes)
      .map_err(|_| CaptureError::MalformedFrame(format!("row of {row_bytes} B too wide")))?,
    format: PixelFormat::Bgra8,
    pixels: Bytes::from(frame.data),
    captured_at: Instant::now(),
  })
}

/// A synthetic capture source for tests and headless runs.
///
/// Emits a moving gradient pattern at the configured frame rate.
pub struct FakeCaptureSource {
  dimensions: Dimensions,
  frame_ms: u64,
  counter: u64,
}

impl FakeCaptureSource {
  #[must_use]
  pub fn new(dimensions: Dimensions, fps: u32) -> Self {
    Self {
      dimensions,
      frame_ms: 1000 / u64::from(fps.max(1)),
      counter: 0,
    }
  }
}

impl Default for FakeCaptureSource {
  fn default() -> Self {
    Self::new(Dimensions::new(64, 64), 30)
  }
}

impl CaptureSource for FakeCaptureSource {
  fn dimensions(&self) -> Dimensions {
    self.dimensions
  }

  #[expect(
    clippy::cast_possible_truncation,
    reason = "pattern values are reduced modulo 256"
  )]
  fn next_frame(&mut self) -> Result<RawFrame, CaptureError> {
    std::thread::sleep(Duration::from_millis(self.frame_ms));
    let t = self.counter;
    self.counter += 1;
    let w = usize::try_from(self.dimensions.width).unwrap_or(0);
    let h = usize::try_from(self.dimensions.height).unwrap_or(0);
    let mut pixels = vec![0_u8; w * h * 4];
    for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
      let x = (i / 4) % w.max(1);
      let shade = ((x + t as usize * 3) % 256) as u8;
      px[0] = shade;
      px[1] = 255 - shade;
      px[2] = (t % 256) as u8;
      px[3] = 255;
    }
    Ok(RawFrame {
      width: self.dimensions.width,
      height: self.dimensions.height,
      stride: self.dimensions.width * 4,
      format: PixelFormat::Bgra8,
      pixels: Bytes::from(pixels),
      captured_at: Instant::now(),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The chosen preset must always keep the primary display's output
  /// within the encoder's limits (real display; no capture permission
  /// needed — sizing only reads CoreGraphics metadata).
  #[test]
  #[cfg(target_os = "macos")]
  fn fit_encodable_stays_within_encoder_limits() {
    let options = Options {
      fps: 30,
      output_type: FrameType::BGRAFrame,
      ..Default::default()
    };
    let options = ScapCapture::fit_encodable(options);
    let [w, h] = scap::capturer::get_output_frame_size(&options);
    let max = Dimensions::MAX_ENCODABLE;
    assert!(
      w <= max.width && h <= max.height,
      "primary display {w}x{h} exceeds encodable limit"
    );
  }

  #[test]
  fn fake_source_yields_consistent_frames() {
    let mut fake = FakeCaptureSource::new(Dimensions::new(16, 8), 1000);
    let frame = fake.next_frame().unwrap();
    assert_eq!(frame.dimensions(), Dimensions::new(16, 8));
    assert_eq!(frame.stride, 64);
    assert_eq!(frame.pixels.len(), 16 * 8 * 4);
    assert_eq!(frame.format, PixelFormat::Bgra8);
    let second = fake.next_frame().unwrap();
    assert_eq!(second.pixels.len(), frame.pixels.len());
  }

  #[test]
  fn malformed_bgra_is_rejected() {
    let bad = scap::frame::BGRAFrame {
      display_time: 0,
      width: 4,
      height: 3,
      data: vec![0_u8; 10],
    };
    assert!(matches!(
      bgra_to_raw(bad),
      Err(CaptureError::MalformedFrame(_))
    ));
  }

  #[test]
  fn stride_is_derived_from_buffer() {
    // 4 rows of 8 bytes = 2 pixels wide despite width field saying 4.
    let f = scap::frame::BGRAFrame {
      display_time: 0,
      width: 4,
      height: 4,
      data: vec![7_u8; 32],
    };
    let raw = bgra_to_raw(f).unwrap();
    assert_eq!(raw.width, 2);
    assert_eq!(raw.height, 4);
    assert_eq!(raw.stride, 8);
  }
}
