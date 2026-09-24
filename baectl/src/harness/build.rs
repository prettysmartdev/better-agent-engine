//! `baectl build <harness>` — package a harness into a local or launcher image
//! artifact.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::{detect_engine, EngineKind};
use crate::error::CliError;
use crate::harness::manifest::{
    derive_id, load_harness_manifest, validate_derived_id, validate_explicit_id,
    validate_harness_name, BuildManifest, ContainerManifest, ContainerOverrides, Harness, Launcher,
    LauncherConfig, LocalManifest, Sdk,
};

const BUILDS_DIR: &str = ".baectl/builds";
const PUBLISHED_IMAGE_PREFIX: &str = "ghcr.io/prettysmartdev/better-agent-engine";
const DEV_IMAGE_PREFIX: &str = "better-agent-engine";

/// The fully resolved inputs to a `build` invocation, mirroring §2's flag set.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// The `<harness>` positional: a bundled example name (`issue-triage` /
    /// `reference-assistant`) or, with `harness_dir`, a directory name is not
    /// used for resolution — the manifest's own `name` is.
    pub harness: String,
    /// `--sdk` — the SDK directory a bundled example is resolved under
    /// (default `rust`).
    pub sdk: Sdk,
    /// `--harness-dir` — an arbitrary directory holding its own
    /// `bae-harness.toml`; when set, no repo checkout is required.
    pub harness_dir: Option<PathBuf>,
    /// `--launcher` — `local` (default), `schedule`, `api`, or `webapp`.
    pub launcher: Launcher,
    /// `--id` — an explicit build id (overrides the derived default).
    pub id: Option<String>,
    /// `--dev` — use locally built images/binaries throughout.
    pub dev: bool,
    /// `--dir` — the workspace dir holding `.baectl/` and the `setup` files
    /// (default `.`).
    pub dir: PathBuf,
}

/// Build a harness and record the resulting disposable local artifact.
pub fn build(opts: BuildOptions) -> Result<(), CliError> {
    let dir = absolute_existing_dir(&opts.dir, "--dir")?;
    let harness_dir = resolve_harness_dir(&opts, &dir)?;
    let harness_manifest = load_harness_manifest(&harness_dir.join("bae-harness.toml"))?;
    let harness = &harness_manifest.harness;
    validate_harness_name(&harness.name)?;
    if let Some(id) = opts.id.as_deref() {
        validate_explicit_id(id)?;
    }

    if opts.launcher.is_container() && harness.launcher.is_none() {
        return Err(CliError::usage(format!(
            "harness '{}' has no [harness.launcher] section; --launcher {} requires it",
            harness.name, opts.launcher
        )));
    }

    let builds_dir = dir.join(BUILDS_DIR);
    fs::create_dir_all(&builds_dir).map_err(|e| {
        CliError::runtime(format!(
            "could not create build artifact directory {}: {e}",
            builds_dir.display()
        ))
    })?;
    let id = resolve_id(&builds_dir, harness, opts.launcher, opts.id.as_deref())?;
    if opts.id.is_none() {
        validate_derived_id(&id)?;
    }
    let artifact_dir = builds_dir.join(&id);
    let rebuilt = artifact_dir.exists();
    fs::create_dir_all(&artifact_dir).map_err(|e| {
        CliError::runtime(format!(
            "could not create build artifact directory {}: {e}",
            artifact_dir.display()
        ))
    })?;

    // A rebuilt artifact needs readiness to be established again; these may
    // contain a plaintext client key or harness secrets from a previous image.
    remove_if_exists(&artifact_dir.join("resolved.json"))?;
    remove_if_exists(&artifact_dir.join("harness.env"))?;

    let manifest = match opts.launcher {
        Launcher::Local => build_local(&artifact_dir, &id, &harness_dir, harness, opts.dev)?,
        launcher => build_container(
            &dir,
            &artifact_dir,
            &id,
            &harness_dir,
            harness,
            launcher,
            opts.dev,
        )?,
    };
    write_manifest(&artifact_dir.join("manifest.json"), &manifest)?;

    if rebuilt {
        println!("rebuilt `{id}`");
    } else {
        println!("built `{id}`");
    }
    println!("next: baectl ready {id}");
    Ok(())
}

fn build_local(
    artifact_dir: &Path,
    id: &str,
    harness_dir: &Path,
    harness: &Harness,
    dev: bool,
) -> Result<BuildManifest, CliError> {
    // A previous container build may have left these inspection files behind
    // under the same explicit id. Local builds deliberately have no Docker
    // artifacts.
    for file in [
        "Dockerfile.build.generated",
        "Dockerfile",
        "bae-schedules.toml",
        "bae-api.toml",
        "bae-app.toml",
    ] {
        remove_if_exists(&artifact_dir.join(file))?;
    }

    // Local harnesses build lazily when `run` executes the manifest command.
    // Running that command here would launch the agent before `ready` has
    // resolved BAE_SERVER_URL/BAE_CLIENT_KEY (and, for the bundled examples,
    // would make `build` perform a real provider turn). There is no
    // host/container compatibility boundary in local mode, so recording the
    // command is the complete build artifact.
    Ok(BuildManifest::Local(LocalManifest {
        id: id.to_string(),
        name: harness.name.clone(),
        sdk: harness.sdk,
        dev,
        harness_dir: harness_dir.to_path_buf(),
        run_command: harness.run.clone(),
        working_dir: harness.working_dir.clone(),
        prepare: harness.prepare.clone(),
        requires: harness.requires.clone(),
        created_at: rfc3339_now(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn build_container(
    dir: &Path,
    artifact_dir: &Path,
    id: &str,
    harness_dir: &Path,
    harness: &Harness,
    launcher: Launcher,
    dev: bool,
) -> Result<BuildManifest, CliError> {
    let launcher_config = harness.launcher.as_ref().ok_or_else(|| {
        CliError::usage(format!(
            "harness '{}' has no [harness.launcher] section",
            harness.name
        ))
    })?;
    let engine = detect_engine(dir).unwrap_or(EngineKind::Docker);
    let build_image = format!("{id}-harness-build:latest");
    let image_tag = format!("{id}:latest");

    let generated_build_dockerfile = launcher_config.dockerfile.is_none();
    // Bundled manifests live beneath their SDK project roots. Their
    // `working_dir` is therefore also the source/build context for a generated
    // Dockerfile; an author-supplied Dockerfile keeps the documented harness
    // directory context exactly as written.
    let build_context = if generated_build_dockerfile {
        absolute_existing_dir(
            &harness_dir.join(&harness.working_dir),
            "harness working_dir",
        )?
    } else {
        harness_dir.to_path_buf()
    };
    let overrides = harness.container.clone().unwrap_or_default();
    if generated_build_dockerfile {
        validate_overrides(&overrides)?;
        // Docker only honours `<context>/.dockerignore`; without one, the
        // generated `COPY . .` ships host `target/`/`node_modules`/`.venv` into
        // the build stage (and risks a host-linked binary in the image).
        ensure_dockerignore(&build_context, harness.sdk)?;
    } else if harness.container.is_some() {
        eprintln!(
            "baectl: warning: [harness.container] is ignored because \
             [harness.launcher].dockerfile is set (that Dockerfile owns the build)"
        );
    }
    let build_dockerfile = resolve_build_dockerfile(
        harness_dir,
        artifact_dir,
        &launcher_config.dockerfile,
        harness.sdk,
        &harness.name,
        &overrides,
    )?;

    let mut harness_build_args = vec![
        OsString::from("build"),
        OsString::from("-f"),
        build_dockerfile.into_os_string(),
    ];
    if let Some(target) = &launcher_config.target {
        harness_build_args.push(OsString::from("--target"));
        harness_build_args.push(OsString::from(target));
    }
    harness_build_args.push(OsString::from("-t"));
    harness_build_args.push(OsString::from(&build_image));
    harness_build_args.push(build_context.as_os_str().to_os_string());
    run_engine_command(engine, &harness_build_args, &build_context)?;

    let config_file = launcher_config_filename(launcher)?;
    let binary_path = match &launcher_config.binary_path {
        Some(path) => path.clone(),
        None if generated_build_dockerfile => {
            generated_binary_path(harness.sdk, &harness.name, &overrides)
        }
        None => {
            return Err(CliError::usage(
                "[harness.launcher].binary_path is required when dockerfile is set",
            ));
        }
    };
    let dockerfile = launcher_dockerfile(
        launcher_base_image(launcher, dev)?,
        &build_image,
        &binary_path,
        &harness.name,
        config_file,
        generated_build_dockerfile.then_some(harness.sdk),
    );
    fs::write(artifact_dir.join("Dockerfile"), dockerfile).map_err(|e| {
        CliError::runtime(format!(
            "could not write launcher Dockerfile in {}: {e}",
            artifact_dir.display()
        ))
    })?;
    fs::write(
        artifact_dir.join(config_file),
        launcher_config_toml(launcher, &harness.name, launcher_config)?,
    )
    .map_err(|e| {
        CliError::runtime(format!(
            "could not write launcher configuration in {}: {e}",
            artifact_dir.display()
        ))
    })?;

    let final_build_args = vec![
        OsString::from("build"),
        OsString::from("-t"),
        OsString::from(&image_tag),
        artifact_dir.as_os_str().to_os_string(),
    ];
    run_engine_command(engine, &final_build_args, artifact_dir)?;

    Ok(BuildManifest::Container(ContainerManifest {
        id: id.to_string(),
        name: harness.name.clone(),
        sdk: harness.sdk,
        dev,
        launcher_type: launcher,
        image_tag,
        harness_build_image: build_image,
        port: match launcher {
            Launcher::Schedule => None,
            Launcher::Api | Launcher::Webapp => Some(9090),
            Launcher::Local => unreachable!("container builder is not called for local"),
        },
        requires: harness.requires.clone(),
        created_at: rfc3339_now(),
    }))
}

/// Resolve which build Dockerfile a container build uses: a harness-supplied
/// `[harness.launcher].dockerfile`, used verbatim (any stale generated file
/// from a prior build is removed), or — when absent — baectl's synthesized
/// per-SDK default, written to `Dockerfile.build.generated` and used instead.
/// Pure file IO only; the caller runs the actual container engine build
/// separately.
fn resolve_build_dockerfile(
    harness_dir: &Path,
    artifact_dir: &Path,
    dockerfile: &Option<String>,
    sdk: Sdk,
    name: &str,
    overrides: &ContainerOverrides,
) -> Result<PathBuf, CliError> {
    match dockerfile {
        Some(path) => {
            remove_if_exists(&artifact_dir.join("Dockerfile.build.generated"))?;
            let path = harness_dir.join(path);
            if !path.is_file() {
                return Err(CliError::usage(format!(
                    "could not find [harness.launcher].dockerfile at {}",
                    path.display()
                )));
            }
            Ok(path)
        }
        None => {
            let path = artifact_dir.join("Dockerfile.build.generated");
            fs::write(&path, generated_build_dockerfile(sdk, name, overrides)).map_err(|e| {
                CliError::runtime(format!(
                    "could not write generated build Dockerfile {}: {e}",
                    path.display()
                ))
            })?;
            Ok(path)
        }
    }
}

fn resolve_harness_dir(opts: &BuildOptions, dir: &Path) -> Result<PathBuf, CliError> {
    if let Some(path) = &opts.harness_dir {
        return absolute_existing_dir(path, "--harness-dir");
    }

    let client_dir = dir.join(format!("client-{}", opts.sdk.as_str()));
    if !client_dir.is_dir() {
        return Err(CliError::usage(format!(
            "could not resolve bundled harness '{}': client-{}/ was not found under {}; \
             use --harness-dir <path> for an external harness",
            opts.harness,
            opts.sdk.as_str(),
            dir.display()
        )));
    }
    let harness_dir = client_dir.join("examples").join(&opts.harness);
    absolute_existing_dir(&harness_dir, "bundled harness directory")
}

fn absolute_existing_dir(path: &Path, label: &str) -> Result<PathBuf, CliError> {
    path.canonicalize()
        .map_err(|e| CliError::usage(format!("could not resolve {label} {}: {e}", path.display())))
        .and_then(|path| {
            if path.is_dir() {
                Ok(path)
            } else {
                Err(CliError::usage(format!(
                    "{label} {} is not a directory",
                    path.display()
                )))
            }
        })
}

fn resolve_id(
    builds_dir: &Path,
    harness: &Harness,
    launcher: Launcher,
    explicit: Option<&str>,
) -> Result<String, CliError> {
    let mut existing = Vec::new();
    let entries = fs::read_dir(builds_dir).map_err(|e| {
        CliError::runtime(format!(
            "could not inspect existing build artifacts in {}: {e}",
            builds_dir.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            CliError::runtime(format!(
                "could not inspect existing build artifacts in {}: {e}",
                builds_dir.display()
            ))
        })?;
        if !entry.path().is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let manifest_path = entry.path().join("manifest.json");
        let same_combo = fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<BuildManifest>(&raw).ok())
            .is_some_and(|manifest| same_build_combo(&manifest, harness, launcher));
        if !same_combo {
            existing.push(id);
        }
    }
    Ok(derive_id(
        &harness.name,
        harness.sdk,
        launcher,
        explicit,
        &existing,
    ))
}

fn same_build_combo(manifest: &BuildManifest, harness: &Harness, launcher: Launcher) -> bool {
    match manifest {
        BuildManifest::Local(local) => {
            launcher == Launcher::Local && local.name == harness.name && local.sdk == harness.sdk
        }
        BuildManifest::Container(container) => {
            container.launcher_type == launcher
                && container.name == harness.name
                && container.sdk == harness.sdk
        }
    }
}

/// The build-context excludes `build` writes when a generated build's context
/// has no `.dockerignore` of its own. The same list is committed as
/// `client-{rust,typescript,python}/.dockerignore`.
pub(crate) const DOCKERIGNORE: &str =
    "target/\nnode_modules/\n.venv/\n__pycache__/\n.baectl/\n.git/\n";

/// The host directory whose absence from `.dockerignore` matters most for each
/// SDK: its build output / installed dependencies, which `COPY . .` would
/// otherwise ship into the build stage.
fn sdk_host_artifact_dir(sdk: Sdk) -> &'static str {
    match sdk {
        Sdk::Rust => "target",
        Sdk::Typescript => "node_modules",
        Sdk::Python => ".venv",
    }
}

/// Create `<context>/.dockerignore` with [`DOCKERIGNORE`] when absent; leave an
/// existing one untouched (warning when it does not exclude the SDK's host
/// artifact directory — `target/`, `node_modules/` or `.venv/`).
/// Returns whether a file was written.
pub(crate) fn ensure_dockerignore(context: &Path, sdk: Sdk) -> Result<bool, CliError> {
    let path = context.join(".dockerignore");
    match fs::read_to_string(&path) {
        Ok(existing) => {
            let dir = sdk_host_artifact_dir(sdk);
            if !dockerignore_excludes(&existing, dir) {
                eprintln!(
                    "baectl: warning: {} does not exclude {dir}/ — host build output will be \
                     sent to the image build",
                    path.display()
                );
            }
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::write(&path, DOCKERIGNORE).map_err(|e| {
                CliError::runtime(format!("could not write {}: {e}", path.display()))
            })?;
            println!("wrote {} (build-context excludes)", path.display());
            Ok(true)
        }
        Err(e) => Err(CliError::runtime(format!(
            "could not read {}: {e}",
            path.display()
        ))),
    }
}

/// Whether a `.dockerignore` body has a pattern excluding a top-level (or any)
/// directory called `dir`.
fn dockerignore_excludes(text: &str, dir: &str) -> bool {
    text.lines().any(|line| {
        let pattern = line.trim().trim_start_matches('/').trim_end_matches('/');
        pattern == "*"
            || pattern == dir
            || pattern.strip_prefix("**/") == Some(dir)
            || pattern.strip_suffix("/**") == Some(dir)
    })
}

/// `[harness.container]` values are spliced into single Dockerfile lines, so
/// they must be single-line.
fn validate_overrides(overrides: &ContainerOverrides) -> Result<(), CliError> {
    for (field, value) in [
        ("build", &overrides.build),
        ("entrypoint", &overrides.entrypoint),
    ] {
        if let Some(v) = value {
            if v.trim().is_empty() || v.contains('\n') || v.contains('\r') {
                return Err(CliError::usage(format!(
                    "[harness.container].{field} must be a non-empty single-line command"
                )));
            }
        }
    }
    Ok(())
}

fn run_engine_command(
    engine: EngineKind,
    args: &[OsString],
    working_dir: &Path,
) -> Result<(), CliError> {
    let engine_name = match engine {
        EngineKind::Docker => "docker",
        EngineKind::Apple => "container",
    };
    let status = Command::new(engine_name)
        .args(args)
        .current_dir(working_dir)
        .status()
        .map_err(|e| CliError::runtime(format!("failed to run {engine_name} build: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::runtime(format!(
            "{engine_name} build exited non-zero (see its output above)"
        )))
    }
}

fn launcher_base_image(launcher: Launcher, dev: bool) -> Result<String, CliError> {
    let suffix = match launcher {
        Launcher::Schedule => "schedule",
        Launcher::Api => "api",
        Launcher::Webapp => "webapp",
        Launcher::Local => return Err(CliError::usage("local builds have no launcher image")),
    };
    let prefix = if dev {
        DEV_IMAGE_PREFIX
    } else {
        PUBLISHED_IMAGE_PREFIX
    };
    Ok(format!("{prefix}:launcher-{suffix}"))
}

fn launcher_config_filename(launcher: Launcher) -> Result<&'static str, CliError> {
    match launcher {
        Launcher::Schedule => Ok("bae-schedules.toml"),
        Launcher::Api => Ok("bae-api.toml"),
        Launcher::Webapp => Ok("bae-app.toml"),
        Launcher::Local => Err(CliError::usage("local builds have no launcher config")),
    }
}

/// Where the generated build Dockerfile stages a self-contained, ready-to-run
/// harness tree for the interpreted SDKs, and the shim inside it that the
/// launcher image executes.
const HARNESS_TREE: &str = "/opt/bae-harness";
const HARNESS_SHIM: &str = "/opt/bae-harness/bae-harness-entrypoint";
/// The unprivileged user every `Dockerfile.launcher-*` runtime stage creates and
/// switches to. The generated final Dockerfile temporarily returns to `root` to
/// provision an SDK runtime, then switches back to it.
const LAUNCHER_USER: &str = "bae";
/// The build stage's `WORKDIR`. Rust bakes it into the binary as
/// `CARGO_MANIFEST_DIR`, so the final image has to reproduce it as a writable
/// prefix (see [`runtime_provisioning`]).
const BUILD_WORKDIR: &str = "/build";

/// baectl's synthesized per-SDK build Dockerfile — the "zero extra files" default
/// that every bundled example relies on.
///
/// Rust produces a self-contained ELF binary, so its stage is just the compile.
/// TypeScript and Python have no such artifact: their harness is source plus an
/// interpreter plus installed dependencies. For those two the stage additionally
/// assembles everything the harness needs under [`HARNESS_TREE`] and writes an
/// executable shim at [`HARNESS_SHIM`] (the generated `binary_path`), so the
/// final image's single `COPY --from … /usr/local/bin/<name>` still lands a real
/// executable. Copying bare `main.ts`/`main.py` into the Debian-slim launcher
/// base — which carries neither Node nor Python — would produce an image whose
/// every trigger fails to spawn its harness.
#[cfg(test)]
fn default_build_dockerfile(sdk: Sdk, name: &str) -> String {
    generated_build_dockerfile(sdk, name, &ContainerOverrides::default())
}

/// [`default_build_dockerfile`] with the `[harness.container]` overrides
/// applied: `build` replaces the SDK build command, `entrypoint` the command the
/// TypeScript/Python shim execs (Rust's `entrypoint` is a binary path, applied
/// by [`generated_binary_path`] instead).
fn generated_build_dockerfile(sdk: Sdk, name: &str, overrides: &ContainerOverrides) -> String {
    let build = overrides.build.as_deref();
    match sdk {
        Sdk::Rust => {
            let default_build = format!("cargo build --release --example {name}");
            let build = build.unwrap_or(&default_build);
            format!(
                "FROM rust:1-bookworm AS build\n\
                 WORKDIR {BUILD_WORKDIR}\n\
                 COPY . .\n\
                 RUN {build}\n"
            )
        }
        Sdk::Typescript => {
            let build = build.unwrap_or("npm ci && npm run build");
            let default_entry = format!("./node_modules/.bin/tsx examples/{name}/main.ts");
            let entry = printf_sq_escape(overrides.entrypoint.as_deref().unwrap_or(&default_entry));
            format!(
            "FROM node:22-bookworm AS build\n\
             WORKDIR {BUILD_WORKDIR}\n\
             COPY . .\n\
             # `npm ci` deletes any node_modules that arrived with the build\n\
             # context (installed on the developer's own OS/arch, possibly with\n\
             # incompatible native binaries) and reinstalls from the lockfile.\n\
             RUN {build}\n\
             # Stage a self-contained project tree (sources, build output and\n\
             # node_modules) plus the shim the launcher image executes. The\n\
             # launcher bases are debian:bookworm-slim and carry no Node, so the\n\
             # harness cannot be a bare .ts file.\n\
             RUN set -eu \\\n\
             \x20&& mkdir -p {HARNESS_TREE} \\\n\
             \x20&& cp -a {BUILD_WORKDIR}/. {HARNESS_TREE}/ \\\n\
             \x20&& printf '#!/bin/sh\\nset -e\\ncd {HARNESS_TREE}\\nexec {entry} \"$@\"\\n' > {HARNESS_SHIM} \\\n\
             \x20&& chmod 0755 {HARNESS_SHIM}\n"
            )
        }
        // Debian bookworm's own python3 rather than the python:3.12 image: the
        // launcher base is debian:bookworm-slim, and installing the *same*
        // interpreter on both sides keeps the staged virtualenv's interpreter
        // symlink and any compiled wheel valid after the COPY --from.
        Sdk::Python => {
            let build = build.unwrap_or("pip install --no-cache-dir .");
            let default_entry = format!("{HARNESS_TREE}/venv/bin/python examples/{name}/main.py");
            let entry = printf_sq_escape(overrides.entrypoint.as_deref().unwrap_or(&default_entry));
            format!(
            "FROM debian:bookworm-slim AS build\n\
             RUN apt-get update && apt-get install -y --no-install-recommends \\\n\
             \x20       ca-certificates python3 python3-venv \\\n\
             \x20   && rm -rf /var/lib/apt/lists/*\n\
             WORKDIR {BUILD_WORKDIR}\n\
             COPY . .\n\
             # A virtualenv keeps the harness's dependencies self-contained, so\n\
             # the launcher image needs only the interpreter — never pip. The\n\
             # build command runs with the venv activated.\n\
             RUN set -eu \\\n\
             \x20&& python3 -m venv {HARNESS_TREE}/venv \\\n\
             \x20&& export VIRTUAL_ENV={HARNESS_TREE}/venv PATH={HARNESS_TREE}/venv/bin:$PATH \\\n\
             \x20&& ( {build} ) \\\n\
             \x20&& mkdir -p {HARNESS_TREE}/app \\\n\
             \x20&& cp -a {BUILD_WORKDIR}/. {HARNESS_TREE}/app/ \\\n\
             \x20&& printf '#!/bin/sh\\nset -e\\ncd {HARNESS_TREE}/app\\nexport PATH={HARNESS_TREE}/venv/bin:$PATH\\nexec {entry} \"$@\"\\n' > {HARNESS_SHIM} \\\n\
             \x20&& chmod 0755 {HARNESS_SHIM}\n"
            )
        }
    }
}

/// The image-internal path [`default_build_dockerfile`] leaves the harness's
/// executable artifact at. Must stay in lockstep with it — the unit tests assert
/// the generated Dockerfile actually produces this exact path.
fn default_binary_path(sdk: Sdk, name: &str) -> String {
    match sdk {
        // Cargo writes `--example` builds under `target/<profile>/examples/`,
        // not beside `--bin` targets in `target/<profile>/`.
        Sdk::Rust => format!("{BUILD_WORKDIR}/target/release/examples/{name}"),
        Sdk::Typescript | Sdk::Python => HARNESS_SHIM.to_string(),
    }
}

/// [`default_binary_path`] honouring a Rust `[harness.container].entrypoint`
/// (the image path of the built binary). TypeScript/Python always land at the
/// shim; their `entrypoint` changes what the shim execs.
fn generated_binary_path(sdk: Sdk, name: &str, overrides: &ContainerOverrides) -> String {
    match (sdk, &overrides.entrypoint) {
        (Sdk::Rust, Some(path)) => path.clone(),
        _ => default_binary_path(sdk, name),
    }
}

/// Escape `s` for embedding in the generated shim's single-quoted `printf`
/// format: `'` closes/reopens the quote, and `\` / `%` are printf escapes.
fn printf_sq_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "%%")
        .replace('\'', "'\\''")
}

/// The lines the generated final Dockerfile needs so the packaged harness can
/// actually start under the launcher's unprivileged user.
///
/// Only emitted when baectl generated the build Dockerfile — a harness that
/// supplies its own `[harness.launcher].dockerfile` owns its artifact's runtime
/// requirements, and baectl has no way to know them.
///
/// - **Rust**: nothing to install, but `cargo build --example` bakes
///   `CARGO_MANIFEST_DIR` (`/build`) into the binary, and the bundled examples
///   create scratch directories beneath it. Reproduce that prefix, owned by the
///   launcher user, or the harness dies on its first `create_dir_all`.
/// - **TypeScript / Python**: install the matching interpreter (Node via the
///   same NodeSource pattern `Dockerfile.max` uses; Debian's own `python3`),
///   then bring the staged harness tree across and hand it to the launcher user.
fn runtime_provisioning(sdk: Sdk, harness_name: &str, build_image: &str) -> String {
    match sdk {
        Sdk::Rust => format!(
            "# `cargo build --example` bakes CARGO_MANIFEST_DIR into the binary;\n\
             # reproduce that prefix so a harness that writes beside its own\n\
             # sources (all bundled examples create a workspace/ dir) can.\n\
             USER root\n\
             RUN mkdir -p {BUILD_WORKDIR}/examples/{harness_name} \\\n\
             \x20&& chown -R {LAUNCHER_USER}:{LAUNCHER_USER} {BUILD_WORKDIR}\n"
        ),
        Sdk::Typescript => format!(
            "# The launcher base is debian:bookworm-slim with no Node. Install the\n\
             # same major version the build stage used, via the NodeSource\n\
             # pattern Dockerfile.max already establishes for this repo.\n\
             USER root\n\
             RUN apt-get update && apt-get install -y --no-install-recommends \\\n\
             \x20       ca-certificates curl \\\n\
             \x20   && curl -fsSL --retry 5 https://deb.nodesource.com/setup_22.x | bash - \\\n\
             \x20   && apt-get install -y --no-install-recommends nodejs \\\n\
             \x20   && rm -rf /var/lib/apt/lists/* \\\n\
             \x20   && node --version\n\
             COPY --from={build_image} --chown={LAUNCHER_USER}:{LAUNCHER_USER} {HARNESS_TREE} {HARNESS_TREE}\n"
        ),
        Sdk::Python => format!(
            "# The launcher base is debian:bookworm-slim with no interpreter.\n\
             # Install the same distro python3 the build stage's venv was made\n\
             # against, so the venv's interpreter symlink still resolves.\n\
             USER root\n\
             RUN apt-get update && apt-get install -y --no-install-recommends \\\n\
             \x20       python3 \\\n\
             \x20   && rm -rf /var/lib/apt/lists/*\n\
             COPY --from={build_image} --chown={LAUNCHER_USER}:{LAUNCHER_USER} {HARNESS_TREE} {HARNESS_TREE}\n"
        ),
    }
}

/// Generate `<dir>/.baectl/builds/<id>/Dockerfile` — the second, launcher-image
/// build. Its context is the artifact directory alone: no source, host toolchain
/// or host-built artifact ever enters it; the harness travels in exclusively via
/// `COPY --from=<harness-build image>`.
///
/// `runtime_sdk` is `Some` only when baectl also generated the build Dockerfile
/// (see [`runtime_provisioning`]); with a harness-supplied one the output is the
/// bare `FROM` / `COPY --from` / `COPY <config>` shape.
fn launcher_dockerfile(
    base_image: String,
    build_image: &str,
    binary_path: &str,
    harness_name: &str,
    config_file: &str,
    runtime_sdk: Option<Sdk>,
) -> String {
    let mut out = format!("FROM {base_image}\n");
    if let Some(sdk) = runtime_sdk {
        out.push_str(&runtime_provisioning(sdk, harness_name, build_image));
    }
    out.push_str(&format!(
        "COPY --from={build_image} {binary_path} /usr/local/bin/{harness_name}\n\
         COPY {config_file} /etc/bae/{config_file}\n"
    ));
    if runtime_sdk.is_some() {
        // Drop back to the launcher base's unprivileged user; `baeapi`/the
        // schedule launcher and every harness it spawns must not run as root.
        out.push_str(&format!("USER {LAUNCHER_USER}\n"));
    }
    out
}

/// The request-body field an api/webapp trigger carries its prompt in — the
/// field the webapp's chat box fills (`chat_input_field`'s default).
const PROMPT_FIELD: &str = "prompt";

fn launcher_config_toml(
    launcher: Launcher,
    harness_name: &str,
    config: &LauncherConfig,
) -> Result<String, CliError> {
    let name = toml_string(harness_name);
    let command = toml_string(&format!("/usr/local/bin/{harness_name}"));
    match launcher {
        Launcher::Schedule => {
            let schedule = config.default_schedule.as_deref().ok_or_else(|| {
                CliError::usage(
                    "[harness.launcher].default_schedule is required for --launcher schedule",
                )
            })?;
            Ok(format!(
                "[[agents]]\nname = {name}\ncommand = {command}\nschedule = {}\n",
                toml_string(schedule)
            ))
        }
        Launcher::Api | Launcher::Webapp => {
            // The request body carries the prompt as `prompt` (the webapp's
            // chat box fills that field); `env_template` hands it to the harness
            // under the env var the manifest names. Using `prompt_env` as the
            // body field too would make the schema reject every chat message.
            let field = toml_string(PROMPT_FIELD);
            let env = toml_string(&config.prompt_env);
            // Scalar `[[agents]]` keys must precede the agent's nested tables.
            let chat_input = match launcher {
                Launcher::Webapp => format!("chat_input_field = {field}\n"),
                _ => String::new(),
            };
            Ok(format!(
                "[server]\naddr = \"0.0.0.0:9090\"\n\n[[agents]]\nname = {name}\ncommand = {command}\n{chat_input}\n[agents.request_schema]\ntype = \"object\"\nrequired = [{field}]\n[agents.request_schema.properties.{field}]\ntype = \"string\"\n\n[[agents.env_template]]\nfield = {field}\nenv = {env}\n"
            ))
        }
        Launcher::Local => Err(CliError::usage("local builds have no launcher config")),
    }
}

fn toml_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

fn write_manifest(path: &Path, manifest: &BuildManifest) -> Result<(), CliError> {
    let raw = serde_json::to_string_pretty(manifest)
        .map_err(|e| CliError::runtime(format!("could not serialize build manifest: {e}")))?;
    fs::write(path, format!("{raw}\n")).map_err(|e| {
        CliError::runtime(format!(
            "could not write build manifest {}: {e}",
            path.display()
        ))
    })
}

fn remove_if_exists(path: &Path) -> Result<(), CliError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CliError::runtime(format!(
            "could not remove stale build artifact {}: {e}",
            path.display()
        ))),
    }
}

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`, computed in-process (no
/// `date` subprocess, so `build` works with a minimal `PATH`).
pub(crate) fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

/// Format Unix seconds as RFC 3339 UTC. Uses Howard Hinnant's
/// civil-from-days algorithm (proleptic Gregorian calendar).
fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::manifest::Requires;

    fn launcher() -> LauncherConfig {
        LauncherConfig {
            dockerfile: None,
            target: None,
            binary_path: Some("/build/reference-assistant".to_string()),
            prompt_env: "AGENT_PROMPT".to_string(),
            default_schedule: Some("0 0 3 * * *".to_string()),
        }
    }

    /// `created_at` is computed in-process (no `date` subprocess).
    #[test]
    fn rfc3339_from_unix_matches_known_instants() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(rfc3339_from_unix(4_102_444_799), "2099-12-31T23:59:59Z");
        assert_eq!(rfc3339_now().len(), "2026-01-01T00:00:00Z".len());
    }

    #[test]
    fn generated_dockerfiles_use_sdk_toolchains() {
        assert!(default_build_dockerfile(Sdk::Rust, "assistant").contains("rust:1-bookworm"));
        assert!(default_build_dockerfile(Sdk::Rust, "assistant").contains("--example assistant"));
        assert!(default_build_dockerfile(Sdk::Typescript, "assistant").contains("node:22-bookworm"));
        assert!(default_build_dockerfile(Sdk::Typescript, "assistant").contains("npm ci"));
        assert!(default_build_dockerfile(Sdk::Python, "assistant").contains("python3 -m venv"));
    }

    /// Regression for the "generated default covers all six bundled examples"
    /// requirement: for **every** SDK the generated build stage must actually
    /// produce an *executable* at the generated `binary_path`, since the final
    /// image copies exactly that path to `/usr/local/bin/<name>` and the
    /// launcher spawns it as a command. The TypeScript/Python defaults
    /// previously named a bare `main.ts`/`main.py` source file, which is neither
    /// executable nor runnable on the Debian-slim launcher base.
    #[test]
    fn generated_build_stage_produces_an_executable_at_the_generated_binary_path() {
        for sdk in [Sdk::Rust, Sdk::Typescript, Sdk::Python] {
            let dockerfile = default_build_dockerfile(sdk, "assistant");
            let binary_path = default_binary_path(sdk, "assistant");
            assert!(
                !binary_path.ends_with(".ts") && !binary_path.ends_with(".py"),
                "{sdk} binary_path {binary_path} is a source file, not an executable"
            );
            assert!(dockerfile.contains(&format!("WORKDIR {BUILD_WORKDIR}")));
            match sdk {
                // Cargo lands a compiled example at
                // `<WORKDIR>/target/release/examples/<name>` — a real ELF binary.
                Sdk::Rust => {
                    assert!(dockerfile.contains("cargo build --release --example assistant"));
                    assert_eq!(
                        binary_path,
                        format!("{BUILD_WORKDIR}/target/release/examples/assistant")
                    );
                }
                // The interpreted SDKs reach an executable through a shim the
                // build stage writes at exactly `binary_path` and marks 0755.
                Sdk::Typescript | Sdk::Python => {
                    assert_eq!(binary_path, HARNESS_SHIM);
                    assert!(
                        dockerfile.contains(&format!("> {HARNESS_SHIM}")),
                        "{sdk} build stage never writes its shim:\n{dockerfile}"
                    );
                    assert!(
                        dockerfile.contains(&format!("chmod 0755 {HARNESS_SHIM}")),
                        "{sdk} shim is never made executable:\n{dockerfile}"
                    );
                    assert!(
                        dockerfile.contains("examples/assistant/main."),
                        "{sdk} shim does not launch the harness's entry module:\n{dockerfile}"
                    );
                }
            }
        }
    }

    /// The interpreted SDKs cannot run on the `debian:bookworm-slim` launcher
    /// bases (`Dockerfile.launcher-{api,schedule,webapp}` install only
    /// `ca-certificates`), so the generated final image must install their
    /// interpreter and bring the staged harness tree across. Rust needs no
    /// runtime but does need its baked `CARGO_MANIFEST_DIR` prefix to exist and
    /// be writable by the launcher's unprivileged user.
    #[test]
    fn generated_final_image_provisions_each_sdk_runtime_and_returns_to_the_launcher_user() {
        for (sdk, runtime_marker) in [
            (Sdk::Rust, "chown -R bae:bae /build"),
            (Sdk::Typescript, "nodejs"),
            (Sdk::Python, "python3"),
        ] {
            let generated = launcher_dockerfile(
                launcher_base_image(Launcher::Api, false).unwrap(),
                "assistant-build:latest",
                &default_binary_path(sdk, "assistant"),
                "assistant",
                "bae-api.toml",
                Some(sdk),
            );
            assert!(
                generated.contains(runtime_marker),
                "{sdk} final image does not provision its runtime:\n{generated}"
            );
            if sdk != Sdk::Rust {
                assert!(
                    generated.contains(&format!(
                        "COPY --from=assistant-build:latest --chown=bae:bae \
                         {HARNESS_TREE} {HARNESS_TREE}\n"
                    )),
                    "{sdk} final image never copies the staged harness tree to the \
                     launcher user:\n{generated}"
                );
            }
            // Root is only borrowed for provisioning; the harness must not end
            // up running as root.
            assert!(generated.contains("USER root"));
            assert!(
                generated.trim_end().ends_with("USER bae"),
                "{sdk} final image does not drop back to the launcher user:\n{generated}"
            );
            assert!(!has_directive_line(&generated, "ENTRYPOINT"));
            assert!(!has_directive_line(&generated, "CMD"));
        }
    }

    /// A harness that supplies its own `[harness.launcher].dockerfile` owns its
    /// artifact's runtime needs; baectl must not inject SDK provisioning it has
    /// no way to validate, and must keep the bare documented shape.
    #[test]
    fn harness_supplied_dockerfile_gets_no_injected_runtime_provisioning() {
        let generated = launcher_dockerfile(
            launcher_base_image(Launcher::Api, false).unwrap(),
            "assistant-build:latest",
            "/opt/custom/assistant",
            "assistant",
            "bae-api.toml",
            None,
        );
        assert!(!generated.contains("USER root"));
        assert!(!generated.contains("apt-get"));
        assert_eq!(
            generated,
            "FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-api\n\
             COPY --from=assistant-build:latest /opt/custom/assistant /usr/local/bin/assistant\n\
             COPY bae-api.toml /etc/bae/bae-api.toml\n"
        );
    }

    #[test]
    fn launcher_configs_follow_example_shapes() {
        let config = launcher();
        let schedule = launcher_config_toml(Launcher::Schedule, "assistant", &config).unwrap();
        assert!(schedule.contains("[[agents]]"));
        assert!(schedule.contains("schedule = \"0 0 3 * * *\""));

        for kind in [Launcher::Api, Launcher::Webapp] {
            let config = launcher_config_toml(kind, "assistant", &config).unwrap();
            assert!(config.contains("[server]"));
            assert!(config.contains("[agents.request_schema]"));
            assert!(config.contains("[[agents.env_template]]"));
            toml::from_str::<toml::Value>(&config).unwrap();
        }
    }

    #[test]
    fn dev_launcher_tags_match_makefile_convention() {
        assert_eq!(
            launcher_base_image(Launcher::Api, true).unwrap(),
            "better-agent-engine:launcher-api"
        );
        assert_eq!(
            launcher_base_image(Launcher::Webapp, false).unwrap(),
            "ghcr.io/prettysmartdev/better-agent-engine:launcher-webapp"
        );
    }

    #[test]
    fn matching_manifest_allows_same_combo_to_rebuild() {
        let harness = Harness {
            name: "assistant".to_string(),
            sdk: Sdk::Rust,
            run: "true".to_string(),
            working_dir: ".".to_string(),
            prepare: None,
            requires: Requires::default(),
            launcher: None,
            container: None,
        };
        let manifest = BuildManifest::Local(LocalManifest {
            id: "assistant-rust-local".to_string(),
            name: "assistant".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            harness_dir: PathBuf::from("/tmp/assistant"),
            run_command: "true".to_string(),
            working_dir: ".".to_string(),
            prepare: None,
            requires: Requires::default(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        });
        assert!(same_build_combo(&manifest, &harness, Launcher::Local));
        assert!(!same_build_combo(&manifest, &harness, Launcher::Api));
    }

    #[test]
    fn dev_launcher_tags_cover_every_launcher_and_both_dev_states() {
        for (launcher, suffix) in [
            (Launcher::Schedule, "schedule"),
            (Launcher::Api, "api"),
            (Launcher::Webapp, "webapp"),
        ] {
            assert_eq!(
                launcher_base_image(launcher, true).unwrap(),
                format!("better-agent-engine:launcher-{suffix}")
            );
            assert_eq!(
                launcher_base_image(launcher, false).unwrap(),
                format!("ghcr.io/prettysmartdev/better-agent-engine:launcher-{suffix}")
            );
        }
    }

    /// The generated launcher Dockerfile follows the same "FROM the base image,
    /// COPY the config to /etc/bae/<file>, no ENTRYPOINT/CMD" shape as
    /// `examples/launchers/*/Dockerfile` — verified against the real example
    /// files rather than a hand-copied expectation, so drift there is caught.
    #[test]
    fn generated_dockerfile_shape_matches_examples() {
        let cases: [(Launcher, &str, &str); 3] = [
            (
                Launcher::Schedule,
                "bae-schedules.toml",
                include_str!("../../../examples/launchers/schedule/Dockerfile"),
            ),
            (
                Launcher::Api,
                "bae-api.toml",
                include_str!("../../../examples/launchers/api/Dockerfile"),
            ),
            (
                Launcher::Webapp,
                "bae-app.toml",
                include_str!("../../../examples/launchers/webapp/Dockerfile"),
            ),
        ];
        for (launcher, config_file, example) in cases {
            assert_eq!(launcher_config_filename(launcher).unwrap(), config_file);
            // The example never redeclares ENTRYPOINT/CMD as an actual
            // directive (relies on the base image's) and installs its config
            // at the documented /etc/bae path. Line-based, not a raw substring
            // check, because the example's own prose comments mention
            // "ENTRYPOINT"/"CMD" by name while explaining why not to set them.
            assert!(!has_directive_line(example, "ENTRYPOINT"));
            assert!(!has_directive_line(example, "CMD"));
            assert!(example.contains(&format!("COPY {config_file} /etc/bae/{config_file}")));

            let generated = launcher_dockerfile(
                launcher_base_image(launcher, false).unwrap(),
                "harness-build:latest",
                "/build/bin/assistant",
                "assistant",
                config_file,
                None,
            );
            assert!(!has_directive_line(&generated, "ENTRYPOINT"));
            assert!(!has_directive_line(&generated, "CMD"));
            assert!(generated.contains(&format!("COPY {config_file} /etc/bae/{config_file}")));
            assert!(
                generated.starts_with("FROM ghcr.io/prettysmartdev/better-agent-engine:launcher-")
            );
        }
    }

    /// Whether `text` has a real Dockerfile instruction line starting with
    /// `directive` (as opposed to the word merely appearing inside a `#`
    /// comment, e.g. "never redeclare ENTRYPOINT or CMD").
    fn has_directive_line(text: &str, directive: &str) -> bool {
        text.lines()
            .any(|line| line.trim_start().starts_with(directive))
    }

    /// The generated `bae-api.toml`/`bae-app.toml` bodies are pinned
    /// byte-for-byte to `tests/fixtures/generated/`. That directory is the
    /// contract with `launchers/api`: its `baectl_generated` test loads these
    /// exact files through `baeapi`'s real config parser (`deny_unknown_fields`
    /// and JSON-Schema compile included). The dependency points from the
    /// launcher to baectl's fixtures, never the other way, so baectl's image
    /// build needs nothing but `server/` and `baectl/`.
    ///
    /// A deliberate generator change regenerates the fixtures with
    /// `BAECTL_UPDATE_FIXTURES=1 cargo test generated_launcher_configs`.
    #[test]
    fn generated_launcher_configs_match_checked_in_fixtures() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generated");
        for (kind, file) in [
            (Launcher::Api, "bae-api.toml"),
            (Launcher::Webapp, "bae-app.toml"),
        ] {
            let generated = launcher_config_toml(kind, "reference-assistant", &launcher()).unwrap();
            let path = fixtures.join(file);
            if std::env::var_os("BAECTL_UPDATE_FIXTURES").is_some() {
                fs::create_dir_all(&fixtures).unwrap();
                fs::write(&path, &generated).unwrap();
            }
            let fixture = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
            assert_eq!(
                generated,
                fixture,
                "generated {file} drifted from {} — if intended, regenerate with \
                 BAECTL_UPDATE_FIXTURES=1 and re-run launchers/api's tests",
                path.display()
            );
        }
    }

    /// The generated api/webapp configs have the same shape as the example
    /// launcher configs (checked-in copies under `tests/fixtures/launcher-config/`,
    /// themselves pinned to `examples/launchers/`): `[server].addr`, one
    /// `[[agents]]` with `name`/`command`, `request_schema.required`, and an
    /// `env_template` `{field, env}` entry.
    #[test]
    fn generated_launcher_configs_share_the_example_shape() {
        let cases = [
            (
                Launcher::Api,
                include_str!("../../tests/fixtures/launcher-config/bae-api.toml"),
                include_str!("../../../examples/launchers/api/bae-api.toml"),
            ),
            (
                Launcher::Webapp,
                include_str!("../../tests/fixtures/launcher-config/bae-app.toml"),
                include_str!("../../../examples/launchers/webapp/bae-app.toml"),
            ),
        ];
        for (kind, fixture, example) in cases {
            assert_eq!(
                fixture, example,
                "tests/fixtures/launcher-config is a byte copy of examples/launchers"
            );
            let fixture: toml::Value = toml::from_str(fixture).unwrap();
            let generated_text =
                launcher_config_toml(kind, "reference-assistant", &launcher()).unwrap();
            let generated: toml::Value = toml::from_str(&generated_text).unwrap();

            let top_keys = |v: &toml::Value| {
                v.as_table()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
            };
            assert_eq!(top_keys(&generated), top_keys(&fixture), "{kind}");
            for v in [&fixture, &generated] {
                assert!(v["server"]["addr"].is_str());
                let agent = &v["agents"].as_array().unwrap()[0];
                assert!(agent["name"].is_str());
                assert!(agent["command"].is_str());
                assert!(agent["request_schema"]["required"].is_array());
                let template = &agent["env_template"].as_array().unwrap()[0];
                assert!(template["field"].is_str());
                assert!(template["env"].is_str());
            }
            let agent = &generated["agents"].as_array().unwrap()[0];
            assert_eq!(agent["name"].as_str(), Some("reference-assistant"));
            assert_eq!(
                agent["command"].as_str(),
                Some("/usr/local/bin/reference-assistant")
            );
            assert_eq!(generated["server"]["addr"].as_str(), Some("0.0.0.0:9090"));
        }
    }

    /// The generated `bae-schedules.toml` shares the same `[[agents]]`
    /// name/command/schedule shape as `examples/launchers/schedule`'s — checked
    /// generically (`toml::Value`) since `launcher-schedule`'s config struct is
    /// a private binary type, unlike the api/webapp launcher's public crate.
    #[test]
    fn schedule_config_shape_matches_example() {
        let example: toml::Value = toml::from_str(include_str!(
            "../../../examples/launchers/schedule/bae-schedules.toml"
        ))
        .unwrap();
        let example_agent = example["agents"].as_array().unwrap().first().unwrap();
        assert!(example_agent.get("name").is_some());
        assert!(example_agent.get("command").is_some());
        assert!(example_agent.get("schedule").is_some());

        let generated_text =
            launcher_config_toml(Launcher::Schedule, "reference-assistant", &launcher()).unwrap();
        let generated: toml::Value = toml::from_str(&generated_text).unwrap();
        let agent = generated["agents"].as_array().unwrap().first().unwrap();
        assert_eq!(agent["name"].as_str(), Some("reference-assistant"));
        assert_eq!(
            agent["command"].as_str(),
            Some("/usr/local/bin/reference-assistant")
        );
        assert_eq!(agent["schedule"].as_str(), Some("0 0 3 * * *"));
    }

    fn temp_dirs(label: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "baectl-build-dockerfile-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let harness_dir = base.join("harness");
        let artifact_dir = base.join("artifact");
        fs::create_dir_all(&harness_dir).unwrap();
        fs::create_dir_all(&artifact_dir).unwrap();
        (harness_dir, artifact_dir)
    }

    #[test]
    fn resolve_build_dockerfile_writes_default_when_harness_has_none() {
        let (harness_dir, artifact_dir) = temp_dirs("default");
        let path = resolve_build_dockerfile(
            &harness_dir,
            &artifact_dir,
            &None,
            Sdk::Python,
            "assistant",
            &ContainerOverrides::default(),
        )
        .unwrap();
        assert_eq!(path, artifact_dir.join("Dockerfile.build.generated"));
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("python3 -m venv"));
        let _ = fs::remove_dir_all(harness_dir.parent().unwrap());
    }

    #[test]
    fn resolve_build_dockerfile_uses_harness_supplied_file_verbatim_and_skips_generation() {
        let (harness_dir, artifact_dir) = temp_dirs("custom");
        fs::write(harness_dir.join("Dockerfile.custom"), "FROM scratch\n").unwrap();
        // A stale generated file from a prior build must be removed, not left
        // behind to confuse a later run that reads "does a generated file exist".
        fs::write(artifact_dir.join("Dockerfile.build.generated"), "stale").unwrap();

        let dockerfile = Some("Dockerfile.custom".to_string());
        let path = resolve_build_dockerfile(
            &harness_dir,
            &artifact_dir,
            &dockerfile,
            Sdk::Rust,
            "assistant",
            &ContainerOverrides::default(),
        )
        .unwrap();

        assert_eq!(path, harness_dir.join("Dockerfile.custom"));
        assert!(
            !artifact_dir.join("Dockerfile.build.generated").exists(),
            "the harness's own dockerfile is used verbatim; no default should be generated"
        );
        let _ = fs::remove_dir_all(harness_dir.parent().unwrap());
    }

    fn overrides(build: Option<&str>, entrypoint: Option<&str>) -> ContainerOverrides {
        ContainerOverrides {
            build: build.map(str::to_string),
            entrypoint: entrypoint.map(str::to_string),
        }
    }

    /// Regression (B10): `[harness.container]` replaces each SDK's generated
    /// build command and entrypoint, so a harness outside the bundled
    /// `examples/<name>/main.*` layout can be packaged.
    #[test]
    fn container_overrides_replace_each_sdks_build_and_entrypoint() {
        // Rust: `build` replaces the cargo line; `entrypoint` is the binary path.
        let o = overrides(
            Some("cargo build --release --bin agent"),
            Some("/build/target/release/agent"),
        );
        let df = generated_build_dockerfile(Sdk::Rust, "probe", &o);
        assert!(
            df.lines()
                .any(|l| l == "RUN cargo build --release --bin agent"),
            "{df}"
        );
        assert!(!df.contains("--example probe"), "{df}");
        assert_eq!(
            generated_binary_path(Sdk::Rust, "probe", &o),
            "/build/target/release/agent"
        );

        // TypeScript: `build` replaces `npm ci && npm run build`; the shim execs
        // `entrypoint` (quoted for the printf format).
        let o = overrides(Some("npm ci && npx tsc"), Some("node dist/it's.js"));
        let df = generated_build_dockerfile(Sdk::Typescript, "probe", &o);
        assert!(df.lines().any(|l| l == "RUN npm ci && npx tsc"), "{df}");
        assert!(!df.contains("npm run build"), "{df}");
        assert!(df.contains("exec node dist/it'\\''s.js \"$@\""), "{df}");
        assert!(!df.contains("examples/probe/main.ts"), "{df}");
        assert_eq!(
            generated_binary_path(Sdk::Typescript, "probe", &o),
            HARNESS_SHIM
        );

        // Python: `build` runs in a subshell with the venv active.
        let o = overrides(Some("pip install -e ."), Some("python -m agent"));
        let df = generated_build_dockerfile(Sdk::Python, "probe", &o);
        assert!(df.contains("&& ( pip install -e . ) \\"), "{df}");
        assert!(!df.contains("pip install --no-cache-dir ."), "{df}");
        assert!(df.contains("exec python -m agent \"$@\""), "{df}");
        assert!(!df.contains("examples/probe/main.py"), "{df}");

        // Each field is optional on its own.
        let only_entry = overrides(None, Some("node x.js"));
        let df = generated_build_dockerfile(Sdk::Typescript, "probe", &only_entry);
        assert!(df.contains("RUN npm ci && npm run build"), "{df}");
        assert_eq!(
            generated_build_dockerfile(Sdk::Rust, "probe", &overrides(None, None)),
            default_build_dockerfile(Sdk::Rust, "probe")
        );
    }

    #[test]
    fn container_overrides_must_be_single_line_and_non_empty() {
        assert!(validate_overrides(&overrides(Some("make"), Some("/bin/x"))).is_ok());
        assert!(validate_overrides(&ContainerOverrides::default()).is_ok());
        for (o, field) in [
            (overrides(Some("a\nb"), None), "build"),
            (overrides(Some("   "), None), "build"),
            (overrides(None, Some("a\r\nb")), "entrypoint"),
        ] {
            let e = validate_overrides(&o).unwrap_err();
            assert_eq!(e.exit_code(), 2);
            assert_eq!(
                e.message(),
                format!("[harness.container].{field} must be a non-empty single-line command")
            );
        }
    }

    /// Regression (B6): the generated build's context gets the excludes when it
    /// has none; an existing file is never touched.
    #[test]
    fn ensure_dockerignore_writes_only_when_absent() {
        let (context, _) = temp_dirs("dockerignore");
        assert!(ensure_dockerignore(&context, Sdk::Rust).unwrap());
        let written = fs::read_to_string(context.join(".dockerignore")).unwrap();
        assert_eq!(written, DOCKERIGNORE);
        for entry in [
            "target/",
            "node_modules/",
            ".venv/",
            "__pycache__/",
            ".baectl/",
            ".git/",
        ] {
            assert!(written.lines().any(|l| l == entry), "{entry}");
        }

        fs::write(context.join(".dockerignore"), "custom\n").unwrap();
        assert!(!ensure_dockerignore(&context, Sdk::Rust).unwrap());
        assert_eq!(
            fs::read_to_string(context.join(".dockerignore")).unwrap(),
            "custom\n"
        );
        let _ = fs::remove_dir_all(context.parent().unwrap());
    }

    #[test]
    fn dockerignore_target_detection() {
        for yes in [
            "target/\n",
            "/target\n",
            "**/target\n",
            "  target  \n",
            "*\n",
        ] {
            assert!(dockerignore_excludes(yes, "target"), "{yes:?}");
        }
        for no in [
            "",
            "targets/\n",
            "# target\n",
            "src/target/x\n",
            "node_modules/\n",
        ] {
            assert!(!dockerignore_excludes(no, "target"), "{no:?}");
        }
    }

    /// Regression (RF-8): the warning checks the directory that matters for the
    /// harness's SDK, not always `target/`.
    #[test]
    fn dockerignore_check_is_per_sdk() {
        assert_eq!(sdk_host_artifact_dir(Sdk::Rust), "target");
        assert_eq!(sdk_host_artifact_dir(Sdk::Typescript), "node_modules");
        assert_eq!(sdk_host_artifact_dir(Sdk::Python), ".venv");
        let python_only_target = "target/\n";
        assert!(!dockerignore_excludes(python_only_target, ".venv"));
        assert!(dockerignore_excludes(".venv/\n", ".venv"));
        assert!(dockerignore_excludes("**/node_modules\n", "node_modules"));
        assert!(dockerignore_excludes("node_modules/**\n", "node_modules"));
        assert!(!dockerignore_excludes(
            "node_modules_old/\n",
            "node_modules"
        ));
    }
}
