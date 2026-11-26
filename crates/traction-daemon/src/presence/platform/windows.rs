//! Windows backend: tier `reduced` (idle + foreground executable category).
//!
//! No global input hook of any kind is installed (an unsigned binary with a
//! low-level hook trips Defender's keylogger heuristics). The Raw Input
//! `SendInput` spike could not be run from the macOS development host, so
//! the injection signal is not built and the tier stays `reduced`; see
//! `docs/protocol.md` section 2b for the outcome and what is still open.

use std::mem::size_of;

use traction_core::{AppCategory, TelemetryTier};
use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::super::Backend;
use super::super::aggregate::Sample;
use super::super::category;
use super::tick_delta_ms;

pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> Self {
        Self
    }
}

fn idle_ms() -> Option<u64> {
    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a properly sized, initialized LASTINPUTINFO.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // SAFETY: no arguments, no preconditions.
    let now = unsafe { GetTickCount() };
    Some(tick_delta_ms(now, info.dwTime))
}

fn foreground_category() -> Option<AppCategory> {
    // SAFETY: no preconditions; null means no foreground window.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: `hwnd` came from GetForegroundWindow and `pid` is writable.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return None;
    }
    // SAFETY: a limited query right needs no elevation.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if process.is_null() {
        // Typically an elevated window we may not query: not a failure.
        return Some(AppCategory::Other);
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units and `process` is a valid handle.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) };
    // SAFETY: `process` was opened above and is closed exactly once.
    unsafe { CloseHandle(process) };
    if ok == 0 {
        return Some(AppCategory::Other);
    }
    // The path lives only for this lookup.
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(category::windows_executable(&path))
}

impl Backend for WindowsBackend {
    fn tier(&self) -> TelemetryTier {
        TelemetryTier::Reduced
    }

    fn sample(&mut self) -> Sample {
        Sample {
            idle_ms: idle_ms(),
            category: foreground_category(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_reports_an_idle_time() {
        let mut backend = WindowsBackend::new();
        assert!(backend.sample().idle_ms.is_some());
    }
}
