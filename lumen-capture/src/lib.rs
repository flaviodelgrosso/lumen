//! Display/window enumeration, screen capture, and system-audio capture
//! behind portable traits.
//!
//! Each platform drives its native capture API directly: macOS uses
//! `ScreenCaptureKit` (the `screencapturekit` crate), Windows uses
//! `Windows.Graphics.Capture` (the `windows-capture` crate), and other
//! platforms report [`CaptureError::NotSupported`] so the crate still builds
//! for them. Tests use [`FakeCaptureSource`] / [`FakeAudioCapture`] so they
//! never need display hardware.
//!
//! Platform types stay private to this crate: consumers see only
//! [`PlatformCapture`], [`PlatformAudioCapture`], [`CaptureSource`],
//! [`AudioCaptureSource`], [`FakeCaptureSource`], [`FakeAudioCapture`], and
//! the enumeration functions.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use lumen_core::{Dimensions, PixelFormat, RawFrame};
use thiserror::Error;

pub mod audio;
/// Windowed-sinc resampler used only by the macOS audio path (SCK delivers
/// arbitrary device rates); other platforms have no system-audio backend.
#[cfg(target_os = "macos")]
mod resample;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[path = "unsupported.rs"]
mod backend;

pub use audio::{AudioCaptureSource, FakeAudioCapture, PlatformAudioCapture};
pub use backend::PlatformCapture;

pub(crate) use backend::require_permission;

/// Tail appended to [`CaptureError::PermissionDenied`] with the guidance
/// that actually applies on the current OS.
#[cfg(target_os = "macos")]
const PERMISSION_HINT: &str = concat!(
  "on macOS open System Settings \u{2192} Privacy & Security \u{2192} Screen ",
  "Recording, enable your terminal app, then rerun lumen"
);
/// Windows grants capture to ordinary processes; the realistic blockers are
/// elevated (admin) windows and enterprise screen-capture policy.
#[cfg(target_os = "windows")]
const PERMISSION_HINT: &str = concat!(
  "on Windows capture is blocked for elevated (Run as administrator) windows ",
  "and when a group policy disables screen capture; close the elevated ",
  "target or run lumen from an elevated terminal, then rerun lumen"
);
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PERMISSION_HINT: &str =
  "check your system's screen-recording privacy settings, then rerun lumen";

/// Note appended to [`CaptureError::AudioNotSupported`] per OS.
#[cfg(target_os = "macos")]
const AUDIO_UNSUPPORTED_NOTE: &str = " (macOS 13+ required)";
#[cfg(target_os = "windows")]
const AUDIO_UNSUPPORTED_NOTE: &str = " (system audio capture is not implemented on Windows)";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const AUDIO_UNSUPPORTED_NOTE: &str = "";

/// Errors from capture enumeration or frame delivery.
#[derive(Debug, Error)]
pub enum CaptureError {
  /// This platform/OS version cannot capture screens.
  #[error("screen capture is not supported on this platform")]
  NotSupported,
  /// The OS denied the screen-recording permission.
  ///
  /// The corrective guidance is chosen per OS so a Windows user never sees
  /// macOS System Settings instructions.
  #[error("screen-recording permission not granted; {PERMISSION_HINT}")]
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
  #[error("system audio capture is not supported on this platform{AUDIO_UNSUPPORTED_NOTE}")]
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

/// List capturable displays.
///
/// # Errors
///
/// See [`CaptureError`] (notably [`CaptureError::PermissionDenied`] with
/// corrective guidance on macOS).
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
  backend::list_displays()
}

/// List capturable windows.
///
/// # Errors
///
/// See [`CaptureError`] (notably [`CaptureError::PermissionDenied`]).
pub fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
  backend::list_windows()
}

/// Whether `native` fits inside [`Dimensions::MAX_ENCODABLE`] unchanged.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) const fn fits_encodable(native: Dimensions) -> bool {
  native.width <= Dimensions::MAX_ENCODABLE.width
    && native.height <= Dimensions::MAX_ENCODABLE.height
}

/// Smallest side the H.264 encoder accepts.
pub(crate) const MIN_ENCODABLE: u32 = 16;

/// The even size that keeps `native` within the encoder's bounds while
/// preserving its aspect ratio as far as possible.
///
/// Sizes are always floored to even numbers because H.264 chroma sampling
/// requires even dimensions (e.g. a 1728x1117 logical display becomes
/// 1728x1116), and Retina displays above the 4K ceiling are scaled down;
/// `ScreenCaptureKit` performs the scaling natively, so it costs no CPU in
/// our pipeline. Degenerate targets are raised to the encoder's 16x16
/// floor.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn fit_encodable(native: Dimensions) -> Dimensions {
  let max = Dimensions::MAX_ENCODABLE;
  let scale = (f64::from(max.width) / f64::from(native.width.max(1)))
    .min(f64::from(max.height) / f64::from(native.height.max(1)))
    .min(1.0);
  #[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the scale is at most 1.0, so each product is bounded by the \
              matching `Dimensions::MAX_ENCODABLE` side"
  )]
  let fit_side = |side: u32, limit: u32| -> u32 {
    // Truncating cast floors the product; masking the low bit rounds down
    // to an even number; the clamp keeps the result encodable.
    ((f64::from(side) * scale) as u32 & !1).clamp(MIN_ENCODABLE, limit)
  };
  let fitted = Dimensions::new(
    fit_side(native.width, max.width),
    fit_side(native.height, max.height),
  );
  if fitted != native {
    tracing::info!(
      native = %native,
      fitted = %fitted,
      "adjusted capture size to fit the encoder"
    );
  }
  fitted
}

/// Build a [`RawFrame`] from a BGRA buffer with an explicit row stride.
///
/// Both native backends hand over pitch-padded buffers (`CVPixelBuffer`
/// `bytesPerRow` on macOS, D3D11 `RowPitch` on Windows), so the logical
/// size is authoritative and the stride is carried through to the encoder,
/// which repacks padded rows. Every inconsistency is reported as
/// [`CaptureError::MalformedFrame`]; nothing here can panic.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
fn bgra_to_raw(
  width: u32,
  height: u32,
  stride: u32,
  data: Vec<u8>,
) -> Result<RawFrame, CaptureError> {
  if width == 0 || height == 0 {
    return Err(CaptureError::MalformedFrame(format!(
      "zero-size frame {width}x{height}"
    )));
  }
  if stride == 0 || stride % 4 != 0 {
    return Err(CaptureError::MalformedFrame(format!(
      "row stride {stride} not a positive multiple of 4"
    )));
  }
  let expected = usize::try_from(
    u64::from(stride)
      .checked_mul(u64::from(height))
      .ok_or_else(|| {
        CaptureError::MalformedFrame(format!("stride {stride} x {height} rows overflows"))
      })?,
  )
  .map_err(|_| {
    CaptureError::MalformedFrame(format!("{stride}x{height} buffer exceeds memory size"))
  })?;
  if data.len() != expected {
    return Err(CaptureError::MalformedFrame(format!(
      "{} bytes for {height} rows at stride {stride}",
      data.len()
    )));
  }
  let row_pixels = width
    .checked_mul(4)
    .ok_or_else(|| CaptureError::MalformedFrame(format!("width {width} too wide")))?;
  if stride < row_pixels {
    return Err(CaptureError::MalformedFrame(format!(
      "row stride {stride} B below width {width} px"
    )));
  }
  Ok(RawFrame {
    width,
    height,
    stride,
    format: PixelFormat::Bgra8,
    pixels: Bytes::from(data),
    captured_at: Instant::now(),
  })
}

/// Bounded producer side of the native-engine → pipeline frame hop.
///
/// The native capture callbacks (SCK dispatch queues, the WGC engine thread)
/// share one `Arc<Self>`; [`Self::close`] drops the sole sender so the
/// consumer's blocking [`Receiver::recv`] wakes with `Disconnected`, which
/// the backends map to [`CaptureError::Stopped`].
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub(crate) struct FrameSender<T> {
  tx: Mutex<Option<SyncSender<T>>>,
}

/// What happened to a frame handed to [`FrameSender::send`].
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SendOutcome {
  /// The frame entered the queue.
  Sent,
  /// The queue was full; the frame was dropped. For live capture, latency
  /// beats backlog — the pipeline keeps only the latest frame anyway.
  Full,
  /// The consumer is gone (or the channel was closed).
  Closed,
}

#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
impl<T> FrameSender<T> {
  /// Create a bounded channel; the [`Receiver`] side backs the blocking
  /// [`CaptureSource::next_frame`] / [`AudioCaptureSource::next_audio`] API.
  pub(crate) fn bounded(capacity: usize) -> (Self, Receiver<T>) {
    let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
    (
      Self {
        tx: Mutex::new(Some(tx)),
      },
      rx,
    )
  }

  /// Non-blocking send; never stalls the capture callback.
  pub(crate) fn send(&self, item: T) -> SendOutcome {
    let Ok(guard) = self.tx.lock() else {
      return SendOutcome::Closed;
    };
    let Some(tx) = guard.as_ref() else {
      return SendOutcome::Closed;
    };
    match tx.try_send(item) {
      Ok(()) => SendOutcome::Sent,
      Err(TrySendError::Full(_)) => SendOutcome::Full,
      Err(TrySendError::Disconnected(_)) => SendOutcome::Closed,
    }
  }

  /// Drop the sender; wakes the consumer with `Disconnected`.
  pub(crate) fn close(&self) {
    if let Ok(mut guard) = self.tx.lock() {
      drop(guard.take());
    }
  }
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

  // ── encoder-fit policy ──

  #[test]
  fn native_size_within_limit_is_untouched() {
    let native = Dimensions::new(1920, 1080);
    assert_eq!(fit_encodable(native), native);
    assert!(fits_encodable(native));
  }

  #[test]
  fn max_encodable_boundary_is_untouched() {
    let boundary = Dimensions::MAX_ENCODABLE;
    assert!(fits_encodable(boundary));
    assert_eq!(fit_encodable(boundary), boundary);
  }

  /// Retina-class displays (3456x2234) must land inside the 4K ceiling with
  /// the aspect ratio preserved — the exact case the old preset ladder
  /// handled by snapping to 2160p.
  #[test]
  fn retina_display_fits_within_encoder_limits() {
    let fitted = fit_encodable(Dimensions::new(3456, 2234));
    let max = Dimensions::MAX_ENCODABLE;
    assert!(
      fitted.width <= max.width && fitted.height <= max.height,
      "{fitted}"
    );
    assert_eq!(fitted.width % 2, 0);
    assert_eq!(fitted.height % 2, 0);
    let native_ratio = 3456.0 / 2234.0;
    let fitted_ratio = f64::from(fitted.width) / f64::from(fitted.height);
    assert!(
      (native_ratio - fitted_ratio).abs() / native_ratio < 0.01,
      "aspect ratio drifted: {fitted}"
    );
  }

  /// 8K (7680x4320) fits on both axes at the same scale.
  #[test]
  fn oversized_display_fits_and_stays_even() {
    let fitted = fit_encodable(Dimensions::new(7680, 4320));
    assert_eq!(fitted, Dimensions::new(3840, 2160));
  }

  #[test]
  fn extreme_aspect_ratio_keeps_positive_even_output() {
    // 5120x640 ultrawide strip: height is untouched, width scales.
    let fitted = fit_encodable(Dimensions::new(5120, 640));
    assert_eq!(fitted, Dimensions::new(3840, 480));
    // An absurdly tall strip still produces encoder-legal output.
    let tall = fit_encodable(Dimensions::new(4, 8000));
    assert!(tall.width >= MIN_ENCODABLE && tall.height >= MIN_ENCODABLE);
    assert_eq!(tall.width % 2, 0);
    assert_eq!(tall.height % 2, 0);
  }

  /// A logical (scaled) display size like 1728x1117 is in range but odd;
  /// it must be floored to even without any downscaling.
  #[test]
  fn odd_in_range_size_is_floored_to_even() {
    assert_eq!(
      fit_encodable(Dimensions::new(1728, 1117)),
      Dimensions::new(1728, 1116)
    );
  }

  /// Degenerate targets are raised to the encoder floor instead of being
  /// rejected later.
  #[test]
  fn degenerate_size_is_raised_to_encoder_floor() {
    assert_eq!(
      fit_encodable(Dimensions::new(10, 10)),
      Dimensions::new(16, 16)
    );
    assert_eq!(
      fit_encodable(Dimensions::new(0, 0)),
      Dimensions::new(16, 16)
    );
  }

  // ── frame conversion policy ──

  #[test]
  fn malformed_bgra_is_rejected() {
    // 10 bytes cannot be 3 rows of any 4-aligned stride for a 4 px width.
    assert!(matches!(
      bgra_to_raw(4, 3, 16, vec![0_u8; 10]),
      Err(CaptureError::MalformedFrame(_))
    ));
    // Zero-size frames are invalid, not deliverable.
    assert!(matches!(
      bgra_to_raw(0, 4, 16, vec![0_u8; 64]),
      Err(CaptureError::MalformedFrame(_))
    ));
    // A stride below the logical width would let the encoder read past rows.
    assert!(matches!(
      bgra_to_raw(4, 4, 8, vec![0_u8; 32]),
      Err(CaptureError::MalformedFrame(_))
    ));
    // A stride that is not a multiple of 4 cannot hold BGRA pixels.
    assert!(matches!(
      bgra_to_raw(2, 4, 7, vec![0_u8; 28]),
      Err(CaptureError::MalformedFrame(_))
    ));
  }

  /// `CVPixelBuffer`/D3D11 hand over pitch-padded buffers; the logical width
  /// must survive and the pitch must become the stride (the encoder repacks
  /// padded rows).
  #[test]
  fn padded_pitch_keeps_logical_width() {
    // 4 rows at pitch 12 B for a 2 px (8 B) logical width.
    let raw = bgra_to_raw(2, 4, 12, vec![7_u8; 48]).unwrap();
    assert_eq!(raw.width, 2);
    assert_eq!(raw.height, 4);
    assert_eq!(raw.stride, 12);
    assert_eq!(raw.pixels.len(), 48);
  }

  #[test]
  fn tight_pitch_round_trips() {
    let raw = bgra_to_raw(2, 4, 8, vec![7_u8; 32]).unwrap();
    assert_eq!(raw.width, 2);
    assert_eq!(raw.stride, 8);
    assert_eq!(raw.stride, raw.width * 4);
    assert_eq!(raw.dimensions(), Dimensions::new(2, 4));
  }

  // ── frame channel plumbing ──

  #[test]
  fn frame_channel_delivers_and_wakes_consumer_on_close() {
    let (tx, rx) = FrameSender::<RawFrame>::bounded(1);
    let frame = bgra_to_raw(1, 1, 4, vec![0_u8; 4]).unwrap();
    assert_eq!(tx.send(frame.clone()), SendOutcome::Sent);
    let received = rx.recv().expect("frame delivered");
    assert_eq!(received.dimensions(), frame.dimensions());

    tx.close();
    assert_eq!(tx.send(frame), SendOutcome::Closed);
    assert!(
      rx.recv().is_err(),
      "a closed channel must wake the consumer with Disconnected"
    );
  }

  #[test]
  fn frame_channel_drops_on_full_without_blocking() {
    let (tx, _rx) = FrameSender::<RawFrame>::bounded(1);
    let frame = bgra_to_raw(1, 1, 4, vec![0_u8; 4]).unwrap();
    assert_eq!(tx.send(frame.clone()), SendOutcome::Sent);
    // Queue full: the newest frame is dropped, the callback never stalls.
    assert_eq!(tx.send(frame), SendOutcome::Full);
  }

  #[test]
  fn frame_channel_send_after_consumer_gone_is_closed() {
    let (tx, rx) = FrameSender::<RawFrame>::bounded(1);
    drop(rx);
    let frame = bgra_to_raw(1, 1, 4, vec![0_u8; 4]).unwrap();
    assert_eq!(tx.send(frame), SendOutcome::Closed);
  }

  // ── fake source (headless CI) ──

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

  /// A Windows user must never be told to open macOS System Settings.
  #[test]
  #[cfg(target_os = "windows")]
  fn permission_error_gives_windows_guidance() {
    let msg = CaptureError::PermissionDenied.to_string();
    assert!(msg.contains("Windows"), "{msg}");
    assert!(!msg.contains("macOS"), "{msg}");
    assert!(!msg.contains("System Settings"), "{msg}");
  }

  #[test]
  #[cfg(target_os = "windows")]
  fn audio_not_supported_error_gives_windows_guidance() {
    let msg = CaptureError::AudioNotSupported.to_string();
    assert!(msg.contains("Windows"), "{msg}");
    assert!(!msg.contains("macOS"), "{msg}");
  }

  /// The macOS guidance must stay intact (no regression from the per-OS split).
  #[test]
  #[cfg(target_os = "macos")]
  fn permission_error_keeps_macos_guidance() {
    let msg = CaptureError::PermissionDenied.to_string();
    assert!(msg.contains("macOS"), "{msg}");
    assert!(msg.contains("System Settings"), "{msg}");
    assert!(msg.contains("Screen Recording"), "{msg}");
  }

  #[test]
  #[cfg(target_os = "macos")]
  fn audio_not_supported_error_keeps_macos_guidance() {
    let msg = CaptureError::AudioNotSupported.to_string();
    assert!(msg.contains("macOS 13"), "{msg}");
  }
}
