//! RFC 7807 problem-details error responses.
//!
//! Reuses the `{type, title, status, detail}` body shape `baectl/src/error.rs`
//! and `server/src/api/error.rs` already establish, so every bae-published
//! binary emits errors in one consistent format. `type` is a short stable slug
//! clients can match on; `detail` carries the specifics (a schema failure names
//! the failing path(s); a missing `${VAR}` names the variable).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use launcher_core::LauncherError;

/// An RFC 7807 problem response.
#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    type_slug: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
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

    /// A 400 for a body that is not valid JSON or fails its schema.
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Bad Request",
            detail,
        )
    }

    /// A 400 whose `detail` names every failing JSON Schema path — the child is
    /// never spawned. `failures` are pre-formatted `"<instance-path>: <msg>"`
    /// strings.
    pub fn schema_validation(failures: &[String]) -> Self {
        let detail = format!(
            "request body failed schema validation: {}",
            failures.join("; ")
        );
        Self::bad_request(detail)
    }

    /// A 401 for a missing/invalid bearer token on an authenticated route.
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Unauthorized",
            detail,
        )
    }

    /// A 404 for a trigger/introspection request naming an unknown agent.
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Not Found", detail)
    }

    /// A 500 for a spawn-time failure (an unset `${VAR}` secret, or a `command`
    /// that could not be started) — surfaced as an RFC 7807 body rather than a
    /// silent empty-string substitution or a launcher crash.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal Server Error",
            detail,
        )
    }

    /// Map a spawn-preparation [`LauncherError`] (an unset `${VAR}`, or a
    /// malformed `${...}` reference in static config) to a 500. Both are reported
    /// to the caller as an internal error: the request itself was well-formed;
    /// the *launcher's* environment/config could not satisfy it.
    pub fn from_launcher(e: &LauncherError) -> Self {
        Self::internal(format!("cannot start agent: {e}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = self.status_code;
        let mut response = (code, Json(self)).into_response();
        // RFC 7807 §3 defines its own media type for JSON problem details;
        // standards-aware clients key off it to recognize the body shape.
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_error_is_rfc7807_and_names_instance_paths() {
        let response = ApiError::schema_validation(&["/prompt: is required".to_string()]);
        let body = serde_json::to_value(&response).expect("serializable problem details");
        assert_eq!(body["type"], "bad_request");
        assert_eq!(body["status"], 400);
        assert!(body["detail"].as_str().unwrap().contains("/prompt"));
    }

    #[test]
    fn missing_env_error_is_internal_and_names_variable() {
        let response = ApiError::from_launcher(&LauncherError::MissingEnv {
            var: "SECRET_TOKEN".to_string(),
        });
        let body = serde_json::to_value(&response).expect("serializable problem details");
        assert_eq!(body["status"], 500);
        assert!(body["detail"].as_str().unwrap().contains("SECRET_TOKEN"));
    }
}
