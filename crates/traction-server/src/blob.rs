//! `BlobStore` (spec S3a, Q15): where Audit Bundles are stored between
//! `POST /v1/bundles` and `complete`.
//!
//! - [`R2BlobStore`]: sans-IO AWS Signature V4 query presigning ([`sigv4`]).
//!   Works against Cloudflare R2 and any S3-compatible store (the CI test
//!   uses MinIO). The server itself only reads and deletes through presigned
//!   URLs it signs for itself.
//! - [`LocalBlobStore`]: the server serves `PUT /v1/blobs/{key}` itself, with
//!   URLs signed by an HMAC-SHA-256 key, so a self-hoster needs no R2.
//!
//! Both presign with the **exact** `Content-Length`, a 15-minute expiry
//! (`[blob] presign_expiry_sec`) and a 64 MB cap (`[blob] max_bytes`). Keys
//! and secrets come from environment variables or files only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::Blob;
use crate::decision::BoxFuture;
use crate::oidc::url_encode;
use crate::token::{hex_encode, secure_eq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobError {
    /// More than `[blob] max_bytes` (or nothing at all).
    BadLength,
    /// The key is not a plain `[A-Za-z0-9._-/]` path.
    BadKey,
    /// The presigned URL's signature does not match.
    BadSignature,
    /// The presigned URL has expired.
    Expired,
    /// The stored or uploaded bytes are not the signed length.
    LengthMismatch,
    Io(String),
}

impl BlobError {
    pub fn code(&self) -> &'static str {
        match self {
            BlobError::BadLength => "bad_length",
            BlobError::BadKey => "bad_key",
            BlobError::BadSignature => "bad_signature",
            BlobError::Expired => "expired",
            BlobError::LengthMismatch => "length_mismatch",
            BlobError::Io(_) => "blob_io",
        }
    }
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::Io(e) => write!(f, "blob_io: {e}"),
            other => f.write_str(other.code()),
        }
    }
}

impl std::error::Error for BlobError {}

/// A presigned upload target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutTarget {
    pub url: String,
    pub expires_at_ms: u64,
}

pub trait BlobStore: Send + Sync {
    /// A URL that accepts exactly one `PUT` of exactly `content_length`
    /// bytes until it expires. `content_length` is checked against the cap.
    fn presign_put(
        &self,
        key: &str,
        content_length: u64,
        now_ms: u64,
    ) -> Result<PutTarget, BlobError>;

    /// The stored bytes, or `None` when there is no such object.
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, BlobError>>;

    /// Deletes the object; a missing object is fine.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), BlobError>>;

    /// The local store, when this is one (it serves its own `PUT` route).
    fn as_local(&self) -> Option<&LocalBlobStore> {
        None
    }
}

/// Keys are plain relative paths of `[A-Za-z0-9._-]` segments.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 200
        && key.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

fn check_length(len: u64, max: u64) -> Result<(), BlobError> {
    if len == 0 || len > max {
        Err(BlobError::BadLength)
    } else {
        Ok(())
    }
}

/// HMAC-SHA-256 (RFC 2104) over the `sha2` crate.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

// ---------------------------------------------------------------------------
// Local store
// ---------------------------------------------------------------------------

pub struct LocalBlobStore {
    dir: PathBuf,
    hmac_key: Vec<u8>,
    public_url: String,
    max_bytes: u64,
    expiry_ms: u64,
}

impl LocalBlobStore {
    pub fn new(
        dir: PathBuf,
        hmac_key: Vec<u8>,
        public_url: &str,
        max_bytes: u64,
        expiry_sec: u64,
    ) -> Self {
        Self {
            dir,
            hmac_key,
            public_url: public_url.trim_end_matches('/').to_owned(),
            max_bytes,
            expiry_ms: expiry_sec * 1000,
        }
    }

    fn signature(&self, key: &str, expires_ms: u64, len: u64) -> String {
        let message = format!("PUT\n{key}\n{expires_ms}\n{len}");
        hex_encode(&hmac_sha256(&self.hmac_key, message.as_bytes()))
    }

    fn path(&self, key: &str) -> Result<PathBuf, BlobError> {
        if !valid_key(key) {
            return Err(BlobError::BadKey);
        }
        Ok(self.dir.join(key))
    }

    /// Serves a `PUT /v1/blobs/{key}?exp&len&sig`: the signature, the expiry
    /// and the exact length are all checked before a byte is written.
    pub fn accept_put(
        &self,
        key: &str,
        expires_ms: u64,
        len: u64,
        signature: &str,
        now_ms: u64,
        body: &[u8],
    ) -> Result<(), BlobError> {
        let path = self.path(key)?;
        if !secure_eq(
            self.signature(key, expires_ms, len).as_bytes(),
            signature.as_bytes(),
        ) {
            return Err(BlobError::BadSignature);
        }
        if now_ms >= expires_ms {
            return Err(BlobError::Expired);
        }
        check_length(len, self.max_bytes)?;
        if u64::try_from(body.len()).ok() != Some(len) {
            return Err(BlobError::LengthMismatch);
        }
        write_atomically(&path, body).map_err(|e| BlobError::Io(e.to_string()))
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("part-{}", crate::token::random_hex(4)));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

impl BlobStore for LocalBlobStore {
    fn presign_put(
        &self,
        key: &str,
        content_length: u64,
        now_ms: u64,
    ) -> Result<PutTarget, BlobError> {
        self.path(key)?;
        check_length(content_length, self.max_bytes)?;
        let expires_ms = now_ms + self.expiry_ms;
        let signature = self.signature(key, expires_ms, content_length);
        Ok(PutTarget {
            url: format!(
                "{}/v1/blobs/{key}?exp={expires_ms}&len={content_length}&sig={signature}",
                self.public_url
            ),
            expires_at_ms: expires_ms,
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, BlobError>> {
        Box::pin(async move {
            let path = self.path(key)?;
            let max = self.max_bytes;
            tokio::task::spawn_blocking(move || match std::fs::metadata(&path) {
                Ok(meta) if meta.len() > max => Err(BlobError::BadLength),
                Ok(_) => std::fs::read(&path)
                    .map(Some)
                    .map_err(|e| BlobError::Io(e.to_string())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(BlobError::Io(e.to_string())),
            })
            .await
            .map_err(|e| BlobError::Io(e.to_string()))?
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            let path = self.path(key)?;
            tokio::task::spawn_blocking(move || match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(BlobError::Io(e.to_string())),
            })
            .await
            .map_err(|e| BlobError::Io(e.to_string()))?
        })
    }

    fn as_local(&self) -> Option<&LocalBlobStore> {
        Some(self)
    }
}

// ---------------------------------------------------------------------------
// AWS Signature Version 4, query-string presigning (sans IO)
// ---------------------------------------------------------------------------

pub mod sigv4 {
    use super::*;

    pub struct Credentials {
        pub access_key_id: String,
        pub secret_access_key: String,
    }

    pub struct Presign<'a> {
        pub method: &'a str,
        /// `https://host[:port]`, no trailing slash.
        pub origin: &'a str,
        /// The already URI-encoded absolute path (`/bucket/key`).
        pub path: &'a str,
        pub region: &'a str,
        pub service: &'a str,
        pub unix_secs: u64,
        pub expires_sec: u64,
        /// Extra signed headers (lowercase name, value) besides `host`.
        pub signed_headers: &'a [(&'a str, String)],
    }

    /// `(year, month, day)` of a count of days since 1970-01-01.
    fn civil(days: i64) -> (i64, u32, u32) {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
        let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
        let year = yoe + era * 400 + i64::from(month <= 2);
        (year, month, day)
    }

    /// `(20130524T000000Z, 20130524)`.
    pub fn timestamps(unix_secs: u64) -> (String, String) {
        let days = i64::try_from(unix_secs / 86_400).unwrap_or(0);
        let rem = unix_secs % 86_400;
        let (y, m, d) = civil(days);
        let date = format!("{y:04}{m:02}{d:02}");
        let stamp = format!(
            "{date}T{:02}{:02}{:02}Z",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        );
        (stamp, date)
    }

    /// The presigned URL: every parameter in the query string, payload
    /// `UNSIGNED-PAYLOAD`.
    pub fn presign(credentials: &Credentials, request: &Presign<'_>) -> String {
        let (stamp, date) = timestamps(request.unix_secs);
        let host = request
            .origin
            .split_once("://")
            .map_or(request.origin, |(_, rest)| rest);
        let scope = format!("{date}/{}/{}/aws4_request", request.region, request.service);

        let mut headers: Vec<(String, String)> = vec![("host".into(), host.to_owned())];
        headers.extend(
            request
                .signed_headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.trim().to_owned())),
        );
        headers.sort();
        let signed_names = headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");

        let mut query = vec![
            ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
            (
                "X-Amz-Credential".to_owned(),
                format!("{}/{scope}", credentials.access_key_id),
            ),
            ("X-Amz-Date".to_owned(), stamp.clone()),
            ("X-Amz-Expires".to_owned(), request.expires_sec.to_string()),
            ("X-Amz-SignedHeaders".to_owned(), signed_names.clone()),
        ];
        let encode_sorted = |pairs: &[(String, String)]| {
            let mut encoded: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (url_encode(k), url_encode(v)))
                .collect();
            encoded.sort();
            encoded
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&")
        };
        let canonical_query = encode_sorted(&query);
        let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let canonical_request = format!(
            "{}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_names}\nUNSIGNED-PAYLOAD",
            request.method, request.path
        );
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
            hex_encode(&Sha256::digest(canonical_request.as_bytes()))
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", credentials.secret_access_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, request.region.as_bytes());
        let k_service = hmac_sha256(&k_region, request.service.as_bytes());
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex_encode(&hmac_sha256(&k_signing, to_sign.as_bytes()));
        query.push(("X-Amz-Signature".to_owned(), signature));
        format!(
            "{}{}?{}",
            request.origin,
            request.path,
            // Parameters are sorted by name, so the signature precedes SignedHeaders.
            encode_sorted(&query)
        )
    }
}

// ---------------------------------------------------------------------------
// R2 / S3-compatible store
// ---------------------------------------------------------------------------

pub struct R2BlobStore {
    credentials: sigv4::Credentials,
    /// `https://account.r2.cloudflarestorage.com` (or `http://127.0.0.1:9000`).
    origin: String,
    bucket: String,
    region: String,
    max_bytes: u64,
    expiry_sec: u64,
    clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl R2BlobStore {
    pub fn new(
        credentials: sigv4::Credentials,
        endpoint: &str,
        bucket: &str,
        region: &str,
        max_bytes: u64,
        expiry_sec: u64,
        clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, String> {
        let origin = endpoint.trim().trim_end_matches('/').to_owned();
        let (scheme, rest) = origin
            .split_once("://")
            .ok_or("[blob] r2_endpoint must be a URL")?;
        if rest.is_empty() || rest.contains('/') || !matches!(scheme, "https" | "http") {
            return Err("[blob] r2_endpoint must be https://host[:port] with no path".into());
        }
        if bucket.is_empty() {
            return Err("[blob] r2_bucket is required for the r2 provider".into());
        }
        Ok(Self {
            credentials,
            origin,
            bucket: bucket.to_owned(),
            region: region.to_owned(),
            max_bytes,
            expiry_sec,
            clock_ms,
        })
    }

    fn url(
        &self,
        method: &str,
        key: &str,
        now_ms: u64,
        extra: &[(&str, String)],
    ) -> Result<String, BlobError> {
        if !valid_key(key) {
            return Err(BlobError::BadKey);
        }
        let path = format!("/{}/{key}", self.bucket);
        Ok(sigv4::presign(
            &self.credentials,
            &sigv4::Presign {
                method,
                origin: &self.origin,
                path: &path,
                region: &self.region,
                service: "s3",
                unix_secs: now_ms / 1000,
                expires_sec: self.expiry_sec,
                signed_headers: extra,
            },
        ))
    }

    fn call(method: &'static str, url: String, max: u64) -> Result<Option<Vec<u8>>, BlobError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        let io = |e: ureq::Error| BlobError::Io(e.to_string());
        let mut response = match method {
            "GET" => agent.get(&url).call().map_err(io)?,
            _ => agent.delete(&url).call().map_err(io)?,
        };
        match response.status().as_u16() {
            200 | 204 => {
                if method == "GET" {
                    let bytes = response
                        .body_mut()
                        .with_config()
                        .limit(max)
                        .read_to_vec()
                        .map_err(io)?;
                    Ok(Some(bytes))
                } else {
                    Ok(None)
                }
            }
            404 => Ok(None),
            status => Err(BlobError::Io(format!("the object store answered {status}"))),
        }
    }
}

impl BlobStore for R2BlobStore {
    fn presign_put(
        &self,
        key: &str,
        content_length: u64,
        now_ms: u64,
    ) -> Result<PutTarget, BlobError> {
        check_length(content_length, self.max_bytes)?;
        let url = self.url(
            "PUT",
            key,
            now_ms,
            &[("content-length", content_length.to_string())],
        )?;
        Ok(PutTarget {
            url,
            expires_at_ms: now_ms + self.expiry_sec * 1000,
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, BlobError>> {
        Box::pin(async move {
            let url = self.url("GET", key, (self.clock_ms)(), &[])?;
            let max = self.max_bytes;
            tokio::task::spawn_blocking(move || Self::call("GET", url, max))
                .await
                .map_err(|e| BlobError::Io(e.to_string()))?
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), BlobError>> {
        Box::pin(async move {
            let url = self.url("DELETE", key, (self.clock_ms)(), &[])?;
            tokio::task::spawn_blocking(move || Self::call("DELETE", url, 0))
                .await
                .map_err(|e| BlobError::Io(e.to_string()))?
                .map(|_| ())
        })
    }
}

/// Builds the configured store. `env` reads environment variables.
pub fn build(
    config: &Blob,
    public_url: &str,
    clock_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn BlobStore>, String> {
    match config.provider.as_str() {
        "local" => {
            let key = match &config.local_hmac_key_file {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|_| "[blob] local_hmac_key_file cannot be read".to_owned())?
                    .trim()
                    .as_bytes()
                    .to_vec(),
                None => match env(&config.local_hmac_key_env).filter(|v| !v.is_empty()) {
                    Some(value) => value.into_bytes(),
                    // No key configured: a per-process random one. Presigned
                    // URLs (15 minutes) do not survive a restart.
                    None => crate::token::random_hex(32).into_bytes(),
                },
            };
            if key.len() < 16 {
                return Err("the blob HMAC key must be at least 16 bytes".into());
            }
            Ok(Arc::new(LocalBlobStore::new(
                PathBuf::from(&config.local_dir),
                key,
                public_url,
                config.max_bytes,
                config.presign_expiry_sec,
            )))
        }
        "r2" => {
            let access_key_id = env(&config.r2_access_key_id_env)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{} is not set", config.r2_access_key_id_env))?;
            let secret_access_key = match &config.r2_secret_access_key_file {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|_| "[blob] r2_secret_access_key_file cannot be read".to_owned())?
                    .trim()
                    .to_owned(),
                None => env(&config.r2_secret_access_key_env)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| format!("{} is not set", config.r2_secret_access_key_env))?,
            };
            Ok(Arc::new(R2BlobStore::new(
                sigv4::Credentials {
                    access_key_id,
                    secret_access_key,
                },
                &config.r2_endpoint,
                &config.r2_bucket,
                &config.r2_region,
                config.max_bytes,
                config.presign_expiry_sec,
                clock_ms,
            )?))
        }
        other => Err(format!("unknown blob provider `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_the_rfc_4231_vectors() {
        // Test case 1.
        assert_eq!(
            hex_encode(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2.
        assert_eq!(
            hex_encode(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block size.
        assert_eq!(
            hex_encode(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn timestamps_format_like_sigv4() {
        // 2013-05-24T00:00:00Z, the date of AWS's published examples.
        assert_eq!(
            sigv4::timestamps(1_369_353_600),
            ("20130524T000000Z".into(), "20130524".into())
        );
        assert_eq!(
            sigv4::timestamps(951_782_400 + 86_399),
            ("20000229T235959Z".into(), "20000229".into())
        );
    }

    /// AWS's published "Authenticating Requests: Using Query Parameters"
    /// example (S3 `GET /test.txt`, virtual-hosted style, expires 86400).
    #[test]
    fn presigning_matches_the_aws_published_test_vector() {
        let url = sigv4::presign(
            &sigv4::Credentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            },
            &sigv4::Presign {
                method: "GET",
                origin: "https://examplebucket.s3.amazonaws.com",
                path: "/test.txt",
                region: "us-east-1",
                service: "s3",
                unix_secs: 1_369_353_600,
                expires_sec: 86_400,
                signed_headers: &[],
            },
        );
        assert_eq!(
            url,
            "https://examplebucket.s3.amazonaws.com/test.txt?X-Amz-Algorithm=AWS4-HMAC-SHA256\
             &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date=20130524T000000Z&X-Amz-Expires=86400\
             &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404&X-Amz-SignedHeaders=host"
        );
    }

    fn credentials() -> sigv4::Credentials {
        sigv4::Credentials {
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
        }
    }

    fn r2() -> R2BlobStore {
        R2BlobStore::new(
            credentials(),
            "https://acct.r2.cloudflarestorage.com",
            "bundles",
            "auto",
            64 * 1024 * 1024,
            900,
            Arc::new(|| 1_369_353_600_000),
        )
        .unwrap()
    }

    #[test]
    fn an_r2_put_signs_the_exact_content_length_and_a_15_minute_expiry() {
        let target = r2()
            .presign_put("bundles/abc.json", 1234, 1_369_353_600_000)
            .unwrap();
        assert!(target.url.starts_with(
            "https://acct.r2.cloudflarestorage.com/bundles/bundles/abc.json?X-Amz-Algorithm"
        ));
        assert!(target.url.contains("X-Amz-Expires=900"));
        assert!(
            target
                .url
                .contains("X-Amz-SignedHeaders=content-length%3Bhost"),
            "{}",
            target.url
        );
        assert_eq!(target.expires_at_ms, 1_369_353_600_000 + 900_000);
        // A different length is a different signature.
        let other = r2()
            .presign_put("bundles/abc.json", 1235, 1_369_353_600_000)
            .unwrap();
        let sig = |u: &str| u.rsplit_once("X-Amz-Signature=").unwrap().1.to_owned();
        assert_ne!(sig(&target.url), sig(&other.url));
    }

    #[test]
    fn the_cap_and_the_key_alphabet_are_enforced_before_signing() {
        let store = r2();
        assert_eq!(
            store.presign_put("k", 64 * 1024 * 1024 + 1, 0),
            Err(BlobError::BadLength)
        );
        assert_eq!(store.presign_put("k", 0, 0), Err(BlobError::BadLength));
        assert!(store.presign_put("k", 64 * 1024 * 1024, 0).is_ok());
        for bad in ["", "../x", "a/../b", "a b", "a?b", "/abs", "a//b", "a#b"] {
            assert_eq!(
                store.presign_put(bad, 1, 0),
                Err(BlobError::BadKey),
                "{bad}"
            );
        }
    }

    #[test]
    fn r2_endpoint_and_bucket_are_validated() {
        let make = |endpoint: &str, bucket: &str| {
            R2BlobStore::new(
                credentials(),
                endpoint,
                bucket,
                "auto",
                1,
                1,
                Arc::new(|| 0),
            )
            .is_ok()
        };
        assert!(make("https://a.b", "bkt"));
        assert!(make("http://127.0.0.1:9000", "bkt"));
        assert!(!make("a.b", "bkt"));
        assert!(!make("https://a.b/path", "bkt"));
        assert!(!make("ftp://a.b", "bkt"));
        assert!(!make("https://a.b", ""));
    }

    fn local(dir: &Path) -> LocalBlobStore {
        LocalBlobStore::new(
            dir.to_path_buf(),
            b"0123456789abcdef0123".to_vec(),
            "http://127.0.0.1:8080/",
            1000,
            900,
        )
    }

    /// Splits a local URL into the `accept_put` arguments.
    fn parts(url: &str) -> (String, u64, u64, String) {
        let rest = url.strip_prefix("http://127.0.0.1:8080/v1/blobs/").unwrap();
        let (key, query) = rest.split_once('?').unwrap();
        let get = |name: &str| {
            query
                .split('&')
                .find_map(|p| p.strip_prefix(&format!("{name}=")))
                .unwrap()
                .to_owned()
        };
        (
            key.to_owned(),
            get("exp").parse().unwrap(),
            get("len").parse().unwrap(),
            get("sig"),
        )
    }

    #[tokio::test]
    async fn a_local_blob_round_trips_through_a_signed_url() {
        let dir = tempfile::tempdir().unwrap();
        let store = local(dir.path());
        let target = store.presign_put("bundles/b1.json", 5, 10_000).unwrap();
        assert_eq!(target.expires_at_ms, 10_000 + 900_000);
        let (key, exp, len, sig) = parts(&target.url);
        store
            .accept_put(&key, exp, len, &sig, 10_001, b"hello")
            .unwrap();
        assert_eq!(
            store.get("bundles/b1.json").await.unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(store.get("bundles/none.json").await.unwrap(), None);
        store.delete("bundles/b1.json").await.unwrap();
        store.delete("bundles/b1.json").await.unwrap();
        assert_eq!(store.get("bundles/b1.json").await.unwrap(), None);
    }

    #[test]
    fn a_local_url_expires_and_is_bound_to_key_length_and_signature() {
        let dir = tempfile::tempdir().unwrap();
        let store = local(dir.path());
        let target = store.presign_put("bundles/b1.json", 5, 10_000).unwrap();
        let (key, exp, len, sig) = parts(&target.url);
        // Expired at the expiry instant.
        assert_eq!(
            store.accept_put(&key, exp, len, &sig, exp, b"hello"),
            Err(BlobError::Expired)
        );
        // The exact Content-Length: a longer or shorter body is refused.
        for body in [&b"hell"[..], b"hello!"] {
            assert_eq!(
                store.accept_put(&key, exp, len, &sig, 10_001, body),
                Err(BlobError::LengthMismatch)
            );
        }
        // Tampering with any signed part breaks the signature.
        assert_eq!(
            store.accept_put("bundles/other.json", exp, len, &sig, 10_001, b"hello"),
            Err(BlobError::BadSignature)
        );
        assert_eq!(
            store.accept_put(&key, exp + 1, len, &sig, 10_001, b"hello"),
            Err(BlobError::BadSignature)
        );
        assert_eq!(
            store.accept_put(&key, exp, 6, &sig, 10_001, b"hello!"),
            Err(BlobError::BadSignature)
        );
        assert_eq!(
            store.accept_put(&key, exp, len, "00", 10_001, b"hello"),
            Err(BlobError::BadSignature)
        );
        // A different server key cannot forge one either.
        let other = LocalBlobStore::new(
            dir.path().to_path_buf(),
            b"another-key-another-key".to_vec(),
            "http://127.0.0.1:8080",
            1000,
            900,
        );
        assert_eq!(
            other.accept_put(&key, exp, len, &sig, 10_001, b"hello"),
            Err(BlobError::BadSignature)
        );
        // Nothing was written by any refusal.
        assert!(!dir.path().join("bundles").exists());
    }

    #[test]
    fn the_local_cap_applies_at_presign_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = local(dir.path());
        assert!(store.presign_put("k", 1000, 0).is_ok());
        assert_eq!(store.presign_put("k", 1001, 0), Err(BlobError::BadLength));
        assert_eq!(store.presign_put("../k", 5, 0), Err(BlobError::BadKey));
    }

    #[test]
    fn build_refuses_an_unknown_provider_and_missing_r2_credentials() {
        let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 0);
        let mut config = Blob {
            provider: "gcs".into(),
            ..Blob::default()
        };
        assert!(build(&config, "http://x", Arc::clone(&clock), |_| None).is_err());
        config.provider = "r2".into();
        config.r2_endpoint = "https://a.b".into();
        config.r2_bucket = "b".into();
        assert!(build(&config, "http://x", Arc::clone(&clock), |_| None).is_err());
        let ok = build(&config, "http://x", clock, |name| {
            Some(format!("value-of-{name}"))
        });
        assert!(ok.is_ok());
    }
}
