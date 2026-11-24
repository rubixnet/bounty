//! The Local Ingestion Bus request pipeline, shared by every transport
//! (loopback HTTP now, the Unix socket later).
//!
//! Checks run in a fixed order and the first failure answers the request:
//!
//! 1. `Host` allowlist (HTTP only)        -- ticket 05
//! 2. `Origin` allowlist                  -- ticket 05
//! 3. bearer token                        -> `401`
//! 4. route (`POST /v1/event`)            -> `404`
//! 5. body size cap                       -> `413`
//! 6. deserialize + validate              -> `400` with a reason code
//! 7. enqueue to the writer               -> `503` when full, else `202`
//!
//! Responses never echo the request: every body is a fixed reason code.

use std::convert::Infallible;

use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use traction_core::TractionEvent;

use crate::storage::{EventRow, IntegerOutOfRange};
use crate::token::BusToken;
use crate::writer::WriterSender;

pub const EVENT_PATH: &str = "/v1/event";
/// Request bodies above this many bytes are refused with `413`.
pub const MAX_BODY_BYTES: usize = 16 * 1024;

pub type BusResponse = Response<Full<Bytes>>;

/// What every transport needs to answer a request.
pub struct Bus {
    token: BusToken,
    writer: WriterSender,
}

/// Why a request was refused: a status and a fixed reason code. Holds no
/// request data, so it cannot echo input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rejection {
    status: StatusCode,
    code: &'static str,
}

impl Rejection {
    const UNAUTHORIZED: Self = Self::new(StatusCode::UNAUTHORIZED, "unauthorized");
    const NOT_FOUND: Self = Self::new(StatusCode::NOT_FOUND, "not_found");
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
    pub fn new(token: BusToken, writer: WriterSender) -> Self {
        Self { token, writer }
    }

    /// Runs the full check pipeline for one request.
    pub async fn handle(&self, request: Request<Incoming>) -> Result<BusResponse, Infallible> {
        Ok(match self.check(request).await {
            Ok(()) => respond(StatusCode::ACCEPTED, "status", "accepted"),
            Err(rejection) => respond(rejection.status, "error", rejection.code),
        })
    }

    async fn check(&self, request: Request<Incoming>) -> Result<(), Rejection> {
        self.authorize(&request)?;
        if request.uri().path() != EVENT_PATH || request.method() != Method::POST {
            return Err(Rejection::NOT_FOUND);
        }
        let body = read_capped_body(request.into_body()).await?;
        let event = TractionEvent::from_json(&body)
            .map_err(|error| Rejection::bad_request(error.code()))?;
        let row = EventRow::try_from(event)
            .map_err(|IntegerOutOfRange| Rejection::bad_request(IntegerOutOfRange::CODE))?;
        self.writer.try_enqueue(row).map_err(|_| Rejection::BUSY)
    }

    fn authorize(&self, request: &Request<Incoming>) -> Result<(), Rejection> {
        let presented = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|value| bearer_value(value.as_bytes()));
        match presented {
            Some(token) if self.token.matches(token) => Ok(()),
            _ => Err(Rejection::UNAUTHORIZED),
        }
    }
}

/// The credentials of an `Authorization: Bearer <token>` header.
fn bearer_value(header: &[u8]) -> Option<&[u8]> {
    const SCHEME: &[u8] = b"bearer ";
    let (scheme, rest) = header.split_at_checked(SCHEME.len())?;
    scheme
        .eq_ignore_ascii_case(SCHEME)
        .then(|| rest.trim_ascii())
}

async fn read_capped_body(body: Incoming) -> Result<Bytes, Rejection> {
    match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.is::<LengthLimitError>() => Err(Rejection::BODY_TOO_LARGE),
        Err(_) => Err(Rejection::UNREADABLE_BODY),
    }
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
