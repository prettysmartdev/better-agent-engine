//! `bae-config.toml` — the optional file-driven configuration.
//!
//! This is deliberately kept separate from [`crate::config`] (the `BAE_*`
//! environment-driven [`Config`](crate::config::Config)): the two have
//! different failure semantics. Env/flag config errors are usage errors at
//! every startup, whereas a **missing** config file — whether because neither
//! `--config` nor `BAE_CONFIG` was set, or because the resolved path does not
//! exist — is not an error at all: the server simply starts with an empty MCP
//! server registry (fully consistent with the "opt-in" model, where nothing is
//! available unless explicitly configured).
//!
//! # Shape
//!
//! MCP servers and LLM providers are **not** top-level entries; they live
//! under top-level `[mcp]` / `[providers]` tables so further sections (e.g.
//! `[logging]`, `[limits]`) can be added without restructuring:
//!
//! ```toml
//! [mcp]
//!
//! [[mcp.servers]]
//! name = "filesystem"
//! transport = "stdio"
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
//!
//! [[mcp.servers]]
//! name = "search"
//! transport = "sse"
//! url = "https://mcp.example.com/sse"
//! headers = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
//!
//! [providers]
//!
//! [[providers.entries]]
//! name       = "anthropic-sonnet"
//! provider   = "anthropic"
//! model      = "claude-sonnet-4-6"
//! auth_token = "${ANTHROPIC_API_KEY}"
//! max_tokens = 8096
//! ```
//!
//! A file with no `[mcp]` (or `[providers]`) table at all is valid and yields
//! an empty registry for that section, exactly like a missing file. The two
//! sections are **separate registries** with no shared namespace: an MCP
//! server and a provider may safely share a name.
//!
//! # Secrets
//!
//! `headers` and provider `auth_token` values may carry `${ENV_VAR}` tokens.
//! They are **not** resolved here: the raw tokens are preserved on
//! [`McpServerConfig`] / [`ProviderConfig`] and resolved immediately before
//! connecting/calling, via [`crate::engine::provider::resolve_tokens`], so the
//! resolved secret is never persisted.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::provider::ProviderConfig;

/// The parsed contents of a `bae-config.toml`.
///
/// The top-level struct deliberately has room for sibling sections later; only
/// `mcp` exists today. Unknown top-level sections are ignored (not rejected) so
/// a newer config on an older binary stays forward-compatible; typo protection
/// applies within the known sections via `deny_unknown_fields` there. A
/// document root with no known sections deserializes to all-`None`, i.e.
/// [`BaeConfig::default`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BaeConfig {
    /// The optional MCP server section. Absent → no configured MCP servers.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
    /// The optional LLM provider section. Absent → no configured providers.
    #[serde(default)]
    pub providers: Option<ProvidersConfig>,
}

/// The `[mcp]` section: a list of configured MCP servers.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Every `[[mcp.servers]]` entry. May be empty.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// One configured MCP server (`[[mcp.servers]]`).
///
/// Transport-specific fields are optional at the type level and validated per
/// transport when the registry is built (see [`BaeConfig::mcp_registry`]):
/// `stdio` needs `command`; `sse`/`http` need `url`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Unique name; profiles opt in by this name. Unique within `mcp.servers`.
    pub name: String,
    /// How the server is reached.
    pub transport: McpTransport,
    /// `stdio`: the executable to spawn (e.g. `npx`, `uvx`).
    #[serde(default)]
    pub command: Option<String>,
    /// `stdio`: arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// `sse`/`http`: the endpoint URL.
    #[serde(default)]
    pub url: Option<String>,
    /// `sse`/`http`: extra request headers. Values may contain unresolved
    /// `${ENV_VAR}` tokens, resolved only at connect time — never persisted.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// The `[providers]` section: a list of configured LLM providers.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    /// Every `[[providers.entries]]` entry. May be empty.
    #[serde(default)]
    pub entries: Vec<NamedProviderConfig>,
}

/// One configured provider (`[[providers.entries]]`): a registry name plus the
/// call configuration itself, which is the engine's own [`ProviderConfig`]
/// flattened in (no duplicated field definitions). An unsupported `provider`
/// value is rejected at parse time as an unknown [`ProviderKind`] variant,
/// exactly like an unsupported MCP `transport`.
///
/// [`ProviderKind`]: crate::engine::provider::ProviderKind
// NOTE: no `deny_unknown_fields` here — serde does not support it together
// with `#[serde(flatten)]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NamedProviderConfig {
    /// Unique name; profiles reference providers by this name. Unique within
    /// `providers.entries` (but independent of `mcp.servers` names).
    pub name: String,
    /// The call configuration. `${ENV_VAR}` tokens in `auth_token` are
    /// preserved raw here and resolved only at call time.
    #[serde(flatten)]
    pub config: ProviderConfig,
}

/// Supported MCP transports. Any other value is rejected at parse time with a
/// clear "unknown variant" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn a subprocess and speak MCP over its stdio.
    Stdio,
    /// Server-Sent Events over HTTP.
    Sse,
    /// Streamable HTTP transport.
    Http,
}

impl McpTransport {
    /// The wire/config string for this transport (matches the TOML value).
    pub fn as_str(&self) -> &'static str {
        match self {
            McpTransport::Stdio => "stdio",
            McpTransport::Sse => "sse",
            McpTransport::Http => "http",
        }
    }
}

/// A problem loading or validating `bae-config.toml`.
///
/// These are operator **authoring** errors (a file that exists but is wrong),
/// treated as usage errors (exit code 2) — distinct from a missing file, which
/// is not an error at all. See [`ConfigFileError::exit_code`].
#[derive(Debug)]
pub enum ConfigFileError {
    /// The file exists but could not be read (e.g. permission denied). A
    /// not-found error is deliberately *not* surfaced here — it maps to an
    /// empty registry instead.
    Read {
        path: String,
        source: std::io::Error,
    },
    /// The file could not be parsed as TOML, or an entry did not match the
    /// schema (including an unsupported `transport` value).
    Parse {
        path: String,
        source: toml::de::Error,
    },
    /// Two `[[mcp.servers]]` entries share a `name`.
    DuplicateServer(String),
    /// An `[[mcp.servers]]` entry has a blank `name`.
    EmptyName,
    /// Two `[[providers.entries]]` entries share a `name`.
    DuplicateProvider(String),
    /// A `[[providers.entries]]` entry has a blank `name`.
    EmptyProviderName,
    /// An entry is missing a field its transport requires.
    MissingField {
        server: String,
        transport: &'static str,
        field: &'static str,
    },
}

impl ConfigFileError {
    /// Process exit code — always 2 (usage error) per `aspec/uxui/cli.md`.
    pub fn exit_code(&self) -> i32 {
        2
    }
}

impl std::fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigFileError::Read { path, source } => {
                write!(f, "cannot read config file {path:?}: {source}")
            }
            ConfigFileError::Parse { path, source } => {
                write!(f, "invalid config file {path:?}: {source}")
            }
            ConfigFileError::DuplicateServer(name) => {
                write!(f, "duplicate MCP server name {name:?} in [mcp.servers]")
            }
            ConfigFileError::EmptyName => {
                write!(f, "an [[mcp.servers]] entry has an empty name")
            }
            ConfigFileError::DuplicateProvider(name) => {
                write!(f, "duplicate provider name {name:?} in [providers.entries]")
            }
            ConfigFileError::EmptyProviderName => {
                write!(f, "a [[providers.entries]] entry has an empty name")
            }
            ConfigFileError::MissingField {
                server,
                transport,
                field,
            } => write!(
                f,
                "MCP server {server:?} uses transport {transport:?} but is missing required field {field:?}"
            ),
        }
    }
}

impl std::error::Error for ConfigFileError {}

/// Loader for `bae-config.toml`. A thin namespace so the loading concern stays
/// visibly separate from env-driven [`Config`](crate::config::Config).
pub struct BaeConfigFile;

impl BaeConfigFile {
    /// Load and parse the config file at `path`.
    ///
    /// - `path` is `None`, or the file does not exist → [`BaeConfig::default`]
    ///   (an empty registry), with no error.
    /// - The file exists but cannot be read (other than not-found) → a
    ///   [`ConfigFileError::Read`].
    /// - The file exists but is malformed TOML / schema → a
    ///   [`ConfigFileError::Parse`].
    ///
    /// Structural validation (duplicate names, missing per-transport fields) is
    /// performed later, in [`BaeConfig::mcp_registry`].
    pub fn load(path: Option<&Path>) -> Result<BaeConfig, ConfigFileError> {
        let Some(path) = path else {
            return Ok(BaeConfig::default());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // A missing file is explicitly not an error: empty registry.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BaeConfig::default());
            }
            Err(source) => {
                return Err(ConfigFileError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| ConfigFileError::Parse {
            path: path.display().to_string(),
            source,
        })
    }
}

impl BaeConfig {
    /// Build the `name -> config` MCP server registry, validating structure.
    ///
    /// Fails startup (usage error) on a duplicate name, a blank name, or an
    /// entry missing a field its transport requires. A config with no `[mcp]`
    /// table yields an empty registry.
    pub fn mcp_registry(&self) -> Result<HashMap<String, McpServerConfig>, ConfigFileError> {
        let mut registry: HashMap<String, McpServerConfig> = HashMap::new();
        let servers = self
            .mcp
            .as_ref()
            .map(|m| m.servers.as_slice())
            .unwrap_or(&[]);
        for server in servers {
            if server.name.trim().is_empty() {
                return Err(ConfigFileError::EmptyName);
            }
            match server.transport {
                McpTransport::Stdio => {
                    if server.command.as_deref().unwrap_or("").trim().is_empty() {
                        return Err(ConfigFileError::MissingField {
                            server: server.name.clone(),
                            transport: "stdio",
                            field: "command",
                        });
                    }
                }
                McpTransport::Sse | McpTransport::Http => {
                    if server.url.as_deref().unwrap_or("").trim().is_empty() {
                        return Err(ConfigFileError::MissingField {
                            server: server.name.clone(),
                            transport: server.transport.as_str(),
                            field: "url",
                        });
                    }
                }
            }
            if registry
                .insert(server.name.clone(), server.clone())
                .is_some()
            {
                return Err(ConfigFileError::DuplicateServer(server.name.clone()));
            }
        }
        Ok(registry)
    }

    /// Build the `name -> config` provider registry, validating structure.
    ///
    /// Fails startup (usage error, exit 2) on a duplicate or blank name,
    /// mirroring [`BaeConfig::mcp_registry`]. A config with no `[providers]`
    /// table yields an empty registry. Names are checked only *within* this
    /// section — a name shared with an `[[mcp.servers]]` entry is fine, since
    /// the two registries have no shared namespace.
    pub fn provider_registry(&self) -> Result<HashMap<String, ProviderConfig>, ConfigFileError> {
        let mut registry: HashMap<String, ProviderConfig> = HashMap::new();
        let entries = self
            .providers
            .as_ref()
            .map(|p| p.entries.as_slice())
            .unwrap_or(&[]);
        for entry in entries {
            if entry.name.trim().is_empty() {
                return Err(ConfigFileError::EmptyProviderName);
            }
            if registry
                .insert(entry.name.clone(), entry.config.clone())
                .is_some()
            {
                return Err(ConfigFileError::DuplicateProvider(entry.name.clone()));
            }
        }
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Parse a TOML string and build its registry in one step (the common path).
    fn registry_from(toml_str: &str) -> Result<HashMap<String, McpServerConfig>, ConfigFileError> {
        let cfg: BaeConfig = toml::from_str(toml_str).unwrap();
        cfg.mcp_registry()
    }

    #[test]
    fn none_path_is_empty_registry() {
        let cfg = BaeConfigFile::load(None).unwrap();
        assert!(cfg.mcp_registry().unwrap().is_empty());
    }

    #[test]
    fn missing_file_is_empty_registry() {
        let path = std::env::temp_dir().join("baesrv-does-not-exist-xyz.toml");
        let cfg = BaeConfigFile::load(Some(&path)).unwrap();
        assert!(cfg.mcp_registry().unwrap().is_empty());
    }

    #[test]
    fn empty_mcp_table_is_empty_registry() {
        assert!(registry_from("[mcp]\n").unwrap().is_empty());
        // No sections at all is also valid.
        assert!(registry_from("").unwrap().is_empty());
    }

    #[test]
    fn parses_stdio_and_sse_entries() {
        let reg = registry_from(
            r#"
            [mcp]
            [[mcp.servers]]
            name = "filesystem"
            transport = "stdio"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]

            [[mcp.servers]]
            name = "search"
            transport = "sse"
            url = "https://mcp.example.com/sse"
            headers = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
            "#,
        )
        .unwrap();
        assert_eq!(reg.len(), 2);
        let fs = &reg["filesystem"];
        assert_eq!(fs.transport, McpTransport::Stdio);
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args.len(), 3);
        let search = &reg["search"];
        assert_eq!(search.transport, McpTransport::Sse);
        // ${ENV_VAR} tokens are preserved raw, never resolved at parse time.
        assert_eq!(
            search.headers.get("Authorization").map(String::as_str),
            Some("Bearer ${SEARCH_MCP_TOKEN}")
        );
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let err = registry_from(
            r#"
            [[mcp.servers]]
            name = "dup"
            transport = "stdio"
            command = "a"
            [[mcp.servers]]
            name = "dup"
            transport = "stdio"
            command = "b"
            "#,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(err, ConfigFileError::DuplicateServer(n) if n == "dup"));
    }

    #[test]
    fn unsupported_transport_is_rejected() {
        // Rejected at parse time as an unknown enum variant.
        let cfg: Result<BaeConfig, _> = toml::from_str(
            r#"
            [[mcp.servers]]
            name = "x"
            transport = "carrier-pigeon"
            "#,
        );
        assert!(cfg.is_err());
    }

    #[test]
    fn stdio_without_command_is_rejected() {
        let err = registry_from(
            r#"
            [[mcp.servers]]
            name = "x"
            transport = "stdio"
            "#,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigFileError::MissingField {
                field: "command",
                ..
            }
        ));
    }

    #[test]
    fn sse_without_url_is_rejected() {
        let err = registry_from(
            r#"
            [[mcp.servers]]
            name = "x"
            transport = "sse"
            "#,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigFileError::MissingField { field: "url", .. }
        ));
    }

    #[test]
    fn header_env_substitution_resolves() {
        // `${ENV_VAR}` tokens in headers are preserved raw at parse time and
        // resolved at connect time via the shared provider token resolver.
        let reg = registry_from(
            r#"
            [[mcp.servers]]
            name = "search"
            transport = "sse"
            url = "https://mcp.example.com/sse"
            headers = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
            "#,
        )
        .unwrap();
        let raw = reg["search"].headers.get("Authorization").unwrap();
        assert_eq!(raw, "Bearer ${SEARCH_MCP_TOKEN}", "token preserved raw");

        let resolved = crate::engine::provider::resolve_tokens_with(raw, &|k| {
            (k == "SEARCH_MCP_TOKEN").then(|| "s3cr3t".to_string())
        })
        .unwrap();
        assert_eq!(resolved, "Bearer s3cr3t");
    }

    #[test]
    fn header_env_substitution_missing_var_errors_clearly() {
        let reg = registry_from(
            r#"
            [[mcp.servers]]
            name = "search"
            transport = "sse"
            url = "https://mcp.example.com/sse"
            headers = { Authorization = "Bearer ${SEARCH_MCP_TOKEN}" }
            "#,
        )
        .unwrap();
        let raw = reg["search"].headers.get("Authorization").unwrap();
        // An absent variable is a clear, named error — never a silent empty value.
        let err = crate::engine::provider::resolve_tokens_with(raw, &|_| None).unwrap_err();
        assert_eq!(
            err,
            crate::engine::provider::ProviderConfigError::MissingEnv("SEARCH_MCP_TOKEN".into())
        );
        assert!(
            err.to_string().contains("SEARCH_MCP_TOKEN"),
            "error must name the missing variable: {err}"
        );
    }

    // -- [providers] ----------------------------------------------------------

    /// Parse a TOML string and build its provider registry in one step.
    fn provider_registry_from(
        toml_str: &str,
    ) -> Result<HashMap<String, crate::engine::provider::ProviderConfig>, ConfigFileError> {
        let cfg: BaeConfig = toml::from_str(toml_str).unwrap();
        cfg.provider_registry()
    }

    #[test]
    fn absent_providers_table_is_empty_registry() {
        assert!(provider_registry_from("").unwrap().is_empty());
        assert!(provider_registry_from("[providers]\n").unwrap().is_empty());
        // A missing file also yields an empty provider registry.
        let cfg = BaeConfigFile::load(None).unwrap();
        assert!(cfg.provider_registry().unwrap().is_empty());
    }

    #[test]
    fn parses_provider_entries_with_and_without_base_url() {
        use crate::engine::provider::ProviderKind;
        let reg = provider_registry_from(
            r#"
            [providers]
            [[providers.entries]]
            name = "anthropic-sonnet"
            provider = "anthropic"
            model = "claude-sonnet-4-6"
            auth_token = "${ANTHROPIC_API_KEY}"
            max_tokens = 8096

            [[providers.entries]]
            name = "self-hosted-openai"
            provider = "openai"
            base_url = "https://llm-gateway.internal.example.com"
            model = "gpt-5"
            auth_token = "${INTERNAL_GATEWAY_TOKEN}"
            "#,
        )
        .unwrap();
        assert_eq!(reg.len(), 2);
        let anth = &reg["anthropic-sonnet"];
        assert_eq!(anth.provider, ProviderKind::Anthropic);
        assert_eq!(anth.base_url, None);
        assert_eq!(anth.effective_base_url(), "https://api.anthropic.com");
        assert_eq!(anth.max_tokens, 8096);
        // ${ENV_VAR} tokens are preserved raw, never resolved at parse time.
        assert_eq!(anth.auth_token, "${ANTHROPIC_API_KEY}");
        let gw = &reg["self-hosted-openai"];
        assert_eq!(gw.provider, ProviderKind::OpenAi);
        // An explicit base_url is used verbatim regardless of kind.
        assert_eq!(
            gw.effective_base_url(),
            "https://llm-gateway.internal.example.com"
        );
        // max_tokens omitted → the schema default.
        assert_eq!(gw.max_tokens, 4096);
    }

    #[test]
    fn duplicate_provider_name_is_rejected() {
        let err = provider_registry_from(
            r#"
            [[providers.entries]]
            name = "dup"
            provider = "anthropic"
            model = "m"
            auth_token = "t"
            [[providers.entries]]
            name = "dup"
            provider = "openai"
            model = "m"
            auth_token = "t"
            "#,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(err, ConfigFileError::DuplicateProvider(n) if n == "dup"));
    }

    #[test]
    fn blank_provider_name_is_rejected() {
        let err = provider_registry_from(
            r#"
            [[providers.entries]]
            name = "  "
            provider = "anthropic"
            model = "m"
            auth_token = "t"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigFileError::EmptyProviderName));
    }

    #[test]
    fn unsupported_provider_kind_is_rejected_at_parse_time() {
        // Rejected as an unknown enum variant, like an unsupported transport.
        let cfg: Result<BaeConfig, _> = toml::from_str(
            r#"
            [[providers.entries]]
            name = "x"
            provider = "cohere"
            model = "m"
            auth_token = "t"
            "#,
        );
        assert!(cfg.is_err());
    }

    #[test]
    fn provider_and_mcp_names_may_collide_across_sections() {
        // Separate registries, no shared namespace.
        let toml_str = r#"
            [[mcp.servers]]
            name = "shared"
            transport = "stdio"
            command = "npx"

            [[providers.entries]]
            name = "shared"
            provider = "anthropic"
            model = "m"
            auth_token = "t"
        "#;
        let cfg: BaeConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.mcp_registry().unwrap().contains_key("shared"));
        assert!(cfg.provider_registry().unwrap().contains_key("shared"));
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        // Unique-enough name without a dev-dependency: thread id + a stack address.
        let name = format!(
            "baesrv-cfgtest-{:?}-{:p}.toml",
            std::thread::current().id(),
            &0u8 as *const u8
        );
        let path = std::env::temp_dir().join(name);
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "this is not = = toml").unwrap();
        }
        let err = BaeConfigFile::load(Some(&path)).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ConfigFileError::Parse { .. }));
    }
}
