//! Server-side tests for the read-only admin endpoint `GET /admin/v1/config`
//! (work item 0011).
//!
//! Boots the **real** admin router (auth disabled, the same shape
//! `tests/integration.rs` and `tests/admin_auth.rs` use) against a
//! `bae-config.toml` fixture loaded through the real `BaeConfigFile` loader +
//! validator, exactly as `AppState`'s `mcp_registry` / `provider_registry` /
//! `telemetry_config` are built at startup. Admin-auth *enforcement* of this
//! route (401 without a key, 200 with one, open when auth is disabled) is
//! covered by the table-driven route matrix in `tests/admin_auth.rs`, not
//! here.
//!
//! Coverage (see `/awman/context/workflow/server-test-plan.md`):
//! - the full response shape against a fixture with a stdio MCP server (with
//!   `args`), an sse MCP server (with a `${ENV_VAR}`-style header), two
//!   providers (one explicit `base_url`, one on the kind default), and an
//!   enabled `[telemetry]` section with an `otlp_endpoint`, an `otlp_headers`
//!   entry, a non-default `sample_ratio`/`service_name`, and a
//!   `metrics.disabled` entry;
//! - literal (non-`${...}`) secrets are masked unconditionally across all
//!   three secret-bearing fields;
//! - an absent config (no file, and a file with none of the three tables)
//!   yields empty lists plus default-disabled telemetry, `200 OK`;
//! - telemetry's effective `service_name` default (`"baesrv"`, never `null`)
//!   when the section is enabled but `service_name` is unset.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use baesrv::api::{admin, AppState};
use baesrv::config_file::BaeConfigFile;
use baesrv::store::{generate_id, Store};
use serde_json::{json, Value};

/// A stdio server with `args`, an sse server with a `${ENV_VAR}`-style
/// header, two providers (one explicit `base_url`, one on the `anthropic`
/// kind default), and an enabled `[telemetry]` section with every field the
/// shape test asserts.
const FULL_FIXTURE_TOML: &str = r#"
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

[[providers.entries]]
name = "anthropic-sonnet"
provider = "anthropic"
model = "claude-sonnet-4-6"
base_url = "https://gateway.example.com"
auth_token = "${ANTHROPIC_API_KEY}"

[[providers.entries]]
name = "openai-default"
provider = "openai"
model = "gpt-4o"
auth_token = "${OPENAI_API_KEY}"

[telemetry]
enabled = true
otlp_endpoint = "http://otel-collector:4317"
otlp_headers = { Authorization = "Bearer ${OTEL_TOKEN}" }
sample_ratio = 0.25
service_name = "custom-service"

[telemetry.metrics]
disabled = ["bae.events.total"]
"#;

/// A literal (no `${...}` at all) value in each of the three secret-bearing
/// fields: an MCP header, a provider `auth_token`, and a telemetry
/// `otlp_headers` value. Proves masking is unconditional rather than
/// pattern-matched on the value's shape.
const LITERAL_SECRETS_TOML: &str = r#"
[[mcp.servers]]
name = "search"
transport = "sse"
url = "https://mcp.example.com/sse"
headers = { Authorization = "literal-mcp-secret-xyz" }

[[providers.entries]]
name = "anthropic-sonnet"
provider = "anthropic"
model = "claude-sonnet-4-6"
auth_token = "literal-auth-token-abc"

[telemetry]
enabled = true
otlp_endpoint = "http://otel-collector:4317"
otlp_headers = { Authorization = "literal-otel-secret-123" }
"#;

/// Telemetry enabled but with no `service_name` set at all.
const TELEMETRY_NO_SERVICE_NAME_TOML: &str = r#"
[telemetry]
enabled = true
otlp_endpoint = "http://otel-collector:4317"
"#;

/// A temp-DB admin-router harness parameterized over the `bae-config.toml`
/// text this endpoint's `AppState` fields are built from. Auth is always
/// disabled — see the module doc for why.
struct Harness {
    dir: PathBuf,
    base: String,
    client: reqwest::Client,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    /// Boot the admin router against `toml`, loaded through the real
    /// `BaeConfigFile` loader + validator exactly as startup would.
    /// `None` exercises the "no config file at all" path; `Some("")` (or any
    /// TOML with none of the three tables) exercises the "file present but
    /// empty" path — both are documented to behave identically.
    async fn serve(toml: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!("baesrv-adminconfig-{}", generate_id("")));
        std::fs::create_dir_all(&dir).unwrap();

        let cfg_path = toml.map(|text| {
            let path = dir.join("bae-config.toml");
            std::fs::write(&path, text).unwrap();
            path
        });
        let cfg = BaeConfigFile::load(cfg_path.as_deref()).expect("load bae-config.toml");
        let mcp_registry = cfg.mcp_registry().expect("build mcp registry");
        let provider_registry = cfg.provider_registry().expect("build provider registry");
        let telemetry_config = cfg.telemetry_config().expect("build telemetry config");

        let store = Store::open(&dir.join("test.db")).expect("open temp store");
        let mut state = AppState::with_registries(store, mcp_registry, provider_registry);
        // Not a constructor parameter (see server-changes.md): the field is
        // `pub` precisely so tests can set it before the router captures its
        // `AppState` clone via `.with_state(...)`.
        state.telemetry_config = Arc::new(telemetry_config);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Mirrors `tests/integration.rs`'s admin router setup: these tests
        // predate/are independent of admin-port auth, so build with auth
        // disabled and exercise the endpoint directly.
        let app = admin::router(state, false);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let h = Harness {
            dir,
            base: format!("http://{addr}"),
            client: reqwest::Client::new(),
        };
        h.wait_ready().await;
        h
    }

    async fn wait_ready(&self) {
        for _ in 0..200 {
            if self
                .client
                .get(format!("{}/admin/v1/mcp-servers", self.base))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("admin server did not become ready");
    }

    /// Issue a GET; returns `(status, parsed_json_or_null, raw_text)`.
    async fn get(&self, path: &str) -> (u16, Value, String) {
        let resp = self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request send");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let val = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        (status, val, text)
    }
}

// ---------------------------------------------------------------------------
// Full-fixture shape
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_config_shape() {
    let h = Harness::serve(Some(FULL_FIXTURE_TOML)).await;
    let (status, body, raw) = h.get("/admin/v1/config").await;
    assert_eq!(status, 200, "{raw}");

    // `mcp.servers`, sorted by name: "filesystem" < "search".
    let servers = body["mcp"]["servers"].as_array().expect("servers array");
    assert_eq!(servers.len(), 2, "{raw}");

    assert_eq!(servers[0]["name"], json!("filesystem"));
    assert_eq!(servers[0]["transport"], json!("stdio"));
    assert_eq!(servers[0]["command"], json!("npx"));
    assert_eq!(
        servers[0]["args"],
        json!(["-y", "@modelcontextprotocol/server-filesystem", "/data"])
    );
    assert_eq!(servers[0]["url"], json!(null));
    assert_eq!(servers[0]["headers"], json!({}));

    assert_eq!(servers[1]["name"], json!("search"));
    assert_eq!(servers[1]["transport"], json!("sse"));
    assert_eq!(servers[1]["command"], json!(null));
    assert_eq!(servers[1]["args"], json!([]));
    assert_eq!(servers[1]["url"], json!("https://mcp.example.com/sse"));
    assert_eq!(
        servers[1]["headers"],
        json!({ "Authorization": admin::config::REDACTED })
    );

    // `providers.entries`, sorted by name.
    let entries = body["providers"]["entries"]
        .as_array()
        .expect("entries array");
    assert_eq!(entries.len(), 2, "{raw}");

    assert_eq!(entries[0]["name"], json!("anthropic-sonnet"));
    assert_eq!(entries[0]["provider"], json!("anthropic"));
    assert_eq!(entries[0]["model"], json!("claude-sonnet-4-6"));
    assert_eq!(
        entries[0]["base_url"],
        json!("https://gateway.example.com"),
        "explicit base_url is emitted verbatim"
    );
    assert_eq!(entries[0]["auth_token"], json!(admin::config::REDACTED));

    assert_eq!(entries[1]["name"], json!("openai-default"));
    assert_eq!(entries[1]["provider"], json!("openai"));
    assert_eq!(entries[1]["model"], json!("gpt-4o"));
    assert_eq!(
        entries[1]["base_url"],
        json!("https://api.openai.com"),
        "no explicit base_url -> the openai kind's default is the effective value"
    );
    assert_eq!(entries[1]["auth_token"], json!(admin::config::REDACTED));

    // `telemetry`.
    let telemetry = &body["telemetry"];
    assert_eq!(telemetry["enabled"], json!(true));
    assert_eq!(
        telemetry["otlp_endpoint"],
        json!("http://otel-collector:4317")
    );
    assert_eq!(telemetry["sample_ratio"], json!(0.25));
    assert_eq!(telemetry["service_name"], json!("custom-service"));
    assert_eq!(telemetry["traces"], json!({ "enabled": true }));
    assert_eq!(
        telemetry["metrics"],
        json!({ "enabled": true, "disabled": ["bae.events.total"] })
    );
    assert_eq!(
        telemetry["otlp_headers"],
        json!({ "Authorization": admin::config::REDACTED })
    );
}

// ---------------------------------------------------------------------------
// Literal-secret masking
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn literal_secrets_are_masked_unconditionally() {
    let h = Harness::serve(Some(LITERAL_SECRETS_TOML)).await;
    let (status, body, raw) = h.get("/admin/v1/config").await;
    assert_eq!(status, 200, "{raw}");

    // Absence check: none of the three literal secrets appear anywhere.
    assert!(
        !raw.contains("literal-mcp-secret-xyz"),
        "MCP header value leaked: {raw}"
    );
    assert!(
        !raw.contains("literal-auth-token-abc"),
        "provider auth_token leaked: {raw}"
    );
    assert!(
        !raw.contains("literal-otel-secret-123"),
        "telemetry otlp_headers value leaked: {raw}"
    );

    // Positive-shape check: the redaction contract is *mask*, not *drop*. Each
    // secret-bearing field must still be present with its key(s) preserved and
    // its value exactly the fixed marker — otherwise a regression that dropped
    // `headers`/`auth_token`/`otlp_headers` (or replaced them with `{}`/`null`)
    // would still satisfy the absence check above.
    assert_eq!(
        body["mcp"]["servers"][0]["headers"],
        json!({ "Authorization": admin::config::REDACTED }),
        "MCP header must be masked, not dropped: {raw}"
    );
    assert_eq!(
        body["providers"]["entries"][0]["auth_token"],
        json!(admin::config::REDACTED),
        "provider auth_token must be the marker, not dropped: {raw}"
    );
    assert_eq!(
        body["telemetry"]["otlp_headers"],
        json!({ "Authorization": admin::config::REDACTED }),
        "telemetry otlp_headers must be masked, not dropped: {raw}"
    );
}

// ---------------------------------------------------------------------------
// Absent config
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_config_file_yields_empty_lists_and_disabled_telemetry() {
    let h = Harness::serve(None).await;
    let (status, body, raw) = h.get("/admin/v1/config").await;
    assert_eq!(status, 200, "{raw}");

    assert_eq!(body["mcp"]["servers"], json!([]));
    assert_eq!(body["providers"]["entries"], json!([]));
    assert_eq!(body["telemetry"]["enabled"], json!(false));
    assert_eq!(body["telemetry"]["otlp_endpoint"], json!(null));
    assert_eq!(body["telemetry"]["otlp_headers"], json!({}));
    // With no config there is nothing to redact, so the fixed marker itself
    // must never appear. (The previous version searched for the ASCII word
    // "REDACTED" — the constant's *name*, not its value `••••••••` — which
    // could never appear regardless and so tested nothing.)
    assert!(
        !raw.contains(admin::config::REDACTED),
        "no secrets to invent, so the redaction marker must be absent: {raw}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_file_with_no_tables_yields_empty_lists_and_disabled_telemetry() {
    let h = Harness::serve(Some("")).await;
    let (status, body, raw) = h.get("/admin/v1/config").await;
    assert_eq!(status, 200, "{raw}");

    assert_eq!(body["mcp"]["servers"], json!([]));
    assert_eq!(body["providers"]["entries"], json!([]));
    assert_eq!(body["telemetry"]["enabled"], json!(false));
}

// ---------------------------------------------------------------------------
// Telemetry effective service_name default
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn telemetry_service_name_effective_default() {
    let h = Harness::serve(Some(TELEMETRY_NO_SERVICE_NAME_TOML)).await;
    let (status, body, raw) = h.get("/admin/v1/config").await;
    assert_eq!(status, 200, "{raw}");

    assert_eq!(body["telemetry"]["enabled"], json!(true));
    assert_ne!(
        body["telemetry"]["service_name"],
        json!(null),
        "must never be null: {raw}"
    );
    assert_eq!(body["telemetry"]["service_name"], json!("baesrv"));
}
