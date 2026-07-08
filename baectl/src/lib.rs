//! `baectl` — a small HTTP/REST wrapper over the BAE **admin** API
//! (`/admin/v1/*`, served on `BAE_ADMIN_ADDR`, default `127.0.0.1:8081`).
//!
//! It is bundled into both the dev and production images so an operator can run
//! `docker exec bae baectl create profile …` instead of hand-assembling a curl
//! command with a JSON body, and it auto-configures itself (address + admin
//! token) with zero flags when run the documented way — via `docker exec` /
//! `container exec` inside the same container as `baesrv`.
//!
//! Module layout mirrors the work item's spec:
//! - [`cli`] — the clap derive command tree and the runner that dispatches it.
//! - [`admin_client`] — a `reqwest`-based wrapper over `/admin/v1/*` with typed
//!   request/response bodies mirroring `docs/reference/admin-api.md`.
//! - [`output`] — human-readable vs `--json` rendering.
//! - [`error`] — maps RFC 7807 error bodies and transport failures to exit
//!   codes (0 success / 1 runtime / 2 usage).
//!
//! `baectl` deliberately does **not** depend on `client-rust`/`bae-rs`: that
//! crate is the client-port session harness (tool dispatch, hooks, the agent
//! loop); baectl only talks to the admin port and shares none of those
//! concerns, so it carries its own minimal admin HTTP client instead.

pub mod admin_client;
pub mod cli;
pub mod error;
pub mod keygen;
pub mod output;
