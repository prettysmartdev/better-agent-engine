//! Offline CLI regression tests for WI 0018 (B2–B11, C1, C2).
//!
//! These drive the real `baectl` binary against `tests/fixtures/fake-engine.sh`
//! installed on `PATH` as both `docker` and `container`. The fake records every
//! invocation and emulates the in-container admin surface over JSON-lines files,
//! so readiness exit codes, what was (not) mutated, the cwd/argv the engine was
//! driven with, and the files `build`/`run`/`setup` write are all observable
//! with no container engine. Nothing here needs Docker; the engine-backed
//! coverage stays in `harness_engine.rs` / `setup_engine.rs` behind
//! `BAECTL_HARNESS_ENGINE_TESTS`.

#![cfg(unix)]

use std::borrow::BorrowMut;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

static SERIAL: AtomicUsize = AtomicUsize::new(0);

const FAKE_ENGINE: &str = include_str!("fixtures/fake-engine.sh");
const DOCKERIGNORE: &str = "target/\nnode_modules/\n.venv/\n__pycache__/\n.baectl/\n.git/\n";

/// A captured `baectl` invocation.
#[derive(Debug)]
struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn marks(&self) -> Vec<char> {
        self.stdout
            .lines()
            .filter_map(|l| l.chars().next())
            .filter(|c| matches!(c, '✓' | '⚠' | '✗'))
            .collect()
    }
}

/// An isolated workspace: `work/` (the `--dir`), `harness/` (a
/// `--harness-dir`), `bin/` (the fake engine, first on `PATH`), `state/` (the
/// fake's admin state and call log) and `empty-bin/` (a `PATH` with nothing).
struct Ws {
    root: PathBuf,
}

impl Drop for Ws {
    fn drop(&mut self) {
        // Undo any permission clamp a test applied so cleanup can recurse.
        let _ = Command::new("chmod")
            .args(["-R", "u+rwx"])
            .arg(&self.root)
            .status();
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Ws {
    fn new(label: &str) -> Ws {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!(
                "baectl-cli-offline-{label}-{}-{serial}",
                std::process::id()
            ))
            .to_path_buf();
        let _ = fs::remove_dir_all(&root);
        for sub in ["work", "harness", "bin", "state", "empty-bin"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        for name in ["docker", "container"] {
            write_exe(&root.join("bin").join(name), FAKE_ENGINE);
        }
        Ws { root }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("work")
    }
    fn harness_dir(&self) -> PathBuf {
        self.root.join("harness")
    }
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }
    fn artifact(&self, id: &str) -> PathBuf {
        self.dir().join(".baectl/builds").join(id)
    }

    /// `PATH` with the fake engine first, then the host's own (for `sh`, `env`…).
    fn fake_path(&self) -> String {
        format!(
            "{}:{}",
            self.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// A `baectl` command with the fake engine on PATH and no provider keys
    /// inherited from the test process.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_baectl"));
        c.args(args)
            .current_dir(&self.root)
            .env("PATH", self.fake_path())
            .env("FAKE_ENGINE_STATE", self.state())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("BAE_PROVIDER_KEY_ENV")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, mut c: impl BorrowMut<Command>) -> Out {
        output(c.borrow_mut(), None)
    }

    fn run_stdin(&self, mut c: impl BorrowMut<Command>, stdin: &str) -> Out {
        output(c.borrow_mut(), Some(stdin))
    }

    /// Scaffold what `setup` leaves behind for the harness verbs: a compose
    /// launcher (so the Docker engine is detected) and a registry with one
    /// provider.
    fn server(&self, provider: &str, kind: &str, auth_env: &str) {
        fs::write(
            self.dir().join("docker-compose.yml"),
            "services:\n  baesrv:\n    image: better-agent-engine:latest\n",
        )
        .unwrap();
        self.config(&format!(
            "[mcp]\n\n[providers]\n\n[[providers.entries]]\nname = \"{provider}\"\n\
             provider = \"{kind}\"\nmodel = \"m\"\nauth_token = \"${{{auth_env}}}\"\n"
        ));
    }

    fn config(&self, text: &str) {
        fs::write(self.dir().join("bae-config.toml"), text).unwrap();
    }

    /// Record `name` as the profile `setup` created/reused, as its launch step
    /// does in `<dir>/.baectl/setup.json`.
    fn setup_profile(&self, name: &str) {
        let state = self.dir().join(".baectl");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("setup.json"),
            json!({ "profile": name }).to_string(),
        )
        .unwrap();
    }

    fn add_profile(&self, profile: Value) {
        append_line(&self.state().join("profiles.jsonl"), &profile.to_string());
    }

    fn add_key(&self, key: Value) {
        append_line(&self.state().join("keys.jsonl"), &key.to_string());
    }

    fn jsonl(&self, file: &str) -> Vec<Value> {
        fs::read_to_string(self.state().join(file))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn keys(&self) -> Vec<Value> {
        self.jsonl("keys.jsonl")
    }

    fn profiles(&self) -> Vec<Value> {
        self.jsonl("profiles.jsonl")
    }

    /// Every fake-engine invocation, as `cwd=<cwd> argv=<argv>`.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.state().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The admin *mutations* the fake received (`create …` / `update …`).
    fn mutations(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.contains(" baectl create ") || c.contains(" baectl update "))
            .collect()
    }

    fn harness(&self, toml: &str) {
        fs::write(self.harness_dir().join("bae-harness.toml"), toml).unwrap();
    }

    /// `build` the `--harness-dir` harness as `id` (asserting success).
    fn build(&self, id: &str, launcher: &str) -> Out {
        let mut c = self.cmd(&["build", "probe", "--launcher", launcher, "--id", id]);
        c.arg("--harness-dir")
            .arg(self.harness_dir())
            .arg("--dir")
            .arg(self.dir());
        let out = self.run(&mut c);
        assert_eq!(out.code, 0, "build {id} failed: {out:?}");
        out
    }

    fn ready(&self, id: &str, extra: &[&str]) -> Command {
        let mut c = self.cmd(&["ready", id]);
        c.args(extra).arg("--dir").arg(self.dir());
        c
    }

    fn run_cmd(&self, id: &str) -> Command {
        let mut c = self.cmd(&["run", id]);
        c.arg("--dir").arg(self.dir());
        c
    }
}

fn output(c: &mut Command, stdin: Option<&str>) -> Out {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        c.stdin(Stdio::piped());
    }
    let mut child = c.spawn().expect("spawn baectl");
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn write_exe(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn append_line(path: &Path, line: &str) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{line}").unwrap();
}

fn set_mtime(path: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| fs::File::open(path))
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// A profile shaped like the admin API's, compact so the fake can match it.
fn profile(id: &str, name: &str, primary: &str, tools: &[&str]) -> Value {
    json!({
        "id": id,
        "name": name,
        "primary_provider": primary,
        "fallback_providers": [],
        "allowed_tools": tools,
        "mcp_servers": [],
        "available_sandboxes": [],
    })
}

fn key(id: &str, profile_id: &str, name: &str) -> Value {
    json!({"id": id, "profile_id": profile_id, "name": name, "prefix": "bae_x", "last_used_at": null})
}

/// A local-only harness needing one tool, whose `run` dumps its environment to
/// `$BAECTL_TEST_CAPTURE`.
const PROBE_HARNESS: &str = r#"[harness]
name = "probe"
sdk = "rust"
run = "env > \"$BAECTL_TEST_CAPTURE\""

[harness.requires]
allowed_tools = ["get_current_time"]
"#;

/// [`PROBE_HARNESS`] plus container packaging (`working_dir = "."`, so the
/// harness dir itself is the generated build's context).
const PROBE_CONTAINER_HARNESS: &str = r#"[harness]
name = "probe"
sdk = "rust"
run = "env > \"$BAECTL_TEST_CAPTURE\""

[harness.requires]
allowed_tools = ["get_current_time"]

[harness.launcher]
prompt_env = "AGENT_PROMPT"
"#;

fn exit3_message(id: &str) -> String {
    format!(
        "baectl: all remaining issues are auto-fixable — run `baectl run {id}` (fixes them \
         without prompting) or `baectl ready {id} --fix`\n"
    )
}

// ---------------------------------------------------------------------------
// B3 — evaluate every check, gate, then mutate
// ---------------------------------------------------------------------------

/// Regression (B3): `run` used to apply the #2/#4 fixes before discovering a
/// blocking #5 failure, minting a key (and widening a profile) for a launch
/// that then aborted.
#[test]
fn failed_run_on_a_blocking_check_creates_no_key_and_widens_nothing() {
    let ws = Ws::new("b3-run");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.add_key(key("key_1", "pro_1", "default"));
    ws.harness(&PROBE_HARNESS.replace(
        "allowed_tools = [\"get_current_time\"]",
        "allowed_tools = [\"get_current_time\"]\nenv = [\"BAECTL_TEST_NEVER_SET\"]",
    ));
    ws.build("probe-local", "local");
    let keys_before = ws.keys().len();
    let profiles_before = ws.profiles();

    let out = ws.run(
        ws.run_cmd("probe-local")
            .env("ANTHROPIC_API_KEY", "sk-ant-test")
            .env("BAECTL_TEST_CAPTURE", ws.root.join("capture.env")),
    );

    assert_eq!(out.code, 1, "{out:?}");
    assert!(out
        .stdout
        .contains("✗ required env vars resolvable (missing: BAECTL_TEST_NEVER_SET)"));
    assert_eq!(
        ws.keys().len(),
        keys_before,
        "a failed run must create no key"
    );
    assert_eq!(
        ws.profiles(),
        profiles_before,
        "a failed run must widen nothing"
    );
    assert!(ws.mutations().is_empty(), "mutations: {:?}", ws.mutations());
    assert!(!ws.artifact("probe-local").join("resolved.json").exists());
    assert!(
        !ws.root.join("capture.env").exists(),
        "the harness must not start"
    );
}

/// B3: `ready` without `--fix` reports the fixes it would apply but never
/// performs one, even when everything is fixable.
#[test]
fn ready_without_fix_performs_no_mutation() {
    let ws = Ws::new("b3-ready");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));

    assert_eq!(out.code, 3, "{out:?}");
    assert!(ws.mutations().is_empty(), "mutations: {:?}", ws.mutations());
    assert!(ws.keys().is_empty());
    assert_eq!(
        ws.profiles(),
        vec![profile("pro_1", "default", "anthropic-default", &[])]
    );
}

/// B3: a key's plaintext is shown once, so no key (and no profile widening)
/// may happen when `resolved.json` cannot then be written.
#[test]
fn unwritable_build_dir_refuses_to_create_a_key_before_any_mutation() {
    let ws = Ws::new("b3-unwritable");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");
    let artifact = ws.artifact("probe-local");
    // A read-only mode alone does not stop root (e.g. CI containers), so also
    // occupy the write probe's path with a directory, which no user can
    // overwrite as a file.
    let blocker = artifact.join(".resolved.probe");
    fs::create_dir(&blocker).unwrap();
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o555)).unwrap();

    let out = ws.run(ws.run_cmd("probe-local").env("ANTHROPIC_API_KEY", "sk"));
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir(&blocker).unwrap();

    assert_eq!(out.code, 1, "{out:?}");
    assert!(
        out.stderr
            .contains("refusing to create a client key whose secret could not be saved"),
        "{out:?}"
    );
    assert!(ws.keys().is_empty());
    assert!(ws.mutations().is_empty(), "mutations: {:?}", ws.mutations());
}

// ---------------------------------------------------------------------------
// Setup's recorded profile (`.baectl/setup.json`)
// ---------------------------------------------------------------------------

/// `setup` may create a profile under a non-`default` name when `default`
/// already exists; `ready`/`run` must then pick *that* profile over another
/// compatible one.
#[test]
fn ready_prefers_the_profile_setup_recorded_among_compatible_ones() {
    let ws = Ws::new("setup-profile-compatible");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile(
        "pro_1",
        "default",
        "anthropic-default",
        &["get_current_time"],
    ));
    ws.add_profile(profile(
        "pro_2",
        "team-a",
        "anthropic-default",
        &["get_current_time"],
    ));
    ws.setup_profile("team-a");
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stdout
            .lines()
            .any(|l| l == "✓ compatible profile (pro_2, team-a)"),
        "{}",
        out.stdout
    );
}

/// With no compatible profile, the widen fix targets setup's recorded profile
/// rather than one named `default`.
#[test]
fn ready_widens_the_profile_setup_recorded() {
    let ws = Ws::new("setup-profile-widen");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.add_profile(profile("pro_2", "team-a", "anthropic-default", &[]));
    ws.setup_profile("team-a");
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stdout
            .contains("update profile pro_2 anthropic-default --name team-a"),
        "{}",
        out.stdout
    );
}

// ---------------------------------------------------------------------------
// B7 — ⚠ vs ✗, exit codes 0/3/1, copy-pasteable hints
// ---------------------------------------------------------------------------

#[test]
fn ready_with_only_fixable_checks_prints_warnings_hints_and_exits_3() {
    let ws = Ws::new("b7-fixable");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));

    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.marks(), vec!['✓', '⚠', '✓', '⚠', '✓'], "{}", out.stdout);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines.contains(&"⚠ compatible profile — will be fixed by run"));
    assert!(lines.contains(&"⚠ client key with a stored secret — will be fixed by run"));
    assert_eq!(out.stderr, exit3_message("probe-local"));

    // `--dir` is not the cwd, so the hint names the compose file (absolute).
    let exec = format!(
        "docker compose -f {}/docker-compose.yml exec -T baesrv baectl",
        ws.dir().display()
    );
    let widen = format!(
        "    widen the existing profile (additive): {exec} update profile pro_1 \
         anthropic-default --name default --allowed-tool get_current_time  (or re-run with --fix)"
    );
    assert!(lines.contains(&widen.as_str()), "{}", out.stdout);
    let key_hint = format!(
        "    create a client key baectl can hand to `run`: {exec} create key probe-local pro_1  \
         (--fix also records the key for `run`)"
    );
    assert!(lines.contains(&key_hint.as_str()), "{}", out.stdout);
}

#[test]
fn ready_hint_uses_the_plain_exec_form_when_dir_is_the_cwd() {
    let ws = Ws::new("b7-cwd");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(
        ws.cmd(&["ready", "probe-local"])
            .current_dir(ws.dir())
            .env("ANTHROPIC_API_KEY", "sk"),
    );

    assert_eq!(out.code, 3, "{out:?}");
    assert!(
        out.stdout.contains(
            "widen the existing profile (additive): docker compose exec -T baesrv baectl update \
             profile pro_1"
        ),
        "{}",
        out.stdout
    );
}

#[test]
fn ready_hint_uses_container_exec_for_an_apple_setup() {
    let ws = Ws::new("b7-apple");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    fs::remove_file(ws.dir().join("docker-compose.yml")).unwrap();
    write_exe(&ws.dir().join("bae-setup.sh"), "#!/bin/sh\n");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));

    assert_eq!(out.code, 3, "{out:?}");
    assert!(out
        .stdout
        .contains("create a client key baectl can hand to `run`: container exec bae baectl create key probe-local pro_1"));
    // The Apple engine was driven as `container exec bae baectl …`.
    assert!(ws
        .calls()
        .iter()
        .any(|c| c.ends_with("argv=exec bae baectl list profiles --json")));
}

#[test]
fn ready_with_a_blocking_check_exits_1_with_or_without_fix() {
    let ws = Ws::new("b7-blocking");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(&PROBE_HARNESS.replace(
        "allowed_tools = [\"get_current_time\"]",
        "allowed_tools = [\"get_current_time\"]\nmcp_servers = [\"github\"]",
    ));
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out
        .stdout
        .lines()
        .any(|l| l == "✗ required MCP servers registered (missing: github)"));
    // #2/#4 are still fixable, so they stay ⚠ next to the blocking ✗.
    assert_eq!(out.marks(), vec!['✓', '⚠', '✗', '⚠', '✓']);
    assert_eq!(
        out.stderr,
        "baectl: some checks are blocking — follow the guidance above, then re-run \
         `baectl ready probe-local`\n"
    );

    let out = ws.run_stdin(
        ws.ready("probe-local", &["--fix"])
            .env("ANTHROPIC_API_KEY", "sk"),
        "y\n",
    );
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(
        out.stderr,
        "baectl: some checks are still unresolved (see above)\n"
    );
    assert!(ws.mutations().is_empty(), "blocked --fix must not mutate");
}

/// No profile and no provider to create one against: #2 (and so #4) cannot be
/// fixed by `run`, so both are ✗ rather than ⚠.
#[test]
fn ready_marks_an_impossible_profile_fix_as_blocking() {
    let ws = Ws::new("b7-impossible");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.config("[mcp]\n\n[providers]\n");
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]));

    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.marks(), vec!['✓', '✗', '✓', '✗', '✓'], "{}", out.stdout);
    assert!(out.stdout.lines().any(|l| l == "✗ compatible profile"));
}

#[test]
fn ready_reports_an_unreachable_server_as_blocking() {
    let ws = Ws::new("b7-unreachable");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    fs::write(ws.state().join("fail_list"), "").unwrap();
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]));

    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.marks(), vec!['✗']);
    assert!(out.stdout.starts_with("✗ server reachable\n"));
}

#[test]
fn ready_fix_declined_with_only_warnings_exits_3_and_mutates_nothing() {
    let ws = Ws::new("b7-declined");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run_stdin(
        ws.ready("probe-local", &["--fix"])
            .env("ANTHROPIC_API_KEY", "sk"),
        "n\n",
    );

    assert_eq!(out.code, 3, "{out:?}");
    assert!(out.stdout.contains("Apply the 2 safe fix(es) above? [y/N]"));
    assert_eq!(out.stderr, exit3_message("probe-local"));
    assert!(ws.mutations().is_empty());
}

/// `ready --fix` (accepted) widens + keys, exits 0; a second `ready` is all ✓,
/// exit 0, and mutation-free.
#[test]
fn ready_fix_accepted_exits_0_and_a_second_ready_is_all_green() {
    let ws = Ws::new("b7-accepted");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run_stdin(
        ws.ready("probe-local", &["--fix"])
            .env("ANTHROPIC_API_KEY", "sk"),
        "y\n",
    );
    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.contains("fixed: widened profile pro_1"));
    assert!(out
        .stdout
        .contains("fixed: created client key key_new1 for profile pro_1"));
    assert_eq!(ws.keys().len(), 1);
    let mutations = ws.mutations().len();

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(out.marks(), vec!['✓'; 5]);
    assert!(out
        .stdout
        .contains("✓ client key (key_new1) — reusing saved credential"));
    assert_eq!(
        ws.mutations().len(),
        mutations,
        "second ready must not mutate"
    );
}

/// Regression (B7 + C1): the documented fresh path — `setup --yes`, a bundled
/// `build`, then `ready` — reports only ⚠ (the `default` profile lacks the
/// tools; no key is recorded yet) and exits 3, never ✗.
#[test]
fn fresh_setup_yes_then_ready_reports_only_warnings_and_exits_3() {
    let ws = Ws::new("b7-fresh");
    // `setup --yes` with no engine on PATH writes the files, then stops.
    let empty = ws.root.join("empty-bin");
    let out = ws.run(
        ws.cmd(&["setup", "--yes", "--dir"])
            .arg(ws.dir())
            .env("PATH", &empty)
            .env("ANTHROPIC_API_KEY", "sk-ant-test"),
    );
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("docker not found on PATH"), "{out:?}");
    // …what setup's launch step would have created on a live server:
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.add_key(key("key_1", "pro_1", "default"));

    symlink(
        repo_root().join("client-rust"),
        ws.dir().join("client-rust"),
    )
    .unwrap();
    let out = ws.run(
        ws.cmd(&["build", "reference-assistant", "--dir"])
            .arg(ws.dir()),
    );
    assert_eq!(out.code, 0, "{out:?}");

    let out = ws.run(
        ws.ready("reference-assistant-rust-local", &[])
            .env("ANTHROPIC_API_KEY", "sk-ant-test"),
    );
    assert_eq!(out.code, 3, "{out:?}");
    assert_eq!(out.marks(), vec!['✓', '⚠', '✓', '⚠', '✓'], "{}", out.stdout);
    assert!(!out.stdout.contains('✗'));
}

// ---------------------------------------------------------------------------
// B2 — widening re-sends every list field
// ---------------------------------------------------------------------------

/// Regression (B2): the widening `update profile` is a full PUT; it must carry
/// every existing fallback, tool, MCP server and sandbox image, even though the
/// harness declares no sandboxes.
#[test]
fn run_widening_preserves_every_existing_list_value() {
    let ws = Ws::new("b2-widen");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(json!({
        "id": "pro_1",
        "name": "default",
        "primary_provider": "anthropic-default",
        "fallback_providers": ["openai-default"],
        "allowed_tools": ["existing_tool"],
        "mcp_servers": ["fs"],
        "available_sandboxes": ["python:3.12", "alpine"],
    }));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");

    let out = ws.run(
        ws.run_cmd("probe-local")
            .env("ANTHROPIC_API_KEY", "sk")
            .env("BAECTL_TEST_CAPTURE", ws.root.join("capture.env")),
    );
    assert_eq!(out.code, 0, "{out:?}");

    let update = ws
        .mutations()
        .into_iter()
        .find(|c| c.contains(" baectl update profile "))
        .expect("run widened the profile");
    for pair in [
        "--name default",
        "--fallback openai-default",
        "--allowed-tool existing_tool",
        "--allowed-tool get_current_time",
        "--mcp-server fs",
        "--available-sandbox python:3.12",
        "--available-sandbox alpine",
    ] {
        assert!(update.contains(pair), "missing `{pair}` in {update}");
    }
    let widened = &ws.profiles()[0];
    assert_eq!(
        widened["available_sandboxes"],
        json!(["python:3.12", "alpine"])
    );
    assert_eq!(widened["fallback_providers"], json!(["openai-default"]));
    assert_eq!(widened["mcp_servers"], json!(["fs"]));
    assert_eq!(
        widened["allowed_tools"],
        json!(["existing_tool", "get_current_time"])
    );
}

/// B2: a harness's `requires.sandboxes` makes a profile without them
/// incompatible, and the widening adds them next to the existing images.
#[test]
fn required_sandboxes_gate_compatibility_and_are_unioned_in() {
    let ws = Ws::new("b2-sandboxes");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    let mut p = profile(
        "pro_1",
        "default",
        "anthropic-default",
        &["get_current_time"],
    );
    p["available_sandboxes"] = json!(["python:3.12"]);
    ws.add_profile(p);
    ws.harness(&PROBE_HARNESS.replace(
        "allowed_tools = [\"get_current_time\"]",
        "allowed_tools = [\"get_current_time\"]\nsandboxes = [\"alpine\"]",
    ));
    ws.build("probe-local", "local");

    let out = ws.run(ws.ready("probe-local", &[]).env("ANTHROPIC_API_KEY", "sk"));
    assert_eq!(out.code, 3, "{out:?}");
    assert!(out.stdout.contains(
        "--allowed-tool get_current_time --available-sandbox python:3.12 --available-sandbox alpine"
    ), "{}", out.stdout);
}

// ---------------------------------------------------------------------------
// B4 — relative --dir is canonicalized for build/ready/run
// ---------------------------------------------------------------------------

#[test]
fn relative_dir_is_canonicalized_for_build_ready_and_run() {
    let ws = Ws::new("b4-relative");
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    fs::write(ws.dir().join(".env"), "ANTHROPIC_API_KEY=sk-from-dotenv\n").unwrap();
    ws.add_profile(profile("pro_1", "default", "anthropic-default", &[]));
    ws.harness(PROBE_CONTAINER_HARNESS);
    let abs = ws.dir();

    // cwd = the workspace root; every path argument is relative to it.
    let out = ws.run(ws.cmd(&[
        "build",
        "probe",
        "--harness-dir",
        "harness",
        "--launcher",
        "api",
        "--id",
        "probe-api",
        "--dir",
        "work",
    ]));
    assert_eq!(out.code, 0, "{out:?}");
    assert!(abs.join(".baectl/builds/probe-api/manifest.json").is_file());

    let out = ws.run(ws.cmd(&["ready", "probe-api", "--dir", "work"]));
    assert_eq!(out.code, 3, "{out:?}");
    // The hint names the canonical (absolute) compose file…
    assert!(
        out.stdout.contains(&format!(
            "docker compose -f {}/docker-compose.yml",
            abs.display()
        )),
        "{}",
        out.stdout
    );
    // …and every in-container exec ran from the canonical dir.
    let execs: Vec<String> = ws
        .calls()
        .into_iter()
        .filter(|c| c.contains(" baectl "))
        .collect();
    assert!(!execs.is_empty());
    for c in &execs {
        assert!(c.starts_with(&format!("cwd={} ", abs.display())), "{c}");
    }

    // `run` hands the engine `--env-file` paths; relative ones would resolve
    // against the engine's cwd (the dir itself) and not exist.
    let out = ws.run(ws.cmd(&["run", "probe-api", "--dir", "work"]));
    assert_eq!(out.code, 0, "{out:?}");
    let launch = ws
        .calls()
        .into_iter()
        .find(|c| c.contains("argv=run -d "))
        .expect("container launched");
    assert!(
        launch.contains(&format!(
            "--env-file {}",
            abs.join(".baectl/builds/probe-api/harness.env").display()
        )),
        "{launch}"
    );
    assert!(launch.contains(&format!("--env-file {}", abs.join(".env").display())));
}

#[test]
fn missing_dir_is_a_runtime_error_for_build_ready_and_run() {
    let ws = Ws::new("b4-missing");
    for args in [
        vec!["build", "probe", "--dir", "no-such-dir"],
        vec!["ready", "probe-local", "--dir", "no-such-dir"],
        vec!["run", "probe-local", "--dir", "no-such-dir"],
    ] {
        let out = ws.run(&mut ws.cmd(&args));
        assert_eq!(out.code, 1, "{args:?}: {out:?}");
        assert_eq!(
            out.stderr, "baectl: --dir no-such-dir does not exist or is not a directory\n",
            "{args:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// B5 — BAE_PROVIDER_KEY_ENV for a non-Anthropic provider
// ---------------------------------------------------------------------------

#[test]
fn run_local_exports_provider_key_env_for_an_openai_provider() {
    let ws = Ws::new("b5-local");
    ws.server("openai-default", "openai", "OPENAI_API_KEY");
    ws.add_profile(profile(
        "pro_1",
        "default",
        "openai-default",
        &["get_current_time"],
    ));
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");
    let capture = ws.root.join("capture.env");

    let out = ws.run(
        ws.run_cmd("probe-local")
            .env("OPENAI_API_KEY", "sk-openai-test")
            .env("BAECTL_TEST_CAPTURE", &capture),
    );

    assert_eq!(out.code, 0, "{out:?}");
    let env = fs::read_to_string(&capture).expect("harness ran");
    let lines: Vec<&str> = env.lines().collect();
    assert!(
        lines.contains(&"BAE_PROVIDER_KEY_ENV=OPENAI_API_KEY"),
        "{env}"
    );
    assert!(lines.contains(&"BAE_CLIENT_KEY=bae_fake_secret_1"));
    assert!(lines.contains(&"BAE_SERVER_URL=http://localhost:8080"));
}

#[test]
fn run_container_writes_provider_key_env_into_harness_env() {
    let ws = Ws::new("b5-container");
    ws.server("openai-default", "openai", "OPENAI_API_KEY");
    ws.add_profile(profile(
        "pro_1",
        "default",
        "openai-default",
        &["get_current_time"],
    ));
    ws.harness(PROBE_CONTAINER_HARNESS);
    ws.build("probe-api", "api");

    let out = ws.run(
        ws.run_cmd("probe-api")
            .env("OPENAI_API_KEY", "sk-openai-host-only"),
    );

    assert_eq!(out.code, 0, "{out:?}");
    let body = fs::read_to_string(ws.artifact("probe-api").join("harness.env")).unwrap();
    assert_eq!(
        body,
        "OPENAI_API_KEY=sk-openai-host-only\n\
         BAE_PROVIDER_KEY_ENV=OPENAI_API_KEY\n\
         BAE_SERVER_URL=http://host.docker.internal:8080\n\
         BAE_CLIENT_KEY=bae_fake_secret_1\n"
    );
    let launch = ws
        .calls()
        .into_iter()
        .find(|c| c.contains("argv=run -d "))
        .unwrap();
    assert!(!launch.contains("sk-openai-host-only") && !launch.contains("bae_fake_secret"));
}

#[test]
fn apple_run_uses_server_ip_and_listener_port_and_refreshes_saved_addresses() {
    let ws = Ws::new("apple-network");
    ws.server("openai-default", "openai", "OPENAI_API_KEY");
    fs::remove_file(ws.dir().join("docker-compose.yml")).unwrap();
    write_exe(
        &ws.dir().join("bae-setup.sh"),
        "#!/bin/sh\n# --publish 3000:3000\n",
    );
    fs::write(
        ws.dir().join(".env"),
        "BAE_ADDR=0.0.0.0:8181\nBAE_ADDR_PORT=18080\nOPENAI_API_KEY=sk-test\n",
    )
    .unwrap();
    ws.add_profile(profile(
        "pro_1",
        "default",
        "openai-default",
        &["get_current_time"],
    ));
    ws.harness(PROBE_CONTAINER_HARNESS);
    ws.build("probe-api", "api");

    let out = ws.run(ws.run_cmd("probe-api"));
    assert_eq!(out.code, 0, "{out:?}");
    let env = ws.artifact("probe-api").join("harness.env");
    assert!(fs::read_to_string(&env)
        .unwrap()
        .contains("BAE_SERVER_URL=http://192.168.64.3:8181\n"));
    assert!(ws
        .calls()
        .iter()
        .any(|c| c.ends_with("argv=inspect bae-max")));
    let launch = ws
        .calls()
        .into_iter()
        .find(|c| c.contains("argv=run -d "))
        .unwrap();
    assert!(!launch.contains("--add-host"));

    // Upgrades must also repair old resolved.json records under --no-ready.
    let path = ws.artifact("probe-api").join("resolved.json");
    let mut saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    saved["server_url"] = json!("http://host.docker.internal:18080");
    fs::write(path, saved.to_string()).unwrap();
    fs::write(
        ws.state().join("inspect.json"),
        "[{\"networks\":[{\"network\":\"default\",\"address\":\"192.168.64.9/24\"}]}]\n",
    )
    .unwrap();
    let out = ws.run(ws.run_cmd("probe-api").arg("--no-ready"));
    assert_eq!(out.code, 0, "{out:?}");
    assert!(fs::read_to_string(&env)
        .unwrap()
        .contains("BAE_SERVER_URL=http://192.168.64.9:8181\n"));

    // An explicit URL works even if inspect fails; automatic discovery must
    // fail before stopping an existing harness container.
    fs::write(ws.state().join("fail_inspect"), "").unwrap();
    fs::write(ws.state().join("calls.log"), "").unwrap();
    let out = ws.run(ws.run_cmd("probe-api").arg("--no-ready"));
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("--server-url"), "{out:?}");
    assert!(!ws.calls().iter().any(|c| c.contains("argv=stop ")));
    for extra in [vec![], vec!["--no-ready"]] {
        let out = ws.run(
            ws.run_cmd("probe-api")
                .args(extra)
                .args(["--server-url", "http://192.168.65.2:8080"]),
        );
        assert_eq!(out.code, 0, "{out:?}");
        assert!(fs::read_to_string(&env)
            .unwrap()
            .contains("BAE_SERVER_URL=http://192.168.65.2:8080\n"));
    }
}

// ---------------------------------------------------------------------------
// B6 — generated build context excludes host build output
// ---------------------------------------------------------------------------

#[test]
fn container_build_writes_a_dockerignore_into_the_generated_build_context() {
    let ws = Ws::new("b6-write");
    ws.harness(PROBE_CONTAINER_HARNESS);

    let out = ws.build("probe-api", "api");

    let path = ws.harness_dir().join(".dockerignore");
    assert_eq!(fs::read_to_string(&path).unwrap(), DOCKERIGNORE);
    for entry in [
        "target/",
        "node_modules/",
        ".venv/",
        "__pycache__/",
        ".baectl/",
        ".git/",
    ] {
        assert!(DOCKERIGNORE.lines().any(|l| l == entry));
    }
    assert!(out.stdout.contains(&format!(
        "wrote {} (build-context excludes)",
        path.display()
    )));
    // The engine built from exactly that context.
    let build = ws
        .calls()
        .into_iter()
        .find(|c| c.contains("probe-api-harness-build:latest"))
        .unwrap();
    assert!(
        build.ends_with(&format!(" {}", ws.harness_dir().display())),
        "{build}"
    );
}

#[test]
fn an_existing_dockerignore_is_never_overwritten_and_a_missing_target_warns() {
    let ws = Ws::new("b6-existing");
    ws.harness(PROBE_CONTAINER_HARNESS);
    let path = ws.harness_dir().join(".dockerignore");
    fs::write(&path, "secrets/\n").unwrap();

    let out = ws.build("probe-api", "api");

    assert_eq!(fs::read_to_string(&path).unwrap(), "secrets/\n");
    assert!(!out.stdout.contains("wrote"));
    assert!(out.stderr.contains(&format!(
        "baectl: warning: {} does not exclude target/",
        path.display()
    )));

    // One that does exclude target/ is accepted silently.
    fs::write(&path, "/target\n").unwrap();
    let out = ws.build("probe-api", "api");
    assert!(!out.stderr.contains("does not exclude"), "{out:?}");
}

#[test]
fn local_build_writes_no_dockerignore() {
    let ws = Ws::new("b6-local");
    ws.harness(PROBE_HARNESS);
    ws.build("probe-local", "local");
    assert!(!ws.harness_dir().join(".dockerignore").exists());
}

/// The bundled examples' build contexts (`client-<sdk>/`) carry the same
/// excludes as a committed second layer.
#[test]
fn bundled_sdk_build_contexts_commit_the_same_dockerignore() {
    for sdk in ["rust", "typescript", "python"] {
        let path = repo_root().join(format!("client-{sdk}/.dockerignore"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()));
        for entry in DOCKERIGNORE.lines() {
            assert!(
                text.lines().any(|l| l.trim() == entry),
                "{} lacks {entry}",
                path.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// B9 — strict manifest, name/id validation (exit 2)
// ---------------------------------------------------------------------------

fn build_probe(ws: &Ws, extra: &[&str]) -> Out {
    ws.run(
        ws.cmd(&["build", "probe"])
            .args(extra)
            .arg("--harness-dir")
            .arg(ws.harness_dir())
            .arg("--dir")
            .arg(ws.dir()),
    )
}

#[test]
fn unknown_manifest_key_is_a_usage_error_naming_the_key() {
    let ws = Ws::new("b9-unknown");
    ws.harness(
        "[harness]\nname = \"probe\"\nsdk = \"rust\"\nrun = \"true\"\n\n\
         [harness.requires]\nallowed_tool = [\"x\"]\n",
    );

    let out = build_probe(&ws, &[]);

    assert_eq!(out.code, 2, "{out:?}");
    assert!(
        out.stderr.starts_with(
            "baectl: invalid bae-harness.toml: unknown field `allowed_tool`, expected one of"
        ),
        "{out:?}"
    );
    assert!(
        out.stderr.contains("(line 7: `allowed_tool = [\"x\"]`)"),
        "{out:?}"
    );
    assert!(!ws.dir().join(".baectl").exists());
}

#[test]
fn invalid_harness_name_is_a_usage_error_that_does_not_mention_id() {
    let ws = Ws::new("b9-name");
    ws.harness("[harness]\nname = \"My Agent\"\nsdk = \"rust\"\nrun = \"true\"\n");

    let out = build_probe(&ws, &[]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "baectl: invalid harness name \"My Agent\": must match [a-z0-9][a-z0-9_.-]* \
         (Docker tag/container-name rules)\n"
    );
    assert!(!ws.dir().join(".baectl").exists());
}

#[test]
fn invalid_explicit_id_is_a_usage_error_naming_the_flag() {
    let ws = Ws::new("b9-id");
    ws.harness(PROBE_HARNESS);

    let out = build_probe(&ws, &["--id", "Bad/Id"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "baectl: invalid --id \"Bad/Id\": must match [a-z0-9][a-z0-9_.-]* \
         (Docker tag/container-name rules)\n"
    );
    assert!(!ws.artifact("Bad/Id").exists());
}

// ---------------------------------------------------------------------------
// B10 — [harness.container] overrides
// ---------------------------------------------------------------------------

#[test]
fn container_overrides_replace_the_generated_build_and_binary_path() {
    let ws = Ws::new("b10-overrides");
    ws.harness(&format!(
        "{PROBE_CONTAINER_HARNESS}\n[harness.container]\nbuild = \"make release\"\n\
         entrypoint = \"/build/out/probe\"\n"
    ));

    ws.build("probe-api", "api");

    let generated =
        fs::read_to_string(ws.artifact("probe-api").join("Dockerfile.build.generated")).unwrap();
    assert!(
        generated.lines().any(|l| l == "RUN make release"),
        "{generated}"
    );
    assert!(!generated.contains("cargo build"), "{generated}");
    let launcher = fs::read_to_string(ws.artifact("probe-api").join("Dockerfile")).unwrap();
    assert!(
        launcher.contains(
            "COPY --from=probe-api-harness-build:latest /build/out/probe /usr/local/bin/probe"
        ),
        "{launcher}"
    );
}

#[test]
fn container_overrides_are_ignored_with_a_warning_when_a_dockerfile_is_supplied() {
    let ws = Ws::new("b10-dockerfile");
    fs::write(ws.harness_dir().join("Dockerfile.custom"), "FROM scratch\n").unwrap();
    ws.harness(&format!(
        "{}dockerfile = \"Dockerfile.custom\"\nbinary_path = \"/app/probe\"\n\n\
         [harness.container]\nbuild = \"make release\"\n",
        PROBE_CONTAINER_HARNESS
    ));

    let out = ws.build("probe-api", "api");

    assert!(out.stderr.contains(
        "baectl: warning: [harness.container] is ignored because [harness.launcher].dockerfile is set"
    ), "{out:?}");
    assert!(!ws
        .artifact("probe-api")
        .join("Dockerfile.build.generated")
        .exists());
    // The supplied Dockerfile's context is left alone (no .dockerignore added).
    assert!(!ws.harness_dir().join(".dockerignore").exists());
}

#[test]
fn a_multi_line_container_override_is_a_usage_error() {
    let ws = Ws::new("b10-multiline");
    ws.harness(&format!(
        "{PROBE_CONTAINER_HARNESS}\n[harness.container]\nbuild = \"a\\nb\"\n"
    ));

    let out = build_probe(&ws, &["--launcher", "api"]);

    assert_eq!(out.code, 2, "{out:?}");
    assert_eq!(
        out.stderr,
        "baectl: [harness.container].build must be a non-empty single-line command\n"
    );
}

// ---------------------------------------------------------------------------
// B11 — build needs no `date` on PATH
// ---------------------------------------------------------------------------

/// Regression (B11): the build timestamp came from a `date` subprocess, so a
/// minimal PATH broke `build`. A local build spawns nothing at all now.
#[test]
fn local_build_succeeds_with_an_empty_path_and_stamps_rfc3339() {
    let ws = Ws::new("b11-path");
    ws.harness(PROBE_HARNESS);

    let out = ws.run(
        ws.cmd(&["build", "probe", "--harness-dir"])
            .arg(ws.harness_dir())
            .arg("--dir")
            .arg(ws.dir())
            .env("PATH", ws.root.join("empty-bin")),
    );

    assert_eq!(out.code, 0, "{out:?}");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(ws.artifact("probe-rust-local").join("manifest.json")).unwrap(),
    )
    .unwrap();
    let ts = manifest["created_at"].as_str().unwrap();
    let b = ts.as_bytes();
    assert_eq!(b.len(), 20, "{ts}");
    for (i, c) in b.iter().enumerate() {
        match i {
            4 | 7 => assert_eq!(*c, b'-', "{ts}"),
            10 => assert_eq!(*c, b'T', "{ts}"),
            13 | 16 => assert_eq!(*c, b':', "{ts}"),
            19 => assert_eq!(*c, b'Z', "{ts}"),
            _ => assert!(c.is_ascii_digit(), "{ts}"),
        }
    }
    assert!(ts >= "2026-01-01T00:00:00Z", "{ts}");
}

// ---------------------------------------------------------------------------
// C1 — setup --yes
// ---------------------------------------------------------------------------

const NO_KEY_STDERR: &str = "baectl: no provider API key found in the environment.
        export ANTHROPIC_API_KEY=\"sk-ant-…\"   # or OPENAI_API_KEY=\"sk-…\"
        then re-run `baectl setup --yes`
";

fn setup_yes(ws: &Ws, flags: &[&str], env: &[(&str, &str)], path: &Path) -> Out {
    let mut c = ws.cmd(&["setup"]);
    c.args(flags).arg("--dir").arg(ws.dir()).env("PATH", path);
    for (k, v) in env {
        c.env(k, v);
    }
    ws.run(&mut c)
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn setup_yes_without_a_provider_key_fails_before_writing_anything() {
    let ws = Ws::new("c1-nokey");
    let out = setup_yes(&ws, &["--yes"], &[], &ws.root.join("empty-bin"));
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.stderr, NO_KEY_STDERR);
    assert!(
        dir_entries(&ws.dir()).is_empty(),
        "{:?}",
        dir_entries(&ws.dir())
    );

    // An empty value is not a key.
    let out = setup_yes(
        &ws,
        &["-y"],
        &[("ANTHROPIC_API_KEY", ""), ("OPENAI_API_KEY", "")],
        &ws.root.join("empty-bin"),
    );
    assert_eq!(out.code, 1, "{out:?}");
    assert_eq!(out.stderr, NO_KEY_STDERR);
}

#[test]
fn setup_yes_detects_anthropic_first_and_never_echoes_the_key() {
    let ws = Ws::new("c1-anthropic");
    let out = setup_yes(
        &ws,
        &["--yes"],
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-SECRET-VALUE-1"),
            ("OPENAI_API_KEY", "sk-SECRET-VALUE-2"),
        ],
        &ws.root.join("empty-bin"),
    );

    // No engine on PATH: files are written, then the launch step fails.
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out.stderr.contains("docker not found on PATH"), "{out:?}");
    assert!(out
        .stdout
        .lines()
        .any(|l| l == "using provider anthropic (ANTHROPIC_API_KEY is set)"));
    assert!(!out.stdout.contains("openai"), "{}", out.stdout);
    for secret in ["SECRET-VALUE-1", "SECRET-VALUE-2"] {
        assert!(!out.stdout.contains(secret) && !out.stderr.contains(secret));
    }
    assert_eq!(
        dir_entries(&ws.dir()),
        vec![".env", "bae-config.toml", "docker-compose.yml"]
    );
    let config = fs::read_to_string(ws.dir().join("bae-config.toml")).unwrap();
    assert_eq!(config.matches("[[providers.entries]]").count(), 1);
    assert!(config.contains("name = \"anthropic-default\""));
    assert!(config.contains("auth_token = \"${ANTHROPIC_API_KEY}\""));
    let compose = fs::read_to_string(ws.dir().join("docker-compose.yml")).unwrap();
    assert!(compose.contains(
        "    extra_hosts:\n      - \"host.docker.internal:host-gateway\"\n    restart: unless-stopped\n"
    ));
}

#[test]
fn setup_yes_falls_back_to_openai() {
    let ws = Ws::new("c1-openai");
    let out = setup_yes(
        &ws,
        &["-y"],
        &[("OPENAI_API_KEY", "sk-SECRET-VALUE-3")],
        &ws.root.join("empty-bin"),
    );
    assert_eq!(out.code, 1, "{out:?}");
    assert!(out
        .stdout
        .lines()
        .any(|l| l == "using provider openai (OPENAI_API_KEY is set)"));
    assert!(!out.stdout.contains("SECRET-VALUE-3") && !out.stderr.contains("SECRET-VALUE-3"));
    let config = fs::read_to_string(ws.dir().join("bae-config.toml")).unwrap();
    assert!(config.contains("name = \"openai-default\""));
    assert!(config.contains("auth_token = \"${OPENAI_API_KEY}\""));
}

#[test]
fn setup_yes_apple_writes_the_apple_script() {
    let ws = Ws::new("c1-apple");
    let out = setup_yes(
        &ws,
        &["--yes", "--apple"],
        &[("ANTHROPIC_API_KEY", "sk-ant-x")],
        &ws.root.join("empty-bin"),
    );
    assert_eq!(out.code, 1, "{out:?}");
    assert!(
        out.stderr.contains("container not found on PATH"),
        "{out:?}"
    );
    assert_eq!(
        dir_entries(&ws.dir()),
        vec![".env", "bae-config.toml", "bae-setup.sh"]
    );
}

#[test]
fn setup_yes_on_partial_state_aborts_with_exit_1_and_changes_nothing() {
    let ws = Ws::new("c1-partial");
    fs::write(ws.dir().join(".env"), "KEEP=me\n").unwrap();

    let out = setup_yes(
        &ws,
        &["--yes"],
        &[("ANTHROPIC_API_KEY", "sk-ant-x")],
        &ws.root.join("empty-bin"),
    );

    assert_eq!(out.code, 1, "{out:?}");
    assert!(
        out.stderr
            .ends_with("baectl: aborted; no files were changed.\n"),
        "{out:?}"
    );
    // The abort says how to recover, naming only the files actually present.
    assert!(
        out.stderr.contains(&format!(
            "To start over, remove .env from {} and re-run, or run `baectl setup` without --yes",
            ws.dir().display()
        )),
        "{out:?}"
    );
    assert_eq!(dir_entries(&ws.dir()), vec![".env"]);
    assert_eq!(
        fs::read_to_string(ws.dir().join(".env")).unwrap(),
        "KEEP=me\n"
    );
}

/// Answer every HTTP request with `200 ok` (the `/healthz` poll).
fn healthz_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    port
}

#[test]
fn setup_yes_rerun_relaunches_the_saved_config_without_changing_files() {
    let ws = Ws::new("c1-rerun");
    let out = setup_yes(
        &ws,
        &["--yes"],
        &[("ANTHROPIC_API_KEY", "sk-ant-x")],
        &ws.root.join("empty-bin"),
    );
    assert_eq!(out.code, 1, "{out:?}");
    // Point the saved config's health check at a local stand-in server.
    let port = healthz_server();
    append_line(
        &ws.dir().join(".env"),
        &format!("BAE_ADDR=127.0.0.1:{port}"),
    );
    let snapshot: Vec<(String, Vec<u8>)> = dir_entries(&ws.dir())
        .into_iter()
        .map(|n| {
            let bytes = fs::read(ws.dir().join(&n)).unwrap();
            (n, bytes)
        })
        .collect();

    let fake_path = ws.fake_path();
    let out = setup_yes(
        &ws,
        &["--yes"],
        &[("ANTHROPIC_API_KEY", "sk-ant-x")],
        Path::new(&fake_path),
    );

    assert_eq!(out.code, 0, "{out:?}");
    assert!(out.stdout.lines().any(|l| l
        == format!(
            "already set up in {} — server launched from the saved configuration",
            ws.dir().display()
        )));
    assert!(
        !out.stdout.contains("using provider"),
        "no provider re-detection"
    );
    let calls = ws.calls();
    assert!(
        calls.iter().any(|c| c.ends_with("argv=compose up -d")),
        "{calls:?}"
    );
    assert!(
        ws.mutations().is_empty(),
        "rerun must not create a profile/key"
    );
    let after: Vec<(String, Vec<u8>)> = dir_entries(&ws.dir())
        .into_iter()
        .map(|n| {
            let bytes = fs::read(ws.dir().join(&n)).unwrap();
            (n, bytes)
        })
        .collect();
    assert_eq!(after, snapshot, "rerun must not rewrite any file");
}

// ---------------------------------------------------------------------------
// C2 — prepare
// ---------------------------------------------------------------------------

fn prepare_ws(label: &str, prepare: &str) -> Ws {
    let ws = Ws::new(label);
    ws.server("anthropic-default", "anthropic", "ANTHROPIC_API_KEY");
    ws.add_profile(profile(
        "pro_1",
        "default",
        "anthropic-default",
        &["get_current_time"],
    ));
    ws.harness(&PROBE_CONTAINER_HARNESS.replace(
        "run = \"env > \\\"$BAECTL_TEST_CAPTURE\\\"\"",
        &format!("run = \"echo harness >> \\\"$BAECTL_TEST_LOG\\\"\"\nprepare = \"{prepare}\""),
    ));
    ws
}

fn run_logged(ws: &Ws, id: &str) -> Out {
    ws.run(
        ws.run_cmd(id)
            .env("ANTHROPIC_API_KEY", "sk")
            .env("BAECTL_TEST_LOG", ws.root.join("run.log")),
    )
}

fn run_log(ws: &Ws) -> Vec<String> {
    fs::read_to_string(ws.root.join("run.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn prepare_runs_before_the_harness_only_when_needed() {
    let ws = prepare_ws("c2-marker", "echo prepare >> \\\"$BAECTL_TEST_LOG\\\"");
    ws.build("probe-local", "local");
    let manifest = fs::read_to_string(ws.artifact("probe-local").join("manifest.json")).unwrap();
    assert!(manifest.contains("\"prepare\": \"echo prepare >> \\\"$BAECTL_TEST_LOG\\\"\""));

    let out = run_logged(&ws, "probe-local");
    assert_eq!(out.code, 0, "{out:?}");
    assert!(
        out.stdout.contains(&format!(
            "prepare: echo prepare >> \"$BAECTL_TEST_LOG\"   (in {})",
            ws.harness_dir().join(".").display()
        )),
        "{}",
        out.stdout
    );
    assert_eq!(run_log(&ws), vec!["prepare", "harness"]);
    let marker = ws.artifact("probe-local").join("prepared");
    assert!(marker.is_file());

    // Nothing changed → not needed again.
    let out = run_logged(&ws, "probe-local");
    assert_eq!(out.code, 0, "{out:?}");
    assert!(!out.stdout.contains("prepare:"));
    assert_eq!(run_log(&ws), vec!["prepare", "harness", "harness"]);

    // bae-harness.toml edited after the marker → needed again.
    set_mtime(
        &ws.harness_dir().join("bae-harness.toml"),
        SystemTime::now() + Duration::from_secs(60),
    );
    let out = run_logged(&ws, "probe-local");
    assert_eq!(out.code, 0, "{out:?}");
    assert_eq!(
        run_log(&ws),
        vec!["prepare", "harness", "harness", "prepare", "harness"]
    );
}

#[test]
fn npm_prepare_follows_node_modules_and_the_lockfile() {
    let ws = prepare_ws("c2-npm", "npm install");
    write_exe(
        &ws.bin().join("npm"),
        "#!/bin/sh\necho \"npm $*\" >> \"$BAECTL_TEST_LOG\"\nmkdir -p node_modules\n",
    );
    fs::write(ws.harness_dir().join("package-lock.json"), "{}").unwrap();
    ws.build("probe-local", "local");

    // No node_modules/ → install.
    assert_eq!(run_logged(&ws, "probe-local").code, 0);
    assert_eq!(run_log(&ws), vec!["npm install", "harness"]);
    // Installed and the lockfile is older → skip.
    assert_eq!(run_logged(&ws, "probe-local").code, 0);
    assert_eq!(run_log(&ws), vec!["npm install", "harness", "harness"]);
    // Lockfile newer than node_modules/ → install again.
    set_mtime(
        &ws.harness_dir().join("package-lock.json"),
        SystemTime::now() + Duration::from_secs(60),
    );
    assert_eq!(run_logged(&ws, "probe-local").code, 0);
    assert_eq!(
        run_log(&ws),
        vec![
            "npm install",
            "harness",
            "harness",
            "npm install",
            "harness"
        ]
    );
}

#[test]
fn a_failing_prepare_aborts_run_with_its_exit_code() {
    let ws = prepare_ws("c2-fail", "exit 7");
    ws.build("probe-local", "local");

    let out = run_logged(&ws, "probe-local");

    assert_eq!(out.code, 7, "{out:?}");
    assert!(
        out.stderr
            .ends_with("baectl: prepare command failed (exit 7): exit 7\n"),
        "{out:?}"
    );
    assert!(run_log(&ws).is_empty(), "the harness must not start");
    assert!(!ws.artifact("probe-local").join("prepared").exists());
}

#[test]
fn prepare_never_runs_for_a_container_launcher() {
    let ws = prepare_ws("c2-container", "echo prepare >> \\\"$BAECTL_TEST_LOG\\\"");
    ws.build("probe-api", "api");
    let manifest = fs::read_to_string(ws.artifact("probe-api").join("manifest.json")).unwrap();
    assert!(!manifest.contains("prepare"), "{manifest}");

    let out = run_logged(&ws, "probe-api");

    assert_eq!(out.code, 0, "{out:?}");
    assert!(!out.stdout.contains("prepare:"));
    assert!(run_log(&ws).is_empty());
    assert!(!ws.artifact("probe-api").join("prepared").exists());
}
