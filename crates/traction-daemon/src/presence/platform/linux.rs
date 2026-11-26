//! Linux backends.
//!
//! X11 (tier `full`): `_NET_ACTIVE_WINDOW` -> `WM_CLASS` for the foreground
//! category, the XScreenSaver extension for idle, and XInput2 *raw* events
//! for input counts. An event whose source device is an XTEST slave
//! (`Virtual core XTEST ...`) is injected. Only the event type and
//! `sourceid` are read: never `detail` (the key code or button), pointer
//! values or any other field. No raw device files, no `input` group.
//!
//! Wayland, or no display at all: the compositor hides the focused window
//! and global input. The `ext-idle-notify-v1` and
//! `org.freedesktop.ScreenSaver` idle sources are not built yet, so these
//! sessions report tier `none` (see `docs/protocol.md` section 2b).
//!
//! Any X11 error lowers the tier and logs once. Nothing here panics.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;

use traction_core::{AppCategory, TelemetryTier};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::screensaver;
use x11rb::protocol::xinput::{self, XIEventMask};
use x11rb::protocol::xproto::{self, AtomEnum};
use x11rb::rust_connection::RustConnection;

use super::super::aggregate::{InputKind, Sample};
use super::super::category;
use super::super::{Backend, Msg};
use super::Unavailable;
use crate::clock::now_ms;
use crate::log;

/// `XIAllMasterDevices`: raw events are delivered per master device and
/// carry the slave they came from in `sourceid`.
const ALL_MASTER_DEVICES: u16 = 1;
/// `XIAllDevices`.
const ALL_DEVICES: u16 = 0;

/// Which backend a session gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    X11,
    /// Wayland (a compositor, possibly with XWayland) or no display.
    NoX11,
}

/// Pure choice from the environment. A Wayland compositor wins over an
/// XWayland `DISPLAY`: XWayland only sees X clients' windows and input.
pub fn choose(display: Option<&OsStr>, wayland_display: Option<&OsStr>) -> Session {
    let set = |v: Option<&OsStr>| v.is_some_and(|v| !v.is_empty());
    if set(wayland_display) || !set(display) {
        Session::NoX11
    } else {
        Session::X11
    }
}

pub fn detect() -> Box<dyn Backend> {
    detect_with(
        std::env::var_os("DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
    )
}

pub fn detect_with(display: Option<&OsStr>, wayland_display: Option<&OsStr>) -> Box<dyn Backend> {
    match choose(display, wayland_display) {
        Session::NoX11 => Box::new(Unavailable),
        Session::X11 => match display
            .and_then(OsStr::to_str)
            .ok_or(())
            .and_then(X11Backend::connect)
        {
            Ok(backend) => Box::new(backend),
            Err(()) => {
                log::warn("presence: cannot use the X11 display, presence is off");
                Box::new(Unavailable)
            }
        },
    }
}

struct X11Backend {
    /// The display both connections open.
    display: String,
    conn: RustConnection,
    root: xproto::Window,
    active_window: xproto::Atom,
    idle_ok: bool,
    category_ok: bool,
    input_ok: Arc<AtomicBool>,
    logged: bool,
}

impl X11Backend {
    fn connect(display: &str) -> Result<Self, ()> {
        let (conn, screen) = RustConnection::connect(Some(display)).map_err(|_| ())?;
        let root = conn.setup().roots.get(screen).ok_or(())?.root;
        // Created if missing: the daemon may start before the window manager,
        // and until one sets the property there is just no active window.
        let active_window =
            x11rb::protocol::xproto::intern_atom(&conn, false, b"_NET_ACTIVE_WINDOW")
                .map_err(|_| ())?
                .reply()
                .map_err(|_| ())?
                .atom;
        let idle_ok = screensaver::query_version(&conn, 1, 1)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        Ok(Self {
            display: display.to_owned(),
            conn,
            root,
            active_window,
            idle_ok,
            category_ok: active_window != 0,
            input_ok: Arc::new(AtomicBool::new(true)),
            logged: false,
        })
    }

    fn log_once(&mut self, message: &'static str) {
        if !self.logged {
            self.logged = true;
            log::warn(message);
        }
    }

    fn idle_ms(&mut self) -> Option<u64> {
        if !self.idle_ok {
            return None;
        }
        let info = screensaver::query_info(&self.conn, self.root)
            .ok()
            .and_then(|c| c.reply().ok());
        match info {
            Some(info) => Some(u64::from(info.ms_since_user_input)),
            None => {
                self.idle_ok = false;
                self.log_once("presence: X11 idle query failed, tier lowered");
                None
            }
        }
    }

    fn foreground_category(&mut self) -> Option<AppCategory> {
        if !self.category_ok {
            return None;
        }
        match self.read_category() {
            Ok(category) => category,
            Err(()) => {
                self.category_ok = false;
                self.log_once("presence: X11 foreground query failed, tier lowered");
                None
            }
        }
    }

    /// `Ok(None)` when no window is active (not an error).
    fn read_category(&self) -> Result<Option<AppCategory>, ()> {
        let active = xproto::get_property(
            &self.conn,
            false,
            self.root,
            self.active_window,
            AtomEnum::WINDOW,
            0,
            1,
        )
        .map_err(|_| ())?
        .reply()
        .map_err(|_| ())?;
        let Some(window) = active
            .value32()
            .and_then(|mut v| v.next())
            .filter(|w| *w != 0)
        else {
            return Ok(Some(AppCategory::Other));
        };
        let class = xproto::get_property(
            &self.conn,
            false,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            256,
        )
        .map_err(|_| ())?
        .reply();
        let Ok(class) = class else {
            // The window vanished between the two requests.
            return Ok(Some(AppCategory::Other));
        };
        // "instance\0class\0": matched here, then dropped.
        let mut parts = class.value.split(|b| *b == 0);
        let instance = String::from_utf8_lossy(parts.next().unwrap_or_default());
        let class = String::from_utf8_lossy(parts.next().unwrap_or_default());
        Ok(Some(category::x11_wm_class(&instance, &class)))
    }
}

impl Backend for X11Backend {
    fn tier(&self) -> TelemetryTier {
        match (
            self.idle_ok,
            self.category_ok,
            self.input_ok.load(Ordering::Relaxed),
        ) {
            (false, _, _) => TelemetryTier::None,
            (true, false, _) => TelemetryTier::IdleOnly,
            (true, true, false) => TelemetryTier::Reduced,
            (true, true, true) => TelemetryTier::Full,
        }
    }

    fn sample(&mut self) -> Sample {
        Sample {
            idle_ms: self.idle_ms(),
            category: self.foreground_category(),
        }
    }

    fn start_input(&mut self, tx: Sender<Msg>) {
        let input_ok = Arc::clone(&self.input_ok);
        let display = self.display.clone();
        // The thread blocks in `wait_for_event` (an OS notification: no
        // polling) and ends when the sensor is gone (a failed send) or the
        // connection closes.
        let spawned = thread::Builder::new()
            .name("traction-presence-input".to_owned())
            .spawn({
                let input_ok = Arc::clone(&input_ok);
                move || {
                    if run_input(&display, &tx).is_err() {
                        input_ok.store(false, Ordering::Relaxed);
                    }
                }
            });
        if spawned.is_err() {
            input_ok.store(false, Ordering::Relaxed);
        }
    }
}

/// Ids of the XTEST slave devices (names start `Virtual core XTEST`).
fn xtest_devices(conn: &RustConnection) -> Result<HashSet<u16>, ()> {
    let reply = xinput::xi_query_device(conn, ALL_DEVICES)
        .map_err(|_| ())?
        .reply()
        .map_err(|_| ())?;
    Ok(reply
        .infos
        .iter()
        .filter(|info| is_xtest_name(&info.name))
        .map(|info| info.deviceid)
        .collect())
}

fn is_xtest_name(name: &[u8]) -> bool {
    name.windows(5).any(|w| w.eq_ignore_ascii_case(b"XTEST"))
}

/// Selects raw key/button/motion events on the root window and forwards
/// one `Msg::Input` per event (kind and injected flag only).
fn run_input(display: &str, tx: &Sender<Msg>) -> Result<(), ()> {
    let (conn, screen) = RustConnection::connect(Some(display)).map_err(|_| ())?;
    let root = conn.setup().roots.get(screen).ok_or(())?.root;
    xinput::xi_query_version(&conn, 2, 0)
        .map_err(|_| ())?
        .reply()
        .map_err(|_| ())?;
    let xtest = xtest_devices(&conn)?;
    let mask = XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_BUTTON_PRESS | XIEventMask::RAW_MOTION;
    xinput::xi_select_events(
        &conn,
        root,
        &[xinput::EventMask {
            deviceid: ALL_MASTER_DEVICES,
            mask: vec![mask],
        }],
    )
    .map_err(|_| ())?
    .check()
    .map_err(|_| ())?;
    loop {
        let event = conn.wait_for_event().map_err(|_| ())?;
        let (kind, source) = match event {
            Event::XinputRawKeyPress(e) => (InputKind::Key, e.sourceid),
            Event::XinputRawButtonPress(e) => (InputKind::Pointer, e.sourceid),
            Event::XinputRawMotion(e) => (InputKind::Pointer, e.sourceid),
            _ => continue,
        };
        let sent = tx.send(Msg::Input {
            t_ms: now_ms(),
            kind,
            injected: xtest.contains(&source),
        });
        if sent.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn the_session_is_chosen_from_the_environment() {
        let x = OsString::from(":0");
        let w = OsString::from("wayland-0");
        let empty = OsString::new();
        assert_eq!(choose(Some(&x), None), Session::X11);
        assert_eq!(choose(Some(&x), Some(&w)), Session::NoX11);
        assert_eq!(choose(None, Some(&w)), Session::NoX11);
        assert_eq!(choose(None, None), Session::NoX11);
        assert_eq!(choose(Some(&empty), None), Session::NoX11);
        assert_eq!(choose(Some(&x), Some(&empty)), Session::X11);
    }

    #[test]
    fn xtest_slave_devices_are_recognised_by_name() {
        assert!(is_xtest_name(b"Virtual core XTEST pointer"));
        assert!(is_xtest_name(b"Virtual core XTEST keyboard"));
        assert!(!is_xtest_name(b"Virtual core pointer"));
        assert!(!is_xtest_name(b"AT Translated Set 2 keyboard"));
        assert!(!is_xtest_name(b""));
    }

    #[test]
    fn no_display_and_no_wayland_is_tier_none_and_never_panics() {
        let mut backend = detect_with(None, None);
        assert_eq!(backend.tier(), TelemetryTier::None);
        assert_eq!(backend.sample(), Sample::default());
    }

    #[test]
    fn a_wayland_session_is_tier_none() {
        let w = OsString::from("wayland-0");
        let x = OsString::from(":0");
        let backend = detect_with(Some(&x), Some(&w));
        assert_eq!(backend.tier(), TelemetryTier::None);
    }

    #[test]
    fn an_unreachable_display_lowers_to_none_instead_of_failing() {
        let bogus = OsString::from(":9999");
        let backend = detect_with(Some(&bogus), None);
        assert_eq!(backend.tier(), TelemetryTier::None);
    }
}
