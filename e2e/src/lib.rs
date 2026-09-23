//! Cross-component end-to-end tests for BAE.
//!
//! This crate is a container for tests that span more than one component and
//! therefore cannot live inside any single one of them. Nothing here is a
//! runtime dependency of anything: the library only holds the fixtures the
//! suites under `tests/` share (the mock model provider, the quickstart doc
//! extraction, and the engine-gated workspace helpers).
//!
//! # Why this crate exists
//!
//! `tests/telemetry.rs` proves W3C trace-context propagation joins client and
//! server spans into a single trace. Doing that honestly requires *both* the
//! real `baesrv` binary and the real `bae-rs` client SDK — if the test forged
//! the `traceparent` header itself it would be asserting its own header
//! construction rather than the shipped client's instrumentation.
//!
//! It used to live in `server/tests/`, which forced `server/` to carry a
//! `bae-rs` path dev-dependency. That inverted the component layering (the
//! client depends on the server's wire contract, not the reverse), broke the
//! "each component is independently buildable and testable" property in
//! `aspec/architecture/design.md` Principle 3, and — because Cargo resolves
//! dev-dependencies even for `cargo build --release` — made the production
//! Docker images fail to build unless `client-rust/` was in the build context.
//!
//! Putting the test in a leaf crate downstream of both components keeps the
//! coverage and removes the edge.
//!
//! `tests/quickstart.rs` and `tests/sdk_containers.rs` drive the host `baectl`
//! binary against a real Docker engine (opt-in, see [`engine`]), with the
//! server's provider pointed at [`provider_mock`] so no API key is needed.
//!
//! # Running
//!
//! Use `make test-e2e` from the repository root (or `make test` in this
//! directory), which builds `baesrv` first and points the suite at it. See
//! `baesrv_binary()` in `tests/telemetry.rs` for how the binary is located.
//! The engine-gated suites run with `make test-quickstart` /
//! `make test-engine` in this directory.

pub mod provider_mock {
    //! A tiny Anthropic-shaped model provider (`POST /v1/messages`, answered on
    //! any path) for tests that point a real `baesrv` at it.

    use std::net::{IpAddr, SocketAddr};

    use axum::extract::Request;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::{Json, Router};
    use serde_json::{json, Value};

    /// The assistant text [`quickstart_reply`] answers every turn with. The
    /// quickstart smoke test asserts the harness prints exactly this.
    pub const QUICKSTART_SMOKE_REPLY: &str = "quickstart smoke reply: the agent is wired up.";

    /// Maps one provider request body to the assistant message to return.
    pub type Responder = fn(&Value) -> Value;

    /// Serve `respond` on `bind:0` (an ephemeral port) on the current Tokio
    /// runtime and return the bound address. Bind `0.0.0.0` when a container
    /// must reach the mock through `host.docker.internal`.
    pub async fn start(bind: IpAddr, respond: Responder) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(bind, 0))
            .await
            .expect("bind provider mock");
        let addr = listener.local_addr().expect("provider mock address");
        let app = Router::new().fallback(move |request: Request| handle(request, respond));
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve provider mock");
        });
        addr
    }

    async fn handle(request: Request, respond: Responder) -> Response {
        let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("read provider request");
        let body: Value = serde_json::from_slice(&bytes).expect("provider JSON");
        (StatusCode::OK, Json(respond(&body))).into_response()
    }

    /// The canonical client-tool parity fixture: the first response asks for
    /// one client tool; the result round trip receives a final assistant
    /// message.
    pub fn tool_round_trip(body: &Value) -> Value {
        let has_tool_result = body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
        if has_tool_result {
            json!({
                "role": "assistant",
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "tool round-trip complete"}],
            })
        } else {
            json!({
                "role": "assistant",
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use",
                    "id": "tu_e2e",
                    "name": "get_current_time",
                    "input": {},
                }],
            })
        }
    }

    /// Every turn ends immediately with [`QUICKSTART_SMOKE_REPLY`].
    pub fn quickstart_reply(_body: &Value) -> Value {
        json!({
            "role": "assistant",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": QUICKSTART_SMOKE_REPLY}],
        })
    }
}

pub mod quickstart {
    //! Extraction of the copy-paste command blocks from the quickstart guides,
    //! so the smoke test runs exactly what the docs tell a reader to type.

    /// Opening marker around `docs/guides/00-quickstart.md`'s command block.
    pub const START_MARKER: &str = "<!-- quickstart-commands:start -->";
    /// Closing marker around `docs/guides/00-quickstart.md`'s command block.
    pub const END_MARKER: &str = "<!-- quickstart-commands:end -->";
    /// The HTML anchor that precedes the developer quickstart's `--dev` block.
    pub const DEV_FASTEST_PATH_ANCHOR: &str =
        r#"<a id="fastest-path-three-commands-all---dev"></a>"#;

    /// The `baectl …` lines between the single marker pair in `markdown`,
    /// trimmed, in order. Errors when the pair is missing, duplicated or out
    /// of order.
    pub fn marked_commands(markdown: &str) -> Result<Vec<String>, String> {
        let starts = markdown.matches(START_MARKER).count();
        let ends = markdown.matches(END_MARKER).count();
        if starts != 1 || ends != 1 {
            return Err(format!(
                "expected exactly one `{START_MARKER}` / `{END_MARKER}` pair, found {starts} start and {ends} end markers"
            ));
        }
        let start = markdown.find(START_MARKER).expect("counted above") + START_MARKER.len();
        let end = markdown.find(END_MARKER).expect("counted above");
        if end < start {
            return Err(format!("`{END_MARKER}` appears before `{START_MARKER}`"));
        }
        Ok(baectl_lines(&markdown[start..end]))
    }

    /// The `baectl …` lines of the first fenced code block after `anchor`,
    /// with trailing `# comments` removed.
    pub fn fenced_commands_after(markdown: &str, anchor: &str) -> Result<Vec<String>, String> {
        let at = markdown
            .find(anchor)
            .ok_or_else(|| format!("anchor `{anchor}` not found"))?;
        let rest = &markdown[at..];
        let open = rest
            .find("```")
            .ok_or_else(|| format!("no fenced code block after `{anchor}`"))?;
        let body = &rest[open + 3..];
        let body = &body[body.find('\n').map_or(body.len(), |nl| nl + 1)..];
        let close = body
            .find("```")
            .ok_or_else(|| format!("unterminated fenced code block after `{anchor}`"))?;
        Ok(baectl_lines(&body[..close]))
    }

    fn baectl_lines(text: &str) -> Vec<String> {
        text.lines()
            .map(|line| line.split(" #").next().unwrap_or_default().trim())
            .filter(|line| line.starts_with("baectl "))
            .map(str::to_owned)
            .collect()
    }

    /// The developer variant of a quickstart: ` --dev` appended to every
    /// command (the smoke test runs against the locally built image tag).
    pub fn with_dev_flag(commands: &[String]) -> Vec<String> {
        commands.iter().map(|c| format!("{c} --dev")).collect()
    }

    /// A command's whitespace-separated words, sorted (`-y` read as `--yes`);
    /// used to compare the two guides' blocks without caring about flag order.
    pub fn normalized_words(command: &str) -> Vec<&str> {
        let mut words: Vec<&str> = command
            .split_whitespace()
            .map(|word| if word == "-y" { "--yes" } else { word })
            .collect();
        words.sort_unstable();
        words
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const GOOD: &str = "intro\n\n<!-- quickstart-commands:start -->\n```sh\nbaectl setup --yes\nbaectl build demo\nbaectl run demo-rust-local\n```\n<!-- quickstart-commands:end -->\n\nbaectl outside the block\n";

        #[test]
        fn marked_commands_returns_only_the_baectl_lines_inside_the_markers() {
            assert_eq!(
                marked_commands(GOOD).unwrap(),
                [
                    "baectl setup --yes",
                    "baectl build demo",
                    "baectl run demo-rust-local"
                ]
            );
        }

        #[test]
        fn marked_commands_rejects_missing_duplicate_and_reversed_markers() {
            assert!(marked_commands("baectl setup --yes").is_err());
            assert!(marked_commands(&format!("{GOOD}{START_MARKER}")).is_err());
            assert!(marked_commands(&format!("{END_MARKER}\n{START_MARKER}")).is_err());
        }

        #[test]
        fn fenced_commands_after_strips_comments_and_non_baectl_lines() {
            let md = "## Other\n```sh\nbaectl nope\n```\n<a id=\"x\"></a>\n```sh\nmake image   # tag\nexport K=v\nbaectl setup --dev          # once\nbaectl run a --dev\n```\n```sh\nbaectl later\n```\n";
            assert_eq!(
                fenced_commands_after(md, "<a id=\"x\"></a>").unwrap(),
                ["baectl setup --dev", "baectl run a --dev"]
            );
            assert!(fenced_commands_after(md, "<a id=\"missing\"></a>").is_err());
            assert!(
                fenced_commands_after("<a id=\"x\"></a>\n```sh\nbaectl", "<a id=\"x\"></a>")
                    .is_err()
            );
        }

        #[test]
        fn with_dev_flag_appends_to_every_command() {
            let commands = vec!["baectl setup --yes".to_owned(), "baectl run x".to_owned()];
            assert_eq!(
                with_dev_flag(&commands),
                ["baectl setup --yes --dev", "baectl run x --dev"]
            );
        }

        #[test]
        fn normalized_words_ignores_flag_order_but_not_yes() {
            assert_eq!(
                normalized_words("baectl setup --yes --dev"),
                normalized_words("baectl setup --dev -y")
            );
            assert_ne!(
                normalized_words("baectl setup --yes --dev"),
                normalized_words("baectl setup --dev")
            );
            assert_ne!(
                normalized_words("baectl build a --dev"),
                normalized_words("baectl build b --dev")
            );
        }
    }
}

#[cfg(unix)]
pub mod engine {
    //! Opt-in Docker-engine fixtures shared by `tests/quickstart.rs` and
    //! `tests/sdk_containers.rs`.
    //!
    //! Offline by default, with the same gate as baectl's engine suite:
    //! `BAECTL_HARNESS_ENGINE_TESTS=1`, a responsive `docker info`, the
    //! locally built images, and a host `baectl` (`$BAECTL_BIN`, else
    //! `baectl/target/host/release/baectl` from `make build-baectl`). A missing
    //! prerequisite skips the test with a one-line reason — unless
    //! `BAE_E2E_ENGINE_REQUIRED=1` (set by `make test-quickstart` /
    //! `make test-engine` and therefore CI), where it fails instead, so a
    //! broken CI setup can never pass by skipping.

    use std::ffi::OsString;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    pub const ENGINE_TESTS_ENV: &str = "BAECTL_HARNESS_ENGINE_TESTS";
    pub const REQUIRED_ENV: &str = "BAE_E2E_ENGINE_REQUIRED";
    pub const SERVER_IMAGE: &str = "better-agent-engine:latest";
    pub const API_LAUNCHER_IMAGE: &str = "better-agent-engine:launcher-api";
    /// The placeholder provider key: `setup --yes` needs *a* key in the
    /// environment, and the mock provider ignores it.
    pub const PLACEHOLDER_KEY: &str = "test-token";
    /// The host port `setup`'s default configuration publishes the server on.
    pub const SERVER_PORT: u16 = 8080;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);
    // Every engine test publishes the same host ports (8080, 9090).
    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    /// Serialize engine tests within one test binary.
    pub fn lock() -> MutexGuard<'static, ()> {
        ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn flag(name: &str) -> bool {
        std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1"))
    }

    fn quiet_success(program: &str, args: &[&str]) -> bool {
        Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    pub fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("e2e has a repository parent")
            .to_path_buf()
    }

    fn baectl_binary() -> PathBuf {
        std::env::var_os("BAECTL_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root().join("baectl/target/host/release/baectl"))
    }

    fn skip(label: &str, reason: String) -> Option<PathBuf> {
        if flag(REQUIRED_ENV) {
            panic!("{label}: {reason} ({REQUIRED_ENV}=1 turns this skip into a failure)");
        }
        eprintln!("skipping {label}: {reason}");
        None
    }

    /// Check the gate; returns the host `baectl` binary to run, or `None`
    /// (after printing why) when the test should be skipped.
    pub fn require(label: &str, images: &[&str]) -> Option<PathBuf> {
        if !flag(ENGINE_TESTS_ENV) {
            return skip(label, format!("set {ENGINE_TESTS_ENV}=1 to opt in"));
        }
        if !quiet_success("docker", &["info"]) {
            return skip(label, "Docker is unavailable or its daemon is down".into());
        }
        for image in images {
            if !quiet_success("docker", &["image", "inspect", image]) {
                return skip(label, format!("image {image} is missing — build it with `make image` / `make image-launcher-api`"));
            }
        }
        let baectl = baectl_binary();
        if !baectl.is_file() {
            return skip(
                label,
                format!(
                    "{} not found — run `make build-baectl` or set BAECTL_BIN",
                    baectl.display()
                ),
            );
        }
        Some(baectl)
    }

    /// An isolated `baectl --dir` (the process cwd) holding a symlink to one
    /// bundled SDK. Dropping it tears down the compose project, any launched
    /// containers, and the directory.
    pub struct Workspace {
        dir: PathBuf,
        path_env: OsString,
        containers: Vec<String>,
    }

    impl Workspace {
        pub fn new(label: &str, baectl: &Path, sdk: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "bae-e2e-{label}-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create workspace");
            let dir = dir.canonicalize().expect("canonical workspace path");
            symlink(
                repo_root().join(format!("client-{sdk}")),
                dir.join(format!("client-{sdk}")),
            )
            .unwrap_or_else(|err| panic!("link bundled {sdk} client: {err}"));
            let mut paths = vec![baectl.parent().expect("baectl has a parent").to_path_buf()];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            Self {
                dir,
                path_env: std::env::join_paths(paths).expect("join PATH"),
                containers: Vec::new(),
            }
        }

        pub fn path(&self) -> &Path {
            &self.dir
        }

        /// Run one shell line verbatim in the workspace, with the host
        /// `baectl` first on `PATH` and only the placeholder provider key set.
        pub fn sh(&self, line: &str) -> Output {
            eprintln!("$ {line}");
            let output = Command::new("sh")
                .args(["-c", line])
                .current_dir(&self.dir)
                .env("PATH", &self.path_env)
                .env("ANTHROPIC_API_KEY", PLACEHOLDER_KEY)
                .env_remove("OPENAI_API_KEY")
                .stdin(Stdio::null())
                .output()
                .unwrap_or_else(|err| panic!("spawn `{line}`: {err}"));
            eprintln!(
                "  exit {:?}\n  stdout:\n{}\n  stderr:\n{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        }

        /// [`Self::sh`], asserting exit 0.
        pub fn sh_ok(&self, line: &str) -> Output {
            let output = self.sh(line);
            assert!(
                output.status.success(),
                "`{line}` failed with {:?}",
                output.status.code()
            );
            output
        }

        /// Remove `name` now and again when the workspace is dropped.
        pub fn track_container(&mut self, name: &str) {
            remove_container(name);
            self.containers.push(name.to_owned());
        }

        /// Point every provider in the generated `bae-config.toml` at
        /// `base_url`, recreate the server so it re-reads the file, and wait
        /// for `/healthz`.
        pub fn point_provider_at(&self, base_url: &str) {
            let path = self.dir.join("bae-config.toml");
            let config = fs::read_to_string(&path).expect("read generated bae-config.toml");
            assert!(
                !config.contains("base_url"),
                "setup unexpectedly wrote a base_url:\n{config}"
            );
            let header = "[[providers.entries]]";
            assert!(config.contains(header), "no provider entry in:\n{config}");
            let patched = config.replace(header, &format!("{header}\nbase_url = \"{base_url}\""));
            fs::write(&path, patched).expect("write patched bae-config.toml");
            self.sh_ok("docker compose up -d --force-recreate");
            wait_healthy(SERVER_PORT);
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            for name in &self.containers {
                remove_container(name);
            }
            let _ = Command::new("docker")
                .args(["compose", "down", "-v", "--remove-orphans"])
                .current_dir(&self.dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    pub fn remove_container(name: &str) {
        let _ = quiet_success("docker", &["rm", "-f", name]);
    }

    /// Poll `GET /healthz` on the loopback port until it answers 200.
    pub fn wait_healthy(port: u16) {
        for _ in 0..120 {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut response = String::new();
                if stream
                    .write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")
                    .is_ok()
                    && stream.read_to_string(&mut response).is_ok()
                    && response.lines().next().is_some_and(|l| l.contains(" 200 "))
                {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        panic!("server on 127.0.0.1:{port} did not report healthy within 60s");
    }
}
