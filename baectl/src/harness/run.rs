//! `baectl run <id>` — launch a built harness and print where/how to reach it.
//!
//! By default `run` performs the same six checks as `ready` (via the shared
//! [`crate::harness::checks`]), re-validating rather than trusting any existing
//! `resolved.json` and auto-applying the safe #2/#4 fixes with no prompt — the
//! non-interactive fast path that keeps the "2–3 commands" promise. `--no-ready`
//! skips straight to launch from the existing `resolved.json`.
//!
//! - `kind: "local"` runs `run_command` on the host in the foreground with
//!   inherited stdio (Ctrl-C reaches the child) and propagates its exit code.
//! - `kind: "container"` launches a detached container off `setup`'s network,
//!   reaching `baesrv` via the published host port, and prints the launcher's
//!   "where/how" summary.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::engine::{detect_engine, EngineKind, Prompter};
use crate::error::CliError;
use crate::harness::artifact::{
    artifact_dir, harness_env_path, load_manifest, load_resolved, warn_dev_mismatch, write_private,
    write_resolved,
};
use crate::harness::checks::{evaluate, FixMode};
use crate::harness::manifest::{
    BuildManifest, ContainerManifest, Launcher, LocalManifest, Resolved,
};
use crate::setup::ENV_FILE;

/// The fully resolved inputs to a `run` invocation, mirroring §4's flag set.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The build id to launch (its `<dir>/.baectl/builds/<id>/manifest.json`).
    pub id: String,
    /// `--dir` — the workspace dir holding `.baectl/` and the `setup` files
    /// (default `.`).
    pub dir: PathBuf,
    /// `--no-ready` — skip the readiness re-check and launch straight from the
    /// existing `resolved.json`.
    pub no_ready: bool,
    /// `--server-url` — override the auto-derived address a launched container
    /// uses to reach `baesrv`.
    pub server_url: Option<String>,
    /// `--dev` — consistency guard against the build's recorded dev flag (§5).
    pub dev: bool,
}

/// Entry point for `baectl run`.
pub fn run(opts: RunOptions) -> Result<(), CliError> {
    let manifest = load_manifest(&opts.dir, &opts.id)?;
    warn_dev_mismatch(&manifest, opts.dev);

    let resolved = if opts.no_ready {
        // Launch verbatim from the existing resolved.json — fail loudly if the
        // readiness step was never run.
        load_resolved(&opts.dir, &opts.id).ok_or_else(|| {
            CliError::runtime(format!(
                "--no-ready but no resolved.json for '{}' — run `baectl ready {}` first \
                 (or drop --no-ready to check + launch in one step)",
                opts.id, opts.id
            ))
        })?
    } else {
        let prior = load_resolved(&opts.dir, &opts.id);
        match evaluate(&opts.dir, &manifest, prior.as_ref(), FixMode::Auto)? {
            Some(resolved) => {
                write_resolved(&opts.dir, &opts.id, &resolved)?;
                resolved
            }
            None => {
                return Err(CliError::runtime(format!(
                    "readiness checks did not pass (see above) — run `baectl ready {}` for the \
                     full report and fix guidance",
                    opts.id
                )));
            }
        }
    };

    match &manifest {
        BuildManifest::Local(m) => run_local(&opts, m, &resolved),
        BuildManifest::Container(m) => run_container(&opts, m, &resolved),
    }
}

/// `kind: "local"` — export the two `BAE_*` vars, inherit every other host env
/// var untouched, `cd` to `harness_dir/working_dir`, and run in the foreground.
fn run_local(opts: &RunOptions, m: &LocalManifest, resolved: &Resolved) -> Result<(), CliError> {
    let key = client_key(resolved)?;
    let server_url = opts
        .server_url
        .clone()
        .unwrap_or_else(|| resolved.server_url.clone());
    let workdir = m.harness_dir.join(&m.working_dir);

    println!("── running {} (local) ──────────────", m.name);
    println!(
        "profile:  {} ({})",
        resolved.profile_name, resolved.profile_id
    );
    println!("key:      {}", resolved.key_id);
    println!("server:   {server_url}");
    if let Some(url) = &resolved.max_url {
        println!("MAX:      {url}");
    }
    println!("(Ctrl-C to stop)\n");

    // Inherited stdio (the default) streams the child's output live and lets
    // Ctrl-C reach it directly; `.env` adds only the two BAE_* vars, leaving
    // every other host env var exactly as it was.
    let status = Command::new("sh")
        .args(["-c", &m.run_command])
        .current_dir(&workdir)
        .env("BAE_SERVER_URL", &server_url)
        .env("BAE_CLIENT_KEY", key)
        .status()
        .map_err(|e| {
            CliError::runtime(format!(
                "failed to launch harness in {}: {e}",
                workdir.display()
            ))
        })?;

    // Propagate the child's exit code as our own (128 for a signal death).
    std::process::exit(status.code().unwrap_or(1));
}

/// `kind: "container"` — resolve `requires.env`, write `harness.env` (`0600`),
/// replace any prior same-named container, launch detached, and print the
/// launcher-appropriate "where/how" summary.
fn run_container(
    opts: &RunOptions,
    m: &ContainerManifest,
    resolved: &Resolved,
) -> Result<(), CliError> {
    let key = client_key(resolved)?;
    let server_url = opts
        .server_url
        .clone()
        .unwrap_or_else(|| resolved.server_url.clone());
    let dir = &opts.dir;
    let kind = detect_engine(dir).unwrap_or(EngineKind::Docker);
    let bin = engine_binary(kind);

    // Resolve every required env var → harness.env, prompting for what is
    // missing (failing loudly, naming the var, when there is no TTY).
    let captured = resolve_container_env(dir, m, resolved.provider_env.as_deref())?;
    let env_body = harness_env_body(&captured, &server_url, key);
    let env_path = harness_env_path(dir, &m.id);
    write_private(&env_path, &env_body)?;

    // Idempotent re-run: drop any prior container of this name first.
    remove_prior_container(bin, kind, &m.id);

    let dotenv = dir.join(ENV_FILE);
    let args = container_run_args(
        kind,
        m,
        dotenv.exists().then(|| dotenv.display().to_string()),
        &env_path.display().to_string(),
    );

    let status = Command::new(bin)
        .args(&args)
        .current_dir(dir)
        .status()
        .map_err(|e| CliError::runtime(format!("failed to launch container with {bin}: {e}")))?;
    if !status.success() {
        return Err(CliError::runtime(format!(
            "{bin} run exited non-zero while launching '{}' (see its output above)",
            m.id
        )));
    }

    print_where_how(bin, dir, m, &server_url);
    Ok(())
}

/// The body written to the `0600` `harness.env`: every value resolved for this
/// launch, including `BAE_SERVER_URL`/`BAE_CLIENT_KEY`.
///
/// The client key deliberately travels in this file rather than as a
/// `--env BAE_CLIENT_KEY=<secret>` argument: an argv element is readable by any
/// local user through `ps`/`/proc/<pid>/cmdline` for the lifetime of the engine
/// client process, so a plaintext key must never appear there. The two `BAE_*`
/// entries are written last so they win over any same-named key in `<dir>/.env`
/// (the engine applies `--env-file`s in the order they are given).
fn harness_env_body(captured: &[(String, String)], server_url: &str, key: &str) -> String {
    let mut body: String = captured.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    body.push_str(&format!("BAE_SERVER_URL={server_url}\n"));
    body.push_str(&format!("BAE_CLIENT_KEY={key}\n"));
    body
}

/// The engine argv for a detached container launch. Pure, so the "no secret
/// ever reaches argv" guarantee is directly unit-testable: every value-bearing
/// variable is delivered through an `--env-file`, and only file paths, the
/// container name, the published port and the image tag appear here.
fn container_run_args(
    kind: EngineKind,
    m: &ContainerManifest,
    dotenv_path: Option<String>,
    harness_env_path: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), m.id.clone()];
    if kind == EngineKind::Docker {
        // Map the host-gateway alias in so `host.docker.internal` resolves on
        // Linux too (Docker Desktop provides it already; harmless there).
        args.push("--add-host".into());
        args.push("host.docker.internal:host-gateway".into());
    }
    if let Some(dotenv) = dotenv_path {
        args.push("--env-file".into());
        args.push(dotenv);
    }
    args.push("--env-file".into());
    args.push(harness_env_path.to_string());
    if let Some(port) = m.port {
        args.push("--publish".into());
        args.push(format!("{port}:{port}"));
    }
    args.push(m.image_tag.clone());
    args
}

/// Extract the plaintext client key `run` must pass, failing loudly when a
/// hand-written / stale `resolved.json` lacks it (`--no-ready` can hit this).
fn client_key(resolved: &Resolved) -> Result<&str, CliError> {
    resolved.client_key_plaintext.as_deref().ok_or_else(|| {
        CliError::runtime(
            "resolved.json has no stored client key — re-run `baectl ready` (without \
             --no-ready) so it can mint and persist one",
        )
    })
}

/// Resolve every env var a container launch needs into `(key, value)` pairs to
/// write into `harness.env`. Vars already in `<dir>/.env` are passed through by
/// `--env-file .env` and skipped here; ones only in the host env are captured; a
/// still-missing one prompts (TTY) or fails loudly (no TTY).
///
/// The set is `requires.env` **plus** the resolved profile's provider
/// auth-token var (`provider_env`). Readiness check #5 accepts that var from
/// either `<dir>/.env` *or* the host environment, so a launch that only
/// forwarded `.env` would drop a host-only provider token and the harness would
/// fail its provider call inside the container.
fn resolve_container_env(
    dir: &Path,
    m: &ContainerManifest,
    provider_env: Option<&str>,
) -> Result<Vec<(String, String)>, CliError> {
    let prompt = Prompter::new();
    let mut captured = Vec::new();
    let saved_path = harness_env_path(dir, &m.id);
    let mut needed: Vec<String> = m.requires.env.clone();
    if let Some(var) = provider_env {
        if !needed.iter().any(|n| n == var) {
            needed.push(var.to_string());
        }
    }
    for var in &needed {
        if dotenv_has(dir, var) {
            continue;
        }
        match std::env::var(var) {
            Ok(v) if !v.is_empty() => captured.push((var.clone(), v)),
            _ => {
                // Preserve a value captured by an earlier run, and support the
                // documented non-interactive path where CI pre-populates
                // harness.env before invoking `run --no-ready`.
                if let Some(value) = env_file_value(&saved_path, var) {
                    captured.push((var.clone(), value));
                    continue;
                }
                if prompt.interactive {
                    let value = prompt.ask_line(&format!("Value for required env var {var}?"), "");
                    if value.trim().is_empty() {
                        return Err(CliError::runtime(format!(
                            "no value provided for required env var {var}"
                        )));
                    }
                    captured.push((var.clone(), value.trim().to_string()));
                } else {
                    return Err(CliError::runtime(format!(
                        "required env var {var} is not set and there is no TTY to prompt — set \
                         it in the environment or {}/{ENV_FILE}, or pre-populate {}, then re-run",
                        dir.display(),
                        harness_env_path(dir, &m.id).display()
                    )));
                }
            }
        }
    }
    Ok(captured)
}

/// Whether `<dir>/.env` declares `var` (its value is then passed straight
/// through to the container by `--env-file .env`).
fn dotenv_has(dir: &Path, var: &str) -> bool {
    env_file_value(&dir.join(ENV_FILE), var).is_some()
}

/// Read one non-empty value from a Docker-style env file. Splitting only on
/// the first `=` preserves tokens that legitimately contain further equals
/// signs. Blank values are unresolved, matching the readiness check.
fn env_file_value(path: &Path, var: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        (key.trim() == var && !value.is_empty()).then(|| value.to_string())
    })
}

/// The raw engine binary a detached `run` shells out to.
fn engine_binary(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Docker => "docker",
        EngineKind::Apple => "container",
    }
}

/// Best-effort removal of any prior container of the same name, so a re-run does
/// not collide on `--name`. Mirrors `bae-setup.sh`'s stop/rm idempotency.
fn remove_prior_container(bin: &str, kind: EngineKind, id: &str) {
    match kind {
        EngineKind::Docker => {
            let _ = Command::new(bin).args(["rm", "-f", id]).status();
        }
        EngineKind::Apple => {
            // Apple's `container` has no `rm -f`; stop then remove, ignoring
            // "no such container" on a first run.
            let _ = Command::new(bin).args(["stop", id]).status();
            let _ = Command::new(bin).args(["rm", id]).status();
        }
    }
}

/// Print the launcher-appropriate "where/how" summary after a detached launch.
fn print_where_how(bin: &str, dir: &Path, m: &ContainerManifest, server_url: &str) {
    println!(
        "── launched {} ({}) ──────────────",
        m.name, m.launcher_type
    );
    println!("server:   {server_url}");
    match m.launcher_type {
        Launcher::Webapp => {
            let port = m.port.unwrap_or(9090);
            println!("open the chat UI:  http://localhost:{port}");
        }
        Launcher::Api => {
            let port = m.port.unwrap_or(9090);
            let field = read_prompt_field(dir, m).unwrap_or_else(|| "prompt".to_string());
            println!("trigger it:");
            println!(
                "  curl --no-buffer -X POST http://localhost:{port}/agents/{}/trigger \\",
                m.name
            );
            println!("    -H 'content-type: application/json' \\");
            println!("    -d '{{\"{field}\": \"your prompt here\"}}'");
        }
        Launcher::Schedule => {
            let cron =
                read_schedule(dir, m).unwrap_or_else(|| "(see bae-schedules.toml)".to_string());
            println!("cron schedule:  {cron}");
        }
        Launcher::Local => {}
    }
    println!("logs:     {bin} logs -f {}", m.id);
}

/// Parse the generated launcher config TOML alongside the build artifact.
fn read_launcher_toml(dir: &Path, m: &ContainerManifest) -> Option<toml::Value> {
    let file = match m.launcher_type {
        Launcher::Api => "bae-api.toml",
        Launcher::Webapp => "bae-app.toml",
        Launcher::Schedule => "bae-schedules.toml",
        Launcher::Local => return None,
    };
    let text = std::fs::read_to_string(artifact_dir(dir, &m.id).join(file)).ok()?;
    toml::from_str(&text).ok()
}

/// The request field name (`request_schema.required[0]`) an api/webapp trigger
/// expects, so the printed curl is genuinely ready to copy.
fn read_prompt_field(dir: &Path, m: &ContainerManifest) -> Option<String> {
    let v = read_launcher_toml(dir, m)?;
    v.get("agents")?
        .as_array()?
        .first()?
        .get("request_schema")?
        .get("required")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

/// The cron expression a schedule launcher runs on.
fn read_schedule(dir: &Path, m: &ContainerManifest) -> Option<String> {
    let v = read_launcher_toml(dir, m)?;
    v.get("agents")?
        .as_array()?
        .first()?
        .get("schedule")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::manifest::{Requires, Sdk};

    fn container(id: &str, port: Option<u16>, env: &[&str]) -> ContainerManifest {
        ContainerManifest {
            id: id.to_string(),
            name: "reference-assistant".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            launcher_type: Launcher::Api,
            image_tag: format!("{id}:latest"),
            harness_build_image: format!("{id}-harness-build"),
            port,
            requires: Requires {
                allowed_tools: Vec::new(),
                mcp_servers: Vec::new(),
                env: env.iter().map(|s| s.to_string()).collect(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// Regression: a plaintext client key must never reach the engine's argv,
    /// where any local user can read it out of `ps` / `/proc/<pid>/cmdline` for
    /// the lifetime of the launch. Every value-bearing variable travels in the
    /// `0600` `harness.env` instead; the argv carries only file paths, the
    /// container name, the published port and the image tag.
    #[test]
    fn container_launch_argv_never_carries_a_secret_value() {
        let m = container("ref-rust-api", Some(9090), &["GITHUB_TOKEN"]);
        let args = container_run_args(
            EngineKind::Docker,
            &m,
            Some("/work/.env".to_string()),
            "/work/.baectl/builds/ref-rust-api/harness.env",
        );
        let joined = args.join(" ");
        for forbidden in [
            "BAE_CLIENT_KEY=",
            "BAE_SERVER_URL=",
            "bae_secret",
            "ghp_secret",
        ] {
            assert!(
                !joined.contains(forbidden),
                "launch argv leaked `{forbidden}`: {joined}"
            );
        }
        // Both env files are still delivered, harness.env last so its entries
        // win over any same-named key in the workspace `.env`.
        let dotenv_at = args.iter().position(|a| a == "/work/.env").unwrap();
        let harness_at = args
            .iter()
            .position(|a| a == "/work/.baectl/builds/ref-rust-api/harness.env")
            .unwrap();
        assert!(dotenv_at < harness_at);
        assert!(args.contains(&"--publish".to_string()));
        assert!(args.contains(&"9090:9090".to_string()));
        assert_eq!(args.last().unwrap(), "ref-rust-api:latest");

        // A schedule launcher publishes no port, and a workspace with no .env
        // simply omits that --env-file.
        let sched = container("ref-rust-schedule", None, &[]);
        let args = container_run_args(EngineKind::Docker, &sched, None, "/work/harness.env");
        assert!(!args.contains(&"--publish".to_string()));
        assert_eq!(
            args.iter().filter(|a| *a == "--env-file").count(),
            1,
            "only harness.env should be passed when no .env exists"
        );
    }

    /// The two `BAE_*` values are appended after the captured variables so the
    /// engine's last-wins `--env-file` ordering resolves them to `run`'s values.
    #[test]
    fn harness_env_body_carries_the_secret_and_puts_bae_vars_last() {
        let body = harness_env_body(
            &[("GITHUB_TOKEN".to_string(), "ghp_secret".to_string())],
            "http://host.docker.internal:8080",
            "bae_secret",
        );
        assert_eq!(
            body,
            "GITHUB_TOKEN=ghp_secret\n\
             BAE_SERVER_URL=http://host.docker.internal:8080\n\
             BAE_CLIENT_KEY=bae_secret\n"
        );
    }

    /// Readiness check #5 accepts the profile's provider auth-token var from the
    /// *host* environment for a container build, so `run` must capture it into
    /// `harness.env` — otherwise a host-only token silently never reaches the
    /// launched container and the harness fails its provider call.
    #[test]
    fn container_env_resolution_captures_a_host_only_provider_token() {
        let dir = std::env::temp_dir().join(format!(
            "baectl-run-provider-env-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(super::artifact_dir(&dir, "ref-rust-api")).unwrap();
        // Only the harness's own declared var is in `.env`; the provider token
        // exists solely in the host environment.
        std::fs::write(dir.join(ENV_FILE), "GITHUB_TOKEN=ghp_from_dotenv\n").unwrap();
        let var = "BAECTL_TEST_PROVIDER_TOKEN";
        std::env::set_var(var, "sk-host-only");

        let m = container("ref-rust-api", Some(9090), &["GITHUB_TOKEN"]);
        let captured = resolve_container_env(&dir, &m, Some(var)).unwrap();

        assert_eq!(
            captured,
            vec![(var.to_string(), "sk-host-only".to_string())],
            "the provider token must be captured; GITHUB_TOKEN is already \
             delivered by --env-file .env"
        );

        std::env::remove_var(var);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dotenv_has_detects_declared_keys() {
        let dir = std::env::temp_dir().join(format!("baectl-run-dotenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(ENV_FILE),
            "# comment\nGITHUB_TOKEN=abc\nOTHER = 1\n",
        )
        .unwrap();

        assert!(dotenv_has(&dir, "GITHUB_TOKEN"));
        assert!(dotenv_has(&dir, "OTHER"));
        assert!(!dotenv_has(&dir, "MISSING"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_key_requires_a_stored_plaintext() {
        let mut resolved = Resolved {
            profile_id: "pro_1".to_string(),
            profile_name: "default".to_string(),
            key_id: "key_1".to_string(),
            client_key_plaintext: None,
            server_url: "http://localhost:8080".to_string(),
            max_url: None,
            provider_env: None,
        };
        assert!(client_key(&resolved).is_err());
        resolved.client_key_plaintext = Some("bae_secret".to_string());
        assert_eq!(client_key(&resolved).unwrap(), "bae_secret");
    }

    #[test]
    fn read_prompt_field_and_schedule_from_generated_config() {
        use crate::harness::manifest::Sdk;
        let dir = std::env::temp_dir().join(format!("baectl-run-toml-{}", std::process::id()));
        let build = artifact_dir(&dir, "ref-rust-api");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("bae-api.toml"),
            "[[agents]]\nname = \"reference-assistant\"\ncommand = \"/usr/local/bin/reference-assistant\"\n\n[agents.request_schema]\ntype = \"object\"\nrequired = [\"AGENT_PROMPT\"]\n[agents.request_schema.properties.AGENT_PROMPT]\ntype = \"string\"\n",
        )
        .unwrap();

        let api = ContainerManifest {
            id: "ref-rust-api".to_string(),
            name: "reference-assistant".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            launcher_type: Launcher::Api,
            image_tag: "ref-rust-api:latest".to_string(),
            harness_build_image: "ref-rust-api-harness-build".to_string(),
            port: Some(9090),
            requires: Default::default(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert_eq!(
            read_prompt_field(&dir, &api).as_deref(),
            Some("AGENT_PROMPT")
        );

        let sched_build = artifact_dir(&dir, "ref-rust-schedule");
        std::fs::create_dir_all(&sched_build).unwrap();
        std::fs::write(
            sched_build.join("bae-schedules.toml"),
            "[[agents]]\nname = \"reference-assistant\"\ncommand = \"/usr/local/bin/reference-assistant\"\nschedule = \"0 0 3 * * *\"\n",
        )
        .unwrap();
        let sched = ContainerManifest {
            id: "ref-rust-schedule".to_string(),
            launcher_type: Launcher::Schedule,
            port: None,
            ..api
        };
        assert_eq!(read_schedule(&dir, &sched).as_deref(), Some("0 0 3 * * *"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
