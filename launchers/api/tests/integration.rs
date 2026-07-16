//! Process-level, loopback-only API launcher coverage.
//!
//! The test starts the real `baeapi` binary with two configured agents and uses
//! curl only against that local listener. It never contacts a provider or an
//! external network.

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

fn temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "baeapi-integration-{label}-{}.tmp",
        std::process::id()
    ))
}

fn curl(port: u16, path: &str, body: Option<&str>, token: Option<&str>) -> String {
    let url = format!("http://127.0.0.1:{port}{path}");
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--silent",
        "--show-error",
        "-H",
        "content-type: application/json",
    ]);
    if let Some(token) = token {
        command.args(["-H", &format!("authorization: Bearer {token}")]);
    }
    if let Some(body) = body {
        command.args(["--data", body]);
    }
    command.args(["-w", "\n%{http_code}", &url]);
    let output = command.output().expect("run local curl");
    assert!(output.status.success(), "curl failed: {:?}", output);
    String::from_utf8(output.stdout).expect("curl response is utf8")
}

fn wait_until_ready(port: u16) {
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("baeapi did not bind port {port}");
}

#[test]
fn booted_multi_agent_api_validates_streams_and_scopes_auth() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("reserve local port")
        .local_addr()
        .unwrap()
        .port();
    let config = temp_path("config");
    fs::write(
        &config,
        format!(
            r#"
[server]
addr = "127.0.0.1:{port}"

[[agents]]
name = "alpha"
command = "/bin/sh"
args = ["-c", "printf 'alpha\\n'; sleep 1; printf 'alpha-done\\n'"]
[agents.request_schema]
type = "object"
required = ["prompt"]
[agents.request_schema.properties.prompt]
type = "string"

[[agents]]
name = "beta"
command = "/bin/sh"
args = ["-c", "printf 'beta\\n'"]
[agents.request_schema]
type = "object"
required = ["text"]
[agents.request_schema.properties.text]
type = "string"
"#
        ),
    )
    .expect("write API fixture");

    let mut server = Command::new(env!("CARGO_BIN_EXE_baeapi"))
        .env("BAE_LAUNCHER_API_CONFIG", &config)
        .env("BAE_LAUNCHER_API_TOKEN", "integration-token")
        .env("BAE_LOG", "error")
        .spawn()
        .expect("start baeapi");
    wait_until_ready(port);

    let health = curl(port, "/healthz", None, None);
    assert!(health.ends_with("\n200"), "health response: {health}");

    let listed = curl(port, "/_launcher/agents", None, None);
    assert!(listed.contains("alpha") && listed.contains("beta"));
    assert!(!listed.contains("command"));
    assert!(!listed.contains("integration-token"));

    let invalid_alpha = curl(
        port,
        "/agents/alpha/trigger",
        Some(r#"{"text":"wrong"}"#),
        Some("integration-token"),
    );
    assert!(invalid_alpha.ends_with("\n400"));
    assert!(invalid_alpha.contains("prompt"));

    let missing_token = curl(port, "/agents/beta/trigger", Some(r#"{"text":"x"}"#), None);
    assert!(missing_token.ends_with("\n401"));

    // Different agents can be triggered concurrently on the same listener.
    let alpha = thread::spawn(move || {
        curl(
            port,
            "/agents/alpha/trigger",
            Some(r#"{"prompt":"hello"}"#),
            Some("integration-token"),
        )
    });
    let beta = thread::spawn(move || {
        curl(
            port,
            "/agents/beta/trigger",
            Some(r#"{"text":"world"}"#),
            Some("integration-token"),
        )
    });
    let alpha = alpha.join().expect("alpha request");
    let beta = beta.join().expect("beta request");
    assert!(alpha.contains("[alpha] alpha") && alpha.contains("alpha-done"));
    assert!(beta.contains("[beta] beta"));
    assert!(alpha.ends_with("\n200") && beta.ends_with("\n200"));

    let pid = server.id().to_string();
    let signal = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {pid}")])
        .status()
        .expect("stop baeapi");
    assert!(signal.success());
    let status = server.wait().expect("wait for baeapi");
    assert!(status.success(), "baeapi exited with {status}");
    fs::remove_file(config).ok();
}

/// A hung child agent must never block `baeapi`'s own shutdown: SIGTERM starts
/// a graceful drain bounded by `BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT`, after which
/// the process exits 0 and the hung child is force-killed. This also asserts,
/// at process level, that triggered agents' output is forwarded to the
/// launcher's own stdout (so `docker logs` stays the attributed log surface)
/// and that the unset-token startup warning is loud.
#[test]
fn sigterm_with_hung_child_exits_within_bounded_grace() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("reserve local port")
        .local_addr()
        .unwrap()
        .port();
    let config = temp_path("hung-config");
    fs::write(
        &config,
        format!(
            r#"
[server]
addr = "127.0.0.1:{port}"

[[agents]]
name = "quick"
command = "/bin/sh"
args = ["-c", "printf 'quick-ran\\n'"]

[[agents]]
name = "hung"
command = "/bin/sh"
args = ["-c", "printf 'hung-started\\n'; sleep 60"]
"#
        ),
    )
    .expect("write hung fixture config");

    let stdout_file = temp_path("hung-stdout");
    let stderr_file = temp_path("hung-stderr");
    let mut server = Command::new(env!("CARGO_BIN_EXE_baeapi"))
        .env("BAE_LAUNCHER_API_CONFIG", &config)
        .env("BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT", "1")
        .env_remove("BAE_LAUNCHER_API_TOKEN")
        .env("BAE_LOG", "info")
        .stdout(fs::File::create(&stdout_file).expect("stdout capture"))
        .stderr(fs::File::create(&stderr_file).expect("stderr capture"))
        .spawn()
        .expect("start baeapi");
    wait_until_ready(port);

    // The completed trigger's output reaches BOTH the response body and the
    // launcher's own stdout, `[name]`-prefixed.
    let quick = curl(port, "/agents/quick/trigger", Some("{}"), None);
    assert!(quick.contains("[quick] quick-ran") && quick.ends_with("\n200"));

    // Park a trigger on the hung agent; wait until its child has started.
    let hung_request = thread::spawn(move || {
        // The connection dies when the server exits; ignore curl's failure.
        let _ = Command::new("/usr/bin/curl")
            .args([
                "--silent",
                "--max-time",
                "30",
                "--data",
                "{}",
                &format!("http://127.0.0.1:{port}/agents/hung/trigger"),
            ])
            .output();
    });
    for _ in 0..100 {
        let logged = fs::read_to_string(&stdout_file).unwrap_or_default();
        if logged.contains("[hung] hung-started") {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let logged = fs::read_to_string(&stdout_file).unwrap_or_default();
    assert!(
        logged.contains("[quick] quick-ran") && logged.contains("[hung] hung-started"),
        "launcher stdout must carry both agents' attributed output, got: {logged:?}"
    );

    let pid = server.id().to_string();
    let signal = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {pid}")])
        .status()
        .expect("signal baeapi");
    assert!(signal.success());

    // The 1s drain bound plus scheduling slack: the launcher must be gone well
    // before the hung child's 60s sleep.
    let mut exited = None;
    for _ in 0..100 {
        if let Some(status) = server.try_wait().expect("poll baeapi") {
            exited = Some(status);
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let status = exited.expect("baeapi still running 10s after SIGTERM with a hung child");
    assert!(status.success(), "bounded shutdown exits 0, got {status}");
    hung_request.join().expect("hung request thread");

    // The unset-token posture is loudly warned about at startup.
    let warnings = fs::read_to_string(&stderr_file).unwrap_or_default();
    assert!(
        warnings.contains("BAE_LAUNCHER_API_TOKEN is UNSET"),
        "open-port startup warning missing from launcher logs: {warnings:?}"
    );

    fs::remove_file(config).ok();
    fs::remove_file(stdout_file).ok();
    fs::remove_file(stderr_file).ok();
}

/// A missing config file is nonfatal at process level: `baeapi` warns, starts
/// with zero agents, and still serves its fixed routes.
#[test]
fn missing_config_warns_and_starts_with_zero_agents() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("reserve local port")
        .local_addr()
        .unwrap()
        .port();
    let missing = temp_path("never-written-config");
    fs::remove_file(&missing).ok();

    let stderr_file = temp_path("missing-config-stderr");
    let mut server = Command::new(env!("CARGO_BIN_EXE_baeapi"))
        .env("BAE_LAUNCHER_API_CONFIG", &missing)
        .env("BAE_LAUNCHER_API_ADDR", format!("127.0.0.1:{port}"))
        .env("BAE_LOG", "warn")
        .stderr(fs::File::create(&stderr_file).expect("stderr capture"))
        .spawn()
        .expect("start baeapi");
    wait_until_ready(port);

    let health = curl(port, "/healthz", None, None);
    assert!(health.ends_with("\n200"));
    let listed = curl(port, "/_launcher/agents", None, None);
    assert!(listed.starts_with("[]"), "zero agents listed: {listed}");

    let pid = server.id().to_string();
    Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {pid}")])
        .status()
        .expect("stop baeapi");
    let status = server.wait().expect("wait for baeapi");
    assert!(status.success());

    let warnings = fs::read_to_string(&stderr_file).unwrap_or_default();
    assert!(
        warnings.contains("no config file found"),
        "missing-config warning absent: {warnings:?}"
    );
    fs::remove_file(stderr_file).ok();
}

/// The webapp launcher (work item 0014 section D) reuses `baeapi` unmodified,
/// gated by `BAE_LAUNCHER_WEBAPP_STATIC_DIR`. This points at a small committed
/// fixture SPA (`tests/fixtures/webapp-dist/`) rather than the real
/// `launchers/webapp/web/dist` build output, so this test stays self-contained
/// and offline — `make -C launchers/api test` never needs a Node toolchain or a
/// prior `make -C launchers/webapp/web build`.
#[test]
fn webapp_static_dir_serves_spa_and_never_leaks_agent_secrets() {
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("reserve local port")
        .local_addr()
        .unwrap()
        .port();
    let config = temp_path("webapp-config");
    fs::write(
        &config,
        format!(
            r#"
[server]
addr = "127.0.0.1:{port}"

[[agents]]
name = "summarize"
command = "/bin/sh"
args = ["-c", "printf 'summarize\\n'"]
env = {{ SECRET_TOKEN = "${{WEBAPP_TEST_SECRET}}" }}
display_name = "Summarizer"
description = "Summarizes long documents."
icon = "📝"
[agents.request_schema]
type = "object"
required = ["prompt"]
[agents.request_schema.properties.prompt]
type = "string"

[[agents]]
name = "translate"
command = "/bin/sh"
args = ["-c", "printf 'translate\\n'"]
display_name = "Translator"
description = "Translates text between languages."
[agents.request_schema]
type = "object"
required = ["text"]
[agents.request_schema.properties.text]
type = "string"
"#
        ),
    )
    .expect("write webapp fixture config");

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/webapp-dist");
    assert!(
        static_dir.join("index.html").is_file(),
        "fixture SPA is missing: {}",
        static_dir.display()
    );

    let mut server = Command::new(env!("CARGO_BIN_EXE_baeapi"))
        .env("BAE_LAUNCHER_API_CONFIG", &config)
        .env("BAE_LAUNCHER_WEBAPP_STATIC_DIR", &static_dir)
        .env_remove("WEBAPP_TEST_SECRET")
        .env("BAE_LOG", "error")
        .spawn()
        .expect("start baeapi");
    wait_until_ready(port);

    // GET /_launcher/agents: both agents present, safe fields only — no
    // `command`, `env`, `env_template`, or the resolved `${WEBAPP_TEST_SECRET}`
    // value ever appears (that env var is intentionally left unset above, so
    // any leak would be unmistakable, not just coincidentally absent).
    let listed = curl(port, "/_launcher/agents", None, None);
    assert!(
        listed.ends_with("\n200"),
        "introspection response: {listed}"
    );
    assert!(listed.contains("\"summarize\"") && listed.contains("\"translate\""));
    assert!(listed.contains("Summarizer") && listed.contains("Translator"));
    assert!(!listed.contains("\"command\""));
    assert!(!listed.contains("\"env\""));
    assert!(!listed.contains("env_template"));
    assert!(!listed.contains("SECRET_TOKEN"));
    assert!(!listed.contains("${"));

    // GET /: the SPA's index.html, not a 404.
    let root = curl(port, "/", None, None);
    assert!(root.ends_with("\n200"), "root response: {root}");
    assert!(root.contains("bae-launcher-webapp-fixture-root"));

    // GET /agents/summarize (an unknown *client-side* route — note this is not
    // the `/agents/{name}/trigger` API route): falls back to the same
    // index.html rather than a router 404, matching the SPA client-routing
    // contract the webapp frontend needs.
    let unknown_route = curl(port, "/agents/summarize", None, None);
    assert!(
        unknown_route.ends_with("\n200"),
        "client-route fallback: {unknown_route}"
    );
    assert!(unknown_route.contains("bae-launcher-webapp-fixture-root"));

    // The real API/introspection routes still take priority over the static
    // fallback — `/healthz` stays a plain empty 200, not the SPA body.
    let health = curl(port, "/healthz", None, None);
    assert_eq!(health, "\n200");

    let pid = server.id().to_string();
    let signal = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {pid}")])
        .status()
        .expect("stop baeapi");
    assert!(signal.success());
    let status = server.wait().expect("wait for baeapi");
    assert!(status.success(), "baeapi exited with {status}");
    fs::remove_file(config).ok();
}
