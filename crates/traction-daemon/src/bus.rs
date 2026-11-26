//! The Local Ingestion Bus request pipeline, shared by every transport
//! (loopback HTTP and, on macOS/Linux, the Unix socket).
//!
//! Checks run in a fixed order and the first failure answers the request:
//!
//! 1. `Host` allowlist (HTTP only)        -> `403 forbidden_host`
//! 2. `Origin` allowlist                  -> `403 forbidden_origin`
//! 3. bearer token                        -> `401`
//! 4. route (`POST /v1/event`, `POST /v1/project`,
//!    `POST /v1/hooks/claude-code`)       -> `404`, or `405`
//! 5. body size cap                       -> `413` (8 MiB for the hook
//!    route, [`MAX_BODY_BYTES`] otherwise)
//!
//! Then, for `/v1/event`:
//!
//! 6. deserialize + validate              -> `400` with a reason code
//! 7. bus policy (see [`check_policy`])   -> `400` with a reason code
//! 8. enqueue to the Project Router       -> `503` when full, else `202`
//!
//! The bus answers as soon as the event is queued: routing (Git calls
//! included) happens later on the Router thread.
//!
//! And for `/v1/project` (`{"path": "<absolute folder>"}`):
//!
//! 6. deserialize (unknown fields refused) -> `400 malformed_registration`
//! 7. absolute, bounded path               -> `400 invalid_path`
//! 8. the Router registers it              -> `422` with a reason code, `503`
//!    when busy, else `200 {"project_id": "..."}`
//!
//! And for `/v1/hooks/claude-code` (a Claude Code native hook payload):
//!
//! 6. map to at most one content-free Traction Event (or a `DirectoryAdded`
//!    registration); the body buffer is dropped right after
//! 7. queue it to the Project Router, never waiting for an answer
//! 8. `200 {}`: an unusable payload, a hook outside the mapping and a full
//!    Router queue (the event is dropped and counted in the log) all get
//!    the same non-blocking answer. The one exception is a `UserPromptSubmit`
//!    that is exactly `/stop-track` or `/resume-track` (Session Pause): it
//!    gets `200 {"decision":"block","reason":"..."}`, so the command never
//!    reaches the model, and stores no event
//!
//! Responses never echo the request: every body is a fixed reason code.
//!
//! A request refused before its body is fully read has the rest of the body
//! read and discarded (bounded in bytes and time) before the reply. Closing
//! with unread bytes makes the kernel reset the connection, and a reset can
//! destroy the reply before the caller reads it (always on Windows).

use crate::clock::now_ms;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Bytes, Incoming};
use hyper::header::{ALLOW, AUTHORIZATION, CONTENT_TYPE, EXPECT, HOST, HeaderValue, ORIGIN};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use traction_core::{AttributionSource, EventType, HarnessId, MAX_CWD_LEN, TractionEvent};

use crate::claude_code::hook::{self, HOOK_PATH, HookAction, MAX_HOOK_BODY_BYTES};
use crate::claude_code::pause::BUSY_REASON;
use crate::claude_code::tailer::TranscriptHints;
use crate::log;
use crate::router::{Pause, RouterSender};
use crate::storage::EventRow;
use crate::symbol_index::SymbolIndexes;
use crate::token::BusToken;

pub const EVENT_PATH: &str = "/v1/event";
/// Registers a Project path (`docs/protocol.md` section 2a).
pub const PROJECT_PATH: &str = "/v1/project";
/// A registration still unanswered after this long gets `503 busy`.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(30);
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
    router: RouterSender,
    /// The bound loopback HTTP port: the only port allowed in `Host` and
    /// `Origin`.
    port: u16,
    /// Hook events dropped because the Router queue was full.
    dropped_hooks: AtomicU64,
    /// Where hook-reported `transcript_path`s go (absent without a tailer).
    transcript_hints: Option<TranscriptHints>,
    /// File-name indexes, for a hook prompt's `repo_symbol_hits`.
    symbols: SymbolIndexes,
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
    const MALFORMED_REGISTRATION: Self = Self::bad_request("malformed_registration");
    const INVALID_PATH: Self = Self::bad_request("invalid_path");

    const fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    const fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }
}

/// What a request that passed every check gets.
enum Reply {
    /// `202`: the event is queued for routing and storage.
    Accepted,
    /// `200`: the path's Project is registered under this ID.
    Registered(String),
    /// `200 {}`: a Claude Code hook, handled or not. Never blocks Claude.
    Hook,
    /// `200 {"decision":"block","reason":<reason>}`: a Session Pause
    /// command, kept from the model.
    BlockHook(&'static str),
}

/// The `/v1/project` body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    path: String,
}

impl Bus {
    pub fn new(token: BusToken, router: RouterSender, port: u16) -> Self {
        Self {
            token,
            router,
            port,
            dropped_hooks: AtomicU64::new(0),
            transcript_hints: None,
            symbols: SymbolIndexes::default(),
        }
    }

    /// Matches hook prompts against these file-name indexes.
    #[must_use]
    pub fn with_symbols(mut self, symbols: SymbolIndexes) -> Self {
        self.symbols = symbols;
        self
    }

    /// Sends every hook-reported `transcript_path` to the transcript tailer.
    #[must_use]
    pub fn with_transcript_hints(mut self, hints: Option<TranscriptHints>) -> Self {
        self.transcript_hints = hints;
        self
    }

    /// Runs the full check pipeline for one request.
    pub async fn handle(
        &self,
        request: Request<Incoming>,
        transport: Transport,
    ) -> Result<BusResponse, Infallible> {
        Ok(match self.check(request, transport).await {
            Ok(Reply::Accepted) => respond(StatusCode::ACCEPTED, "status", "accepted"),
            Ok(Reply::Registered(project_id)) => respond_registered(&project_id),
            Ok(Reply::Hook) => respond_json(StatusCode::OK, "{}".to_owned()),
            Ok(Reply::BlockHook(reason)) => respond_json(
                StatusCode::OK,
                serde_json::json!({"decision": "block", "reason": reason}).to_string(),
            ),
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
    ) -> Result<Reply, Rejection> {
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
    ) -> Result<Reply, Rejection> {
        if transport == Transport::Http {
            self.check_host(parts)?;
        }
        self.check_origin(parts)?;
        self.authorize(parts)?;
        let path = parts.uri.path();
        if path != EVENT_PATH && path != PROJECT_PATH && path != HOOK_PATH {
            return Err(Rejection::NOT_FOUND);
        }
        if parts.method != Method::POST {
            return Err(Rejection::METHOD_NOT_ALLOWED);
        }
        if path == HOOK_PATH {
            let body = read_capped_body(body, read, MAX_HOOK_BODY_BYTES).await?;
            let mapped = hook::map_hook(&body, now_ms(), &self.symbols);
            drop(body);
            if let (Some(path), Some(hints)) = (mapped.transcript_path, &self.transcript_hints) {
                hints.hint(path);
            }
            return Ok(self.queue_hook(mapped.action));
        }
        let body = read_capped_body(body, read, MAX_BODY_BYTES).await?;
        if path == PROJECT_PATH {
            return self.register(&body).await;
        }
        let event = TractionEvent::from_json(&body)
            .map_err(|error| Rejection::bad_request(error.code()))?;
        check_policy(&event)?;
        EventRow::check(&event).map_err(|error| Rejection::bad_request(error.code()))?;
        self.router
            .try_route(event)
            .map(|()| Reply::Accepted)
            .map_err(|_| Rejection::BUSY)
    }

    /// Queues what a Claude Code hook asked for, without waiting: the
    /// answer goes back to Claude as soon as this returns.
    fn queue_hook(&self, action: HookAction) -> Reply {
        let queued = match action {
            HookAction::Nothing => return Reply::Hook,
            // The answer does not depend on the session's state (a repeat
            // gives the same one), so it never waits for the Router.
            HookAction::Pause {
                command,
                session_id,
                at_ms,
            } => {
                let pause = Pause {
                    action: command.action,
                    harness: HarnessId::ClaudeCode,
                    session_id,
                    at_ms,
                };
                return match self.router.try_pause(pause) {
                    Ok(()) => Reply::BlockHook(command.reason),
                    Err(_) => {
                        log::warn("could not queue a Session Pause command: the Router is full");
                        Reply::BlockHook(BUSY_REASON)
                    }
                };
            }
            HookAction::Event(event) => self.router.try_route(*event),
            // The answer (or refusal) is not waited for: a hook never
            // blocks on a registration.
            HookAction::AddDirectory(path) => match registration_path(&path) {
                Some(path) => self.router.try_register(path).map(drop),
                None => return Reply::Hook,
            },
        };
        if queued.is_err() {
            let count = self.dropped_hooks.fetch_add(1, Ordering::Relaxed) + 1;
            log::warn_counted(
                "dropped a Claude Code hook event: the Project Router queue is full",
                count,
            );
        }
        Reply::Hook
    }

    /// `/v1/project`: asks the Router to register the body's path.
    async fn register(&self, body: &[u8]) -> Result<Reply, Rejection> {
        let registration: Registration =
            serde_json::from_slice(body).map_err(|_| Rejection::MALFORMED_REGISTRATION)?;
        let path = registration_path(&registration.path).ok_or(Rejection::INVALID_PATH)?;
        let answer = self
            .router
            .try_register(path)
            .map_err(|_| Rejection::BUSY)?;
        match tokio::time::timeout(REGISTER_TIMEOUT, answer).await {
            Ok(Ok(Ok(project_id))) => Ok(Reply::Registered(project_id)),
            Ok(Ok(Err(error))) => Err(Rejection::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                error.code(),
            )),
            Ok(Err(_)) | Err(_) => Err(Rejection::BUSY),
        }
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

/// What an external caller may send, on top of `traction-core` validation.
///
/// - Every `FILE_MUTATION` is refused (`external_file_mutation`): in v1 only
///   the daemon's own file watcher produces them, and it never uses the bus.
/// - `file_watcher` is daemon-internal (`internal_attribution_source`). Core
///   already allows it only on a `FILE_MUTATION`, so today this guards
///   against core loosening that rule.
/// - `browser_extension`, `editor_extension` and `manual_pick` are reserved
///   until their sub-step turns them on (`reserved_attribution_source`).
fn check_policy(event: &TractionEvent) -> Result<(), Rejection> {
    if event.event_type == EventType::FileMutation {
        return Err(Rejection::bad_request("external_file_mutation"));
    }
    match event.attribution_source {
        AttributionSource::HarnessHook
        | AttributionSource::HarnessPlugin
        | AttributionSource::HarnessLog
        | AttributionSource::HarnessDbPoll => Ok(()),
        AttributionSource::FileWatcher => {
            Err(Rejection::bad_request("internal_attribution_source"))
        }
        AttributionSource::BrowserExtension
        | AttributionSource::EditorExtension
        | AttributionSource::ManualPick => {
            Err(Rejection::bad_request("reserved_attribution_source"))
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

/// Reads the whole body, refusing it once it passes `limit` bytes. Adds
/// every byte read to `read`, so a refusal can drain the rest.
async fn read_capped_body(
    body: &mut Incoming,
    read: &mut usize,
    limit: usize,
) -> Result<Vec<u8>, Rejection> {
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Rejection::UNREADABLE_BODY)?;
        if let Some(data) = frame.data_ref() {
            *read += data.len();
            if *read > limit {
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

/// A registration path: absolute for this OS, bounded, no control chars.
fn registration_path(path: impl AsRef<Path>) -> Option<PathBuf> {
    let path = path.as_ref();
    let text = path.to_str()?;
    (path.is_absolute() && text.len() <= MAX_CWD_LEN && !text.chars().any(char::is_control))
        .then(|| path.to_path_buf())
}

/// `200 {"project_id":"<id>"}`. The ID is daemon-derived (`git:`/`dir:`
/// plus hex), never request input.
fn respond_registered(project_id: &str) -> BusResponse {
    respond_json(
        StatusCode::OK,
        format!("{{\"project_id\":\"{project_id}\"}}"),
    )
}

/// A JSON response `{"<key>":"<code>"}` built only from static strings.
fn respond(status: StatusCode, key: &'static str, code: &'static str) -> BusResponse {
    respond_json(status, format!("{{\"{key}\":\"{code}\"}}"))
}

/// A JSON response with `body`, which never holds request data.
fn respond_json(status: StatusCode, body: String) -> BusResponse {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}
