//! Sandbox drivers: containerised shell execution for sessions.
//!
//! Mirrors the shape of [`super::mcp`]: a small hand-rolled surface over an
//! external, potentially slow, subprocess-backed capability, a closed set of
//! implementations, and every failure mode captured explicitly.
//!
//! # Why hand-rolled
//!
//! Exactly as `mcp.rs` hand-rolls its JSON-RPC subset rather than pulling the
//! official SDK, this module shells out to the `docker` / `container` CLIs via
//! [`tokio::process::Command`] instead of depending on a Docker Engine API
//! client crate. The engine needs four operations (inspect/pull, run, exec,
//! stop); a CLI subprocess per call keeps the dependency graph small, the
//! behaviour fully under our control, and test builds deterministic and
//! offline (tests exercise a mock [`SandboxDriver`], never a real daemon).
//!
//! # Lifecycle
//!
//! One driver is chosen server-wide at startup (`BAE_SANDBOX_DRIVER`, see
//! [`crate::config`]) and held on `AppState` as an `Arc<dyn SandboxDriver>`.
//! Images are provisioned in the background at profile-write time
//! ([`SandboxDriver::ensure_image`]), a session's one remote sandbox is started
//! on demand (`session.startRemoteSandbox`) and retained on `AppState` keyed by
//! session id, and it is stopped either explicitly
//! (`session.stopRemoteSandbox`) or at session close — the same teardown point
//! that shuts down the session's MCP connections.
//!
//! # Trait shape
//!
//! The trait is consumed as a trait object (`Arc<dyn SandboxDriver>`), so its
//! async methods are expressed as boxed futures ([`BoxFuture`]) rather than
//! `async fn` — `async fn` in traits is not dyn-compatible. Implementations
//! just wrap their bodies in `Box::pin(async move { ... })`.

use std::future::Future;
use std::pin::Pin;
use std::process::Output;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A boxed, sendable future — the return shape of every [`SandboxDriver`]
/// method, so the trait stays dyn-compatible.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Cap on how much captured stderr is carried into a [`SandboxError`] detail,
/// so a chatty CLI failure cannot bloat an event payload or log line.
const STDERR_DETAIL_MAX: usize = 1024;

/// A running sandboxed container, opaque to callers beyond its id.
#[derive(Debug, Clone)]
pub struct SandboxHandle {
    pub id: String,
    pub image: String,
}

/// The captured output of one command run inside a sandbox.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// How [`SandboxDriver::ensure_image`] satisfied the request, so the
/// provisioning task can log "already available" vs "pulled successfully".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// The image was already present locally; nothing was pulled.
    AlreadyPresent,
    /// The image was absent and has been pulled.
    Pulled,
}

/// A failure from a sandbox driver call.
#[derive(Debug)]
pub enum SandboxError {
    /// This host/platform cannot run this driver (e.g. Apple Containers on
    /// Linux).
    Unsupported(String),
    /// Pulling/inspecting an image failed.
    Image { image: String, detail: String },
    /// Starting, execing into, or stopping a container failed.
    Runtime { detail: String },
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Unsupported(m) => write!(f, "sandbox driver unsupported: {m}"),
            SandboxError::Image { image, detail } => {
                write!(f, "sandbox image {image:?} error: {detail}")
            }
            SandboxError::Runtime { detail } => write!(f, "sandbox runtime error: {detail}"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// A container engine the server can launch sandboxes with.
///
/// One implementation is selected server-wide at startup; `availableSandboxes`
/// on a profile is the per-profile *image allowlist* layered on top of this
/// host-wide driver.
pub trait SandboxDriver: Send + Sync {
    /// The container CLI used for `exec`. Kept on the driver so another
    /// server-managed workload can run inside the same already-started
    /// container without introducing a host-execution fallback.
    fn cli_program(&self) -> &'static str {
        "docker"
    }
    /// Idempotent: inspect `image` locally; pull it if absent. Called both by
    /// the profile-write background task and defensively before [`Self::start`].
    fn ensure_image<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<EnsureOutcome, SandboxError>>;

    /// Start a long-lived container from `image` (keep-alive command, no
    /// published ports — exec is the only interaction surface) and return its
    /// handle.
    fn start<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<SandboxHandle, SandboxError>>;

    /// Run one shell command inside an already-started container and capture
    /// stdout/stderr/exit code. Never interprets the command string itself —
    /// the caller is responsible for any interpolation before this point.
    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> BoxFuture<'a, Result<ExecResult, SandboxError>>;

    /// Stop and remove the container. Idempotent; safe to call on an
    /// already-gone id.
    fn stop<'a>(&'a self, handle: &'a SandboxHandle) -> BoxFuture<'a, Result<(), SandboxError>>;
}

// ---------------------------------------------------------------------------
// Subprocess plumbing
// ---------------------------------------------------------------------------

/// The one seam between a driver and the host: run a CLI to completion and
/// capture its output. Injected so unit tests can script exit codes and
/// output without a real `docker`/`container` binary (the same offline
/// posture as the MCP tests' `command = "true"` fixture).
pub trait CommandRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, std::io::Result<Output>>;

    /// Like [`Self::run`], with bytes written to the child's stdin before it
    /// is awaited. Test runners that only model completed commands may retain
    /// the default; the production runner implements real stdin plumbing.
    fn run_with_stdin<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        _stdin: Option<&'a [u8]>,
    ) -> BoxFuture<'a, std::io::Result<Output>> {
        self.run(program, args)
    }
}

/// The production runner: [`tokio::process::Command`], output fully captured,
/// `kill_on_drop` as a backstop against leaking a wedged CLI invocation.
pub struct TokioCommandRunner;

impl CommandRunner for TokioCommandRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, std::io::Result<Output>> {
        self.run_with_stdin(program, args, None)
    }

    fn run_with_stdin<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> BoxFuture<'a, std::io::Result<Output>> {
        Box::pin(async move {
            let mut cmd = Command::new(program);
            cmd.args(args).kill_on_drop(true);
            if stdin.is_some() {
                cmd.stdin(std::process::Stdio::piped());
            }
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let mut child = cmd.spawn()?;
            let stdin_writer = if let (Some(bytes), Some(mut input)) = (stdin, child.stdin.take()) {
                let bytes = bytes.to_vec();
                // Drain stdout/stderr through `wait_with_output` concurrently
                // with a potentially large prompt write. Awaiting the write
                // first can deadlock when the child fills stdout before it
                // consumes all of stdin.
                Some(tokio::spawn(async move {
                    input.write_all(&bytes).await?;
                    input.shutdown().await
                }))
            } else {
                None
            };
            let output = child.wait_with_output().await?;
            if let Some(writer) = stdin_writer {
                writer.await.map_err(std::io::Error::other)??;
            }
            Ok(output)
        })
    }
}

/// A finished CLI invocation, decoded for the drivers' exit-code checks.
struct CliOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl CliOutput {
    fn ok(&self) -> bool {
        self.exit_code == 0
    }

    /// stderr truncated to a detail-sized string (stdout as a fallback when
    /// stderr is empty, e.g. a CLI that reports errors on stdout).
    fn detail(&self) -> String {
        let src = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let mut s = src.trim().to_owned();
        if s.len() > STDERR_DETAIL_MAX {
            s.truncate(STDERR_DETAIL_MAX);
            s.push('…');
        }
        s
    }
}

/// Run `program args…`, mapping a spawn failure (missing binary, permissions)
/// to [`SandboxError::Runtime`] — never a panic, exactly like an MCP connect
/// failure surfaces as a structured error the caller logs and skips.
async fn run_cli(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
) -> Result<CliOutput, SandboxError> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let output = runner
        .run(program, &args)
        .await
        .map_err(|e| SandboxError::Runtime {
            detail: format!("failed to run {program:?}: {e}"),
        })?;
    Ok(CliOutput {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

// ---------------------------------------------------------------------------
// Docker driver
// ---------------------------------------------------------------------------

/// Shells out to the `docker` CLI. The default driver (`BAE_SANDBOX_DRIVER`
/// unset or `docker`).
pub struct DockerDriver {
    runner: Arc<dyn CommandRunner>,
}

impl Default for DockerDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl DockerDriver {
    pub fn new() -> Self {
        Self::with_runner(Arc::new(TokioCommandRunner))
    }

    /// Test seam: run the same CLI invocations against a scripted runner.
    pub fn with_runner(runner: Arc<dyn CommandRunner>) -> Self {
        DockerDriver { runner }
    }
}

impl SandboxDriver for DockerDriver {
    fn cli_program(&self) -> &'static str {
        "docker"
    }
    fn ensure_image<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<EnsureOutcome, SandboxError>> {
        Box::pin(async move {
            // `docker image inspect` exits 0 iff the image is present locally.
            let inspect = run_cli(&*self.runner, "docker", &["image", "inspect", image]).await?;
            if inspect.ok() {
                return Ok(EnsureOutcome::AlreadyPresent);
            }
            let pull = run_cli(&*self.runner, "docker", &["pull", image]).await?;
            if pull.ok() {
                Ok(EnsureOutcome::Pulled)
            } else {
                Err(SandboxError::Image {
                    image: image.to_owned(),
                    detail: pull.detail(),
                })
            }
        })
    }

    fn start<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<SandboxHandle, SandboxError>> {
        Box::pin(async move {
            // Long-lived keep-alive container; no published ports — exec is
            // the only interaction surface. `--rm` keeps a stopped container
            // from lingering on the host.
            let run = run_cli(
                &*self.runner,
                "docker",
                &["run", "-d", "--rm", image, "sleep", "infinity"],
            )
            .await?;
            if !run.ok() {
                return Err(SandboxError::Runtime {
                    detail: format!("docker run {image:?} failed: {}", run.detail()),
                });
            }
            let id = run.stdout.trim().to_owned();
            if id.is_empty() {
                return Err(SandboxError::Runtime {
                    detail: "docker run printed no container id".to_owned(),
                });
            }
            Ok(SandboxHandle {
                id,
                image: image.to_owned(),
            })
        })
    }

    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            let out = run_cli(
                &*self.runner,
                "docker",
                &["exec", &handle.id, "sh", "-c", command],
            )
            .await?;
            // A non-zero exit here is normally the *command's* own exit code
            // passed through by `docker exec` — that is a successful exec with
            // a failing command, reported in ExecResult, not a driver error.
            Ok(ExecResult {
                stdout: out.stdout,
                stderr: out.stderr,
                exit_code: out.exit_code,
            })
        })
    }

    fn stop<'a>(&'a self, handle: &'a SandboxHandle) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            let out = run_cli(&*self.runner, "docker", &["stop", &handle.id]).await?;
            // Idempotency: stopping an already-gone container (e.g. `--rm`
            // already reaped it) is success, not an error.
            if out.ok()
                || out
                    .stderr
                    .to_ascii_lowercase()
                    .contains("no such container")
            {
                Ok(())
            } else {
                Err(SandboxError::Runtime {
                    detail: format!("docker stop {:?} failed: {}", handle.id, out.detail()),
                })
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Apple Containers driver
// ---------------------------------------------------------------------------

/// Shells out to Apple's `container` CLI (`BAE_SANDBOX_DRIVER=apple-container`).
///
/// The constructor takes the host OS as a parameter (normally
/// [`std::env::consts::OS`]) rather than hard-coding a `cfg!` check, so tests
/// can exercise the unsupported-platform rejection on any host. On a non-macOS
/// host it fails immediately with [`SandboxError::Unsupported`] — one clear,
/// structured failure at startup instead of a confusing subprocess-spawn error
/// on first use.
pub struct AppleContainerDriver {
    runner: Arc<dyn CommandRunner>,
}

impl AppleContainerDriver {
    pub fn new(os: &str) -> Result<Self, SandboxError> {
        Self::with_runner(os, Arc::new(TokioCommandRunner))
    }

    /// Test seam: same OS gate, scripted CLI runner.
    pub fn with_runner(os: &str, runner: Arc<dyn CommandRunner>) -> Result<Self, SandboxError> {
        if os != "macos" {
            return Err(SandboxError::Unsupported(format!(
                "the apple-container driver requires macOS; this host is {os:?}"
            )));
        }
        Ok(AppleContainerDriver { runner })
    }
}

impl SandboxDriver for AppleContainerDriver {
    fn cli_program(&self) -> &'static str {
        "container"
    }
    fn ensure_image<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<EnsureOutcome, SandboxError>> {
        Box::pin(async move {
            let inspect =
                run_cli(&*self.runner, "container", &["images", "inspect", image]).await?;
            if inspect.ok() {
                return Ok(EnsureOutcome::AlreadyPresent);
            }
            let pull = run_cli(&*self.runner, "container", &["images", "pull", image]).await?;
            if pull.ok() {
                Ok(EnsureOutcome::Pulled)
            } else {
                Err(SandboxError::Image {
                    image: image.to_owned(),
                    detail: pull.detail(),
                })
            }
        })
    }

    fn start<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<SandboxHandle, SandboxError>> {
        Box::pin(async move {
            let run = run_cli(
                &*self.runner,
                "container",
                &["run", "-d", "--rm", image, "sleep", "infinity"],
            )
            .await?;
            if !run.ok() {
                return Err(SandboxError::Runtime {
                    detail: format!("container run {image:?} failed: {}", run.detail()),
                });
            }
            let id = run.stdout.trim().to_owned();
            if id.is_empty() {
                return Err(SandboxError::Runtime {
                    detail: "container run printed no container id".to_owned(),
                });
            }
            Ok(SandboxHandle {
                id,
                image: image.to_owned(),
            })
        })
    }

    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            let out = run_cli(
                &*self.runner,
                "container",
                &["exec", &handle.id, "sh", "-c", command],
            )
            .await?;
            Ok(ExecResult {
                stdout: out.stdout,
                stderr: out.stderr,
                exit_code: out.exit_code,
            })
        })
    }

    fn stop<'a>(&'a self, handle: &'a SandboxHandle) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            let out = run_cli(&*self.runner, "container", &["stop", &handle.id]).await?;
            if out.ok()
                || out
                    .stderr
                    .to_ascii_lowercase()
                    .contains("no such container")
            {
                Ok(())
            } else {
                Err(SandboxError::Runtime {
                    detail: format!("container stop {:?} failed: {}", handle.id, out.detail()),
                })
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Unsupported fallback
// ---------------------------------------------------------------------------

/// A driver whose every call fails with [`SandboxError::Unsupported`].
///
/// Installed when `BAE_SANDBOX_DRIVER=apple-container` is selected on a
/// non-macOS host but **no** profile declares `available_sandboxes` — the
/// server may still start (nothing needs sandboxes yet), but any later attempt
/// to use one surfaces the original misconfiguration verbatim. If any profile
/// *does* declare images at startup, `cli::run_serve` refuses to start instead
/// (usage error, exit 2).
pub struct UnsupportedDriver {
    reason: String,
}

impl UnsupportedDriver {
    pub fn new(reason: impl Into<String>) -> Self {
        UnsupportedDriver {
            reason: reason.into(),
        }
    }

    fn err(&self) -> SandboxError {
        SandboxError::Unsupported(self.reason.clone())
    }
}

impl SandboxDriver for UnsupportedDriver {
    fn ensure_image<'a>(
        &'a self,
        _image: &'a str,
    ) -> BoxFuture<'a, Result<EnsureOutcome, SandboxError>> {
        Box::pin(async move { Err(self.err()) })
    }

    fn start<'a>(&'a self, _image: &'a str) -> BoxFuture<'a, Result<SandboxHandle, SandboxError>> {
        Box::pin(async move { Err(self.err()) })
    }

    fn exec<'a>(
        &'a self,
        _handle: &'a SandboxHandle,
        _command: &'a str,
    ) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async move { Err(self.err()) })
    }

    fn stop<'a>(&'a self, _handle: &'a SandboxHandle) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move { Err(self.err()) })
    }
}

// ---------------------------------------------------------------------------
// Image provisioning status
// ---------------------------------------------------------------------------

/// Pull status of one profile-declared sandbox image, tracked in-memory on
/// `AppState.sandbox_status` (`profile_id -> image -> status`). Seeded at
/// `Pending` synchronously when a profile is written (and for every declared
/// image at server startup), then resolved by the background provisioning
/// task. Never persisted — a restart re-triggers `ensure_image`, so status is
/// never permanently stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxImageStatus {
    Pending,
    Available,
    Error(String),
}

impl SandboxImageStatus {
    /// The wire string used in `session.sandbox.available` payloads and the
    /// admin `GET /admin/v1/sandbox-status` response.
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxImageStatus::Pending => "pending",
            SandboxImageStatus::Available => "available",
            SandboxImageStatus::Error(_) => "error",
        }
    }

    /// The failure detail, present only for [`SandboxImageStatus::Error`].
    pub fn detail(&self) -> Option<&str> {
        match self {
            SandboxImageStatus::Error(d) => Some(d),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Every driver is exercised **only** through the injected [`CommandRunner`]
// seam — a scripted, call-recording fake — so the whole suite runs offline with
// no `docker`/`container` binary or daemon present, mirroring the MCP tests'
// `command = "true"` posture. Assertions target three things the drivers own:
// (1) `ensure_image` idempotency by CLI call count (a present image never
// pulls); (2) non-zero exit → the correct `SandboxError` variant; and (3) the
// `AppleContainerDriver` OS gate.
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::Mutex;

    /// One scripted subprocess outcome the fake runner returns in order.
    enum Scripted {
        /// A completed process with this exit code and captured output.
        Exit {
            code: i32,
            stdout: String,
            stderr: String,
        },
        /// A spawn failure (missing binary, permission denied) — the `io::Error`
        /// `tokio::process` would surface, mapped by `run_cli` to `Runtime`.
        SpawnError,
    }

    impl Scripted {
        fn ok(stdout: &str) -> Self {
            Scripted::Exit {
                code: 0,
                stdout: stdout.to_owned(),
                stderr: String::new(),
            }
        }
        fn fail(code: i32, stderr: &str) -> Self {
            Scripted::Exit {
                code,
                stdout: String::new(),
                stderr: stderr.to_owned(),
            }
        }
    }

    /// A [`CommandRunner`] that records every `(program, args)` invocation and
    /// returns pre-scripted outcomes in FIFO order (defaulting to a clean exit-0
    /// once the script is exhausted, so an over-run test fails loudly on the
    /// assertion rather than a panic).
    struct RecordingRunner {
        scripted: Mutex<VecDeque<Scripted>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl RecordingRunner {
        fn new(scripted: Vec<Scripted>) -> Arc<Self> {
            Arc::new(RecordingRunner {
                scripted: Mutex::new(scripted.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            })
        }

        /// Every recorded call as a flat `"program arg1 arg2 …"` string, in order.
        fn call_lines(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(p, a)| {
                    if a.is_empty() {
                        p.clone()
                    } else {
                        format!("{p} {}", a.join(" "))
                    }
                })
                .collect()
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        /// How many recorded calls contain `needle` anywhere in their argv.
        fn calls_matching(&self, needle: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a.iter().any(|s| s.contains(needle)))
                .count()
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
        ) -> BoxFuture<'a, std::io::Result<Output>> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_owned(), args.to_vec()));
            let next = self.scripted.lock().unwrap().pop_front();
            Box::pin(async move {
                match next {
                    Some(Scripted::SpawnError) => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no such file or directory",
                    )),
                    Some(Scripted::Exit {
                        code,
                        stdout,
                        stderr,
                    }) => Ok(Output {
                        status: ExitStatus::from_raw(code << 8),
                        stdout: stdout.into_bytes(),
                        stderr: stderr.into_bytes(),
                    }),
                    None => Ok(Output {
                        status: ExitStatus::from_raw(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    }),
                }
            })
        }
    }

    fn docker(runner: Arc<RecordingRunner>) -> DockerDriver {
        DockerDriver::with_runner(runner)
    }

    // --- ensure_image idempotency by call count ---

    #[tokio::test]
    async fn ensure_image_present_is_a_single_inspect_no_pull() {
        // `docker image inspect` exits 0 ⇒ present ⇒ no pull, one CLI call.
        let runner = RecordingRunner::new(vec![Scripted::ok("")]);
        let d = docker(runner.clone());
        let out = d.ensure_image("python:3.12").await.unwrap();
        assert_eq!(out, EnsureOutcome::AlreadyPresent);
        assert_eq!(runner.call_count(), 1, "present image must not pull");
        assert_eq!(runner.calls_matching("pull"), 0);
        assert_eq!(
            runner.call_lines()[0],
            "docker image inspect python:3.12",
            "first call inspects the image"
        );
    }

    #[tokio::test]
    async fn ensure_image_idempotent_across_repeated_calls_never_pulls() {
        // Two ensure_image calls on an always-present image ⇒ two inspects,
        // zero pulls — the idempotency-by-call-count assertion.
        let runner = RecordingRunner::new(vec![Scripted::ok(""), Scripted::ok("")]);
        let d = docker(runner.clone());
        assert_eq!(
            d.ensure_image("img").await.unwrap(),
            EnsureOutcome::AlreadyPresent
        );
        assert_eq!(
            d.ensure_image("img").await.unwrap(),
            EnsureOutcome::AlreadyPresent
        );
        assert_eq!(runner.call_count(), 2, "one inspect per call");
        assert_eq!(
            runner.calls_matching("pull"),
            0,
            "never pulls a present image"
        );
    }

    #[tokio::test]
    async fn ensure_image_absent_pulls_then_reports_pulled() {
        // inspect exits non-zero ⇒ absent ⇒ pull; two calls, second is a pull.
        let runner =
            RecordingRunner::new(vec![Scripted::fail(1, "No such image"), Scripted::ok("")]);
        let d = docker(runner.clone());
        let out = d.ensure_image("node:22").await.unwrap();
        assert_eq!(out, EnsureOutcome::Pulled);
        assert_eq!(runner.call_count(), 2);
        assert_eq!(runner.call_lines()[1], "docker pull node:22");
    }

    #[tokio::test]
    async fn ensure_image_pull_failure_maps_to_image_error() {
        let runner = RecordingRunner::new(vec![
            Scripted::fail(1, "absent"),
            Scripted::fail(1, "pull access denied for private/x"),
        ]);
        let d = docker(runner);
        let err = d.ensure_image("private/x").await.unwrap_err();
        match err {
            SandboxError::Image { image, detail } => {
                assert_eq!(image, "private/x");
                assert!(detail.contains("pull access denied"), "detail: {detail}");
            }
            other => panic!("expected Image error, got {other:?}"),
        }
    }

    // --- start: exit code → SandboxError, id parsing ---

    #[tokio::test]
    async fn start_success_parses_container_id_from_stdout() {
        let runner = RecordingRunner::new(vec![Scripted::ok("deadbeefcafe\n")]);
        let d = docker(runner.clone());
        let handle = d.start("python:3.12").await.unwrap();
        assert_eq!(handle.id, "deadbeefcafe", "trimmed container id");
        assert_eq!(handle.image, "python:3.12");
        assert_eq!(
            runner.call_lines()[0],
            "docker run -d --rm python:3.12 sleep infinity"
        );
    }

    #[tokio::test]
    async fn start_nonzero_exit_maps_to_runtime_error() {
        let runner = RecordingRunner::new(vec![Scripted::fail(125, "docker: daemon not running")]);
        let d = docker(runner);
        let err = d.start("img").await.unwrap_err();
        match err {
            SandboxError::Runtime { detail } => {
                assert!(detail.contains("daemon not running"), "detail: {detail}");
            }
            other => panic!("expected Runtime error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_empty_stdout_is_a_runtime_error() {
        // A zero exit that prints no id is still a failure (nothing to exec into).
        let runner = RecordingRunner::new(vec![Scripted::ok("   \n")]);
        let d = docker(runner);
        let err = d.start("img").await.unwrap_err();
        assert!(matches!(err, SandboxError::Runtime { .. }));
    }

    // --- exec: non-zero exit is the command's result, not a driver error ---

    #[tokio::test]
    async fn exec_passes_a_nonzero_command_exit_through_as_result() {
        let runner = RecordingRunner::new(vec![Scripted::Exit {
            code: 3,
            stdout: "partial\n".into(),
            stderr: "boom\n".into(),
        }]);
        let d = docker(runner.clone());
        let handle = SandboxHandle {
            id: "c1".into(),
            image: "img".into(),
        };
        let r = d.exec(&handle, "false && echo hi").await.unwrap();
        assert_eq!(
            r.exit_code, 3,
            "the command's own exit is surfaced, not an error"
        );
        assert_eq!(r.stdout, "partial\n");
        assert_eq!(r.stderr, "boom\n");
        // The command string is handed to `sh -c`, never re-interpreted.
        assert_eq!(
            runner.call_lines()[0],
            "docker exec c1 sh -c false && echo hi"
        );
    }

    // --- stop: idempotency and error mapping ---

    #[tokio::test]
    async fn stop_success_calls_docker_stop() {
        let runner = RecordingRunner::new(vec![Scripted::ok("")]);
        let d = docker(runner.clone());
        let handle = SandboxHandle {
            id: "c9".into(),
            image: "img".into(),
        };
        d.stop(&handle).await.unwrap();
        assert_eq!(runner.call_lines()[0], "docker stop c9");
    }

    #[tokio::test]
    async fn stop_no_such_container_is_idempotent_success() {
        // A container already reaped by `--rm` is not an error to stop.
        let runner = RecordingRunner::new(vec![Scripted::fail(
            1,
            "Error response: No such container: c9",
        )]);
        let d = docker(runner);
        let handle = SandboxHandle {
            id: "c9".into(),
            image: "img".into(),
        };
        assert!(d.stop(&handle).await.is_ok());
    }

    #[tokio::test]
    async fn stop_other_failure_maps_to_runtime_error() {
        let runner = RecordingRunner::new(vec![Scripted::fail(1, "permission denied")]);
        let d = docker(runner);
        let handle = SandboxHandle {
            id: "c9".into(),
            image: "img".into(),
        };
        let err = d.stop(&handle).await.unwrap_err();
        assert!(matches!(err, SandboxError::Runtime { .. }));
    }

    // --- missing binary never panics: spawn failure → Runtime ---

    #[tokio::test]
    async fn missing_binary_surfaces_as_runtime_not_a_panic() {
        let runner = RecordingRunner::new(vec![Scripted::SpawnError]);
        let d = docker(runner);
        let err = d.ensure_image("img").await.unwrap_err();
        match err {
            SandboxError::Runtime { detail } => {
                assert!(
                    detail.contains("docker"),
                    "detail names the program: {detail}"
                );
            }
            other => panic!("expected Runtime error, got {other:?}"),
        }
    }

    // --- stderr detail truncation ---

    #[tokio::test]
    async fn oversized_stderr_detail_is_truncated() {
        let huge = "x".repeat(STDERR_DETAIL_MAX + 500);
        let runner = RecordingRunner::new(vec![Scripted::fail(125, &huge)]);
        let d = docker(runner);
        let err = d.start("img").await.unwrap_err();
        let SandboxError::Runtime { detail } = err else {
            panic!("expected Runtime error");
        };
        // The 'x' run is capped at STDERR_DETAIL_MAX (plus the ellipsis and the
        // surrounding "docker run …" framing).
        let x_run = detail.chars().filter(|&c| c == 'x').count();
        assert_eq!(x_run, STDERR_DETAIL_MAX, "stderr detail is capped");
        assert!(detail.contains('…'), "truncation marker present");
    }

    // --- AppleContainerDriver OS gate + CLI shape ---

    #[tokio::test]
    async fn apple_driver_rejects_non_macos_host() {
        // Constructed with an injected non-macOS OS override — a structured
        // Unsupported failure, never a subprocess spawn.
        let runner = RecordingRunner::new(vec![]);
        match AppleContainerDriver::with_runner("linux", runner.clone()) {
            Err(SandboxError::Unsupported(m)) => assert!(m.contains("linux"), "message: {m}"),
            Err(other) => panic!("expected Unsupported, got {other:?}"),
            Ok(_) => panic!("apple-container must be rejected on linux"),
        }
        assert_eq!(
            runner.call_count(),
            0,
            "no CLI is ever spawned on the wrong OS"
        );
    }

    #[tokio::test]
    async fn apple_driver_on_macos_uses_the_container_cli() {
        let runner = RecordingRunner::new(vec![Scripted::ok("")]);
        let d = AppleContainerDriver::with_runner("macos", runner.clone()).unwrap();
        let out = d.ensure_image("python:3.12").await.unwrap();
        assert_eq!(out, EnsureOutcome::AlreadyPresent);
        assert_eq!(
            runner.call_lines()[0],
            "container images inspect python:3.12",
            "Apple driver shells out to the `container` CLI"
        );
    }

    #[tokio::test]
    async fn apple_driver_absent_image_pulls_via_images_pull() {
        let runner = RecordingRunner::new(vec![Scripted::fail(1, "absent"), Scripted::ok("")]);
        let d = AppleContainerDriver::with_runner("macos", runner.clone()).unwrap();
        assert_eq!(
            d.ensure_image("node:22").await.unwrap(),
            EnsureOutcome::Pulled
        );
        assert_eq!(runner.call_lines()[1], "container images pull node:22");
    }

    // --- UnsupportedDriver: every call fails the same way ---

    #[tokio::test]
    async fn unsupported_driver_fails_every_call() {
        let d = UnsupportedDriver::new("apple-container on linux");
        assert!(matches!(
            d.ensure_image("img").await,
            Err(SandboxError::Unsupported(_))
        ));
        assert!(matches!(
            d.start("img").await,
            Err(SandboxError::Unsupported(_))
        ));
        let handle = SandboxHandle {
            id: "c".into(),
            image: "img".into(),
        };
        assert!(matches!(
            d.exec(&handle, "echo").await,
            Err(SandboxError::Unsupported(_))
        ));
        assert!(matches!(
            d.stop(&handle).await,
            Err(SandboxError::Unsupported(_))
        ));
    }

    // --- SandboxImageStatus wire projection ---

    #[test]
    fn image_status_wire_strings_and_detail() {
        assert_eq!(SandboxImageStatus::Pending.as_str(), "pending");
        assert_eq!(SandboxImageStatus::Available.as_str(), "available");
        assert_eq!(SandboxImageStatus::Error("x".into()).as_str(), "error");
        assert_eq!(SandboxImageStatus::Pending.detail(), None);
        assert_eq!(
            SandboxImageStatus::Error("nope".into()).detail(),
            Some("nope")
        );
    }
}
