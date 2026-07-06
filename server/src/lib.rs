//! Better Agent Engine (BAE) — server library.
//!
//! The binary in `main.rs` is a thin entrypoint; all server logic lives here
//! so it stays unit-testable. Planned modules (see `aspec/architecture/design.md`):
//!
//! - `api`   — HTTP surface (`/api/v1`)
//! - `store` — SQLite persistence and migrations
//! - `engine`— agent/session/run state machine

/// Server version, from the crate manifest.
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
