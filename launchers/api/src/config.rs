//! `bae-api.toml` / `bae-app.toml` config model and loading semantics.
//!
//! # Shape
//!
//! ```toml
//! [server]
//! addr = "0.0.0.0:9090"        # optional; BAE_LAUNCHER_API_ADDR overrides it
//!
//! [[agents]]
//! name    = "daily-digest"     # becomes POST /agents/daily-digest/trigger
//! command = "/usr/local/bin/daily-digest-harness"
//! args    = ["--mode", "digest"]
//! working_dir = "/app"
//!
//! [agents.request_schema]      # JSON Schema the POST body must satisfy
//! type = "object"
//! required = ["prompt"]
//! [agents.request_schema.properties.prompt]
//! type = "string"
//!
//! [agents.env]                 # optional static env; values may carry ${VAR}
//! API_TOKEN = "${MY_HARNESS_TOKEN}"
//!
//! [[agents.env_template]]      # validated body field -> child-process env var
//! field = "prompt"
//! env   = "AGENT_PROMPT"
//!
//! [[agents.arg_template]]      # validated body field -> appended CLI flag+value
//! field = "priority"
//! flag  = "--priority"
//! ```
//!
//! `bae-app.toml` (the webapp launcher) is this exact schema **plus** optional
//! presentation fields ([`AgentConfig::display_name`], `description`, `icon`,
//! `chat_input_field`, and `[[agents.prompts]]`). The **same binary** parses
//! both; the plain API launcher simply ignores the presentation fields. Keeping
//! the model complete here means the webapp step only adds behavior, never
//! config parsing.
//!
//! # Loading semantics (mirroring `bae-config.toml`, with one difference)
//!
//! A **missing** file is not fatal — [`load`] starts with zero agents and logs a
//! warning (an image built before its config is dropped in still starts). A file
//! that **exists but is malformed** — bad TOML, an invalid JSON Schema, or a
//! duplicate/blank agent `name` — is a fatal startup error, exit code 2 (a
//! broken config has no useful degraded mode).
//!
//! # Secrets
//!
//! Every **operator-authored** string that lands on the spawned command may
//! carry `${VAR}` references — [`AgentConfig::env`] values, an `env_template`
//! entry's `env`, an `arg_template` entry's `flag` — resolved at spawn time
//! only (see [`crate::template`]); an unset variable fails that invocation
//! (HTTP 500). Request-body-derived values are copied **verbatim**, never
//! `${VAR}`-resolved — a request is untrusted input and must never be a source
//! of the launcher's own secrets.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use launcher_core::LauncherError;

/// A failure loading the config. Every variant is fatal at startup; the
/// [`exit_code`](ConfigError::exit_code) mapping follows `aspec/uxui/cli.md`
/// (usage → 2, runtime → 1), matching `launcher_core::LauncherError`.
#[derive(Debug)]
pub enum ConfigError {
    /// The file exists but could not be read (permissions, I/O). Runtime (1).
    Read {
        /// The path we tried to read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or violates the config schema (unknown field,
    /// wrong type). A config-authoring usage error (2).
    Toml(String),
    /// An agent's `request_schema` is not a valid JSON Schema. Usage error (2) —
    /// rejected now, at boot, rather than failing every request later.
    Schema {
        /// The offending agent's `name`.
        name: String,
        /// The validator's own compile error.
        detail: String,
    },
    /// A duplicate or blank agent `name` (from `launcher_core`). Usage error (2).
    Name(LauncherError),
}

impl ConfigError {
    /// Process exit code: usage errors (malformed config the operator authored)
    /// map to 2; a read/IO failure maps to 1.
    pub fn exit_code(&self) -> u8 {
        match self {
            ConfigError::Read { .. } => 1,
            ConfigError::Toml(_) | ConfigError::Schema { .. } => 2,
            ConfigError::Name(e) => e.exit_code() as u8,
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "cannot read config file {path:?}: {source}")
            }
            ConfigError::Toml(detail) => write!(f, "malformed config: {detail}"),
            ConfigError::Schema { name, detail } => write!(
                f,
                "agent {name:?} has an invalid request_schema (JSON Schema): {detail}"
            ),
            ConfigError::Name(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The whole `bae-api.toml` / `bae-app.toml` document.
///
/// `deny_unknown_fields` at every level, top level included: a config that
/// parses as TOML but uses the wrong shape — the forbidden singular `[agent]`
/// table, or a typo like `agnts` — must be a fatal startup error (exit 2), not
/// a silently-ignored key that degrades the launcher to zero agents (work item
/// 0014's "`[[agents]]` from day one" and "malformed config is fatal" rules).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    /// Optional `[server]` table (currently just `addr`).
    #[serde(default)]
    pub server: ServerConfig,
    /// The `[[agents]]` array. Absent or empty is valid (zero agents).
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

/// The optional `[server]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Listen address; overridden by `BAE_LAUNCHER_API_ADDR`, then defaulted to
    /// `0.0.0.0:9090`.
    #[serde(default)]
    pub addr: Option<String>,
}

/// One `[[agents]]` entry.
///
/// Fields split into three groups: **launch** (`name`/`command`/`args`/
/// `working_dir`/`env`), **API** (`request_schema`/`env_template`/`arg_template`),
/// and **presentation** (`display_name`/`description`/`icon`/`chat_input_field`/
/// `prompts` — used only by the webapp launcher, parsed-and-ignored by the plain
/// API launcher).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Unique agent name — the URL path segment, the log-prefix key, and the
    /// webapp card key all at once.
    pub name: String,
    /// The executable to spawn (PATH-resolved if not absolute).
    pub command: String,
    /// Base CLI arguments, always passed; `arg_template` values are appended
    /// after these.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the child; `None` inherits the launcher's.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Static per-agent environment. Values may carry `${VAR}` tokens, resolved
    /// against the launcher's own environment at spawn time (as may
    /// `env_template.env` / `arg_template.flag` strings — but a request body
    /// never is). Never exposed by the introspection routes.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// JSON Schema the POST body must satisfy. `None` accepts any JSON body.
    #[serde(default)]
    pub request_schema: Option<serde_json::Value>,
    /// Maps a validated body field to a child-process env var.
    #[serde(default)]
    pub env_template: Vec<EnvTemplate>,
    /// Maps a validated body field to an appended CLI flag+value.
    #[serde(default)]
    pub arg_template: Vec<ArgTemplate>,

    // ---- presentation (webapp launcher; ignored by the plain API launcher) ----
    /// Human-friendly name for the webapp card/detail header.
    #[serde(default)]
    pub display_name: Option<String>,
    /// One-line description for the webapp card.
    #[serde(default)]
    pub description: Option<String>,
    /// An emoji or image URL for the webapp card.
    #[serde(default)]
    pub icon: Option<String>,
    /// Which `request_schema` field the chat box's free-text input fills.
    /// Defaults to `"prompt"`.
    #[serde(default = "default_chat_input_field")]
    pub chat_input_field: String,
    /// Pre-defined prompt buttons for the webapp chat view.
    #[serde(default)]
    pub prompts: Vec<PromptConfig>,
}

/// Default for [`AgentConfig::chat_input_field`].
fn default_chat_input_field() -> String {
    "prompt".to_string()
}

/// A `[[agents.env_template]]` entry: copy validated body `field` into env `env`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvTemplate {
    /// The body field name to read.
    pub field: String,
    /// The child-process env var to set to that field's value. May carry
    /// `${VAR}` tokens, resolved against the launcher's environment at spawn
    /// time (an unset variable fails that invocation with a 500).
    pub env: String,
}

/// A `[[agents.arg_template]]` entry: append `flag` then validated body `field`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgTemplate {
    /// The body field name to read.
    pub field: String,
    /// The CLI flag to append before that field's value. May carry `${VAR}`
    /// tokens, resolved like [`EnvTemplate::env`].
    pub flag: String,
}

/// A `[[agents.prompts]]` entry (webapp launcher): one pre-defined-prompt button.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    /// The button label shown in the webapp.
    pub label: String,
    /// The prompt text sent when the button is clicked.
    pub prompt: String,
}

/// An agent with its `request_schema` compiled once at startup.
///
/// Holds the parsed [`AgentConfig`] and, when a schema was supplied, its
/// compiled [`jsonschema::Validator`] (shared across concurrent requests). The
/// validator is built at load time so an invalid schema is a fatal startup
/// error, not a per-request surprise.
pub struct Agent {
    /// The parsed config for this agent.
    pub config: AgentConfig,
    /// Compiled request-body validator, if a `request_schema` was configured.
    pub validator: Option<jsonschema::Validator>,
}

/// A fully-loaded, ready-to-serve configuration.
pub struct LoadedConfig {
    /// The `[server] addr`, if the file set one.
    pub addr: Option<String>,
    /// Agents in config order (the introspection list preserves this order),
    /// each wrapped in an `Arc` for cheap cloning into the shared router state.
    pub agents: Vec<Arc<Agent>>,
}

/// Load and validate the config at `path`.
///
/// A missing file yields an empty config with a warning (not fatal). Any other
/// failure — unreadable file, bad TOML, invalid JSON Schema, or a duplicate/
/// blank `name` — is a fatal [`ConfigError`].
pub fn load(path: &str) -> Result<LoadedConfig, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                path,
                "no config file found; starting with zero configured agents"
            );
            return Ok(LoadedConfig {
                addr: None,
                agents: Vec::new(),
            });
        }
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            })
        }
    };

    let parsed: ApiConfig = toml::from_str(&raw).map_err(|e| ConfigError::Toml(e.to_string()))?;

    // Fatal on a duplicate or blank name — the most likely multi-agent mistake
    // (a copy-pasted `[[agents]]` block with an unchanged `name`).
    launcher_core::validate_unique_names(parsed.agents.iter().map(|a| a.name.as_str()))
        .map_err(ConfigError::Name)?;

    let mut agents = Vec::with_capacity(parsed.agents.len());
    for config in parsed.agents {
        let validator = match &config.request_schema {
            Some(schema) => {
                Some(
                    jsonschema::validator_for(schema).map_err(|e| ConfigError::Schema {
                        name: config.name.clone(),
                        detail: e.to_string(),
                    })?,
                )
            }
            None => None,
        };
        agents.push(Arc::new(Agent { config, validator }));
    }

    if agents.is_empty() {
        tracing::warn!(path, "config parsed but declares zero agents");
    }

    Ok(LoadedConfig {
        addr: parsed.server.addr,
        agents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture(contents: &str) -> PathBuf {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("baeapi-test-{}-{id}.toml", std::process::id()));
        fs::write(&path, contents).expect("write fixture");
        path
    }

    fn valid_config() -> &'static str {
        r#"
[server]
addr = "127.0.0.1:9999"

[[agents]]
name = "alpha"
command = "echo"
env = { API_TOKEN = "${ALPHA_TOKEN}" }
[agents.request_schema]
type = "object"
required = ["prompt"]
[agents.request_schema.properties.prompt]
type = "string"

[[agents]]
name = "beta"
command = "echo"
[agents.request_schema]
type = "object"
required = ["text"]
[agents.request_schema.properties.text]
type = "string"
"#
    }

    #[test]
    fn valid_multi_agent_config_preserves_order_and_schema() {
        let path = fixture(valid_config());
        let loaded = load(path.to_str().unwrap()).expect("valid config");
        fs::remove_file(path).ok();
        assert_eq!(loaded.addr.as_deref(), Some("127.0.0.1:9999"));
        assert_eq!(loaded.agents.len(), 2);
        assert_eq!(loaded.agents[0].config.name, "alpha");
        assert_eq!(loaded.agents[1].config.name, "beta");
        assert!(loaded.agents.iter().all(|agent| agent.validator.is_some()));
    }

    #[test]
    fn malformed_toml_is_fatal_exit_two() {
        let path = fixture(
            r#"
[[agents]]
name = "alpha"
command = [not valid

[[agents]]
name = "beta"
command = "echo"
"#,
        );
        let error = match load(path.to_str().unwrap()) {
            Ok(_) => panic!("malformed TOML must fail"),
            Err(error) => error,
        };
        fs::remove_file(path).ok();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("malformed config"));
    }

    #[test]
    fn invalid_schema_names_agent_and_is_fatal_exit_two() {
        let path = fixture(
            r#"
[[agents]]
name = "alpha"
command = "echo"
[agents.request_schema]
type = "not-a-json-schema-type"

[[agents]]
name = "beta"
command = "echo"
"#,
        );
        let error = match load(path.to_str().unwrap()) {
            Ok(_) => panic!("invalid schema must fail"),
            Err(error) => error,
        };
        fs::remove_file(path).ok();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("alpha"));
        assert!(error.to_string().contains("invalid request_schema"));
    }

    #[test]
    fn duplicate_name_names_offender_and_is_fatal_exit_two() {
        let path = fixture(&valid_config().replace("name = \"beta\"", "name = \"alpha\""));
        let error = match load(path.to_str().unwrap()) {
            Ok(_) => panic!("duplicate name must fail"),
            Err(error) => error,
        };
        fs::remove_file(path).ok();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("duplicate agent name \"alpha\""));
    }

    #[test]
    fn singular_agent_table_and_unknown_top_level_keys_are_fatal_exit_two() {
        // The forbidden singular `[agent]` shape must never silently degrade
        // into a zero-agent launcher.
        for contents in [
            "[agent]\nname = \"legacy\"\ncommand = \"echo\"\n",
            "[[agnts]]\nname = \"typo\"\ncommand = \"echo\"\n",
        ] {
            let path = fixture(contents);
            let error = match load(path.to_str().unwrap()) {
                Ok(_) => panic!("wrong-shape config must fail: {contents}"),
                Err(error) => error,
            };
            fs::remove_file(path).ok();
            assert_eq!(error.exit_code(), 2, "for {contents}");
            assert!(error.to_string().contains("malformed config"));
        }
    }

    #[test]
    fn tens_of_agents_parse_and_stay_in_config_order() {
        // No hard cap in V1: a config with tens of agents loads in one pass.
        let mut contents = String::new();
        for i in 0..30 {
            contents.push_str(&format!(
                "[[agents]]\nname = \"agent-{i:02}\"\ncommand = \"echo\"\n\n"
            ));
        }
        let path = fixture(&contents);
        let loaded = load(path.to_str().unwrap()).expect("30 agents");
        fs::remove_file(path).ok();
        assert_eq!(loaded.agents.len(), 30);
        assert_eq!(loaded.agents[0].config.name, "agent-00");
        assert_eq!(loaded.agents[29].config.name, "agent-29");
    }

    #[test]
    fn missing_config_is_a_nonfatal_empty_config() {
        let path = std::env::temp_dir().join(format!(
            "baeapi-missing-{}-{}.toml",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let loaded = load(path.to_str().unwrap()).expect("missing config");
        assert!(loaded.agents.is_empty());
    }

    #[test]
    fn one_agent_config_remains_supported() {
        let path = fixture(
            r#"
[[agents]]
name = "only"
command = "echo"
"#,
        );
        let loaded = load(path.to_str().unwrap()).expect("single agent");
        fs::remove_file(path).ok();
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].config.name, "only");
    }
}
