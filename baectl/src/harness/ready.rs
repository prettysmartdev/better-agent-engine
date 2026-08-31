//! `baectl ready <id>` — verify a built harness's profile/auth/registry/env are
//! compatible with the server, reporting each of the six checks ✓/✗ and (with
//! `--fix`) applying the safe #2/#4 admin-API creates after a single
//! confirmation.
//!
//! The check logic itself lives in [`crate::harness::checks`] and is shared with
//! `run`; this module owns only the CLI-facing wiring: loading the build's
//! `manifest.json`, the `--dev` consistency guard, the report-vs-confirm mode
//! choice, and persisting `resolved.json` on full success.

use std::path::PathBuf;

use crate::error::CliError;
use crate::harness::artifact::{load_manifest, load_resolved, warn_dev_mismatch, write_resolved};
use crate::harness::checks::{evaluate, FixMode};

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

    match evaluate(&opts.dir, &manifest, prior.as_ref(), mode)? {
        Some(resolved) => {
            write_resolved(&opts.dir, &opts.id, &resolved)?;
            println!(
                "\nready: '{}' is good to run — `baectl run {}`",
                opts.id, opts.id
            );
            Ok(())
        }
        None => {
            // An unresolved check → exit 1, leaving no (or a stale) resolved.json.
            let hint = if opts.fix {
                "some checks are still unresolved (see above)"
            } else {
                "some checks failed — re-run with --fix to apply the safe fixes, or follow the \
                 guidance above"
            };
            Err(CliError::runtime(hint))
        }
    }
}
