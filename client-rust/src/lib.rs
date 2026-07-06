//! # bae-rs
//!
//! Rust client library and customizable harness for the Better Agent
//! Engine (BAE). The client is thin and stateless: all durable agent state
//! lives on the server, and this crate provides an idiomatic way to drive it.
//!
//! Planned modules (see `aspec/architecture/design.md`):
//!
//! - `client`  — typed HTTP client for the `/api/v1` surface
//! - `harness` — customizable agent loop built on the client
//! - `types`   — shared API request/response types

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
