//! Visual Proof Mode (Step 3b). Everything that touches pixels runs in the
//! short-lived `traction-daemon capture-frame` child; the daemon only
//! schedules it.
//!
//! This first commit only proves the native crates build and work on stock
//! runners (spec addition 3); the pipeline that uses them follows.
#![allow(dead_code)]

pub mod grab;

#[cfg(test)]
mod tests {
    /// Addition 3 smoke: libwebp (bundled, built by `cc`) encodes lossy,
    /// XChaCha20-Poly1305 round-trips, HMAC-SHA256 computes.
    #[test]
    fn webp_aead_and_hmac_build_and_run_here() {
        use chacha20poly1305::aead::{Aead, KeyInit, Payload};
        use chacha20poly1305::{XChaCha20Poly1305, XNonce};
        use hmac::{Hmac, Mac};

        let rgb = vec![128u8; 64 * 48 * 3];
        let webp = webp::Encoder::from_rgb(&rgb, 64, 48).encode(70.0);
        assert_eq!((&webp[..4], &webp[8..12]), (&b"RIFF"[..], &b"WEBP"[..]));

        let cipher = XChaCha20Poly1305::new_from_slice(&[1u8; 32]).unwrap();
        let nonce = XNonce::from([2u8; 24]);
        let sealed = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &webp,
                    aad: b"id",
                },
            )
            .unwrap();
        let opened = cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: &sealed,
                    aad: b"id",
                },
            )
            .unwrap();
        assert_eq!(opened, &webp[..]);
        assert!(
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &sealed,
                        aad: b"other"
                    }
                )
                .is_err()
        );

        let mut mac = <Hmac<sha2::Sha256> as hmac::KeyInit>::new_from_slice(&[0u8; 32]).unwrap();
        mac.update(&0u64.to_be_bytes());
        assert_eq!(mac.finalize().into_bytes().len(), 32);
    }
}
