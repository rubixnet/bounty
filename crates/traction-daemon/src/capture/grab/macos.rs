//! ScreenCaptureKit `SCScreenshotManager` single-shot grab (macOS 14+).
//!
//! The permission check is `CGPreflightScreenCaptureAccess`, which never
//! prompts. Without access the grab stops there with `permission_denied`,
//! before any ScreenCaptureKit call that could show the system prompt. The
//! only prompting call (the Screen Recording access request) belongs to
//! `visual-proof enable` (S2); a source scan keeps it everywhere else out.
//!
//! `--display` is a `CGDirectDisplayID`. The focus is the frontmost
//! application's process id, and its display is the main display (the one
//! with the menu bar and the key window), a documented S1 simplification.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_app_kit::NSWorkspace;
use objc2_core_graphics::{
    CGDataProvider, CGImage, CGMainDisplayID, CGPreflightScreenCaptureAccess,
};
use objc2_foundation::{NSArray, NSError, NSOperatingSystemVersion, NSProcessInfo};
use objc2_screen_capture_kit::{
    SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
};

use super::{Bgra, Focus, GrabError};

/// `SCStreamErrorUserDeclined`.
const SC_STREAM_ERROR_USER_DECLINED: isize = -3801;

/// How long one ScreenCaptureKit completion handler may take.
const SCK_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub fn focus() -> Option<Focus> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let pid = app.processIdentifier();
    Some(Focus {
        token: format!("pid:{pid}"),
        display: CGMainDisplayID().to_string(),
    })
}

fn at_least_macos_14() -> bool {
    NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
        majorVersion: 14,
        minorVersion: 0,
        patchVersion: 0,
    })
}

pub fn grab(display: &str) -> Result<Bgra, GrabError> {
    if !at_least_macos_14() {
        return Err(GrabError::Unsupported);
    }
    let display_id: u32 = display.parse().map_err(|_| GrabError::Error)?;
    // Never prompts; without access nothing below runs.
    if !CGPreflightScreenCaptureAccess() {
        return Err(GrabError::PermissionDenied);
    }
    let content = shareable_content()?;
    // SAFETY: plain property reads on a live SCShareableContent.
    let displays = unsafe { content.displays() };
    let target = displays
        .iter()
        // SAFETY: plain property read.
        .find(|d| unsafe { d.displayID() } == display_id)
        .ok_or(GrabError::Error)?;
    // SAFETY: the display came from SCShareableContent; an empty exclusion list.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &target,
            &NSArray::new(),
        )
    };
    // SAFETY: a fresh configuration; plain setters.
    let config = unsafe {
        let config = SCStreamConfiguration::new();
        config.setWidth(target.width().max(1) as usize);
        config.setHeight(target.height().max(1) as usize);
        config.setShowsCursor(false);
        config
    };
    let image = screenshot(&filter, &config)?;
    bgra_from(&image)
}

fn shareable_content() -> Result<Retained<SCShareableContent>, GrabError> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: the handler's object arguments are valid for the call; we
            // retain the content to keep it past the handler.
            let content = unsafe { Retained::retain(content) };
            let _ = tx.try_send(content.ok_or(classify(error)));
        },
    );
    // SAFETY: the block lives until the call returns and is copied by SCK.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.recv_timeout(SCK_REPLY_TIMEOUT)
        .map_err(|_| GrabError::Error)?
}

fn screenshot(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
) -> Result<objc2_core_foundation::CFRetained<CGImage>, GrabError> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let image = NonNull::new(image).map(|image| {
            // SAFETY: a valid CGImage for the duration of the handler; retained.
            unsafe { objc2_core_foundation::CFRetained::retain(image) }
        });
        let _ = tx.try_send(image.ok_or(classify(error)));
    });
    // SAFETY: valid filter and configuration; SCK copies the block.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            filter,
            config,
            Some(&block),
        );
    }
    rx.recv_timeout(SCK_REPLY_TIMEOUT)
        .map_err(|_| GrabError::Error)?
}

/// `SCStreamErrorUserDeclined` (-3801) is a missing permission; anything else
/// is an error. The error's text is never read.
fn classify(error: *mut NSError) -> GrabError {
    // SAFETY: null or a valid NSError for the duration of the handler.
    match unsafe { error.as_ref() } {
        Some(error) if error.code() == SC_STREAM_ERROR_USER_DECLINED => GrabError::PermissionDenied,
        _ => GrabError::Error,
    }
}

fn bgra_from(image: &CGImage) -> Result<Bgra, GrabError> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    let stride = CGImage::bytes_per_row(Some(image));
    if CGImage::bits_per_pixel(Some(image)) != 32 || width == 0 || height == 0 {
        return Err(GrabError::Error);
    }
    let provider = CGImage::data_provider(Some(image)).ok_or(GrabError::Error)?;
    let data = CGDataProvider::data(Some(&provider)).ok_or(GrabError::Error)?;
    let bytes = data.to_vec();
    if stride < width * 4 || bytes.len() < stride * height {
        return Err(GrabError::Error);
    }
    // SDR screenshots are BGRA (SCScreenshotManager docs); rows lose padding.
    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in bytes.chunks_exact(stride).take(height) {
        pixels.extend_from_slice(&row[..width * 4]);
    }
    Ok(Bgra {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deviation 3: GitHub's macOS runners cannot grant Screen Recording, so
    /// CI checks the real path ends in a clean status (no panic, no prompt).
    #[test]
    fn the_real_grab_returns_a_closed_status_without_access() {
        let display = CGMainDisplayID().to_string();
        match grab(&display) {
            Ok(image) => assert!(image.width > 0 && image.height > 0),
            Err(error) if CGPreflightScreenCaptureAccess() => {
                panic!("access is granted but the grab failed: {error:?}")
            }
            Err(error) => assert_eq!(error, GrabError::PermissionDenied),
        }
    }

    /// Run once by the human with Screen Recording granted (deviation 3).
    #[test]
    #[ignore = "needs Screen Recording access; run locally (docs/steps.md deviation 3)"]
    fn macos_real_screencapturekit_grab() {
        let image = grab(&CGMainDisplayID().to_string()).expect("real grab");
        assert!(image.width >= 640 && image.height >= 480);
        // The WebP quality measurement (ticket 01): sizes at 720p on this
        // real screen. Nothing is written to disk.
        let scaled = crate::capture::image::downscale(&image);
        for quality in [5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 40.0, 60.0, 80.0] {
            let webp = webp::Encoder::from_rgb(&scaled.pixels, scaled.width, scaled.height)
                .encode(quality);
            eprintln!(
                "{}x{} q{quality}: {} bytes",
                scaled.width,
                scaled.height,
                webp.len()
            );
        }
        assert!(
            image
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[..3] != [0, 0, 0])
        );
    }
}
