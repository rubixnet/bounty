//! Source-level guards for the Presence Sensor's privacy promises.
//!
//! The banned names are assembled from pieces so this file does not trip
//! its own scan.

use std::path::{Path, PathBuf};

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn crate_sources() -> Vec<(PathBuf, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(files.len() > 10, "found the crate sources");
    files
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("read source");
            (path, text)
        })
        .collect()
}

fn assert_never_named(banned: &[String]) {
    assert_named_only_in(banned, &[]);
}

/// Like [`assert_never_named`], except in the files ending with one of
/// `allowed` (crate-relative, `/`-separated).
fn assert_named_only_in(banned: &[String], allowed: &[&str]) {
    for (path, text) in crate_sources() {
        let unix = path.to_string_lossy().replace('\\', "/");
        if allowed.iter().any(|a| unix.ends_with(a)) {
            continue;
        }
        for name in banned {
            assert!(
                !text.contains(name.as_str()),
                "{} names {name}",
                path.display()
            );
        }
    }
}

#[test]
fn no_windows_input_hook_exists_anywhere_in_the_crate() {
    assert_never_named(&[
        ["WH_KEYBOARD", "_LL"].concat(),
        ["WH_MOUSE", "_LL"].concat(),
        ["SetWindows", "HookEx"].concat(),
    ]);
}

#[test]
fn no_macos_permission_prompting_call_exists_anywhere_in_the_crate() {
    assert_never_named(&[
        ["CGEventTap", "Create"].concat(),
        ["AXIsProcessTrusted", "WithOptions"].concat(),
        ["CGRequestListen", "EventAccess"].concat(),
        ["CGRequestScreenCapture", "Access"].concat(),
    ]);
}

/// The Screen Recording preflight never prompts. Only the `capture-frame`
/// child's macOS grab calls it, so a missing permission stops there with
/// `permission_denied` before any ScreenCaptureKit call (S1).
#[test]
fn only_the_macos_grab_checks_screen_recording_access() {
    assert_named_only_in(
        &[["CGPreflightScreenCapture", "Access"].concat()],
        &["src/capture/grab/macos.rs"],
    );
    let grab = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/capture/grab/macos.rs"),
    )
    .expect("read the macOS grab");
    assert!(grab.contains(&["CGPreflightScreenCapture", "Access"].concat()));
}

#[test]
fn no_linux_backend_reads_dev_input() {
    assert_never_named(&[["/dev/", "input"].concat()]);
}
