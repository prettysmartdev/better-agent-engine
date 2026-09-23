//! Wire types for the BAE client API (`/api/v1`).
//!
//! These mirror the JSON contract in `api-contract.md` exactly (snake_case
//! fields). The content model is Anthropic-style: a message's `content` is
//! either a plain string or an ordered list of typed [`ContentBlock`]s.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A message exchanged with the server: a `role` plus its `content`.
///
/// User turns are usually plain text ([`Content::Text`]); assistant turns and
/// tool-result turns carry [`ContentBlock`]s.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    /// `"user"` or `"assistant"`. Defaults to `"user"` on deserialize.
    #[serde(default = "default_role")]
    pub role: String,
    /// String or block-array content.
    pub content: Content,
}

fn default_role() -> String {
    "user".to_string()
}

impl Message {
    /// A user-role message from the given content (string or blocks).
    pub fn user(content: impl Into<Content>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    /// An assistant-role message from the given content.
    pub fn assistant(content: impl Into<Content>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    /// The `tool_use` blocks present in this message's content, in order.
    /// Empty for plain-text turns — that emptiness is what ends the harness
    /// loop.
    pub fn tool_uses(&self) -> Vec<ToolUse> {
        match &self.content {
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        dispatch,
                    } => Some(ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        dispatch: dispatch.clone(),
                    }),
                    _ => None,
                })
                .collect(),
            Content::Text(_) => Vec::new(),
        }
    }

    /// Concatenation of all `text` blocks (or the whole string, for string
    /// content). Convenient for printing the final assistant turn.
    pub fn text(&self) -> String {
        match &self.content {
            Content::Text(s) => s.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

impl From<&str> for Message {
    fn from(s: &str) -> Self {
        Message::user(s)
    }
}

impl From<String> for Message {
    fn from(s: String) -> Self {
        Message::user(s)
    }
}

/// A message's content: either a plain string or a list of typed blocks.
///
/// Serialized untagged, so it round-trips to the exact JSON the server expects
/// (`"content": "hi"` vs `"content": [ {...} ]`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    /// Plain-text content.
    Text(String),
    /// An ordered list of content blocks.
    Blocks(Vec<ContentBlock>),
}

impl From<&str> for Content {
    fn from(s: &str) -> Self {
        Content::Text(s.to_string())
    }
}

impl From<String> for Content {
    fn from(s: String) -> Self {
        Content::Text(s)
    }
}

impl From<Vec<ContentBlock>> for Content {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Content::Blocks(blocks)
    }
}

/// A single Anthropic-style content block.
///
/// The closed set is a discriminated union on `type`, so an unhandled variant
/// is a compile error rather than a silent pass-through.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Free text.
    Text {
        /// The text.
        text: String,
    },
    /// A model request to invoke a tool.
    ToolUse {
        /// Correlation id; echoed back as `tool_result.tool_use_id`.
        id: String,
        /// Registered tool name to dispatch to.
        name: String,
        /// JSON arguments for the tool handler.
        input: Value,
        /// Server-selected owner for this invocation. This receive-only routing
        /// tag is omitted by older servers; see [`ToolUse::dispatch`].
        #[serde(default, skip_serializing)]
        dispatch: Option<String>,
    },
    /// The result of a tool invocation, sent back to the server.
    ToolResult {
        /// The `id` of the `tool_use` this answers.
        tool_use_id: String,
        /// Handler output (string or blocks), as raw JSON.
        content: Value,
    },
}

/// A tool-invocation request extracted from an assistant turn. This is the
/// event passed to the `before_tool_call` hook.
#[derive(Clone, Debug)]
pub struct ToolUse {
    /// Correlation id.
    pub id: String,
    /// Tool name to dispatch.
    pub name: String,
    /// JSON arguments.
    pub input: Value,
    /// Server-selected owner for this invocation, when supplied. `"client"`
    /// means this harness must execute it; every other present value is
    /// server-owned and informational. When absent, the harness falls back to
    /// local registry membership for compatibility with older servers.
    ///
    /// The full assistant [`Message`] (including server-owned calls) is passed
    /// to [`Hooks::after_receive`](crate::Hooks::after_receive), allowing an
    /// application or UI to display informational calls without executing them.
    pub dispatch: Option<String>,
}

/// The outcome of a tool invocation, before it is sent back to the server.
/// This is the event passed to the `after_tool_call` hook, which may mutate
/// `content`.
#[derive(Clone, Debug)]
pub struct ToolResult {
    /// The `tool_use` id being answered.
    pub tool_use_id: String,
    /// The name of the tool that produced this result.
    pub name: String,
    /// Handler output; the hook may rewrite this before it is transmitted.
    pub content: Value,
}

/// An event row as returned in the `session.sendMessage` terminal result, in
/// live `session.event` notifications, and by the events replay endpoint.
/// `event_type` is one of the closed set documented in `api-contract.md` §8;
/// `payload` is freeform JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventView {
    /// Event id (`evt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    pub session_id: String,
    /// Acting key id (`key_…`), or null.
    #[serde(default)]
    pub client_key_id: Option<String>,
    /// One of the closed `event_type` strings.
    pub event_type: String,
    /// Freeform payload.
    pub payload: Value,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}

/// The sanitized profile returned at session open. Contains no secrets (no
/// `auth_token`, no env-var names). Unknown/extra fields are ignored so the
/// SDK tolerates server-side additions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    /// Profile id (`pro_…`).
    pub id: String,
    /// Human-readable profile name.
    pub name: String,
    /// Tools this profile permits the client to declare.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// MCP server descriptors (opaque here).
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    /// Sanitized provider summary (`{provider, model}`), if present.
    #[serde(default)]
    pub provider: Option<Value>,
}

/// An RFC 7807 problem document as emitted by the server on non-2xx responses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    /// Stable short slug (e.g. `unauthorized`, `tool_not_allowed`). Match on
    /// this, not on `title`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Human-readable summary.
    #[serde(default)]
    pub title: String,
    /// HTTP status code.
    #[serde(default)]
    pub status: u16,
    /// Specifics for this occurrence.
    #[serde(default)]
    pub detail: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.kind,
            self.status,
            if self.detail.is_empty() {
                &self.title
            } else {
                &self.detail
            }
        )
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelopes for the session loop (`POST …/rpc`)
//
// The management routes (session open/close, events replay) stay plain REST;
// only the message loop is JSON-RPC. A request is POSTed to
// `POST /api/v1/sessions/{id}/rpc` and the reply is an `application/x-ndjson`
// stream of these envelopes: frames with no `id` are notifications
// (`session.event`), and the frame carrying the request `id` is the terminal
// response (`result` on success, `error` on failure).
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request envelope. `method` is one of `session.sendMessage`,
/// `session.subscribe`, or `session.unsubscribe`.
#[derive(Clone, Debug, Serialize)]
pub struct JsonRpcRequest<P> {
    /// Protocol tag; always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Correlation id echoed back on the terminal response.
    pub id: u64,
    /// The method to invoke.
    pub method: String,
    /// Method parameters.
    pub params: P,
}

impl<P> JsonRpcRequest<P> {
    /// Build a `"2.0"` request with the given id, method, and params.
    pub fn new(id: u64, method: impl Into<String>, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// A single JSON-RPC 2.0 frame decoded from the NDJSON response stream.
///
/// Branch on [`id`](Self::id): a frame with no `id` is a **notification** (its
/// `method`/`params` carry a `session.event`, or an `error` for a mid-stream
/// notice such as `lagged`); the frame carrying the request `id` is the
/// **terminal response** (`result` on success, `error` on failure).
#[derive(Clone, Debug, Deserialize)]
pub struct JsonRpcFrame {
    /// Present only on the terminal response.
    #[serde(default)]
    pub id: Option<Value>,
    /// Notification method (e.g. `session.event`).
    #[serde(default)]
    pub method: Option<String>,
    /// Notification params (e.g. the [`EventView`] for a `session.event`).
    #[serde(default)]
    pub params: Option<Value>,
    /// Terminal success payload.
    #[serde(default)]
    pub result: Option<Value>,
    /// A JSON-RPC error object (terminal, or a mid-stream notice).
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object. `code` follows the spec's reserved ranges plus
/// the server's `-32000` application errors (session-not-open,
/// profile-unavailable-mid-session, lagged).
#[derive(Clone, Debug, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured detail.
    #[serde(default)]
    pub data: Option<Value>,
}

/// Params for the `session.sendMessage` method.
#[derive(Clone, Debug, Serialize)]
pub struct SendMessageParams {
    /// The user (or tool-result) turn to send.
    pub message: Message,
}

/// Params for the `session.subscribe` method. An absent `since_event_id`
/// subscribes from the live tip; a known id replays persisted events after it
/// before going live.
#[derive(Clone, Debug, Default, Serialize)]
pub struct SubscribeParams {
    /// Replay persisted events strictly after this event id, then go live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_event_id: Option<String>,
}

/// The terminal `result` of a `session.sendMessage` call — the same
/// `{message, events}` body the legacy synchronous message route returned.
/// `events` is the full turn event list; the live `session.event`
/// notifications are an additive, filtered subset of it.
#[derive(Clone, Debug, Deserialize)]
pub struct SendMessageResult {
    /// The assistant turn produced by this exchange.
    pub message: Message,
    /// The full ordered list of events appended during the turn.
    #[serde(default)]
    pub events: Vec<EventView>,
}

/// Payload of a `session.join` session event: a second (or further) client key
/// minted a session key for an existing session via `POST …/join`. Carried in
/// the `payload` of an [`EventView`] whose `event_type` is `"session.join"`;
/// the joining client is identified by the event's `client_key_id`. Same shape
/// as the `session.open` payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionJoinPayload {
    /// The joining client's declared `client_version`, if any.
    #[serde(default)]
    pub client_version: Option<String>,
    /// The names of the tools the joining client declared (independent of any
    /// other driver's set).
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Payload of an `mcp.request` session event: the engine dispatching a tool
/// call to a configured MCP server. Carried in the `payload` of an
/// [`EventView`] whose `event_type` is `"mcp.request"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpRequestPayload {
    /// The MCP method invoked (currently always `"tools/call"`).
    pub method: String,
    /// The server the call was routed to, or `null` if the tool was unroutable.
    #[serde(default)]
    pub server_name: Option<String>,
    /// The requested tool name.
    pub tool: String,
    /// The JSON arguments passed to the tool.
    pub input: Value,
}

/// Payload of an `mcp.response` session event. `ok` discriminates success
/// (carrying the MCP `result` object) from failure (carrying an `error`
/// string). Carried in the `payload` of an [`EventView`] whose `event_type`
/// is `"mcp.response"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpResponsePayload {
    /// The server that handled (or failed) the call.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Whether the MCP call succeeded.
    pub ok: bool,
    /// The MCP `result` object (`{content, isError?}`) on success.
    #[serde(default)]
    pub result: Option<Value>,
    /// The error description on failure.
    #[serde(default)]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// Session-level compaction, fixed at session creation. Sent as the
/// `compaction` field of `POST /api/v1/sessions` (never on join), serialized as
/// the server's mode-tagged enum: `{"mode":"auto","size":N}`,
/// `{"mode":"client"}`, or `{"mode":"client","prompt":"…"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum CompactionConfig {
    /// The server compacts automatically once the session reaches `size`
    /// tokens.
    Auto {
        /// Token threshold that triggers an automatic compaction.
        size: u64,
    },
    /// Compaction runs only when a driver calls [`Session::compact`](crate::Session::compact).
    Client {
        /// Default summarization prompt for this session (the server default
        /// when `None`). Omitted from the wire when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
}

impl CompactionConfig {
    /// `{"mode":"auto","size":size}`.
    pub fn auto(size: u64) -> Self {
        Self::Auto { size }
    }

    /// `{"mode":"client"}` — the server's default summarization prompt.
    pub fn client() -> Self {
        Self::Client { prompt: None }
    }

    /// `{"mode":"client","prompt":prompt}`.
    pub fn client_with_prompt(prompt: impl Into<String>) -> Self {
        Self::Client {
            prompt: Some(prompt.into()),
        }
    }
}

/// Params for the `session.compact` method. An absent `prompt` serializes as
/// `{}`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CompactParams {
    /// Summarization prompt override for this compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Payload of a `session.compaction.started` event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionStartedPayload {
    /// `"auto"` (token threshold) or `"client"` (`session.compact`).
    pub trigger: String,
    /// `"token_threshold"`, `"retry_after_failure"` (auto only), or `"manual"`.
    pub reason: String,
    /// The session's token count when compaction started, if known.
    #[serde(default)]
    pub token_count: Option<u64>,
    /// The auto-compaction threshold (`null` for client-triggered runs).
    #[serde(default)]
    pub threshold_tokens: Option<u64>,
}

/// Payload of a `session.compaction.completed` event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionCompletedPayload {
    /// The synthetic user-role preamble written before the summary. `None` on
    /// events written by servers that predate the preamble.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preamble_event_id: Option<String>,
    /// The `server.message.send` event holding the summary.
    pub summary_event_id: String,
    /// How many history messages were summarized.
    pub compacted_message_count: u64,
    /// Provider-reported compaction input usage, if any.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Provider-reported summary output usage, if any.
    #[serde(default)]
    pub summary_tokens: Option<u64>,
}

/// A typed `session.compaction.started` event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionStarted {
    /// Event id (`evt_…`).
    pub id: String,
    /// Owning session id.
    pub session_id: String,
    /// The acting client key, if any.
    #[serde(default)]
    pub client_key_id: Option<String>,
    /// RFC 3339 timestamp.
    pub created_at: String,
    /// The typed payload.
    pub payload: SessionCompactionStartedPayload,
}

/// A typed `session.compaction.completed` event — the terminal result of
/// [`Session::compact`](crate::Session::compact).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionCompactionCompleted {
    /// Event id (`evt_…`).
    pub id: String,
    /// Owning session id.
    pub session_id: String,
    /// The acting client key, if any.
    #[serde(default)]
    pub client_key_id: Option<String>,
    /// RFC 3339 timestamp.
    pub created_at: String,
    /// The typed payload.
    pub payload: SessionCompactionCompletedPayload,
}

/// `payload.synthetic` on the user-role preamble that precedes a compaction
/// summary.
pub const SYNTHETIC_COMPACTION_PREAMBLE: &str = "compaction_preamble";
/// `payload.synthetic` on the user-role tool results the server writes when it
/// retires an expired paused turn.
pub const SYNTHETIC_ABANDONED_TOOL_RESULTS: &str = "abandoned_tool_results";

/// Payload of a `server.message.send` event. `role` is `"assistant"` for model
/// turns and `"user"` only on server-written synthetic messages, which carry
/// `synthetic` naming why they exist (see [`SYNTHETIC_COMPACTION_PREAMBLE`]).
/// `content` is kept as raw JSON so every block round-trips untouched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerMessagePayload {
    /// `"assistant"` or `"user"`.
    #[serde(default = "default_server_role")]
    pub role: String,
    /// The message content blocks, verbatim.
    pub content: Value,
    /// Why a synthetic message was written; absent on ordinary messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<String>,
}

fn default_server_role() -> String {
    "assistant".to_string()
}

impl ServerMessagePayload {
    /// Is this the compaction preamble marker?
    pub fn is_compaction_preamble(&self) -> bool {
        self.synthetic.as_deref() == Some(SYNTHETIC_COMPACTION_PREAMBLE)
    }
}

impl EventView {
    /// Decode `payload` as `T` (e.g. [`ServerMessagePayload`] for a
    /// `server.message.send` row). The caller checks `event_type` first.
    pub fn payload_as<T: serde::de::DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.payload)
    }
}

impl TryFrom<EventView> for SessionCompactionCompleted {
    type Error = serde_json::Error;
    fn try_from(e: EventView) -> Result<Self, Self::Error> {
        Ok(Self {
            payload: serde_json::from_value(e.payload)?,
            id: e.id,
            session_id: e.session_id,
            client_key_id: e.client_key_id,
            created_at: e.created_at,
        })
    }
}

impl TryFrom<EventView> for SessionCompactionStarted {
    type Error = serde_json::Error;
    fn try_from(e: EventView) -> Result<Self, Self::Error> {
        Ok(Self {
            payload: serde_json::from_value(e.payload)?,
            id: e.id,
            session_id: e.session_id,
            client_key_id: e.client_key_id,
            created_at: e.created_at,
        })
    }
}

#[cfg(test)]
mod compaction_tests {
    use super::*;
    use serde_json::json;

    // Fixtures shared with the other SDKs and the server (WI 0018 contracts.md
    // §1/§8), copied verbatim into tests/fixtures/.
    const PREAMBLE_EVENT: &str = include_str!("../tests/fixtures/compaction_preamble_event.json");
    const SUMMARY_EVENT: &str = include_str!("../tests/fixtures/compaction_summary_event.json");
    const COMPLETED_EVENT: &str = include_str!("../tests/fixtures/compaction_completed_event.json");
    const COMPLETED_EVENT_LEGACY: &str =
        include_str!("../tests/fixtures/compaction_completed_event_legacy.json");
    const STARTED_MANUAL: &str = include_str!("../tests/fixtures/compaction_started_manual.json");
    const STARTED_RETRY_AFTER_FAILURE: &str =
        include_str!("../tests/fixtures/compaction_started_retry_after_failure.json");

    // -------------------------------------------------------------------
    // `CompactionConfig` serialization (contracts.md §1.11) — byte-exact.
    // -------------------------------------------------------------------

    #[test]
    fn compaction_config_auto_serializes_exactly() {
        let v = serde_json::to_value(CompactionConfig::auto(128_000)).unwrap();
        assert_eq!(v, json!({ "mode": "auto", "size": 128_000 }));
    }

    #[test]
    fn compaction_config_client_no_prompt_omits_prompt_key() {
        let v = serde_json::to_value(CompactionConfig::client()).unwrap();
        assert_eq!(v, json!({ "mode": "client" }));
        assert!(
            v.as_object().unwrap().get("prompt").is_none(),
            "prompt key must be absent, not null: {v}"
        );
    }

    #[test]
    fn compaction_config_client_with_prompt_serializes_exactly() {
        let v = serde_json::to_value(CompactionConfig::client_with_prompt(
            "Summarize focusing on open TODOs.",
        ))
        .unwrap();
        assert_eq!(
            v,
            json!({ "mode": "client", "prompt": "Summarize focusing on open TODOs." })
        );
    }

    // -------------------------------------------------------------------
    // `CompactParams` — `session.compact` request params (contracts.md §1.12).
    // -------------------------------------------------------------------

    #[test]
    fn compact_params_no_prompt_serializes_as_empty_object() {
        let v = serde_json::to_value(CompactParams::default()).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn compact_params_with_prompt_serializes_exactly() {
        let params = CompactParams {
            prompt: Some("…".to_string()),
        };
        let v = serde_json::to_value(params).unwrap();
        assert_eq!(v, json!({ "prompt": "…" }));
    }

    // -------------------------------------------------------------------
    // `synthetic` / `preamble_event_id` round-trip through the shared fixtures.
    // -------------------------------------------------------------------

    #[test]
    fn preamble_event_payload_has_synthetic_and_user_role() {
        let ev: EventView = serde_json::from_str(PREAMBLE_EVENT).unwrap();
        assert_eq!(ev.event_type, "server.message.send");
        let payload: ServerMessagePayload = ev.payload_as().unwrap();
        assert_eq!(payload.role, "user");
        assert_eq!(
            payload.synthetic.as_deref(),
            Some(SYNTHETIC_COMPACTION_PREAMBLE)
        );
        assert!(payload.is_compaction_preamble());

        // Round-trip: serializing it back out keeps `synthetic`.
        let back = serde_json::to_value(&payload).unwrap();
        assert_eq!(back["synthetic"], json!("compaction_preamble"));
        assert_eq!(back["role"], json!("user"));
    }

    #[test]
    fn summary_event_payload_has_no_synthetic_field() {
        let ev: EventView = serde_json::from_str(SUMMARY_EVENT).unwrap();
        let payload: ServerMessagePayload = ev.payload_as().unwrap();
        assert_eq!(payload.role, "assistant");
        assert_eq!(payload.synthetic, None);
        assert!(!payload.is_compaction_preamble());

        // A `None` synthetic never reappears on the wire.
        let back = serde_json::to_value(&payload).unwrap();
        assert!(back.as_object().unwrap().get("synthetic").is_none());
    }

    #[test]
    fn completed_payload_preamble_event_id_round_trips() {
        let ev: EventView = serde_json::from_str(COMPLETED_EVENT).unwrap();
        let completed = SessionCompactionCompleted::try_from(ev).unwrap();
        assert_eq!(completed.id, "evt_01completed");
        assert_eq!(
            completed.payload.preamble_event_id.as_deref(),
            Some("evt_01preamble")
        );
        assert_eq!(completed.payload.summary_event_id, "evt_01summary");
        assert_eq!(completed.payload.compacted_message_count, 17);
        assert_eq!(completed.payload.input_tokens, Some(42_100));
        assert_eq!(completed.payload.summary_tokens, Some(900));

        // Round-trip back through JSON keeps the field, not dropped.
        let back = serde_json::to_value(&completed.payload).unwrap();
        assert_eq!(back["preamble_event_id"], json!("evt_01preamble"));
    }

    #[test]
    fn completed_payload_missing_preamble_event_id_defaults_to_none() {
        // A pre-A1 event has no `preamble_event_id` key at all.
        let ev: EventView = serde_json::from_str(COMPLETED_EVENT_LEGACY).unwrap();
        assert!(ev.payload.get("preamble_event_id").is_none());
        let completed = SessionCompactionCompleted::try_from(ev).unwrap();
        assert_eq!(completed.payload.preamble_event_id, None);
        assert_eq!(completed.payload.summary_event_id, "evt_01summaryold");
        assert_eq!(completed.payload.input_tokens, None);
        assert_eq!(completed.payload.summary_tokens, None);

        // And omitting it when `None` keeps the wire shape byte-for-byte with
        // what an old server sent — no stray `"preamble_event_id":null`.
        let back = serde_json::to_value(&completed.payload).unwrap();
        assert!(back.as_object().unwrap().get("preamble_event_id").is_none());
    }

    #[test]
    fn started_manual_matches_fixture() {
        let ev: EventView = serde_json::from_str(STARTED_MANUAL).unwrap();
        let started = SessionCompactionStarted::try_from(ev).unwrap();
        assert_eq!(started.payload.trigger, "client");
        assert_eq!(started.payload.reason, "manual");
        assert_eq!(started.payload.token_count, Some(1_100));
        assert_eq!(started.payload.threshold_tokens, None);
    }

    #[test]
    fn started_retry_after_failure_matches_fixture() {
        let ev: EventView = serde_json::from_str(STARTED_RETRY_AFTER_FAILURE).unwrap();
        let started = SessionCompactionStarted::try_from(ev).unwrap();
        assert_eq!(started.payload.trigger, "auto");
        assert_eq!(started.payload.reason, "retry_after_failure");
        assert_eq!(started.payload.token_count, Some(140_010));
        assert_eq!(started.payload.threshold_tokens, Some(128_000));
    }
}
