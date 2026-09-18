//! Capture and encoding for Lumen: display/window/system-audio capture and
//! H.264 video + Opus audio encoding behind portable traits.
//!
//! [`capture`] drives each platform's native capture API directly (macOS
//! `ScreenCaptureKit`, Windows `Windows.Graphics.Capture`) and provides fake
//! sources for tests. [`encoder`] wraps the bundled `openh264` and `libopus`
//! codecs and provides fake encoders for tests. Both layers exchange the raw
//! and encoded frame types from `lumen-core`.

pub mod capture;
pub mod encoder;
