//! API error responses.
//!
//! Every error returned by either router is an [`ApiError`], serialized as the
//! RFC 7807-style body `aspec/architecture/apis.md` mandates:
//! `{type, title, status, detail}`. The `type` is a short stable slug (not a
//! URL) that clients can match on; `title` is a human summary; `detail` carries
//! the specifics.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// A problem response. Construct via the helper constructors so the `type` slugs
/// stay consistent across handlers.
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub type_slug: &'static str,
    pub title: &'static str,
    pub status: u16,
    pub detail: String,
    #[serde(skip)]
    status_code: StatusCode,
}

impl ApiError {
    fn new(
        status: StatusCode,
        type_slug: &'static str,
        title: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        ApiError {
            type_slug,
            title,
            status: status.as_u16(),
            detail: detail.into(),
            status_code: status,
        }
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Bad Request",
            detail,
        )
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Unauthorized",
            detail,
        )
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Not Found", detail)
    }

    pub fn conflict(type_slug: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, type_slug, "Conflict", detail)
    }

    pub fn forbidden(type_slug: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, type_slug, "Forbidden", detail)
    }

    /// A domain-specific error at 422 Unprocessable Entity (e.g. an unusable
    /// profile), with a caller-chosen `type` slug.
    pub fn unprocessable(type_slug: &'static str, detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            type_slug,
            "Unprocessable Entity",
            detail,
        )
    }

    /// A provider (upstream) failure, surfaced as 502 Bad Gateway.
    pub fn bad_gateway(type_slug: &'static str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, type_slug, "Bad Gateway", detail)
    }

    /// An unexpected server-side failure. The `detail` is logged, not leaked, by
    /// callers that build this from an internal error.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal Server Error",
            detail,
        )
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.title, self.status, self.detail)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.status_code;
        (code, Json(self)).into_response()
    }
}

/// Convenience: turn a `rusqlite::Error` into a logged 500. Use in handlers via
/// `.map_err(ApiError::from_db)`.
impl ApiError {
    pub fn from_db(e: rusqlite::Error) -> Self {
        tracing::error!("database error: {e}");
        ApiError::internal("a database error occurred")
    }
}
