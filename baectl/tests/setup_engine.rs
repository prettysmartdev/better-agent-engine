//! Opt-in container-engine coverage for `baectl setup`.
//!
//! These tests remain offline by default: setting `BAECTL_SETUP_ENGINE_TESTS=1`
//! is an explicit acknowledgement that a local `make image` has produced the
//! `better-agent-engine:latest` fixture image and that the selected engine may
//! create containers and a named `bae-data` volume.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "baectl-setup-engine-{label}-{}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn enabled(engine: &str) -> bool {
    std::env::var_os("BAECTL_SETUP_ENGINE_TESTS").as_deref() == Some(std::ffi::OsStr::new("1"))
        && Command::new(engine)
            .arg(if engine == "docker" {
                "info"
            } else {
                "version"
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `script` gives the child a pseudoterminal, allowing this test to exercise
/// the real interactive wizard instead of its safe non-interactive defaults.
fn interactive_setup(dir: &Path, apple: bool, answers: &str) {
    let binary = env!("CARGO_BIN_EXE_baectl");
    let mut command = format!(
        "cd {} && {} setup --dev --dir {}",
        shell_quote(&dir.display().to_string()),
        shell_quote(binary),
        shell_quote(&dir.display().to_string())
    );
    if apple {
        command.push_str(" --apple");
    }
    let mut child = Command::new("script")
        .args(["-qefc", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("script is available on Unix CI/dev hosts");
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "interactive setup failed");
}

fn docker_compose(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(["compose"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "docker compose {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn fresh_launch_then_relaunch_keeps_the_single_profile_and_key() {
    if !enabled("docker") {
        return;
    }
    if !Command::new("docker")
        .args(["image", "inspect", "better-agent-engine:latest"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let dir = TempDir::new("docker");
    // apple=no (compose); standard; anthropic defaults (kind, registry name,
    // model, ANTHROPIC_API_KEY var name); fixture token as the secret value; no
    // extra provider/MCP; default BAE_* values; then Launch=yes. (`--dev` is
    // passed, so the image-source question is skipped; `--apple` is not, so it
    // is asked and answered "n".)
    interactive_setup(
        dir.path(),
        false,
        "n\n\n\n\n\n\nfixture-token\nn\nn\n\n\n\n\ny\n",
    );
    let profiles = docker_compose(
        dir.path(),
        &[
            "exec", "-T", "baesrv", "baectl", "list", "profiles", "--json",
        ],
    );
    let keys = docker_compose(
        dir.path(),
        &["exec", "-T", "baesrv", "baectl", "list", "keys", "--json"],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&profiles)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&keys)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    docker_compose(dir.path(), &["down"]);
    // Existing setup: apple=no (compose), Edit=no, Launch=yes. This must not
    // make another key.
    interactive_setup(dir.path(), false, "n\nn\ny\n");
    let profiles = docker_compose(
        dir.path(),
        &[
            "exec", "-T", "baesrv", "baectl", "list", "profiles", "--json",
        ],
    );
    let keys = docker_compose(
        dir.path(),
        &["exec", "-T", "baesrv", "baectl", "list", "keys", "--json"],
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&profiles)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&keys)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    docker_compose(dir.path(), &["down", "-v"]);
}

#[test]
fn apple_script_is_directly_runnable_when_apple_container_is_available() {
    if !enabled("container") {
        return;
    }
    if !Command::new("container")
        .args(["image", "inspect", "better-agent-engine:latest"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let dir = TempDir::new("apple");
    // Generate only; the test invokes the generated launcher itself. Both mode
    // questions are skipped (`--apple` and `--dev` are passed): standard,
    // anthropic defaults, fixture token as the secret value, default BAE_*,
    // Launch=no.
    interactive_setup(
        dir.path(),
        true,
        "\n\n\n\n\nfixture-token\nn\nn\n\n\n\n\nn\n",
    );
    assert!(Command::new(dir.path().join("bae-setup.sh"))
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let health = Command::new("container")
        .args([
            "exec",
            "bae",
            "sh",
            "-c",
            "wget -qO- http://127.0.0.1:8080/healthz",
        ])
        .output()
        .unwrap();
    assert!(
        health.status.success(),
        "generated script did not start a healthy server"
    );
    let _ = Command::new("container").args(["stop", "bae"]).status();
    let _ = Command::new("container").args(["rm", "bae"]).status();
    let _ = Command::new("container")
        .args(["volume", "rm", "bae-data"])
        .status();
}
