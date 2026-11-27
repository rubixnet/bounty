//! Every non-2xx answer is `{"error": "<code>"}` (`traction_core::sync::ErrorBody`).

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use traction_core::sync::ErrorBody;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub retry_after_sec: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            retry_after_sec: None,
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    pub fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }

    pub fn unprocessable(code: &'static str) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code)
    }

    pub fn too_many(retry_after_sec: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            retry_after_sec: Some(retry_after_sec),
        }
    }

    /// A database failure. The error text is logged, never returned.
    pub fn database(error: &sqlx::Error) -> Self {
        eprintln!("traction-server: database error: {error}");
        if error.to_string().contains("no partition of relation") {
            return Self::new(StatusCode::INTERNAL_SERVER_ERROR, "no_partition");
        }
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal")
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::database(&error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorBody {
                error: self.code.into(),
            }),
        )
            .into_response();
        if let Some(seconds) = self.retry_after_sec
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}
