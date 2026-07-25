//! Cross-component end-to-end tests for BAE.
//!
//! This crate is a container for tests that span more than one component and
//! therefore cannot live inside any single one of them. It intentionally
//! exports nothing: all of the substance is under `tests/`, and every
//! dependency is a dev-dependency.
//!
//! # Why this crate exists
//!
//! `tests/telemetry.rs` proves W3C trace-context propagation joins client and
//! server spans into a single trace. Doing that honestly requires *both* the
//! real `baesrv` binary and the real `bae-rs` client SDK — if the test forged
//! the `traceparent` header itself it would be asserting its own header
//! construction rather than the shipped client's instrumentation.
//!
//! It used to live in `server/tests/`, which forced `server/` to carry a
//! `bae-rs` path dev-dependency. That inverted the component layering (the
//! client depends on the server's wire contract, not the reverse), broke the
//! "each component is independently buildable and testable" property in
//! `aspec/architecture/design.md` Principle 3, and — because Cargo resolves
//! dev-dependencies even for `cargo build --release` — made the production
//! Docker images fail to build unless `client-rust/` was in the build context.
//!
//! Putting the test in a leaf crate downstream of both components keeps the
//! coverage and removes the edge.
//!
//! # Running
//!
//! Use `make test-e2e` from the repository root (or `make test` in this
//! directory), which builds `baesrv` first and points the suite at it. See
//! `baesrv_binary()` in `tests/telemetry.rs` for how the binary is located.
