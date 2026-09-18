//! Capture backend for platforms without a native implementation.
//!
//! Everything reports [`CaptureError::NotSupported`] so the crate (and the
//! rest of Lumen) still builds and runs on these targets — only live capture
//! is unavailable. A future PipeWire/portal backend for Linux would slot in
//! here as its own `#[path]` backend module.

use lumen_core::Dimensions;

use crate::{CaptureError, CaptureSource, DisplayInfo, WindowInfo};

/// Capture source that always reports [`CaptureError::NotSupported`].
pub struct PlatformCapture;

impl PlatformCapture {
  /// Always fails with [`CaptureError::NotSupported`].
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_display(_display_id: Option<u32>, _fps: u32) -> Result<Self, CaptureError> {
    crate::require_permission()?;
    Err(CaptureError::NotSupported)
  }

  /// Always fails with [`CaptureError::NotSupported`].
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn for_window(_window_id: u32, _fps: u32) -> Result<Self, CaptureError> {
    crate::require_permission()?;
    Err(CaptureError::NotSupported)
  }

  /// Always fails with [`CaptureError::NotSupported`].
  ///
  /// # Errors
  ///
  /// See [`CaptureError`].
  pub fn start(&mut self) -> Result<(), CaptureError> {
    Err(CaptureError::NotSupported)
  }
}

impl CaptureSource for PlatformCapture {
  fn dimensions(&self) -> Dimensions {
    Dimensions::new(0, 0)
  }

  fn next_frame(&mut self) -> Result<lumen_core::RawFrame, CaptureError> {
    Err(CaptureError::NotSupported)
  }
}

pub(crate) fn require_permission() -> Result<(), CaptureError> {
  Err(CaptureError::NotSupported)
}

pub(crate) fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
  crate::require_permission()?;
  Err(CaptureError::NotSupported)
}

pub(crate) fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
  crate::require_permission()?;
  Err(CaptureError::NotSupported)
}
