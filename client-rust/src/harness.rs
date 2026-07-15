//! The agent harness: session lifecycle and the tool-dispatch loop.
//!
//! [`Harness`] holds the [`Config`], the tool registry, and the [`Hooks`]. Its
//! async [`Harness::connect`] opens a session on the server and hands back a
//! [`Session`], whose [`Session::send`] drives the full round-trip described in
//! `docs/client-api.md`'s harness loop:
//!
//! 1. Send the user turn via the `session.sendMessage` JSON-RPC method.
//! 2. If the assistant response has no `tool_use` block, it is final — return it.
//! 3. Otherwise use the server's `dispatch` tag to execute only client-owned
//!    `tool_use` blocks, collect only those `tool_result` blocks, send them
//!    back, and go to step 2. Server-owned blocks remain visible through
//!    [`Hooks::after_receive`], but are informational and never executed.
//!
//! Session open, events replay, and close stay plain REST (`POST`/`GET`/`DELETE`
//! against `/api/v1/sessions…`); only the message loop is JSON-RPC. A
//! `session.sendMessage` request is POSTed to `…/rpc` and the reply is an
//! `application/x-ndjson` stream of JSON-RPC frames: notifications (no `id`)
//! carry live `session.event`s and are handed to [`Hooks::on_event`]; the frame
//! carrying the request `id` is the terminal `{message, events}` result.
//!
//! The transport is abstracted behind a small crate-private [`Transport`] trait
//! so the loop can be unit-tested offline against a mock — the loop logic never
//! touches HTTP directly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Config;
use crate::error::Error;
use crate::hooks::Hooks;
use crate::sandbox::{
    ExecResult, LocalSandboxReport, RemoteSandboxStarted, RemoteSandboxStopped, SandboxFuture,
    SandboxRpc, SandboxSession, SandboxTool, SandboxToolDef,
};
use crate::subagent::{
    LocalSubagentReport, SubagentFuture, SubagentRpc, SubagentSession, SubagentTool,
    SubagentToolDef,
};
use crate::tool::Tool;
use crate::types::{
    Content, ContentBlock, EventView, JsonRpcFrame, JsonRpcRequest, Message, Profile,
    SendMessageParams, SendMessageResult, SubscribeParams, ToolResult,
};

/// The outcome of one `session.sendMessage` turn: the terminal `{message,
/// events}` result plus the live `session.event` notifications observed on the
/// stream (a filtered subset of `result.events` — client-origin events are not
/// broadcast).
pub(crate) struct SendOutcome {
    pub result: SendMessageResult,
    pub notifications: Vec<EventView>,
}

/// The transport the loop drives. Abstracted so tests can mock the server.
///
/// `async fn` in a crate-private trait used only through generics (never as a
/// trait object) is fine; the allow silences the public-API lint pre-emptively.
#[allow(async_fn_in_trait)]
pub(crate) trait Transport {
    /// Drive one `session.sendMessage` turn: POST the JSON-RPC request to
    /// `…/rpc`, stream the NDJSON reply, collect `session.event` notifications,
    /// and return the terminal result. An all-providers-failed turn surfaces as
    /// [`Error::ProvidersFailed`]; a JSON-RPC error object as [`Error::Rpc`].
    async fn send_message(&self, params: &SendMessageParams) -> Result<SendOutcome, Error>;

    /// Drive `session.registerDriver`: register the calling session key as a
    /// driver so `session.sendMessage` is permitted. Issued once during
    /// `connect()`/`join()`, before the caller's first send — application code
    /// must never call it directly. Idempotent server-side.
    async fn register_driver(&self) -> Result<(), Error>;

    /// Drive `session.subscribe`: stream `session.event` notifications to
    /// `on_event` until it returns `false`, the server ends the stream, or an
    /// error frame arrives.
    async fn subscribe(
        &self,
        params: &SubscribeParams,
        on_event: &mut dyn FnMut(&EventView) -> bool,
    ) -> Result<(), Error>;

    /// Drive `session.unsubscribe`: end any active `subscribe` streams for this
    /// session. Returns once the terminal `{unsubscribed:true}` result arrives.
    async fn unsubscribe(&self) -> Result<(), Error>;

    /// Close the session (`DELETE /api/v1/sessions/{id}`).
    async fn close(&self) -> Result<(), Error>;
}

/// Run the tool-dispatch loop to completion, returning the final (no-tool-use)
/// assistant message. Generic over the transport so it is unit-testable.
pub(crate) async fn run_loop<T: Transport>(
    transport: &T,
    tools: &HashMap<String, Tool>,
    hooks: &mut Hooks,
    message: Message,
) -> Result<Message, Error> {
    let mut current = message;
    loop {
        hooks.run_before_send(&mut current).map_err(Error::Hook)?;

        let outcome = transport
            .send_message(&SendMessageParams { message: current })
            .await?;

        // Distribute the live notification stream to the observer hook, in
        // arrival order, before surfacing the terminal turn.
        for event in &outcome.notifications {
            hooks.run_on_event(event).map_err(Error::Hook)?;
        }

        let mut assistant = outcome.result.message;

        hooks
            .run_after_receive(&mut assistant)
            .map_err(Error::Hook)?;

        let tool_uses = assistant.tool_uses();
        if tool_uses.is_empty() {
            // No tool calls: this is the final assistant turn. Loop ends.
            return Ok(assistant);
        }

        let mut result_blocks = Vec::with_capacity(tool_uses.len());
        for mut call in tool_uses {
            // `dispatch` is authoritative whenever a current server supplies
            // it: a client/MCP name collision must still go to the side the
            // server selected. Older servers omit it, so retain the original
            // registry-membership routing as the compatibility fallback.
            let owned_by_client = match call.dispatch.as_deref() {
                Some("client") => true,
                Some(_) => false,
                None => tools.contains_key(&call.name),
            };
            if !owned_by_client {
                // The complete assistant message, including this call, was
                // already exposed via `after_receive` for UI/observability.
                // Server-owned calls must not run client hooks or handlers and
                // must not receive a synthetic tool_result.
                continue;
            }

            hooks.run_before_tool_call(&mut call).map_err(Error::Hook)?;

            let tool = tools
                .get(&call.name)
                // This path is deliberately reachable only for client-owned
                // calls: a dispatch:"client" request can reveal a stale local
                // declaration/handler mismatch, while a server-owned request
                // never becomes an UnknownTool error.
                .ok_or_else(|| Error::UnknownTool(call.name.clone()))?;
            let output = tool
                .call(call.input.clone())
                .await
                .map_err(|source| Error::Tool {
                    name: call.name.clone(),
                    source,
                })?;

            let mut result = ToolResult {
                tool_use_id: call.id.clone(),
                name: call.name.clone(),
                content: output,
            };
            hooks
                .run_after_tool_call(&mut result)
                .map_err(Error::Hook)?;

            result_blocks.push(ContentBlock::ToolResult {
                tool_use_id: result.tool_use_id,
                content: result.content,
            });
        }

        // Feed the tool results back as the next user turn and iterate.
        current = Message::user(Content::Blocks(result_blocks));
    }
}

/// HTTP transport against a real BAE server, authenticated with a session key.
struct HttpTransport {
    http: reqwest::Client,
    base: String,
    session_id: String,
    session_key: String,
    /// Monotonic JSON-RPC request id, unique per session.
    next_id: AtomicU64,
}

impl HttpTransport {
    fn rpc_url(&self) -> String {
        format!("{}/api/v1/sessions/{}/rpc", self.base, self.session_id)
    }

    fn session_url(&self) -> String {
        format!("{}/api/v1/sessions/{}", self.base, self.session_id)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// POST a JSON-RPC request to `…/rpc` and, on a `2xx`, hand back the NDJSON
    /// body reader. A non-2xx status is a pre-stream RFC 7807 error (e.g. `401`).
    async fn open_rpc<P: Serialize>(&self, req: &JsonRpcRequest<P>) -> Result<NdjsonReader, Error> {
        let resp = self
            .http
            .post(self.rpc_url())
            .bearer_auth(&self.session_key)
            .json(req)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let bytes = resp.bytes().await?;
            return Err(Error::Api(parse_problem(status.as_u16(), &bytes)));
        }
        Ok(NdjsonReader::new(resp))
    }
}

impl Transport for HttpTransport {
    async fn send_message(&self, params: &SendMessageParams) -> Result<SendOutcome, Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.sendMessage", params);
        let mut reader = self.open_rpc(&req).await?;

        let mut notifications = Vec::new();
        let mut terminal: Option<SendMessageResult> = None;
        while let Some(frame) = reader.next_frame().await? {
            if frame.id.is_some() {
                // The terminal response: `result` on success, `error` on failure.
                if let Some(err) = frame.error {
                    return Err(Error::Rpc {
                        code: err.code,
                        message: err.message,
                    });
                }
                let result = frame
                    .result
                    .ok_or_else(|| rpc_protocol_error("terminal frame missing `result`"))?;
                terminal = Some(serde_json::from_value(result)?);
                break;
            }
            // A notification (no `id`): a `session.event`, or a mid-stream
            // error notice such as `lagged`.
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            if let Some(event) = frame.into_event()? {
                notifications.push(event);
            }
        }

        let result = terminal
            .ok_or_else(|| rpc_protocol_error("stream ended without a terminal response"))?;

        // The server delivers an all-providers-failed turn as a normal terminal
        // result; recognise it and surface it as ProvidersFailed for continuity.
        if providers_failed(&result.events) {
            return Err(Error::ProvidersFailed {
                events: result.events,
            });
        }

        Ok(SendOutcome {
            result,
            notifications,
        })
    }

    async fn register_driver(&self) -> Result<(), Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.registerDriver", json!({}));
        let mut reader = self.open_rpc(&req).await?;
        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            // The terminal `{registered: true}` result ends the stream.
            if frame.id.is_some() {
                break;
            }
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        params: &SubscribeParams,
        on_event: &mut dyn FnMut(&EventView) -> bool,
    ) -> Result<(), Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.subscribe", params);
        let mut reader = self.open_rpc(&req).await?;

        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            // A terminal `result` (e.g. cancellation) ends the stream.
            if frame.id.is_some() {
                break;
            }
            if let Some(event) = frame.into_event()? {
                if !on_event(&event) {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn unsubscribe(&self) -> Result<(), Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.unsubscribe", json!({}));
        let mut reader = self.open_rpc(&req).await?;

        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            if frame.id.is_some() {
                break;
            }
        }
        Ok(())
    }

    async fn close(&self) -> Result<(), Error> {
        let resp = self
            .http
            .delete(self.session_url())
            .bearer_auth(&self.session_key)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = resp.bytes().await?;
        Err(Error::Api(parse_problem(status.as_u16(), &bytes)))
    }
}

impl HttpTransport {
    /// `session.execRemoteSandbox`: run `command` in the session's live remote
    /// sandbox and return its `{stdout, stderr, exit_code}`. A synchronous
    /// utility call (no turn-loop involvement), like `registerDriver`.
    async fn exec_remote_sandbox_rpc(&self, command: &str) -> Result<ExecResult, Error> {
        let req = JsonRpcRequest::new(
            self.next_id(),
            "session.execRemoteSandbox",
            json!({ "command": command }),
        );
        let mut reader = self.open_rpc(&req).await?;
        let mut terminal: Option<serde_json::Value> = None;
        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            if frame.id.is_some() {
                terminal = Some(
                    frame
                        .result
                        .ok_or_else(|| rpc_protocol_error("execRemoteSandbox missing `result`"))?,
                );
                break;
            }
        }
        let result = terminal
            .ok_or_else(|| rpc_protocol_error("execRemoteSandbox stream ended without a result"))?;
        Ok(serde_json::from_value(result)?)
    }

    /// `session.reportLocalSandbox`: record a local sandbox lifecycle event in
    /// the session's event log.
    async fn report_local_sandbox_rpc(&self, report: &LocalSandboxReport) -> Result<(), Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.reportLocalSandbox", report);
        let mut reader = self.open_rpc(&req).await?;
        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            if frame.id.is_some() {
                break;
            }
        }
        Ok(())
    }

    /// `session.startRemoteSandbox`: ask the server to start the session's one
    /// remote sandbox from `image` (validated against the profile's
    /// `available_sandboxes`) and return its `{sandbox_id, image, started_at}`.
    /// A control-plane call, like `registerDriver` — no turn-loop involvement.
    async fn start_remote_sandbox_rpc(&self, image: &str) -> Result<RemoteSandboxStarted, Error> {
        let req = JsonRpcRequest::new(
            self.next_id(),
            "session.startRemoteSandbox",
            json!({ "image": image }),
        );
        let result = self.rpc_terminal(&req, "startRemoteSandbox").await?;
        Ok(serde_json::from_value(result)?)
    }

    /// `session.stopRemoteSandbox`: stop the session's one remote sandbox and
    /// return `{stopped, image, sandbox_id}`.
    async fn stop_remote_sandbox_rpc(&self) -> Result<RemoteSandboxStopped, Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.stopRemoteSandbox", json!({}));
        let result = self.rpc_terminal(&req, "stopRemoteSandbox").await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Drive a single non-turn JSON-RPC request to its terminal frame and return
    /// the raw `result` value, surfacing an error frame as [`Error::Rpc`]. Shared
    /// by the control-plane sandbox calls.
    async fn rpc_terminal(
        &self,
        req: &JsonRpcRequest<serde_json::Value>,
        label: &str,
    ) -> Result<serde_json::Value, Error> {
        let mut reader = self.open_rpc(req).await?;
        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            if frame.id.is_some() {
                return frame
                    .result
                    .ok_or_else(|| rpc_protocol_error(&format!("{label} missing `result`")));
            }
        }
        Err(rpc_protocol_error(&format!(
            "{label} stream ended without a result"
        )))
    }
}

impl SandboxRpc for HttpTransport {
    fn exec_remote_sandbox(&self, command: String) -> SandboxFuture<'_, Result<ExecResult, Error>> {
        Box::pin(async move { self.exec_remote_sandbox_rpc(&command).await })
    }

    fn report_local_sandbox(
        &self,
        report: LocalSandboxReport,
    ) -> SandboxFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.report_local_sandbox_rpc(&report).await })
    }
}

impl HttpTransport {
    /// `session.reportLocalSubagent`: mirror a local subagent lifecycle
    /// transition into the session's event log. Telemetry, like
    /// `reportLocalSandbox` — no turn-loop involvement.
    async fn report_local_subagent_rpc(&self, report: &LocalSubagentReport) -> Result<(), Error> {
        let req = JsonRpcRequest::new(self.next_id(), "session.reportLocalSubagent", report);
        self.drive_to_terminal(&req).await
    }

    /// `session.updateClientTools`: full-replace this client's advertised
    /// client-tool list (drives the disappearing status tool).
    async fn update_client_tools_rpc(&self, tools: Vec<serde_json::Value>) -> Result<(), Error> {
        let req = JsonRpcRequest::new(
            self.next_id(),
            "session.updateClientTools",
            json!({ "tools": tools }),
        );
        self.drive_to_terminal(&req).await
    }

    /// `session.cancelSubagent`: cancel a **remote** (server-tracked) subagent.
    async fn cancel_subagent_rpc(&self, subagent_id: &str) -> Result<(), Error> {
        let req = JsonRpcRequest::new(
            self.next_id(),
            "session.cancelSubagent",
            json!({ "subagent_id": subagent_id }),
        );
        self.drive_to_terminal(&req).await
    }

    /// Drive a non-turn JSON-RPC request to its terminal frame, surfacing an
    /// error frame as [`Error::Rpc`] and discarding the terminal `result`.
    async fn drive_to_terminal<P: Serialize>(&self, req: &JsonRpcRequest<P>) -> Result<(), Error> {
        let mut reader = self.open_rpc(req).await?;
        while let Some(frame) = reader.next_frame().await? {
            if let Some(err) = frame.error {
                return Err(Error::Rpc {
                    code: err.code,
                    message: err.message,
                });
            }
            if frame.id.is_some() {
                break;
            }
        }
        Ok(())
    }
}

impl SubagentRpc for HttpTransport {
    fn report_local_subagent(
        &self,
        report: LocalSubagentReport,
    ) -> SubagentFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.report_local_subagent_rpc(&report).await })
    }

    fn update_client_tools(
        &self,
        tools: Vec<serde_json::Value>,
    ) -> SubagentFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.update_client_tools_rpc(tools).await })
    }

    fn cancel_subagent(&self, subagent_id: String) -> SubagentFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.cancel_subagent_rpc(&subagent_id).await })
    }
}

/// Does this turn's event list mark an all-providers-failed outcome? The server
/// no longer returns a `502`: the failure turn arrives as a normal terminal
/// result, distinguished only by a `session.error`/`all_providers_failed` event.
fn providers_failed(events: &[EventView]) -> bool {
    events.iter().any(|e| {
        e.event_type == "session.error"
            && e.payload.get("reason").and_then(|r| r.as_str()) == Some("all_providers_failed")
    })
}

/// A synthetic [`Error::Rpc`] for a well-formed-HTTP-but-malformed-stream case.
fn rpc_protocol_error(message: &str) -> Error {
    Error::Rpc {
        code: -32603,
        message: message.to_string(),
    }
}

impl JsonRpcFrame {
    /// Decode a `session.event` notification's `params` into an [`EventView`].
    /// Returns `Ok(None)` for any other (ignorable) notification.
    fn into_event(self) -> Result<Option<EventView>, Error> {
        if self.method.as_deref() != Some("session.event") {
            return Ok(None);
        }
        match self.params {
            Some(params) => Ok(Some(serde_json::from_value(params)?)),
            None => Ok(None),
        }
    }
}

/// Reads newline-delimited JSON-RPC frames from a streaming response body,
/// using `chunk()` (no extra reqwest features) and buffering partial lines.
struct NdjsonReader {
    resp: reqwest::Response,
    buf: Vec<u8>,
    done: bool,
}

impl NdjsonReader {
    fn new(resp: reqwest::Response) -> Self {
        Self {
            resp,
            buf: Vec::new(),
            done: false,
        }
    }

    /// Yield the next frame, or `None` at end of stream. Blank lines are skipped.
    async fn next_frame(&mut self) -> Result<Option<JsonRpcFrame>, Error> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                return Ok(Some(serde_json::from_slice(line)?));
            }
            if self.done {
                if self.buf.iter().all(u8::is_ascii_whitespace) {
                    self.buf.clear();
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.buf);
                return Ok(Some(serde_json::from_slice(&line)?));
            }
            match self.resp.chunk().await? {
                Some(bytes) => self.buf.extend_from_slice(&bytes),
                None => self.done = true,
            }
        }
    }
}

/// Parse an RFC 7807 body, falling back to a synthetic problem doc if the
/// server sent something unexpected.
fn parse_problem(status: u16, bytes: &[u8]) -> crate::types::ApiError {
    serde_json::from_slice(bytes).unwrap_or_else(|_| crate::types::ApiError {
        kind: "unknown".to_string(),
        title: "unexpected error response".to_string(),
        status,
        detail: String::from_utf8_lossy(bytes).into_owned(),
    })
}

/// `POST /api/v1/sessions` request body.
#[derive(Debug, Serialize)]
struct OpenRequest {
    client_version: String,
    tools: Vec<serde_json::Value>,
    /// Auto-mode sandbox tool declarations (part D); omitted when none are
    /// registered, so the field is invisible to servers/harnesses not using it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sandbox_tools: Vec<serde_json::Value>,
    /// Remote-launch subagent declarations; omitted when none are registered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subagent_tools: Vec<serde_json::Value>,
}

/// `POST /api/v1/sessions` success body.
#[derive(Debug, Deserialize)]
struct OpenResponse {
    session_id: String,
    session_key: String,
    profile: Profile,
}

/// The agent harness: configuration + a tool registry + lifecycle hooks.
///
/// Build one, register tools and hooks, then [`connect`](Harness::connect) to
/// open a session.
///
/// ```no_run
/// use bae_rs::{Config, Harness, Tool};
/// use serde_json::json;
///
/// # async fn run() -> Result<(), bae_rs::Error> {
/// let mut session = Harness::new(Config::new("http://localhost:8080", "bae_…"))
///     .with_tool(Tool::new("noop", "does nothing", json!({}), |_| async move { Ok(json!("ok")) }))
///     .connect()
///     .await?;
/// let reply = session.send("hello").await?;
/// println!("{}", reply.text());
/// session.close().await?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Harness {
    config: Config,
    http: reqwest::Client,
    tools: HashMap<String, Tool>,
    hooks: Hooks,
    /// The late-bound sandbox handle shared with every sandbox tool and the
    /// eventual [`Session`]; its transport is filled in [`Harness::open`].
    sandbox: SandboxSession,
    /// Auto-mode sandbox tool declarations, sent in the session-open
    /// `sandbox_tools` array.
    sandbox_defs: Vec<SandboxToolDef>,
    /// The late-bound subagent handle shared with every subagent tool and the
    /// eventual [`Session`]; its transport is filled in [`Harness::open`].
    subagent: SubagentSession,
    /// Remote-launch subagent declarations, sent in the session-open
    /// `subagent_tools` array.
    subagent_defs: Vec<SubagentToolDef>,
}

impl Harness {
    /// Create a harness from a [`Config`], with an empty tool registry and no
    /// hooks.
    pub fn new(config: Config) -> Self {
        let sandbox = SandboxSession::new();
        let subagent = SubagentSession::new(sandbox.clone());
        Self {
            config,
            http: reqwest::Client::new(),
            tools: HashMap::new(),
            hooks: Hooks::default(),
            sandbox,
            sandbox_defs: Vec::new(),
            subagent,
            subagent_defs: Vec::new(),
        }
    }

    /// A handle to this harness's sandbox capability, for building sandbox tools
    /// **before** `connect()`. Its transport is late-bound at connect; see
    /// [`crate::sandbox`] for the required ordering. Pass the handle to
    /// [`run_shell_command`](crate::sandbox::run_shell_command) /
    /// [`run_shell_named`](crate::sandbox::run_shell_named), then register the
    /// result with [`with_sandbox_tool`](Harness::with_sandbox_tool).
    pub fn sandbox_session(&self) -> SandboxSession {
        self.sandbox.clone()
    }

    /// Register a builtin sandbox tool, routing it to the correct place: a
    /// client-dispatched [`SandboxTool::Tool`] joins the ordinary tool registry;
    /// an Auto-mode [`SandboxTool::Def`] joins the session-open `sandbox_tools`
    /// list. Builder-style; returns `self`.
    pub fn with_sandbox_tool(mut self, tool: SandboxTool) -> Self {
        self.register_sandbox_tool(tool);
        self
    }

    /// Register a builtin sandbox tool in place (non-consuming). See
    /// [`with_sandbox_tool`](Harness::with_sandbox_tool).
    pub fn register_sandbox_tool(&mut self, tool: SandboxTool) -> &mut Self {
        match tool {
            SandboxTool::Tool(t) => {
                self.tools.insert(t.name.clone(), t);
            }
            SandboxTool::Def(d) => {
                self.sandbox_defs.push(d);
            }
        }
        self
    }

    /// A handle to this harness's subagent capability, for building subagent
    /// tools **before** `connect()`. Its transport is late-bound at connect; see
    /// [`crate::subagent`] for the required ordering. Pass the handle to
    /// [`launch_subagent`](crate::subagent::launch_subagent), then register the
    /// result with [`with_subagent_tool`](Harness::with_subagent_tool).
    pub fn subagent_session(&self) -> SubagentSession {
        self.subagent.clone()
    }

    /// Register a builtin subagent tool, routing it to the correct place: a
    /// client-dispatched [`SubagentTool::Tool`] (a `Local` launch) joins the
    /// ordinary tool registry — and pulls in the automatic `local_subagent_status`
    /// tool at connect — while a [`SubagentTool::Def`] (a `Remote` launch) joins
    /// the session-open `subagent_tools` list. Builder-style; returns `self`.
    pub fn with_subagent_tool(mut self, tool: SubagentTool) -> Self {
        self.register_subagent_tool(tool);
        self
    }

    /// Register a builtin subagent tool in place (non-consuming). See
    /// [`with_subagent_tool`](Harness::with_subagent_tool).
    pub fn register_subagent_tool(&mut self, tool: SubagentTool) -> &mut Self {
        match tool {
            SubagentTool::Tool(t) => {
                self.tools.insert(t.name.clone(), t);
            }
            SubagentTool::Def(d) => {
                self.subagent_defs.push(d);
            }
        }
        self
    }

    /// Register a client-side tool. A later tool with the same name replaces an
    /// earlier one. Builder-style; returns `self`.
    pub fn with_tool(mut self, tool: Tool) -> Self {
        self.tools.insert(tool.name.clone(), tool);
        self
    }

    /// Register a client-side tool in place (non-consuming).
    pub fn register_tool(&mut self, tool: Tool) -> &mut Self {
        self.tools.insert(tool.name.clone(), tool);
        self
    }

    /// Replace the lifecycle hooks. Builder-style; returns `self`.
    pub fn with_hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// The names of all registered tools (unordered).
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Open a new session on the server and return a [`Session`] handle.
    ///
    /// Sends the configured `client_version` and every registered tool's
    /// declaration; the server validates each name against the profile's
    /// `allowed_tools`. Consumes the harness, moving the tool registry and
    /// hooks into the session. Registers this connection as a driver
    /// (`session.registerDriver`) before returning, so the first
    /// [`send`](Session::send) is permitted.
    pub async fn connect(self) -> Result<Session, Error> {
        let url = format!("{}/api/v1/sessions", self.config.base());
        self.open(url).await
    }

    /// Join an **existing** session as an additional driver and return a
    /// [`Session`] handle shaped identically to [`connect`](Harness::connect)'s.
    ///
    /// POSTs to `/api/v1/sessions/{session_id}/join` with this harness's
    /// `client_version` and registered tool declarations (a joining client
    /// declares its own, independent tool set, validated against the *same*
    /// profile's `allowed_tools`). The joining client key must resolve to the
    /// same profile as the session, or the server rejects with
    /// `403 profile_mismatch`. Like [`connect`](Harness::connect), registers
    /// this connection as a driver before returning.
    pub async fn join(self, session_id: impl AsRef<str>) -> Result<Session, Error> {
        let url = format!(
            "{}/api/v1/sessions/{}/join",
            self.config.base(),
            session_id.as_ref()
        );
        self.open(url).await
    }

    /// Shared body of [`connect`](Harness::connect) and [`join`](Harness::join):
    /// POST the declared tools to `url` with client-key auth, then register as a
    /// driver before handing back the [`Session`]. Both endpoints return the
    /// identical `{session_id, session_key, profile}` shape.
    async fn open(self, url: String) -> Result<Session, Error> {
        let Harness {
            config,
            http,
            mut tools,
            hooks,
            sandbox,
            sandbox_defs,
            subagent,
            subagent_defs,
        } = self;

        let body = OpenRequest {
            client_version: config.client_version.clone(),
            tools: tools.values().map(Tool::declaration).collect(),
            sandbox_tools: sandbox_defs
                .iter()
                .map(SandboxToolDef::declaration)
                .collect(),
            subagent_tools: subagent_defs
                .iter()
                .map(SubagentToolDef::declaration)
                .collect(),
        };

        let resp = http
            .post(url)
            .bearer_auth(&config.client_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(Error::Api(parse_problem(status.as_u16(), &bytes)));
        }
        let open: OpenResponse = serde_json::from_slice(&bytes)?;

        let transport = Arc::new(HttpTransport {
            http,
            base: config.base().to_string(),
            session_id: open.session_id.clone(),
            session_key: open.session_key,
            next_id: AtomicU64::new(1),
        });

        // Late-bind the sandbox transport now that the session exists, so any
        // sandbox tool built pre-connect can reach `session.execRemoteSandbox` /
        // `session.reportLocalSandbox` the first time it fires.
        sandbox.bind(transport.clone() as Arc<dyn SandboxRpc>);
        subagent.bind(transport.clone() as Arc<dyn SubagentRpc>);

        // If a Local launch tool was registered, wire the automatic
        // `local_subagent_status` tool: capture the declared client-tool list
        // (so `session.updateClientTools` can full-replace it) and register the
        // status tool for **dispatch only** — it is advertised dynamically, never
        // in the session-open `tools` array.
        if subagent.has_local() {
            subagent.set_base_client_tools(body.tools.clone());
            let status = subagent.status_tool();
            tools.insert(status.name.clone(), status);
        }

        // Register as a driver before any send: session.sendMessage requires it
        // (a `-32001` error otherwise). Application code never calls this.
        transport.register_driver().await?;

        Ok(Session {
            transport,
            session_id: open.session_id,
            profile: open.profile,
            tools,
            hooks,
            sandbox,
            subagent,
        })
    }
}

/// A live session handle. Created by [`Harness::connect`].
///
/// The session key is held internally and never exposed; drive the agent with
/// [`send`](Session::send) and release it with [`close`](Session::close).
pub struct Session {
    transport: Arc<HttpTransport>,
    session_id: String,
    profile: Profile,
    tools: HashMap<String, Tool>,
    hooks: Hooks,
    sandbox: SandboxSession,
    subagent: SubagentSession,
}

impl Session {
    /// The server-assigned session id (`ses_…`).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The sanitized profile the session opened against.
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Send a message and drive the full tool-dispatch loop, returning the
    /// final assistant turn (the first response with no `tool_use` block).
    ///
    /// Accepts anything that converts into a [`Message`] — a `&str`/`String`
    /// becomes a user text turn.
    pub async fn send(&mut self, message: impl Into<Message>) -> Result<Message, Error> {
        run_loop(
            &*self.transport,
            &self.tools,
            &mut self.hooks,
            message.into(),
        )
        .await
    }

    /// A clone of this session's [`SandboxSession`] handle — for building
    /// sandbox tools **after** connect (an alternative to
    /// [`Harness::sandbox_session`]). Tools built from it must be registered on
    /// this session with [`register_tool`](Session::register_tool); Auto-mode
    /// declarations cannot be added post-open and must instead be registered on
    /// the [`Harness`] before connect.
    pub fn sandbox_session(&self) -> SandboxSession {
        self.sandbox.clone()
    }

    /// Register an additional client-dispatched tool on the live session (e.g. a
    /// local or remote-manual sandbox tool built after connect). A later tool
    /// with the same name replaces an earlier one.
    pub fn register_tool(&mut self, tool: Tool) -> &mut Self {
        self.tools.insert(tool.name.clone(), tool);
        self
    }

    /// Run `command` in the session's remote sandbox via
    /// `session.execRemoteSandbox`. Available to any tool handler or application
    /// code holding a [`SandboxSession`].
    pub async fn exec_remote_sandbox(&self, command: &str) -> Result<ExecResult, Error> {
        self.sandbox.exec_remote_sandbox(command).await
    }

    /// Report a local sandbox lifecycle transition via
    /// `session.reportLocalSandbox` (`state` ∈ `running`/`stopped`/`error`).
    pub async fn report_local_sandbox(
        &self,
        state: &str,
        image: impl crate::sandbox::IntoLocalSandboxImage,
        container_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), Error> {
        self.sandbox
            .report_local_sandbox(state, image, container_id, detail)
            .await
    }

    /// Eagerly start a local sandbox for `image` (otherwise it starts lazily on
    /// the first local-target tool call), reporting `running` to the server.
    pub async fn start_local_sandbox(&self, image: &str) -> Result<(), Error> {
        self.sandbox.start_local(image).await.map(|_| ())
    }

    /// Stop every local sandbox this session started, reporting `stopped`.
    pub async fn stop_local_sandbox(&self) {
        self.sandbox.stop_all_local().await
    }

    /// Ask the server to start this session's **remote** sandbox from `image`
    /// (`session.startRemoteSandbox`). `image` must be in the session profile's
    /// `available_sandboxes`, or the call fails with [`Error::Rpc`] code
    /// `-32011`. One sandbox per session: a second start while one is running
    /// fails with `-32000`. Required before any `Remote`-target tool
    /// (Auto-dispatched or [`exec_remote_sandbox`](Session::exec_remote_sandbox))
    /// can run.
    pub async fn start_remote_sandbox(&self, image: &str) -> Result<RemoteSandboxStarted, Error> {
        self.transport.start_remote_sandbox_rpc(image).await
    }

    /// Stop this session's remote sandbox (`session.stopRemoteSandbox`). Fails
    /// with [`Error::Rpc`] code `-32013` if none is running. (Session close also
    /// stops a still-running remote sandbox server-side.)
    pub async fn stop_remote_sandbox(&self) -> Result<RemoteSandboxStopped, Error> {
        self.transport.stop_remote_sandbox_rpc().await
    }

    /// A clone of this session's [`SubagentSession`] handle — for building
    /// subagent tools after connect, or driving cancellation directly. Note a
    /// `Local` launch tool must be built and registered on the [`Harness`]
    /// **before** connect (it is declared in the session-open `tools` array).
    pub fn subagent_session(&self) -> SubagentSession {
        self.subagent.clone()
    }

    /// Cancel a **local** subagent in-process (`Session::cancel_subagent`):
    /// abort its background task (killing the child), mark it `Cancelled`, and
    /// report `cancelled{reason:"explicit"}`. Idempotent on a terminal/unknown
    /// id. Does not touch remote subagents — use
    /// [`cancel_remote_subagent`](Session::cancel_remote_subagent) for those.
    pub async fn cancel_subagent(&self, subagent_id: &str) {
        self.subagent.cancel_subagent(subagent_id).await
    }

    /// Cancel a **remote** (server-tracked) subagent via `session.cancelSubagent`.
    pub async fn cancel_remote_subagent(&self, subagent_id: &str) -> Result<(), Error> {
        self.transport.cancel_subagent_rpc(subagent_id).await
    }

    /// Subscribe to this session's live `session.event` feed via the
    /// `session.subscribe` JSON-RPC method, invoking `on_event` for each event
    /// in order. With a `since_event_id`, the server first replays persisted
    /// events after that id, then streams live ones **indefinitely**.
    ///
    /// The stream is open-ended: return `false` from `on_event` to stop reading
    /// (dropping the connection ends the subscription server-side), or call
    /// [`unsubscribe`](Session::unsubscribe) from another task. Returns once the
    /// stream ends.
    pub async fn subscribe<F>(
        &self,
        since_event_id: Option<&str>,
        mut on_event: F,
    ) -> Result<(), Error>
    where
        F: FnMut(&EventView) -> bool,
    {
        let params = SubscribeParams {
            since_event_id: since_event_id.map(str::to_string),
        };
        self.transport.subscribe(&params, &mut on_event).await
    }

    /// End any active [`subscribe`](Session::subscribe) streams for this session
    /// via `session.unsubscribe`.
    pub async fn unsubscribe(&self) -> Result<(), Error> {
        self.transport.unsubscribe().await
    }

    /// Close the session on the server (idempotent from the caller's view; a
    /// second close returns a `session_closed` [`Error::Api`]).
    ///
    /// Before releasing the session, kills any still-running **local**
    /// subagents (reporting `cancelled{reason:"session_close"}` for each and
    /// retiring the status tool), then stops any still-running **local**
    /// sandboxes this session started — mirroring how the server tears down its
    /// own remote subagents and sandbox at session close.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.subagent.close_all().await;
        self.sandbox.stop_all_local().await;
        self.transport.close().await
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("session_id", &self.session_id)
            .field("profile", &self.profile)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("hooks", &self.hooks)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::Tool;
    use crate::types::{ApiError, ToolUse};
    use serde_json::json;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    /// A scripted transport: returns queued outcomes in order, records each
    /// request it received, and never touches the network.
    struct MockTransport {
        // RefCell is fine: the loop awaits sequentially on one task/thread.
        responses: RefCell<Vec<Result<SendOutcome, Error>>>,
        sent: RefCell<Vec<SendMessageParams>>,
        closed: RefCell<bool>,
        /// How many times `register_driver` was called — connect()/join() each
        /// call it exactly once during setup.
        driver_registrations: RefCell<usize>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<SendOutcome, Error>>) -> Self {
            Self {
                responses: RefCell::new(responses),
                sent: RefCell::new(Vec::new()),
                closed: RefCell::new(false),
                driver_registrations: RefCell::new(0),
            }
        }
    }

    impl Transport for MockTransport {
        async fn send_message(&self, params: &SendMessageParams) -> Result<SendOutcome, Error> {
            self.sent.borrow_mut().push(SendMessageParams {
                message: params.message.clone(),
            });
            if self.responses.borrow().is_empty() {
                panic!("mock transport ran out of scripted responses");
            }
            self.responses.borrow_mut().remove(0)
        }

        async fn register_driver(&self) -> Result<(), Error> {
            *self.driver_registrations.borrow_mut() += 1;
            Ok(())
        }

        async fn subscribe(
            &self,
            _params: &SubscribeParams,
            _on_event: &mut dyn FnMut(&EventView) -> bool,
        ) -> Result<(), Error> {
            Ok(())
        }

        async fn unsubscribe(&self) -> Result<(), Error> {
            Ok(())
        }

        async fn close(&self) -> Result<(), Error> {
            *self.closed.borrow_mut() = true;
            Ok(())
        }
    }

    fn assistant_text(text: &str) -> SendOutcome {
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: text.to_string(),
                }]),
                events: vec![],
            },
            notifications: vec![],
        }
    }

    fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> SendOutcome {
        assistant_tool_uses(vec![tool_use(id, name, input, None)])
    }

    fn tool_use(
        id: &str,
        name: &str,
        input: serde_json::Value,
        dispatch: Option<&str>,
    ) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
            dispatch: dispatch.map(str::to_string),
        }
    }

    fn assistant_tool_uses(tool_uses: Vec<ContentBlock>) -> SendOutcome {
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(tool_uses),
                events: vec![],
            },
            notifications: vec![],
        }
    }

    fn time_tool() -> Tool {
        Tool::new(
            "get_current_time",
            "current time",
            json!({}),
            |_input| async move { Ok(json!("2026-07-06T00:00:00Z")) },
        )
    }

    fn registry(tools: Vec<Tool>) -> HashMap<String, Tool> {
        tools.into_iter().map(|t| (t.name.clone(), t)).collect()
    }

    #[tokio::test]
    async fn plain_text_turn_ends_loop_without_tool_calls() {
        let transport = MockTransport::new(vec![Ok(assistant_text("hello there"))]);
        let tools = registry(vec![]);
        let mut hooks = Hooks::default();

        let out = run_loop(&transport, &tools, &mut hooks, Message::user("hi"))
            .await
            .unwrap();

        assert_eq!(out.text(), "hello there");
        // Exactly one round-trip: the loop stopped as soon as no tool_use appeared.
        assert_eq!(transport.sent.borrow().len(), 1);
    }

    #[tokio::test]
    async fn no_dispatch_falls_back_to_registered_tool_membership() {
        // Turn 1: model asks for the tool. Turn 2: model replies with text.
        let transport = MockTransport::new(vec![
            Ok(assistant_tool_use("tu_1", "get_current_time", json!({}))),
            Ok(assistant_text("the time is noon")),
        ]);
        let tools = registry(vec![time_tool()]);
        let mut hooks = Hooks::default();

        let out = run_loop(
            &transport,
            &tools,
            &mut hooks,
            Message::user("what time is it"),
        )
        .await
        .unwrap();

        assert_eq!(out.text(), "the time is noon");

        let sent = transport.sent.borrow();
        assert_eq!(sent.len(), 2, "one user turn + one tool_result turn");

        // The second request must carry a tool_result echoing the tool_use id.
        match &sent[1].message.content {
            Content::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => {
                    assert_eq!(tool_use_id, "tu_1");
                    assert_eq!(content, &json!("2026-07-06T00:00:00Z"));
                }
                other => panic!("expected tool_result block, got {other:?}"),
            },
            other => panic!("expected block content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hooks_fire_in_order_across_the_round_trip() {
        let transport = MockTransport::new(vec![
            Ok(assistant_tool_use("tu_1", "get_current_time", json!({}))),
            Ok(assistant_text("done")),
        ]);
        let tools = registry(vec![time_tool()]);

        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let (l1, l2, l3, l4) = (log.clone(), log.clone(), log.clone(), log.clone());
        let mut hooks = Hooks::default()
            .before_send(move |_m| {
                l1.lock().unwrap().push("before_send".into());
                Ok(())
            })
            .after_receive(move |_m| {
                l2.lock().unwrap().push("after_receive".into());
                Ok(())
            })
            .before_tool_call(move |c| {
                l3.lock()
                    .unwrap()
                    .push(format!("before_tool_call:{}", c.name));
                Ok(())
            })
            .after_tool_call(move |r| {
                l4.lock()
                    .unwrap()
                    .push(format!("after_tool_call:{}", r.name));
                Ok(())
            });

        run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap();

        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                // Turn 1: send, receive tool_use, dispatch tool.
                "before_send".to_string(),
                "after_receive".to_string(),
                "before_tool_call:get_current_time".to_string(),
                "after_tool_call:get_current_time".to_string(),
                // Turn 2: send tool_result, receive final text.
                "before_send".to_string(),
                "after_receive".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn after_tool_call_hook_can_rewrite_the_result() {
        let transport = MockTransport::new(vec![
            Ok(assistant_tool_use("tu_1", "get_current_time", json!({}))),
            Ok(assistant_text("ok")),
        ]);
        let tools = registry(vec![time_tool()]);
        let mut hooks = Hooks::default().after_tool_call(|r| {
            r.content = json!("REDACTED");
            Ok(())
        });

        run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap();

        let sent = transport.sent.borrow();
        match &sent[1].message.content {
            Content::Blocks(blocks) => match &blocks[0] {
                ContentBlock::ToolResult { content, .. } => {
                    assert_eq!(content, &json!("REDACTED"));
                }
                other => panic!("expected tool_result, got {other:?}"),
            },
            other => panic!("expected blocks, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hook_error_aborts_the_loop() {
        let transport = MockTransport::new(vec![Ok(assistant_text("never reached"))]);
        let tools = registry(vec![]);
        let mut hooks = Hooks::default().before_send(|_m| Err("boom".into()));

        let err = run_loop(&transport, &tools, &mut hooks, Message::user("hi"))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::Hook(_)));
        // Aborted before any request went out.
        assert_eq!(transport.sent.borrow().len(), 0);
    }

    #[tokio::test]
    async fn client_dispatch_without_handler_raises_unknown_tool() {
        let transport = MockTransport::new(vec![Ok(assistant_tool_uses(vec![tool_use(
            "tu_1",
            "mystery",
            json!({}),
            Some("client"),
        )]))]);
        let tools = registry(vec![time_tool()]); // "mystery" not registered
        let mut hooks = Hooks::default();

        let err = run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap_err();

        match err {
            Error::UnknownTool(name) => assert_eq!(name, "mystery"),
            other => panic!("expected UnknownTool, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn null_dispatch_falls_back_to_registered_tool_membership() {
        let assistant: Message = serde_json::from_value(json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "tu_null",
                "name": "get_current_time",
                "input": {},
                "dispatch": null
            }]
        }))
        .unwrap();
        let transport = MockTransport::new(vec![
            Ok(SendOutcome {
                result: SendMessageResult {
                    message: assistant,
                    events: vec![],
                },
                notifications: vec![],
            }),
            Ok(assistant_text("done")),
        ]);
        let tools = registry(vec![time_tool()]);
        let mut hooks = Hooks::default();

        run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap();

        let sent = transport.sent.borrow();
        let Content::Blocks(blocks) = &sent[1].message.content else {
            panic!("expected tool_result blocks");
        };
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tu_null"
        ));
    }

    #[tokio::test]
    async fn dispatch_wins_over_registry_membership_for_same_name_collision() {
        let transport = MockTransport::new(vec![
            Ok(assistant_tool_uses(vec![
                tool_use(
                    "tu_server",
                    "get_current_time",
                    json!({ "owner": "server" }),
                    Some("mcp"),
                ),
                tool_use(
                    "tu_client",
                    "get_current_time",
                    json!({ "owner": "client" }),
                    Some("client"),
                ),
            ])),
            Ok(assistant_text("done")),
        ]);
        let tools = registry(vec![time_tool()]);
        let mut hooks = Hooks::default();

        run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap();

        let sent = transport.sent.borrow();
        let Content::Blocks(blocks) = &sent[1].message.content else {
            panic!("expected tool_result blocks");
        };
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tu_client"
        ));
    }

    #[tokio::test]
    async fn mixed_dispatch_executes_only_client_result_and_surfaces_server_tool() {
        // `issue_read` has no client handler. Its MCP tag must make it
        // informational rather than an UnknownTool error.
        let transport = MockTransport::new(vec![
            Ok(assistant_tool_uses(vec![
                tool_use("tu_mcp", "issue_read", json!({ "id": 9 }), Some("mcp")),
                tool_use("tu_client", "get_current_time", json!({}), Some("client")),
            ])),
            Ok(assistant_text("done")),
        ]);
        let tools = registry(vec![time_tool()]);

        // `after_receive` is the informational surface: it sees the full
        // assistant turn, including server-owned blocks that will not run.
        let informational = Arc::new(Mutex::new(Vec::<ToolUse>::new()));
        let sink = informational.clone();
        let mut hooks = Hooks::default().after_receive(move |message| {
            sink.lock().unwrap().extend(
                message.tool_uses().into_iter().filter(|call| {
                    matches!(call.dispatch.as_deref(), Some("mcp") | Some("sandbox"))
                }),
            );
            Ok(())
        });

        let out = run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap();
        assert_eq!(out.text(), "done");

        let sent = transport.sent.borrow();
        assert_eq!(sent.len(), 2, "initial turn plus client-only tool result");
        let Content::Blocks(blocks) = &sent[1].message.content else {
            panic!("expected tool_result blocks");
        };
        assert_eq!(blocks.len(), 1, "server-owned calls have no client result");
        match &blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                assert_eq!(tool_use_id, "tu_client");
                assert_eq!(content, &json!("2026-07-06T00:00:00Z"));
            }
            other => panic!("expected tool_result block, got {other:?}"),
        }
        drop(sent);

        let observed = informational.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].id, "tu_mcp");
        assert_eq!(observed[0].name, "issue_read");
        assert_eq!(observed[0].dispatch.as_deref(), Some("mcp"));
    }

    #[tokio::test]
    async fn tool_handler_error_propagates() {
        let failing = Tool::new("boom", "always fails", json!({}), |_| async move {
            Err("handler exploded".into())
        });
        let transport = MockTransport::new(vec![Ok(assistant_tool_use("tu_1", "boom", json!({})))]);
        let tools = registry(vec![failing]);
        let mut hooks = Hooks::default();

        let err = run_loop(&transport, &tools, &mut hooks, Message::user("go"))
            .await
            .unwrap_err();

        match err {
            Error::Tool { name, source } => {
                assert_eq!(name, "boom");
                assert_eq!(source.to_string(), "handler exploded");
            }
            other => panic!("expected Tool error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn providers_failed_error_propagates() {
        let transport = MockTransport::new(vec![Err(Error::ProvidersFailed { events: vec![] })]);
        let tools = registry(vec![]);
        let mut hooks = Hooks::default();

        let err = run_loop(&transport, &tools, &mut hooks, Message::user("hi"))
            .await
            .unwrap_err();

        assert!(matches!(err, Error::ProvidersFailed { .. }));
    }

    #[test]
    fn api_error_displays_slug_status_and_detail() {
        let e = ApiError {
            kind: "tool_not_allowed".into(),
            title: "Tool not allowed".into(),
            status: 403,
            detail: "get_current_time is not permitted".into(),
        };
        assert_eq!(
            e.to_string(),
            "tool_not_allowed (403): get_current_time is not permitted"
        );
    }

    // -----------------------------------------------------------------------
    // Cross-SDK MCP parity
    //
    // The three client SDKs (Rust, TypeScript, Python) must observe an IDENTICAL
    // ordered live event sequence for the same scripted MCP-enabled turn, and
    // parse the real (non-stub) mcp.request / mcp.response payload shapes. The
    // canonical sequence below MUST stay byte-for-byte identical to the arrays
    // in the TypeScript and Python SDK parity tests:
    //   - client-typescript/src/harness.test.ts   (MCP_PARITY_SEQUENCE)
    //   - client-python/tests/test_mcp_parity.py   (MCP_PARITY_SEQUENCE)
    // -----------------------------------------------------------------------

    /// The canonical live-notification sequence for the scripted MCP turn.
    const MCP_PARITY_SEQUENCE: [&str; 9] = [
        "provider.request",
        "provider.response",
        "tool.call",
        "mcp.request",
        "mcp.response",
        "tool.result",
        "provider.request",
        "provider.response",
        "server.message.send",
    ];

    fn parity_event(event_type: &str, payload: serde_json::Value) -> EventView {
        EventView {
            id: format!("evt_{event_type}"),
            session_id: "ses_test".into(),
            client_key_id: None,
            event_type: event_type.into(),
            payload,
            created_at: "t".into(),
        }
    }

    /// The scripted MCP turn: the live notifications, then a terminal text turn.
    fn mcp_scenario_outcome() -> SendOutcome {
        let echo = json!([{ "type": "text", "text": "echo: x" }]);
        let notifications = vec![
            parity_event("provider.request", json!({ "attempt": 0 })),
            parity_event("provider.response", json!({ "ok": true, "status": 200 })),
            parity_event(
                "tool.call",
                json!({ "dispatch": "mcp", "name": "remote_search", "server_name": "echo", "input": { "q": "x" } }),
            ),
            parity_event(
                "mcp.request",
                json!({ "method": "tools/call", "server_name": "echo", "tool": "remote_search", "input": { "q": "x" } }),
            ),
            parity_event(
                "mcp.response",
                json!({ "server_name": "echo", "ok": true, "result": { "content": echo, "isError": false } }),
            ),
            parity_event(
                "tool.result",
                json!({ "tool_use_id": "tu_mcp", "dispatch": "mcp", "server_name": "echo", "is_error": false, "content": echo }),
            ),
            parity_event("provider.request", json!({ "attempt": 0 })),
            parity_event("provider.response", json!({ "ok": true, "status": 200 })),
            parity_event(
                "server.message.send",
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "after mcp" }] }),
            ),
        ];
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "after mcp".into(),
                }]),
                events: vec![],
            },
            notifications,
        }
    }

    #[tokio::test]
    async fn mcp_scenario_matches_canonical_sequence_and_parses_real_payloads() {
        let transport = MockTransport::new(vec![Ok(mcp_scenario_outcome())]);
        let tools = registry(vec![]);

        // Collect (event_type, payload) for each observed live notification.
        let observed = Arc::new(Mutex::new(Vec::<(String, serde_json::Value)>::new()));
        let sink = observed.clone();
        let mut hooks = Hooks::default().on_event(move |ev| {
            sink.lock()
                .unwrap()
                .push((ev.event_type.clone(), ev.payload.clone()));
            Ok(())
        });

        // MCP tools are dispatched server-side, so the loop ends after one turn.
        let out = run_loop(
            &transport,
            &tools,
            &mut hooks,
            Message::user("search please"),
        )
        .await
        .unwrap();
        assert_eq!(out.text(), "after mcp");

        let events = observed.lock().unwrap();
        let seq: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(seq, MCP_PARITY_SEQUENCE);

        // Real (non-stub) mcp.request / mcp.response payloads parse to shape.
        let req_payload = &events.iter().find(|(t, _)| t == "mcp.request").unwrap().1;
        let req: crate::McpRequestPayload = serde_json::from_value(req_payload.clone()).unwrap();
        assert_eq!(req.method, "tools/call");
        assert_eq!(req.server_name.as_deref(), Some("echo"));
        assert_eq!(req.tool, "remote_search");
        assert_eq!(req.input, json!({ "q": "x" }));

        let resp_payload = &events.iter().find(|(t, _)| t == "mcp.response").unwrap().1;
        let resp: crate::McpResponsePayload = serde_json::from_value(resp_payload.clone()).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap()["content"][0]["text"], json!("echo: x"));
        assert!(resp.error.is_none());

        // No trace of the removed stub payload shape.
        assert!(events
            .iter()
            .all(|(_, p)| p.get("status").and_then(|s| s.as_str()) != Some("stub")));
    }

    // -----------------------------------------------------------------------
    // Cross-SDK two-driver parity (WI 0005)
    //
    // Two client keys attach to one session (driver A via connect, driver B via
    // join, same profile), both register as drivers, both send a message. Every
    // driver observes the SAME ordered broadcast event sequence — including the
    // other driver's `session.join` / `session.driver.register` and, in FIFO
    // order, both turns' messages. The canonical sequence below MUST stay
    // byte-for-byte identical to the arrays in the TypeScript and Python SDK
    // two-driver parity tests:
    //   - client-typescript/src/harness.test.ts   (TWO_DRIVER_PARITY_SEQUENCE)
    //   - client-python/tests/test_two_driver_parity.py (TWO_DRIVER_PARITY_SEQUENCE)
    // -----------------------------------------------------------------------

    /// The canonical live-notification sequence every driver observes for the
    /// scripted two-driver session.
    const TWO_DRIVER_PARITY_SEQUENCE: [&str; 11] = [
        "session.driver.register", // driver A registered (connect)
        "session.join",            // driver B joined
        "session.driver.register", // driver B registered (join)
        "client.message.send",     // driver A's message (FIFO first)
        "provider.request",
        "provider.response",
        "server.message.send",
        "client.message.send", // driver B's message (FIFO second)
        "provider.request",
        "provider.response",
        "server.message.send",
    ];

    const DRIVER_A_KEY: &str = "key_driver_a";
    const DRIVER_B_KEY: &str = "key_driver_b";

    fn attributed_event(
        event_type: &str,
        client_key_id: &str,
        payload: serde_json::Value,
    ) -> EventView {
        EventView {
            id: format!("evt_{event_type}_{client_key_id}"),
            session_id: "ses_two_driver".into(),
            client_key_id: Some(client_key_id.into()),
            event_type: event_type.into(),
            payload,
            created_at: "t".into(),
        }
    }

    /// One `session.sendMessage` outcome carrying the full two-driver broadcast
    /// as live notifications, then a terminal assistant turn. Both drivers'
    /// streams deliver this identical sequence (cross-visibility).
    fn two_driver_outcome() -> SendOutcome {
        let notifications = vec![
            attributed_event("session.driver.register", DRIVER_A_KEY, json!({})),
            attributed_event(
                "session.join",
                DRIVER_B_KEY,
                json!({ "client_version": "9.9.9", "tools": ["get_current_time"] }),
            ),
            attributed_event("session.driver.register", DRIVER_B_KEY, json!({})),
            attributed_event(
                "client.message.send",
                DRIVER_A_KEY,
                json!({ "role": "user", "content": "from A" }),
            ),
            attributed_event("provider.request", DRIVER_A_KEY, json!({ "attempt": 0 })),
            attributed_event(
                "provider.response",
                DRIVER_A_KEY,
                json!({ "ok": true, "status": 200 }),
            ),
            attributed_event(
                "server.message.send",
                DRIVER_A_KEY,
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "reply A" }] }),
            ),
            attributed_event(
                "client.message.send",
                DRIVER_B_KEY,
                json!({ "role": "user", "content": "from B" }),
            ),
            attributed_event("provider.request", DRIVER_B_KEY, json!({ "attempt": 0 })),
            attributed_event(
                "provider.response",
                DRIVER_B_KEY,
                json!({ "ok": true, "status": 200 }),
            ),
            attributed_event(
                "server.message.send",
                DRIVER_B_KEY,
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "reply B" }] }),
            ),
        ];
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "reply B".into(),
                }]),
                events: vec![],
            },
            notifications,
        }
    }

    /// Collect the observed `(event_type, client_key_id)` pairs of one driver by
    /// running its send loop against a transport delivering the shared broadcast.
    async fn observe_two_driver_stream() -> (Vec<(String, Option<String>)>, MockTransport) {
        let transport = MockTransport::new(vec![Ok(two_driver_outcome())]);
        // Both connect() and join() register a driver once before the first
        // send; model that here since the offline loop bypasses connect()/join().
        transport.register_driver().await.unwrap();

        let observed = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let sink = observed.clone();
        let mut hooks = Hooks::default().on_event(move |ev| {
            sink.lock()
                .unwrap()
                .push((ev.event_type.clone(), ev.client_key_id.clone()));
            Ok(())
        });
        run_loop(
            &transport,
            &registry(vec![]),
            &mut hooks,
            Message::user("go"),
        )
        .await
        .unwrap();
        let out = observed.lock().unwrap().clone();
        (out, transport)
    }

    #[tokio::test]
    async fn two_drivers_observe_identical_fifo_broadcast_and_register() {
        let (observed_a, transport_a) = observe_two_driver_stream().await;
        let (observed_b, transport_b) = observe_two_driver_stream().await;

        let types_a: Vec<&str> = observed_a.iter().map(|(t, _)| t.as_str()).collect();
        let types_b: Vec<&str> = observed_b.iter().map(|(t, _)| t.as_str()).collect();

        // Both drivers observe the identical canonical sequence (cross-visibility).
        assert_eq!(types_a, TWO_DRIVER_PARITY_SEQUENCE);
        assert_eq!(types_b, TWO_DRIVER_PARITY_SEQUENCE);
        assert_eq!(observed_a, observed_b);

        // Each driver registered exactly once (connect / join issue it).
        assert_eq!(*transport_a.driver_registrations.borrow(), 1);
        assert_eq!(*transport_b.driver_registrations.borrow(), 1);

        // Cross-visibility of client keys: an observer sees events attributed to
        // BOTH drivers, not just its own.
        let keys: Vec<&str> = observed_a
            .iter()
            .filter_map(|(_, k)| k.as_deref())
            .collect();
        assert!(keys.contains(&DRIVER_A_KEY));
        assert!(keys.contains(&DRIVER_B_KEY));

        // FIFO ordering: driver A's message turn precedes driver B's. Compare the
        // two `client.message.send` events' positions and attribution.
        let sends: Vec<&(String, Option<String>)> = observed_a
            .iter()
            .filter(|(t, _)| t == "client.message.send")
            .collect();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].1.as_deref(), Some(DRIVER_A_KEY));
        assert_eq!(sends[1].1.as_deref(), Some(DRIVER_B_KEY));

        // The new event payloads parse to their real shapes.
        let join = two_driver_outcome()
            .notifications
            .into_iter()
            .find(|e| e.event_type == "session.join")
            .unwrap();
        let join_payload: crate::SessionJoinPayload = serde_json::from_value(join.payload).unwrap();
        assert_eq!(join_payload.client_version.as_deref(), Some("9.9.9"));
        assert_eq!(join_payload.tools, vec!["get_current_time".to_string()]);
    }

    // -----------------------------------------------------------------------
    // Cross-SDK sandbox dispatch parity (WI 0006)
    //
    // The three client SDKs must observe an IDENTICAL ordered live event
    // sequence for the same scripted sandbox turn, for both the AUTO
    // (server-dispatched, structurally identical to an MCP round-trip) and
    // MANUAL (client-dispatched, two turns) paths. The canonical sequences below
    // MUST stay byte-for-byte identical to the arrays in the TS and Python SDK
    // sandbox parity tests:
    //   - client-typescript/src/sandbox.test.ts   (SANDBOX_AUTO_PARITY_SEQUENCE /
    //                                               SANDBOX_MANUAL_PARITY_SEQUENCE)
    //   - client-python/tests/test_sandbox_parity.py (same two names)
    // -----------------------------------------------------------------------

    /// Auto dispatch: one server-dispatched turn. Mirrors `MCP_PARITY_SEQUENCE`
    /// exactly, with `sandbox.request`/`sandbox.response` in place of the MCP
    /// events — the two dispatch paths look structurally identical in the log.
    const SANDBOX_AUTO_PARITY_SEQUENCE: [&str; 9] = [
        "provider.request",
        "provider.response",
        "tool.call",
        "sandbox.request",
        "sandbox.response",
        "tool.result",
        "provider.request",
        "provider.response",
        "server.message.send",
    ];

    /// Manual dispatch: the assistant turn pauses with a `tool_use`, the client
    /// harness runs the tool (issuing `session.execRemoteSandbox` out of band —
    /// which emits no lifecycle event on success), then sends the `tool_result`
    /// back for a second turn. Two turns' worth of broadcast events.
    const SANDBOX_MANUAL_PARITY_SEQUENCE: [&str; 7] = [
        "provider.request",
        "provider.response",
        "server.message.send",
        "client.message.send",
        "provider.request",
        "provider.response",
        "server.message.send",
    ];

    /// The scripted AUTO turn: the live notifications, then a terminal text turn.
    fn sandbox_auto_outcome() -> SendOutcome {
        let notifications = vec![
            parity_event("provider.request", json!({ "attempt": 0 })),
            parity_event("provider.response", json!({ "ok": true, "status": 200 })),
            parity_event(
                "tool.call",
                json!({ "dispatch": "sandbox", "name": "run_shell_command", "input": { "command": "echo hi" } }),
            ),
            parity_event(
                "sandbox.request",
                json!({ "tool": "run_shell_command", "input": { "command": "echo hi" }, "command": "echo hi" }),
            ),
            parity_event(
                "sandbox.response",
                json!({ "sandbox_id": "cid-1", "ok": true, "result": { "stdout": "hi\n", "stderr": "", "exit_code": 0 } }),
            ),
            parity_event(
                "tool.result",
                json!({ "tool_use_id": "tu_sbx", "dispatch": "sandbox", "is_error": false, "content": [{ "type": "text", "text": "hi\n" }] }),
            ),
            parity_event("provider.request", json!({ "attempt": 0 })),
            parity_event("provider.response", json!({ "ok": true, "status": 200 })),
            parity_event(
                "server.message.send",
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "ran it" }] }),
            ),
        ];
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "ran it".into(),
                }]),
                events: vec![],
            },
            notifications,
        }
    }

    #[tokio::test]
    async fn sandbox_auto_scenario_matches_canonical_sequence() {
        let transport = MockTransport::new(vec![Ok(sandbox_auto_outcome())]);
        let tools = registry(vec![]);

        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = observed.clone();
        let mut hooks = Hooks::default().on_event(move |ev| {
            sink.lock().unwrap().push(ev.event_type.clone());
            Ok(())
        });

        // Auto sandbox tools are dispatched server-side, so the loop ends after
        // one turn — never pausing, never reaching the client.
        let out = run_loop(&transport, &tools, &mut hooks, Message::user("run it"))
            .await
            .unwrap();
        assert_eq!(out.text(), "ran it");
        assert_eq!(
            transport.sent.borrow().len(),
            1,
            "server-dispatched: single turn"
        );

        let guard = observed.lock().unwrap();
        let seq: Vec<&str> = guard.iter().map(String::as_str).collect();
        assert_eq!(seq, SANDBOX_AUTO_PARITY_SEQUENCE);
    }

    /// A recording [`SandboxRpc`] for the manual path: records each
    /// `execRemoteSandbox` command and returns a scripted result.
    struct RecordingSandboxRpc {
        execs: Mutex<Vec<String>>,
    }

    impl crate::sandbox::SandboxRpc for RecordingSandboxRpc {
        fn exec_remote_sandbox(
            &self,
            command: String,
        ) -> crate::sandbox::SandboxFuture<'_, Result<crate::sandbox::ExecResult, Error>> {
            self.execs.lock().unwrap().push(command);
            Box::pin(async {
                Ok(crate::sandbox::ExecResult {
                    stdout: "manual-out".into(),
                    stderr: String::new(),
                    exit_code: 0,
                })
            })
        }
        fn report_local_sandbox(
            &self,
            _report: crate::sandbox::LocalSandboxReport,
        ) -> crate::sandbox::SandboxFuture<'_, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn sandbox_manual_turn1() -> SendOutcome {
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::ToolUse {
                    id: "tu_manual".into(),
                    name: "run_shell_command".into(),
                    input: json!({ "command": "ls -la" }),
                    dispatch: None,
                }]),
                events: vec![],
            },
            notifications: vec![
                parity_event("provider.request", json!({ "attempt": 0 })),
                parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                parity_event(
                    "server.message.send",
                    json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "tu_manual", "name": "run_shell_command", "input": { "command": "ls -la" } }] }),
                ),
            ],
        }
    }

    fn sandbox_manual_turn2() -> SendOutcome {
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "done".into(),
                }]),
                events: vec![],
            },
            notifications: vec![
                parity_event(
                    "client.message.send",
                    json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "tu_manual" }] }),
                ),
                parity_event("provider.request", json!({ "attempt": 0 })),
                parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                parity_event(
                    "server.message.send",
                    json!({ "role": "assistant", "content": [{ "type": "text", "text": "done" }] }),
                ),
            ],
        }
    }

    #[tokio::test]
    async fn sandbox_manual_scenario_matches_canonical_sequence_and_dispatches_client_side() {
        let transport =
            MockTransport::new(vec![Ok(sandbox_manual_turn1()), Ok(sandbox_manual_turn2())]);

        // A remote-manual sandbox tool, built from a session bound to a recorder.
        let session = crate::sandbox::SandboxSession::new();
        let rpc = Arc::new(RecordingSandboxRpc {
            execs: Mutex::new(Vec::new()),
        });
        session.bind(rpc.clone() as Arc<dyn crate::sandbox::SandboxRpc>);
        let tool = crate::sandbox::run_shell_command(
            &session,
            crate::sandbox::SandboxTarget::Remote,
            crate::sandbox::RemoteMode::manual(|r| json!(r.stdout)),
        )
        .into_tool()
        .expect("remote+manual yields a client-dispatched tool");
        let tools = registry(vec![tool]);

        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = observed.clone();
        let mut hooks = Hooks::default().on_event(move |ev| {
            sink.lock().unwrap().push(ev.event_type.clone());
            Ok(())
        });

        let out = run_loop(&transport, &tools, &mut hooks, Message::user("list files"))
            .await
            .unwrap();
        assert_eq!(out.text(), "done");
        // Manual dispatch pauses: two turns (tool_use, then tool_result).
        assert_eq!(transport.sent.borrow().len(), 2);

        let guard = observed.lock().unwrap();
        let seq: Vec<&str> = guard.iter().map(String::as_str).collect();
        assert_eq!(seq, SANDBOX_MANUAL_PARITY_SEQUENCE);
        drop(guard);

        // The client harness actually dispatched the tool, issuing the fully
        // interpolated command over `session.execRemoteSandbox`.
        assert_eq!(*rpc.execs.lock().unwrap(), vec!["ls -la".to_string()]);
    }

    // -----------------------------------------------------------------------
    // Cross-SDK local-subagent parity (WI 0010)
    //
    // The three client SDKs must observe an IDENTICAL ordered live event
    // sequence for the same scripted local launch -> poll(running) ->
    // poll(completed) flow, driven entirely through client-dispatched tools
    // (no server-side subagent dispatch is exercised here — that is the
    // server suite's job). The canonical sequence below MUST stay
    // byte-for-byte identical to the arrays in the TS and Python SDK parity
    // tests:
    //   - client-typescript/src/subagent.test.ts (LOCAL_SUBAGENT_PARITY_SEQUENCE)
    //   - client-python/tests/test_subagent_parity.py (same name)
    // -----------------------------------------------------------------------

    /// The full observed sequence across two `send()` calls: launch, a poll
    /// that still finds it `running`, a "check back later" reply, then (after
    /// the fake subprocess is released to "exit") a second `send()` whose poll
    /// finds it `completed`.
    const LOCAL_SUBAGENT_PARITY_SEQUENCE: [&str; 18] = [
        "provider.request",
        "provider.response",
        "server.message.send", // assistant: tool_use launch_subagent
        "client.message.send", // tool_result: {"status":"started",...}
        "provider.request",
        "provider.response",
        "server.message.send", // assistant: tool_use local_subagent_status
        "client.message.send", // tool_result: {"subagents":[{"status":"running",...}]}
        "provider.request",
        "provider.response",
        "server.message.send", // assistant: final text; first send() ends
        "provider.request",
        "provider.response",
        "server.message.send", // assistant: tool_use local_subagent_status (2nd send())
        "client.message.send", // tool_result: {"subagents":[{"status":"completed",...}]}
        "provider.request",
        "provider.response",
        "server.message.send", // assistant: final text; second send() ends
    ];

    /// A [`crate::subagent::SubagentRunner`] whose single subprocess blocks on
    /// a [`tokio::sync::Notify`] gate until the test releases it, so the
    /// launch -> poll(running) -> poll(completed) ordering is deterministic.
    struct GatedSubagentRunner {
        gate: Arc<tokio::sync::Notify>,
    }

    impl crate::subagent::SubagentRunner for GatedSubagentRunner {
        fn run<'a>(
            &'a self,
            _program: &'a str,
            _args: &'a [String],
            _stdin: Option<&'a [u8]>,
        ) -> SubagentFuture<'a, std::io::Result<crate::subagent::RunnerOutput>> {
            let gate = self.gate.clone();
            Box::pin(async move {
                gate.notified().await;
                Ok(crate::subagent::RunnerOutput {
                    stdout: "subagent done".to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                })
            })
        }
    }

    /// A recording [`SubagentRpc`]: mirrors `RecordingSandboxRpc` above, plus a
    /// notify fired on every terminal `reportLocalSubagent` — the test's
    /// synchronization point for the detached watcher.
    #[derive(Default)]
    struct RecordingSubagentRpc {
        updates: Mutex<Vec<Vec<serde_json::Value>>>,
        terminal_notify: Mutex<Option<Arc<tokio::sync::Notify>>>,
    }

    impl SubagentRpc for RecordingSubagentRpc {
        fn report_local_subagent(
            &self,
            report: LocalSubagentReport,
        ) -> SubagentFuture<'_, Result<(), Error>> {
            let terminal = matches!(report.state.as_str(), "completed" | "failed" | "cancelled");
            if terminal {
                if let Some(n) = self.terminal_notify.lock().unwrap().as_ref() {
                    n.notify_one();
                }
            }
            Box::pin(async { Ok(()) })
        }
        fn update_client_tools(
            &self,
            tools: Vec<serde_json::Value>,
        ) -> SubagentFuture<'_, Result<(), Error>> {
            self.updates.lock().unwrap().push(tools);
            Box::pin(async { Ok(()) })
        }
        fn cancel_subagent(&self, _subagent_id: String) -> SubagentFuture<'_, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// The five scripted assistant turns backing [`LOCAL_SUBAGENT_PARITY_SEQUENCE`].
    fn local_subagent_parity_outcomes() -> Vec<Result<SendOutcome, Error>> {
        vec![
            // Turn A1: assistant launches a subagent.
            Ok(SendOutcome {
                result: SendMessageResult {
                    message: Message::assistant(vec![tool_use(
                        "tu_launch",
                        "launch_subagent",
                        json!({ "harness": "claude", "model": "claude-sonnet-5", "prompt": "do the task" }),
                        None,
                    )]),
                    events: vec![],
                },
                notifications: vec![
                    parity_event("provider.request", json!({ "attempt": 0 })),
                    parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                    parity_event(
                        "server.message.send",
                        json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "tu_launch", "name": "launch_subagent", "input": { "harness": "claude", "model": "claude-sonnet-5", "prompt": "do the task" } }] }),
                    ),
                ],
            }),
            // Turn A2: assistant polls; the fake subprocess is still gated.
            Ok(SendOutcome {
                result: SendMessageResult {
                    message: Message::assistant(vec![tool_use(
                        "tu_poll1",
                        "local_subagent_status",
                        json!({}),
                        None,
                    )]),
                    events: vec![],
                },
                notifications: vec![
                    parity_event(
                        "client.message.send",
                        json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "tu_launch" }] }),
                    ),
                    parity_event("provider.request", json!({ "attempt": 0 })),
                    parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                    parity_event(
                        "server.message.send",
                        json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "tu_poll1", "name": "local_subagent_status", "input": {} }] }),
                    ),
                ],
            }),
            // Turn A3: assistant reports back and the first send() ends.
            Ok(SendOutcome {
                result: SendMessageResult {
                    message: Message::assistant(vec![ContentBlock::Text {
                        text: "still running, I'll check back".into(),
                    }]),
                    events: vec![],
                },
                notifications: vec![
                    parity_event(
                        "client.message.send",
                        json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "tu_poll1" }] }),
                    ),
                    parity_event("provider.request", json!({ "attempt": 0 })),
                    parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                    parity_event(
                        "server.message.send",
                        json!({ "role": "assistant", "content": [{ "type": "text", "text": "still running, I'll check back" }] }),
                    ),
                ],
            }),
            // Turn B1: a fresh send() polls again; by now the subagent completed.
            Ok(SendOutcome {
                result: SendMessageResult {
                    message: Message::assistant(vec![tool_use(
                        "tu_poll2",
                        "local_subagent_status",
                        json!({}),
                        None,
                    )]),
                    events: vec![],
                },
                notifications: vec![
                    parity_event("provider.request", json!({ "attempt": 0 })),
                    parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                    parity_event(
                        "server.message.send",
                        json!({ "role": "assistant", "content": [{ "type": "tool_use", "id": "tu_poll2", "name": "local_subagent_status", "input": {} }] }),
                    ),
                ],
            }),
            // Turn B2: assistant reports completion; second send() ends.
            Ok(SendOutcome {
                result: SendMessageResult {
                    message: Message::assistant(vec![ContentBlock::Text {
                        text: "done".into(),
                    }]),
                    events: vec![],
                },
                notifications: vec![
                    parity_event(
                        "client.message.send",
                        json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "tu_poll2" }] }),
                    ),
                    parity_event("provider.request", json!({ "attempt": 0 })),
                    parity_event("provider.response", json!({ "ok": true, "status": 200 })),
                    parity_event(
                        "server.message.send",
                        json!({ "role": "assistant", "content": [{ "type": "text", "text": "done" }] }),
                    ),
                ],
            }),
        ]
    }

    #[tokio::test]
    async fn local_subagent_scenario_matches_canonical_sequence_across_two_sends() {
        let transport = MockTransport::new(local_subagent_parity_outcomes());

        let gate = Arc::new(tokio::sync::Notify::new());
        let rpc = Arc::new(RecordingSubagentRpc::default());
        let terminal_notify = Arc::new(tokio::sync::Notify::new());
        *rpc.terminal_notify.lock().unwrap() = Some(terminal_notify.clone());

        let subagent_session = SubagentSession::new(SandboxSession::new());
        subagent_session.bind(rpc.clone() as Arc<dyn SubagentRpc>);
        subagent_session.set_base_client_tools(vec![json!({
            "name": "launch_subagent",
            "description": "launch",
            "input_schema": {},
        })]);
        subagent_session.set_runner(Arc::new(GatedSubagentRunner { gate: gate.clone() }));

        let launch_tool = crate::subagent::launch_subagent(
            &subagent_session,
            vec![crate::subagent::SubagentDef::new("claude", "cat")],
            crate::subagent::SubagentLaunch::Local(crate::sandbox::SandboxTarget::None),
        )
        .into_tool()
        .expect("local launch yields a client-dispatched tool");
        let status_tool = subagent_session.status_tool();
        let tools = registry(vec![launch_tool, status_tool]);

        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = observed.clone();
        let mut hooks = Hooks::default().on_event(move |ev| {
            sink.lock().unwrap().push(ev.event_type.clone());
            Ok(())
        });

        // First send(): launch, then poll while still running.
        let out1 = run_loop(
            &transport,
            &tools,
            &mut hooks,
            Message::user("please launch a subagent"),
        )
        .await
        .unwrap();
        assert_eq!(out1.text(), "still running, I'll check back");

        // Let the fake subprocess "exit" and wait for the watcher's terminal report.
        gate.notify_one();
        terminal_notify.notified().await;

        // Second send(): poll again, now completed.
        let out2 = run_loop(&transport, &tools, &mut hooks, Message::user("check again"))
            .await
            .unwrap();
        assert_eq!(out2.text(), "done");

        let guard = observed.lock().unwrap();
        let seq: Vec<&str> = guard.iter().map(String::as_str).collect();
        assert_eq!(seq, LOCAL_SUBAGENT_PARITY_SEQUENCE);
        drop(guard);

        // Structural parity of the actual tool_result content exchanged with
        // the server at each turn (not just the event-type skeleton) —
        // per the contract's "structural comparison, not raw bytes" note.
        let sent = transport.sent.borrow();
        let tool_result_content = |turn: usize| -> serde_json::Value {
            match &sent[turn].message.content {
                Content::Blocks(blocks) => match &blocks[0] {
                    ContentBlock::ToolResult { content, .. } => {
                        serde_json::from_str(content.as_str().unwrap()).unwrap()
                    }
                    other => panic!("expected tool_result, got {other:?}"),
                },
                other => panic!("expected blocks, got {other:?}"),
            }
        };
        // sent[0] is the first user turn; [1] the launch tool_result; [2] the
        // running-poll tool_result; [3] the second send()'s user turn; [4]
        // the completed-poll tool_result.
        let started = tool_result_content(1);
        assert_eq!(started["status"], json!("started"));
        assert_eq!(started["harness"], json!("claude"));
        assert_eq!(started["model"], json!("claude-sonnet-5"));
        let subagent_id = started["subagent_id"].as_str().unwrap().to_string();

        let running = tool_result_content(2);
        assert_eq!(running["subagents"][0]["status"], json!("running"));
        assert_eq!(running["subagents"][0]["subagent_id"], json!(subagent_id));

        let completed = tool_result_content(4);
        assert_eq!(completed["subagents"][0]["status"], json!("completed"));
        assert_eq!(completed["subagents"][0]["subagent_id"], json!(subagent_id));
        assert_eq!(completed["subagents"][0]["stdout"], json!("subagent done"));

        // `updateClientTools` fired exactly on the two transitions — never
        // redundantly (the eviction on the completed poll removes it again).
        let updates = rpc.updates.lock().unwrap();
        assert_eq!(
            updates.len(),
            2,
            "empty->non-empty at launch, non-empty->empty at eviction"
        );
        assert!(updates[0]
            .iter()
            .any(|t| t["name"] == json!("local_subagent_status")));
        assert!(!updates[1]
            .iter()
            .any(|t| t["name"] == json!("local_subagent_status")));
    }
}
