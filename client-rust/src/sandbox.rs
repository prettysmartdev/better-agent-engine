//! Builtin **sandbox tools** — give an agent real shell/execution ability with
//! a security boundary the harness developer controls.
//!
//! This module mirrors the server's `engine/sandbox.rs` on the client side. It
//! offers, per the same two-driver [`SandboxDriver`] shape the server uses:
//!
//! - a **local container-engine driver** ([`DockerDriver`] / [`AppleContainerDriver`])
//!   that shells out to the `docker` / `container` CLI — no Docker SDK, the same
//!   subprocess-based approach the server takes, with byte-for-byte matching CLI
//!   invocations so "the sandbox my client started locally" and "the sandbox the
//!   server started remotely" behave identically;
//! - two tool constructors, [`run_shell_command`] (arbitrary shell) and
//!   [`run_shell_named`] (a fixed command template with `{param}` placeholders),
//!   each routable to a **local** or **remote** sandbox;
//! - a [`SandboxTarget`] / [`RemoteMode`] builder describing *where* a command
//!   runs and, for remote commands, *who* builds the `tool_result`.
//!
//! # Sandbox tools require a live [`Session`](crate::Session)
//!
//! **Unlike every other builtin tool, sandbox tools need a session handle.**
//! Local-target tools report their `running`/`stopped`/`error` lifecycle to the
//! server (`session.reportLocalSandbox`); remote-manual tools fetch raw output
//! via `session.execRemoteSandbox`. Both need a live transport.
//!
//! The idiomatic flow, therefore, differs from the pre-`connect()` builder the
//! rest of the harness uses: obtain a [`SandboxSession`] handle from the
//! [`Harness`](crate::Harness) with [`Harness::sandbox_session`], build tools
//! against it, register them, then `connect()`. The handle's transport is
//! **late-bound**: it is empty until `connect()`/`join()` fills it, and any tool
//! that fires before then returns [`Error::Sandbox`]. Because a handler can only
//! run *after* `send()` (hence after connect), this ordering is safe.
//!
//! (This late-binding is a deliberate deviation from a strict "construct after
//! connect" reading: **Auto**-mode tools must be declared in the session-open
//! `sandbox_tools` list, i.e. *before* connect — so a single pre-connect handle
//! is the only shape under which local, remote-manual, and remote-auto tools can
//! all be registered uniformly through the normal builder.)
//!
//! [`Harness::sandbox_session`]: crate::Harness::sandbox_session

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex as TokioMutex;

use crate::error::Error;
use crate::tool::{BoxError, Tool, ToolHandler};

// ---------------------------------------------------------------------------
// Core data types (mirror the server's `SandboxDriver` surface)
// ---------------------------------------------------------------------------

/// A running sandboxed container, opaque to callers beyond its id and image.
#[derive(Clone, Debug)]
pub struct SandboxHandle {
    /// Container id printed by `docker run -d` / `container run -d`.
    pub id: String,
    /// The image the container was started from.
    pub image: String,
}

/// The captured result of one command run inside a sandbox.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code (`-1` if the process was killed by a signal).
    pub exit_code: i32,
}

/// The terminal result of `session.startRemoteSandbox` — the server-hosted
/// sandbox is up and its handle retained session-wide.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteSandboxStarted {
    /// The started container's id.
    pub sandbox_id: String,
    /// The image it was started from.
    pub image: String,
    /// When it started (the `session.sandbox.running` event's `created_at`), or
    /// `null` if that log write failed.
    #[serde(default)]
    pub started_at: Option<String>,
}

/// The terminal result of `session.stopRemoteSandbox`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteSandboxStopped {
    /// Always `true` on success.
    pub stopped: bool,
    /// The stopped sandbox's image.
    pub image: String,
    /// The stopped sandbox's container id.
    pub sandbox_id: String,
}

/// A structured sandbox failure, mirroring the server's `SandboxError`.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// This host/platform cannot run this driver (e.g. Apple Containers on Linux).
    #[error("unsupported sandbox driver: {0}")]
    Unsupported(String),
    /// Pulling/inspecting an image failed.
    #[error("sandbox image `{image}` error: {detail}")]
    Image {
        /// The image name.
        image: String,
        /// Truncated CLI stderr / cause.
        detail: String,
    },
    /// Starting, execing into, or stopping a container failed.
    #[error("sandbox runtime error: {detail}")]
    Runtime {
        /// Truncated CLI stderr / cause.
        detail: String,
    },
}

/// A boxed, `Send` future — the object-safe return shape both the local
/// [`SandboxDriver`] and the [`SandboxRpc`] transport seam use so they can be
/// stored behind `Arc<dyn …>` and captured by a `'static` tool handler.
pub type SandboxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The local container-engine abstraction — a **full mirror** of the server's
/// `SandboxDriver` (`ensure_image`/`start`/`exec`/`stop`, not just `exec`),
/// object-safe so a [`SandboxSession`] can hold `Arc<dyn SandboxDriver>` and a
/// test can inject a fake. Implemented by [`DockerDriver`] and
/// [`AppleContainerDriver`].
pub trait SandboxDriver: Send + Sync {
    /// Idempotent: inspect `image` locally; pull it if absent.
    fn ensure_image<'a>(&'a self, image: &'a str) -> SandboxFuture<'a, Result<(), SandboxError>>;
    /// Start a long-lived container (keep-alive `sleep infinity`, no published
    /// ports — exec is the only interaction surface) and return its handle.
    fn start<'a>(
        &'a self,
        image: &'a str,
    ) -> SandboxFuture<'a, Result<SandboxHandle, SandboxError>>;
    /// Run one shell command inside an already-started container. Never
    /// interprets the command string itself — the caller owns interpolation.
    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> SandboxFuture<'a, Result<ExecResult, SandboxError>>;
    /// Stop and remove the container. Idempotent; safe on an already-gone id.
    fn stop<'a>(&'a self, handle: &'a SandboxHandle)
        -> SandboxFuture<'a, Result<(), SandboxError>>;
}

// ---------------------------------------------------------------------------
// CLI drivers (Docker / Apple Containers)
// ---------------------------------------------------------------------------

/// Per-engine CLI verbs. The `run`/`exec`/`stop` verbs are identical across
/// Docker and Apple's `container`; only image inspect/pull differ.
struct Cli {
    program: &'static str,
    inspect: &'static [&'static str],
    pull: &'static [&'static str],
}

const DOCKER_CLI: Cli = Cli {
    program: "docker",
    inspect: &["image", "inspect"],
    pull: &["pull"],
};

const APPLE_CLI: Cli = Cli {
    program: "container",
    inspect: &["images", "inspect"],
    pull: &["images", "pull"],
};

/// Run a CLI command to completion, capturing `(stdout, stderr, exit_code)`.
/// A spawn failure (missing binary) is a [`SandboxError::Runtime`], never a
/// panic — exactly how the engine treats a missing subprocess binary.
async fn run_cli(program: &str, args: &[&str]) -> Result<(String, String, i32), SandboxError> {
    let output = TokioCommand::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| SandboxError::Runtime {
            detail: format!("failed to spawn `{program}`: {e}"),
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);
    Ok((stdout, stderr, exit_code))
}

/// Truncate CLI stderr carried into a [`SandboxError`] so a runaway log line
/// never balloons the error message.
fn truncate(s: &str) -> String {
    const MAX: usize = 2000;
    let s = s.trim();
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}… (truncated)", &s[..MAX])
    }
}

async fn cli_ensure_image(cli: &Cli, image: &str) -> Result<(), SandboxError> {
    let mut inspect: Vec<&str> = cli.inspect.to_vec();
    inspect.push(image);
    let (_, _, code) = run_cli(cli.program, &inspect).await?;
    if code == 0 {
        return Ok(());
    }
    let mut pull: Vec<&str> = cli.pull.to_vec();
    pull.push(image);
    let (_, stderr, code) = run_cli(cli.program, &pull).await?;
    if code != 0 {
        return Err(SandboxError::Image {
            image: image.to_string(),
            detail: truncate(&stderr),
        });
    }
    Ok(())
}

async fn cli_start(cli: &Cli, image: &str) -> Result<SandboxHandle, SandboxError> {
    let (stdout, stderr, code) = run_cli(
        cli.program,
        &["run", "-d", "--rm", image, "sleep", "infinity"],
    )
    .await?;
    if code != 0 {
        return Err(SandboxError::Runtime {
            detail: truncate(&stderr),
        });
    }
    Ok(SandboxHandle {
        id: stdout.trim().to_string(),
        image: image.to_string(),
    })
}

async fn cli_exec(
    cli: &Cli,
    handle: &SandboxHandle,
    command: &str,
) -> Result<ExecResult, SandboxError> {
    // The command is passed as a single argv element to the container's `sh -c`;
    // no host shell is ever involved, so an arbitrary command can only affect
    // the sandbox, not the harness host. A non-zero exit is the command's own
    // result (surfaced in `exit_code`), not a driver error.
    let (stdout, stderr, exit_code) =
        run_cli(cli.program, &["exec", &handle.id, "sh", "-c", command]).await?;
    Ok(ExecResult {
        stdout,
        stderr,
        exit_code,
    })
}

async fn cli_stop(cli: &Cli, handle: &SandboxHandle) -> Result<(), SandboxError> {
    let (_, stderr, code) = run_cli(cli.program, &["stop", &handle.id]).await?;
    if code != 0 {
        // Idempotent: a `--rm` container that already exited is already gone.
        if stderr.contains("No such container") || stderr.contains("not found") {
            return Ok(());
        }
        return Err(SandboxError::Runtime {
            detail: truncate(&stderr),
        });
    }
    Ok(())
}

/// The Docker driver: `docker image inspect` → `docker pull` on miss;
/// `docker run -d --rm <image> sleep infinity`; `docker exec <id> sh -c <cmd>`;
/// `docker stop <id>`.
#[derive(Clone, Debug, Default)]
pub struct DockerDriver;

impl DockerDriver {
    /// Construct the Docker driver.
    pub fn new() -> Self {
        Self
    }
}

impl SandboxDriver for DockerDriver {
    fn ensure_image<'a>(&'a self, image: &'a str) -> SandboxFuture<'a, Result<(), SandboxError>> {
        Box::pin(cli_ensure_image(&DOCKER_CLI, image))
    }
    fn start<'a>(
        &'a self,
        image: &'a str,
    ) -> SandboxFuture<'a, Result<SandboxHandle, SandboxError>> {
        Box::pin(cli_start(&DOCKER_CLI, image))
    }
    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> SandboxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(cli_exec(&DOCKER_CLI, handle, command))
    }
    fn stop<'a>(
        &'a self,
        handle: &'a SandboxHandle,
    ) -> SandboxFuture<'a, Result<(), SandboxError>> {
        Box::pin(cli_stop(&DOCKER_CLI, handle))
    }
}

/// The Apple Containers driver, shaped identically against the `container` CLI
/// (`container images inspect`/`images pull`, `container run -d --rm`,
/// `container exec`, `container stop`). Its constructor fails fast with
/// [`SandboxError::Unsupported`] on a non-macOS host, so a misconfiguration
/// surfaces as one clear error rather than a confusing subprocess failure.
#[derive(Clone, Debug)]
pub struct AppleContainerDriver {
    _private: (),
}

impl AppleContainerDriver {
    /// Construct the driver, checking the host OS via [`std::env::consts::OS`].
    pub fn new() -> Result<Self, SandboxError> {
        Self::with_os(std::env::consts::OS)
    }

    /// Construct with an explicit OS string (the OS check is injected for
    /// testability rather than a hard `cfg!` in the body).
    pub fn with_os(os: &str) -> Result<Self, SandboxError> {
        if os != "macos" {
            return Err(SandboxError::Unsupported(format!(
                "Apple Containers driver requires macOS; host OS is `{os}`"
            )));
        }
        Ok(Self { _private: () })
    }
}

impl SandboxDriver for AppleContainerDriver {
    fn ensure_image<'a>(&'a self, image: &'a str) -> SandboxFuture<'a, Result<(), SandboxError>> {
        Box::pin(cli_ensure_image(&APPLE_CLI, image))
    }
    fn start<'a>(
        &'a self,
        image: &'a str,
    ) -> SandboxFuture<'a, Result<SandboxHandle, SandboxError>> {
        Box::pin(cli_start(&APPLE_CLI, image))
    }
    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> SandboxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(cli_exec(&APPLE_CLI, handle, command))
    }
    fn stop<'a>(
        &'a self,
        handle: &'a SandboxHandle,
    ) -> SandboxFuture<'a, Result<(), SandboxError>> {
        Box::pin(cli_stop(&APPLE_CLI, handle))
    }
}

// ---------------------------------------------------------------------------
// Shell escaping (the command-injection boundary)
// ---------------------------------------------------------------------------

/// POSIX single-quote escaping — the standard shell-quoting primitive. Wraps
/// `arg` in single quotes and rewrites every embedded `'` as `'\''` (close,
/// escaped quote, reopen), so the shell always treats the result as **one
/// literal argument**. This is the command-injection boundary for
/// [`run_shell_named`]: every model-supplied value is passed through here before
/// it is substituted into a command template.
pub fn shell_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Parse the ordered, unique `{param}` placeholder names out of a command
/// template. Panics on a malformed template (unterminated `{` or empty `{}`) —
/// a template is a compile-time-literal developer input, so a bad one is a bug
/// to surface loudly at construction, not a runtime tool error.
fn parse_params(template: &str) -> Vec<String> {
    let mut params: Vec<String> = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            let mut closed = false;
            for nc in chars.by_ref() {
                if nc == '}' {
                    closed = true;
                    break;
                }
                name.push(nc);
            }
            assert!(
                closed,
                "unterminated `{{` in command template: {template:?}"
            );
            assert!(
                !name.is_empty(),
                "empty `{{}}` placeholder in command template"
            );
            if !params.contains(&name) {
                params.push(name);
            }
        }
    }
    params
}

/// How a tool turns model input into the final command string.
enum CommandSource {
    /// [`run_shell_command`]: the whole command is the input's `command` field.
    FullShell,
    /// [`run_shell_named`]: substitute (shell-escaped) values into a template.
    Template(String),
}

impl CommandSource {
    fn build(&self, input: &Value) -> Result<String, BoxError> {
        match self {
            CommandSource::FullShell => input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "run_shell_command requires a string `command`".into()),
            CommandSource::Template(template) => interpolate(template, input),
        }
    }
}

/// Single left-to-right pass: copy literal text, and for each `{name}` splice in
/// the **shell-escaped** input value. A single pass (rather than repeated
/// `replace`) guarantees a substituted value can never itself be re-interpreted
/// as a placeholder.
fn interpolate(template: &str, input: &Value) -> Result<String, BoxError> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            let mut closed = false;
            for nc in chars.by_ref() {
                if nc == '}' {
                    closed = true;
                    break;
                }
                name.push(nc);
            }
            if !closed {
                return Err("unterminated `{` in command template".into());
            }
            let value = input
                .get(&name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("missing required string parameter `{name}`"))?;
            out.push_str(&shell_quote(value));
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Session RPC seam (implemented on the HTTP transport in `harness.rs`)
// ---------------------------------------------------------------------------

/// Wire params for `session.reportLocalSandbox`.
#[derive(Clone, Debug, Serialize)]
pub struct LocalSandboxReport {
    /// One of `"running"`, `"stopped"`, `"error"`.
    pub state: String,
    /// The local image name.
    pub image: String,
    /// The local container id, if one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    /// A human-readable detail (e.g. the failure message on `"error"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The object-safe transport seam a [`SandboxSession`] calls into for the two
/// new session RPC methods. Implemented on the crate's HTTP transport; a test
/// can supply its own recorder.
pub trait SandboxRpc: Send + Sync {
    /// `session.execRemoteSandbox` — run `command` in the session's live remote
    /// sandbox and return its captured output.
    fn exec_remote_sandbox(&self, command: String) -> SandboxFuture<'_, Result<ExecResult, Error>>;
    /// `session.reportLocalSandbox` — record a local sandbox lifecycle event in
    /// the session's event log (pure telemetry; the server does not verify it).
    fn report_local_sandbox(
        &self,
        report: LocalSandboxReport,
    ) -> SandboxFuture<'_, Result<(), Error>>;
}

// ---------------------------------------------------------------------------
// SandboxSession — the late-bound handle sandbox tools capture
// ---------------------------------------------------------------------------

struct SandboxInner {
    /// Filled by `connect()`/`join()`; empty beforehand.
    rpc: OnceLock<Arc<dyn SandboxRpc>>,
    /// The local container-engine driver (default: Docker).
    driver: StdMutex<Arc<dyn SandboxDriver>>,
    /// Local containers this session started, keyed by image, so `close()` can
    /// stop them and lifecycle reports have a real `container_id`.
    started: TokioMutex<HashMap<String, SandboxHandle>>,
}

/// A cheap, cloneable handle to a live [`Session`](crate::Session)'s sandbox
/// capability: the transport for the remote RPC methods, plus the local
/// container-engine driver and the set of local containers this session started.
///
/// Obtain one from [`Harness::sandbox_session`](crate::Harness::sandbox_session)
/// (before connect) or [`Session::sandbox_session`](crate::Session::sandbox_session)
/// (after), pass it to [`run_shell_command`] / [`run_shell_named`], and the
/// resulting tools capture it. Its transport is late-bound — see the
/// [module docs](self).
#[derive(Clone)]
pub struct SandboxSession {
    inner: Arc<SandboxInner>,
}

impl SandboxSession {
    /// A fresh handle with an unbound transport and the default Docker driver.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(SandboxInner {
                rpc: OnceLock::new(),
                driver: StdMutex::new(Arc::new(DockerDriver::new())),
                started: TokioMutex::new(HashMap::new()),
            }),
        }
    }

    /// Bind the transport once the session is connected. Idempotent-ish: a
    /// second bind is ignored (the first connection wins).
    pub(crate) fn bind(&self, rpc: Arc<dyn SandboxRpc>) {
        let _ = self.inner.rpc.set(rpc);
    }

    /// Replace the local container-engine driver (default Docker). Use to select
    /// [`AppleContainerDriver`], or to inject a fake in tests.
    pub fn set_local_driver(&self, driver: Arc<dyn SandboxDriver>) {
        *self.inner.driver.lock().unwrap() = driver;
    }

    fn rpc(&self) -> Result<Arc<dyn SandboxRpc>, Error> {
        self.inner.rpc.get().cloned().ok_or_else(|| {
            Error::Sandbox(
                "sandbox tool used before the session was connected; build sandbox tools from a \
                 Harness::sandbox_session() handle and register them, then connect()"
                    .to_string(),
            )
        })
    }

    fn driver(&self) -> Arc<dyn SandboxDriver> {
        self.inner.driver.lock().unwrap().clone()
    }

    /// Run `command` in the session's **remote** sandbox via
    /// `session.execRemoteSandbox` and return its captured output.
    pub async fn exec_remote_sandbox(&self, command: &str) -> Result<ExecResult, Error> {
        self.rpc()?.exec_remote_sandbox(command.to_string()).await
    }

    /// Report a **local** sandbox lifecycle transition via
    /// `session.reportLocalSandbox` (pure telemetry into the event log).
    pub async fn report_local_sandbox(
        &self,
        state: &str,
        image: &str,
        container_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), Error> {
        self.rpc()?
            .report_local_sandbox(LocalSandboxReport {
                state: state.to_string(),
                image: image.to_string(),
                container_id: container_id.map(str::to_string),
                detail: detail.map(str::to_string),
            })
            .await
    }

    /// Start (or reuse) a local container for `image`, reporting `running` on a
    /// fresh start and `error` on failure. Idempotent per image.
    pub async fn start_local(&self, image: &str) -> Result<SandboxHandle, Error> {
        // Hold the `started` lock across the driver calls so concurrent first
        // uses of the same image cannot double-start it.
        let mut started = self.inner.started.lock().await;
        if let Some(handle) = started.get(image) {
            return Ok(handle.clone());
        }
        let driver = self.driver();
        if let Err(e) = driver.ensure_image(image).await {
            let _ = self
                .report_local_sandbox("error", image, None, Some(&e.to_string()))
                .await;
            return Err(Error::Sandbox(e.to_string()));
        }
        let handle = match driver.start(image).await {
            Ok(h) => h,
            Err(e) => {
                let _ = self
                    .report_local_sandbox("error", image, None, Some(&e.to_string()))
                    .await;
                return Err(Error::Sandbox(e.to_string()));
            }
        };
        let _ = self
            .report_local_sandbox("running", image, Some(&handle.id), None)
            .await;
        started.insert(image.to_string(), handle.clone());
        Ok(handle)
    }

    /// Lazily start the local container for `image` (if needed), run `command`
    /// in it, and report `error` on a driver failure.
    pub async fn exec_local(&self, image: &str, command: &str) -> Result<ExecResult, Error> {
        let handle = self.start_local(image).await?;
        let driver = self.driver();
        match driver.exec(&handle, command).await {
            Ok(result) => Ok(result),
            Err(e) => {
                let _ = self
                    .report_local_sandbox("error", image, Some(&handle.id), Some(&e.to_string()))
                    .await;
                Err(Error::Sandbox(e.to_string()))
            }
        }
    }

    /// Stop every local container this session started, reporting `stopped`
    /// (or `error`) for each. Called by [`Session::close`](crate::Session::close).
    pub async fn stop_all_local(&self) {
        let entries: Vec<(String, SandboxHandle)> = {
            let mut started = self.inner.started.lock().await;
            started.drain().collect()
        };
        let driver = self.driver();
        for (image, handle) in entries {
            match driver.stop(&handle).await {
                Ok(()) => {
                    let _ = self
                        .report_local_sandbox("stopped", &image, Some(&handle.id), None)
                        .await;
                }
                Err(e) => {
                    let _ = self
                        .report_local_sandbox(
                            "error",
                            &image,
                            Some(&handle.id),
                            Some(&e.to_string()),
                        )
                        .await;
                }
            }
        }
    }
}

impl std::fmt::Debug for SandboxSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxSession")
            .field("connected", &self.inner.rpc.get().is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Builder types + tool constructors
// ---------------------------------------------------------------------------

/// Where a shell tool's commands run.
pub enum SandboxTarget {
    /// The harness's own local container engine (`docker`/`container`).
    Local {
        /// The image to start the local container from.
        image: String,
    },
    /// The remote sandbox the server started for this session.
    Remote,
}

/// For a [`SandboxTarget::Remote`] tool, how the result is handled. Ignored for
/// [`SandboxTarget::Local`] tools (which always dispatch client-side).
pub enum RemoteMode {
    /// **Server-dispatched.** The tool is declared in the session-open
    /// `sandbox_tools` list; the server runs it and continues the turn itself.
    /// The constructor returns a [`SandboxTool::Def`], never a client-dispatched
    /// tool — so it can never be registered as an ordinary [`Tool`].
    Auto,
    /// **Client-dispatched.** The handler fetches raw output via
    /// `session.execRemoteSandbox` and the given closure builds the
    /// `tool_result` content from the [`ExecResult`].
    Manual(Box<dyn Fn(ExecResult) -> Value + Send + Sync>),
}

impl RemoteMode {
    /// Build a [`RemoteMode::Manual`] from a result-transform closure.
    pub fn manual<F>(f: F) -> Self
    where
        F: Fn(ExecResult) -> Value + Send + Sync + 'static,
    {
        RemoteMode::Manual(Box::new(f))
    }
}

/// An **Auto**-mode sandbox tool declaration destined for the session-open
/// `sandbox_tools` list. It carries no handler — the server dispatches it — and
/// the type is distinct from [`Tool`] precisely so it can never be handed to
/// [`Harness::with_tool`](crate::Harness::with_tool) and silently never fire.
#[derive(Clone, Debug)]
pub struct SandboxToolDef {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: Value,
}

impl SandboxToolDef {
    /// The `{name, description, input_schema}` sent in the `sandbox_tools` array.
    pub(crate) fn declaration(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        })
    }
}

/// The result of a sandbox tool constructor: either a client-dispatched
/// [`Tool`] (local, or remote-manual) or an [`SandboxToolDef`] (remote-auto).
/// Register it with [`Harness::with_sandbox_tool`](crate::Harness::with_sandbox_tool),
/// which routes each variant to the correct place; the type distinction is what
/// stops an Auto declaration being mistaken for a callable tool.
pub enum SandboxTool {
    /// A client-dispatched tool (local or remote-manual).
    Tool(Tool),
    /// A server-dispatched Auto declaration.
    Def(SandboxToolDef),
}

impl SandboxTool {
    /// Take the client-dispatched [`Tool`], if this is one.
    pub fn into_tool(self) -> Option<Tool> {
        match self {
            SandboxTool::Tool(t) => Some(t),
            SandboxTool::Def(_) => None,
        }
    }

    /// Take the Auto [`SandboxToolDef`], if this is one.
    pub fn into_def(self) -> Option<SandboxToolDef> {
        match self {
            SandboxTool::Def(d) => Some(d),
            SandboxTool::Tool(_) => None,
        }
    }
}

impl std::fmt::Debug for SandboxTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxTool::Tool(t) => f.debug_tuple("SandboxTool::Tool").field(t).finish(),
            SandboxTool::Def(d) => f.debug_tuple("SandboxTool::Def").field(d).finish(),
        }
    }
}

/// Which sandbox a client-dispatched handler runs against. Captured (cheaply
/// clonable) by the handler closure.
#[derive(Clone)]
enum TargetKind {
    Local(String),
    Remote,
}

/// Turn an [`ExecResult`] into default tool-result content: a JSON **string**
/// of `{stdout, stderr, exit_code}` (a plain-string content block, consistent
/// with the TS/Python SDKs — the model reads the serialized result).
fn exec_result_content(result: &ExecResult) -> Value {
    Value::String(serde_json::to_string(result).unwrap_or_default())
}

/// Declare a tool that runs an **arbitrary** shell command (fixed name
/// `run_shell_command`, one required `command` string) in `target`. For
/// `Remote` + [`RemoteMode::Auto`] this returns a [`SandboxTool::Def`]; for all
/// other combinations a client-dispatched [`SandboxTool::Tool`].
///
/// `run_shell_command` is, by design, unconstrained: the **image** (for a local
/// target) or the server's sandbox is the entire security boundary. Use
/// [`run_shell_named`] when the agent should only run one specific command.
///
/// `session` must be a handle from
/// [`Harness::sandbox_session`](crate::Harness::sandbox_session); see the
/// [module docs](self) for the required ordering.
pub fn run_shell_command(
    session: &SandboxSession,
    target: SandboxTarget,
    remote_mode: RemoteMode,
) -> SandboxTool {
    let input_schema = json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to run inside the sandbox."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    });
    build_tool(
        session,
        "run_shell_command".to_string(),
        "Run an arbitrary shell command inside the configured sandbox and return its stdout, \
         stderr, and exit code."
            .to_string(),
        input_schema,
        CommandSource::FullShell,
        target,
        remote_mode,
    )
}

/// Declare a **named** shell tool whose input schema is derived from the
/// `{param}` placeholders in `command_template`. Each placeholder becomes a
/// required string input; at call time every model-supplied value is
/// **shell-escaped** ([`shell_quote`]) before substitution — the command-injection
/// boundary — so metacharacters in a value can never break out of their argument.
///
/// Returns a [`SandboxTool::Def`] for `Remote` + [`RemoteMode::Auto`], else a
/// client-dispatched [`SandboxTool::Tool`]. Panics if `command_template` is
/// malformed (unterminated or empty `{}` placeholder).
pub fn run_shell_named(
    session: &SandboxSession,
    name: impl Into<String>,
    description: impl Into<String>,
    command_template: &str,
    target: SandboxTarget,
    remote_mode: RemoteMode,
) -> SandboxTool {
    let params = parse_params(command_template);
    let mut props = serde_json::Map::new();
    for p in &params {
        props.insert(
            p.clone(),
            json!({
                "type": "string",
                "description": format!("Value substituted for {{{p}}} (shell-escaped before use).")
            }),
        );
    }
    let input_schema = json!({
        "type": "object",
        "properties": props,
        "required": params,
        "additionalProperties": false
    });
    build_tool(
        session,
        name.into(),
        description.into(),
        input_schema,
        CommandSource::Template(command_template.to_string()),
        target,
        remote_mode,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_tool(
    session: &SandboxSession,
    name: String,
    description: String,
    input_schema: Value,
    source: CommandSource,
    target: SandboxTarget,
    remote_mode: RemoteMode,
) -> SandboxTool {
    // Auto-mode remote tools are declarations only — no client handler.
    if matches!(target, SandboxTarget::Remote) && matches!(remote_mode, RemoteMode::Auto) {
        return SandboxTool::Def(SandboxToolDef {
            name,
            description,
            input_schema,
        });
    }

    let session = session.clone();
    let target_kind = match &target {
        SandboxTarget::Local { image } => TargetKind::Local(image.clone()),
        SandboxTarget::Remote => TargetKind::Remote,
    };
    let source = Arc::new(source);
    let remote_mode = Arc::new(remote_mode);

    let handler: ToolHandler = Box::new(move |input| {
        let session = session.clone();
        let target_kind = target_kind.clone();
        let source = source.clone();
        let remote_mode = remote_mode.clone();
        Box::pin(async move {
            let command = source.build(&input)?;
            match &target_kind {
                TargetKind::Local(image) => {
                    let result = session
                        .exec_local(image, &command)
                        .await
                        .map_err(|e| Box::new(e) as BoxError)?;
                    Ok(exec_result_content(&result))
                }
                TargetKind::Remote => {
                    let result = session
                        .exec_remote_sandbox(&command)
                        .await
                        .map_err(|e| Box::new(e) as BoxError)?;
                    match &*remote_mode {
                        RemoteMode::Manual(f) => Ok(f(result)),
                        // Auto is handled above (declaration-only); a Local+Auto
                        // tool takes the Local branch, so this is unreachable.
                        RemoteMode::Auto => Ok(exec_result_content(&result)),
                    }
                }
            }
        })
    });

    SandboxTool::Tool(Tool::from_handler(name, description, input_schema, handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A fake local [`SandboxDriver`] that records every call — and the exact
    /// command string handed to `exec` — into a shared timeline, returning
    /// scripted results. No real `docker`/`container` binary is ever touched.
    struct FakeDriver {
        timeline: Arc<Mutex<Vec<String>>>,
        exec_stdout: String,
    }

    impl SandboxDriver for FakeDriver {
        fn ensure_image<'a>(
            &'a self,
            _image: &'a str,
        ) -> SandboxFuture<'a, Result<(), SandboxError>> {
            self.timeline.lock().unwrap().push("ensure_image".into());
            Box::pin(async { Ok(()) })
        }
        fn start<'a>(
            &'a self,
            image: &'a str,
        ) -> SandboxFuture<'a, Result<SandboxHandle, SandboxError>> {
            self.timeline.lock().unwrap().push("start".into());
            let image = image.to_string();
            Box::pin(async move {
                Ok(SandboxHandle {
                    id: "cid-1".into(),
                    image,
                })
            })
        }
        fn exec<'a>(
            &'a self,
            _handle: &'a SandboxHandle,
            command: &'a str,
        ) -> SandboxFuture<'a, Result<ExecResult, SandboxError>> {
            self.timeline
                .lock()
                .unwrap()
                .push(format!("exec:{command}"));
            let stdout = self.exec_stdout.clone();
            Box::pin(async move {
                Ok(ExecResult {
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                })
            })
        }
        fn stop<'a>(
            &'a self,
            _handle: &'a SandboxHandle,
        ) -> SandboxFuture<'a, Result<(), SandboxError>> {
            self.timeline.lock().unwrap().push("stop".into());
            Box::pin(async { Ok(()) })
        }
    }

    /// A fake [`SandboxRpc`] recording the outbound `reportLocalSandbox` /
    /// `execRemoteSandbox` calls the same way the real transport would — the
    /// client-side analogue of the server tests' recording driver.
    struct FakeRpc {
        timeline: Arc<Mutex<Vec<String>>>,
        reports: Mutex<Vec<LocalSandboxReport>>,
        execs: Mutex<Vec<String>>,
        remote_stdout: String,
    }

    impl FakeRpc {
        fn new(timeline: Arc<Mutex<Vec<String>>>) -> Arc<Self> {
            Arc::new(Self {
                timeline,
                reports: Mutex::new(Vec::new()),
                execs: Mutex::new(Vec::new()),
                remote_stdout: "remote-out".into(),
            })
        }
    }

    impl SandboxRpc for FakeRpc {
        fn exec_remote_sandbox(
            &self,
            command: String,
        ) -> SandboxFuture<'_, Result<ExecResult, Error>> {
            self.execs.lock().unwrap().push(command);
            let stdout = self.remote_stdout.clone();
            Box::pin(async move {
                Ok(ExecResult {
                    stdout,
                    stderr: String::new(),
                    exit_code: 0,
                })
            })
        }
        fn report_local_sandbox(
            &self,
            report: LocalSandboxReport,
        ) -> SandboxFuture<'_, Result<(), Error>> {
            self.timeline
                .lock()
                .unwrap()
                .push(format!("report:{}", report.state));
            self.reports.lock().unwrap().push(report);
            Box::pin(async { Ok(()) })
        }
    }

    async fn call(tool: &Tool, input: Value) -> Value {
        tool.call(input).await.expect("sandbox tool call ok")
    }

    // -----------------------------------------------------------------------
    // Command-injection resistance — the single most important test.
    //
    // The template `echo {name}` interpolated with each classic
    // shell-metacharacter payload must yield a final command string in which the
    // WHOLE argument is one literal string argument to `echo`. We assert the
    // EXACT command string the driver receives. The payload list is IDENTICAL
    // across the three SDKs; the expected escaped strings use Rust/TS-style
    // `'\''` quoting (Python's `shlex.quote` uses `'"'"'`, asserted in its own
    // test — same semantics, different bytes).
    // -----------------------------------------------------------------------

    /// `(payload, expected final command)` — identical payloads to the TS/Python
    /// injection tests; see `client-typescript/src/sandbox.test.ts` and
    /// `client-python/tests/test_sandbox_injection.py`.
    fn injection_cases() -> Vec<(&'static str, &'static str)> {
        vec![
            ("a'; rm -rf / #", "echo 'a'\\''; rm -rf / #'"),
            ("`whoami`", "echo '`whoami`'"),
            ("$(whoami)", "echo '$(whoami)'"),
            ("x && y", "echo 'x && y'"),
            ("he said \"hi\"", "echo 'he said \"hi\"'"),
        ]
    }

    #[tokio::test]
    async fn run_shell_named_shell_escapes_every_injection_payload() {
        for (payload, expected) in injection_cases() {
            let timeline = Arc::new(Mutex::new(Vec::new()));
            let session = SandboxSession::new();
            session.bind(FakeRpc::new(timeline.clone()));
            session.set_local_driver(Arc::new(FakeDriver {
                timeline: timeline.clone(),
                exec_stdout: String::new(),
            }));

            let tool = run_shell_named(
                &session,
                "echo_it",
                "echo the name",
                "echo {name}",
                SandboxTarget::Local {
                    image: "alpine".into(),
                },
                RemoteMode::Auto,
            )
            .into_tool()
            .expect("local target yields a client-dispatched tool");

            call(&tool, json!({ "name": payload })).await;

            // The exact command string that reached the container's `sh -c`.
            let exec = timeline
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.starts_with("exec:"))
                .cloned()
                .expect("exec recorded");
            assert_eq!(exec, format!("exec:{expected}"), "payload {payload:?}");
        }
    }

    #[test]
    fn shell_quote_wraps_the_whole_value_as_one_literal_argument() {
        // Spot-check the primitive directly, independent of the tool pipeline.
        assert_eq!(shell_quote("a'; rm -rf / #"), "'a'\\''; rm -rf / #'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    // -----------------------------------------------------------------------
    // Local sandbox lifecycle reporting (running-before-exec, stopped-on-close).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn local_tool_reports_running_before_exec_and_stopped_on_close() {
        let timeline = Arc::new(Mutex::new(Vec::new()));
        let session = SandboxSession::new();
        let rpc = FakeRpc::new(timeline.clone());
        session.bind(rpc.clone());
        session.set_local_driver(Arc::new(FakeDriver {
            timeline: timeline.clone(),
            exec_stdout: "hi".into(),
        }));

        let tool = run_shell_command(
            &session,
            SandboxTarget::Local {
                image: "alpine".into(),
            },
            RemoteMode::Auto,
        )
        .into_tool()
        .unwrap();

        call(&tool, json!({ "command": "echo hi" })).await;
        // `close()` calls `stop_all_local()`; drive that directly here.
        session.stop_all_local().await;

        let seen = timeline.lock().unwrap().clone();
        // Ordering: the container is reported `running` before the first exec.
        let running = seen
            .iter()
            .position(|e| e == "report:running")
            .expect("running reported");
        let exec = seen
            .iter()
            .position(|e| e.starts_with("exec:"))
            .expect("exec ran");
        let stopped = seen
            .iter()
            .position(|e| e == "report:stopped")
            .expect("stopped reported");
        assert!(running < exec, "running must precede first exec: {seen:?}");
        assert!(
            exec < stopped,
            "stopped must follow exec (at close): {seen:?}"
        );

        // Verified via the recorded outbound RPC calls: running carries the real
        // container id, then stopped.
        let reports = rpc.reports.lock().unwrap();
        assert_eq!(reports[0].state, "running");
        assert_eq!(reports[0].container_id.as_deref(), Some("cid-1"));
        assert_eq!(reports.last().unwrap().state, "stopped");
    }

    // -----------------------------------------------------------------------
    // Auto vs. manual dispatch type split + manual result construction.
    // -----------------------------------------------------------------------

    #[test]
    fn remote_auto_returns_a_def_never_a_client_tool() {
        let session = SandboxSession::new();
        let tool = run_shell_command(&session, SandboxTarget::Remote, RemoteMode::Auto);
        assert!(matches!(tool, SandboxTool::Def(_)));
        // An Auto tool can never be mistaken for a client-dispatched Tool.
        assert!(
            run_shell_command(&session, SandboxTarget::Remote, RemoteMode::Auto)
                .into_tool()
                .is_none()
        );
    }

    #[tokio::test]
    async fn remote_manual_tool_builds_result_from_transform() {
        let timeline = Arc::new(Mutex::new(Vec::new()));
        let session = SandboxSession::new();
        let rpc = FakeRpc::new(timeline);
        session.bind(rpc.clone());

        let tool = run_shell_command(
            &session,
            SandboxTarget::Remote,
            RemoteMode::manual(|r| json!({ "custom": r.stdout })),
        )
        .into_tool()
        .expect("remote+manual yields a client-dispatched tool");

        let result = call(&tool, json!({ "command": "uname -a" })).await;
        // The manual closure shaped the tool_result from the raw ExecResult.
        assert_eq!(result, json!({ "custom": "remote-out" }));
        // The handler fetched raw output over `session.execRemoteSandbox`.
        assert_eq!(*rpc.execs.lock().unwrap(), vec!["uname -a".to_string()]);
    }
}
