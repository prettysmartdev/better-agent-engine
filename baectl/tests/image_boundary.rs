//! Regression (WI 0018 B1): `make image` builds baectl from a context holding
//! only `server/` and `baectl/` (see the root `Dockerfile`s), so baectl may not
//! depend — not even as a dev-dependency, which Cargo still resolves — on any
//! other directory of the repo. A `launcher-api` dev-dependency once broke the
//! image build this way; the launcher-config round trip now runs in the other
//! direction (`launchers/api/tests/baectl_generated.rs`).

use std::path::{Component, Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `path = …` dependency in `[dependencies]`, `[dev-dependencies]` and
/// `[build-dependencies]` (including target-specific tables).
fn path_dependencies(manifest: &toml::Value) -> Vec<(String, String)> {
    let mut tables = vec![manifest.clone()];
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        tables.extend(targets.values().cloned());
    }
    let mut out = Vec::new();
    for table in tables {
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = table.get(section).and_then(toml::Value::as_table) else {
                continue;
            };
            for (name, spec) in deps {
                if let Some(path) = spec.get("path").and_then(toml::Value::as_str) {
                    out.push((name.clone(), path.to_string()));
                }
            }
        }
    }
    out
}

/// Lexically resolve `rel` against `base` (no filesystem access).
fn normalize(base: &Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[test]
fn path_dependencies_stay_inside_the_image_build_context() {
    let dir = manifest_dir();
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let manifest: toml::Value = toml::from_str(&text).unwrap();
    let repo = dir.parent().unwrap();
    let allowed = [repo.join("server"), repo.join("baectl")];

    let deps = path_dependencies(&manifest);
    for (name, path) in &deps {
        let resolved = normalize(&dir, path);
        assert!(
            allowed.iter().any(|root| resolved.starts_with(root)),
            "baectl depends on `{name}` at {path} ({}), outside the server/ + baectl/ \
             context `make image` builds from",
            resolved.display()
        );
    }
    assert!(
        !deps.iter().any(|(name, _)| name.contains("launcher")),
        "no launcher crate may be a baectl dependency: {deps:?}"
    );
}

/// The lockfile must not pull a launcher crate in either (a stale lock would
/// make an offline image build try to resolve it).
#[test]
fn lockfile_has_no_launcher_package() {
    let lock = std::fs::read_to_string(manifest_dir().join("Cargo.lock")).unwrap();
    let lock: toml::Value = toml::from_str(&lock).unwrap();
    let names: Vec<&str> = lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.get("name").and_then(toml::Value::as_str))
        .collect();
    for forbidden in ["launcher-api", "launcher_api", "baeapi", "launcher-core"] {
        assert!(
            !names.contains(&forbidden),
            "Cargo.lock contains {forbidden}"
        );
    }
}

/// The checked-in generated configs the launchers/api guard loads must exist
/// where that guard reads them.
#[test]
fn generated_launcher_config_fixtures_exist_for_the_reverse_guard() {
    for name in ["bae-api.toml", "bae-app.toml"] {
        let path = manifest_dir().join("tests/fixtures/generated").join(name);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let _: toml::Value = toml::from_str(&text).unwrap();
    }
}
