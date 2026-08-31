//! Locating and reading/writing the per-build artifacts under
//! `<dir>/.baectl/builds/<id>/` that `ready` and `run` share: the `build`-written
//! `manifest.json`, and the secret-bearing `resolved.json` / `harness.env` these
//! two verbs own. Every file here that can hold a secret is written mode `0600`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::CliError;
use crate::harness::manifest::{BuildManifest, Resolved};

const BUILDS_DIR: &str = ".baectl/builds";

/// `<dir>/.baectl/builds/<id>/` — a build's artifact directory.
pub(crate) fn artifact_dir(dir: &Path, id: &str) -> PathBuf {
    dir.join(BUILDS_DIR).join(id)
}

/// Load a build's `manifest.json`. A missing one is a usage error pointing at
/// `baectl build` (the id was never built, or `--dir` is wrong).
pub(crate) fn load_manifest(dir: &Path, id: &str) -> Result<BuildManifest, CliError> {
    let path = artifact_dir(dir, id).join("manifest.json");
    let raw = fs::read_to_string(&path).map_err(|_| {
        CliError::usage(format!(
            "no build '{id}' found under {} — run `baectl build …` first (or check --dir)",
            dir.display()
        ))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        CliError::runtime(format!("build manifest {} is corrupt: {e}", path.display()))
    })
}

/// `<dir>/.baectl/builds/<id>/resolved.json`.
pub(crate) fn resolved_path(dir: &Path, id: &str) -> PathBuf {
    artifact_dir(dir, id).join("resolved.json")
}

/// Load an existing `resolved.json`, or `None` if absent/unreadable. A malformed
/// one is treated as absent — `ready`/`run` re-derive it from scratch.
pub(crate) fn load_resolved(dir: &Path, id: &str) -> Option<Resolved> {
    let raw = fs::read_to_string(resolved_path(dir, id)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write `resolved.json` at mode `0600` (it may carry a plaintext client key).
pub(crate) fn write_resolved(dir: &Path, id: &str, resolved: &Resolved) -> Result<(), CliError> {
    let raw = serde_json::to_string_pretty(resolved)
        .map_err(|e| CliError::runtime(format!("could not serialize resolved.json: {e}")))?;
    write_private(&resolved_path(dir, id), &format!("{raw}\n"))
}

/// `<dir>/.baectl/builds/<id>/harness.env`.
pub(crate) fn harness_env_path(dir: &Path, id: &str) -> PathBuf {
    artifact_dir(dir, id).join("harness.env")
}

/// Write a file with owner-only (`0600`) permissions, clamping the mode even
/// when overwriting a pre-existing (possibly looser) file. Mirrors
/// `cli.rs::write_secret`, the admin-key writer.
pub(crate) fn write_private(path: &Path, contents: &str) -> Result<(), CliError> {
    let map_err =
        |e: std::io::Error| CliError::runtime(format!("could not write {}: {e}", path.display()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(map_err)?;
        // `mode(0o600)` only applies on *creation*; clamp explicitly so an
        // overwrite of a looser pre-existing file is still tightened.
        f.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(map_err)?;
        f.write_all(contents.as_bytes()).map_err(map_err)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents).map_err(map_err)?;
    }
    Ok(())
}

/// Warn (never fail) when `--dev` disagrees with what the build recorded — the
/// §5 consistency guard. The artifact was fixed at build time; the flag does
/// nothing functional on `ready`/`run`, so a mismatch is only a likely-mistake
/// heuristic.
pub(crate) fn warn_dev_mismatch(manifest: &BuildManifest, dev_flag: bool) {
    if manifest.dev() != dev_flag {
        let (built, now) = if manifest.dev() {
            ("--dev", "without --dev")
        } else {
            ("without --dev", "--dev")
        };
        eprintln!(
            "baectl: warning — build '{}' was built {built} but you invoked this {now}; \
             the artifact is already fixed, so --dev has no functional effect here",
            manifest.id()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("baectl-artifact-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Regression — local state hygiene (offline half): `write_private` is the
    // sole writer behind both `resolved.json` (write_resolved) and
    // `harness.env` (run.rs), so proving it here covers both without a live
    // engine. The engine-gated integration suite separately re-proves this
    // against real build/ready/run-written files.
    #[cfg(unix)]
    #[test]
    fn write_private_sets_mode_0600_on_create_and_on_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("write-private");
        let path = dir.join("resolved.json");

        write_private(&path, "{}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // A pre-existing looser file must be clamped back to 0600, not left as
        // whatever mode it already had.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "{\"k\":1}").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"k\":1}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_resolved_writes_through_write_private_at_the_documented_path() {
        let dir = temp_dir("write-resolved");
        let resolved = Resolved {
            profile_id: "pro_1".to_string(),
            profile_name: "default".to_string(),
            key_id: "key_1".to_string(),
            client_key_plaintext: Some("bae_secret".to_string()),
            server_url: "http://localhost:8080".to_string(),
            max_url: None,
            provider_env: None,
        };
        fs::create_dir_all(artifact_dir(&dir, "ref-rust-local")).unwrap();
        write_resolved(&dir, "ref-rust-local", &resolved).unwrap();
        let path = resolved_path(&dir, "ref-rust-local");
        assert_eq!(
            path,
            artifact_dir(&dir, "ref-rust-local").join("resolved.json")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let loaded = load_resolved(&dir, "ref-rust-local").expect("just-written resolved.json");
        assert_eq!(loaded, resolved);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_manifest_missing_is_a_usage_error_pointing_at_build() {
        let dir = temp_dir("load-manifest-missing");
        let err = load_manifest(&dir, "does-not-exist").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().contains("baectl build"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_resolved_treats_a_malformed_file_as_absent() {
        let dir = temp_dir("load-resolved-malformed");
        let path = resolved_path(&dir, "ref-rust-local");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        assert!(load_resolved(&dir, "ref-rust-local").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    // Regression — local state hygiene: `.baectl/` (this module's BUILDS_DIR
    // parent) must be covered by the repo-root .gitignore, offline and without
    // relying on the engine-gated integration suite ever running.
    #[test]
    fn gitignore_covers_the_baectl_local_state_directory() {
        let gitignore = include_str!("../../../.gitignore");
        assert!(
            gitignore.lines().any(|l| l.trim() == ".baectl/"),
            ".gitignore must list `.baectl/` so a plaintext client key/secret \
             file is never accidentally committed"
        );
    }
}
