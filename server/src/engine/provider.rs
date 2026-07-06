//! Provider (LLM) configuration and the outbound HTTP call.
//!
//! A profile carries a primary [`ProviderConfig`] plus ordered fallbacks, each
//! matching the schema in `aspec/work-items/0002-session-and-auth.md`:
//!
//! ```json
//! {
//!   "provider": "anthropic",
//!   "base_url": "https://api.anthropic.com",
//!   "model": "claude-sonnet-4-6",
//!   "auth_token": "${ANTHROPIC_API_KEY}",
//!   "max_tokens": 8096
//! }
//! ```
//!
//! # Secret handling
//!
//! `auth_token` may contain `${ENV_VAR_NAME}` tokens. They are resolved by
//! [`resolve_tokens`] **immediately before** the HTTP call, held only in a local
//! for the duration of that call, and never written to an event, a log line, or
//! the database. An unset variable is a [`ProviderConfigError`], not a silently
//! blank token (a blank token would just produce opaque 401s from the provider).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One provider configuration (primary or a fallback).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// May contain `${ENV_VAR_NAME}` tokens; resolved only at call time.
    pub auth_token: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    4096
}

/// Parse the primary config and the fallback list out of a profile's JSON blobs.
/// A malformed primary config is a hard error; a malformed fallback list is
/// treated as "no fallbacks".
pub fn configs_from_profile(
    provider_config: &Value,
    fallback_configs: &Value,
) -> Result<(ProviderConfig, Vec<ProviderConfig>), ProviderConfigError> {
    let primary: ProviderConfig = serde_json::from_value(provider_config.clone())
        .map_err(|e| ProviderConfigError::Malformed(e.to_string()))?;
    let fallbacks: Vec<ProviderConfig> =
        serde_json::from_value(fallback_configs.clone()).unwrap_or_default();
    Ok((primary, fallbacks))
}

/// Error resolving provider configuration or its `${ENV_VAR}` tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderConfigError {
    /// A referenced environment variable was not set at call time.
    MissingEnv(String),
    /// An unterminated `${…` token.
    Unterminated,
    /// The provider config JSON did not match [`ProviderConfig`].
    Malformed(String),
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
    /// The request was sent but the transport failed (DNS, connect, timeout).
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

/// Perform one provider call.
///
/// Resolves `auth_token` here and holds it only for this function's body. Sends
/// an Anthropic Messages-API-shaped request to `{base_url}/v1/messages` with the
/// given `messages` and `tools`, returning the parsed JSON response on a 2xx and
/// a [`ProviderCallError`] otherwise. The resolved token is never logged.
pub async fn call(
    http: &reqwest::Client,
    cfg: &ProviderConfig,
    messages: &Value,
    tools: &Value,
) -> Result<Value, ProviderCallError> {
    let token = resolve_tokens(&cfg.auth_token).map_err(ProviderCallError::Config)?;

    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let request_body = serde_json::json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens,
        "messages": messages,
        "tools": tools,
    });

    let resp = http
        .post(&url)
        .header("x-api-key", &token)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
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
        serde_json::from_str(&text).map_err(|e| {
            ProviderCallError::Transport(format!("provider returned non-JSON body: {e}"))
        })
    } else {
        Err(ProviderCallError::Status {
            status: status.as_u16(),
            body: text,
        })
    }
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
}
