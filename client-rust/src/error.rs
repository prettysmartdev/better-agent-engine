//! The crate's error type.

use crate::tool::BoxError;
use crate::types::{ApiError, EventView};

/// Everything that can go wrong driving a session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport-level failure (connection refused, timeout, malformed body).
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    /// The server returned an RFC 7807 problem document (any non-2xx that
    /// isn't the special 502 providers-failed case).
    #[error("API error: {0}")]
    Api(ApiError),

    /// `POST …/messages` returned `502`: every provider (primary + fallbacks)
    /// failed server-side. The session is now in the `error` state. `events`
    /// carries the events appended during the call — inspect the
    /// `provider.response` failures for the cause (e.g. an unset provider key).
    #[error("all providers failed (502); session is now in the error state")]
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

    /// A response body could not be decoded into the expected shape.
    #[error("failed to decode server response: {0}")]
    Decode(#[from] serde_json::Error),
}
