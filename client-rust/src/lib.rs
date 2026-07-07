//! # bae-rs
//!
//! Rust client library and customizable **agent harness** for the Better Agent
//! Engine (BAE). The client is thin and stateless — all durable agent state
//! lives on the server (`/api/v1`); this crate gives Rust programs an
//! idiomatic way to drive it.
//!
//! This is an agent harness, not a bare REST wrapper. The core object is a
//! [`Harness`]: give it a [`Config`], register [`Tool`]s and optional
//! [`Hooks`], then [`Harness::connect`] to open a [`Session`]. Each
//! [`Session::send`] drives the full tool-dispatch round-trip until the model
//! returns a final answer.
//!
//! ```no_run
//! use bae_rs::{Config, Harness, Tool};
//! use serde_json::json;
//!
//! # async fn run() -> Result<(), bae_rs::Error> {
//! let config = Config::new("http://localhost:8080", std::env::var("BAE_CLIENT_KEY").unwrap());
//!
//! let get_time = Tool::new(
//!     "get_current_time",
//!     "Return the current time as an ISO-8601 string",
//!     json!({ "type": "object", "properties": {} }),
//!     |_input| Ok(json!("2026-07-06T12:00:00Z")),
//! );
//!
//! let mut session = Harness::new(config).with_tool(get_time).connect().await?;
//! let reply = session.send("What time is it?").await?;
//! println!("{}", reply.text());
//! session.close().await?;
//! # Ok(()) }
//! ```
//!
//! ## The five surface pieces
//!
//! 1. [`Config`] — server URL, client key, client version.
//! 2. [`Tool`] — name, description, JSON input schema, and a callable handler.
//! 3. [`Harness`] — config + tool registry + hooks; `connect()` opens a session.
//! 4. [`Session`] — `send(message)` drives the round-trip; `close()` ends it.
//! 5. [`Hooks`] — optional `before_send` / `after_receive` / `before_tool_call`
//!    / `after_tool_call` / `on_event` callbacks; an error from any aborts the
//!    loop.
//!
//! The message loop rides JSON-RPC 2.0 over `POST …/rpc` (session open, events
//! replay, and close stay plain REST); [`Session::subscribe`] taps the same
//! live `session.event` stream out-of-band.

mod config;
mod error;
mod harness;
mod hooks;
mod tool;
mod types;

pub use config::Config;
pub use error::Error;
pub use harness::{Harness, Session};
pub use hooks::{HookResult, Hooks};
pub use tool::{BoxError, Tool, ToolHandler};
pub use types::{
    ApiError, Content, ContentBlock, EventView, JsonRpcError, JsonRpcFrame, JsonRpcRequest,
    McpRequestPayload, McpResponsePayload, Message, Profile, SendMessageParams, SendMessageResult,
    SubscribeParams, ToolResult, ToolUse,
};

/// Client library version, from the crate manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_manifest() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }
}
