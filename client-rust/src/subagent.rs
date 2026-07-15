//! Builtin **subagent tools** — let an agent hand a prompt off to an external
//! CLI coding agent (`claude`, `codex`, `opencode`, …) that runs in the
//! background, then poll for its result.
//!
//! This module mirrors [`crate::sandbox`] on the client side, one axis further:
//! a subagent has a **launch location** (who owns the subprocess — the harness,
//! or `baesrv`) crossed with a **sandbox target** (where it runs). Only three
//! combinations are ever valid, and the [`SubagentLaunch`] type makes the
//! fourth (`baesrv` running a subagent unsandboxed on its own host)
//! unconstructible *by shape*:
//!
//! | Launch | Target | Who spawns |
//! |--------|--------|------------|
//! | [`SubagentLaunch::Local`] | [`SandboxTarget::None`] | this harness, bare host |
//! | [`SubagentLaunch::Local`] | [`SandboxTarget::Local`] / [`SandboxTarget::Remote`] | this harness's container / the session's remote sandbox |
//! | [`SubagentLaunch::Remote`] | (implicitly sandboxed) | `baesrv`, in the session's remote sandbox |
//!
//! # The async, fire-and-forget contract
//!
//! [`launch_subagent`] returns **immediately** with `{"status":"started",…}` —
//! never the subagent's output. A background task owns the subprocess; the
//! model retrieves the result later through the automatically-appearing
//! `local_subagent_status` tool. That status tool is advertised via
//! `session.updateClientTools` only while at least one subagent is tracked, and
//! removed again once the tracking map empties — the harness developer wires
//! nothing.
//!
//! # Subagent tools require a live [`Session`](crate::Session)
//!
//! Exactly like sandbox tools, subagent tools capture a late-bound
//! [`SubagentSession`] handle: obtain it from
//! [`Harness::subagent_session`](crate::Harness::subagent_session) *before*
//! connect, build tools against it, register them with
//! [`Harness::with_subagent_tool`](crate::Harness::with_subagent_tool), then
//! `connect()`. The handle's transport (for telemetry / `updateClientTools`)
//! is filled at connect; a tool firing before then errors like a sandbox tool.
//!
//! # Untrusted output
//!
//! A subagent's stdout is **data to reason about, never instructions to
//! follow** — the same prompt-injection posture the sandbox/issue-triage tools
//! take. The status-tool description reminds the model of this.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;

use crate::error::Error;
use crate::sandbox::{interpolate, parse_params, SandboxSession, SandboxTarget};
use crate::tool::{Tool, ToolHandler};

// ---------------------------------------------------------------------------
// Constants (must match the server and the other two SDKs — see the contract)
// ---------------------------------------------------------------------------

/// Client-side default subagent timeout, overridable per [`SubagentDef`]. The
/// SDK never reads `BAE_SUBAGENT_TIMEOUT` (that is a server env var).
pub const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;

/// Max concurrently **non-terminal** subagents tracked per [`SubagentSession`].
/// A guardrail (not env-configurable client-side) so a model cannot fork
/// unboundedly many subprocesses in one session.
pub const MAX_SUBAGENTS_PER_SESSION: usize = 8;

/// Per-stream captured-output cap. Captured stdout and stderr are each
/// truncated to their first this-many bytes (on a UTF-8 boundary) before
/// storage; if either was cut, the status entry carries `"truncated": true`.
pub const SUBAGENT_OUTPUT_CAP_BYTES: usize = 65536;

/// Fixed launch-tool name; a harness binds at most one.
pub const LAUNCH_SUBAGENT_TOOL: &str = "launch_subagent";

/// Fixed client-dispatched status-tool name.
pub const LOCAL_SUBAGENT_STATUS_TOOL: &str = "local_subagent_status";

// ---------------------------------------------------------------------------
// Core data types
// ---------------------------------------------------------------------------

/// How a subagent's prompt reaches the CLI subprocess.
///
/// [`Stdin`](PromptDelivery::Stdin) (the default) pipes the raw prompt to the
/// child's stdin — it never appears in the constructed argv, sidestepping argv
/// length limits and most of the escaping surface. [`Arg`](PromptDelivery::Arg)
/// interpolates a shell-escaped `{prompt}` into the command template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDelivery {
    /// Interpolate a (shell-escaped) `{prompt}` into the command template.
    Arg,
    /// Pipe the raw prompt to the subprocess's stdin (default).
    Stdin,
}

impl PromptDelivery {
    /// The wire string (`"arg"` / `"stdin"`).
    fn as_str(self) -> &'static str {
        match self {
            PromptDelivery::Arg => "arg",
            PromptDelivery::Stdin => "stdin",
        }
    }
}

/// One configured CLI subagent: the `harness` value the model selects, the
/// shell `command_template` to run (with `{model}` / `{prompt}` placeholders,
/// §8 of the contract), how the prompt is delivered, and a per-subagent
/// timeout.
#[derive(Clone, Debug)]
pub struct SubagentDef {
    /// The enum value the LLM selects, e.g. `"claude"`.
    pub harness: String,
    /// e.g. `"claude --model {model} --print"`. `{model}` is optional;
    /// `{prompt}` is required iff `prompt_via == Arg` and forbidden otherwise.
    pub command_template: String,
    /// How the prompt is handed to the subprocess (default [`PromptDelivery::Stdin`]).
    pub prompt_via: PromptDelivery,
    /// Wall-clock timeout; on expiry the process is killed and the subagent
    /// becomes [`SubagentStatus::TimedOut`] (default 600s).
    pub timeout: Duration,
}

impl SubagentDef {
    /// A def with the defaults: [`PromptDelivery::Stdin`] and a
    /// [`DEFAULT_SUBAGENT_TIMEOUT_SECS`] timeout.
    pub fn new(harness: impl Into<String>, command_template: impl Into<String>) -> Self {
        Self {
            harness: harness.into(),
            command_template: command_template.into(),
            prompt_via: PromptDelivery::Stdin,
            timeout: Duration::from_secs(DEFAULT_SUBAGENT_TIMEOUT_SECS),
        }
    }

    /// Builder: set the prompt delivery mode.
    pub fn with_prompt_via(mut self, prompt_via: PromptDelivery) -> Self {
        self.prompt_via = prompt_via;
        self
    }

    /// Builder: set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Where a launched subagent runs — the one and only place the
/// "no remote-unsandboxed" invariant is enforced, **by construction**.
///
/// [`Local`](SubagentLaunch::Local) carries any [`SandboxTarget`] (including
/// [`SandboxTarget::None`], the harness's own risk to accept).
/// [`Remote`](SubagentLaunch::Remote) carries **only** an image — there is no
/// variant, flag, or field that expresses a bare-host remote launch, and none
/// may be added.
pub enum SubagentLaunch {
    /// The client harness owns the subprocess; it runs per the [`SandboxTarget`].
    Local(SandboxTarget),
    /// `baesrv` owns the subprocess; it runs inside the session's already-started
    /// remote sandbox (`image`). The only remote shape — always sandboxed.
    Remote {
        /// The image the server-managed sandbox must be running.
        image: String,
    },
}

/// The status of a tracked subagent. `TimedOut` is distinct here even though the
/// lifecycle *event* folds it into `failed{reason:"timeout"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentStatus {
    /// Still running.
    Running,
    /// Exited zero.
    Completed,
    /// Exited non-zero, or failed to spawn.
    Failed,
    /// Killed after exceeding its timeout.
    TimedOut,
    /// Killed by an explicit cancel or session-close teardown.
    Cancelled,
}

impl SubagentStatus {
    /// The status-tool wire string.
    fn wire(self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Failed => "failed",
            SubagentStatus::TimedOut => "timed_out",
            SubagentStatus::Cancelled => "cancelled",
        }
    }

    /// The telemetry (`reportLocalSubagent`) `state` string — `TimedOut` folds
    /// into `"failed"` (its `reason` is `"timeout"`).
    fn report_state(self) -> &'static str {
        match self {
            SubagentStatus::Running => "running",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Failed | SubagentStatus::TimedOut => "failed",
            SubagentStatus::Cancelled => "cancelled",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, SubagentStatus::Running)
    }
}

// ---------------------------------------------------------------------------
// The Tool-vs-Def split (verbatim the sandbox rationale)
// ---------------------------------------------------------------------------

/// A **remote**-launch subagent declaration destined for the session-open
/// `subagent_tools` list. It carries no handler — the server dispatches it — and
/// its type is distinct from [`Tool`] precisely so a `Remote` launch can never
/// be handed to [`Harness::with_tool`](crate::Harness::with_tool) and silently
/// never fire.
#[derive(Clone, Debug)]
pub struct SubagentToolDef {
    /// Tool name (`launch_subagent`).
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: Value,
    /// The image the server's sandbox must be running.
    pub image: String,
    /// The `{harness, command_template, prompt_via, timeout_secs}` config array.
    pub subagents: Vec<Value>,
}

impl SubagentToolDef {
    /// The full `subagent_tools` array element (§3.4 of the contract).
    pub(crate) fn declaration(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
            "image": self.image,
            "subagents": self.subagents,
        })
    }
}

/// The result of [`launch_subagent`]: either a client-dispatched [`Tool`]
/// (a `Local` launch) or a [`SubagentToolDef`] (a `Remote` launch). Register it
/// with [`Harness::with_subagent_tool`](crate::Harness::with_subagent_tool),
/// which routes each variant; the type distinction stops a `Remote` declaration
/// being mistaken for a callable tool.
pub enum SubagentTool {
    /// A client-dispatched launch tool (a `Local` launch).
    Tool(Tool),
    /// A server-dispatched `Remote` launch declaration.
    Def(SubagentToolDef),
}

impl SubagentTool {
    /// Take the client-dispatched [`Tool`], if this is one.
    pub fn into_tool(self) -> Option<Tool> {
        match self {
            SubagentTool::Tool(t) => Some(t),
            SubagentTool::Def(_) => None,
        }
    }

    /// Take the [`SubagentToolDef`], if this is one.
    pub fn into_def(self) -> Option<SubagentToolDef> {
        match self {
            SubagentTool::Def(d) => Some(d),
            SubagentTool::Tool(_) => None,
        }
    }
}

impl std::fmt::Debug for SubagentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubagentTool::Tool(t) => f.debug_tuple("SubagentTool::Tool").field(t).finish(),
            SubagentTool::Def(d) => f.debug_tuple("SubagentTool::Def").field(d).finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// Runner seam + RPC seam (fake-able offline, exactly like the sandbox seams)
// ---------------------------------------------------------------------------

/// A boxed, `Send` future — the object-safe return shape for the runner and RPC
/// seams so they can live behind `Arc<dyn …>` and be captured by a `'static`
/// handler.
pub type SubagentFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The captured result of one subagent subprocess.
#[derive(Clone, Debug)]
pub struct RunnerOutput {
    /// Captured standard output (untruncated; the caller applies the cap).
    pub stdout: String,
    /// Captured standard error (untruncated; the caller applies the cap).
    pub stderr: String,
    /// Process exit code (`-1` if killed by a signal).
    pub exit_code: i32,
}

/// The injectable subprocess seam. The production [`ProcessRunner`] shells out
/// via `tokio::process`; a test injects a fake so no real `claude`/`codex`
/// binary is ever required.
pub trait SubagentRunner: Send + Sync {
    /// Run `program` with `args`, optionally writing `stdin` to the child before
    /// waiting, and capture `{stdout, stderr, exit_code}`. A spawn/io failure is
    /// an `Err` (surfaced as `failed{reason:"spawn_failed"}`).
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> SubagentFuture<'a, std::io::Result<RunnerOutput>>;
}

/// Drain a pipe to EOF while retaining only the cap plus one marker byte. The
/// extra byte lets the existing truncation layer report `truncated:true`.
async fn read_capped<R: AsyncRead + Unpin>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(SUBAGENT_OUTPUT_CAP_BYTES + 1);
    let mut chunk = [0_u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let remaining = (SUBAGENT_OUTPUT_CAP_BYTES + 1).saturating_sub(retained.len());
        retained.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    Ok(retained)
}

/// The production runner: `tokio::process` with `kill_on_drop(true)`, so a
/// timeout (future dropped) or a cancel (task aborted) reaps the child. Both
/// output pipes are continuously drained while only cap+1 bytes are retained.
#[derive(Clone, Debug, Default)]
pub struct ProcessRunner;

impl SubagentRunner for ProcessRunner {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        stdin: Option<&'a [u8]>,
    ) -> SubagentFuture<'a, std::io::Result<RunnerOutput>> {
        let stdin_owned: Option<Vec<u8>> = stdin.map(|b| b.to_vec());
        Box::pin(async move {
            let mut cmd = TokioCommand::new(program);
            cmd.args(args)
                .kill_on_drop(true)
                .stdin(if stdin_owned.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(bytes) = stdin_owned {
                if let Some(mut pipe) = child.stdin.take() {
                    // Drain stdin from a detached task so writing a large prompt
                    // cannot deadlock against a child filling its stdout pipe.
                    tokio::spawn(async move {
                        let _ = pipe.write_all(&bytes).await;
                        let _ = pipe.shutdown().await;
                    });
                }
            }
            let stdout = child.stdout.take().expect("stdout configured as piped");
            let stderr = child.stderr.take().expect("stderr configured as piped");
            let stdout_task = tokio::spawn(read_capped(stdout));
            let stderr_task = tokio::spawn(read_capped(stderr));
            let status = child.wait().await?;
            let stdout = stdout_task.await.map_err(std::io::Error::other)??;
            let stderr = stderr_task.await.map_err(std::io::Error::other)??;
            Ok(RunnerOutput {
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                exit_code: status.code().unwrap_or(-1),
            })
        })
    }
}

/// Wire params for `session.reportLocalSubagent`.
#[derive(Clone, Debug, Serialize)]
pub struct LocalSubagentReport {
    /// One of `"start"`, `"running"`, `"completed"`, `"failed"`, `"cancelled"`.
    pub state: String,
    /// The subagent's id.
    pub subagent_id: String,
    /// The selected harness.
    pub harness: String,
    /// The model handed to the CLI.
    pub model: String,
    /// Human-readable detail (e.g. a spawn error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A `failed`/`cancelled` reason from the contract's enums.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The process exit code, when meaningful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// The object-safe transport seam a [`SubagentSession`] calls into. Implemented
/// on the crate's HTTP transport; a test can supply its own recorder.
pub trait SubagentRpc: Send + Sync {
    /// `session.reportLocalSubagent` — mirror a local subagent lifecycle
    /// transition into the session's event log (pure telemetry).
    fn report_local_subagent(
        &self,
        report: LocalSubagentReport,
    ) -> SubagentFuture<'_, Result<(), Error>>;
    /// `session.updateClientTools` — full-replace this client's advertised tool
    /// list (used to make `local_subagent_status` appear/disappear).
    fn update_client_tools(&self, tools: Vec<Value>) -> SubagentFuture<'_, Result<(), Error>>;
    /// `session.cancelSubagent` — cancel a **remote** (server-tracked) subagent.
    /// Local cancellation never calls this.
    fn cancel_subagent(&self, subagent_id: String) -> SubagentFuture<'_, Result<(), Error>>;
}

// ---------------------------------------------------------------------------
// Tracked-task state
// ---------------------------------------------------------------------------

/// One tracked local subagent.
struct SubagentTask {
    /// Monotonic insertion order, so the status tool can list in launch order.
    seq: u64,
    harness: String,
    model: String,
    status: SubagentStatus,
    exit_code: Option<i32>,
    /// Truncated captured stdout (terminal states only).
    stdout: Option<String>,
    /// Truncated captured stderr (terminal states only).
    stderr: Option<String>,
    truncated: bool,
    reason: Option<String>,
    detail: Option<String>,
    /// The watcher task; `abort()` reaps the child via `kill_on_drop`.
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl SubagentTask {
    fn entry(&self, id: &str) -> StatusEntry {
        StatusEntry {
            subagent_id: id.to_string(),
            harness: self.harness.clone(),
            model: self.model.clone(),
            status: self.status.wire(),
            exit_code: self.exit_code,
            stdout: self.stdout.clone(),
            stderr: self.stderr.clone(),
            truncated: self.truncated,
            reason: self.reason.clone(),
            detail: self.detail.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Serialized wire shapes (structs, so serde emits the pinned key order — the
// crate does not enable serde_json's `preserve_order`, and json! would sort)
// ---------------------------------------------------------------------------

/// The immediate `launch_subagent` success result.
#[derive(Serialize)]
struct StartedResult<'a> {
    subagent_id: &'a str,
    harness: &'a str,
    model: &'a str,
    status: &'a str,
}

/// One status-tool entry (pinned key order).
#[derive(Serialize)]
struct StatusEntry {
    subagent_id: String,
    harness: String,
    model: String,
    status: &'static str,
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    truncated: bool,
    reason: Option<String>,
    detail: Option<String>,
}

/// The status-tool result wrapper.
#[derive(Serialize)]
struct StatusResult {
    subagents: Vec<StatusEntry>,
}

/// An in-band error-shaped result (`is_error: true` server-side).
#[derive(Serialize)]
struct ErrorResult<'a> {
    error: &'a str,
}

/// Serialize `value` into the plain-string content value client tools deliver.
fn json_string(value: &impl Serialize) -> Value {
    Value::String(serde_json::to_string(value).unwrap_or_default())
}

fn error_result(msg: &str) -> Value {
    json_string(&ErrorResult { error: msg })
}

// ---------------------------------------------------------------------------
// SubagentSession — the late-bound handle subagent tools capture
// ---------------------------------------------------------------------------

struct SubagentInner {
    /// Filled by `connect()`/`join()`; empty beforehand.
    rpc: OnceLock<Arc<dyn SubagentRpc>>,
    /// The injectable subprocess seam (default [`ProcessRunner`]).
    runner: StdMutex<Arc<dyn SubagentRunner>>,
    /// Shared sandbox handle: container start/reuse for `Local{image}` targets
    /// and `execRemoteSandbox` for `Remote` targets.
    sandbox: SandboxSession,
    /// Tracked local subagents, keyed by `subagent_id`.
    tasks: StdMutex<HashMap<String, SubagentTask>>,
    /// Serializes task-set transitions with their full-replace tool update so
    /// concurrent launch/eviction/close operations cannot commit stale lists.
    transition: tokio::sync::Mutex<()>,
    /// Monotonic launch-order counter.
    seq: AtomicU64,
    /// The harness's declared client-tool list (no status tool), captured at
    /// connect so `updateClientTools` can full-replace it plus/minus the status
    /// tool.
    base_client_tools: StdMutex<Vec<Value>>,
    /// Whether a `Local` launch tool was built against this session (so the
    /// harness registers the dynamic status tool at connect).
    has_local: AtomicBool,
}

/// A cheap, cloneable handle to a live [`Session`](crate::Session)'s subagent
/// capability. Obtain one from
/// [`Harness::subagent_session`](crate::Harness::subagent_session) (before
/// connect) or [`Session::subagent_session`](crate::Session::subagent_session)
/// (after), pass it to [`launch_subagent`], and the resulting tool captures it.
/// Its transport is late-bound — see the [module docs](self).
#[derive(Clone)]
pub struct SubagentSession {
    inner: Arc<SubagentInner>,
}

impl SubagentSession {
    /// A fresh handle sharing `sandbox`'s container/remote machinery, with an
    /// unbound transport and the default [`ProcessRunner`].
    pub(crate) fn new(sandbox: SandboxSession) -> Self {
        Self {
            inner: Arc::new(SubagentInner {
                rpc: OnceLock::new(),
                runner: StdMutex::new(Arc::new(ProcessRunner)),
                sandbox,
                tasks: StdMutex::new(HashMap::new()),
                transition: tokio::sync::Mutex::new(()),
                seq: AtomicU64::new(0),
                base_client_tools: StdMutex::new(Vec::new()),
                has_local: AtomicBool::new(false),
            }),
        }
    }

    /// Bind the transport once the session is connected (first bind wins).
    pub(crate) fn bind(&self, rpc: Arc<dyn SubagentRpc>) {
        let _ = self.inner.rpc.set(rpc);
    }

    /// Replace the subprocess runner (default [`ProcessRunner`]); use to inject a
    /// fake in tests, exactly like [`SandboxSession::set_local_driver`].
    pub fn set_runner(&self, runner: Arc<dyn SubagentRunner>) {
        *self.inner.runner.lock().unwrap() = runner;
    }

    /// Mark this session as backing a `Local` launch tool (called by
    /// [`launch_subagent`]); the harness reads it to register the dynamic status
    /// tool at connect.
    pub(crate) fn mark_local(&self) {
        self.inner.has_local.store(true, Ordering::SeqCst);
    }

    /// Whether a `Local` launch tool was built against this session.
    pub(crate) fn has_local(&self) -> bool {
        self.inner.has_local.load(Ordering::SeqCst)
    }

    /// Capture the harness's declared client-tool list (no status tool) at
    /// connect, so `updateClientTools` can full-replace it.
    pub(crate) fn set_base_client_tools(&self, tools: Vec<Value>) {
        *self.inner.base_client_tools.lock().unwrap() = tools;
    }

    fn runner(&self) -> Arc<dyn SubagentRunner> {
        self.inner.runner.lock().unwrap().clone()
    }

    fn rpc(&self) -> Result<Arc<dyn SubagentRpc>, Error> {
        self.inner.rpc.get().cloned().ok_or_else(|| {
            Error::Sandbox(
                "subagent tool used before the session was connected; build subagent tools from a \
                 Harness::subagent_session() handle and register them, then connect()"
                    .to_string(),
            )
        })
    }

    /// Best-effort telemetry mirror — an unbound/failed transport is ignored, so
    /// telemetry never fails a launch or a status call.
    async fn report(&self, report: LocalSubagentReport) {
        if let Ok(rpc) = self.rpc() {
            let _ = rpc.report_local_subagent(report).await;
        }
    }

    /// Best-effort `updateClientTools`: full-replace the client's tool list with
    /// the base list, plus the status tool iff `include_status`. A failure is
    /// swallowed (retried at the next transition); it never fails the caller.
    async fn sync_client_tools(&self, include_status: bool) {
        let mut tools = self.inner.base_client_tools.lock().unwrap().clone();
        if include_status {
            tools.push(status_tool_declaration());
        }
        if let Ok(rpc) = self.rpc() {
            let _ = rpc.update_client_tools(tools).await;
        }
    }

    /// The dynamic `local_subagent_status` tool, dispatched from this session's
    /// tracking map. The harness registers it for dispatch at connect; it is
    /// advertised to the provider only while a subagent is tracked.
    pub(crate) fn status_tool(&self) -> Tool {
        let session = self.clone();
        let handler: ToolHandler = Box::new(move |input| {
            let session = session.clone();
            Box::pin(async move {
                session.rpc()?;
                Ok(session.handle_status(&input).await)
            })
        });
        Tool::from_handler(
            LOCAL_SUBAGENT_STATUS_TOOL,
            STATUS_TOOL_DESCRIPTION,
            status_input_schema(),
            handler,
        )
    }

    /// The status-tool handler: read the map, evict any terminal entry included
    /// in this response (evict-on-report), and — if the eviction empties the map
    /// — fire the `updateClientTools` removal transition.
    async fn handle_status(&self, input: &Value) -> Value {
        let _transition = self.inner.transition.lock().await;
        let target = input
            .get("subagent_id")
            .and_then(Value::as_str)
            .map(str::to_string);

        let (payload, emptied): (Value, bool) = {
            let mut map = self.inner.tasks.lock().unwrap();
            match target {
                Some(id) => match map.get(&id) {
                    None => return error_result("unknown subagent_id"),
                    Some(task) => {
                        let entry = task.entry(&id);
                        let was_nonempty = !map.is_empty();
                        if task.status.is_terminal() {
                            map.remove(&id);
                        }
                        let emptied = was_nonempty && map.is_empty();
                        (
                            json_string(&StatusResult {
                                subagents: vec![entry],
                            }),
                            emptied,
                        )
                    }
                },
                None => {
                    let mut ordered: Vec<(u64, String, bool)> = map
                        .iter()
                        .map(|(id, t)| (t.seq, id.clone(), t.status.is_terminal()))
                        .collect();
                    ordered.sort_by_key(|(seq, _, _)| *seq);
                    let entries: Vec<StatusEntry> = ordered
                        .iter()
                        .map(|(_, id, _)| map.get(id).unwrap().entry(id))
                        .collect();
                    let was_nonempty = !map.is_empty();
                    for (_, id, terminal) in &ordered {
                        if *terminal {
                            map.remove(id);
                        }
                    }
                    let emptied = was_nonempty && map.is_empty();
                    (json_string(&StatusResult { subagents: entries }), emptied)
                }
            }
        };

        if emptied {
            self.sync_client_tools(false).await;
        }
        payload
    }

    /// Cancel one local subagent in-process (idempotent). Aborts the watcher
    /// (killing the child via `kill_on_drop`), marks it `Cancelled` with
    /// `reason:"explicit"`, and mirrors the transition via telemetry. The entry
    /// stays tracked so the model can observe the cancellation through the status
    /// tool. A terminal/unknown id is a silent no-op.
    pub async fn cancel_subagent(&self, subagent_id: &str) {
        let _transition = self.inner.transition.lock().await;
        let cancelled: Option<(String, String)> = {
            let mut map = self.inner.tasks.lock().unwrap();
            match map.get_mut(subagent_id) {
                Some(task) if task.status == SubagentStatus::Running => {
                    if let Some(handle) = task.handle.take() {
                        handle.abort();
                    }
                    task.status = SubagentStatus::Cancelled;
                    task.reason = Some("explicit".to_string());
                    Some((task.harness.clone(), task.model.clone()))
                }
                _ => None,
            }
        };
        if let Some((harness, model)) = cancelled {
            self.report(LocalSubagentReport {
                state: SubagentStatus::Cancelled.report_state().to_string(),
                subagent_id: subagent_id.to_string(),
                harness,
                model,
                detail: None,
                reason: Some("explicit".to_string()),
                exit_code: None,
            })
            .await;
        }
    }

    /// Session-close teardown: abort every still-running local subagent (reaping
    /// its child), report each `cancelled{reason:"session_close"}`, clear the
    /// whole map, and — if it was non-empty — fire the `updateClientTools`
    /// removal so the status tool disappears.
    pub async fn close_all(&self) {
        let _transition = self.inner.transition.lock().await;
        let (cancelled, was_nonempty) = {
            let mut map = self.inner.tasks.lock().unwrap();
            let was_nonempty = !map.is_empty();
            let mut cancelled: Vec<(String, String, String)> = Vec::new();
            for (id, task) in map.iter_mut() {
                if task.status == SubagentStatus::Running {
                    if let Some(handle) = task.handle.take() {
                        handle.abort();
                    }
                    task.status = SubagentStatus::Cancelled;
                    task.reason = Some("session_close".to_string());
                    cancelled.push((id.clone(), task.harness.clone(), task.model.clone()));
                }
            }
            map.clear();
            (cancelled, was_nonempty)
        };
        for (id, harness, model) in cancelled {
            self.report(LocalSubagentReport {
                state: SubagentStatus::Cancelled.report_state().to_string(),
                subagent_id: id,
                harness,
                model,
                detail: None,
                reason: Some("session_close".to_string()),
                exit_code: None,
            })
            .await;
        }
        if was_nonempty {
            self.sync_client_tools(false).await;
        }
    }
}

impl std::fmt::Debug for SubagentSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentSession")
            .field("connected", &self.inner.rpc.get().is_some())
            .field("tracked", &self.inner.tasks.lock().unwrap().len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Pinned tool schemas / descriptions
// ---------------------------------------------------------------------------

const STATUS_TOOL_DESCRIPTION: &str =
    "Check the status of subagents launched with launch_subagent. \
Pass a subagent_id to query one subagent, or omit it to list all tracked subagents. \
A subagent that has finished is reported with its captured output exactly once.";

fn status_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "subagent_id": {
                "type": "string",
                "description": "The subagent to query. Omit to report every tracked subagent."
            }
        },
        "required": [],
        "additionalProperties": false
    })
}

fn status_tool_declaration() -> Value {
    json!({
        "name": LOCAL_SUBAGENT_STATUS_TOOL,
        "description": STATUS_TOOL_DESCRIPTION,
        "input_schema": status_input_schema(),
    })
}

fn launch_description(names: &[String]) -> String {
    format!(
        "Launch a CLI subagent ({}) to work on a task in the background. This tool is ASYNCHRONOUS: \
it returns immediately with a subagent_id and status \"started\" — it never waits for or returns \
the subagent's output. The subagent keeps running in the background; call the subagent status tool \
later to check whether it has finished and to retrieve its output.",
        names.join(", ")
    )
}

fn launch_input_schema(names: &[String]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "harness": {
                "type": "string",
                "enum": names,
                "description": "Which configured CLI subagent to launch."
            },
            "model": {
                "type": "string",
                "description": "The model name passed to the subagent CLI."
            },
            "prompt": {
                "type": "string",
                "description": "The task prompt handed to the subagent."
            }
        },
        "required": ["harness", "model", "prompt"],
        "additionalProperties": false
    })
}

// ---------------------------------------------------------------------------
// Template validation (§8) — construction-time developer-bug panics
// ---------------------------------------------------------------------------

/// Validate a def's `command_template` placeholders against §8's rules. Panics
/// (developer bug at construction) on any violation.
fn validate_template(def: &SubagentDef) {
    let params = parse_params(&def.command_template);
    for p in &params {
        assert!(
            p == "model" || p == "prompt",
            "subagent command_template placeholder `{{{p}}}` is not recognized \
             (only `{{model}}` and `{{prompt}}` are allowed)"
        );
    }
    let has_prompt = params.iter().any(|p| p == "prompt");
    match def.prompt_via {
        PromptDelivery::Arg => assert!(
            has_prompt,
            "subagent command_template must contain `{{prompt}}` when prompt_via is Arg"
        ),
        PromptDelivery::Stdin => assert!(
            !has_prompt,
            "subagent command_template must not contain `{{prompt}}` when prompt_via is Stdin \
             (the prompt is piped to stdin, never placed in argv)"
        ),
    }
}

// ---------------------------------------------------------------------------
// The launch tool
// ---------------------------------------------------------------------------

/// A resolved def, captured by the launch handler.
#[derive(Clone)]
struct ResolvedDef {
    command_template: String,
    prompt_via: PromptDelivery,
    timeout: Duration,
}

/// Which sandbox target a `Local` launch runs against (image resolved at
/// construction; the same target for every configured harness).
#[derive(Clone)]
enum SubTarget {
    /// Bare host `/bin/sh -c`.
    Host,
    /// The harness's own container for this image (`exec -i`).
    Container(String),
    /// The session's remote sandbox via `execRemoteSandbox` (no stdin).
    Remote,
}

/// Declare the `launch_subagent` tool covering a **set** of configured CLI
/// subagents (the model picks one by `harness`). Returns a client-dispatched
/// [`SubagentTool::Tool`] for a [`SubagentLaunch::Local`] launch and a
/// declaration-only [`SubagentTool::Def`] for [`SubagentLaunch::Remote`].
///
/// Every `{model}` substitution (and `{prompt}` under [`PromptDelivery::Arg`])
/// is shell-escaped before interpolation — the injection boundary, shared with
/// [`crate::sandbox`]. Under [`PromptDelivery::Stdin`] the prompt is piped to
/// the child and never enters the constructed argv.
///
/// # Panics
///
/// On a developer bug at construction: empty `configs`; duplicate `harness`
/// names; a malformed template or one violating §8's placeholder rules; or a
/// `Local(SandboxTarget::Remote)` launch combined with any [`PromptDelivery::Stdin`]
/// def (`execRemoteSandbox` carries no stdin).
pub fn launch_subagent(
    session: &SubagentSession,
    configs: Vec<SubagentDef>,
    launch: SubagentLaunch,
) -> SubagentTool {
    assert!(
        !configs.is_empty(),
        "launch_subagent requires at least one SubagentDef"
    );
    let mut names: Vec<String> = Vec::with_capacity(configs.len());
    for def in &configs {
        assert!(
            !names.contains(&def.harness),
            "duplicate subagent harness name `{}`",
            def.harness
        );
        names.push(def.harness.clone());
        validate_template(def);
    }

    let description = launch_description(&names);
    let input_schema = launch_input_schema(&names);

    // Remote launch → a declaration only (server-dispatched); no handler.
    let target = match launch {
        SubagentLaunch::Remote { image } => {
            let subagents: Vec<Value> = configs
                .iter()
                .map(|d| {
                    json!({
                        "harness": d.harness,
                        "command_template": d.command_template,
                        "prompt_via": d.prompt_via.as_str(),
                        "timeout_secs": d.timeout.as_secs(),
                    })
                })
                .collect();
            return SubagentTool::Def(SubagentToolDef {
                name: LAUNCH_SUBAGENT_TOOL.to_string(),
                description,
                input_schema,
                image,
                subagents,
            });
        }
        SubagentLaunch::Local(target) => target,
    };

    let sub_target = match target {
        SandboxTarget::None => SubTarget::Host,
        SandboxTarget::Local { image } => SubTarget::Container(image),
        SandboxTarget::Remote => {
            // execRemoteSandbox has no stdin: a Stdin def would silently drop its
            // prompt, so that combination is a construction error.
            for def in &configs {
                assert!(
                    def.prompt_via == PromptDelivery::Arg,
                    "a SandboxTarget::Remote local launch requires prompt_via: Arg \
                     (execRemoteSandbox carries no stdin), but harness `{}` uses Stdin",
                    def.harness
                );
            }
            SubTarget::Remote
        }
    };

    session.mark_local();

    let resolved: Arc<HashMap<String, ResolvedDef>> = Arc::new(
        configs
            .into_iter()
            .map(|d| {
                (
                    d.harness,
                    ResolvedDef {
                        command_template: d.command_template,
                        prompt_via: d.prompt_via,
                        timeout: d.timeout,
                    },
                )
            })
            .collect(),
    );

    let session = session.clone();
    let sub_target = Arc::new(sub_target);
    let handler: ToolHandler = Box::new(move |input| {
        let session = session.clone();
        let resolved = resolved.clone();
        let sub_target = sub_target.clone();
        Box::pin(async move {
            session.rpc()?;
            Ok(handle_launch(&session, &resolved, &sub_target, &input).await)
        })
    });

    SubagentTool::Tool(Tool::from_handler(
        LAUNCH_SUBAGENT_TOOL,
        description,
        input_schema,
        handler,
    ))
}

/// The `Local` launch handler — validates, spawns the fire-and-forget watcher,
/// and returns `{"status":"started",…}` immediately (§5.6 error results on
/// validation failure; never an aborted turn).
async fn handle_launch(
    session: &SubagentSession,
    resolved: &HashMap<String, ResolvedDef>,
    sub_target: &SubTarget,
    input: &Value,
) -> Value {
    // Field validation.
    let (harness, model, prompt) = match parse_launch_input(input) {
        Some(triple) => triple,
        None => {
            return error_result(
                "launch_subagent requires string \"harness\", \"model\", and \"prompt\"",
            )
        }
    };

    // Harness lookup.
    let def = match resolved.get(&harness) {
        Some(def) => def.clone(),
        None => return error_result(&format!("unknown harness \"{harness}\"")),
    };

    // Build the (shell-escaped) command and the spawn plan.
    let interp = json!({ "model": model, "prompt": prompt });
    let command = match interpolate(&def.command_template, &interp) {
        Ok(c) => c,
        Err(e) => {
            // Should not happen (fields validated), but surface in-band.
            return error_result(&format!("failed to build subagent command: {e}"));
        }
    };
    let stdin = match def.prompt_via {
        PromptDelivery::Stdin => Some(prompt.clone().into_bytes()),
        PromptDelivery::Arg => None,
    };
    let plan = match sub_target {
        SubTarget::Host => SpawnPlan::Host { command, stdin },
        SubTarget::Container(image) => SpawnPlan::Container {
            image: image.clone(),
            command,
            stdin,
        },
        SubTarget::Remote => SpawnPlan::Remote { command },
    };

    // Cap check, reservation, and its dynamic-tool update are one serialized
    // state transition. This prevents concurrent handlers from all observing
    // an empty map or committing stale full-replace updates out of order.
    let _transition = session.inner.transition.lock().await;
    let subagent_id = generate_subagent_id();
    let seq = session.inner.seq.fetch_add(1, Ordering::SeqCst);
    let was_empty = {
        let mut map = session.inner.tasks.lock().unwrap();
        let running = map.values().filter(|t| !t.status.is_terminal()).count();
        if running >= MAX_SUBAGENTS_PER_SESSION {
            return error_result(&format!(
                "subagent limit reached (max {MAX_SUBAGENTS_PER_SESSION} per session)"
            ));
        }
        let was_empty = map.is_empty();
        map.insert(
            subagent_id.clone(),
            SubagentTask {
                seq,
                harness: harness.clone(),
                model: model.clone(),
                status: SubagentStatus::Running,
                exit_code: None,
                stdout: None,
                stderr: None,
                truncated: false,
                reason: None,
                detail: None,
                handle: None,
            },
        );
        was_empty
    };

    // Report `start` before anything spawns.
    session
        .report(LocalSubagentReport {
            state: "start".to_string(),
            subagent_id: subagent_id.clone(),
            harness: harness.clone(),
            model: model.clone(),
            detail: None,
            reason: None,
            exit_code: None,
        })
        .await;

    // Spawn the detached watcher, then record its handle for cancel/close.
    // The watcher may run the child immediately, but its terminal state/report
    // is gated until the `running` report below has completed.
    let running_gate = Arc::new(tokio::sync::Notify::new());
    let handle = {
        let session = session.clone();
        let id = subagent_id.clone();
        let harness = harness.clone();
        let model = model.clone();
        let timeout = def.timeout;
        let running_gate = running_gate.clone();
        tokio::spawn(async move {
            watch(session, id, harness, model, timeout, plan, running_gate).await
        })
    };
    {
        let mut map = session.inner.tasks.lock().unwrap();
        if let Some(task) = map.get_mut(&subagent_id) {
            // Only attach the handle if still running — an instantaneous fake
            // runner may already have driven it terminal.
            if task.status == SubagentStatus::Running {
                task.handle = Some(handle);
            }
        }
    }

    // Report `running` right after the spawn.
    session
        .report(LocalSubagentReport {
            state: "running".to_string(),
            subagent_id: subagent_id.clone(),
            harness: harness.clone(),
            model: model.clone(),
            detail: None,
            reason: None,
            exit_code: None,
        })
        .await;

    // Empty→non-empty transition: advertise the status tool.
    if was_empty {
        session.sync_client_tools(true).await;
    }
    running_gate.notify_one();

    json_string(&StartedResult {
        subagent_id: &subagent_id,
        harness: &harness,
        model: &model,
        status: "started",
    })
}

/// Extract the three required non-empty string fields, or `None`.
fn parse_launch_input(input: &Value) -> Option<(String, String, String)> {
    let field = |name: &str| {
        input
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    Some((field("harness")?, field("model")?, field("prompt")?))
}

/// How the watcher runs the subprocess.
enum SpawnPlan {
    Host {
        command: String,
        stdin: Option<Vec<u8>>,
    },
    Container {
        image: String,
        command: String,
        stdin: Option<Vec<u8>>,
    },
    Remote {
        command: String,
    },
}

/// The outcome of running a [`SpawnPlan`] under its timeout.
enum ExecOutcome {
    TimedOut,
    Settled(std::io::Result<RunnerOutput>),
}

/// The detached background task: run the plan under its timeout, truncate the
/// output, set the terminal status (only if still `Running` — a cancel/close may
/// have won the race), and mirror the terminal state via telemetry.
async fn watch(
    session: SubagentSession,
    subagent_id: String,
    harness: String,
    model: String,
    timeout: Duration,
    plan: SpawnPlan,
    running_gate: Arc<tokio::sync::Notify>,
) {
    let runner = session.runner();
    let sandbox = session.inner.sandbox.clone();

    let work = async {
        match plan {
            SpawnPlan::Host { command, stdin } => {
                let args = vec!["-c".to_string(), command];
                runner.run("/bin/sh", &args, stdin.as_deref()).await
            }
            SpawnPlan::Container {
                image,
                command,
                stdin,
            } => {
                let handle = sandbox
                    .start_local(&image)
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let program = sandbox.engine_program();
                let args = vec![
                    "exec".to_string(),
                    "-i".to_string(),
                    handle.id,
                    "sh".to_string(),
                    "-c".to_string(),
                    command,
                ];
                runner.run(program, &args, stdin.as_deref()).await
            }
            SpawnPlan::Remote { command } => sandbox
                .exec_remote_sandbox(&command)
                .await
                .map(|r| RunnerOutput {
                    stdout: r.stdout,
                    stderr: r.stderr,
                    exit_code: r.exit_code,
                })
                .map_err(|e| std::io::Error::other(e.to_string())),
        }
    };

    let outcome = match tokio::time::timeout(timeout, work).await {
        Ok(res) => ExecOutcome::Settled(res),
        Err(_) => ExecOutcome::TimedOut,
    };

    // A zero-duration timeout or immediately-resolving fake must not publish a
    // terminal transition before the launch handler's `running` telemetry.
    running_gate.notified().await;

    // Compute the terminal state from the outcome.
    let (status, exit_code, stdout, stderr, truncated, reason, detail) = match outcome {
        ExecOutcome::TimedOut => (
            SubagentStatus::TimedOut,
            None,
            None,
            None,
            false,
            Some("timeout".to_string()),
            None,
        ),
        ExecOutcome::Settled(Ok(out)) => {
            let (so, so_trunc) = truncate_output(out.stdout);
            let (se, se_trunc) = truncate_output(out.stderr);
            let truncated = so_trunc || se_trunc;
            if out.exit_code == 0 {
                (
                    SubagentStatus::Completed,
                    Some(0),
                    Some(so),
                    Some(se),
                    truncated,
                    None,
                    None,
                )
            } else {
                (
                    SubagentStatus::Failed,
                    Some(out.exit_code),
                    Some(so),
                    Some(se),
                    truncated,
                    Some("nonzero_exit".to_string()),
                    None,
                )
            }
        }
        ExecOutcome::Settled(Err(e)) => (
            SubagentStatus::Failed,
            None,
            None,
            None,
            false,
            Some("spawn_failed".to_string()),
            Some(e.to_string()),
        ),
    };

    // Apply only if still Running (a cancel/close may have set a terminal state
    // already — never overwrite it, and never re-report it).
    let applied = {
        let mut map = session.inner.tasks.lock().unwrap();
        match map.get_mut(&subagent_id) {
            Some(task) if task.status == SubagentStatus::Running => {
                task.status = status;
                task.exit_code = exit_code;
                task.stdout = stdout;
                task.stderr = stderr;
                task.truncated = truncated;
                task.reason = reason.clone();
                task.detail = detail.clone();
                task.handle = None;
                true
            }
            _ => false,
        }
    };

    if applied {
        session
            .report(LocalSubagentReport {
                state: status.report_state().to_string(),
                subagent_id,
                harness,
                model,
                detail,
                reason,
                exit_code,
            })
            .await;
    }
}

/// Truncate to the first [`SUBAGENT_OUTPUT_CAP_BYTES`] bytes on a UTF-8 char
/// boundary; the `bool` is whether anything was cut.
fn truncate_output(s: String) -> (String, bool) {
    if s.len() <= SUBAGENT_OUTPUT_CAP_BYTES {
        return (s, false);
    }
    let mut end = SUBAGENT_OUTPUT_CAP_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Generate a `sba_` + 32-hex-char id from 16 OS-random bytes — the identical
/// format the server mints for remote subagents.
fn generate_subagent_id() -> String {
    use std::fmt::Write;
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
    let mut s = String::with_capacity(4 + 32);
    s.push_str("sba_");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::Mutex as StdMutex2;
    use tokio::sync::Notify;

    // -----------------------------------------------------------------------
    // Test doubles — a fake subprocess runner and a call-recording RPC, both
    // fully offline (no real `claude`/`codex` binary, no network). Mirrors the
    // sandbox module's FakeDriver/FakeRpc pattern (see `sandbox.rs::tests`).
    // -----------------------------------------------------------------------

    /// Scripted outcome for [`FakeRunner::run`].
    #[derive(Clone)]
    enum FakeOutcome {
        Ok(RunnerOutput),
        Err(String),
        /// Never resolves on its own — only killed by an aborted watcher task
        /// (proves real kill-on-abort semantics via `DropMarker`).
        Pending,
    }

    /// Sets an `AtomicBool` when dropped — lets a test prove a background
    /// future was truly aborted (kill_on_drop-style), not merely abandoned.
    struct DropMarker(Arc<AtomicBool>);
    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// One recorded `(program, args, stdin)` call to [`FakeRunner::run`].
    type CallLog = Arc<StdMutex2<Vec<(String, Vec<String>, Option<Vec<u8>>)>>>;

    /// A fake [`SubagentRunner`] recording every `(program, args, stdin)` call
    /// into a shared timeline, optionally gated behind a [`Notify`] so a test
    /// can control exactly when the subprocess "exits".
    struct FakeRunner {
        calls: CallLog,
        gate: Option<Arc<Notify>>,
        outcome: FakeOutcome,
        drop_flag: Option<Arc<AtomicBool>>,
    }

    impl FakeRunner {
        fn immediate(outcome: FakeOutcome) -> (Arc<Self>, CallLog) {
            Self::new(outcome, None, None)
        }

        fn gated(outcome: FakeOutcome, gate: Arc<Notify>) -> (Arc<Self>, CallLog) {
            Self::new(outcome, Some(gate), None)
        }

        fn new(
            outcome: FakeOutcome,
            gate: Option<Arc<Notify>>,
            drop_flag: Option<Arc<AtomicBool>>,
        ) -> (Arc<Self>, CallLog) {
            let calls = Arc::new(StdMutex2::new(Vec::new()));
            (
                Arc::new(Self {
                    calls: calls.clone(),
                    gate,
                    outcome,
                    drop_flag,
                }),
                calls,
            )
        }
    }

    impl SubagentRunner for FakeRunner {
        fn run<'a>(
            &'a self,
            program: &'a str,
            args: &'a [String],
            stdin: Option<&'a [u8]>,
        ) -> SubagentFuture<'a, io::Result<RunnerOutput>> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.to_vec(),
                stdin.map(|b| b.to_vec()),
            ));
            let gate = self.gate.clone();
            let outcome = self.outcome.clone();
            let marker = self.drop_flag.clone().map(DropMarker);
            Box::pin(async move {
                let _marker = marker;
                if let Some(g) = gate {
                    g.notified().await;
                }
                match outcome {
                    FakeOutcome::Ok(o) => Ok(o),
                    FakeOutcome::Err(e) => Err(io::Error::other(e)),
                    FakeOutcome::Pending => std::future::pending().await,
                }
            })
        }
    }

    /// A fake [`SubagentRpc`] recording every `reportLocalSubagent` /
    /// `updateClientTools` / `cancelSubagent` call, exactly the call-recording
    /// technique `MockTransport` uses for `register_driver` (see
    /// `harness.rs::tests`). `terminal_notify`, if set, fires once per
    /// terminal (`completed`/`failed`/`cancelled`) report — the test's
    /// synchronization point for the detached watcher task.
    #[derive(Default)]
    struct FakeSubagentRpc {
        reports: StdMutex2<Vec<LocalSubagentReport>>,
        update_calls: StdMutex2<Vec<Vec<Value>>>,
        cancel_calls: StdMutex2<Vec<String>>,
        terminal_notify: StdMutex2<Option<Arc<Notify>>>,
    }

    impl FakeSubagentRpc {
        fn with_terminal_notify(notify: Arc<Notify>) -> Arc<Self> {
            let rpc = Self::default();
            *rpc.terminal_notify.lock().unwrap() = Some(notify);
            Arc::new(rpc)
        }
    }

    impl SubagentRpc for FakeSubagentRpc {
        fn report_local_subagent(
            &self,
            report: LocalSubagentReport,
        ) -> SubagentFuture<'_, Result<(), Error>> {
            let terminal = matches!(report.state.as_str(), "completed" | "failed" | "cancelled");
            self.reports.lock().unwrap().push(report);
            if terminal {
                if let Some(n) = self.terminal_notify.lock().unwrap().as_ref() {
                    n.notify_one();
                }
            }
            Box::pin(async { Ok(()) })
        }
        fn update_client_tools(&self, tools: Vec<Value>) -> SubagentFuture<'_, Result<(), Error>> {
            self.update_calls.lock().unwrap().push(tools);
            Box::pin(async { Ok(()) })
        }
        fn cancel_subagent(&self, subagent_id: String) -> SubagentFuture<'_, Result<(), Error>> {
            self.cancel_calls.lock().unwrap().push(subagent_id);
            Box::pin(async { Ok(()) })
        }
    }

    fn session_with_rpc() -> (SubagentSession, Arc<FakeSubagentRpc>) {
        let session = SubagentSession::new(SandboxSession::new());
        let rpc = FakeSubagentRpc::with_terminal_notify(Arc::new(Notify::new()));
        session.bind(rpc.clone() as Arc<dyn SubagentRpc>);
        session.set_base_client_tools(vec![json!({
            "name": LAUNCH_SUBAGENT_TOOL,
            "description": "launch",
            "input_schema": {},
        })]);
        (session, rpc)
    }

    fn terminal_notify(rpc: &FakeSubagentRpc) -> Arc<Notify> {
        rpc.terminal_notify
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone()
    }

    async fn call(tool: &Tool, input: Value) -> Value {
        let raw = tool.call(input).await.expect("tool call ok");
        serde_json::from_str(raw.as_str().expect("string content")).unwrap()
    }

    fn claude_def(template: &str, via: PromptDelivery) -> SubagentDef {
        SubagentDef::new("claude", template).with_prompt_via(via)
    }

    // -----------------------------------------------------------------------
    // 1. Shell-escaping for {model}/{prompt}, parametrized Arg vs. Stdin.
    //    Identical payload list to `sandbox.rs::tests::injection_cases` /
    //    `client-typescript/src/sandbox.test.ts` / `test_sandbox_injection.py`.
    // -----------------------------------------------------------------------

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
    async fn arg_mode_shell_escapes_every_injection_payload_into_argv() {
        for (payload, expected) in injection_cases() {
            let (session, rpc) = session_with_rpc();
            let notify = terminal_notify(&rpc);
            let (runner, calls) = FakeRunner::immediate(FakeOutcome::Ok(RunnerOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }));
            session.set_runner(runner);
            let tool = launch_subagent(
                &session,
                vec![claude_def("echo {model} {prompt}", PromptDelivery::Arg)],
                SubagentLaunch::Local(SandboxTarget::None),
            )
            .into_tool()
            .expect("local launch yields a client-dispatched tool");

            call(
                &tool,
                json!({ "harness": "claude", "model": payload, "prompt": payload }),
            )
            .await;
            // Wait for the detached watcher to actually invoke the runner and
            // report its terminal state before inspecting the call log.
            notify.notified().await;

            let seen = calls.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "exactly one subprocess spawned");
            let (program, args, stdin) = &seen[0];
            assert_eq!(program, "/bin/sh");
            let quoted = expected.strip_prefix("echo ").unwrap();
            assert_eq!(
                args,
                &vec!["-c".to_string(), format!("{expected} {quoted}")]
            );
            assert!(
                stdin.is_none(),
                "Arg mode never writes to stdin (payload {payload:?})"
            );
        }
    }

    #[tokio::test]
    async fn stdin_mode_never_places_the_raw_prompt_in_argv() {
        for (payload, _) in injection_cases() {
            let prompt = format!("prompt:\n{payload}");
            let (session, rpc) = session_with_rpc();
            let notify = terminal_notify(&rpc);
            let (runner, calls) = FakeRunner::immediate(FakeOutcome::Ok(RunnerOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }));
            session.set_runner(runner);
            // No `{prompt}` placeholder at all under Stdin (construction would
            // panic otherwise) — the command is fixed regardless of payload.
            let tool = launch_subagent(
                &session,
                vec![claude_def("cat --model {model}", PromptDelivery::Stdin)],
                SubagentLaunch::Local(SandboxTarget::None),
            )
            .into_tool()
            .expect("local launch yields a client-dispatched tool");

            call(
                &tool,
                json!({ "harness": "claude", "model": payload, "prompt": prompt }),
            )
            .await;
            notify.notified().await;

            let seen = calls.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            let (program, args, stdin) = &seen[0];
            assert_eq!(program, "/bin/sh");
            assert_eq!(
                args,
                &vec![
                    "-c".to_string(),
                    format!("cat --model {}", crate::sandbox::shell_quote(payload)),
                ]
            );
            // The constructed argv carries no trace of the payload anywhere.
            assert!(args.iter().all(|a| !a.contains(&prompt)));
            // The raw (unescaped) prompt reaches the child only via stdin.
            assert_eq!(stdin.as_deref(), Some(prompt.as_bytes()));
        }
    }

    // -----------------------------------------------------------------------
    // 2. Immediate-return contract: `launch_subagent`'s result is exactly
    //    `{"status":"started",...}`, never the subagent's output — even when
    //    the (fake) subprocess has already produced output by the time the
    //    handler's own future resolves.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn launch_result_is_exactly_the_started_shape_never_output() {
        let (session, _rpc) = session_with_rpc();
        let (runner, _calls) = FakeRunner::immediate(FakeOutcome::Ok(RunnerOutput {
            stdout: "SECRET_SUBAGENT_OUTPUT".to_string(),
            stderr: "SECRET_STDERR".to_string(),
            exit_code: 0,
        }));
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();

        let result = call(
            &tool,
            json!({ "harness": "claude", "model": "claude-sonnet-5", "prompt": "hi" }),
        )
        .await;

        let obj = result.as_object().expect("object result");
        assert_eq!(obj.len(), 4, "exactly the pinned four keys: {obj:?}");
        assert_eq!(obj["harness"], json!("claude"));
        assert_eq!(obj["model"], json!("claude-sonnet-5"));
        assert_eq!(obj["status"], json!("started"));
        assert!(obj["subagent_id"].as_str().unwrap().starts_with("sba_"));
        let dumped = serde_json::to_string(&result).unwrap();
        assert!(!dumped.contains("SECRET_SUBAGENT_OUTPUT"));
        assert!(!dumped.contains("SECRET_STDERR"));
    }

    // -----------------------------------------------------------------------
    // 3. Status-tool visibility — `updateClientTools` fires exactly on the
    //    empty→non-empty and non-empty→empty transitions, never redundantly
    //    (a second concurrent launch does not re-send).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn update_client_tools_fires_exactly_on_transitions_never_redundantly() {
        let (session, rpc) = session_with_rpc();
        // A runner that never settles on its own (blocked on a gate that is
        // never notified in this test) — we only care about the launch-side
        // transition here, not completion.
        let (runner, _calls) = FakeRunner::immediate(FakeOutcome::Pending);
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();

        assert_eq!(rpc.update_calls.lock().unwrap().len(), 0);

        // First launch: empty -> non-empty. Fires once, includes the status tool.
        call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "first" }),
        )
        .await;
        {
            let updates = rpc.update_calls.lock().unwrap();
            assert_eq!(updates.len(), 1);
            assert!(updates[0]
                .iter()
                .any(|t| t["name"] == json!(LOCAL_SUBAGENT_STATUS_TOOL)));
            assert!(updates[0]
                .iter()
                .any(|t| t["name"] == json!(LAUNCH_SUBAGENT_TOOL)));
        }

        // Second concurrent launch: non-empty -> non-empty. No re-send.
        call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "second" }),
        )
        .await;
        assert_eq!(
            rpc.update_calls.lock().unwrap().len(),
            1,
            "a second concurrent launch must not re-send updateClientTools"
        );
    }

    // -----------------------------------------------------------------------
    // 4. Eviction-after-acknowledgment + unknown-id error, plus the
    //    non-empty->empty `updateClientTools` transition on the evicting read.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn terminal_entry_reported_once_then_evicted_and_unknown_id_errors() {
        let (session, rpc) = session_with_rpc();
        let notify = terminal_notify(&rpc);
        let status = session.status_tool();

        // Unknown id before anything was ever launched.
        let err = call(&status, json!({ "subagent_id": "sba_doesnotexist" })).await;
        assert_eq!(err, json!({ "error": "unknown subagent_id" }));

        let gate = Arc::new(Notify::new());
        let (runner, _calls) = FakeRunner::gated(
            FakeOutcome::Ok(RunnerOutput {
                stdout: "done".to_string(),
                stderr: String::new(),
                exit_code: 0,
            }),
            gate.clone(),
        );
        session.set_runner(runner);

        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();

        let started = call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "hi" }),
        )
        .await;
        let id = started["subagent_id"].as_str().unwrap().to_string();

        // While running: listed, but not evicted, no output yet.
        let running = call(&status, json!({})).await;
        let entries = running["subagents"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["status"], json!("running"));
        assert_eq!(entries[0]["stdout"], Value::Null);

        // Let the fake subprocess "exit" and wait for the watcher's terminal report.
        gate.notify_one();
        notify.notified().await;

        // First poll after completion: included exactly once, terminal.
        let first = call(&status, json!({})).await;
        let entries = first["subagents"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["subagent_id"], json!(id));
        assert_eq!(entries[0]["status"], json!("completed"));
        assert_eq!(entries[0]["stdout"], json!("done"));

        // Second poll: the map has emptied — omitted entirely (evict-on-report).
        let second = call(&status, json!({})).await;
        assert_eq!(second["subagents"], json!([]));

        // Querying that id now answers the unknown-id error.
        let by_id = call(&status, json!({ "subagent_id": id })).await;
        assert_eq!(by_id, json!({ "error": "unknown subagent_id" }));

        // The evicting read fired the non-empty->empty updateClientTools
        // removal: the last update no longer includes the status tool.
        let updates = rpc.update_calls.lock().unwrap();
        assert_eq!(updates.len(), 2, "one at launch, one at eviction");
        assert!(updates[0]
            .iter()
            .any(|t| t["name"] == json!(LOCAL_SUBAGENT_STATUS_TOOL)));
        assert!(!updates[1]
            .iter()
            .any(|t| t["name"] == json!(LOCAL_SUBAGENT_STATUS_TOOL)));
    }

    // -----------------------------------------------------------------------
    // 5. Truncation: fake subprocess output past the cap yields
    //    `"truncated": true` and output capped at the documented limit.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn output_past_the_cap_is_truncated_and_flagged() {
        let (session, rpc) = session_with_rpc();
        let notify = terminal_notify(&rpc);
        let huge = "a".repeat(SUBAGENT_OUTPUT_CAP_BYTES + 1000);
        let (runner, _calls) = FakeRunner::immediate(FakeOutcome::Ok(RunnerOutput {
            stdout: huge.clone(),
            stderr: String::new(),
            exit_code: 0,
        }));
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();
        call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "hi" }),
        )
        .await;
        notify.notified().await;

        let status = session.status_tool();
        let result = call(&status, json!({})).await;
        let entry = &result["subagents"].as_array().unwrap()[0];
        assert_eq!(entry["truncated"], json!(true));
        let stdout = entry["stdout"].as_str().unwrap();
        assert_eq!(stdout.len(), SUBAGENT_OUTPUT_CAP_BYTES);
        assert!(huge.starts_with(stdout));
        assert!(stdout.len() < huge.len(), "output was actually cut");
    }

    /// A spawn/io failure (`FakeOutcome::Err`) surfaces as `Failed` with
    /// `reason:"spawn_failed"` and a `null` exit code — not a panic, not a
    /// dropped turn.
    #[tokio::test]
    async fn spawn_failure_reports_failed_with_spawn_failed_reason() {
        let (session, rpc) = session_with_rpc();
        let notify = terminal_notify(&rpc);
        let (runner, _calls) = FakeRunner::immediate(FakeOutcome::Err("no such file".to_string()));
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();
        call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "hi" }),
        )
        .await;
        notify.notified().await;

        let status = session.status_tool();
        let result = call(&status, json!({})).await;
        let entry = &result["subagents"].as_array().unwrap()[0];
        assert_eq!(entry["status"], json!("failed"));
        assert_eq!(entry["reason"], json!("spawn_failed"));
        assert_eq!(entry["exit_code"], Value::Null);
        assert!(entry["detail"].as_str().unwrap().contains("no such file"));
    }

    #[tokio::test]
    async fn timeout_kills_work_and_reports_timed_out_status() {
        let (session, rpc) = session_with_rpc();
        let notify = terminal_notify(&rpc);
        let dropped = Arc::new(AtomicBool::new(false));
        let (runner, _) = FakeRunner::new(FakeOutcome::Pending, None, Some(dropped.clone()));
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat").with_timeout(Duration::ZERO)],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();
        call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "p" }),
        )
        .await;
        notify.notified().await;
        assert!(dropped.load(Ordering::SeqCst));
        let result = call(&session.status_tool(), json!({})).await;
        assert_eq!(result["subagents"][0]["status"], json!("timed_out"));
        assert_eq!(result["subagents"][0]["reason"], json!("timeout"));
        let states: Vec<String> = rpc
            .reports
            .lock()
            .unwrap()
            .iter()
            .map(|report| report.state.clone())
            .collect();
        assert_eq!(states, ["start", "running", "failed"]);
    }

    #[tokio::test]
    async fn explicit_cancel_kills_work_and_remains_visible_until_status() {
        let (session, rpc) = session_with_rpc();
        let dropped = Arc::new(AtomicBool::new(false));
        let (runner, _) = FakeRunner::new(FakeOutcome::Pending, None, Some(dropped.clone()));
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();
        let started = call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "p" }),
        )
        .await;
        let id = started["subagent_id"].as_str().unwrap();
        tokio::task::yield_now().await;
        session.cancel_subagent(id).await;
        for _ in 0..10 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(dropped.load(Ordering::SeqCst));
        let result = call(&session.status_tool(), json!({ "subagent_id": id })).await;
        assert_eq!(result["subagents"][0]["status"], json!("cancelled"));
        assert_eq!(result["subagents"][0]["reason"], json!("explicit"));
        assert!(rpc.reports.lock().unwrap().iter().any(|report| {
            report.state == "cancelled" && report.reason.as_deref() == Some("explicit")
        }));
    }

    // -----------------------------------------------------------------------
    // 6. Session close teardown: a still-Running local subagent is killed
    //    (its watcher future dropped) and reported `cancelled{reason:
    //    "session_close"}`; the removal transition fires if the map emptied.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn close_all_kills_running_subagent_and_reports_session_close() {
        let (session, rpc) = session_with_rpc();
        let dropped = Arc::new(AtomicBool::new(false));
        let (runner, _calls) = FakeRunner::new(FakeOutcome::Pending, None, Some(dropped.clone()));
        session.set_runner(runner);
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();
        call(
            &tool,
            json!({ "harness": "claude", "model": "m", "prompt": "hi" }),
        )
        .await;
        // Give the spawned watcher a chance to actually start polling the
        // pending future before we tear down.
        tokio::task::yield_now().await;

        assert!(
            !dropped.load(Ordering::SeqCst),
            "still running before close"
        );

        session.close_all().await;
        // `abort()` schedules cancellation; give the runtime a few ticks to
        // actually drop the aborted task's future.
        for _ in 0..10 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(
            dropped.load(Ordering::SeqCst),
            "close_all must abort the watcher, dropping (killing) the subprocess future"
        );
        let reports = rpc.reports.lock().unwrap();
        assert!(reports
            .iter()
            .any(|r| r.state == "cancelled" && r.reason.as_deref() == Some("session_close")));
        let updates = rpc.update_calls.lock().unwrap();
        // Launch fired the empty->non-empty transition; close_all fires the
        // non-empty->empty removal.
        assert_eq!(updates.len(), 2);
        assert!(!updates
            .last()
            .unwrap()
            .iter()
            .any(|t| t["name"] == json!(LOCAL_SUBAGENT_STATUS_TOOL)));
    }

    #[tokio::test]
    async fn local_tools_fail_before_the_session_is_connected() {
        let session = SubagentSession::new(SandboxSession::new());
        let tool = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::None),
        )
        .into_tool()
        .unwrap();

        let launch_error = tool
            .call(json!({ "harness": "claude", "model": "m", "prompt": "p" }))
            .await
            .expect_err("launch before connect must fail");
        assert!(launch_error
            .to_string()
            .contains("before the session was connected"));
        let status_error = session
            .status_tool()
            .call(json!({}))
            .await
            .expect_err("status before connect must fail");
        assert!(status_error
            .to_string()
            .contains("before the session was connected"));
        assert!(session.inner.tasks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn prompt_and_model_boundary_whitespace_is_delivered_verbatim() {
        let prompt = "  keep this indentation\n";
        let model = " model-with-spaces ";
        for definition in [
            SubagentDef::new("claude", "cli --model {model}"),
            SubagentDef::new("claude", "cli --model {model} --prompt {prompt}")
                .with_prompt_via(PromptDelivery::Arg),
        ] {
            let via = definition.prompt_via;
            let (session, rpc) = session_with_rpc();
            let notify = terminal_notify(&rpc);
            let (runner, calls) = FakeRunner::immediate(FakeOutcome::Ok(RunnerOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            }));
            session.set_runner(runner);
            let tool = launch_subagent(
                &session,
                vec![definition],
                SubagentLaunch::Local(SandboxTarget::None),
            )
            .into_tool()
            .unwrap();
            let started = call(
                &tool,
                json!({ "harness": "claude", "model": model, "prompt": prompt }),
            )
            .await;
            assert_eq!(started["model"], model);
            notify.notified().await;
            let seen = calls.lock().unwrap().clone();
            let (_, args, stdin) = &seen[0];
            assert!(args.last().unwrap().contains("' model-with-spaces '"));
            match via {
                PromptDelivery::Stdin => {
                    assert_eq!(stdin.as_deref(), Some(prompt.as_bytes()));
                    assert!(!args.last().unwrap().contains(prompt));
                }
                PromptDelivery::Arg => {
                    assert!(stdin.is_none());
                    assert!(args.last().unwrap().contains("'  keep this indentation\n'"));
                }
            }
            session.close_all().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nine_concurrent_launches_reserve_only_eight() {
        #[derive(Default)]
        struct YieldingRpc {
            reports: StdMutex2<Vec<String>>,
            updates: StdMutex2<Vec<Vec<Value>>>,
        }
        impl SubagentRpc for YieldingRpc {
            fn report_local_subagent(
                &self,
                report: LocalSubagentReport,
            ) -> SubagentFuture<'_, Result<(), Error>> {
                self.reports.lock().unwrap().push(report.state);
                Box::pin(async {
                    tokio::task::yield_now().await;
                    Ok(())
                })
            }
            fn update_client_tools(
                &self,
                tools: Vec<Value>,
            ) -> SubagentFuture<'_, Result<(), Error>> {
                self.updates.lock().unwrap().push(tools);
                Box::pin(async {
                    tokio::task::yield_now().await;
                    Ok(())
                })
            }
            fn cancel_subagent(&self, _: String) -> SubagentFuture<'_, Result<(), Error>> {
                Box::pin(async { Ok(()) })
            }
        }

        let session = SubagentSession::new(SandboxSession::new());
        let rpc = Arc::new(YieldingRpc::default());
        session.bind(rpc.clone());
        session.set_base_client_tools(Vec::new());
        let (runner, _) = FakeRunner::immediate(FakeOutcome::Pending);
        session.set_runner(runner);
        let tool = Arc::new(
            launch_subagent(
                &session,
                vec![SubagentDef::new("claude", "cat")],
                SubagentLaunch::Local(SandboxTarget::None),
            )
            .into_tool()
            .unwrap(),
        );

        let mut launches = Vec::new();
        for index in 0..9 {
            let tool = tool.clone();
            launches.push(tokio::spawn(async move {
                let raw = tool
                    .call(
                        json!({ "harness": "claude", "model": "m", "prompt": format!("p{index}") }),
                    )
                    .await
                    .unwrap();
                serde_json::from_str::<Value>(raw.as_str().unwrap()).unwrap()
            }));
        }
        let mut started = 0;
        let mut rejected = 0;
        for launch in launches {
            let result = launch.await.unwrap();
            if result["status"] == json!("started") {
                started += 1;
            } else if result["error"]
                .as_str()
                .is_some_and(|error| error.contains("limit reached"))
            {
                rejected += 1;
            }
        }
        assert_eq!(started, 8);
        assert_eq!(rejected, 1);
        assert_eq!(session.inner.tasks.lock().unwrap().len(), 8);
        assert_eq!(rpc.updates.lock().unwrap().len(), 1);
        session.close_all().await;
    }

    #[tokio::test]
    async fn terminal_report_cannot_overtake_delayed_running_report() {
        struct BlockingRunningRpc {
            reports: StdMutex2<Vec<String>>,
            running_entered: Arc<Notify>,
            release_running: Arc<Notify>,
            terminal: Arc<Notify>,
        }
        impl SubagentRpc for BlockingRunningRpc {
            fn report_local_subagent(
                &self,
                report: LocalSubagentReport,
            ) -> SubagentFuture<'_, Result<(), Error>> {
                let state = report.state;
                self.reports.lock().unwrap().push(state.clone());
                let entered = self.running_entered.clone();
                let release = self.release_running.clone();
                let terminal = self.terminal.clone();
                Box::pin(async move {
                    if state == "running" {
                        entered.notify_one();
                        release.notified().await;
                    } else if matches!(state.as_str(), "completed" | "failed" | "cancelled") {
                        terminal.notify_one();
                    }
                    Ok(())
                })
            }
            fn update_client_tools(&self, _: Vec<Value>) -> SubagentFuture<'_, Result<(), Error>> {
                Box::pin(async { Ok(()) })
            }
            fn cancel_subagent(&self, _: String) -> SubagentFuture<'_, Result<(), Error>> {
                Box::pin(async { Ok(()) })
            }
        }

        let session = SubagentSession::new(SandboxSession::new());
        let rpc = Arc::new(BlockingRunningRpc {
            reports: StdMutex2::new(Vec::new()),
            running_entered: Arc::new(Notify::new()),
            release_running: Arc::new(Notify::new()),
            terminal: Arc::new(Notify::new()),
        });
        session.bind(rpc.clone());
        let (runner, _) = FakeRunner::immediate(FakeOutcome::Ok(RunnerOutput {
            stdout: "done".to_string(),
            stderr: String::new(),
            exit_code: 0,
        }));
        session.set_runner(runner);
        let tool = Arc::new(
            launch_subagent(
                &session,
                vec![SubagentDef::new("claude", "cat")],
                SubagentLaunch::Local(SandboxTarget::None),
            )
            .into_tool()
            .unwrap(),
        );
        let launch = {
            let tool = tool.clone();
            tokio::spawn(async move {
                tool.call(json!({ "harness": "claude", "model": "m", "prompt": "p" }))
                    .await
                    .unwrap()
            })
        };
        rpc.running_entered.notified().await;
        tokio::task::yield_now().await;
        assert_eq!(&*rpc.reports.lock().unwrap(), &["start", "running"]);
        rpc.release_running.notify_one();
        launch.await.unwrap();
        rpc.terminal.notified().await;
        assert_eq!(
            &*rpc.reports.lock().unwrap(),
            &["start", "running", "completed"]
        );
        session.close_all().await;
    }

    #[tokio::test]
    async fn production_pipe_collector_retains_only_cap_plus_marker() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let write = tokio::spawn(async move {
            let data = vec![b'x'; SUBAGENT_OUTPUT_CAP_BYTES + 50_000];
            writer.write_all(&data).await.unwrap();
        });
        let retained = read_capped(reader).await.unwrap();
        write.await.unwrap();
        assert_eq!(retained.len(), SUBAGENT_OUTPUT_CAP_BYTES + 1);
    }

    // -----------------------------------------------------------------------
    // 7. Remote-shape safety: no remote-unsandboxed `SubagentLaunch` value is
    //    constructible/expressible — `Remote` always carries an image and
    //    always yields a declaration-only `SubagentTool::Def`, never a
    //    client-dispatched `Tool`.
    // -----------------------------------------------------------------------

    #[test]
    fn remote_launch_is_always_sandboxed_and_never_a_client_tool() {
        let session = SubagentSession::new(SandboxSession::new());
        let tool = launch_subagent(
            &session,
            vec![
                SubagentDef::new("claude", "claude --model {model} --print {prompt}")
                    .with_prompt_via(PromptDelivery::Arg),
            ],
            SubagentLaunch::Remote {
                image: "bae-subagents:latest".to_string(),
            },
        );
        // The ONLY constructible remote shape carries an image; there is no
        // `Remote(Unsandboxed)` value the type permits (Remote's sole field is
        // `image: String` — see the `SubagentLaunch` definition above).
        let def = match tool {
            SubagentTool::Def(d) => d,
            SubagentTool::Tool(_) => panic!("a Remote launch must never yield a client tool"),
        };
        assert_eq!(def.image, "bae-subagents:latest");
        let declaration = def.declaration();
        assert_eq!(declaration["image"], json!("bae-subagents:latest"));
        assert!(declaration["subagents"].as_array().unwrap()[0]
            .get("harness")
            .is_some());
        // `into_tool()` confirms the same value can never be treated as a
        // client-dispatched Tool either.
        let session2 = SubagentSession::new(SandboxSession::new());
        let tool2 = launch_subagent(
            &session2,
            vec![SubagentDef::new("claude", "claude --print {prompt}")
                .with_prompt_via(PromptDelivery::Arg)],
            SubagentLaunch::Remote {
                image: "img".to_string(),
            },
        );
        assert!(tool2.into_tool().is_none());
    }

    #[test]
    #[should_panic(expected = "execRemoteSandbox carries no stdin")]
    fn remote_local_target_rejects_stdin_prompt_delivery() {
        let session = SubagentSession::new(SandboxSession::new());
        let _ = launch_subagent(
            &session,
            vec![SubagentDef::new("claude", "cat")],
            SubagentLaunch::Local(SandboxTarget::Remote),
        );
    }
}
