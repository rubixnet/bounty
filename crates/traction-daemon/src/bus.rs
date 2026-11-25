//! The Local Ingestion Bus request pipeline, shared by every transport
//! (loopback HTTP and, on macOS/Linux, the Unix socket).
//!
//! Checks run in a fixed order and the first failure answers the request:
//!
//! 1. `Host` allowlist (HTTP only)        -> `403 forbidden_host`
//! 2. `Origin` allowlist                  -> `403 forbidden_origin`
//! 3. bearer token                        -> `401`
//! 4. route (`POST /v1/event`)            -> `404`, or `405` for another method
//! 5. body size cap                       -> `413`
//! 6. deserialize + validate              -> `400` with a reason code
//! 7. enqueue to the writer               -> `503` when full, else `202`
//!
//! Responses never echo the request: every body is a fixed reason code.
//!
//! A request refused before its body is fully read has the rest of the body
//! read and discarded (bounded in bytes and time) before the reply. Closing
//! with unread bytes makes the kernel reset the connection, and a reset can
//! destroy the reply before the caller reads it (always on Windows).

use std::convert::Infallible;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Incoming};
use hyper::header::{ALLOW, AUTHORIZATION, CONTENT_TYPE, EXPECT, HOST, HeaderValue, ORIGIN};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode};
use traction_core::TractionEvent;

use crate::storage::EventRow;
use crate::token::BusToken;
use crate::writer::WriterSender;

pub const EVENT_PATH: &str = "/v1/event";
/// Request bodies above this many bytes are refused with `413`.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

/// After a refusal, at most this many body bytes in total are read (and
/// dropped) before the reply. Discarded chunk by chunk, never buffered.
const DRAIN_MAX_BYTES: usize = 64 * 1024;
/// After a refusal, the rest of the body must arrive within this time.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

pub type BusResponse = Response<Full<Bytes>>;

/// What every transport needs to answer a request.
pub struct Bus {
    token: BusToken,
    writer: WriterSender,
    /// The bound loopback HTTP port: the only port allowed in `Host` and
    /// `Origin`.
    port: u16,
}

/// Which listener a request arrived on. Only loopback HTTP checks `Host`: a
/// Unix-socket request has no TCP origin, so DNS rebinding cannot reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Http,
    /// The macOS/Linux bus socket (no socket on Windows in Step 1a).
    #[cfg(unix)]
    Unix,
}

/// Why a request was refused: a status and a fixed reason code. Holds no
/// request data, so it cannot echo input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rejection {
    status: StatusCode,
    code: &'static str,
}

impl Rejection {
    const FORBIDDEN_HOST: Self = Self::new(StatusCode::FORBIDDEN, "forbidden_host");
    const FORBIDDEN_ORIGIN: Self = Self::new(StatusCode::FORBIDDEN, "forbidden_origin");
    const UNAUTHORIZED: Self = Self::new(StatusCode::UNAUTHORIZED, "unauthorized");
    const NOT_FOUND: Self = Self::new(StatusCode::NOT_FOUND, "not_found");
    const METHOD_NOT_ALLOWED: Self =
        Self::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    const BODY_TOO_LARGE: Self = Self::new(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large");
    const UNREADABLE_BODY: Self = Self::new(StatusCode::BAD_REQUEST, "unreadable_body");
    const BUSY: Self = Self::new(StatusCode::SERVICE_UNAVAILABLE, "busy");

    const fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    const fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }
}

impl Bus {
    pub fn new(token: BusToken, writer: WriterSender, port: u16) -> Self {
        Self {
            token,
            writer,
            port,
        }
    }

    /// Runs the full check pipeline for one request.
    pub async fn handle(
        &self,
        request: Request<Incoming>,
        transport: Transport,
    ) -> Result<BusResponse, Infallible> {
        Ok(match self.check(request, transport).await {
            Ok(()) => respond(StatusCode::ACCEPTED, "status", "accepted"),
            Err(rejection) => {
                let mut response = respond(rejection.status, "error", rejection.code);
                if rejection == Rejection::METHOD_NOT_ALLOWED {
                    response
                        .headers_mut()
                        .insert(ALLOW, HeaderValue::from_static("POST"));
                }
                response
            }
        })
    }

    async fn check(
        &self,
        request: Request<Incoming>,
        transport: Transport,
    ) -> Result<(), Rejection> {
        let (parts, mut body) = request.into_parts();
        let mut read = 0;
        let result = self
            .check_parts(&parts, &mut body, &mut read, transport)
            .await;
        if result.is_err() && !body.is_end_stream() && !expects_continue(&parts) {
            discard_body(&mut body, read).await;
        }
        result
    }

    /// The pipeline proper. `read` counts the body bytes consumed so far.
    async fn check_parts(
        &self,
        parts: &Parts,
        body: &mut Incoming,
        read: &mut usize,
        transport: Transport,
    ) -> Result<(), Rejection> {
        if transport == Transport::Http {
            self.check_host(parts)?;
        }
        self.check_origin(parts)?;
        self.authorize(parts)?;
        if parts.uri.path() != EVENT_PATH {
            return Err(Rejection::NOT_FOUND);
        }
        if parts.method != Method::POST {
            return Err(Rejection::METHOD_NOT_ALLOWED);
        }
        let body = read_capped_body(body, read).await?;
        let event = TractionEvent::from_json(&body)
            .map_err(|error| Rejection::bad_request(error.code()))?;
        let row =
            EventRow::try_from(event).map_err(|error| Rejection::bad_request(error.code()))?;
        self.writer.try_enqueue(row).map_err(|_| Rejection::BUSY)
    }

    /// Exactly one `Host` header naming loopback on the bound port. An
    /// absolute-form request target must name the same. Blocks DNS rebinding.
    fn check_host(&self, parts: &Parts) -> Result<(), Rejection> {
        let mut hosts = parts.headers.get_all(HOST).iter();
        let allowed = match (hosts.next(), hosts.next()) {
            (Some(host), None) => self.is_own_authority(host.as_bytes()),
            _ => false,
        };
        let target_ok = parts
            .uri
            .authority()
            .is_none_or(|authority| self.is_own_authority(authority.as_str().as_bytes()));
        if allowed && target_ok {
            Ok(())
        } else {
            Err(Rejection::FORBIDDEN_HOST)
        }
    }

    /// No `Origin` (non-browser callers), or exactly the daemon's own
    /// `http://` origin. Blocks browser CSRF, including `Origin: null`.
    fn check_origin(&self, parts: &Parts) -> Result<(), Rejection> {
        let mut origins = parts.headers.get_all(ORIGIN).iter();
        let allowed = match (origins.next(), origins.next()) {
            (None, _) => true,
            (Some(origin), None) => origin
                .as_bytes()
                .split_at_checked(HTTP_SCHEME.len())
                .is_some_and(|(scheme, authority)| {
                    scheme.eq_ignore_ascii_case(HTTP_SCHEME) && self.is_own_authority(authority)
                }),
            _ => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(Rejection::FORBIDDEN_ORIGIN)
        }
    }

    /// `127.0.0.1:<port>` or `localhost:<port>` (host case-insensitive).
    fn is_own_authority(&self, authority: &[u8]) -> bool {
        let Some(colon) = authority.iter().rposition(|&b| b == b':') else {
            return false;
        };
        let (host, port) = (&authority[..colon], &authority[colon + 1..]);
        let port_ok = port == self.port.to_string().as_bytes();
        let host_ok = host == b"127.0.0.1" || host.eq_ignore_ascii_case(b"localhost");
        port_ok && host_ok
    }

    fn authorize(&self, parts: &Parts) -> Result<(), Rejection> {
        let presented = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| bearer_value(value.as_bytes()));
        match presented {
            Some(token) if self.token.matches(token) => Ok(()),
            _ => Err(Rejection::UNAUTHORIZED),
        }
    }
}

const HTTP_SCHEME: &[u8] = b"http://";

/// The credentials of an `Authorization: Bearer <token>` header.
fn bearer_value(header: &[u8]) -> Option<&[u8]> {
    const SCHEME: &[u8] = b"bearer ";
    let (scheme, rest) = header.split_at_checked(SCHEME.len())?;
    scheme
        .eq_ignore_ascii_case(SCHEME)
        .then(|| rest.trim_ascii())
}

/// Reads the whole body, refusing it once it passes [`MAX_BODY_BYTES`].
/// Adds every byte read to `read`, so a refusal can drain the rest.
async fn read_capped_body(body: &mut Incoming, read: &mut usize) -> Result<Vec<u8>, Rejection> {
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Rejection::UNREADABLE_BODY)?;
        if let Some(data) = frame.data_ref() {
            *read += data.len();
            if *read > MAX_BODY_BYTES {
                return Err(Rejection::BODY_TOO_LARGE);
            }
            collected.extend_from_slice(data);
        }
    }
    Ok(collected)
}

/// Reads and drops the rest of a refused request's body until it ends, the
/// total read reaches [`DRAIN_MAX_BYTES`], it fails, or [`DRAIN_TIMEOUT`]
/// passes. Memory stays bounded: each chunk is dropped as it arrives.
async fn discard_body(body: &mut Incoming, mut read: usize) {
    let drain = async {
        while read < DRAIN_MAX_BYTES {
            match body.frame().await {
                Some(Ok(frame)) => read += frame.data_ref().map_or(0, Bytes::len),
                Some(Err(_)) | None => break,
            }
        }
    };
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
}

/// The caller waits for `100 Continue` before sending a body. Reading the
/// body would invite it, so a refused request of this kind is not drained.
fn expects_continue(parts: &Parts) -> bool {
    parts.headers.contains_key(EXPECT)
}

/// A JSON response `{"<key>":"<code>"}` built only from static strings.
fn respond(status: StatusCode, key: &'static str, code: &'static str) -> BusResponse {
    let body = format!("{{\"{key}\":\"{code}\"}}");
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}
