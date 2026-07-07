//! Agent/session/run engine.
//!
//! - [`provider`] — provider (LLM) configuration, `${ENV_VAR}` resolution, and
//!   the outbound HTTP call.
//! - [`mcp`] — the MCP (Model Context Protocol) client: per-session connections
//!   to configured MCP servers, the `initialize`/`tools/list` handshake, and
//!   `tools/call` dispatch.
//! - [`session`] — the session message loop ([`session::run_turn`]): stream
//!   history, call the provider (with fallbacks), dispatch tool calls, and
//!   persist every step to `session_events`.

pub mod broadcast;
pub mod mcp;
pub mod provider;
pub mod session;
