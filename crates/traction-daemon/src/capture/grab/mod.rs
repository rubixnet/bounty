//! Native screen grabs for the `capture-frame` child (spec Q3): ScreenCaptureKit
//! `SCScreenshotManager` on macOS 14+, GDI `BitBlt` on Windows and `x11rb`
//! `GetImage` on X11. Wayland and older macOS report `unsupported`. Only the
//! child process ever calls into this module; the daemon never holds pixels.
//!
//! A [`Focus`] is what the daemon reads before spawning and passes on as
//! `--display` and `--expect-focus`; the child reads it again right after the
//! grab and discards the Frame if it changed. Its `token` is opaque (a window
//! or process identity, never a title) and is never stored.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// One grabbed display, 32-bit BGRA (alpha ignored), top-down rows with no
/// padding.
pub struct Bgra {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Why a grab produced no pixels. Each maps to one closed child status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabError {
    /// The OS has not granted screen capture (macOS Screen Recording).
    PermissionDenied,
    /// No supported backend here (Wayland, macOS < 14, no display).
    Unsupported,
    /// The backend failed.
    Error,
}

/// The focused window's identity and the display that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    /// Opaque focus identity (window handle/id or process id).
    pub token: String,
    /// The backend's display id (`--display`).
    pub display: String,
}

/// The current focus, or `None` when nothing is focused or this platform
/// cannot tell (no capture then: fail closed).
pub fn focus() -> Option<Focus> {
    #[cfg(target_os = "linux")]
    return linux::focus();
    #[cfg(target_os = "macos")]
    return macos::focus();
    #[cfg(windows)]
    return windows::focus();
    #[allow(unreachable_code)]
    None
}

/// Grabs the display named by `display` (a [`Focus::display`]).
pub fn grab(display: &str) -> Result<Bgra, GrabError> {
    #[cfg(target_os = "linux")]
    return linux::grab(display);
    #[cfg(target_os = "macos")]
    return macos::grab(display);
    #[cfg(windows)]
    return windows::grab(display);
    #[allow(unreachable_code)]
    {
        let _ = display;
        Err(GrabError::Unsupported)
    }
}
