//! GDI `BitBlt` of the monitor that holds the foreground window.
//!
//! `--display` is the `HMONITOR` value in decimal (monitor handles are
//! session-wide, so the daemon's value is valid in the child).

use std::mem::size_of;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetMonitorInfoW,
    HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use super::{Bgra, Focus, GrabError};

pub fn focus() -> Option<Focus> {
    // SAFETY: no preconditions; null means no foreground window.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: `hwnd` is a window handle; the flag never returns null.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
    Some(Focus {
        token: format!("hwnd:{:x}", hwnd as usize),
        display: (monitor as usize).to_string(),
    })
}

fn monitor_rect(monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rcWork: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dwFlags: 0,
    };
    // SAFETY: `info` is a properly sized MONITORINFO; a stale handle fails.
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then_some(info.rcMonitor)
}

pub fn grab(display: &str) -> Result<Bgra, GrabError> {
    let monitor = display.parse::<usize>().map_err(|_| GrabError::Error)? as HMONITOR;
    let rect = monitor_rect(monitor).ok_or(GrabError::Error)?;
    grab_rect(rect)
}

fn grab_rect(rect: RECT) -> Result<Bgra, GrabError> {
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return Err(GrabError::Error);
    }
    // SAFETY: every GDI object created here is released below, on every path.
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Err(GrabError::Error);
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let result = if memory.is_null() || bitmap.is_null() {
            Err(GrabError::Error)
        } else {
            let previous = SelectObject(memory, bitmap);
            let copied = BitBlt(
                memory,
                0,
                0,
                width,
                height,
                screen,
                rect.left,
                rect.top,
                SRCCOPY | CAPTUREBLT,
            );
            SelectObject(memory, previous);
            if copied == 0 {
                Err(GrabError::Error)
            } else {
                read_bitmap(memory, bitmap, width, height)
            }
        };
        if !bitmap.is_null() {
            DeleteObject(bitmap);
        }
        if !memory.is_null() {
            DeleteDC(memory);
        }
        ReleaseDC(null_mut(), screen);
        result
    }
}

/// # Safety
/// `dc` and `bitmap` are valid and `bitmap` is not selected into a DC.
unsafe fn read_bitmap(
    dc: windows_sys::Win32::Graphics::Gdi::HDC,
    bitmap: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    width: i32,
    height: i32,
) -> Result<Bgra, GrabError> {
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        // Negative: top-down rows.
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: `pixels` holds exactly `height` top-down 32-bit rows of `width`.
    let rows = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if rows != height {
        return Err(GrabError::Error);
    }
    Ok(Bgra {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    //! Addition 3: GDI `BitBlt` must return real pixels on `windows-latest`.
    //! A topmost window filled with a known colour is drawn, then the
    //! primary monitor is grabbed through the real path above.

    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, MonitorFromPoint, UpdateWindow};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
        PeekMessageW, RegisterClassW, SW_SHOW, SetForegroundWindow, ShowWindow, TranslateMessage,
        WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    use super::*;

    /// Magenta: never produced by a black or default-desktop failure.
    const RED: u8 = 0xFF;
    const GREEN: u8 = 0x00;
    const BLUE: u8 = 0xFF;
    const SIDE: i32 = 400;

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: forwarded unchanged to the default procedure.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn pump(for_how_long: Duration) {
        let deadline = Instant::now() + for_how_long;
        while Instant::now() < deadline {
            // SAFETY: a zeroed MSG is a valid out-parameter.
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: standard message loop on this thread's queue.
            while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn bitblt_returns_a_known_colour_window_on_the_primary_display() {
        let class = wide("TractionBitBltCheck");
        // SAFETY: plain Win32 window creation on the test thread; the window
        // is destroyed before the test returns.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let brush = CreateSolidBrush(
                u32::from(RED) | (u32::from(GREEN) << 8) | (u32::from(BLUE) << 16),
            );
            let mut wc: WNDCLASSW = std::mem::zeroed();
            wc.lpfnWndProc = Some(window_proc);
            wc.hInstance = instance;
            wc.lpszClassName = class.as_ptr();
            wc.hbrBackground = brush;
            assert_ne!(RegisterClassW(&wc), 0, "RegisterClassW");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP | WS_VISIBLE,
                100,
                100,
                SIDE,
                SIDE,
                null_mut(),
                null_mut(),
                instance,
                std::ptr::null(),
            );
            assert!(!hwnd.is_null(), "CreateWindowExW");
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
            UpdateWindow(hwnd);
            hwnd
        };
        pump(Duration::from_millis(1500));

        // SAFETY: (0, 0) is always on the primary monitor.
        let primary = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let grabbed = grab(&(primary as usize).to_string());
        // SAFETY: created above on this thread.
        unsafe { DestroyWindow(hwnd) };
        let image = grabbed.expect("the real GDI grab succeeds");
        assert!(image.width >= SIDE as u32 && image.height >= SIDE as u32);

        let pixels = image.pixels.as_chunks::<4>().0.iter();
        let non_black = pixels.clone().filter(|p| p[..3] != [0, 0, 0]).count();
        let magenta = pixels
            .filter(|p| p[0] == BLUE && p[1] == GREEN && p[2] == RED)
            .count();
        eprintln!(
            "bitblt: {}x{}, {non_black} non-black, {magenta} known-colour pixels",
            image.width, image.height
        );
        assert!(non_black > 0, "the GDI grab is all black");
        // Allow for a border or shadow: most of the window's area must match.
        let area = (SIDE * SIDE) as usize;
        assert!(
            magenta >= area / 2,
            "only {magenta} of {area} window pixels have the known colour"
        );
    }
}
