//! The `build` / `ready` / `run` verbs and the manifest types they act on.
//!
//! These three verbs turn "a running BAE server" (produced by `setup`) plus
//! "some harness code" into "a running, wired-up agent." Like `setup`, they are
//! **host-invoked** — they need `cargo`/`npm`/`uv`/`docker` and drive the local
//! container engine from outside via the shared [`crate::engine`] plumbing;
//! they never link an admin client against the loopback-only admin port.
//!
//! Module layout (each verb owns its own file so later workflow steps can own
//! one without colliding):
//! - [`manifest`] — the `bae-harness.toml` / `manifest.json` / `resolved.json`
//!   types, their parsers, name/id validation, and id derivation.
//! - [`build`] / [`ready`] / [`run`] — the verb entry points. Their option
//!   structs are defined; the behavior is placeholder pending later steps.

pub mod artifact;
pub mod build;
pub mod checks;
pub mod manifest;
pub mod ready;
pub mod run;

pub use build::{build, BuildOptions};
pub use manifest::{
    derive_id, load_harness_manifest, parse_harness_manifest, BuildManifest, ContainerManifest,
    ContainerOverrides, Harness, HarnessManifest, Launcher, LauncherConfig, LocalManifest,
    Requires, Resolved, Sdk,
};
pub use ready::{ready, ReadyOptions};
pub use run::{run, RunOptions};
