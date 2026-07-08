//! The crate's error type.

use crate::tool::BoxError;
use crate::types::{ApiError, EventView};

/// Everything that can go wrong driving a session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level failure (connection refused, timeout, malformed body).
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    /// The server returned an RFC 7807 problem document on a REST route, or as
    /// the pre-stream HTTP status of a `/rpc` call (e.g. `401` auth failure,
    /// checked before the NDJSON body opens).
    #[error("API error: {0}")]
    Api(ApiError),

    /// The `/rpc` stream carried a JSON-RPC 2.0 error object (HTTP was still
    /// `200`). Reserved for `-32700` parse / `-32600` invalid-request /
    /// `-32601` method-not-found / `-32602` invalid-params / `-32603` internal
    /// / `-32000` application errors (session-not-open,
    /// profile-unavailable-mid-session, `lagged`). Distinct from
    /// [`Error::Api`], which is a pre-stream HTTP/RFC-7807 failure.
    #[error("JSON-RPC error {code}: {message}")]
    Rpc {
        /// The JSON-RPC error code.
        code: i64,
        /// The JSON-RPC error message.
        message: String,
    },

    /// Every provider (primary + fallbacks) failed server-side during a
    /// `session.sendMessage` turn. The session is now in the `error` state.
    ///
    /// The `/rpc` loop delivers this as a normal terminal `result` (the generic
    /// "provider unavailable" turn), **not** a 502 or a JSON-RPC error; the
    /// harness recognises the `session.error`/`all_providers_failed` event in
    /// the turn's `events` and surfaces it here for API continuity. Inspect the
    /// `provider.response` failures in `events` for the cause (e.g. an unset
    /// provider key).
    #[error("all providers failed; session is now in the error state")]
    ProvidersFailed {
        /// Events appended during the failed call, in order.
        events: Vec<EventView>,
    },

    /// The model asked to call a tool the harness never registered. Indicates
    /// the declared tool set and the profile allowlist are out of sync.
    #[error("server requested unknown tool '{0}' not in the registry")]
    UnknownTool(String),

    /// A registered tool handler returned an error. Aborts the loop.
    #[error("tool '{name}' handler failed: {source}")]
    Tool {
        /// The tool that failed.
        name: String,
        /// The handler's error.
        source: BoxError,
    },

    /// A lifecycle hook returned an error, aborting the loop.
    #[error("hook aborted the loop: {0}")]
    Hook(BoxError),

    /// A sandbox operation failed: a local container-engine (`docker`/
    /// `container`) call errored, or a sandbox tool was used before its
    /// [`Session`](crate::Session) was connected (so the transport it needs for
    /// `session.execRemoteSandbox` / `session.reportLocalSandbox` is unbound).
    #[error("sandbox error: {0}")]
    Sandbox(String),

    /// A response body could not be decoded into the expected shape.
    #[error("failed to decode server response: {0}")]
    Decode(#[from] serde_json::Error),
}
