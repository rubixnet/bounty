//! The Device Signing Key (private, never printed) and the public keys a
//! verifier accepts.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use super::{
    ChainError, ChainLink, KeyId, LinkHeader, LinkPayload, SealError, TAG_KEY_ID, TAG_LINK,
    TAG_PAYLOAD, TAG_SIG_LINK, canon, hex, tagged_digest, types,
};

/// A public key that parsed as a curve point and is not of small order.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifyingKeyHex {
    key: VerifyingKey,
}

impl std::fmt::Debug for VerifyingKeyHex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("VerifyingKeyHex")
            .field(&self.to_hex())
            .finish()
    }
}

impl VerifyingKeyHex {
    /// Parse 32 bytes of lowercase hex. Malformed, off-curve and weak
    /// (small-order) keys are all [`ChainError::WeakKey`].
    pub fn parse(text: &str) -> Result<Self, ChainError> {
        let bytes = hex::decode::<32>(text).ok_or(ChainError::WeakKey)?;
        Self::from_bytes(&bytes)
    }

    /// Wrap 32 public key bytes, rejecting weak keys.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, ChainError> {
        let key = VerifyingKey::from_bytes(bytes).map_err(|_| ChainError::WeakKey)?;
        if key.is_weak() {
            return Err(ChainError::WeakKey);
        }
        Ok(Self { key })
    }

    /// The 32 public key bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    /// Lowercase hex of the public key.
    pub fn to_hex(&self) -> String {
        hex::encode(&self.to_bytes())
    }

    /// The Key ID derived from this key.
    pub fn key_id(&self) -> KeyId {
        let digest = tagged_digest(TAG_KEY_ID, &self.to_bytes());
        KeyId::from_string(format!("ed25519:{}", hex::encode(&digest[..16])))
    }

    /// Verify `signature` over `tag || message` with `verify_strict` only.
    pub(crate) fn verify_tagged(&self, tag: &[u8], message: &[u8; 32], signature: &str) -> bool {
        let Some(sig_bytes) = hex::decode::<64>(signature) else {
            return false;
        };
        let signature = Signature::from_bytes(&sig_bytes);
        self.key
            .verify_strict(&tagged_input(tag, message), &signature)
            .is_ok()
    }
}

/// The exact bytes both signing and verifying cover: `tag || message`.
fn tagged_input(tag: &[u8], message: &[u8; 32]) -> Vec<u8> {
    let mut input = Vec::with_capacity(tag.len() + message.len());
    input.extend_from_slice(tag);
    input.extend_from_slice(message);
    input
}

impl Serialize for VerifyingKeyHex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for VerifyingKeyHex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The per-device Ed25519 signing key. Built only from a zeroizing seed; it
/// is deliberately not `Debug`, `Display`, `Clone` or `Serialize`.
pub struct DeviceSigningKey {
    key: SigningKey,
    public: VerifyingKeyHex,
}

impl DeviceSigningKey {
    /// Build from a 32-byte seed. A seed whose public key is weak is
    /// rejected (it cannot be a legitimate key).
    pub fn from_seed(seed: Zeroizing<[u8; 32]>) -> Result<Self, ChainError> {
        let key = SigningKey::from_bytes(&seed);
        let public = VerifyingKeyHex::from_bytes(&key.verifying_key().to_bytes())?;
        Ok(Self { key, public })
    }

    /// The matching public key.
    pub fn verifying_key(&self) -> &VerifyingKeyHex {
        &self.public
    }

    /// The Key ID of the matching public key.
    pub fn key_id(&self) -> KeyId {
        self.public.key_id()
    }

    /// Sign `tag || message`; returns the 64-byte signature as lowercase hex.
    pub(crate) fn sign_tagged(&self, tag: &[u8], message: &[u8; 32]) -> String {
        hex::encode(&self.key.sign(&tagged_input(tag, message)).to_bytes())
    }

    /// Seal a payload into the next Chain Link after `prev` (`None` for a
    /// genesis link). `seq`, `prev_hash`, `key_id` and `clock_regressed` are
    /// derived here, so a caller cannot build an unlinked header.
    pub fn seal(
        &self,
        prev: Option<&LinkHeader>,
        payload: LinkPayload,
        sealed_at_ms: u64,
    ) -> Result<ChainLink, SealError> {
        let key_id = self.key_id();
        let (seq, prev_hash, clock_regressed) = match prev {
            None => (0, types::GENESIS_PREV_HASH.to_owned(), false),
            Some(prev) => {
                if prev.key_id != key_id {
                    return Err(SealError::ForeignPrev);
                }
                if payload.window_start_ms <= prev.window_start_ms {
                    return Err(SealError::WindowOrder);
                }
                if payload.supersedes.is_some() {
                    return Err(SealError::SupersedesOnNonGenesis);
                }
                let seq = prev.seq.checked_add(1).ok_or(SealError::SeqOverflow)?;
                // The previous link's own `entry_hash` is what `prev_hash`
                // names; recompute it from the header it was derived from.
                let prev_hash = super::digest_hex(TAG_LINK, prev);
                (seq, prev_hash, sealed_at_ms < prev.sealed_at_ms)
            }
        };
        let header = LinkHeader {
            schema_version: 1,
            seq,
            prev_hash,
            payload_digest: super::digest_hex(TAG_PAYLOAD, &payload),
            key_id,
            ruleset_id: payload.ruleset_id.clone(),
            window_start_ms: payload.window_start_ms,
            sealed_at_ms,
            clock_regressed,
        };
        let entry = tagged_digest(TAG_LINK, &canon(&header));
        let signature = self.sign_tagged(TAG_SIG_LINK, &entry);
        Ok(ChainLink {
            header,
            payload,
            entry_hash: hex::encode(&entry),
            signature,
        })
    }
}
