//! X11 `GetImage` of the root window of the default screen. Wayland (no
//! `DISPLAY`) is `unsupported`; the portal is never tried.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use super::{Bgra, Focus, GrabError};
use crate::presence::platform::linux::{Session, choose};

fn connect() -> Result<(RustConnection, usize), GrabError> {
    let display = std::env::var_os("DISPLAY");
    let wayland = std::env::var_os("WAYLAND_DISPLAY");
    match choose(display.as_deref(), wayland.as_deref()) {
        Session::X11 => RustConnection::connect(None).map_err(|_| GrabError::Error),
        Session::NoX11 => Err(GrabError::Unsupported),
    }
}

pub fn focus() -> Option<Focus> {
    let (conn, screen) = connect().ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let atom = xproto::intern_atom(&conn, true, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    // Without a window manager there is no active window property: the
    // focus is then the X input focus.
    let window = if atom == 0 {
        None
    } else {
        xproto::get_property(&conn, false, root, atom, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()
            .and_then(|mut v| v.next())
            .filter(|w| *w != 0)
    };
    let window = match window {
        Some(window) => window,
        None => conn.get_input_focus().ok()?.reply().ok()?.focus,
    };
    Some(Focus {
        token: format!("x11:{window:x}"),
        display: screen.to_string(),
    })
}

pub fn grab(display: &str) -> Result<Bgra, GrabError> {
    let (conn, default_screen) = connect()?;
    let screen_index = display.parse::<usize>().map_err(|_| GrabError::Error)?;
    if screen_index != default_screen {
        return Err(GrabError::Error);
    }
    let setup = conn.setup();
    let screen = setup.roots.get(screen_index).ok_or(GrabError::Error)?;
    let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);
    let bpp = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .map(|f| f.bits_per_pixel);
    // Only the 24/32-bit little-endian TrueColor layout (BGRX) is read.
    if bpp != Some(32) || setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(GrabError::Unsupported);
    }
    let reply = conn
        .get_image(ImageFormat::Z_PIXMAP, screen.root, 0, 0, width, height, !0)
        .map_err(|_| GrabError::Error)?
        .reply()
        .map_err(|_| GrabError::Error)?;
    let expected = usize::from(width) * usize::from(height) * 4;
    if reply.data.len() != expected {
        return Err(GrabError::Error);
    }
    Ok(Bgra {
        width: u32::from(width),
        height: u32::from(height),
        pixels: reply.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `GetImage` under Xvfb (CI runs the suite under `xvfb-run`).
    #[test]
    fn the_real_x11_grab_reads_the_whole_screen() {
        if std::env::var_os("DISPLAY").is_none() {
            assert!(std::env::var_os("CI").is_none(), "CI runs under Xvfb");
            eprintln!("SKIPPED (no X display)");
            return;
        }
        let focus = focus().expect("an X display has a focus");
        let image = grab(&focus.display).expect("the real X11 grab succeeds");
        assert!(image.width > 0 && image.height > 0);
        assert_eq!(
            image.pixels.len(),
            image.width as usize * image.height as usize * 4
        );
        assert!(
            matches!(grab("7"), Err(GrabError::Error)),
            "an unknown screen"
        );
    }
}
