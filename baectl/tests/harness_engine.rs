//! Opt-in engine coverage for the WI 0017 build/ready/run first-run flow.
//!
//! These tests are deliberately offline by default.  Set
//! `BAECTL_HARNESS_ENGINE_TESTS=1` on a Docker host which has built the local
//! server and launcher images, and provide `ANTHROPIC_API_KEY` for the bundled
//! reference assistant's real provider turn.  The gate is intentionally more
//! conservative than a simple `docker` presence check: normal `make test` and
//! `make test-baectl` must never pull images, create containers, or call a
//! provider merely because Docker happens to be installed.

#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);
static ENGINE_TEST_LOCK: Mutex<()> = Mutex::new(());

const ENGINE_TESTS_ENV: &str = "BAECTL_HARNESS_ENGINE_TESTS";
const PROVIDER_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const DEV_SERVER_IMAGE: &str = "better-agent-engine:latest";
const DEV_API_IMAGE: &str = "better-agent-engine:launcher-api";

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "baectl-harness-engine-{label}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // `setup` uses a directory-derived Compose project name, so this tears
        // down only the isolated test server and its test-local volume.
        let _ = Command::new("docker")
            .args(["compose", "down", "-v", "--remove-orphans"])
            .current_dir(&self.0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lock_engine_tests() -> MutexGuard<'static, ()> {
    ENGINE_TEST_LOCK.lock().expect("engine test lock poisoned")
}

fn enabled() -> bool {
    std::env::var_os(ENGINE_TESTS_ENV).as_deref() == Some(OsStr::new("1"))
}

fn docker_ready() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn image_present(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn provider_key() -> Option<OsString> {
    std::env::var_os(PROVIDER_KEY_ENV).filter(|value| !value.is_empty())
}

/// Print one actionable skip reason instead of making a developer infer why a
/// live test did not run.  Returning from a `#[test]` is Rust's established
/// skippable-test posture (also used by `setup_engine.rs`).
fn require_live_fixture(api_launcher: bool) -> Option<OsString> {
    if !enabled() {
        eprintln!("skipping WI 0017 engine test: set {ENGINE_TESTS_ENV}=1 to opt in");
        return None;
    }
    if !docker_ready() {
        eprintln!("skipping WI 0017 engine test: Docker is unavailable or its daemon is down");
        return None;
    }
    if !image_present(DEV_SERVER_IMAGE) {
        eprintln!("skipping WI 0017 engine test: build {DEV_SERVER_IMAGE} with `make image`");
        return None;
    }
    if api_launcher && !image_present(DEV_API_IMAGE) {
        eprintln!(
            "skipping WI 0017 API engine test: build {DEV_API_IMAGE} with `make image-launcher-api`"
        );
        return None;
    }
    match provider_key() {
        Some(key) => Some(key),
        None => {
            eprintln!(
                "skipping WI 0017 engine test: {PROVIDER_KEY_ENV} is required for the bundled reference assistant"
            );
            None
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("baectl has workspace parent")
        .to_path_buf()
}

fn link_bundled_rust_client(dir: &Path) {
    link_bundled_client(dir, "rust");
}

/// Link one SDK's bundled examples into an isolated `--dir` so bundled-harness
/// resolution (`<dir>/client-<sdk>/examples/<name>`) finds them.
fn link_bundled_client(dir: &Path, sdk: &str) {
    symlink(
        workspace_root().join(format!("client-{sdk}")),
        dir.join(format!("client-{sdk}")),
    )
    .unwrap_or_else(|err| panic!("link bundled {sdk} harness into isolated --dir: {err}"));
}

/// Best-effort teardown of a `run`-launched container so the next engine-gated
/// test can bind the same published port.
fn remove_container(name: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Run the actual interactive `setup --dev` wizard under a pseudoterminal.
/// The provider key is inherited from the process and captured by setup without
/// prompting, so no credential is ever written into this test's answer stream.
fn interactive_setup(dir: &Path, provider_key: &OsStr) {
    let binary = env!("CARGO_BIN_EXE_baectl");
    let command = format!(
        "cd {} && {} setup --dev --dir {}",
        shell_quote(&dir.display().to_string()),
        shell_quote(binary),
        shell_quote(&dir.display().to_string())
    );
    let mut child = Command::new("script")
        .args(["-qefc", &command, "/dev/null"])
        .env(PROVIDER_KEY_ENV, provider_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("script is available on Unix CI/dev hosts");

    // apple=no; standard and all provider defaults; no extra provider; no MCP
    // server; all BAE_* defaults; launch=yes.  The provider credential is
    // skipped because `collect_secret` discovers it in the inherited env.
    let answers = ["n", "", "", "", "", "", "n", "n", "", "", "", "", "", "y"].join("\n");
    child
        .stdin
        .take()
        .expect("setup stdin")
        .write_all(format!("{answers}\n").as_bytes())
        .expect("write setup answers");
    let output = child.wait_with_output().expect("wait for setup");
    assert_success("setup --dev", &output);
}

fn baectl(dir: &Path, provider_key: &OsStr, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_baectl"))
        .args(args)
        .arg("--dir")
        .arg(dir)
        .env(PROVIDER_KEY_ENV, provider_key)
        .output()
        .unwrap_or_else(|err| panic!("run baectl {args:?}: {err}"))
}

/// `ready --fix` deliberately asks for an affirmative confirmation even when
/// stdin is not a TTY, so drive that real prompt instead of silently relying on
/// `run`'s auto-fix path.
fn baectl_with_stdin(dir: &Path, provider_key: &OsStr, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_baectl"))
        .args(args)
        .arg("--dir")
        .arg(dir)
        .env(PROVIDER_KEY_ENV, provider_key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("run baectl {args:?}: {err}"));
    child
        .stdin
        .take()
        .expect("baectl stdin")
        .write_all(stdin)
        .expect("write baectl stdin");
    child.wait_with_output().expect("wait for baectl command")
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn compose_baectl_json(dir: &Path, resource: &str) -> String {
    let output = Command::new("docker")
        .args([
            "compose", "exec", "-T", "baesrv", "baectl", "list", resource, "--json",
        ])
        .current_dir(dir)
        .output()
        .expect("list admin resource through compose");
    assert_success(&format!("list {resource}"), &output);
    String::from_utf8(output.stdout).expect("admin JSON is UTF-8")
}

/// The mutable admin resources a fully provisioned `run` must not touch.
/// Profiles are compared whole (an unnecessary `update profile` bumps
/// `updated_at`); keys are projected to `{id, profile_id, name}` because
/// `last_used_at` legitimately advances whenever a harness authenticates.
fn admin_snapshot(dir: &Path) -> (String, serde_json::Value) {
    (
        compose_baectl_json(dir, "profiles"),
        project_keys(&compose_baectl_json(dir, "keys")),
    )
}

/// Project `list keys --json` output to `[{id, profile_id, name}]`.
fn project_keys(keys_json: &str) -> serde_json::Value {
    let keys: serde_json::Value = serde_json::from_str(keys_json).expect("keys JSON");
    let items = keys.as_array().cloned().unwrap_or_default();
    serde_json::Value::Array(
        items
            .iter()
            .map(|k| {
                serde_json::json!({
                    "id": k.get("id"),
                    "profile_id": k.get("profile_id"),
                    "name": k.get("name"),
                })
            })
            .collect(),
    )
}

fn assert_private(path: &Path) {
    let mode = fs::metadata(path)
        .unwrap_or_else(|err| panic!("metadata for {}: {err}", path.display()))
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{} must be mode 0600", path.display());
}

#[test]
fn setup_build_ready_run_local_prints_a_final_answer_and_second_run_is_mutation_free() {
    let _lock = lock_engine_tests();
    let Some(key) = require_live_fixture(false) else {
        return;
    };
    let dir = TempDir::new("local-lifecycle");
    link_bundled_rust_client(dir.path());

    interactive_setup(dir.path(), &key);
    let build = baectl(
        dir.path(),
        &key,
        &[
            "build",
            "reference-assistant",
            "--launcher",
            "local",
            "--dev",
        ],
    );
    assert_success("build reference-assistant --launcher local --dev", &build);

    let ready = baectl_with_stdin(
        dir.path(),
        &key,
        &["ready", "reference-assistant-rust-local", "--fix", "--dev"],
        b"y\n",
    );
    assert_success("ready", &ready);

    let first_run = baectl(
        dir.path(),
        &key,
        &["run", "reference-assistant-rust-local", "--dev"],
    );
    assert_success("first local run", &first_run);
    let first_stdout = String::from_utf8_lossy(&first_run.stdout);
    let final_answer = first_stdout
        .split("(Ctrl-C to stop)")
        .nth(1)
        .unwrap_or_default()
        .trim();
    assert!(
        !final_answer.is_empty(),
        "the bundled harness did not print a final answer\nstdout:\n{first_stdout}"
    );

    // Snapshot the full mutable admin resources, not just their counts: an
    // unnecessary `update profile` is therefore caught along with extra key or
    // profile creation.
    let before_second_run = admin_snapshot(dir.path());
    let second_run = baectl(
        dir.path(),
        &key,
        &["run", "reference-assistant-rust-local", "--dev"],
    );
    assert_success("second local run", &second_run);
    assert_eq!(
        admin_snapshot(dir.path()),
        before_second_run,
        "a fully provisioned second run made an admin-API mutation"
    );
}

#[test]
fn api_container_run_streams_the_printed_curl_endpoint_replaces_its_container_and_keeps_secret_files_private(
) {
    let _lock = lock_engine_tests();
    let Some(key) = require_live_fixture(true) else {
        return;
    };
    let dir = TempDir::new("api-lifecycle");
    link_bundled_rust_client(dir.path());

    interactive_setup(dir.path(), &key);
    let id = "reference-assistant-rust-api";
    remove_container(id);
    let build = baectl(
        dir.path(),
        &key,
        &["build", "reference-assistant", "--launcher", "api", "--dev"],
    );
    assert_success("build reference-assistant --launcher api --dev", &build);
    let ready = baectl_with_stdin(dir.path(), &key, &["ready", id, "--fix", "--dev"], b"y\n");
    assert_success("ready --fix", &ready);
    assert_private(
        &dir.path()
            .join(".baectl/builds")
            .join(id)
            .join("resolved.json"),
    );

    let first_run = baectl(dir.path(), &key, &["run", id, "--dev"]);
    assert_success("first API run", &first_run);
    let first_stdout = String::from_utf8_lossy(&first_run.stdout);
    assert!(
        first_stdout.contains(
            "curl --no-buffer -X POST http://localhost:9090/agents/reference-assistant/trigger"
        ) && first_stdout.contains("AGENT_PROMPT"),
        "API run did not print its copyable curl command:\n{first_stdout}"
    );
    assert_private(
        &dir.path()
            .join(".baectl/builds")
            .join(id)
            .join("harness.env"),
    );

    // This is the printed curl endpoint, driven with the generated
    // `AGENT_PROMPT` request field. `--retry-connrefused` accounts for the
    // detached launcher process reaching its listening state after `run`
    // returns, while `--no-buffer` keeps the regression focused on streaming.
    let curl = Command::new("curl")
        .args([
            "--fail",
            "--no-buffer",
            "--retry",
            "20",
            "--retry-connrefused",
            "--max-time",
            "180",
            "-X",
            "POST",
            "http://localhost:9090/agents/reference-assistant/trigger",
            "-H",
            "content-type: application/json",
            "-d",
            r#"{"AGENT_PROMPT":"Reply with a concise confirmation that the API harness is running."}"#,
        ])
        .output()
        .expect("curl is available on Unix CI/dev hosts");
    assert_success("printed API curl command", &curl);
    let streamed = String::from_utf8_lossy(&curl.stdout);
    assert!(
        streamed.contains("[reference-assistant]")
            && streamed.trim().len() > "[reference-assistant]".len(),
        "API endpoint did not return streamed harness output:\n{streamed}"
    );

    let first_container_id = docker_container_id(id);
    let second_run = baectl(dir.path(), &key, &["run", id, "--dev"]);
    assert_success("second API run", &second_run);
    let second_container_id = docker_container_id(id);
    assert_ne!(
        first_container_id, second_container_id,
        "second API run reused rather than replaced the named container"
    );

    // The client key is delivered exclusively through the 0600 harness.env
    // (never as a `--env BAE_CLIENT_KEY=…` argv element visible in `ps`), so
    // prove the file really carries it after a live launch — the runtime half
    // of `run.rs`'s `container_launch_argv_never_carries_a_secret_value`.
    let harness_env = fs::read_to_string(
        dir.path()
            .join(".baectl/builds")
            .join(id)
            .join("harness.env"),
    )
    .expect("read harness.env after a live container run");
    assert!(
        harness_env
            .lines()
            .any(|line| line.starts_with("BAE_CLIENT_KEY=") && line.len() > "BAE_CLIENT_KEY=".len()),
        "harness.env must carry the client key that `run` kept out of the launch argv"
    );

    remove_container(id);
}

/// The blocker regression for the generated per-SDK default: an *interpreted*
/// SDK's bundled example must package and actually run on the Debian-slim
/// launcher base, which carries neither Node nor Python. Before the generated
/// default staged a runnable tree plus a shim, this build produced an image
/// whose every trigger failed to spawn its harness.
#[test]
fn typescript_reference_assistant_packages_and_answers_through_the_api_launcher() {
    let _lock = lock_engine_tests();
    let Some(key) = require_live_fixture(true) else {
        return;
    };
    let dir = TempDir::new("ts-api-lifecycle");
    link_bundled_client(dir.path(), "typescript");

    interactive_setup(dir.path(), &key);
    let id = "reference-assistant-typescript-api";
    // The Rust API test publishes the same host port; make sure nothing it left
    // behind is still bound.
    remove_container("reference-assistant-rust-api");
    remove_container(id);

    let build = baectl(
        dir.path(),
        &key,
        &[
            "build",
            "reference-assistant",
            "--sdk",
            "typescript",
            "--launcher",
            "api",
            "--dev",
        ],
    );
    assert_success(
        "build reference-assistant --sdk typescript --launcher api --dev",
        &build,
    );

    let run = baectl(dir.path(), &key, &["run", id, "--dev"]);
    assert_success("TypeScript API run", &run);

    let curl = Command::new("curl")
        .args([
            "--fail",
            "--no-buffer",
            "--retry",
            "20",
            "--retry-connrefused",
            "--max-time",
            "180",
            "-X",
            "POST",
            "http://localhost:9090/agents/reference-assistant/trigger",
            "-H",
            "content-type: application/json",
            "-d",
            r#"{"AGENT_PROMPT":"Reply with a concise confirmation that the TypeScript harness is running."}"#,
        ])
        .output()
        .expect("curl is available on Unix CI/dev hosts");
    assert_success("TypeScript API trigger", &curl);
    let streamed = String::from_utf8_lossy(&curl.stdout);
    assert!(
        streamed.contains("[reference-assistant]")
            && streamed.trim().len() > "[reference-assistant]".len(),
        "packaged TypeScript harness produced no output — it likely failed to spawn \
         inside the launcher image:\n{streamed}"
    );

    remove_container(id);
}

fn docker_container_id(name: &str) -> String {
    let output = Command::new("docker")
        .args(["container", "inspect", "--format", "{{.Id}}", name])
        .output()
        .expect("inspect launched container");
    assert_success("inspect launched API container", &output);
    String::from_utf8(output.stdout)
        .expect("container ID is UTF-8")
        .trim()
        .to_string()
}

#[test]
fn generated_container_build_succeeds_with_a_path_that_contains_docker_but_no_host_rust_toolchain()
{
    let _lock = lock_engine_tests();
    if !enabled() {
        eprintln!("skipping WI 0017 host-toolchain regression: set {ENGINE_TESTS_ENV}=1 to opt in");
        return;
    }
    if !docker_ready() {
        eprintln!("skipping WI 0017 host-toolchain regression: Docker is unavailable or its daemon is down");
        return;
    }

    let dir = TempDir::new("no-host-toolchain");
    link_bundled_rust_client(dir.path());
    let path_bin = dir.path().join("engine-only-path");
    fs::create_dir_all(&path_bin).expect("create restricted PATH directory");
    let docker = find_path_executable("docker").expect("Docker passed the engine gate");
    symlink(docker, path_bin.join("docker")).expect("place Docker alone on PATH");

    // These checks make the regression real: the child gets an intentionally
    // restricted PATH, rather than merely observing that a build happened to
    // succeed while Cargo/Rustc remained available elsewhere on the host.
    for tool in ["cargo", "rustc"] {
        let result = Command::new(tool).env("PATH", &path_bin).output();
        assert!(
            result.is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound),
            "{tool} unexpectedly resolves on the restricted PATH {}",
            path_bin.display()
        );
    }

    let output = Command::new(env!("CARGO_BIN_EXE_baectl"))
        .args([
            "build",
            "reference-assistant",
            "--launcher",
            "api",
            "--id",
            "reference-assistant-no-host-rust-api",
            "--dir",
        ])
        .arg(dir.path())
        .env("PATH", &path_bin)
        .output()
        .expect("run baectl with engine-only PATH");
    assert_success("container build with no host cargo/rustc on PATH", &output);
}

fn find_path_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .find_map(|entry| {
            let candidate = Path::new(entry).join(name);
            candidate.is_file().then_some(candidate)
        })
}

#[test]
fn repository_ignores_baectl_local_state() {
    let gitignore = fs::read_to_string(workspace_root().join(".gitignore"))
        .expect("read repository .gitignore");
    assert!(
        gitignore.lines().any(|line| line.trim() == ".baectl/"),
        ".gitignore must cover plaintext-key-bearing .baectl/ state"
    );
}

/// Regression (B11): the mutation-free snapshot must ignore `last_used_at`,
/// which the server advances every time the harness authenticates — the
/// engine-gated second-`run` assertion compared it and failed spuriously.
/// Pure, so it runs without the engine gate.
#[test]
fn key_snapshot_projection_ignores_last_used_at_but_not_identity() {
    let before = r#"[{"id":"key_1","profile_id":"pro_1","name":"default","prefix":"bae_a","created_at":"t0","last_used_at":null}]"#;
    let used = r#"[{"id":"key_1","profile_id":"pro_1","name":"default","prefix":"bae_a","created_at":"t0","last_used_at":"2026-09-23T10:00:00Z"}]"#;
    assert_eq!(project_keys(before), project_keys(used));
    assert_eq!(
        project_keys(before),
        serde_json::json!([{"id": "key_1", "profile_id": "pro_1", "name": "default"}])
    );

    // A new key, a rebinding or a rename is still a mutation.
    let added = r#"[{"id":"key_1","profile_id":"pro_1","name":"default"},{"id":"key_2","profile_id":"pro_1","name":"x"}]"#;
    let rebound = r#"[{"id":"key_1","profile_id":"pro_2","name":"default"}]"#;
    let renamed = r#"[{"id":"key_1","profile_id":"pro_1","name":"other"}]"#;
    for changed in [added, rebound, renamed] {
        assert_ne!(project_keys(before), project_keys(changed), "{changed}");
    }
}
