//! `baectl ready <id>` — verify a built harness's profile/auth/registry/env are
//! compatible with the server, reporting each of the six checks ✓/⚠/✗ and (with
//! `--fix`) applying the safe #2/#4 admin-API creates after a single
//! confirmation.
//!
//! Exit codes: `0` all checks pass; `3` only auto-fixable (⚠) checks remain
//! (`run` or `ready --fix` resolves them); `1` a blocking (✗) check failed.
//!
//! The check logic itself lives in [`crate::harness::checks`] and is shared with
//! `run`; this module owns only the CLI-facing wiring: loading the build's
//! `manifest.json`, the `--dev` consistency guard, the report-vs-confirm mode
//! choice, and persisting `resolved.json` on full success.

use std::path::PathBuf;

use crate::error::CliError;
use crate::harness::artifact::{load_manifest, load_resolved, warn_dev_mismatch, write_resolved};
use crate::harness::checks::{evaluate, FixMode, Outcome};

/// The fully resolved inputs to a `ready` invocation, mirroring §3's flag set.
#[derive(Debug, Clone)]
pub struct ReadyOptions {
    /// The build id to check (its `<dir>/.baectl/builds/<id>/manifest.json`).
    pub id: String,
    /// `--fix` — apply the safe (#2/#4) admin-API creates after confirmation.
    pub fix: bool,
    /// `--dir` — the workspace dir holding `.baectl/` and the `setup` files
    /// (default `.`).
    pub dir: PathBuf,
    /// `--dev` — consistency guard against the build's recorded dev flag (§5).
    pub dev: bool,
}

/// Entry point for `baectl ready`.
pub fn ready(opts: ReadyOptions) -> Result<(), CliError> {
    let manifest = load_manifest(&opts.dir, &opts.id)?;
    warn_dev_mismatch(&manifest, opts.dev);

    let prior = load_resolved(&opts.dir, &opts.id);
    let mode = if opts.fix {
        FixMode::Prompt
    } else {
        FixMode::Report
    };

    match evaluate(&opts.dir, &manifest, prior.as_ref(), mode, None, None)? {
        Outcome::Ready(resolved) => {
            write_resolved(&opts.dir, &opts.id, &resolved)?;
            println!(
                "\nready: '{}' is good to run — `baectl run {}`",
                opts.id, opts.id
            );
            Ok(())
        }
        Outcome::Fixable => Err(CliError::fixable(fixable_message(&opts.id))),
        // A blocking check → exit 1, leaving no (or a stale) resolved.json.
        Outcome::Blocked if opts.fix => Err(CliError::runtime(
            "some checks are still unresolved (see above)",
        )),
        Outcome::Blocked => Err(CliError::runtime(format!(
            "some checks are blocking — follow the guidance above, then re-run `baectl ready {}`",
            opts.id
        ))),
    }
}

/// The exit-3 stderr message (after the `baectl: ` prefix).
pub(crate) fn fixable_message(id: &str) -> String {
    format!(
        "all remaining issues are auto-fixable — run `baectl run {id}` (fixes them without \
         prompting) or `baectl ready {id} --fix`"
    )
}
