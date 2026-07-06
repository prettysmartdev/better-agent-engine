//! Agent/session/run engine.
//!
//! - [`provider`] — provider (LLM) configuration, `${ENV_VAR}` resolution, and
//!   the outbound HTTP call.
//! - [`session`] — the session message loop ([`session::run_turn`]): stream
//!   history, call the provider (with fallbacks), dispatch tool calls, and
//!   persist every step to `session_events`.

pub mod provider;
pub mod session;
