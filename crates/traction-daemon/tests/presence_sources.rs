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
    for (path, text) in crate_sources() {
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
        ["CGPreflightScreenCapture", "Access"].concat(),
    ]);
}

#[test]
fn no_linux_backend_reads_dev_input() {
    assert_never_named(&[["/dev/", "input"].concat()]);
}
