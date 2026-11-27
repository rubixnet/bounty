//! The child's pixel pipeline: downscale to the 720p target height (never
//! upscale) and the 64-bit dHash used to skip duplicate screens.

use super::grab::Bgra;

/// Frames are stored at this height (or their own, when smaller).
pub const TARGET_HEIGHT: u32 = 720;

/// One RGB image, 3 bytes per pixel, top-down rows with no padding.
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// The output size for a `width`×`height` grab.
pub fn target_size(width: u32, height: u32) -> (u32, u32) {
    if height <= TARGET_HEIGHT || height == 0 {
        return (width, height);
    }
    let scaled =
        (u64::from(width) * u64::from(TARGET_HEIGHT) + u64::from(height) / 2) / u64::from(height);
    (
        u32::try_from(scaled).unwrap_or(u32::MAX).max(1),
        TARGET_HEIGHT,
    )
}

/// Area-averaged downscale of a BGRA grab to RGB at [`target_size`].
pub fn downscale(image: &Bgra) -> Rgb {
    let (width, height) = target_size(image.width, image.height);
    let pixels = box_resize(
        &image.pixels,
        4,
        [2, 1, 0],
        image.width,
        image.height,
        width,
        height,
    );
    Rgb {
        width,
        height,
        pixels,
    }
}

/// Resizes `channels`-byte pixels (picking `order` as R, G, B) to RGB by
/// averaging each output pixel's source box.
fn box_resize(
    src: &[u8],
    channels: usize,
    order: [usize; 3],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let span = |i: usize, s: usize, d: usize| {
        let a = i * s / d;
        let b = ((i + 1) * s / d).max(a + 1).min(s);
        (a, b)
    };
    let mut out = Vec::with_capacity(dw * dh * 3);
    for y in 0..dh {
        let (y0, y1) = span(y, sh, dh);
        for x in 0..dw {
            let (x0, x1) = span(x, sw, dw);
            let mut sum = [0u64; 3];
            for row in y0..y1 {
                let base = (row * sw + x0) * channels;
                for px in src[base..base + (x1 - x0) * channels].chunks_exact(channels) {
                    for (c, &o) in order.iter().enumerate() {
                        sum[c] += u64::from(px[o]);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            out.extend(sum.iter().map(|s| ((s + n / 2) / n) as u8));
        }
    }
    out
}

/// 64-bit difference hash: the image reduced to 9×8 grayscale, one bit per
/// horizontally adjacent pair (`left < right`), rows top to bottom.
pub fn dhash(image: &Rgb) -> u64 {
    let small = box_resize(&image.pixels, 3, [0, 1, 2], image.width, image.height, 9, 8);
    let gray: Vec<u32> = small
        .as_chunks::<3>()
        .0
        .iter()
        .map(|p| (299 * u32::from(p[0]) + 587 * u32::from(p[1]) + 114 * u32::from(p[2])) / 1000)
        .collect();
    let mut hash = 0u64;
    for row in 0..8 {
        for col in 0..8 {
            hash <<= 1;
            if gray[row * 9 + col] < gray[row * 9 + col + 1] {
                hash |= 1;
            }
        }
    }
    hash
}

/// Bits that differ between two dHashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Bgra {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let [r, g, b] = f(x, y);
                pixels.extend_from_slice(&[b, g, r, 0xFF]);
            }
        }
        Bgra {
            width,
            height,
            pixels,
        }
    }

    #[test]
    fn downscales_to_720p_and_never_upscales() {
        assert_eq!(target_size(2560, 1440), (1280, 720));
        assert_eq!(target_size(3024, 1964), (1109, 720));
        assert_eq!(target_size(1280, 720), (1280, 720));
        assert_eq!(target_size(800, 600), (800, 600));
        let image = downscale(&bgra(1920, 1080, |_, _| [10, 20, 30]));
        assert_eq!((image.width, image.height), (1280, 720));
        assert_eq!(&image.pixels[..3], &[10, 20, 30], "BGRA becomes RGB");
        assert_eq!(image.pixels.len(), 1280 * 720 * 3);
    }

    #[test]
    fn dhash_tells_screens_apart_and_ignores_tiny_noise() {
        let stripes = downscale(&bgra(640, 480, |x, _| {
            if x / 40 % 2 == 0 { [0; 3] } else { [255; 3] }
        }));
        let gradient = downscale(&bgra(640, 480, |x, _| [(x / 3) as u8; 3]));
        let noisy = downscale(&bgra(640, 480, |x, y| {
            [((x / 3) as u8).saturating_add((x * y % 2) as u8); 3]
        }));
        assert!(hamming(dhash(&stripes), dhash(&gradient)) >= 3);
        assert!(hamming(dhash(&gradient), dhash(&noisy)) < 3);
        assert_eq!(hamming(0, u64::MAX), 64);
    }
}
