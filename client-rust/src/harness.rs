//! The agent harness: session lifecycle and the tool-dispatch loop.
//!
//! [`Harness`] holds the [`Config`], the tool registry, and the [`Hooks`]. Its
//! async [`Harness::connect`] opens a session on the server and hands back a
//! [`Session`], whose [`Session::send`] drives the full round-trip described in
//! `docs/client-api.md`'s harness loop:
//!
//! 1. Send the user turn via the `session.sendMessage` JSON-RPC method.
//! 2. If the assistant response has no `tool_use` block, it is final — return it.
//! 3. Otherwise dispatch each `tool_use` to its registered handler, collect the
//!    `tool_result` blocks, send them back, and go to step 2.
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

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Config;
use crate::error::Error;
use crate::hooks::Hooks;
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
            hooks.run_before_tool_call(&mut call).map_err(Error::Hook)?;

            let tool = tools
                .get(&call.name)
                .ok_or_else(|| Error::UnknownTool(call.name.clone()))?;
            let output = tool
                .call(call.input.clone())
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
///     .with_tool(Tool::new("noop", "does nothing", json!({}), |_| Ok(json!("ok"))))
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
}

impl Harness {
    /// Create a harness from a [`Config`], with an empty tool registry and no
    /// hooks.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            tools: HashMap::new(),
            hooks: Hooks::default(),
        }
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
            tools,
            hooks,
        } = self;

        let body = OpenRequest {
            client_version: config.client_version.clone(),
            tools: tools.values().map(Tool::declaration).collect(),
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

        let transport = HttpTransport {
            http,
            base: config.base().to_string(),
            session_id: open.session_id.clone(),
            session_key: open.session_key,
            next_id: AtomicU64::new(1),
        };

        // Register as a driver before any send: session.sendMessage requires it
        // (a `-32001` error otherwise). Application code never calls this.
        transport.register_driver().await?;

        Ok(Session {
            transport,
            session_id: open.session_id,
            profile: open.profile,
            tools,
            hooks,
        })
    }
}

/// A live session handle. Created by [`Harness::connect`].
///
/// The session key is held internally and never exposed; drive the agent with
/// [`send`](Session::send) and release it with [`close`](Session::close).
pub struct Session {
    transport: HttpTransport,
    session_id: String,
    profile: Profile,
    tools: HashMap<String, Tool>,
    hooks: Hooks,
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
            &self.transport,
            &self.tools,
            &mut self.hooks,
            message.into(),
        )
        .await
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
    pub async fn close(&mut self) -> Result<(), Error> {
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
    use crate::types::ApiError;
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
        SendOutcome {
            result: SendMessageResult {
                message: Message::assistant(vec![ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input,
                }]),
                events: vec![],
            },
            notifications: vec![],
        }
    }

    fn time_tool() -> Tool {
        Tool::new("get_current_time", "current time", json!({}), |_input| {
            Ok(json!("2026-07-06T00:00:00Z"))
        })
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
    async fn dispatches_tool_call_and_sends_result_back() {
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
    async fn unknown_tool_is_an_error() {
        let transport =
            MockTransport::new(vec![Ok(assistant_tool_use("tu_1", "mystery", json!({})))]);
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
    async fn tool_handler_error_propagates() {
        let failing = Tool::new("boom", "always fails", json!({}), |_| {
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
}
