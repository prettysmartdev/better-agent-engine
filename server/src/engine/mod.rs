//! Agent/session/run engine.
//!
//! - [`provider`] — provider (LLM) configuration, `${ENV_VAR}` resolution, and
//!   the outbound HTTP call.
//! - [`mcp`] — the MCP (Model Context Protocol) client: per-session connections
//!   to configured MCP servers, the `initialize`/`tools/list` handshake, and
//!   `tools/call` dispatch.
//! - [`sandbox`] — the sandbox drivers ([`sandbox::SandboxDriver`]): Docker /
//!   Apple Containers backed shell-execution sandboxes, provisioned per
//!   profile and started per session.
//! - [`session`] — the session message loop ([`session::run_turn`]): stream
//!   history, call the provider (with fallbacks), dispatch tool calls, and
//!   persist every step to `session_events`.

pub mod broadcast;
pub mod mcp;
pub mod provider;
pub mod sandbox;
pub mod session;
