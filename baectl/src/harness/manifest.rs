//! The manifest types the `build`/`ready`/`run` verbs read and write, plus id
//! derivation. Three distinct files are modeled here:
//!
//! - **`bae-harness.toml`** ([`HarnessManifest`]) — the local, per-harness
//!   manifest an author drops next to their harness code to describe what it is
//!   and needs. Authoritative schema: the work item's "New local manifest"
//!   section. `baectl` is its only consumer; the server never reads it.
//! - **`manifest.json`** ([`BuildManifest`]) — what `build` writes under
//!   `<dir>/.baectl/builds/<id>/` recording a completed build, in one of two
//!   `kind` variants (`local` / `container`).
//! - **`resolved.json`** ([`Resolved`]) — what `ready` writes on full success,
//!   carrying the resolved profile/key/URLs `run` launches against.
//!
//! Only the types, their (de)serialization, and pure id derivation live here —
//! no build/ready/run behavior. Later workflow steps own that logic in the
//! sibling `build`/`ready`/`run` modules.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::CliError;

// -- SDK + launcher enums ----------------------------------------------------

/// The client SDK a harness is written against — selects the default build
/// toolchain (`--launcher schedule/api/webapp`) and how `--launcher local`
/// runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Sdk {
    Rust,
    Typescript,
    Python,
}

impl Sdk {
    /// The lowercase wire/id spelling (`rust` / `typescript` / `python`).
    pub fn as_str(self) -> &'static str {
        match self {
            Sdk::Rust => "rust",
            Sdk::Typescript => "typescript",
            Sdk::Python => "python",
        }
    }
}

impl std::fmt::Display for Sdk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `--launcher` choice: `local` runs on the host, the other three package
/// the harness into a `FROM bae-launcher-*` container image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Launcher {
    Local,
    Schedule,
    Api,
    Webapp,
}

impl Launcher {
    /// The lowercase wire/id spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Launcher::Local => "local",
            Launcher::Schedule => "schedule",
            Launcher::Api => "api",
            Launcher::Webapp => "webapp",
        }
    }

    /// Whether this launcher packages the harness into a container image (i.e.
    /// anything other than `local`, which runs on the host).
    pub fn is_container(self) -> bool {
        !matches!(self, Launcher::Local)
    }
}

impl std::fmt::Display for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// -- bae-harness.toml --------------------------------------------------------

/// A parsed `bae-harness.toml`. The file has a single top-level `[harness]`
/// table, so this is a thin wrapper whose only field is that table.
///
/// Every `bae-harness.toml` struct is `deny_unknown_fields`: a typo such as
/// `allowed_tool` is a usage error naming the key, never a silently ignored
/// requirement. (The `manifest.json`/`resolved.json` structs below stay
/// lenient for forward compatibility.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManifest {
    pub harness: Harness,
}

/// The `[harness]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    /// Harness name — the id stem and the packaged binary/agent name.
    pub name: String,
    /// Which client SDK the harness is written against.
    pub sdk: Sdk,
    /// The host command that builds+runs the harness under `--launcher local`.
    pub run: String,
    /// Working directory for `run`, relative to this file's directory
    /// (defaults to `.`).
    #[serde(default = "default_working_dir")]
    pub working_dir: String,
    /// Optional host command `run` executes (via `sh -c`, in `working_dir`)
    /// before `run` for `--launcher local` only, and only when needed — e.g.
    /// `npm install` for the TypeScript examples. Never used by container
    /// launchers, whose generated Dockerfile installs dependencies itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<String>,
    /// What the harness needs from the server to run correctly.
    #[serde(default)]
    pub requires: Requires,
    /// Container-packaging metadata. Absent for a harness that only supports
    /// `--launcher local` (e.g. a two-phase control loop with no single
    /// prompt) — parses to `None` without error.
    #[serde(default)]
    pub launcher: Option<LauncherConfig>,
    /// `[harness.container]` — optional overrides for baectl's generated
    /// per-SDK build Dockerfile. A harness outside the bundled
    /// `examples/<name>/main.*` layout must set these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerOverrides>,
}

/// `[harness.container]` — overrides for the generated build Dockerfile. Both
/// fields are optional individually, and the table is ignored entirely when
/// `[harness.launcher].dockerfile` is set (that Dockerfile owns its build).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerOverrides {
    /// Replaces the SDK build `RUN` command (`cargo build --release --example
    /// <name>` / `npm ci && npm run build` / the venv `pip install .`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// TypeScript/Python: the command the generated shim `exec`s, run from the
    /// staged harness tree. Rust: the image path of the built binary,
    /// replacing `/build/target/release/<name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

/// `[harness.requires]` — the compatibility surface `ready` checks a profile /
/// registry / environment against. Every field defaults to empty, so a harness
/// with no requirements may omit the whole table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Client-side tools the profile must allow.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// MCP servers the profile must enable *and* the registry must define.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// Extra required env vars, beyond `BAE_SERVER_URL` / `BAE_CLIENT_KEY` /
    /// the provider's auth-token var.
    #[serde(default)]
    pub env: Vec<String>,
    /// Sandbox images the profile's `available_sandboxes` must include.
    #[serde(default)]
    pub sandboxes: Vec<String>,
}

/// `[harness.launcher]` — used only when packaging with `--launcher
/// schedule/api/webapp`. Present only for harnesses that support container
/// packaging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LauncherConfig {
    /// Optional harness-supplied build Dockerfile (relative to the harness
    /// dir). Omitted → `baectl` synthesizes a per-SDK default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    /// Optional `docker build --target` stage, if `dockerfile` is multi-stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The built artifact's path *inside* the image `dockerfile` produces —
    /// deliberately a Docker-image path, never a host path. Omitted only for
    /// baectl's generated per-SDK Dockerfile, whose artifact path is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Env var the packaged harness reads its triggered prompt from.
    pub prompt_env: String,
    /// Cron expression used only when packaged with `--launcher schedule`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_schedule: Option<String>,
}

fn default_working_dir() -> String {
    ".".to_string()
}

/// Parse a `bae-harness.toml`'s text. A missing required field or malformed
/// TOML is a fatal usage error (exit 2) whose message names the offending
/// field, matching the posture WI 0014 set for `bae-schedules.toml` etc.
///
/// The message is serde's own one-line text (e.g. ``unknown field
/// `allowed_tool`, expected one of `allowed_tools`, …``) followed by the line
/// number and source line it was found on, rather than the TOML crate's
/// multi-line snippet.
pub fn parse_harness_manifest(text: &str) -> Result<HarnessManifest, CliError> {
    toml::from_str(text).map_err(|e| {
        let line = e
            .span()
            .map(|span| {
                let line_no = text[..span.start].matches('\n').count() + 1;
                let source = text.lines().nth(line_no - 1).unwrap_or("").trim();
                format!(" (line {line_no}: `{source}`)")
            })
            .unwrap_or_default();
        CliError::usage(format!(
            "invalid bae-harness.toml: {}{line}",
            e.message().trim_end()
        ))
    })
}

/// Read and parse the `bae-harness.toml` at `path`. A file that cannot be read
/// (e.g. absent from the harness dir) is the same fatal usage error as a
/// malformed one — a harness `build`/`ready`/`run` can act on must declare
/// itself.
pub fn load_harness_manifest(path: &std::path::Path) -> Result<HarnessManifest, CliError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        CliError::usage(format!(
            "could not read bae-harness.toml at {}: {e}",
            path.display()
        ))
    })?;
    parse_harness_manifest(&text)
}

// -- manifest.json -----------------------------------------------------------

/// A completed build's `manifest.json`, in one of two `kind` variants. Tagged
/// on `kind` so `{"kind":"local", …}` / `{"kind":"container", …}` matches the
/// on-disk shape the work item specifies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BuildManifest {
    /// A `--launcher local` build: nothing containerized; `run` executes
    /// `run_command` on the host.
    Local(LocalManifest),
    /// A `--launcher schedule/api/webapp` build: a packaged container image
    /// `run` launches detached.
    Container(ContainerManifest),
}

impl BuildManifest {
    /// The resolved build id (the `<dir>/.baectl/builds/<id>/` stem).
    pub fn id(&self) -> &str {
        match self {
            BuildManifest::Local(m) => &m.id,
            BuildManifest::Container(m) => &m.id,
        }
    }

    /// The harness requirements `ready`/`run` check, regardless of kind.
    pub fn requires(&self) -> &Requires {
        match self {
            BuildManifest::Local(m) => &m.requires,
            BuildManifest::Container(m) => &m.requires,
        }
    }

    /// Whether the build was produced with `--dev` (the §5 consistency guard).
    pub fn dev(&self) -> bool {
        match self {
            BuildManifest::Local(m) => m.dev,
            BuildManifest::Container(m) => m.dev,
        }
    }
}

/// The `kind: "local"` manifest body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalManifest {
    pub id: String,
    pub name: String,
    pub sdk: Sdk,
    /// Whether this build was produced under `--dev`.
    pub dev: bool,
    /// Absolute path to the harness directory.
    pub harness_dir: PathBuf,
    /// The host command `run` executes (`harness.run`).
    pub run_command: String,
    /// Working directory for `run`, relative to `harness_dir`.
    pub working_dir: String,
    /// `harness.prepare`, run before `run_command` only when needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<String>,
    pub requires: Requires,
    /// RFC 3339 build timestamp, stamped by the caller.
    pub created_at: String,
}

/// The `kind: "container"` manifest body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerManifest {
    pub id: String,
    pub name: String,
    pub sdk: Sdk,
    /// Whether this build was produced under `--dev`.
    pub dev: bool,
    /// Which container launcher packaged it (`schedule` / `api` / `webapp`).
    pub launcher_type: Launcher,
    /// The final packaged image tag `run` launches.
    pub image_tag: String,
    /// The intermediate `<id>-harness-build` image the final image copies from.
    pub harness_build_image: String,
    /// Published host port for `api`/`webapp` launchers (`None` for
    /// `schedule`, which exposes no port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub requires: Requires,
    /// RFC 3339 build timestamp, stamped by the caller.
    pub created_at: String,
}

// -- resolved.json -----------------------------------------------------------

/// `ready`'s success artifact: the resolved profile/key/URLs `run` launches
/// against, written at mode `0600` (it may carry a plaintext client key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    pub profile_id: String,
    pub profile_name: String,
    pub key_id: String,
    /// The freshly created key's plaintext — populated **only** when `ready`
    /// itself just created the key (the admin API shows it exactly once).
    /// Omitted entirely (not null) otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_plaintext: Option<String>,
    pub server_url: String,
    /// The MAX dashboard URL, when the `setup` image variant was `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_url: Option<String>,
    /// The name (never the value) of the resolved profile's provider auth-token
    /// env var, as derived from `primary_provider` in `bae-config.toml` during
    /// readiness check #5. `run` needs it because check #5 accepts a value that
    /// lives *only* in the host environment, and a container launched off
    /// `setup`'s network would otherwise never receive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_env: Option<String>,
}

// -- Name / id validation ----------------------------------------------------

/// The longest harness name / build id accepted (Docker's tag length limit).
pub const MAX_NAME_LEN: usize = 128;

/// Whether `s` is a valid Docker tag / container-name component under the
/// lowercase subset baectl accepts: `^[a-z0-9][a-z0-9_.-]*$`, at most
/// [`MAX_NAME_LEN`] characters. Both the harness name and the build id end up
/// in image tags (`<id>:latest`) and container names (`--name <id>`).
pub fn is_valid_docker_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() <= MAX_NAME_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

const DOCKER_NAME_RULE: &str = "must match [a-z0-9][a-z0-9_.-]* (Docker tag/container-name rules)";

/// Validate a `bae-harness.toml` `name` (exit 2 naming the offending value).
pub fn validate_harness_name(name: &str) -> Result<(), CliError> {
    if is_valid_docker_name(name) {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid harness name {name:?}: {DOCKER_NAME_RULE}{}",
            too_long_note(name)
        )))
    }
}

/// Validate an explicit `--id` (exit 2). Only called when `--id` was given, so
/// the message names the flag only then.
pub fn validate_explicit_id(id: &str) -> Result<(), CliError> {
    if is_valid_docker_name(id) {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid --id {id:?}: {DOCKER_NAME_RULE}{}",
            too_long_note(id)
        )))
    }
}

/// Validate a derived `<name>-<sdk>-<launcher>[-N]` id. The name already
/// passed [`validate_harness_name`], so only the length can fail here.
pub fn validate_derived_id(id: &str) -> Result<(), CliError> {
    if is_valid_docker_name(id) {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid build id {id:?} derived from the harness name: {DOCKER_NAME_RULE}{} \
             — shorten the harness name or pass --id",
            too_long_note(id)
        )))
    }
}

fn too_long_note(s: &str) -> String {
    if s.len() > MAX_NAME_LEN {
        format!(", at most {MAX_NAME_LEN} characters")
    } else {
        String::new()
    }
}

// -- Id derivation -----------------------------------------------------------

/// Derive a build id.
///
/// - An explicit `--id` (`explicit`) always wins verbatim — never suffixed.
/// - Otherwise the id is `<name>-<sdk>-<launcher>`, collision-suffixed
///   (`-2`, `-3`, …) against `existing` only when that bare id is already taken.
///
/// Overwrite-in-place semantics (re-running `build` for the same
/// harness/sdk/launcher combination reuses its id) are the caller's job: pass
/// only the ids of *other* combinations in `existing`, so the same-combo id is
/// returned unsuffixed and its files are overwritten.
pub fn derive_id(
    name: &str,
    sdk: Sdk,
    launcher: Launcher,
    explicit: Option<&str>,
    existing: &[String],
) -> String {
    if let Some(id) = explicit {
        return id.to_string();
    }
    let base = format!("{name}-{sdk}-{launcher}");
    if !existing.iter().any(|e| e == &base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|e| e == &candidate) {
            return candidate;
        }
    }
    unreachable!("the 2.. suffix search always terminates")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST_LOCAL: &str = r#"
[harness]
name = "reference-assistant"
sdk = "rust"
run = "cargo run --release --example reference-assistant"
working_dir = "."

[harness.requires]
allowed_tools = ["get_current_time"]
mcp_servers = []
env = []

[harness.launcher]
dockerfile = "Dockerfile.build"
target = "build"
binary_path = "/build/target/release/reference-assistant"
prompt_env = "AGENT_PROMPT"
default_schedule = "0 0 3 * * *"
"#;

    #[test]
    fn parses_full_rust_manifest() {
        let m = parse_harness_manifest(RUST_LOCAL).unwrap();
        assert_eq!(m.harness.name, "reference-assistant");
        assert_eq!(m.harness.sdk, Sdk::Rust);
        assert_eq!(
            m.harness.run,
            "cargo run --release --example reference-assistant"
        );
        assert_eq!(m.harness.working_dir, ".");
        assert_eq!(m.harness.requires.allowed_tools, vec!["get_current_time"]);
        assert!(m.harness.requires.mcp_servers.is_empty());
        let launcher = m.harness.launcher.expect("launcher section present");
        assert_eq!(launcher.dockerfile.as_deref(), Some("Dockerfile.build"));
        assert_eq!(launcher.target.as_deref(), Some("build"));
        assert_eq!(
            launcher.binary_path.as_deref(),
            Some("/build/target/release/reference-assistant")
        );
        assert_eq!(launcher.prompt_env, "AGENT_PROMPT");
        assert_eq!(launcher.default_schedule.as_deref(), Some("0 0 3 * * *"));
    }

    #[test]
    fn parses_each_sdk_value() {
        for (name, sdk) in [
            ("rust", Sdk::Rust),
            ("typescript", Sdk::Typescript),
            ("python", Sdk::Python),
        ] {
            let text = format!("[harness]\nname = \"x\"\nsdk = \"{name}\"\nrun = \"go\"\n");
            let m = parse_harness_manifest(&text).unwrap();
            assert_eq!(m.harness.sdk, sdk);
        }
    }

    #[test]
    fn launcher_section_is_optional_and_parses_to_none() {
        // issue-triage-shaped manifest: no [harness.launcher], no [requires].
        let text = "\
[harness]
name = \"issue-triage\"
sdk = \"rust\"
run = \"cargo run --release --example issue-triage\"
";
        let m = parse_harness_manifest(text).unwrap();
        assert!(m.harness.launcher.is_none());
        // working_dir defaults to "." when omitted.
        assert_eq!(m.harness.working_dir, ".");
        // requires defaults to all-empty when the table is omitted.
        assert!(m.harness.requires.allowed_tools.is_empty());
        assert!(m.harness.requires.mcp_servers.is_empty());
        assert!(m.harness.requires.env.is_empty());
    }

    #[test]
    fn generated_launcher_dockerfile_may_derive_binary_path() {
        let text = "\
[harness]
name = \"reference-assistant\"
sdk = \"rust\"
run = \"cargo run --release --example reference-assistant\"

[harness.launcher]
prompt_env = \"AGENT_PROMPT\"
";
        let manifest = parse_harness_manifest(text).unwrap();
        assert_eq!(manifest.harness.launcher.unwrap().binary_path, None);
    }

    #[test]
    fn missing_required_field_is_usage_error_naming_the_field() {
        // `run` omitted.
        let text = "[harness]\nname = \"x\"\nsdk = \"rust\"\n";
        let err = parse_harness_manifest(text).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.message().contains("run"),
            "message should name the missing field, got: {}",
            err.message()
        );
    }

    #[test]
    fn unknown_sdk_is_usage_error_naming_sdk() {
        let text = "[harness]\nname = \"x\"\nsdk = \"golang\"\nrun = \"go\"\n";
        let err = parse_harness_manifest(text).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.message().contains("sdk"),
            "message should name the invalid field, got: {}",
            err.message()
        );
    }

    #[test]
    fn malformed_toml_is_usage_error() {
        let err = parse_harness_manifest("this is not = = toml").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn derive_id_default_shape() {
        let id = derive_id("reference-assistant", Sdk::Rust, Launcher::Local, None, &[]);
        assert_eq!(id, "reference-assistant-rust-local");
    }

    #[test]
    fn derive_id_explicit_always_wins_unsuffixed() {
        // Even when the explicit id collides, it is returned verbatim.
        let existing = vec!["my-id".to_string()];
        let id = derive_id("ref", Sdk::Rust, Launcher::Api, Some("my-id"), &existing);
        assert_eq!(id, "my-id");
    }

    #[test]
    fn derive_id_suffixes_only_on_collision_when_id_omitted() {
        let existing = vec!["ref-rust-local".to_string()];
        let id = derive_id("ref", Sdk::Rust, Launcher::Local, None, &existing);
        assert_eq!(id, "ref-rust-local-2");

        let existing = vec!["ref-rust-local".to_string(), "ref-rust-local-2".to_string()];
        let id = derive_id("ref", Sdk::Rust, Launcher::Local, None, &existing);
        assert_eq!(id, "ref-rust-local-3");
    }

    #[test]
    fn local_manifest_round_trips_and_tags_kind() {
        let m = BuildManifest::Local(LocalManifest {
            id: "ref-rust-local".to_string(),
            name: "ref".to_string(),
            sdk: Sdk::Rust,
            dev: true,
            harness_dir: PathBuf::from("/abs/path"),
            run_command: "cargo run".to_string(),
            working_dir: ".".to_string(),
            prepare: None,
            requires: Requires::default(),
            created_at: "2026-08-28T00:00:00Z".to_string(),
        });
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "local");
        assert_eq!(v["harness_dir"], "/abs/path");
        let back: BuildManifest = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.id(), "ref-rust-local");
        assert!(back.dev());
    }

    #[test]
    fn container_manifest_round_trips_and_tags_kind() {
        let m = BuildManifest::Container(ContainerManifest {
            id: "ref-rust-api".to_string(),
            name: "ref".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            launcher_type: Launcher::Api,
            image_tag: "ref-rust-api:latest".to_string(),
            harness_build_image: "ref-rust-api-harness-build".to_string(),
            port: Some(8090),
            requires: Requires::default(),
            created_at: "2026-08-28T00:00:00Z".to_string(),
        });
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "container");
        assert_eq!(v["port"], 8090);
        let back: BuildManifest = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn resolved_omits_plaintext_when_no_key_created() {
        let r = Resolved {
            profile_id: "pro_1".to_string(),
            profile_name: "default".to_string(),
            key_id: "key_1".to_string(),
            client_key_plaintext: None,
            server_url: "http://localhost:8080".to_string(),
            max_url: None,
            provider_env: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("client_key_plaintext"),
            "plaintext must be omitted, not null-stuffed"
        );
        assert!(!obj.contains_key("max_url"));

        let r2 = Resolved {
            client_key_plaintext: Some("bae_sk_live".to_string()),
            ..r
        };
        let v2 = serde_json::to_value(&r2).unwrap();
        assert_eq!(v2["client_key_plaintext"], "bae_sk_live");
    }

    /// Regression (B9): a typo'd key in any `bae-harness.toml` table is a
    /// usage error naming the key and its line — never a silently ignored
    /// requirement.
    #[test]
    fn unknown_key_in_any_table_is_a_usage_error_naming_it() {
        let base = "[harness]\nname = \"probe\"\nsdk = \"rust\"\nrun = \"true\"\n";
        for (extra, key) in [
            ("[other]\nx = 1\n", "other"),
            ("runn = \"x\"\n", "runn"),
            (
                "[harness.requires]\nallowed_tool = [\"x\"]\n",
                "allowed_tool",
            ),
            (
                "[harness.launcher]\nprompt_env = \"P\"\nschedule = \"x\"\n",
                "schedule",
            ),
            ("[harness.container]\nbuld = \"make\"\n", "buld"),
        ] {
            let text = format!("{base}{extra}");
            let err = parse_harness_manifest(&text).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{extra}");
            assert!(
                err.message()
                    .starts_with(&format!("invalid bae-harness.toml: unknown field `{key}`")),
                "{}",
                err.message()
            );
            assert!(err.message().contains("(line "), "{}", err.message());
            assert!(!err.message().contains('\n'), "one line: {}", err.message());
        }
        let err = parse_harness_manifest(&format!(
            "{base}\n[harness.requires]\nallowed_tool = [\"x\"]\n"
        ))
        .unwrap_err();
        assert!(
            err.message()
                .ends_with("(line 7: `allowed_tool = [\"x\"]`)"),
            "{}",
            err.message()
        );
        assert!(err.message().contains("expected one of `allowed_tools`"));
    }

    /// B9/B10/C2: the new optional fields parse.
    #[test]
    fn new_optional_fields_parse() {
        let m = parse_harness_manifest(
            "[harness]\nname = \"probe\"\nsdk = \"typescript\"\nrun = \"npm start\"\n\
             prepare = \"npm install\"\n\n[harness.requires]\nsandboxes = [\"alpine\"]\n\n\
             [harness.container]\nbuild = \"npm ci\"\nentrypoint = \"node dist/main.js\"\n",
        )
        .unwrap();
        assert_eq!(m.harness.prepare.as_deref(), Some("npm install"));
        assert_eq!(m.harness.requires.sandboxes, vec!["alpine"]);
        let c = m.harness.container.unwrap();
        assert_eq!(c.build.as_deref(), Some("npm ci"));
        assert_eq!(c.entrypoint.as_deref(), Some("node dist/main.js"));
        let bare = parse_harness_manifest("[harness]\nname = \"p\"\nsdk = \"rust\"\nrun = \"x\"\n")
            .unwrap();
        assert!(bare.harness.prepare.is_none());
        assert!(bare.harness.container.is_none());
        assert!(bare.harness.requires.sandboxes.is_empty());
    }

    #[test]
    fn docker_name_rule() {
        for ok in ["a", "0", "reference-assistant", "a.b_c-d", "x9"] {
            assert!(is_valid_docker_name(ok), "{ok}");
        }
        for bad in [
            "", "My Agent", "Upper", "-lead", ".lead", "_lead", "a/b", "a b", "é",
        ] {
            assert!(!is_valid_docker_name(bad), "{bad:?}");
        }
        assert!(is_valid_docker_name(&"a".repeat(MAX_NAME_LEN)));
        assert!(!is_valid_docker_name(&"a".repeat(MAX_NAME_LEN + 1)));
    }

    /// Regression (B9): only an explicit `--id` is blamed on `--id`.
    #[test]
    fn name_and_id_validation_messages() {
        let e = validate_harness_name("My Agent").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert_eq!(
            e.message(),
            "invalid harness name \"My Agent\": must match [a-z0-9][a-z0-9_.-]* \
             (Docker tag/container-name rules)"
        );
        assert!(!e.message().contains("--id"));

        let e = validate_explicit_id("Bad/Id").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert_eq!(
            e.message(),
            "invalid --id \"Bad/Id\": must match [a-z0-9][a-z0-9_.-]* \
             (Docker tag/container-name rules)"
        );

        let long = "a".repeat(MAX_NAME_LEN + 1);
        let e = validate_explicit_id(&long).unwrap_err();
        assert!(
            e.message().ends_with(", at most 128 characters"),
            "{}",
            e.message()
        );

        let e = validate_derived_id(&format!("{}-rust-local", "a".repeat(120))).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.message().starts_with("invalid build id \""));
        assert!(e
            .message()
            .ends_with("— shorten the harness name or pass --id"));

        assert!(validate_harness_name("reference-assistant").is_ok());
        assert!(validate_explicit_id("reference-assistant-rust-local").is_ok());
    }
}
