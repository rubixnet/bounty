//! Foreground application -> [`AppCategory`], one compiled-in table per OS.
//!
//! The raw identifier (bundle ID, executable name, `WM_CLASS`) is borrowed
//! for the lookup and never stored, logged or returned: only the category
//! leaves this module. Anything not in a table is `Other`.
//!
//! Every table is compiled on every OS so each can be unit-tested on any
//! host; a backend calls only its own.
#![allow(
    dead_code,
    reason = "each OS backend calls only its own table; all are unit-tested everywhere"
)]

use traction_core::AppCategory;

use AppCategory::{AiDesktop, Browser, Ide, Other, Terminal};

const MACOS_BUNDLE_IDS: &[(&str, AppCategory)] = &[
    ("com.microsoft.VSCode", Ide),
    ("com.microsoft.VSCodeInsiders", Ide),
    ("com.todesktop.230313mzl4w4u92", Ide),
    ("com.exafunction.windsurf", Ide),
    ("dev.zed.Zed", Ide),
    ("dev.zed.Zed-Preview", Ide),
    ("com.sublimetext.4", Ide),
    ("com.apple.dt.Xcode", Ide),
    ("com.vscodium", Ide),
    ("com.apple.Terminal", Terminal),
    ("com.googlecode.iterm2", Terminal),
    ("dev.warp.Warp-Stable", Terminal),
    ("net.kovidgoyal.kitty", Terminal),
    ("com.github.wez.wezterm", Terminal),
    ("io.alacritty", Terminal),
    ("com.mitchellh.ghostty", Terminal),
    ("com.apple.Safari", Browser),
    ("com.google.Chrome", Browser),
    ("org.mozilla.firefox", Browser),
    ("company.thebrowser.Browser", Browser),
    ("com.microsoft.edgemac", Browser),
    ("com.brave.Browser", Browser),
    ("com.operasoftware.Opera", Browser),
    ("com.anthropic.claudefordesktop", AiDesktop),
    ("com.openai.chat", AiDesktop),
    ("com.openai.codex", AiDesktop),
];

/// Bundle-ID prefixes whose whole family is one category (every JetBrains
/// IDE has its own ID under `com.jetbrains.`).
const MACOS_BUNDLE_PREFIXES: &[(&str, AppCategory)] = &[("com.jetbrains.", Ide)];

const WINDOWS_EXECUTABLES: &[(&str, AppCategory)] = &[
    ("code.exe", Ide),
    ("code - insiders.exe", Ide),
    ("cursor.exe", Ide),
    ("windsurf.exe", Ide),
    ("zed.exe", Ide),
    ("devenv.exe", Ide),
    ("idea64.exe", Ide),
    ("pycharm64.exe", Ide),
    ("webstorm64.exe", Ide),
    ("clion64.exe", Ide),
    ("goland64.exe", Ide),
    ("rustrover64.exe", Ide),
    ("sublime_text.exe", Ide),
    ("notepad++.exe", Ide),
    ("windowsterminal.exe", Terminal),
    ("wt.exe", Terminal),
    ("cmd.exe", Terminal),
    ("powershell.exe", Terminal),
    ("pwsh.exe", Terminal),
    ("conhost.exe", Terminal),
    ("alacritty.exe", Terminal),
    ("wezterm-gui.exe", Terminal),
    ("mintty.exe", Terminal),
    ("chrome.exe", Browser),
    ("msedge.exe", Browser),
    ("firefox.exe", Browser),
    ("brave.exe", Browser),
    ("opera.exe", Browser),
    ("vivaldi.exe", Browser),
    ("arc.exe", Browser),
    ("claude.exe", AiDesktop),
    ("chatgpt.exe", AiDesktop),
    ("codex.exe", AiDesktop),
];

const X11_WM_CLASSES: &[(&str, AppCategory)] = &[
    ("code", Ide),
    ("code-oss", Ide),
    ("vscodium", Ide),
    ("cursor", Ide),
    ("windsurf", Ide),
    ("zed", Ide),
    ("dev.zed.zed", Ide),
    ("sublime_text", Ide),
    ("jetbrains-idea", Ide),
    ("jetbrains-pycharm", Ide),
    ("jetbrains-webstorm", Ide),
    ("jetbrains-clion", Ide),
    ("jetbrains-goland", Ide),
    ("jetbrains-rustrover", Ide),
    ("gnome-terminal-server", Terminal),
    ("gnome-terminal", Terminal),
    ("konsole", Terminal),
    ("xterm", Terminal),
    ("alacritty", Terminal),
    ("kitty", Terminal),
    ("wezterm", Terminal),
    ("org.wezfurlong.wezterm", Terminal),
    ("xfce4-terminal", Terminal),
    ("terminator", Terminal),
    ("tilix", Terminal),
    ("foot", Terminal),
    ("ghostty", Terminal),
    ("com.mitchellh.ghostty", Terminal),
    ("firefox", Browser),
    ("navigator", Browser),
    ("google-chrome", Browser),
    ("chromium", Browser),
    ("chromium-browser", Browser),
    ("brave-browser", Browser),
    ("microsoft-edge", Browser),
    ("opera", Browser),
    ("vivaldi-stable", Browser),
    ("epiphany", Browser),
    ("claude", AiDesktop),
    ("chatgpt", AiDesktop),
    ("codex", AiDesktop),
];

fn lookup(table: &[(&str, AppCategory)], key: &str) -> Option<AppCategory> {
    table
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, category)| *category)
}

/// A macOS bundle identifier (case-insensitive).
pub fn macos_bundle_id(bundle_id: &str) -> AppCategory {
    lookup(MACOS_BUNDLE_IDS, bundle_id)
        .or_else(|| {
            MACOS_BUNDLE_PREFIXES.iter().find_map(|(prefix, category)| {
                let head = bundle_id.get(..prefix.len())?;
                head.eq_ignore_ascii_case(prefix).then_some(*category)
            })
        })
        .unwrap_or(Other)
}

/// A Windows executable name or full path (either separator, any case,
/// `.exe` suffix optional).
pub fn windows_executable(path: &str) -> AppCategory {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    if let Some(category) = lookup(WINDOWS_EXECUTABLES, name) {
        return category;
    }
    let has_exe = name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe");
    if has_exe {
        Other
    } else {
        lookup(WINDOWS_EXECUTABLES, &format!("{name}.exe")).unwrap_or(Other)
    }
}

/// An X11 `WM_CLASS` pair (instance, class). Either string may match.
pub fn x11_wm_class(instance: &str, class: &str) -> AppCategory {
    lookup(X11_WM_CLASSES, class)
        .or_else(|| lookup(X11_WM_CLASSES, instance))
        .unwrap_or(Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_bundle_ids_resolve_and_ignore_case() {
        assert_eq!(macos_bundle_id("com.microsoft.VSCode"), Ide);
        assert_eq!(macos_bundle_id("COM.APPLE.TERMINAL"), Terminal);
        assert_eq!(macos_bundle_id("com.google.Chrome"), Browser);
        assert_eq!(macos_bundle_id("com.anthropic.claudefordesktop"), AiDesktop);
        assert_eq!(macos_bundle_id("com.jetbrains.rustrover"), Ide);
        assert_eq!(macos_bundle_id("com.jetbrain"), Other);
        assert_eq!(macos_bundle_id("com.spotify.client"), Other);
        assert_eq!(macos_bundle_id(""), Other);
    }

    #[test]
    fn windows_executables_resolve_from_names_and_paths() {
        assert_eq!(windows_executable("Code.exe"), Ide);
        assert_eq!(
            windows_executable(r"C:\Program Files\Microsoft VS Code\CODE.EXE"),
            Ide
        );
        assert_eq!(windows_executable(r"c:\windows\system32\cmd.exe"), Terminal);
        assert_eq!(
            windows_executable("C:/Program Files/Mozilla/firefox.exe"),
            Browser
        );
        assert_eq!(windows_executable("chatgpt"), AiDesktop);
        assert_eq!(windows_executable("notepad.exe"), Other);
        assert_eq!(windows_executable(r"C:\x\code.exe.bak"), Other);
        assert_eq!(windows_executable(""), Other);
    }

    #[test]
    fn x11_classes_resolve_from_either_string_and_ignore_case() {
        assert_eq!(x11_wm_class("code", "Code"), Ide);
        assert_eq!(
            x11_wm_class("gnome-terminal-server", "Gnome-terminal"),
            Terminal
        );
        assert_eq!(x11_wm_class("Navigator", "firefox"), Browser);
        assert_eq!(x11_wm_class("unknown", "Claude"), AiDesktop);
        assert_eq!(x11_wm_class("mystery", "Mystery"), Other);
        assert_eq!(x11_wm_class("", ""), Other);
    }

    #[test]
    fn every_table_entry_is_unique_ignoring_case() {
        for table in [MACOS_BUNDLE_IDS, WINDOWS_EXECUTABLES, X11_WM_CLASSES] {
            for (i, (a, _)) in table.iter().enumerate() {
                assert!(
                    !table[..i].iter().any(|(b, _)| a.eq_ignore_ascii_case(b)),
                    "duplicate {a}"
                );
            }
        }
    }
}
