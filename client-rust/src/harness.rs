//! The agent harness: session lifecycle and the tool-dispatch loop.
//!
//! [`Harness`] holds the [`Config`], the tool registry, and the [`Hooks`]. Its
//! async [`Harness::connect`] opens a session on the server and hands back a
//! [`Session`], whose [`Session::send`] drives the full round-trip described in
//! `api-contract.md` §6:
//!
//! 1. POST the user turn.
//! 2. If the assistant response has no `tool_use` block, it is final — return it.
//! 3. Otherwise dispatch each `tool_use` to its registered handler, collect the
//!    `tool_result` blocks, POST them back, and go to step 2.
//!
//! The transport is abstracted behind a small crate-private [`Transport`] trait
//! so the loop can be unit-tested offline against a mock — the loop logic never
//! touches HTTP directly.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Error;
use crate::hooks::Hooks;
use crate::tool::Tool;
use crate::types::{Content, ContentBlock, EventView, Message, Profile, ToolResult};

/// `POST /api/v1/sessions/{id}/messages` request body.
#[derive(Debug, Serialize)]
pub(crate) struct MessagesRequest {
    pub message: Message,
}

/// `POST /api/v1/sessions/{id}/messages` success body.
#[derive(Debug, Deserialize)]
pub(crate) struct MessagesResponse {
    pub message: Message,
    #[serde(default)]
    pub events: Vec<EventView>,
}

/// The transport the loop drives. Abstracted so tests can mock the server.
///
/// `async fn` in a crate-private trait used only through generics (never as a
/// trait object) is fine; the allow silences the public-API lint pre-emptively.
#[allow(async_fn_in_trait)]
pub(crate) trait Transport {
    /// Send one message turn and return the assistant response. A `502`
    /// providers-failed outcome must surface as [`Error::ProvidersFailed`].
    async fn post_message(&self, req: &MessagesRequest) -> Result<MessagesResponse, Error>;

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

        let resp = transport
            .post_message(&MessagesRequest { message: current })
            .await?;
        let mut assistant = resp.message;

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
}

impl HttpTransport {
    fn messages_url(&self) -> String {
        format!("{}/api/v1/sessions/{}/messages", self.base, self.session_id)
    }

    fn session_url(&self) -> String {
        format!("{}/api/v1/sessions/{}", self.base, self.session_id)
    }
}

impl Transport for HttpTransport {
    async fn post_message(&self, req: &MessagesRequest) -> Result<MessagesResponse, Error> {
        let resp = self
            .http
            .post(self.messages_url())
            .bearer_auth(&self.session_key)
            .json(req)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if status.is_success() {
            return Ok(serde_json::from_slice(&bytes)?);
        }

        // 502 keeps the {message, events} shape, not a problem doc: providers
        // all failed. Surface the events so the caller can inspect the cause.
        if status.as_u16() == 502 {
            let body: MessagesResponse = serde_json::from_slice(&bytes)?;
            return Err(Error::ProvidersFailed {
                events: body.events,
            });
        }

        Err(Error::Api(parse_problem(status.as_u16(), &bytes)))
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

    /// Open a session on the server and return a [`Session`] handle.
    ///
    /// Sends the configured `client_version` and every registered tool's
    /// declaration; the server validates each name against the profile's
    /// `allowed_tools`. Consumes the harness, moving the tool registry and
    /// hooks into the session.
    pub async fn connect(self) -> Result<Session, Error> {
        let body = OpenRequest {
            client_version: self.config.client_version.clone(),
            tools: self.tools.values().map(Tool::declaration).collect(),
        };

        let url = format!("{}/api/v1/sessions", self.config.base());
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.config.client_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(Error::Api(parse_problem(status.as_u16(), &bytes)));
        }
        let open: OpenResponse = serde_json::from_slice(&bytes)?;

        Ok(Session {
            transport: HttpTransport {
                http: self.http,
                base: self.config.base().to_string(),
                session_id: open.session_id.clone(),
                session_key: open.session_key,
            },
            session_id: open.session_id,
            profile: open.profile,
            tools: self.tools,
            hooks: self.hooks,
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

    /// A scripted transport: returns queued responses in order, records each
    /// request it received, and never touches the network.
    struct MockTransport {
        // RefCell is fine: the loop awaits sequentially on one task/thread.
        responses: RefCell<Vec<Result<MessagesResponse, Error>>>,
        sent: RefCell<Vec<MessagesRequest>>,
        closed: RefCell<bool>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<MessagesResponse, Error>>) -> Self {
            Self {
                responses: RefCell::new(responses),
                sent: RefCell::new(Vec::new()),
                closed: RefCell::new(false),
            }
        }
    }

    impl Transport for MockTransport {
        async fn post_message(&self, req: &MessagesRequest) -> Result<MessagesResponse, Error> {
            self.sent.borrow_mut().push(MessagesRequest {
                message: req.message.clone(),
            });
            if self.responses.borrow().is_empty() {
                panic!("mock transport ran out of scripted responses");
            }
            self.responses.borrow_mut().remove(0)
        }

        async fn close(&self) -> Result<(), Error> {
            *self.closed.borrow_mut() = true;
            Ok(())
        }
    }

    fn assistant_text(text: &str) -> MessagesResponse {
        MessagesResponse {
            message: Message::assistant(vec![ContentBlock::Text {
                text: text.to_string(),
            }]),
            events: vec![],
        }
    }

    fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> MessagesResponse {
        MessagesResponse {
            message: Message::assistant(vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }]),
            events: vec![],
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
}
