//! Provider (LLM) configuration and the outbound HTTP call.
//!
//! Providers are declared once in `bae-config.toml` under `[providers]` (see
//! [`crate::config_file`]) and referenced by profiles by *name*: a profile
//! carries a `primary_provider` name plus an ordered `fallback_providers` name
//! list, resolved against the startup registry by [`resolve_from_profile`].
//! Each registry entry matches this schema:
//!
//! ```toml
//! [[providers.entries]]
//! name       = "anthropic-sonnet"
//! provider   = "anthropic"           # wire format, not vendor
//! model      = "claude-sonnet-4-6"
//! auth_token = "${ANTHROPIC_API_KEY}"
//! max_tokens = 8096
//! # base_url is optional; omitted → the provider kind's own SaaS endpoint.
//! ```
//!
//! # Wire formats
//!
//! `provider` selects a **request/response shape and auth header convention**
//! ([`ProviderKind`]), not a vendor: an `openai`-kind entry may point its
//! `base_url` at any OpenAI-compatible endpoint, and likewise for `anthropic`.
//! The engine ([`super::session::run_turn`]) speaks exactly one canonical shape
//! — the Anthropic Messages API shape (`content` arrays of
//! `text`/`tool_use`/`tool_result` blocks, tools as
//! `{name, description, input_schema}`). [`call`] is the **only** place that
//! knows more than one wire format exists: for [`ProviderKind::OpenAi`] it
//! translates the canonical request into a Chat Completions request and the
//! response back into canonical blocks; for [`ProviderKind::Anthropic`] it
//! passes both through unchanged. The raw, untranslated wire response is still
//! surfaced (for `provider.response` event logging) alongside the canonical
//! translation — see [`ProviderResponse`].
//!
//! # Secret handling
//!
//! `auth_token` may contain `${ENV_VAR_NAME}` tokens. They are resolved by
//! [`resolve_tokens`] **immediately before** the HTTP call, held only in a local
//! for the duration of that call, and never written to an event, a log line, or
//! the database. An unset variable is a [`ProviderConfigError`], not a silently
//! blank token (a blank token would just produce opaque 401s from the provider).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The wire format (and auth header convention) a provider entry speaks. This
/// is a closed enum, mirroring [`crate::config_file::McpTransport`]: an
/// unsupported value is rejected at TOML parse time as an unknown variant.
///
/// It deliberately does **not** restrict `base_url` to the vendor's own
/// service — a self-hosted proxy or third-party API speaking either format at
/// any URL is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// The Anthropic Messages API shape (`POST {base}/v1/messages`,
    /// `x-api-key` + `anthropic-version` headers).
    Anthropic,
    /// The OpenAI Chat Completions API shape
    /// (`POST {base}/v1/chat/completions`, `Authorization: Bearer`).
    OpenAi,
}

impl ProviderKind {
    /// The wire/config string for this kind (matches the TOML value).
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAi => "openai",
        }
    }

    /// The kind's own default SaaS endpoint, used only when a registry entry
    /// omits `base_url`. Bare host (no `/v1` suffix): [`call`] appends the
    /// versioned path itself, so defaulted and explicit values are directly
    /// comparable.
    pub fn default_base_url(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "https://api.anthropic.com",
            ProviderKind::OpenAi => "https://api.openai.com",
        }
    }
}

/// One provider configuration (a `[[providers.entries]]` registry value,
/// resolved for a profile as its primary or one of its fallbacks).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// Which wire format [`call`] speaks to this entry.
    pub provider: ProviderKind,
    /// Optional endpoint override. Always used verbatim when present,
    /// regardless of `provider`; absent → the kind's default SaaS endpoint.
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    /// May contain `${ENV_VAR_NAME}` tokens; resolved only at call time.
    pub auth_token: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

impl ProviderConfig {
    /// The endpoint [`call`] actually targets: an explicit `base_url` verbatim,
    /// or the kind's default when absent.
    pub fn effective_base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or_else(|| self.provider.default_base_url())
    }
}

fn default_max_tokens() -> u32 {
    4096
}

/// Resolve a profile's provider name references against the startup registry.
///
/// - A missing **primary** is fatal for the profile:
///   [`ProviderConfigError::PrimaryProviderMissing`].
/// - A missing **fallback** is logged and skipped per name (never
///   short-circuiting the rest of the list), matching the MCP registry's
///   log-and-skip posture; the resolved subset (possibly empty) is returned.
pub fn resolve_from_profile(
    registry: &HashMap<String, ProviderConfig>,
    primary_provider: &str,
    fallback_providers: &[String],
) -> Result<(ProviderConfig, Vec<ProviderConfig>), ProviderConfigError> {
    let primary = registry
        .get(primary_provider)
        .cloned()
        .ok_or_else(|| ProviderConfigError::PrimaryProviderMissing(primary_provider.to_owned()))?;
    let mut fallbacks = Vec::new();
    for name in fallback_providers {
        match registry.get(name) {
            Some(cfg) => fallbacks.push(cfg.clone()),
            None => tracing::error!(
                fallback_provider = %name,
                "configured fallback provider not found in bae-config.toml; skipping"
            ),
        }
    }
    Ok((primary, fallbacks))
}

/// Error resolving provider configuration or its `${ENV_VAR}` tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConfigError {
    /// A referenced environment variable was not set at call time.
    MissingEnv(String),
    /// An unterminated `${…` token.
    Unterminated,
    /// The profile's provider references did not match the expected shape.
    Malformed(String),
    /// The profile's `primary_provider` name is absent from the registry —
    /// fatal for the profile, unlike a missing fallback (logged and skipped).
    PrimaryProviderMissing(String),
}

impl std::fmt::Display for ProviderConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderConfigError::MissingEnv(v) => {
                write!(
                    f,
                    "environment variable {v} referenced by provider config is not set"
                )
            }
            ProviderConfigError::Unterminated => {
                write!(f, "unterminated ${{...}} token in provider config")
            }
            ProviderConfigError::Malformed(e) => write!(f, "malformed provider config: {e}"),
            ProviderConfigError::PrimaryProviderMissing(name) => write!(
                f,
                "primary provider {name:?} is not configured in bae-config.toml"
            ),
        }
    }
}

impl std::error::Error for ProviderConfigError {}

/// Substitute every `${NAME}` token in `input` with the value of environment
/// variable `NAME`, using `lookup` to read the environment.
///
/// - `${NAME}` → the variable's value, or [`ProviderConfigError::MissingEnv`].
/// - A literal `$` **not** followed by `{` is passed through unchanged.
/// - An opening `${` with no closing `}` is [`ProviderConfigError::Unterminated`].
///
/// `lookup` is injected so this is testable without touching the real process
/// environment; [`resolve_tokens`] wires it to `std::env::var`.
pub fn resolve_tokens_with(
    input: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, ProviderConfigError> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let end = match input[start..].find('}') {
                Some(rel) => start + rel,
                None => return Err(ProviderConfigError::Unterminated),
            };
            let name = &input[start..end];
            let val =
                lookup(name).ok_or_else(|| ProviderConfigError::MissingEnv(name.to_owned()))?;
            out.push_str(&val);
            i = end + 1;
        } else {
            // Push this UTF-8 character whole (handles multi-byte correctly).
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// [`resolve_tokens_with`] against the real process environment.
pub fn resolve_tokens(input: &str) -> Result<String, ProviderConfigError> {
    resolve_tokens_with(input, &|k| std::env::var(k).ok())
}

/// A failed provider attempt, captured richly enough to record as a
/// `provider.response` failure event and to drive the fallback walk.
#[derive(Debug, Clone)]
pub enum ProviderCallError {
    /// The `${ENV_VAR}` resolution failed before any request was sent.
    Config(ProviderConfigError),
    /// The request was sent but the transport failed (DNS, connect, timeout),
    /// or the provider's 2xx body violated its own wire protocol (non-JSON
    /// body, or an OpenAI `tool_calls[].function.arguments` string that is not
    /// valid JSON).
    Transport(String),
    /// The provider returned a non-2xx status.
    Status { status: u16, body: String },
}

impl ProviderCallError {
    /// HTTP status of the failed attempt, if the request reached the provider.
    pub fn status(&self) -> Option<u16> {
        match self {
            ProviderCallError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// A short, secret-free description for the `provider.response` event.
    pub fn detail(&self) -> String {
        match self {
            ProviderCallError::Config(e) => e.to_string(),
            ProviderCallError::Transport(e) => e.clone(),
            ProviderCallError::Status { status, .. } => format!("provider returned HTTP {status}"),
        }
    }

    /// The provider's raw response body, if any.
    pub fn body(&self) -> Option<&str> {
        match self {
            ProviderCallError::Status { body, .. } => Some(body),
            _ => None,
        }
    }
}

/// One successful provider call, in both shapes the engine needs.
///
/// `raw` is what the provider actually said on the wire (untranslated), and is
/// what `provider.response` events record — the event log stays a faithful
/// record of the exchange. `canonical` is always the Anthropic-Messages-shaped
/// `{"content": [ …blocks… ]}` the engine consumes for history and tool
/// dispatch, regardless of which [`ProviderKind`] served the request. For
/// [`ProviderKind::Anthropic`] the two are the same value.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    /// The provider's raw, untranslated wire response.
    pub raw: Value,
    /// The canonical-shape body (`{"content": [...]}`).
    pub canonical: Value,
}

/// Perform one provider call.
///
/// Resolves `auth_token` here and holds it only for this function's body.
/// `messages` and `tools` are always the canonical (Anthropic Messages API)
/// shape; the request is translated per `cfg.provider` — see the module docs —
/// and sent to `cfg.effective_base_url()`'s versioned path. On a 2xx, returns
/// the [`ProviderResponse`] pair (raw wire body + canonical translation); a
/// [`ProviderCallError`] otherwise. The resolved token is never logged.
pub async fn call(
    http: &reqwest::Client,
    cfg: &ProviderConfig,
    messages: &Value,
    tools: &Value,
) -> Result<ProviderResponse, ProviderCallError> {
    let token = resolve_tokens(&cfg.auth_token).map_err(ProviderCallError::Config)?;

    let base = cfg.effective_base_url().trim_end_matches('/').to_owned();
    let (url, request_body) = match cfg.provider {
        ProviderKind::Anthropic => (
            format!("{base}/v1/messages"),
            json!({
                "model": cfg.model,
                "max_tokens": cfg.max_tokens,
                "messages": messages,
                "tools": tools,
            }),
        ),
        ProviderKind::OpenAi => (
            format!("{base}/v1/chat/completions"),
            openai_request_body(cfg, messages, tools),
        ),
    };

    let rb = http.post(&url).header("content-type", "application/json");
    let rb = match cfg.provider {
        ProviderKind::Anthropic => rb
            .header("x-api-key", &token)
            .header("anthropic-version", "2023-06-01"),
        ProviderKind::OpenAi => rb.header("authorization", format!("Bearer {token}")),
    };

    let resp = rb
        .json(&request_body)
        .send()
        .await
        .map_err(|e| ProviderCallError::Transport(sanitize_reqwest_error(e)))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| ProviderCallError::Transport(sanitize_reqwest_error(e)))?;

    if status.is_success() {
        let raw: Value = serde_json::from_str(&text).map_err(|e| {
            ProviderCallError::Transport(format!("provider returned non-JSON body: {e}"))
        })?;
        let canonical = match cfg.provider {
            ProviderKind::Anthropic => raw.clone(),
            ProviderKind::OpenAi => {
                from_openai_response(&raw).map_err(ProviderCallError::Transport)?
            }
        };
        Ok(ProviderResponse { raw, canonical })
    } else {
        Err(ProviderCallError::Status {
            status: status.as_u16(),
            body: text,
        })
    }
}

// ---------------------------------------------------------------------------
// OpenAI ↔ canonical translation (private, pure — no I/O)
// ---------------------------------------------------------------------------

/// Build the full Chat Completions request body from canonical inputs. Pure.
fn openai_request_body(cfg: &ProviderConfig, messages: &Value, tools: &Value) -> Value {
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "messages": to_openai_messages(messages),
    });
    let oa_tools = to_openai_tools(tools);
    // OpenAI rejects an empty `tools` array; omit the field entirely instead.
    if oa_tools.as_array().is_some_and(|a| !a.is_empty()) {
        body["tools"] = oa_tools;
    }
    body
}

/// Canonical tool definitions (`{name, description, input_schema}`) → OpenAI
/// function-calling shape (`{"type":"function","function":{…}}`). Pure.
fn to_openai_tools(tools: &Value) -> Value {
    let arr = tools.as_array().cloned().unwrap_or_default();
    Value::Array(
        arr.iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(Value::Null),
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t.get("input_schema").cloned().unwrap_or_else(|| json!({})),
                    },
                })
            })
            .collect(),
    )
}

/// Canonical messages → OpenAI Chat Completions messages. Pure.
///
/// - A plain `{role, content: "text"}` passes through as-is.
/// - A message whose content array carries `tool_result` blocks is split: each
///   block becomes its own `{"role":"tool","tool_call_id",…}` message (OpenAI
///   does not embed tool results in a user message), followed by one message
///   with any remaining text.
/// - An assistant message carrying `tool_use` blocks becomes an assistant
///   message with an OpenAI `tool_calls` array (`arguments` re-serialized as
///   the JSON string OpenAI expects).
fn to_openai_messages(messages: &Value) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for msg in messages.as_array().cloned().unwrap_or_default() {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        match msg.get("content") {
            Some(Value::String(s)) => out.push(json!({ "role": role, "content": s })),
            Some(Value::Array(blocks)) => {
                if role == "assistant" {
                    let text = joined_text(blocks);
                    let tool_calls: Vec<Value> = blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                        .map(|b| {
                            json!({
                                "id": b.get("id").cloned().unwrap_or(Value::Null),
                                "type": "function",
                                "function": {
                                    "name": b.get("name").cloned().unwrap_or(Value::Null),
                                    "arguments": b
                                        .get("input")
                                        .cloned()
                                        .unwrap_or_else(|| json!({}))
                                        .to_string(),
                                },
                            })
                        })
                        .collect();
                    let mut m = json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { json!(text) },
                    });
                    if !tool_calls.is_empty() {
                        m["tool_calls"] = Value::Array(tool_calls);
                    }
                    out.push(m);
                } else {
                    // Tool results first (OpenAI expects them to directly follow
                    // the assistant tool_calls message), then any leftover text.
                    for b in blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                    {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": b.get("tool_use_id").cloned().unwrap_or(Value::Null),
                            "content": tool_result_text(b),
                        }));
                    }
                    let text = joined_text(blocks);
                    if !text.is_empty() {
                        out.push(json!({ "role": role, "content": text }));
                    }
                }
            }
            _ => out.push(json!({ "role": role, "content": "" })),
        }
    }
    Value::Array(out)
}

/// Concatenate the `text` of every `text` block in a content array.
fn joined_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Flatten a canonical `tool_result` block's `content` into the string OpenAI's
/// `tool` role message requires.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let text = joined_text(blocks);
            if text.is_empty() && !blocks.is_empty() {
                Value::Array(blocks.clone()).to_string()
            } else {
                text
            }
        }
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// OpenAI Chat Completions response → canonical `{"content": [...]}`. Pure.
///
/// A plain `content` string becomes one `text` block; each `tool_calls` entry
/// becomes a `tool_use` block with `input` parsed from the `arguments` JSON
/// string. A malformed (non-JSON) `arguments` string is an `Err` — a
/// provider-side protocol violation for this attempt, never a silently broken
/// `input` reaching the engine.
fn from_openai_response(raw: &Value) -> Result<Value, String> {
    let message = raw
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .ok_or_else(|| "provider response is missing choices[0].message".to_string())?;

    let mut blocks: Vec<Value> = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| "tool_calls entry is missing function.name".to_string())?;
            let arguments = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let arguments = if arguments.trim().is_empty() {
                "{}"
            } else {
                arguments
            };
            let input: Value = serde_json::from_str(arguments).map_err(|e| {
                format!("tool_calls entry {name:?} has non-JSON function.arguments: {e}")
            })?;
            blocks.push(json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or(Value::Null),
                "name": name,
                "input": input,
            }));
        }
    }
    Ok(json!({ "content": blocks }))
}

/// reqwest errors can embed the request URL; strip it defensively so a resolved
/// token can never ride along in an error string that gets persisted.
fn sanitize_reqwest_error(e: reqwest::Error) -> String {
    let e = e.without_url();
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn substitutes_present_var() {
        let out = resolve_tokens_with("Bearer ${TOK}", &env(&[("TOK", "secret")])).unwrap();
        assert_eq!(out, "Bearer secret");
    }

    #[test]
    fn missing_var_is_error() {
        let err = resolve_tokens_with("${NOPE}", &env(&[])).unwrap_err();
        assert_eq!(err, ProviderConfigError::MissingEnv("NOPE".into()));
    }

    #[test]
    fn literal_dollar_passes_through() {
        let out = resolve_tokens_with("cost is $5 and $x", &env(&[])).unwrap();
        assert_eq!(out, "cost is $5 and $x");
    }

    #[test]
    fn multiple_tokens() {
        let out = resolve_tokens_with("${A}-${B}", &env(&[("A", "1"), ("B", "2")])).unwrap();
        assert_eq!(out, "1-2");
    }

    #[test]
    fn unterminated_is_error() {
        assert_eq!(
            resolve_tokens_with("${OPEN", &env(&[])).unwrap_err(),
            ProviderConfigError::Unterminated
        );
    }

    #[test]
    fn empty_and_plain_strings() {
        assert_eq!(resolve_tokens_with("", &env(&[])).unwrap(), "");
        assert_eq!(
            resolve_tokens_with("no tokens here", &env(&[])).unwrap(),
            "no tokens here"
        );
    }

    // -- effective_base_url -------------------------------------------------

    fn cfg(kind: ProviderKind, base_url: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            provider: kind,
            base_url: base_url.map(str::to_owned),
            model: "m".into(),
            auth_token: "t".into(),
            max_tokens: 4096,
        }
    }

    #[test]
    fn effective_base_url_defaults_per_kind() {
        assert_eq!(
            cfg(ProviderKind::Anthropic, None).effective_base_url(),
            "https://api.anthropic.com"
        );
        assert_eq!(
            cfg(ProviderKind::OpenAi, None).effective_base_url(),
            "https://api.openai.com"
        );
    }

    #[test]
    fn explicit_base_url_wins_verbatim_regardless_of_kind() {
        // The two are independent knobs: any kind may point anywhere.
        let gw = "https://llm-gateway.internal.example.com";
        assert_eq!(
            cfg(ProviderKind::Anthropic, Some(gw)).effective_base_url(),
            gw
        );
        assert_eq!(cfg(ProviderKind::OpenAi, Some(gw)).effective_base_url(), gw);
    }

    // -- resolve_from_profile -----------------------------------------------

    fn registry(names: &[&str]) -> HashMap<String, ProviderConfig> {
        names
            .iter()
            .map(|n| (n.to_string(), cfg(ProviderKind::Anthropic, None)))
            .collect()
    }

    #[test]
    fn primary_resolves_with_fallbacks() {
        let reg = registry(&["main", "backup"]);
        let (primary, fallbacks) =
            resolve_from_profile(&reg, "main", &["backup".to_string()]).unwrap();
        assert_eq!(primary.model, "m");
        assert_eq!(fallbacks.len(), 1);
    }

    #[test]
    fn missing_primary_is_fatal() {
        let reg = registry(&["main"]);
        let err = resolve_from_profile(&reg, "ghost", &[]).unwrap_err();
        assert_eq!(
            err,
            ProviderConfigError::PrimaryProviderMissing("ghost".into())
        );
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn missing_fallback_is_skipped_without_short_circuiting() {
        let reg = registry(&["main", "a", "b"]);
        let names = vec!["a".to_string(), "ghost".to_string(), "b".to_string()];
        let (_primary, fallbacks) = resolve_from_profile(&reg, "main", &names).unwrap();
        // The miss between two valid names must not drop the trailing valid one.
        assert_eq!(fallbacks.len(), 2);
    }

    #[test]
    fn all_fallbacks_missing_yields_empty_list_not_error() {
        let reg = registry(&["main"]);
        let (_primary, fallbacks) =
            resolve_from_profile(&reg, "main", &["x".to_string(), "y".to_string()]).unwrap();
        assert!(fallbacks.is_empty());
    }

    // -- OpenAI outgoing translation ------------------------------------------

    #[test]
    fn openai_tools_take_function_calling_shape() {
        let tools = json!([{
            "name": "get_time",
            "description": "Current time",
            "input_schema": { "type": "object", "properties": {} },
        }]);
        let out = to_openai_tools(&tools);
        assert_eq!(
            out,
            json!([{
                "type": "function",
                "function": {
                    "name": "get_time",
                    "description": "Current time",
                    "parameters": { "type": "object", "properties": {} },
                },
            }])
        );
    }

    #[test]
    fn openai_plain_text_message_passes_through() {
        let messages = json!([{ "role": "user", "content": "hello" }]);
        assert_eq!(
            to_openai_messages(&messages),
            json!([{ "role": "user", "content": "hello" }])
        );
    }

    #[test]
    fn openai_tool_result_blocks_split_into_tool_messages() {
        let messages = json!([{
            "role": "user",
            "content": [
                { "type": "tool_result", "tool_use_id": "tu_1", "content": "12:00 UTC" },
                { "type": "text", "text": "and carry on" },
            ],
        }]);
        assert_eq!(
            to_openai_messages(&messages),
            json!([
                { "role": "tool", "tool_call_id": "tu_1", "content": "12:00 UTC" },
                { "role": "user", "content": "and carry on" },
            ])
        );
    }

    #[test]
    fn openai_tool_result_block_content_array_is_flattened() {
        let messages = json!([{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": [{ "type": "text", "text": "echo: x" }],
            }],
        }]);
        assert_eq!(
            to_openai_messages(&messages),
            json!([{ "role": "tool", "tool_call_id": "tu_1", "content": "echo: x" }])
        );
    }

    #[test]
    fn openai_assistant_tool_use_becomes_tool_calls() {
        let messages = json!([{
            "role": "assistant",
            "content": [
                { "type": "text", "text": "let me check" },
                { "type": "tool_use", "id": "tu_1", "name": "get_time", "input": { "tz": "UTC" } },
            ],
        }]);
        let out = to_openai_messages(&messages);
        let m = &out[0];
        assert_eq!(m["role"], json!("assistant"));
        assert_eq!(m["content"], json!("let me check"));
        assert_eq!(m["tool_calls"][0]["id"], json!("tu_1"));
        assert_eq!(m["tool_calls"][0]["type"], json!("function"));
        assert_eq!(m["tool_calls"][0]["function"]["name"], json!("get_time"));
        // `arguments` is the JSON *string* OpenAI expects, not an object.
        let args: Value = serde_json::from_str(
            m["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(args, json!({ "tz": "UTC" }));
    }

    #[test]
    fn openai_request_body_omits_empty_tools() {
        let c = cfg(ProviderKind::OpenAi, None);
        let body = openai_request_body(&c, &json!([]), &json!([]));
        assert!(body.get("tools").is_none(), "empty tools must be omitted");
        let body = openai_request_body(&c, &json!([]), &json!([{ "name": "t" }]));
        assert!(body.get("tools").is_some());
    }

    // -- OpenAI incoming translation ------------------------------------------

    #[test]
    fn openai_plain_content_becomes_text_block() {
        let raw = json!({
            "choices": [{ "message": { "role": "assistant", "content": "hi there" } }],
        });
        assert_eq!(
            from_openai_response(&raw).unwrap(),
            json!({ "content": [{ "type": "text", "text": "hi there" }] })
        );
    }

    #[test]
    fn openai_tool_calls_become_tool_use_blocks() {
        let raw = json!({
            "choices": [{ "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "get_time", "arguments": "{\"tz\":\"UTC\"}" },
                }],
            } }],
        });
        assert_eq!(
            from_openai_response(&raw).unwrap(),
            json!({ "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": "get_time",
                "input": { "tz": "UTC" },
            }] })
        );
    }

    #[test]
    fn openai_malformed_arguments_is_an_error_not_a_broken_input() {
        let raw = json!({
            "choices": [{ "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "get_time", "arguments": "{ not json" },
                }],
            } }],
        });
        let err = from_openai_response(&raw).unwrap_err();
        assert!(err.contains("non-JSON"), "error should say why: {err}");
    }

    #[test]
    fn openai_missing_choices_is_an_error() {
        assert!(from_openai_response(&json!({})).is_err());
    }
}
