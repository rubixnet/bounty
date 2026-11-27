//! In-memory WebP → 8-bit baseline JPEG for Jev frames (spec S4).
//!
//! TypeSafe accepts PNG or JPEG only, recommends a standard 8-bit
//! **baseline** JPEG, and rejects (never resizes) an image with either side
//! outside 280–1568 px. So a sampled WebP frame is decoded, flattened onto
//! white if it has alpha, downscaled (area average) until its longest side is
//! at most [`MAX_SIDE`], and encoded as a baseline (SOF0) JPEG. A frame with a
//! side under [`MIN_SIDE`], before or after downscaling, is **skipped, never
//! upscaled** ([`FrameSkip::TooSmall`], `frame_too_small_skipped`). Nothing
//! touches the disk.
//!
//! Pure Rust on all three OSes: `image-webp` decodes, `jpeg-encoder` encodes
//! (its progressive mode is never turned on).

use std::io::Cursor;

use image_webp::WebPDecoder;
use jpeg_encoder::{ColorType, Encoder};

/// TypeSafe's largest accepted side.
pub const MAX_SIDE: u32 = 1568;
/// TypeSafe's smallest accepted side.
pub const MIN_SIDE: u32 = 280;
/// Frames above this many pixels are refused before decoding (an 8K frame
/// is ~33 MP), so a hostile header cannot make the server allocate gigabytes.
pub const MAX_SOURCE_PIXELS: u64 = 40_000_000;
/// JPEG quality: a reviewer hint, not an archive.
pub const JPEG_QUALITY: u8 = 85;

/// Why a frame is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSkip {
    /// Either side is under [`MIN_SIDE`], before or after downscaling.
    TooSmall,
    /// Not a decodable still WebP (damaged, animated or oversized).
    Undecodable,
}

impl FrameSkip {
    /// The reason code recorded with the decision.
    pub fn code(self) -> &'static str {
        match self {
            FrameSkip::TooSmall => "frame_too_small_skipped",
            FrameSkip::Undecodable => "frame_undecodable",
        }
    }
}

/// A transcoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jpeg {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The size a `width` x `height` frame is sent at, or why it is skipped.
pub fn target_size(width: u32, height: u32) -> Result<(u32, u32), FrameSkip> {
    if width.min(height) < MIN_SIDE {
        return Err(FrameSkip::TooSmall);
    }
    let longest = width.max(height);
    let (w, h) = if longest <= MAX_SIDE {
        (width, height)
    } else {
        let scale = |side: u32| {
            ((u64::from(side) * u64::from(MAX_SIDE) + u64::from(longest) / 2) / u64::from(longest))
                as u32
        };
        (scale(width).min(MAX_SIDE), scale(height).min(MAX_SIDE))
    };
    if w.min(h) < MIN_SIDE {
        return Err(FrameSkip::TooSmall);
    }
    Ok((w, h))
}

/// Reads only a frame's header: the size it would be sent at, or why it
/// would be skipped. Cheap enough to run on every frame of a bundle.
pub fn probe(webp: &[u8]) -> Result<(u32, u32), FrameSkip> {
    let decoder = WebPDecoder::new(Cursor::new(webp)).map_err(|_| FrameSkip::Undecodable)?;
    let (width, height) = decoder.dimensions();
    if decoder.is_animated() || u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(FrameSkip::Undecodable);
    }
    target_size(width, height)
}

/// Transcodes one WebP frame, in memory.
pub fn webp_to_baseline_jpeg(webp: &[u8]) -> Result<Jpeg, FrameSkip> {
    let mut decoder = WebPDecoder::new(Cursor::new(webp)).map_err(|_| FrameSkip::Undecodable)?;
    let (width, height) = decoder.dimensions();
    if decoder.is_animated() || u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(FrameSkip::Undecodable);
    }
    let (out_w, out_h) = target_size(width, height)?;
    let channels = if decoder.has_alpha() { 4 } else { 3 };
    let size = decoder.output_buffer_size().ok_or(FrameSkip::Undecodable)?;
    let mut pixels = vec![0u8; size];
    decoder
        .read_image(&mut pixels)
        .map_err(|_| FrameSkip::Undecodable)?;
    let rgb = if channels == 4 {
        flatten_on_white(&pixels)
    } else {
        pixels
    };
    let rgb = downscale(&rgb, width, height, out_w, out_h);

    let mut bytes = Vec::new();
    let encoder = Encoder::new(&mut bytes, JPEG_QUALITY);
    let (w16, h16) = (
        u16::try_from(out_w).map_err(|_| FrameSkip::Undecodable)?,
        u16::try_from(out_h).map_err(|_| FrameSkip::Undecodable)?,
    );
    encoder
        .encode(&rgb, w16, h16, ColorType::Rgb)
        .map_err(|_| FrameSkip::Undecodable)?;
    Ok(Jpeg {
        bytes,
        width: out_w,
        height: out_h,
    })
}

fn flatten_on_white(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        let a = u32::from(px[3]);
        for &c in &px[..3] {
            rgb.push(((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8);
        }
    }
    rgb
}

/// Area-average downscale of packed RGB (`out` no larger than `src`).
fn downscale(rgb: &[u8], w: u32, h: u32, out_w: u32, out_h: u32) -> Vec<u8> {
    if (w, h) == (out_w, out_h) {
        return rgb.to_vec();
    }
    let (w, h, out_w, out_h) = (w as usize, h as usize, out_w as usize, out_h as usize);
    let span = |i: usize, out: usize, src: usize| {
        let start = i * src / out;
        let end = ((i + 1) * src / out).max(start + 1).min(src);
        start..end
    };
    let mut out = Vec::with_capacity(out_w * out_h * 3);
    for oy in 0..out_h {
        let ys = span(oy, out_h, h);
        for ox in 0..out_w {
            let xs = span(ox, out_w, w);
            let mut sum = [0u64; 3];
            let mut n = 0u64;
            for y in ys.clone() {
                let row = y * w * 3;
                for x in xs.clone() {
                    let p = row + x * 3;
                    for c in 0..3 {
                        sum[c] += u64::from(rgb[p + c]);
                    }
                    n += 1;
                }
            }
            for s in sum {
                out.push(((s + n / 2) / n) as u8);
            }
        }
    }
    out
}
