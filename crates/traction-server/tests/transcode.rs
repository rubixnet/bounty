//! WebP → 8-bit baseline JPEG for Jev frames (spec S4), over real WebP
//! fixtures made with `cwebp` (`tests/data/frames/`). The JPEG checks parse
//! the markers independently of the encoder.

use traction_server::transcode::{FrameSkip, MAX_SIDE, MIN_SIDE, webp_to_baseline_jpeg};

const LARGE: &[u8] = include_bytes!("data/frames/large_2400x1500.webp");
const NORMAL: &[u8] = include_bytes!("data/frames/normal_1280x800.webp");
const TINY: &[u8] = include_bytes!("data/frames/tiny_200x150.webp");
const STRIP: &[u8] = include_bytes!("data/frames/strip_3000x300.webp");
const ALPHA: &[u8] = include_bytes!("data/frames/alpha_lossless_600x400.webp");

/// The start-of-frame markers found before the first scan:
/// `(marker, precision, height, width, components)`.
fn frame_headers(jpeg: &[u8]) -> Vec<(u8, u8, u16, u16, u8)> {
    assert_eq!(&jpeg[..2], [0xFF, 0xD8], "SOI");
    assert_eq!(&jpeg[jpeg.len() - 2..], [0xFF, 0xD9], "EOI");
    let mut found = Vec::new();
    let mut at = 2;
    while at + 4 <= jpeg.len() {
        assert_eq!(jpeg[at], 0xFF, "marker at {at}");
        let marker = jpeg[at + 1];
        let len = usize::from(u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]));
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let seg = &jpeg[at + 4..];
            found.push((
                marker,
                seg[0],
                u16::from_be_bytes([seg[1], seg[2]]),
                u16::from_be_bytes([seg[3], seg[4]]),
                seg[5],
            ));
        }
        if marker == 0xDA {
            break;
        }
        at += 2 + len;
    }
    found
}

fn assert_baseline(jpeg: &[u8], width: u32, height: u32) {
    let headers = frame_headers(jpeg);
    assert_eq!(
        headers,
        [(0xC0, 8, height as u16, width as u16, 3)],
        "exactly one SOF0 (baseline), 8-bit, 3 components"
    );
}

#[test]
fn a_large_frame_is_downscaled_to_a_1568_px_longest_side_baseline_jpeg() {
    let jpeg = webp_to_baseline_jpeg(LARGE).unwrap();
    assert_eq!((jpeg.width, jpeg.height), (1568, 980));
    assert_baseline(&jpeg.bytes, 1568, 980);
}

#[test]
fn a_frame_inside_the_limits_keeps_its_size() {
    let jpeg = webp_to_baseline_jpeg(NORMAL).unwrap();
    assert_eq!((jpeg.width, jpeg.height), (1280, 800));
    assert_baseline(&jpeg.bytes, 1280, 800);
}

#[test]
fn a_lossless_frame_with_alpha_becomes_an_opaque_baseline_jpeg() {
    let jpeg = webp_to_baseline_jpeg(ALPHA).unwrap();
    assert_eq!((jpeg.width, jpeg.height), (600, 400));
    assert_baseline(&jpeg.bytes, 600, 400);
}

#[test]
fn a_frame_under_280_px_is_skipped_never_upscaled() {
    assert_eq!(MIN_SIDE, 280);
    assert_eq!(MAX_SIDE, 1568);
    let skip = webp_to_baseline_jpeg(TINY).unwrap_err();
    assert_eq!(skip, FrameSkip::TooSmall);
    assert_eq!(skip.code(), "frame_too_small_skipped");
}

#[test]
fn a_frame_that_downscaling_would_take_under_280_px_is_skipped() {
    // 3000 x 300 fits 1568 x 157: under TypeSafe's minimum after scaling.
    assert_eq!(
        webp_to_baseline_jpeg(STRIP).unwrap_err(),
        FrameSkip::TooSmall
    );
}

#[test]
fn bytes_that_are_not_webp_are_skipped_as_undecodable() {
    let skip = webp_to_baseline_jpeg(b"RIFF\0\0\0\0WEBPjunk").unwrap_err();
    assert_eq!(skip, FrameSkip::Undecodable);
    assert_eq!(skip.code(), "frame_undecodable");
}
