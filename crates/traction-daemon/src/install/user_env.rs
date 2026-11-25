//! Windows: `TRACTION_TOKEN` as a per-user environment variable under
//! `HKCU\Environment`, so Claude Code started from any terminal, IDE or the
//! Start menu can authenticate. `HKCU` is the user's own hive: no admin
//! rights (and no UAC prompt) are needed.

use std::io;
use std::path::Path;

use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
};

use crate::token::BusToken;

/// The `HKCU` subkey Windows reads per-user environment variables from.
pub const ENVIRONMENT_KEY: &str = "Environment";
/// The variable's name.
pub const TOKEN_VAR: &str = "TRACTION_TOKEN";
/// How long running programs (Explorer) get to pick up the change.
const BROADCAST_TIMEOUT_MS: u32 = 2_000;

/// Sets `HKCU\<key>\TRACTION_TOKEN` to the bus token in `data_dir`
/// (creating the token file first if the daemon has never run), then tells
/// running programs the environment changed so new terminals see it.
pub fn set_token(data_dir: &Path, key: &str) -> io::Result<()> {
    let token = BusToken::load_or_create(data_dir)?;
    let value: Vec<u16> = token
        .reveal()
        .iter()
        .map(|&b| u16::from(b))
        .chain([0])
        .collect();
    let key_w = wide(key);
    let name_w = wide(TOKEN_VAR);
    let bytes = u32::try_from(value.len() * 2).map_err(io::Error::other)?;
    // SAFETY: every pointer is to a live, NUL-terminated UTF-16 buffer, and
    // `bytes` is the length of `value` in bytes (terminator included).
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key_w.as_ptr(),
            name_w.as_ptr(),
            REG_SZ,
            value.as_ptr().cast(),
            bytes,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if key.eq_ignore_ascii_case(ENVIRONMENT_KEY) {
        broadcast_environment_change();
    }
    Ok(())
}

/// `WM_SETTINGCHANGE` with `"Environment"`, so Explorer (and anything it
/// launches next) reloads the user's variables. Best effort.
fn broadcast_environment_change() {
    let area = wide(ENVIRONMENT_KEY);
    let mut result = 0usize;
    // SAFETY: `area` is a live NUL-terminated UTF-16 string for the call's
    // duration; SMTO_ABORTIFHUNG bounds the wait on hung windows.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            area.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            BROADCAST_TIMEOUT_MS,
            &mut result,
        );
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}
