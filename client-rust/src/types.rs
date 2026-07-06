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
                    ContentBlock::ToolUse { id, name, input } => Some(ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
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

/// An event row as returned in `POST …/messages` responses and the events
/// replay endpoint. `event_type` is one of the closed set documented in
/// `api-contract.md` §8; `payload` is freeform JSON.
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
