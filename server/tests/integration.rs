//! End-to-end integration tests for the BAE server.
//!
//! Every test here boots the **real** client and admin routers (the same ones
//! `baesrv::serve` wires up) on an ephemeral 127.0.0.1 port pair, backed by a
//! fresh temp-file SQLite database, and drives them over HTTP with `reqwest`.
//! Provider (LLM) calls are answered by an in-process mock HTTP server that
//! needs no real API keys, so the whole suite runs offline.
//!
//! Coverage (see `/awman/context/workflow/test-plan.md` for the mapping to the
//! work-item "Test Considerations"):
//! - server bootstrap: `/healthz`, `/api/v1/meta`, admin routes absent from the
//!   client port;
//! - admin CRUD for profiles and keys, `key_hash` never leaking, delete blocked
//!   by active keys, list pagination cursors;
//! - the full session lifecycle with the exact ordered event sequence and replay;
//! - auth rejection cases;
//! - provider fallback (failure + success), all-providers-failed (delivered as a
//!   terminal `result` on the 200 NDJSON stream, not a 502), and a missing-env-var
//!   provider failure;
//! - client-side and MCP tool dispatch round trips;
//! - the tool allowlist;
//! - the revoke cascade invalidating a live session;
//! - the JSON-RPC `/rpc` session loop: envelope error codes, live `session.event`
//!   notification delivery on `session.sendMessage`, `session.subscribe` live +
//!   `since_event_id` resume, and real (non-stub) MCP round trips with a local
//!   stdio fixture MCP server (`tests/fixtures/mcp_echo_server.py`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{json, Value};
use tracing_subscriber::fmt::MakeWriter;

use baesrv::api::AppState;
use baesrv::config_file::{BaeConfigFile, McpServerConfig};
use baesrv::store::{generate_id, Store};

// ---------------------------------------------------------------------------
// Mock provider (Anthropic Messages-shaped, no real keys)
// ---------------------------------------------------------------------------

/// Answers `POST {base}/v1/messages`. Behaviour is chosen by the leading path
/// segment of `base_url` so a single mock can play several roles at once:
///
/// - `/text` — always a final text turn.
/// - `/tool` — a `tool_use` (`get_current_time`) until the request carries a
///   `tool_result`, then a final text turn. Drives the client-side tool loop.
/// - `/mcp`  — a `tool_use` for an *undeclared* tool (`remote_search`) until a
///   `tool_result` appears, then text. Drives the server-side MCP stub loop.
/// - `/fail` — always HTTP 500 (a broken provider, for the fallback walk).
async fn mock_handler(req: Request) -> Response {
    let path = req.uri().path().to_string();
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));

    // Does the most recent message carry a tool_result block?
    let has_tool_result = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.last())
        .and_then(|last| last.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
        .unwrap_or(false);

    if path.starts_with("/fail") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "boom" })),
        )
            .into_response();
    }

    let out = if path.starts_with("/tool") {
        if has_tool_result {
            json!({ "role": "assistant", "stop_reason": "end_turn",
                    "content": [{ "type": "text", "text": "tool round-trip complete" }] })
        } else {
            json!({ "role": "assistant", "stop_reason": "tool_use", "content": [
                { "type": "tool_use", "id": "tu_1", "name": "get_current_time", "input": {} }] })
        }
    } else if path.starts_with("/mcp") {
        if has_tool_result {
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "after mcp stub" }] })
        } else {
            json!({ "role": "assistant", "content": [
                { "type": "tool_use", "id": "tu_mcp", "name": "remote_search", "input": { "q": "x" } }] })
        }
    } else if path.starts_with("/sandbox") {
        // Drives the Auto-mode sandbox dispatch path: a `tool_use` for
        // `run_shell_command` carrying the fully-formed `command` the server
        // executes against the session's remote sandbox, until a tool_result
        // comes back — then a final text turn.
        if has_tool_result {
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "after sandbox exec" }] })
        } else {
            json!({ "role": "assistant", "content": [
                { "type": "tool_use", "id": "tu_sbx", "name": "run_shell_command",
                  "input": { "command": "echo hi" } }] })
        }
    } else {
        json!({ "role": "assistant", "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": "Hello from mock" }] })
    };
    (StatusCode::OK, Json(out)).into_response()
}

/// Start the mock provider on an ephemeral port; returns its base origin URL
/// (e.g. `http://127.0.0.1:54321`). Append `/text`, `/tool`, `/mcp`, or `/fail`
/// to select behaviour.
async fn start_mock() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(mock_handler);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Test server harness
// ---------------------------------------------------------------------------

/// A running server instance: the two base URLs and a shared HTTP client. The
/// temp DB directory is removed on drop.
struct TestServer {
    client: String,
    admin: String,
    http: reqwest::Client,
    dir: PathBuf,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The default provider registry every test server starts with: one entry per
/// mock behaviour, named after the mock path it targets ("text", "tool",
/// "mcp", "fail"), plus "badenv" whose `auth_token` references an env var
/// guaranteed to be unset. Built from a `bae-config.toml` `[providers]`
/// section through the **real** loader + validator, exactly as startup would.
/// Profiles opt in by these names via `primary_provider`/`fallback_providers`.
fn default_provider_registry(
    mock: &str,
) -> HashMap<String, baesrv::engine::provider::ProviderConfig> {
    let entry = |name: &str, path: &str, auth_token: &str| {
        format!(
            "[[providers.entries]]\nname = {name:?}\nprovider = \"anthropic\"\n\
             base_url = \"{mock}/{path}\"\nmodel = \"claude-mock-1\"\n\
             auth_token = {auth_token:?}\n\n"
        )
    };
    let toml = [
        entry("text", "text", "test-token"),
        entry("tool", "tool", "test-token"),
        entry("mcp", "mcp", "test-token"),
        entry("sandbox", "sandbox", "test-token"),
        entry("fail", "fail", "test-token"),
        entry("badenv", "text", "${BAE_TEST_DEFINITELY_UNSET_XYZ}"),
    ]
    .concat();
    provider_registry_from_toml(&toml)
}

/// Build a provider registry from a `bae-config.toml` string through the real
/// loader, exactly as startup would (mirrors [`registry_from_toml`] for MCP).
fn provider_registry_from_toml(
    toml: &str,
) -> HashMap<String, baesrv::engine::provider::ProviderConfig> {
    let dir = std::env::temp_dir().join(format!("baecfg-{}", generate_id("")));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bae-config.toml");
    std::fs::write(&path, toml).unwrap();
    let cfg = BaeConfigFile::load(Some(&path)).expect("load bae-config.toml");
    let reg = cfg.provider_registry().expect("build provider registry");
    let _ = std::fs::remove_dir_all(&dir);
    reg
}

/// Boot the real routers on an ephemeral client/admin port pair with a fresh
/// temp-file database, exactly as `baesrv::serve` would (minus signal handling).
/// Also starts the mock provider and preloads the default provider registry
/// pointing at it.
async fn start_server() -> TestServer {
    start_server_with_registry(std::collections::HashMap::new()).await
}

/// Like [`start_server`], but with a preloaded MCP server registry (as
/// `bae-config.toml` would supply at startup). Used by the MCP integration
/// tests, which build the registry from a fixture `bae-config.toml` through the
/// real loader. The default provider registry is loaded in every case.
async fn start_server_with_registry(
    registry: std::collections::HashMap<String, baesrv::config_file::McpServerConfig>,
) -> TestServer {
    // The mock provider must exist before the server: the provider registry is
    // read-only after startup and its entries carry the mock's base URL.
    let mock = start_mock().await;
    let providers = default_provider_registry(&mock);
    boot_server(registry, providers, None).await
}

/// Boot the real routers with an explicit provider registry (whose entries the
/// caller has already pointed at whatever mock server(s) the test owns) and an
/// optional `turn_timeout` override (for the FIFO/abandonment tests, which need
/// a short `BAE_TURN_TIMEOUT` rather than the 120 s default). Unlike
/// [`start_server_with_registry`] this does **not** start the default mock — the
/// caller supplies a registry pointing at its own mocks.
async fn boot_server(
    mcp_registry: std::collections::HashMap<String, baesrv::config_file::McpServerConfig>,
    provider_registry: std::collections::HashMap<String, baesrv::engine::provider::ProviderConfig>,
    turn_timeout: Option<Duration>,
) -> TestServer {
    boot_server_capture(mcp_registry, provider_registry, turn_timeout, None)
        .await
        .0
}

/// Like [`boot_server`], but additionally allows overriding the host-wide
/// [`baesrv::engine::sandbox::SandboxDriver`] and returns a **clone of the live
/// `AppState`** alongside the running server. `AppState` is `Clone` over shared
/// `Arc`s, so the returned handle observes the same `sandboxes` / `sandbox_status`
/// maps the server mutates — the seam the sandbox tests use to assert
/// server-owned state directly (mirroring how `state.turn_timeout` and
/// `state.sandbox_driver` are the documented override seams). Passing `None`
/// leaves the default `DockerDriver` in place.
async fn boot_server_capture(
    mcp_registry: std::collections::HashMap<String, baesrv::config_file::McpServerConfig>,
    provider_registry: std::collections::HashMap<String, baesrv::engine::provider::ProviderConfig>,
    turn_timeout: Option<Duration>,
    sandbox_driver: Option<Arc<dyn baesrv::engine::sandbox::SandboxDriver>>,
) -> (TestServer, AppState) {
    let dir = std::env::temp_dir().join(format!("baesrv-it-{}", generate_id("")));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.db");

    // A real file-backed store exercises the migration runner on a fresh DB.
    let store = Store::open(&db_path).expect("open temp store");
    let mut state = AppState::with_registries(store, mcp_registry, provider_registry);
    if let Some(t) = turn_timeout {
        // `turn_timeout` is a pub field; overriding it here is the documented
        // seam for short-timeout tests (see notes-multi-client.md).
        state.turn_timeout = t;
    }
    if let Some(driver) = sandbox_driver {
        // The sandbox driver is a pub field constructed once at startup; the
        // integration harness swaps in an offline mock exactly the way
        // `cli::build_sandbox_driver` installs the real one at boot.
        state.sandbox_driver = driver;
    }

    let client_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_addr = client_listener.local_addr().unwrap();
    let admin_addr = admin_listener.local_addr().unwrap();

    let client_app = baesrv::api::client::router(state.clone());
    let admin_app = baesrv::api::admin::router(state.clone());
    tokio::spawn(async move {
        axum::serve(client_listener, client_app).await.unwrap();
    });
    tokio::spawn(async move {
        axum::serve(admin_listener, admin_app).await.unwrap();
    });

    let ts = TestServer {
        client: format!("http://{client_addr}"),
        admin: format!("http://{admin_addr}"),
        http: reqwest::Client::new(),
        dir,
    };
    ts.wait_ready().await;
    (ts, state)
}

/// Boot a server pointed at the default mock provider registry (so `text` /
/// `tool` / `mcp` / `sandbox` primaries all resolve) with an offline mock
/// [`baesrv::engine::sandbox::SandboxDriver`] installed, returning the running
/// server and a live `AppState` handle. The workhorse for the sandbox suite.
async fn start_server_with_sandbox(
    driver: Arc<dyn baesrv::engine::sandbox::SandboxDriver>,
) -> (TestServer, AppState) {
    let mock = start_mock().await;
    let providers = default_provider_registry(&mock);
    boot_server_capture(
        std::collections::HashMap::new(),
        providers,
        None,
        Some(driver),
    )
    .await
}

impl TestServer {
    async fn wait_ready(&self) {
        for _ in 0..100 {
            if let Ok(r) = self
                .http
                .get(format!("{}/healthz", self.client))
                .send()
                .await
            {
                if r.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("server did not become ready");
    }

    /// Issue a request; returns `(status, parsed_json_or_null, raw_text)`.
    async fn send(
        &self,
        method: Method,
        url: String,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value, String) {
        let mut rb = self.http.request(method, url);
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().await.expect("request send");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let val = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        (status, val, text)
    }

    async fn admin_post(&self, path: &str, body: Value) -> (u16, Value, String) {
        self.send(
            Method::POST,
            format!("{}{path}", self.admin),
            None,
            Some(body),
        )
        .await
    }
    async fn admin_get(&self, path: &str) -> (u16, Value, String) {
        self.send(Method::GET, format!("{}{path}", self.admin), None, None)
            .await
    }
    async fn admin_put(&self, path: &str, body: Value) -> (u16, Value, String) {
        self.send(
            Method::PUT,
            format!("{}{path}", self.admin),
            None,
            Some(body),
        )
        .await
    }
    async fn admin_delete(&self, path: &str) -> (u16, Value, String) {
        self.send(Method::DELETE, format!("{}{path}", self.admin), None, None)
            .await
    }
    async fn client_post(
        &self,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (u16, Value, String) {
        self.send(
            Method::POST,
            format!("{}{path}", self.client),
            token,
            Some(body),
        )
        .await
    }
    async fn client_get(&self, path: &str, token: Option<&str>) -> (u16, Value, String) {
        self.send(Method::GET, format!("{}{path}", self.client), token, None)
            .await
    }

    /// Drive the JSON-RPC `session.sendMessage` method on
    /// `POST /api/v1/sessions/{id}/rpc` (the replacement for the removed
    /// `POST /messages` route). `message` is the inner message object
    /// (`{role?, content}`).
    ///
    /// Returns `(http_status, value, raw)`. On a non-200 (auth failure, an RFC
    /// 7807 body) `value` is the parsed body. On 200 the response is the NDJSON
    /// JSON-RPC stream: `value` is the terminal response's `result`
    /// (`{message, events}`, the same shape the old route returned), or the whole
    /// terminal object when it is a JSON-RPC error.
    async fn send_message(
        &self,
        session_id: &str,
        token: &str,
        message: Value,
    ) -> (u16, Value, String) {
        let url = format!("{}/api/v1/sessions/{session_id}/rpc", self.client);
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session.sendMessage",
            "params": { "message": message },
        });
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .expect("rpc send");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status != 200 {
            let v = serde_json::from_str(&text).unwrap_or(Value::Null);
            return (status, v, text);
        }
        // NDJSON: the terminal object is the one carrying the request id.
        let mut terminal = Value::Null;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let obj: Value = match serde_json::from_str(line) {
                Ok(o) => o,
                Err(_) => continue,
            };
            if obj.get("id").and_then(Value::as_i64) == Some(1) {
                terminal = obj;
            }
        }
        let out = terminal
            .get("result")
            .cloned()
            .unwrap_or_else(|| terminal.clone());
        (status, out, text)
    }
    async fn client_delete(&self, path: &str, token: Option<&str>) -> (u16, Value, String) {
        self.send(
            Method::DELETE,
            format!("{}{path}", self.client),
            token,
            None,
        )
        .await
    }

    /// Create a profile whose `primary_provider` is `primary` (a provider
    /// registry name — see [`default_provider_registry`]) and whose
    /// `fallback_providers` is `fallbacks` (an array of names), returning its id.
    async fn create_profile(
        &self,
        primary: &str,
        fallbacks: Value,
        allowed_tools: Value,
    ) -> String {
        let body = json!({
            "name": format!("profile-{}", generate_id("")),
            "primary_provider": primary,
            "fallback_providers": fallbacks,
            "allowed_tools": allowed_tools,
        });
        let (status, v, raw) = self.admin_post("/admin/v1/profiles", body).await;
        assert_eq!(status, 201, "create profile failed: {raw}");
        v["id"].as_str().unwrap().to_string()
    }

    /// Issue a client key for `profile_id`, returning the plaintext key.
    async fn create_key(&self, profile_id: &str) -> String {
        let (status, v, raw) = self
            .admin_post(
                "/admin/v1/keys",
                json!({ "name": "agent-1", "profile_id": profile_id }),
            )
            .await;
        assert_eq!(status, 201, "create key failed: {raw}");
        v["key"].as_str().unwrap().to_string()
    }

    /// Open a session; returns `(session_id, session_key, full_json)`.
    ///
    /// Also registers the caller as a driver (`session.registerDriver`),
    /// mirroring what every SDK's `connect()` does as part of session setup,
    /// so `session.sendMessage` is permitted (WI 0005's `-32001` gate).
    async fn open_session(&self, client_key: &str, tools: Value) -> (String, String, Value) {
        let (status, v, raw) = self
            .client_post(
                "/api/v1/sessions",
                Some(client_key),
                json!({ "client_version": "1.0.0", "tools": tools }),
            )
            .await;
        assert_eq!(status, 201, "open session failed: {raw}");
        let session_id = v["session_id"].as_str().unwrap().to_string();
        let session_key = v["session_key"].as_str().unwrap().to_string();
        self.register_driver(&session_id, &session_key).await;
        (session_id, session_key, v)
    }

    /// Register the session key's client as a driver via `session.registerDriver`.
    async fn register_driver(&self, session_id: &str, session_key: &str) {
        let (status, frames, raw) = self
            .rpc(
                session_id,
                session_key,
                json!({ "jsonrpc": "2.0", "id": 1,
                        "method": "session.registerDriver", "params": {} }),
            )
            .await;
        assert_eq!(status, 200, "registerDriver failed: {raw}");
        assert_eq!(
            frames[0]["result"]["registered"],
            json!(true),
            "registerDriver result: {raw}"
        );
    }
}

/// Extract the ordered `event_type` list from an `events` array value.
fn event_types(events: &Value) -> Vec<String> {
    events
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|e| e["event_type"].as_str().unwrap_or("?").to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bootstrap_healthz_and_meta() {
    let ts = start_server().await;

    let (status, _v, _raw) = ts.client_get("/healthz", None).await;
    assert_eq!(status, 200, "/healthz should be 200");

    let (status, v, raw) = ts.client_get("/api/v1/meta", None).await;
    assert_eq!(status, 200);
    assert_eq!(v["version"], json!("0.1.0"), "meta shape: {raw}");
    assert_eq!(v["api_versions"], json!(["v1"]), "meta shape: {raw}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_routes_absent_from_client_port() {
    // The admin surface lives on a *separate* router/listener; it must not be
    // reachable through the client port at all (defence in depth beyond the
    // loopback-only bind, which config validation enforces and a unit test
    // covers). A non-loopback admin bind cannot be exercised in CI — see
    // test-plan.md.
    let ts = start_server().await;
    let (status, _v, _raw) = ts.client_get("/admin/v1/profiles", None).await;
    assert_eq!(
        status, 404,
        "admin routes must not exist on the client port"
    );
}

// ---------------------------------------------------------------------------
// Admin CRUD
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_profile_and_key_crud_lifecycle() {
    let ts = start_server().await;
    // --- Create + read a profile ---
    let profile_id = ts
        .create_profile("text", json!([]), json!(["get_current_time"]))
        .await;

    let (status, prof, raw) = ts
        .admin_get(&format!("/admin/v1/profiles/{profile_id}"))
        .await;
    assert_eq!(status, 200, "get profile: {raw}");
    assert_eq!(prof["id"], json!(profile_id));
    assert_eq!(prof["allowed_tools"], json!(["get_current_time"]));
    // Provider references are registry *names*; no auth material is stored on
    // (or returned by) a profile — tokens live only in bae-config.toml.
    assert_eq!(prof["primary_provider"], json!("text"));
    assert_eq!(prof["fallback_providers"], json!([]));
    assert!(
        !raw.contains("auth_token"),
        "profiles must not carry auth_token: {raw}"
    );

    let (status, list, raw) = ts.admin_get("/admin/v1/profiles").await;
    assert_eq!(status, 200);
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "list: {raw}");

    // --- Replace (PUT) bumps updated_at and swaps fields ---
    let put_body = json!({
        "name": prof["name"],
        "primary_provider": prof["primary_provider"],
        "fallback_providers": [],
        "allowed_tools": [],
    });
    let (status, replaced, raw) = ts
        .admin_put(&format!("/admin/v1/profiles/{profile_id}"), put_body)
        .await;
    assert_eq!(status, 200, "replace: {raw}");
    assert_eq!(replaced["allowed_tools"], json!([]));

    // --- Issue a client key; plaintext returned once, hash never present ---
    let (status, key, key_raw) = ts
        .admin_post(
            "/admin/v1/keys",
            json!({ "name": "agent-1", "profile_id": profile_id }),
        )
        .await;
    assert_eq!(status, 201, "create key: {key_raw}");
    assert!(key["key"].as_str().unwrap().starts_with("bae_"));
    assert!(key["prefix"].as_str().unwrap().starts_with("bae_"));
    let key_id = key["id"].as_str().unwrap().to_string();

    let (status, key_list, list_raw) = ts.admin_get("/admin/v1/keys").await;
    assert_eq!(status, 200);
    assert_eq!(key_list["items"].as_array().unwrap().len(), 1);

    // key_hash must never appear in ANY admin response body.
    for raw in [&raw, &key_raw, &list_raw] {
        assert!(
            !raw.contains("key_hash"),
            "key_hash leaked in a response: {raw}"
        );
    }
    assert!(!key_list["items"][0]
        .as_object()
        .unwrap()
        .contains_key("key_hash"));

    // --- Deleting a profile is blocked while an active key references it ---
    let (status, err, raw) = ts
        .admin_delete(&format!("/admin/v1/profiles/{profile_id}"))
        .await;
    assert_eq!(status, 409, "profile-in-use should block delete: {raw}");
    assert_eq!(err["type"], json!("profile_in_use"));

    // --- Revoke the key, then the profile deletes cleanly ---
    let (status, _v, _raw) = ts.admin_delete(&format!("/admin/v1/keys/{key_id}")).await;
    assert_eq!(status, 204);
    let (status, _v, _raw) = ts
        .admin_delete(&format!("/admin/v1/profiles/{profile_id}"))
        .await;
    assert_eq!(status, 204);

    // Deleted profile is gone.
    let (status, _v, _raw) = ts
        .admin_get(&format!("/admin/v1/profiles/{profile_id}"))
        .await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_pagination_returns_correct_cursor() {
    let ts = start_server().await;
    for _ in 0..3 {
        ts.create_profile("text", json!([]), json!([])).await;
    }

    // First page of 2 → a non-null cursor.
    let (status, page1, raw) = ts.admin_get("/admin/v1/profiles?limit=2").await;
    assert_eq!(status, 200, "{raw}");
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    let cursor = page1["next_cursor"].as_str().expect("cursor present");

    // Second page → the remaining 1 and a null cursor.
    let (status, page2, raw) = ts
        .admin_get(&format!("/admin/v1/profiles?limit=2&cursor={cursor}"))
        .await;
    assert_eq!(status, 200, "{raw}");
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert!(
        page2["next_cursor"].is_null(),
        "last page cursor must be null"
    );

    // No overlap between pages.
    let id_p1_a = page1["items"][0]["id"].as_str().unwrap();
    let id_p1_b = page1["items"][1]["id"].as_str().unwrap();
    let id_p2 = page2["items"][0]["id"].as_str().unwrap();
    assert_ne!(id_p2, id_p1_a);
    assert_ne!(id_p2, id_p1_b);
}

// ---------------------------------------------------------------------------
// Session lifecycle + replay
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_lifecycle_exact_event_sequence_and_replay() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;

    // Open: exchange client key for a session id + session key.
    let (session_id, session_key, open) = ts.open_session(&client_key, json!([])).await;
    assert!(session_id.starts_with("ses_"));
    assert!(session_key.starts_with("bae_ses_"));
    // The returned profile is sanitized: no auth_token, no env var names.
    assert!(
        !open.to_string().contains("auth_token"),
        "open response must not carry auth_token: {open}"
    );
    assert!(!open.to_string().contains("test-token"));

    // Send one text turn.
    let (status, resp, raw) = ts
        .send_message(
            &session_id,
            &session_key,
            json!({ "role": "user", "content": "hello" }),
        )
        .await;
    assert_eq!(status, 200, "messages: {raw}");
    assert_eq!(resp["message"]["role"], json!("assistant"));
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("Hello from mock")
    );

    // The events appended *during this call*, in exact order.
    assert_eq!(
        event_types(&resp["events"]),
        vec![
            "client.message.send",
            "provider.request",
            "provider.response",
            "server.message.send",
        ]
    );
    // The success provider.response records ok:true; no secret rides along.
    let pr = resp["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("provider.response"))
        .unwrap();
    assert_eq!(pr["payload"]["ok"], json!(true));
    assert!(
        !resp.to_string().contains("test-token"),
        "resolved token must never appear in events"
    );

    // Close.
    let (status, closed, raw) = ts
        .client_delete(
            &format!("/api/v1/sessions/{session_id}"),
            Some(&session_key),
        )
        .await;
    assert_eq!(status, 200, "close: {raw}");
    assert_eq!(closed["state"], json!("closed"));

    // Closing again is a conflict.
    let (status, _v, _raw) = ts
        .client_delete(
            &format!("/api/v1/sessions/{session_id}"),
            Some(&session_key),
        )
        .await;
    assert_eq!(status, 409);

    // Replay the full, ordered history.
    let (status, events, raw) = ts
        .client_get(
            &format!("/api/v1/sessions/{session_id}/events"),
            Some(&session_key),
        )
        .await;
    assert_eq!(status, 200, "replay: {raw}");
    assert_eq!(
        event_types(&events["items"]),
        vec![
            "session.open",
            "session.driver.register",
            "client.message.send",
            "provider.request",
            "provider.response",
            "server.message.send",
            "session.close",
        ]
    );
    // The final session.close records the client_close reason.
    let last = events["items"].as_array().unwrap().last().unwrap();
    assert_eq!(last["payload"]["reason"], json!("client_close"));
}

// ---------------------------------------------------------------------------
// Auth rejection
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_rejection_cases() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;

    // Missing bearer → 401.
    let (status, _v, _raw) = ts
        .client_post("/api/v1/sessions", None, json!({ "tools": [] }))
        .await;
    assert_eq!(status, 401, "missing bearer");

    // Garbage client key → 401.
    let (status, _v, _raw) = ts
        .client_post(
            "/api/v1/sessions",
            Some("bae_not_a_real_key"),
            json!({ "tools": [] }),
        )
        .await;
    assert_eq!(status, 401, "bad client key");

    // Two valid sessions from the same client key.
    let (session1, session1_key, _) = ts.open_session(&client_key, json!([])).await;
    let (_session2, session2_key, _) = ts.open_session(&client_key, json!([])).await;

    // A wholly bogus session key → 401 (auth is a transport gate on /rpc, before
    // the JSON-RPC stream is opened).
    let (status, _v, _raw) = ts
        .send_message(&session1, "bae_ses_bogus", json!({ "content": "hi" }))
        .await;
    assert_eq!(status, 401, "bogus session key");

    // A *valid* session key used on the wrong session → 401 (the lookup is
    // scoped to the session id, so a mismatched key finds no row).
    let (status, _v, _raw) = ts
        .send_message(&session1, &session2_key, json!({ "content": "hi" }))
        .await;
    assert_eq!(status, 401, "session key on wrong session");
    let (status, _v, _raw) = ts
        .client_get(
            &format!("/api/v1/sessions/{session1}/events"),
            Some(&session2_key),
        )
        .await;
    assert_eq!(status, 401, "session key on wrong session (events)");

    // The correct key still works.
    let (status, _v, _raw) = ts
        .client_get(
            &format!("/api/v1/sessions/{session1}/events"),
            Some(&session1_key),
        )
        .await;
    assert_eq!(status, 200, "correct session key");

    // A deleted client key can no longer open sessions → 401.
    let victim_key = ts.create_key(&profile_id).await;
    // Find its id via the admin list (prefix match).
    let (_s, key_list, _r) = ts.admin_get("/admin/v1/keys").await;
    let victim_prefix = &victim_key[..8];
    let victim_id = key_list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["prefix"].as_str() == Some(victim_prefix))
        .map(|k| k["id"].as_str().unwrap().to_string())
        .expect("victim key listed");
    let (status, _v, _raw) = ts
        .admin_delete(&format!("/admin/v1/keys/{victim_id}"))
        .await;
    assert_eq!(status, 204);
    let (status, _v, _raw) = ts
        .client_post(
            "/api/v1/sessions",
            Some(&victim_key),
            json!({ "tools": [] }),
        )
        .await;
    assert_eq!(status, 401, "deleted client key");
}

// ---------------------------------------------------------------------------
// Provider fallback + failure paths
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_fallback_failure_then_success() {
    let ts = start_server().await;
    // Primary is broken ("fail" → 500); the single fallback works ("text").
    let profile_id = ts.create_profile("fail", json!(["text"]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&session_id, &session_key, json!({ "content": "hello" }))
        .await;
    assert_eq!(status, 200, "fallback should succeed: {raw}");
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("Hello from mock")
    );

    // Exact ordered sequence for a failed primary + working fallback.
    assert_eq!(
        event_types(&resp["events"]),
        vec![
            "client.message.send",
            "provider.request",  // primary
            "provider.response", // primary failure
            "session.error",     // provider_call_failed context
            "provider.request",  // fallback
            "provider.response", // fallback success
            "server.message.send",
        ]
    );

    // BOTH provider.response events are in the log: one failure, one success.
    let responses: Vec<&Value> = resp["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event_type"] == json!("provider.response"))
        .collect();
    assert_eq!(
        responses.len(),
        2,
        "expected a failure and a success response"
    );
    assert_eq!(responses[0]["payload"]["ok"], json!(false));
    assert_eq!(responses[0]["payload"]["status"], json!(500));
    assert_eq!(responses[1]["payload"]["ok"], json!(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_providers_failed_returns_terminal_result_then_rejects() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("fail", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&session_id, &session_key, json!({ "content": "hello" }))
        .await;
    // On /rpc the HTTP stream is 200; the provider failure rides in the terminal
    // response's result (message + events), session moved to error state.
    assert_eq!(status, 200, "rpc stream opens: {raw}");
    assert!(
        resp["message"].is_object(),
        "terminal result carries a message: {raw}"
    );
    let types = event_types(&resp["events"]);
    assert!(types.contains(&"provider.response".to_string()));
    assert!(types.contains(&"session.error".to_string()));
    // The session is now in error state → a further sendMessage is refused with a
    // JSON-RPC error object (code -32000) in the stream, not a new turn.
    let (status, again, raw) = ts
        .send_message(&session_id, &session_key, json!({ "content": "again" }))
        .await;
    assert_eq!(status, 200, "rpc stream opens even when refusing: {raw}");
    assert_eq!(
        again["error"]["code"],
        json!(-32000),
        "error-state session rejects new turns: {raw}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_primary_provider_blocks_session_creation() {
    let ts = start_server().await;

    // A profile may reference a not-(yet-)configured provider at write time —
    // resolution happens at session-creation time, like mcp_servers...
    let profile_id = ts
        .create_profile("ghost-provider", json!([]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;

    // ...but opening a session against it is refused outright, on every
    // attempt: no session key is ever issued for an unresolvable primary.
    for attempt in 0..2 {
        let (status, err, raw) = ts
            .client_post(
                "/api/v1/sessions",
                Some(&client_key),
                json!({ "tools": [] }),
            )
            .await;
        assert_eq!(status, 422, "attempt {attempt}: {raw}");
        assert_eq!(err["type"], json!("primary_provider_unavailable"));
        assert!(err["detail"].as_str().unwrap().contains("ghost-provider"));
    }

    // The admin registry view shows what IS configured — name, kind, model,
    // and the *effective* base_url — never auth_token.
    let (status, providers, raw) = ts.admin_get("/admin/v1/providers").await;
    assert_eq!(status, 200, "{raw}");
    let items = providers["items"].as_array().unwrap();
    assert!(!items.iter().any(|i| i["name"] == json!("ghost-provider")));
    let text = items
        .iter()
        .find(|i| i["name"] == json!("text"))
        .expect("the default registry's \"text\" entry is listed");
    assert_eq!(text["provider"], json!("anthropic"));
    assert_eq!(text["model"], json!("claude-mock-1"));
    assert!(text["base_url"].as_str().unwrap().ends_with("/text"));
    assert!(!raw.contains("auth_token"), "no secrets: {raw}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_missing_env_var_fails_with_traced_response() {
    let ts = start_server().await;
    // The "badenv" registry entry's auth_token references an environment
    // variable guaranteed to be unset, so token resolution fails *before* any
    // HTTP request and is recorded as a provider.response failure (secret
    // never reaches the wire).
    let profile_id = ts.create_profile("badenv", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&session_id, &session_key, json!({ "content": "hello" }))
        .await;
    // The provider failure now rides in the 200 NDJSON stream's terminal result.
    assert_eq!(
        status, 200,
        "rpc stream opens even on provider failure: {raw}"
    );
    let failure = resp["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("provider.response"))
        .expect("a provider.response failure event");
    assert_eq!(failure["payload"]["ok"], json!(false));
    assert!(
        failure["payload"]["error"]
            .as_str()
            .unwrap()
            .contains("BAE_TEST_DEFINITELY_UNSET_XYZ"),
        "error should name the missing variable: {failure}"
    );
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_side_tool_dispatch_round_trip() {
    let ts = start_server().await;
    let profile_id = ts
        .create_profile("tool", json!([]), json!(["get_current_time"]))
        .await;
    let client_key = ts.create_key(&profile_id).await;
    let tools = json!([{
        "name": "get_current_time",
        "description": "Return the current time",
        "input_schema": { "type": "object", "properties": {} },
    }]);
    let (session_id, session_key, _) = ts.open_session(&client_key, tools).await;

    // First turn: the provider asks for a client-side tool; the loop pauses and
    // hands the tool_use back to the harness.
    let (status, first, raw) = ts
        .send_message(
            &session_id,
            &session_key,
            json!({ "content": "what time is it?" }),
        )
        .await;
    assert_eq!(status, 200, "first turn: {raw}");
    let tool_use = first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == json!("tool_use"))
        .expect("assistant turn carries a tool_use");
    assert_eq!(tool_use["name"], json!("get_current_time"));
    let first_types = event_types(&first["events"]);
    assert!(first_types.contains(&"tool.call".to_string()));
    // The tool.call is tagged as a client dispatch.
    let tool_call = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("tool.call"))
        .unwrap();
    assert_eq!(tool_call["payload"]["dispatch"], json!("client"));

    // The harness runs the tool and returns its result.
    let tool_use_id = tool_use["id"].as_str().unwrap();
    let (status, second, raw) = ts
        .send_message(
            &session_id,
            &session_key,
            json!({ "content": [{
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": "12:00 UTC",
            }] }),
        )
        .await;
    assert_eq!(status, 200, "second turn: {raw}");
    assert_eq!(
        second["message"]["content"][0]["text"],
        json!("tool round-trip complete")
    );
    let second_types = event_types(&second["events"]);
    assert!(second_types.contains(&"tool.result".to_string()));

    // Across the whole session both tool.call and tool.result are in the log.
    let (_s, replay, _r) = ts
        .client_get(
            &format!("/api/v1/sessions/{session_id}/events"),
            Some(&session_key),
        )
        .await;
    let all = event_types(&replay["items"]);
    assert!(all.contains(&"tool.call".to_string()));
    assert!(all.contains(&"tool.result".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_tool_dispatch_round_trip() {
    let ts = start_server().await;
    // The client declares NO tools, so the provider's tool_use for `remote_search`
    // is dispatched server-side as an MCP stub and resolves within one call.
    let profile_id = ts.create_profile("mcp", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(
            &session_id,
            &session_key,
            json!({ "content": "search please" }),
        )
        .await;
    assert_eq!(status, 200, "mcp turn: {raw}");
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("after mcp stub")
    );

    let types = event_types(&resp["events"]);
    for expected in ["tool.call", "mcp.request", "mcp.response", "tool.result"] {
        assert!(
            types.contains(&expected.to_string()),
            "missing {expected}: {types:?}"
        );
    }
    // The tool.call and tool.result are tagged mcp dispatch.
    let tool_call = resp["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("tool.call"))
        .unwrap();
    assert_eq!(tool_call["payload"]["dispatch"], json!("mcp"));
}

// ---------------------------------------------------------------------------
// Tool allowlist
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_allowlist_validation() {
    let ts = start_server().await;
    // Profile allows exactly one tool.
    let allowed_profile = ts
        .create_profile("text", json!([]), json!(["get_current_time"]))
        .await;
    let allowed_key = ts.create_key(&allowed_profile).await;

    // Exact-name match is accepted.
    let (status, _v, _raw) = ts
        .client_post(
            "/api/v1/sessions",
            Some(&allowed_key),
            json!({ "tools": [{ "name": "get_current_time" }] }),
        )
        .await;
    assert_eq!(status, 201, "declared tool in allowlist");

    // An undeclared tool name is rejected.
    let (status, err, raw) = ts
        .client_post(
            "/api/v1/sessions",
            Some(&allowed_key),
            json!({ "tools": [{ "name": "delete_everything" }] }),
        )
        .await;
    assert_eq!(status, 403, "tool not in allowlist: {raw}");
    assert_eq!(err["type"], json!("tool_not_allowed"));

    // An empty allowlist permits no tools at all...
    let empty_profile = ts.create_profile("text", json!([]), json!([])).await;
    let empty_key = ts.create_key(&empty_profile).await;
    let (status, _v, _raw) = ts
        .client_post(
            "/api/v1/sessions",
            Some(&empty_key),
            json!({ "tools": [{ "name": "get_current_time" }] }),
        )
        .await;
    assert_eq!(status, 403, "empty allowlist rejects every tool");

    // ...but declaring zero tools is fine.
    let (status, _v, _raw) = ts
        .client_post("/api/v1/sessions", Some(&empty_key), json!({ "tools": [] }))
        .await;
    assert_eq!(status, 201, "no tools declared is always allowed");
}

// ---------------------------------------------------------------------------
// Revoke cascade
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_client_key_invalidates_live_session() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    // The session key works before revocation.
    let (status, _v, _raw) = ts
        .client_get(
            &format!("/api/v1/sessions/{session_id}/events"),
            Some(&session_key),
        )
        .await;
    assert_eq!(status, 200);

    // Revoke the client key; the cascade soft-deletes its session keys.
    let (_s, key_list, _r) = ts.admin_get("/admin/v1/keys").await;
    let key_id = key_list["items"][0]["id"].as_str().unwrap().to_string();
    let (status, _v, _raw) = ts.admin_delete(&format!("/admin/v1/keys/{key_id}")).await;
    assert_eq!(status, 204);

    // The session key can no longer authenticate.
    let (status, _v, _raw) = ts
        .client_get(
            &format!("/api/v1/sessions/{session_id}/events"),
            Some(&session_key),
        )
        .await;
    assert_eq!(status, 401, "revoked cascade invalidates the session key");
}

// ===========================================================================
// JSON-RPC session loop (`POST /api/v1/sessions/{id}/rpc`) + MCP round trips
// ===========================================================================

/// When to stop reading a `session.subscribe` stream in [`TestServer::subscribe_collect`].
enum StopWhen {
    /// Stop after the first `session.event` whose `event_type` matches.
    EventType(String),
    /// Stop after the first `session.event` whose event `id` matches (unique, so
    /// deterministic — used to catch up to a known last persisted event).
    EventId(String),
}

impl TestServer {
    /// Create a profile whose `primary_provider` is `primary` with an explicit
    /// `mcp_servers` list.
    async fn create_profile_with_mcp(
        &self,
        primary: &str,
        mcp_servers: Value,
        allowed_tools: Value,
    ) -> String {
        let body = json!({
            "name": format!("profile-{}", generate_id("")),
            "primary_provider": primary,
            "fallback_providers": [],
            "mcp_servers": mcp_servers,
            "allowed_tools": allowed_tools,
        });
        let (status, v, raw) = self.admin_post("/admin/v1/profiles", body).await;
        assert_eq!(status, 201, "create profile (mcp) failed: {raw}");
        v["id"].as_str().unwrap().to_string()
    }

    /// POST a raw body to `/rpc` and return `(status, frames, raw_text)`, where
    /// `frames` is every non-empty NDJSON line parsed as JSON. Suitable for
    /// methods that terminate (sendMessage, unsubscribe, envelope errors); NOT
    /// for an open-ended `session.subscribe` (use [`Self::subscribe_collect`]).
    async fn rpc_raw(
        &self,
        session_id: &str,
        token: &str,
        body: String,
    ) -> (u16, Vec<Value>, String) {
        let url = format!("{}/api/v1/sessions/{session_id}/rpc", self.client);
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("rpc send");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let frames = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect();
        (status, frames, text)
    }

    /// [`Self::rpc_raw`] with a JSON `Value` body.
    async fn rpc(&self, session_id: &str, token: &str, body: Value) -> (u16, Vec<Value>, String) {
        self.rpc_raw(session_id, token, serde_json::to_string(&body).unwrap())
            .await
    }

    /// Open a `session.subscribe` stream and collect frames until one satisfies
    /// `stop` or `timeout` elapses (returns whatever was collected on timeout).
    /// Dropping the response on return ends the subscription server-side.
    async fn subscribe_collect(
        &self,
        session_id: &str,
        token: &str,
        since_event_id: Option<&str>,
        stop: StopWhen,
        timeout: Duration,
    ) -> Vec<Value> {
        let url = format!("{}/api/v1/sessions/{session_id}/rpc", self.client);
        let params = match since_event_id {
            Some(s) => json!({ "since_event_id": s }),
            None => json!({}),
        };
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session.subscribe",
            "params": params,
        });
        let mut resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .expect("subscribe send");

        let mut buf: Vec<u8> = Vec::new();
        let mut frames: Vec<Value> = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let chunk = match tokio::time::timeout(deadline - now, resp.chunk()).await {
                Ok(Ok(Some(c))) => c,
                // timeout, clean end, or transport error
                _ => break,
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<Value>(line) else {
                    continue;
                };
                let matched = match &stop {
                    StopWhen::EventType(t) => v["params"]["event_type"] == json!(t),
                    StopWhen::EventId(id) => v["params"]["id"] == json!(id),
                };
                frames.push(v);
                if matched {
                    return frames;
                }
            }
        }
        frames
    }
}

/// The ordered `event_type` list of the `session.event` notifications in a frame
/// list (ignoring the terminal result/error).
fn notification_event_types(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .filter(|f| f["method"] == json!("session.event"))
        .map(|f| {
            f["params"]["event_type"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

/// The ordered event `id` list of the `session.event` notifications in a frame list.
fn notification_event_ids(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .filter(|f| f["method"] == json!("session.event"))
        .map(|f| f["params"]["id"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// The first JSON-RPC error code among a frame list, if any.
fn rpc_error_code(frames: &[Value]) -> Option<i64> {
    frames.iter().find_map(|f| {
        f.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64)
    })
}

/// Frames that are terminal responses (carry `result` or `error`).
fn terminal_frames(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|f| f.get("result").is_some() || f.get("error").is_some())
        .collect()
}

/// Absolute path to a file under `tests/fixtures/`.
fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Build an MCP registry from a `bae-config.toml` string through the **real**
/// loader + validator, exactly as startup would.
fn registry_from_toml(toml: &str) -> HashMap<String, McpServerConfig> {
    let dir = std::env::temp_dir().join(format!("baecfg-{}", generate_id("")));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bae-config.toml");
    std::fs::write(&path, toml).unwrap();
    let cfg = BaeConfigFile::load(Some(&path)).expect("load bae-config.toml");
    let reg = cfg.mcp_registry().expect("build registry");
    let _ = std::fs::remove_dir_all(&dir);
    reg
}

/// A registry with one stdio server (`name`) backed by the echo fixture. If
/// `pidfile` is given, the fixture is told to record its PID there.
fn echo_registry(name: &str, pidfile: Option<&str>) -> HashMap<String, McpServerConfig> {
    let fixture_path = fixture("mcp_echo_server.py");
    let args = match pidfile {
        Some(pf) => format!("[{fixture_path:?}, {pf:?}]"),
        None => format!("[{fixture_path:?}]"),
    };
    registry_from_toml(&format!(
        "[[mcp.servers]]\nname = {name:?}\ntransport = \"stdio\"\ncommand = \"python3\"\nargs = {args}\n"
    ))
}

// ---------------------------------------------------------------------------
// Log capture (for the "not found is logged every session" assertion)
// ---------------------------------------------------------------------------

/// A `tracing` writer that appends formatted log lines to a shared buffer.
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install (once for the whole test binary) a global `tracing` subscriber that
/// captures `baesrv` error logs into a shared buffer, and return that buffer.
/// Every test shares it, so tests asserting on it use unique server names and
/// filter by them.
fn log_capture() -> Arc<Mutex<Vec<u8>>> {
    static BUF: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    static INIT: Once = Once::new();
    let buf = BUF.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone();
    INIT.call_once(|| {
        let writer = CaptureWriter(buf.clone());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("baesrv=error"))
            .with_writer(writer)
            .with_ansi(false)
            .without_time()
            .try_init();
    });
    buf
}

fn captured_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap()).into_owned()
}

// ---------------------------------------------------------------------------
// JSON-RPC envelope handling
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jsonrpc_envelope_error_codes() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    // Malformed JSON → -32700.
    let (status, frames, raw) = ts.rpc_raw(&sid, &key, "{ not json".into()).await;
    assert_eq!(status, 200, "envelope errors ride the 200 stream: {raw}");
    assert_eq!(rpc_error_code(&frames), Some(-32700), "parse error: {raw}");

    // Well-formed JSON that is not a request object → -32600.
    let (_s, frames, raw) = ts.rpc_raw(&sid, &key, "123".into()).await;
    assert_eq!(rpc_error_code(&frames), Some(-32600), "non-object: {raw}");

    // A batch (array) request → -32600.
    let batch = json!([{ "jsonrpc": "2.0", "id": 1, "method": "session.unsubscribe" }]);
    let (_s, frames, raw) = ts.rpc(&sid, &key, batch).await;
    assert_eq!(rpc_error_code(&frames), Some(-32600), "batch: {raw}");

    // Missing/!2.0 `jsonrpc` version → -32600.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "id": 1, "method": "session.unsubscribe" }),
        )
        .await;
    assert_eq!(
        rpc_error_code(&frames),
        Some(-32600),
        "missing jsonrpc: {raw}"
    );

    // Missing `method` → -32600.
    let (_s, frames, raw) = ts
        .rpc(&sid, &key, json!({ "jsonrpc": "2.0", "id": 1 }))
        .await;
    assert_eq!(
        rpc_error_code(&frames),
        Some(-32600),
        "missing method: {raw}"
    );

    // Unknown `method` → -32601, and the response echoes the request id.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 42, "method": "no.such.method", "params": {} }),
        )
        .await;
    assert_eq!(
        rpc_error_code(&frames),
        Some(-32601),
        "unknown method: {raw}"
    );
    let terminals = terminal_frames(&frames);
    assert_eq!(
        terminals.len(),
        1,
        "exactly one response for an id'd request: {raw}"
    );
    assert_eq!(
        terminals[0]["id"],
        json!(42),
        "response echoes the request id: {raw}"
    );

    // Known method, missing required `params.message` → -32602.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "session.sendMessage", "params": {} }),
        )
        .await;
    assert_eq!(
        rpc_error_code(&frames),
        Some(-32602),
        "missing message param: {raw}"
    );

    // Known method, `params.message` present but the wrong shape → -32602.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 6, "method": "session.sendMessage",
                    "params": { "message": 5 } }),
        )
        .await;
    assert_eq!(
        rpc_error_code(&frames),
        Some(-32602),
        "invalid message param: {raw}"
    );

    // NOTIFICATION SEMANTICS: an object with no `id` is a JSON-RPC notification.
    // The server performs the method's side effect (here, cancelling any active
    // subscriptions) but MUST NOT send a response — no result, no error frame.
    let (status, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "method": "session.unsubscribe" }),
        )
        .await;
    assert_eq!(
        status, 200,
        "notification still opens the 200 stream: {raw}"
    );
    let terminals = terminal_frames(&frames);
    assert!(
        terminals.is_empty(),
        "a no-id request (notification) gets no response frame: {raw}"
    );
}

// ---------------------------------------------------------------------------
// session.sendMessage live notification delivery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_streams_events_then_one_terminal() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session.sendMessage",
                    "params": { "message": { "role": "user", "content": "hello" } } }),
        )
        .await;
    assert_eq!(status, 200, "rpc stream: {raw}");

    // The live notifications, in order, exclude the client's own
    // client.message.send (a client-authored event is never echoed back).
    assert_eq!(
        notification_event_types(&frames),
        vec![
            "provider.request",
            "provider.response",
            "server.message.send"
        ],
        "live notifications in order, client-authored events excluded: {raw}"
    );

    // Exactly one terminal response, carrying the request id and the full
    // `{message, events}` body — where `events` still includes the filtered
    // client.message.send (nothing is lost for a caller that ignores the feed).
    let terminals = terminal_frames(&frames);
    assert_eq!(terminals.len(), 1, "exactly one terminal response: {raw}");
    let terminal = terminals[0];
    assert_eq!(terminal["id"], json!(1));
    assert_eq!(terminal["result"]["message"]["role"], json!("assistant"));
    let result_types = event_types(&terminal["result"]["events"]);
    assert!(
        result_types.contains(&"client.message.send".to_string()),
        "terminal result.events retains the full turn including client.message.send: {result_types:?}"
    );
    // The terminal is the last frame (notifications precede it).
    assert!(
        frames.last().unwrap().get("result").is_some(),
        "terminal response comes after all notifications: {raw}"
    );
}

// ---------------------------------------------------------------------------
// session.subscribe — live delivery from a second connection
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_receives_live_turn_events_from_a_second_connection() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    // One connection subscribes; a second drives a turn with the same session
    // key. The subscribe future and the (delayed) send future run concurrently
    // on this task — the send waits briefly so the subscribe is live first.
    let subscriber = ts.subscribe_collect(
        &sid,
        &key,
        None,
        StopWhen::EventType("server.message.send".into()),
        Duration::from_secs(5),
    );
    let driver = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        ts.send_message(&sid, &key, json!({ "role": "user", "content": "hi" }))
            .await
    };
    let (frames, (send_status, _res, _raw)) = tokio::join!(subscriber, driver);
    assert_eq!(send_status, 200, "the driving sendMessage succeeded");

    // The subscribe feed carries the same live, ordered, client-filtered events.
    assert_eq!(
        notification_event_types(&frames),
        vec![
            "provider.request",
            "provider.response",
            "server.message.send"
        ],
        "subscribe delivered the turn's events in order, client-authored events excluded"
    );
}

// ---------------------------------------------------------------------------
// session.subscribe — resume via since_event_id
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_resume_replays_after_since_event_id_without_gap_or_dup() {
    let ts = start_server().await;
    let profile_id = ts.create_profile("text", json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    // Persist two turns' worth of events.
    let (s1, _r1, _raw1) = ts
        .send_message(&sid, &key, json!({ "content": "one" }))
        .await;
    assert_eq!(s1, 200);
    let (s2, _r2, _raw2) = ts
        .send_message(&sid, &key, json!({ "content": "two" }))
        .await;
    assert_eq!(s2, 200);

    // The authoritative, ordered, *unfiltered* history (replay is unfiltered too,
    // matching this — the documented replay/live seam).
    let (_s, events, _raw) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&key))
        .await;
    let items = events["items"].as_array().unwrap();
    assert!(
        items.len() >= 6,
        "expected several persisted events: {items:?}"
    );
    let ids: Vec<String> = items
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();

    // Reconnect mid-history: resume after the 3rd event.
    let since = &ids[2];
    let last_id = ids.last().unwrap().clone();
    let expected_after: Vec<String> = ids[3..].to_vec();

    let frames = ts
        .subscribe_collect(
            &sid,
            &key,
            Some(since),
            StopWhen::EventId(last_id.clone()),
            Duration::from_secs(5),
        )
        .await;
    let got = notification_event_ids(&frames);

    // No missed events, no duplicates, exactly the tail after `since`.
    assert_eq!(
        got, expected_after,
        "replay covers exactly the events after since_event_id"
    );
    let mut unique = got.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        got.len(),
        "no duplicate events in the resume replay"
    );
}

// ---------------------------------------------------------------------------
// MCP round trip — real (non-stub) payloads via a local stdio fixture server
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_round_trip_emits_real_payloads() {
    let ts = start_server_with_registry(echo_registry("echo", None)).await;
    // The profile opts into the "echo" MCP server; the client declares no tools,
    // so the provider's tool_use for `remote_search` is dispatched to MCP.
    let profile_id = ts
        .create_profile_with_mcp("mcp", json!(["echo"]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&sid, &key, json!({ "content": "search please" }))
        .await;
    assert_eq!(status, 200, "mcp turn: {raw}");
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("after mcp stub")
    );

    let evs = resp["events"].as_array().unwrap();
    let find = |t: &str| -> Value {
        evs.iter()
            .find(|e| e["event_type"] == json!(t))
            .unwrap_or_else(|| panic!("missing {t} event: {raw}"))
            .clone()
    };

    // Real mcp.request — routed to the named server, carrying the tool + input.
    let req = find("mcp.request");
    assert_eq!(req["payload"]["method"], json!("tools/call"));
    assert_eq!(req["payload"]["server_name"], json!("echo"));
    assert_eq!(req["payload"]["tool"], json!("remote_search"));
    assert_eq!(req["payload"]["input"], json!({ "q": "x" }));

    // Real mcp.response — non-stub result content produced by the fixture.
    let res = find("mcp.response");
    assert_eq!(res["payload"]["server_name"], json!("echo"));
    assert_eq!(res["payload"]["ok"], json!(true));
    assert_eq!(
        res["payload"]["result"]["content"][0]["text"],
        json!("echo: x"),
        "the fixture's real tool result flows through: {raw}"
    );

    // Real tool.result — mcp dispatch, not an error, carrying the real content.
    let tr = find("tool.result");
    assert_eq!(tr["payload"]["dispatch"], json!("mcp"));
    assert_eq!(tr["payload"]["server_name"], json!("echo"));
    assert_eq!(tr["payload"]["is_error"], json!(false));
    assert_eq!(tr["payload"]["content"][0]["text"], json!("echo: x"));

    // No trace of the removed stub payload shape (`{"status":"stub", ...}`)
    // anywhere in the turn. (The mock's final text happens to read "after mcp
    // stub", so we match the exact old marker, not the bare word.)
    assert!(
        !resp.to_string().contains("\"status\":\"stub\""),
        "the removed stub payload shape must not appear: {raw}"
    );

    let (status, _v, _raw) = ts
        .client_delete(&format!("/api/v1/sessions/{sid}"), Some(&key))
        .await;
    assert_eq!(status, 200, "close tears down the MCP subprocess");
}

// ---------------------------------------------------------------------------
// Invalid MCP server name — logged every session creation (not deduplicated)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_mcp_name_logged_on_every_session_while_valid_connects() {
    // Install the log capture BEFORE the server starts so its errors are caught.
    let logs = log_capture();
    let ts = start_server_with_registry(echo_registry("echo", None)).await;
    // A unique missing name so this test's log lines are unambiguous in the
    // shared capture buffer.
    let ghost = format!("ghost-{}", generate_id(""));
    let profile_id = ts
        .create_profile_with_mcp("mcp", json!(["echo", ghost]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;

    // Two separate sessions against the same profile. For each, drive an MCP turn
    // and assert the valid "echo" server connected (real, ok:true mcp.response).
    for attempt in 0..2 {
        let (sid, key, _) = ts.open_session(&client_key, json!([])).await;
        let (status, resp, raw) = ts
            .send_message(&sid, &key, json!({ "content": "search please" }))
            .await;
        assert_eq!(status, 200, "attempt {attempt}: {raw}");
        let res = resp["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["event_type"] == json!("mcp.response"))
            .unwrap_or_else(|| panic!("attempt {attempt} missing mcp.response: {raw}"));
        assert_eq!(
            res["payload"]["server_name"],
            json!("echo"),
            "attempt {attempt}"
        );
        assert_eq!(
            res["payload"]["ok"],
            json!(true),
            "attempt {attempt}: echo connected"
        );
        let _ = ts
            .client_delete(&format!("/api/v1/sessions/{sid}"), Some(&key))
            .await;
    }

    // The "not found" error was logged for the ghost name on BOTH creations —
    // exactly twice, i.e. never deduplicated/cached across sessions.
    let text = captured_text(&logs);
    let not_found_lines = text
        .lines()
        .filter(|l| l.contains(&ghost) && l.contains("not found"))
        .count();
    assert_eq!(
        not_found_lines, 2,
        "the missing MCP server must be logged on every session creation (found {not_found_lines}):\n{text}"
    );
}

// ---------------------------------------------------------------------------
// MCP connection failure is non-fatal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_connection_failure_is_non_fatal() {
    let logs = log_capture();
    // A stdio server whose command does not exist — connect will fail.
    let bad = format!("broken-{}", generate_id(""));
    let registry = registry_from_toml(&format!(
        "[[mcp.servers]]\nname = {bad:?}\ntransport = \"stdio\"\ncommand = \"baesrv-no-such-binary-xyzzy\"\n"
    ));
    let ts = start_server_with_registry(registry).await;

    let profile_id = ts
        .create_profile_with_mcp("mcp", json!([bad]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;

    // Session creation still succeeds despite the unreachable server.
    let (status, v, raw) = ts
        .client_post(
            "/api/v1/sessions",
            Some(&client_key),
            json!({ "tools": [] }),
        )
        .await;
    assert_eq!(
        status, 201,
        "connect failure must not fail session creation: {raw}"
    );
    let sid = v["session_id"].as_str().unwrap().to_string();
    let key = v["session_key"].as_str().unwrap().to_string();
    ts.register_driver(&sid, &key).await;

    // The failing server is absent from the session's tools: the provider's
    // tool_use for `remote_search` has no MCP server to route to, so the turn
    // still completes with an error-shaped (non-fatal) tool.result.
    let (status, resp, raw) = ts
        .send_message(&sid, &key, json!({ "content": "search please" }))
        .await;
    assert_eq!(status, 200, "turn completes despite absent tool: {raw}");
    let tr = resp["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("tool.result"))
        .expect("a tool.result event");
    assert_eq!(
        tr["payload"]["is_error"],
        json!(true),
        "absent server → error result"
    );

    // And the connect failure was logged.
    let text = captured_text(&logs);
    assert!(
        text.lines()
            .any(|l| l.contains(&bad) && l.contains("failed to connect")),
        "the connect failure must be logged for {bad}:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Cleanup on session close — spawned MCP subprocess is terminated
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_close_terminates_spawned_mcp_subprocess() {
    // The fixture records its PID so we can confirm the subprocess is reaped.
    let pid_dir = std::env::temp_dir().join(format!("baepid-{}", generate_id("")));
    std::fs::create_dir_all(&pid_dir).unwrap();
    let pidfile = pid_dir.join("mcp.pid");
    let pidfile_str = pidfile.to_str().unwrap().to_string();

    let ts = start_server_with_registry(echo_registry("echo", Some(&pidfile_str))).await;
    let profile_id = ts
        .create_profile_with_mcp("text", json!(["echo"]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    // The fixture writes its PID at startup; wait for it, then confirm alive.
    let pid = read_pid(&pidfile).await.expect("fixture wrote its PID");
    assert!(
        proc_exists(pid),
        "MCP subprocess {pid} should be running while the session is open"
    );

    // Closing the session tears the subprocess down.
    let (status, _v, raw) = ts
        .client_delete(&format!("/api/v1/sessions/{sid}"), Some(&key))
        .await;
    assert_eq!(status, 200, "close: {raw}");

    // The spawned subprocess is terminated (poll for reaping).
    let mut terminated = false;
    for _ in 0..100 {
        if !proc_exists(pid) {
            terminated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        terminated,
        "MCP subprocess {pid} must be terminated after DELETE"
    );

    let _ = std::fs::remove_dir_all(&pid_dir);
    // (The broadcast-registry-entry removal on close is covered directly by the
    // `broadcast::tests::remove_closes_receivers` unit test.)
}

/// Poll `pidfile` until it holds a parseable PID (or give up after ~2s).
async fn read_pid(pidfile: &std::path::Path) -> Option<u32> {
    for _ in 0..40 {
        if let Ok(s) = std::fs::read_to_string(pidfile) {
            if let Ok(pid) = s.trim().parse::<u32>() {
                return Some(pid);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Whether a process with `pid` currently exists (Linux `/proc` check — the test
/// container is Linux, matching the deployment target).
fn proc_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

// ===========================================================================
// WI 0005 — parallel client handling
//
// join / multi-key sessions, driver registration, the FIFO turn gate, the
// abandonment timeout, and the OpenAI-kind provider translation / mixed-kind
// fallback chains. See `/awman/context/workflow/test-plan.md` for the mapping
// from each work-item "Test Consideration" to the test(s) below.
//
// Everything here stays offline: mock provider HTTP servers (Anthropic- and
// OpenAI-shaped) plus a short `BAE_TURN_TIMEOUT` override for the abandonment
// test — no real LLM calls and no wall-clock 120 s waits.
// ===========================================================================

/// Recorded provider request bodies, in receive order (newest last).
type Recorder = Arc<Mutex<Vec<Value>>>;

/// A `[[providers.entries]]` TOML fragment for one registry entry, so a test can
/// build a provider registry pointing at its own mock(s) through the **real**
/// loader ([`provider_registry_from_toml`]) exactly as startup would.
fn provider_entry_toml(name: &str, kind: &str, base_url: &str, auth_token: &str) -> String {
    format!(
        "[[providers.entries]]\nname = {name:?}\nprovider = {kind:?}\n\
         base_url = {base_url:?}\nmodel = \"m-mock\"\nauth_token = {auth_token:?}\n\n"
    )
}

/// Whether the last message in a provider request body carries a block of the
/// given `type` (used by the mock to decide the tool-loop phase).
fn last_message_has_block(body: &Value, block_type: &str) -> bool {
    body.get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.last())
        .and_then(|last| last.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some(block_type))
        })
        .unwrap_or(false)
}

/// The sorted `name` list from a provider request body's `tools` array.
fn req_tool_names(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// --- Anthropic-shaped recording/delay mock ---------------------------------

/// Start an Anthropic-Messages-shaped mock that records every request body into
/// the returned [`Recorder`] and waits `delay` before answering (`Duration::ZERO`
/// for none — the delay is what makes the FIFO test's turns genuinely overlap).
/// Behaviour is chosen by the leading path segment, like [`mock_handler`]:
/// `/text` (always final text "Hello from anthropic mock"), `/tool` (a
/// `get_current_time` tool_use until a `tool_result` arrives, then text),
/// `/fail` (HTTP 500). Returns `(base_url, recorder)`.
async fn start_anthropic_recording_mock(delay: Duration) -> (String, Recorder) {
    let requests: Recorder = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(anthropic_recording_handler)
        .with_state((requests.clone(), delay));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), requests)
}

async fn anthropic_recording_handler(
    axum::extract::State((requests, delay)): axum::extract::State<(Recorder, Duration)>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    requests.lock().unwrap().push(body.clone());
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    if path.starts_with("/fail") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "boom" })),
        )
            .into_response();
    }
    let out = if path.starts_with("/tool") {
        if last_message_has_block(&body, "tool_result") {
            json!({ "role": "assistant", "stop_reason": "end_turn",
                    "content": [{ "type": "text", "text": "tool round-trip complete" }] })
        } else {
            json!({ "role": "assistant", "stop_reason": "tool_use", "content": [
                { "type": "tool_use", "id": "tu_1", "name": "get_current_time", "input": {} }] })
        }
    } else {
        json!({ "role": "assistant", "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": "Hello from anthropic mock" }] })
    };
    (StatusCode::OK, Json(out)).into_response()
}

// --- OpenAI-shaped recording mock ------------------------------------------

/// Start an OpenAI-Chat-Completions-shaped mock that records every request body.
/// Behaviour by leading path segment: `/oai-text` (plain text completion "Hello
/// from openai mock"), `/oai-tool` (a `tool_calls` response for
/// `get_current_time` until the request carries a `role:"tool"` message, then
/// text), `/oai-fail` (HTTP 500). Returns `(base_url, recorder)`.
async fn start_openai_mock() -> (String, Recorder) {
    let requests: Recorder = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(openai_handler)
        .with_state(requests.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), requests)
}

async fn openai_handler(
    axum::extract::State(requests): axum::extract::State<Recorder>,
    req: Request,
) -> Response {
    let path = req.uri().path().to_string();
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    requests.lock().unwrap().push(body.clone());
    if path.starts_with("/oai-fail") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": { "message": "boom" } })),
        )
            .into_response();
    }
    // The OpenAI translation emits `role:"tool"` messages for returned results.
    let has_tool_msg = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|ms| {
            ms.iter()
                .any(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        })
        .unwrap_or(false);
    let out = if path.starts_with("/oai-tool") && !has_tool_msg {
        json!({ "choices": [{ "message": {
            "role": "assistant", "content": Value::Null,
            "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": { "name": "get_current_time", "arguments": "{}" },
            }],
        } }] })
    } else if path.starts_with("/oai-tool") {
        json!({ "choices": [{ "message": {
            "role": "assistant", "content": "tool round-trip complete" } }] })
    } else {
        json!({ "choices": [{ "message": {
            "role": "assistant", "content": "Hello from openai mock" } }] })
    };
    (StatusCode::OK, Json(out)).into_response()
}

// --- Multi-client test helpers ---------------------------------------------

impl TestServer {
    /// Open a session **without** registering as a driver — for the driver-gate
    /// test, which asserts `session.sendMessage` is refused until registration.
    async fn open_session_no_register(
        &self,
        client_key: &str,
        tools: Value,
    ) -> (String, String, Value) {
        let (status, v, raw) = self
            .client_post(
                "/api/v1/sessions",
                Some(client_key),
                json!({ "client_version": "1.0.0", "tools": tools }),
            )
            .await;
        assert_eq!(status, 201, "open session failed: {raw}");
        (
            v["session_id"].as_str().unwrap().to_string(),
            v["session_key"].as_str().unwrap().to_string(),
            v,
        )
    }

    /// `POST /api/v1/sessions/{id}/join`, returning `(status, value, raw)`
    /// without registering — for the error/edge cases (profile_mismatch,
    /// session_closed, primary_provider_unavailable).
    async fn join_raw(
        &self,
        session_id: &str,
        client_key: &str,
        tools: Value,
    ) -> (u16, Value, String) {
        self.client_post(
            &format!("/api/v1/sessions/{session_id}/join"),
            Some(client_key),
            json!({ "client_version": "1.0.0", "tools": tools }),
        )
        .await
    }

    /// Join a session on success and register the joiner as a driver; returns
    /// its session key. Mirrors what an SDK `join()` does.
    async fn join_session(&self, session_id: &str, client_key: &str, tools: Value) -> String {
        let (status, v, raw) = self.join_raw(session_id, client_key, tools).await;
        assert_eq!(status, 201, "join failed: {raw}");
        let key = v["session_key"].as_str().unwrap().to_string();
        self.register_driver(session_id, &key).await;
        key
    }

    /// `GET /api/v1/sessions/{id}/participants`.
    async fn participants(&self, session_id: &str, session_key: &str) -> (u16, Value, String) {
        self.client_get(
            &format!("/api/v1/sessions/{session_id}/participants"),
            Some(session_key),
        )
        .await
    }

    /// The client-key id for a plaintext client key, via the admin list (prefix
    /// match, same technique as [`auth_rejection_cases`]).
    async fn client_key_id(&self, plaintext: &str) -> String {
        let (_s, list, _r) = self.admin_get("/admin/v1/keys").await;
        let prefix = &plaintext[..8];
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["prefix"].as_str() == Some(prefix))
            .map(|k| k["id"].as_str().unwrap().to_string())
            .expect("client key listed")
    }

    /// Revoke a client key by id (asserting the 204).
    async fn revoke_key(&self, key_id: &str) {
        let (status, _v, raw) = self.admin_delete(&format!("/admin/v1/keys/{key_id}")).await;
        assert_eq!(status, 204, "revoke failed: {raw}");
    }

    /// The full, ordered `(event_type, client_key_id)` list from the replay
    /// endpoint — for asserting turn ownership and non-interleaving.
    async fn replay_pairs(&self, session_id: &str, session_key: &str) -> Vec<(String, String)> {
        let (_s, events, _r) = self
            .client_get(
                &format!("/api/v1/sessions/{session_id}/events"),
                Some(session_key),
            )
            .await;
        events["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["event_type"].as_str().unwrap_or("?").to_string(),
                    e["client_key_id"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// A) join — profile-match guard + terminal-session guard
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_profile_match_guard() {
    let ts = start_server().await;
    let profile_a = ts.create_profile("text", json!([]), json!([])).await;
    let profile_b = ts.create_profile("text", json!([]), json!([])).await;
    let key_a = ts.create_key(&profile_a).await;
    let key_a2 = ts.create_key(&profile_a).await; // same profile as the session
    let key_b = ts.create_key(&profile_b).await; // a *different* profile

    let (sid, skey_a, _) = ts.open_session(&key_a, json!([])).await;

    // Same profile → join succeeds and returns a usable session key.
    let (status, v, raw) = ts.join_raw(&sid, &key_a2, json!([])).await;
    assert_eq!(status, 201, "same-profile join must succeed: {raw}");
    let joiner_key = v["session_key"].as_str().unwrap().to_string();
    assert!(joiner_key.starts_with("bae_ses_"));
    let (s, _v, _r) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&joiner_key))
        .await;
    assert_eq!(s, 200, "the joiner's session key authenticates");

    // Exactly one session.join so far; snapshot the log length.
    let before = ts.replay_pairs(&sid, &skey_a).await;
    let joins_before = before.iter().filter(|(t, _)| t == "session.join").count();
    assert_eq!(joins_before, 1);

    // Different profile → 403 profile_mismatch, *before* any event is logged or
    // session key minted (the hard boundary): the log is byte-for-byte unchanged.
    let (status, err, raw) = ts.join_raw(&sid, &key_b, json!([])).await;
    assert_eq!(status, 403, "cross-profile join must be refused: {raw}");
    assert_eq!(err["type"], json!("profile_mismatch"));

    let after = ts.replay_pairs(&sid, &skey_a).await;
    assert_eq!(
        before, after,
        "a profile_mismatch join must log no event on the session"
    );
    // The rejected client's session key was never minted, so it cannot auth
    // (there is nothing to present — assert no *second* join event exists).
    assert_eq!(
        after.iter().filter(|(t, _)| t == "session.join").count(),
        1,
        "the mismatched attempt minted no session key / logged no join"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_closed_session_is_conflict() {
    let ts = start_server().await;
    let profile = ts.create_profile("text", json!([]), json!([])).await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let (sid, skey_a, _) = ts.open_session(&key_a, json!([])).await;

    let (s, _v, raw) = ts
        .client_delete(&format!("/api/v1/sessions/{sid}"), Some(&skey_a))
        .await;
    assert_eq!(s, 200, "close: {raw}");

    // A joiner cannot resurrect a terminal session (same error shape as close's
    // conflict).
    let (s, err, raw) = ts.join_raw(&sid, &key_b, json!([])).await;
    assert_eq!(s, 409, "join on a closed session: {raw}");
    assert_eq!(err["type"], json!("session_closed"));
}

// ---------------------------------------------------------------------------
// A) multi-key session lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_key_session_lifecycle() {
    let ts = start_server().await;
    let profile = ts.create_profile("text", json!([]), json!([])).await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;

    let (sid, skey_a, _) = ts.open_session(&key_a, json!([])).await;
    let skey_b = ts.join_session(&sid, &key_b, json!([])).await;

    // Both keys drive a turn via session.sendMessage.
    let (s1, r1, raw1) = ts
        .send_message(&sid, &skey_a, json!({ "content": "hi from a" }))
        .await;
    assert_eq!(s1, 200, "A sendMessage: {raw1}");
    assert_eq!(r1["message"]["role"], json!("assistant"));
    let (s2, r2, raw2) = ts
        .send_message(&sid, &skey_b, json!({ "content": "hi from b" }))
        .await;
    assert_eq!(s2, 200, "B sendMessage: {raw2}");
    assert_eq!(r2["message"]["role"], json!("assistant"));

    // The joiner (B) also authenticates via session.subscribe: it receives A's
    // live turn events on its own session key.
    let subscriber = ts.subscribe_collect(
        &sid,
        &skey_b,
        None,
        StopWhen::EventType("server.message.send".into()),
        Duration::from_secs(5),
    );
    let driver = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ts.send_message(&sid, &skey_a, json!({ "content": "watch this" }))
            .await
    };
    let (frames, (ds, _res, _raw)) = tokio::join!(subscriber, driver);
    assert_eq!(ds, 200, "driver turn during B's subscribe");
    assert!(
        notification_event_types(&frames).contains(&"server.message.send".to_string()),
        "B's subscribe (session key) receives live events"
    );

    // GET participants shows both registered drivers.
    let mut expected = vec![
        ts.client_key_id(&key_a).await,
        ts.client_key_id(&key_b).await,
    ];
    expected.sort();
    let (ps, pv, praw) = ts.participants(&sid, &skey_a).await;
    assert_eq!(ps, 200, "participants: {praw}");
    assert_eq!(pv["drivers"], json!(expected));

    // The event log shows both session.open and session.join.
    let types: Vec<String> = ts
        .replay_pairs(&sid, &skey_a)
        .await
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert!(types.contains(&"session.open".to_string()));
    assert!(types.contains(&"session.join".to_string()));
}

// ---------------------------------------------------------------------------
// A) revoke no longer nukes shared sessions
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_joiner_leaves_shared_session_open_last_key_closes() {
    let ts = start_server().await;
    let profile = ts.create_profile("text", json!([]), json!([])).await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let (sid, skey_a, _) = ts.open_session(&key_a, json!([])).await;
    let skey_b = ts.join_session(&sid, &key_b, json!([])).await;

    // Revoke the joiner's client key: its session key dies, but the session
    // stays open and the creator's driver still authenticates.
    let kid_b = ts.client_key_id(&key_b).await;
    ts.revoke_key(&kid_b).await;

    let (s, _v, _r) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&skey_b))
        .await;
    assert_eq!(
        s, 401,
        "the revoked joiner's session key no longer authenticates"
    );
    let (s, _v, _r) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&skey_a))
        .await;
    assert_eq!(s, 200, "the other driver's session key still authenticates");
    let (s, _r, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "still here" }))
        .await;
    assert_eq!(
        s, 200,
        "session stays open after the joiner is revoked: {raw}"
    );

    // Revoke the last remaining active key → the session auto-closes.
    let kid_a = ts.client_key_id(&key_a).await;
    ts.revoke_key(&kid_a).await;
    let (s, _v, _r) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&skey_a))
        .await;
    assert_eq!(s, 401, "the last session key is now revoked");
    // A fresh client on the same profile finds the session terminal.
    let key_c = ts.create_key(&profile).await;
    let (s, err, raw) = ts.join_raw(&sid, &key_c, json!([])).await;
    assert_eq!(
        s, 409,
        "session auto-closed after the last key was revoked: {raw}"
    );
    assert_eq!(err["type"], json!("session_closed"));
}

// ---------------------------------------------------------------------------
// B) driver registration gate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn driver_registration_gate() {
    let ts = start_server().await;
    let profile = ts.create_profile("text", json!([]), json!([])).await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts.open_session_no_register(&key, json!([])).await;

    // session.sendMessage before session.registerDriver → -32001.
    let (status, frames, raw) = ts
        .rpc(
            &sid,
            &skey,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "session.sendMessage",
                    "params": { "message": { "content": "hi" } } }),
        )
        .await;
    assert_eq!(status, 200, "rpc stream opens: {raw}");
    assert_eq!(
        rpc_error_code(&frames),
        Some(-32001),
        "an unregistered driver's send is refused: {raw}"
    );

    // After registering, the send succeeds.
    ts.register_driver(&sid, &skey).await;
    let (s, r, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "hi" }))
        .await;
    assert_eq!(s, 200, "registered send: {raw}");
    assert_eq!(r["message"]["role"], json!("assistant"));

    // A subscribe-only connection never needs to register: a joiner that only
    // subscribes (no registerDriver) still authenticates and receives live
    // events.
    let key_obs = ts.create_key(&profile).await;
    let (js, jv, jraw) = ts.join_raw(&sid, &key_obs, json!([])).await;
    assert_eq!(js, 201, "observer join: {jraw}");
    let skey_obs = jv["session_key"].as_str().unwrap().to_string();
    let subscriber = ts.subscribe_collect(
        &sid,
        &skey_obs,
        None,
        StopWhen::EventType("server.message.send".into()),
        Duration::from_secs(5),
    );
    let driver = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ts.send_message(&sid, &skey, json!({ "content": "observe me" }))
            .await
    };
    let (frames, (ds, _res, _raw)) = tokio::join!(subscriber, driver);
    assert_eq!(ds, 200);
    assert!(
        notification_event_types(&frames).contains(&"server.message.send".to_string()),
        "a subscribe-only connection receives events without registerDriver"
    );
}

// ---------------------------------------------------------------------------
// B) per-turn tool scoping (asserted against the mock's received tool list)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_turn_tool_scoping_advertises_only_the_acting_driver() {
    let (mock, requests) = start_anthropic_recording_mock(Duration::ZERO).await;
    let reg = provider_registry_from_toml(&provider_entry_toml(
        "text",
        "anthropic",
        &format!("{mock}/text"),
        "test-token",
    ));
    let ts = boot_server(HashMap::new(), reg, None).await;

    let profile = ts
        .create_profile("text", json!([]), json!(["only_a", "only_b"]))
        .await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let tools_a =
        json!([{ "name": "only_a", "input_schema": { "type": "object", "properties": {} } }]);
    let tools_b =
        json!([{ "name": "only_b", "input_schema": { "type": "object", "properties": {} } }]);

    let (sid, skey_a, _) = ts.open_session(&key_a, tools_a).await;
    let skey_b = ts.join_session(&sid, &key_b, tools_b).await;

    // Sequential turns so the recorded requests map deterministically: A then B.
    let (s, resp_a, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "a turn" }))
        .await;
    assert_eq!(s, 200, "A turn: {raw}");
    let (s, _resp_b, raw) = ts
        .send_message(&sid, &skey_b, json!({ "content": "b turn" }))
        .await;
    assert_eq!(s, 200, "B turn: {raw}");

    // The mock's ACTUALLY-received tool lists (not just the persisted event):
    let reqs = requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "one provider request per text turn");
    assert_eq!(
        req_tool_names(&reqs[0]),
        vec!["only_a".to_string()],
        "A's turn advertises only A's tool"
    );
    assert_eq!(
        req_tool_names(&reqs[1]),
        vec!["only_b".to_string()],
        "B's turn advertises only B's tool — never A's"
    );

    // The persisted provider.request event agrees (the other seam).
    let pr_a = resp_a["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("provider.request"))
        .unwrap();
    let ev_names: Vec<String> = pr_a["payload"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(ev_names, vec!["only_a".to_string()]);
}

// ---------------------------------------------------------------------------
// B) FIFO ordering — two drivers' turns never interleave
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifo_two_drivers_turns_do_not_interleave() {
    // A per-request delay keeps each turn in flight long enough that, without
    // the FIFO gate, the two turns' events would interleave.
    let (mock, _rec) = start_anthropic_recording_mock(Duration::from_millis(400)).await;
    let reg = provider_registry_from_toml(&provider_entry_toml(
        "text",
        "anthropic",
        &format!("{mock}/text"),
        "test-token",
    ));
    let ts = boot_server(HashMap::new(), reg, None).await;
    let profile = ts.create_profile("text", json!([]), json!([])).await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let (sid, skey_a, _) = ts.open_session(&key_a, json!([])).await;
    let skey_b = ts.join_session(&sid, &key_b, json!([])).await;
    let kid_a = ts.client_key_id(&key_a).await;
    let kid_b = ts.client_key_id(&key_b).await;

    // Fire both back-to-back (concurrently at the transport layer).
    let a = ts.send_message(&sid, &skey_a, json!({ "content": "a" }));
    let b = ts.send_message(&sid, &skey_b, json!({ "content": "b" }));
    let ((sa, _, ra), (sb, _, rb)) = tokio::join!(a, b);
    assert_eq!(sa, 200, "A: {ra}");
    assert_eq!(sb, 200, "B: {rb}");

    // In the persisted log the two turns form two contiguous blocks: one
    // driver's full turn (through its terminal server.message.send) completes
    // before the other's first turn event appears.
    let turn_owners: Vec<String> = ts
        .replay_pairs(&sid, &skey_a)
        .await
        .into_iter()
        .filter(|(t, _)| {
            matches!(
                t.as_str(),
                "client.message.send"
                    | "provider.request"
                    | "provider.response"
                    | "server.message.send"
            )
        })
        .map(|(_, k)| k)
        .collect();
    assert_eq!(
        turn_owners.iter().filter(|k| **k == kid_a).count(),
        4,
        "A drove exactly one text turn"
    );
    assert_eq!(
        turn_owners.iter().filter(|k| **k == kid_b).count(),
        4,
        "B drove exactly one text turn"
    );
    let transitions = turn_owners.windows(2).filter(|w| w[0] != w[1]).count();
    assert_eq!(
        transitions, 1,
        "the turns must not interleave (ownership sequence: {turn_owners:?})"
    );
}

// ---------------------------------------------------------------------------
// B) same-owner continuation reuses the lock without queuing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_owner_continuation_runs_before_a_queued_driver() {
    let ts = start_server().await;
    let profile = ts
        .create_profile("tool", json!([]), json!(["get_current_time"]))
        .await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let tools = json!([{ "name": "get_current_time", "input_schema": { "type": "object", "properties": {} } }]);
    let (sid, skey_a, _) = ts.open_session(&key_a, tools).await;
    let skey_b = ts.join_session(&sid, &key_b, json!([])).await;
    let kid_a = ts.client_key_id(&key_a).await;
    let kid_b = ts.client_key_id(&key_b).await;

    // A's first turn pauses on a client-side tool call.
    let (s, first, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "time?" }))
        .await;
    assert_eq!(s, 200, "A first turn: {raw}");
    let tuid = first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == json!("tool_use"))
        .expect("A paused on a tool_use")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // B queues a message (blocks on the gate) while A is paused; concurrently A
    // sends its own continuation. A must reclaim the lock without waiting behind
    // B, and B must only run after A's continuation completes.
    let b_fut = ts.send_message(&sid, &skey_b, json!({ "content": "b message" }));
    let a_cont = async {
        tokio::time::sleep(Duration::from_millis(300)).await; // let B queue first
        ts.send_message(
            &sid,
            &skey_a,
            json!({ "content": [{ "type": "tool_result", "tool_use_id": tuid, "content": "12:00" }] }),
        )
        .await
    };
    let ((sb, _, rb), (sa, _, ra)) = tokio::join!(b_fut, a_cont);
    assert_eq!(sa, 200, "A continuation: {ra}");
    assert_eq!(sb, 200, "B eventually runs: {rb}");

    // A had two server.message.send events (the paused one + the completed
    // continuation); B's turn began only after the second.
    let pairs = ts.replay_pairs(&sid, &skey_a).await;
    let a_sms: Vec<usize> = pairs
        .iter()
        .enumerate()
        .filter(|(_, (t, k))| t == "server.message.send" && *k == kid_a)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        a_sms.len(),
        2,
        "A: a paused + a completed server.message.send"
    );
    let b_msg_idx = pairs
        .iter()
        .position(|(t, k)| t == "client.message.send" && *k == kid_b)
        .expect("B's message was logged");
    assert!(
        b_msg_idx > a_sms[1],
        "no queued driver's message ran between A's pause and its continuation \
         (B msg at {b_msg_idx}, A's continuation completed at {})",
        a_sms[1]
    );
}

// ---------------------------------------------------------------------------
// B) a paused turn's owner may send a brand-new message instead of a
//    tool_result continuation (ownership is by client key, not content shape)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paused_owner_may_abandon_its_tool_call_with_a_plain_message() {
    let ts = start_server().await;
    let profile = ts
        .create_profile("tool", json!([]), json!(["get_current_time"]))
        .await;
    let key = ts.create_key(&profile).await;
    let tools = json!([{ "name": "get_current_time", "input_schema": { "type": "object", "properties": {} } }]);
    let (sid, skey, _) = ts.open_session(&key, tools).await;

    // The first turn pauses on a client-side tool call.
    let (s, first, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "time?" }))
        .await;
    assert_eq!(s, 200, "first turn: {raw}");
    assert!(first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["type"] == json!("tool_use")));

    // The owner returns with a brand-new plain message, not a tool_result
    // continuation. Ownership is checked by client key id, not content shape,
    // so the parked turn is reclaimed and the message is served as the next
    // run_turn input (the /tool mock, seeing no tool_result, asks again).
    let (s, second, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "never mind that" }))
        .await;
    assert_eq!(s, 200, "plain-message send from the paused owner: {raw}");
    assert!(
        second.get("error").is_none(),
        "the owner's non-continuation message must be served, not refused: {raw}"
    );
    assert_eq!(second["message"]["role"], json!("assistant"));

    // Both messages were served as ordinary turns, and the reclaim was by
    // ownership — no timeout expiry was involved (no session.error logged).
    let pairs = ts.replay_pairs(&sid, &skey).await;
    assert_eq!(
        pairs
            .iter()
            .filter(|(t, _)| t == "client.message.send")
            .count(),
        2,
        "both of the owner's messages were logged as turn inputs"
    );
    assert!(
        !pairs.iter().any(|(t, _)| t == "session.error"),
        "a voluntary abandonment is a same-owner reclaim, never a \
         driver_turn_abandoned expiry: {pairs:?}"
    );
}

// ---------------------------------------------------------------------------
// B) cross-driver continuation blocks (does not error) until terminal
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_driver_send_blocks_until_owner_turn_terminal() {
    let ts = start_server().await;
    let profile = ts
        .create_profile("tool", json!([]), json!(["get_current_time"]))
        .await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let tools = json!([{ "name": "get_current_time", "input_schema": { "type": "object", "properties": {} } }]);
    let (sid, skey_a, _) = ts.open_session(&key_a, tools).await;
    let skey_b = ts.join_session(&sid, &key_b, json!([])).await;

    // A pauses on a client-side tool call.
    let (s, first, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "time?" }))
        .await;
    assert_eq!(s, 200, "A first turn: {raw}");
    let tuid = first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == json!("tool_use"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // B (a different, registered driver) sends mid-turn: its call must BLOCK
    // (not error) while A's turn is paused.
    let b_fut = ts.send_message(&sid, &skey_b, json!({ "content": "let me in" }));
    tokio::pin!(b_fut);
    let early = tokio::time::timeout(Duration::from_millis(600), &mut b_fut).await;
    assert!(
        early.is_err(),
        "B's send must block (no response) while A's turn is in flight"
    );

    // A reaches a terminal state via its continuation → B then unblocks and runs.
    let (sa, _, ra) = ts
        .send_message(
            &sid,
            &skey_a,
            json!({ "content": [{ "type": "tool_result", "tool_use_id": tuid, "content": "12:00" }] }),
        )
        .await;
    assert_eq!(sa, 200, "A continuation: {ra}");
    let (sb, rbody, rb) = b_fut.await;
    assert_eq!(
        sb, 200,
        "B unblocks and completes after A's turn is terminal: {rb}"
    );
    assert_eq!(rbody["message"]["role"], json!("assistant"));
}

// ---------------------------------------------------------------------------
// B) abandoned turn timeout (short BAE_TURN_TIMEOUT)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_turn_timeout_releases_gate_and_stays_open() {
    let (mock, _rec) = start_anthropic_recording_mock(Duration::ZERO).await;
    let reg = provider_registry_from_toml(&provider_entry_toml(
        "tool",
        "anthropic",
        &format!("{mock}/tool"),
        "test-token",
    ));
    // A short turn timeout so the abandoned paused turn expires in the test.
    let ts = boot_server(HashMap::new(), reg, Some(Duration::from_millis(300))).await;
    let profile = ts
        .create_profile("tool", json!([]), json!(["get_current_time"]))
        .await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let tools = json!([{ "name": "get_current_time", "input_schema": { "type": "object", "properties": {} } }]);
    let (sid, skey_a, _) = ts.open_session(&key_a, tools).await;
    let skey_b = ts.join_session(&sid, &key_b, json!([])).await;
    let kid_a = ts.client_key_id(&key_a).await;

    // A pauses and never returns its continuation.
    let (s, first, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "time?" }))
        .await;
    assert_eq!(s, 200, "A pauses: {raw}");
    assert!(first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["type"] == json!("tool_use")));

    // Wait past BAE_TURN_TIMEOUT, then B's queued message proceeds.
    tokio::time::sleep(Duration::from_millis(450)).await;
    let (sb, rb, raw) = ts
        .send_message(&sid, &skey_b, json!({ "content": "my turn now" }))
        .await;
    assert_eq!(sb, 200, "B proceeds after the abandonment timeout: {raw}");
    assert_eq!(rb["message"]["role"], json!("assistant"));

    // A session.error(driver_turn_abandoned) was logged, attributed to A, and
    // the session is still open.
    let (_s, events, _r) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&skey_b))
        .await;
    let abandoned = events["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["event_type"] == json!("session.error")
                && e["payload"]["reason"] == json!("driver_turn_abandoned")
        })
        .expect("driver_turn_abandoned logged");
    assert_eq!(abandoned["payload"]["owner_client_key_id"], json!(kid_a));
    assert_eq!(
        abandoned["client_key_id"],
        json!(kid_a),
        "the event is attributed to the abandoned owner"
    );

    // Session stays open: another turn still works.
    let (s, _r, raw) = ts
        .send_message(&sid, &skey_b, json!({ "content": "still open?" }))
        .await;
    assert_eq!(s, 200, "session stays open after abandonment: {raw}");
}

// ---------------------------------------------------------------------------
// C) fatal primary — at message time (finish_failed) + at join
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fatal_primary_blocks_join_and_ends_message_via_finish_failed() {
    let ts = start_server().await;
    let profile = ts.create_profile("text", json!([]), json!([])).await;
    let key_a = ts.create_key(&profile).await;
    let key_b = ts.create_key(&profile).await;
    let (sid, skey_a, _) = ts.open_session(&key_a, json!([])).await;

    // A normal turn works first.
    let (s, _r, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "ok" }))
        .await;
    assert_eq!(s, 200, "baseline turn: {raw}");

    // Simulate a config change that drops the primary: point the profile at a
    // provider name absent from the registry (the "restart with a changed
    // bae-config.toml" scenario). The session stays open until its next turn.
    let (ps, _pv, praw) = ts
        .admin_put(
            &format!("/admin/v1/profiles/{profile}"),
            json!({
                "name": "mutated",
                "primary_provider": "ghost-primary",
                "fallback_providers": [],
                "allowed_tools": [],
            }),
        )
        .await;
    assert_eq!(ps, 200, "profile PUT: {praw}");

    // A second client joining the still-open session is refused with 422
    // primary_provider_unavailable (the fatal-for-the-profile check re-runs on
    // join).
    let (js, jerr, jraw) = ts.join_raw(&sid, &key_b, json!([])).await;
    assert_eq!(js, 422, "join on a now-broken primary: {jraw}");
    assert_eq!(jerr["type"], json!("primary_provider_unavailable"));

    // The already-open session's next sendMessage ends the turn via
    // finish_failed: a session.error(provider_config) naming the missing name,
    // then the session moves to error and refuses further turns.
    let (s, resp, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "after change" }))
        .await;
    assert_eq!(s, 200, "rpc stream opens: {raw}");
    let se = resp["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("session.error"))
        .expect("a session.error");
    assert_eq!(se["payload"]["reason"], json!("provider_config"));
    assert!(se["payload"]["detail"]
        .as_str()
        .unwrap()
        .contains("ghost-primary"));
    let (s, again, raw) = ts
        .send_message(&sid, &skey_a, json!({ "content": "again" }))
        .await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(
        again["error"]["code"],
        json!(-32000),
        "the error-state session refuses new turns: {raw}"
    );

    // And a fresh create on the broken profile is refused with 422 on every
    // attempt (no dedup).
    for attempt in 0..2 {
        let (cs, cerr, craw) = ts
            .client_post("/api/v1/sessions", Some(&key_b), json!({ "tools": [] }))
            .await;
        assert_eq!(cs, 422, "create attempt {attempt}: {craw}");
        assert_eq!(cerr["type"], json!("primary_provider_unavailable"));
    }
}

// ---------------------------------------------------------------------------
// C) non-fatal fallback — missing fallback skipped + logged every session
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_fatal_missing_fallback_is_skipped_and_logged_every_session() {
    let logs = log_capture();
    let ts = start_server().await;
    // A unique missing fallback name so this test's log lines are unambiguous in
    // the shared capture buffer.
    let ghost = format!("ghost-fb-{}", generate_id(""));
    // Primary FAILS (500) so the fallback walk runs; one valid ("text") + one
    // missing (ghost) fallback.
    let profile = ts
        .create_profile("fail", json!(["text", ghost]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;

    for attempt in 0..2 {
        let (sid, skey, _) = ts.open_session(&key, json!([])).await;
        let (s, resp, raw) = ts
            .send_message(&sid, &skey, json!({ "content": "go" }))
            .await;
        assert_eq!(s, 200, "attempt {attempt}: {raw}");
        // The one valid fallback served the turn.
        assert_eq!(
            resp["message"]["content"][0]["text"],
            json!("Hello from mock")
        );
        // Exactly two provider attempts (primary fail + valid fallback) — the
        // missing ghost fallback was skipped, never attempted.
        let attempts = resp["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == json!("provider.request"))
            .count();
        assert_eq!(
            attempts, 2,
            "the missing fallback must be skipped, not attempted"
        );
        let _ = ts
            .client_delete(&format!("/api/v1/sessions/{sid}"), Some(&skey))
            .await;
    }

    // The missing fallback name was logged on BOTH turns (never deduplicated).
    let text = captured_text(&logs);
    let n = text
        .lines()
        .filter(|l| l.contains(&ghost) && l.contains("fallback"))
        .count();
    assert_eq!(
        n, 2,
        "the missing fallback must be logged every session (found {n}):\n{text}"
    );
}

// ---------------------------------------------------------------------------
// C) OpenAI-kind end-to-end tool round trip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn openai_kind_end_to_end_tool_round_trip() {
    let (oai, requests) = start_openai_mock().await;
    let reg = provider_registry_from_toml(&provider_entry_toml(
        "oai-tool",
        "openai",
        &format!("{oai}/oai-tool"),
        "test-token",
    ));
    let ts = boot_server(HashMap::new(), reg, None).await;
    let profile = ts
        .create_profile("oai-tool", json!([]), json!(["get_current_time"]))
        .await;
    let key = ts.create_key(&profile).await;
    let tools = json!([{ "name": "get_current_time", "description": "time",
                        "input_schema": { "type": "object", "properties": {} } }]);
    let (sid, skey, _) = ts.open_session(&key, tools).await;

    // First turn: the OpenAI mock returns a tool_calls response, which the engine
    // consumes as a CANONICAL tool_use — identical shape to an Anthropic run.
    let (s, first, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "what time?" }))
        .await;
    assert_eq!(s, 200, "first turn: {raw}");
    let tu = first["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == json!("tool_use"))
        .expect("canonical tool_use from the OpenAI response");
    assert_eq!(tu["name"], json!("get_current_time"));
    let tuid = tu["id"].as_str().unwrap().to_string();

    // tool.call is canonical and tagged client dispatch (same as Anthropic).
    let tc = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("tool.call"))
        .unwrap();
    assert_eq!(tc["payload"]["name"], json!("get_current_time"));
    assert_eq!(tc["payload"]["dispatch"], json!("client"));

    // provider.response records the RAW OpenAI wire body (choices/tool_calls);
    // provider.request identifies the openai kind. (The raw *outgoing* wire body
    // is verified against the mock's recorder below.)
    let pr = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("provider.response"))
        .unwrap();
    assert!(
        pr["payload"]["body"]["choices"][0]["message"]["tool_calls"].is_array(),
        "raw OpenAI response body logged in provider.response: {pr}"
    );
    let preq = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event_type"] == json!("provider.request"))
        .unwrap();
    assert_eq!(preq["payload"]["provider"], json!("openai"));

    // Second turn: return the tool result → a plain-text completion, canonical.
    let (s, second, raw) = ts
        .send_message(
            &sid,
            &skey,
            json!({ "content": [{ "type": "tool_result", "tool_use_id": tuid, "content": "12:00 UTC" }] }),
        )
        .await;
    assert_eq!(s, 200, "second turn: {raw}");
    assert_eq!(
        second["message"]["content"][0]["text"],
        json!("tool round-trip complete")
    );

    // The mock actually received OpenAI-shaped wire requests: function-calling
    // tools on the first call and a role:"tool" message on the continuation.
    let reqs = requests.lock().unwrap().clone();
    assert_eq!(
        reqs[0]["tools"][0]["type"],
        json!("function"),
        "outgoing tools are OpenAI function-shaped: {:?}",
        reqs[0]
    );
    let last = reqs.last().unwrap();
    assert!(
        last["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == json!("tool")),
        "the tool result was sent as a role:\"tool\" message: {last:?}"
    );
}

// ---------------------------------------------------------------------------
// C) mixed-kind fallback chain (either direction)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_kind_fallback_chain_either_direction() {
    // Case 1: OpenAI-kind primary fails (500) → Anthropic-kind fallback succeeds.
    {
        let (anth, _ra) = start_anthropic_recording_mock(Duration::ZERO).await;
        let (oai, _ro) = start_openai_mock().await;
        let toml = [
            provider_entry_toml(
                "primary-oai-fail",
                "openai",
                &format!("{oai}/oai-fail"),
                "test-token",
            ),
            provider_entry_toml(
                "fb-anth-ok",
                "anthropic",
                &format!("{anth}/text"),
                "test-token",
            ),
        ]
        .concat();
        let reg = provider_registry_from_toml(&toml);
        let ts = boot_server(HashMap::new(), reg, None).await;
        let profile = ts
            .create_profile("primary-oai-fail", json!(["fb-anth-ok"]), json!([]))
            .await;
        let key = ts.create_key(&profile).await;
        let (sid, skey, _) = ts.open_session(&key, json!([])).await;
        let (s, resp, raw) = ts
            .send_message(&sid, &skey, json!({ "content": "hi" }))
            .await;
        assert_eq!(s, 200, "{raw}");
        // The persisted/returned canonical content is the fallback's, regardless
        // of the kind mix.
        assert_eq!(
            resp["message"]["content"][0]["text"],
            json!("Hello from anthropic mock")
        );
        let responses: Vec<&Value> = resp["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == json!("provider.response"))
            .collect();
        assert_eq!(responses.len(), 2, "one response per attempt");
        assert_eq!(responses[0]["payload"]["provider"], json!("openai"));
        assert_eq!(responses[0]["payload"]["ok"], json!(false));
        // The successful attempt's own provider.response carries its raw
        // (untranslated) Anthropic wire body.
        assert_eq!(responses[1]["payload"]["provider"], json!("anthropic"));
        assert_eq!(responses[1]["payload"]["ok"], json!(true));
        assert_eq!(
            responses[1]["payload"]["body"]["content"][0]["text"],
            json!("Hello from anthropic mock")
        );
    }

    // Case 2 (reverse): Anthropic-kind primary fails (500) → OpenAI-kind fallback
    // succeeds.
    {
        let (anth, _ra) = start_anthropic_recording_mock(Duration::ZERO).await;
        let (oai, _ro) = start_openai_mock().await;
        let toml = [
            provider_entry_toml(
                "primary-anth-fail",
                "anthropic",
                &format!("{anth}/fail"),
                "test-token",
            ),
            provider_entry_toml(
                "fb-oai-ok",
                "openai",
                &format!("{oai}/oai-text"),
                "test-token",
            ),
        ]
        .concat();
        let reg = provider_registry_from_toml(&toml);
        let ts = boot_server(HashMap::new(), reg, None).await;
        let profile = ts
            .create_profile("primary-anth-fail", json!(["fb-oai-ok"]), json!([]))
            .await;
        let key = ts.create_key(&profile).await;
        let (sid, skey, _) = ts.open_session(&key, json!([])).await;
        let (s, resp, raw) = ts
            .send_message(&sid, &skey, json!({ "content": "hi" }))
            .await;
        assert_eq!(s, 200, "{raw}");
        assert_eq!(
            resp["message"]["content"][0]["text"],
            json!("Hello from openai mock")
        );
        let responses: Vec<&Value> = resp["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["event_type"] == json!("provider.response"))
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["payload"]["provider"], json!("anthropic"));
        assert_eq!(responses[0]["payload"]["ok"], json!(false));
        assert_eq!(responses[1]["payload"]["provider"], json!("openai"));
        assert_eq!(responses[1]["payload"]["ok"], json!(true));
        // The OpenAI fallback's provider.response carries its raw OpenAI wire body.
        assert!(
            responses[1]["payload"]["body"]["choices"].is_array(),
            "raw OpenAI wire body on the fallback attempt: {}",
            responses[1]
        );
    }
}

// ===========================================================================
// WI 0006 — Sandbox lifecycle, dispatch, and cross-profile scoping
// ===========================================================================
//
// Every test here drives the **real** routers against an **offline** mock
// `SandboxDriver` (below): no `docker`/`container` binary or daemon is ever
// required, exactly as the MCP tests use a trivial stdio fixture rather than a
// real MCP server. `start_server_with_sandbox` returns a live `AppState` handle
// so server-owned state (`AppState.sandboxes`, `AppState.sandbox_status`) can be
// asserted directly, alongside the persisted/broadcast event log.

use baesrv::engine::sandbox::{
    BoxFuture, EnsureOutcome, ExecResult, SandboxDriver, SandboxError, SandboxHandle,
    SandboxImageStatus,
};
use std::collections::HashSet;

/// Scripted behaviour for the mock driver, set once at construction.
#[derive(Default, Clone)]
struct MockConfig {
    /// Images whose `ensure_image` fails with `SandboxError::Image`.
    fail_ensure: HashSet<String>,
    /// Make `start` fail with `SandboxError::Runtime`.
    fail_start: bool,
    /// Make `stop` fail with `SandboxError::Runtime`.
    fail_stop: bool,
    /// Make `exec` fail with `SandboxError::Runtime` (a broken sandbox).
    fail_exec: bool,
    /// Artificial delay applied to every `ensure_image` call — proves the
    /// profile-create provisioning task is truly detached (the HTTP handler
    /// returns before this elapses).
    ensure_delay: Duration,
    /// Scripted `exec` output (defaults to a clean `stdout="sbx-out"`, exit 0).
    exec_stdout: String,
    exec_stderr: String,
    exec_exit: i32,
}

struct MockInner {
    cfg: MockConfig,
    /// Every driver call, recorded as `"ensure:<image>"` / `"start:<image>"` /
    /// `"exec:<id>|<command>"` / `"stop:<id>"`, in order.
    calls: Mutex<Vec<String>>,
    /// Monotonic counter for deterministic container ids.
    next_id: Mutex<u64>,
}

/// An offline `SandboxDriver` that records every call and returns scripted
/// results — the sandbox equivalent of the MCP tests' fixture server. Cloneable
/// over a shared `Arc<MockInner>`, so a test keeps an inspection handle while
/// the `Arc<dyn SandboxDriver>` on `AppState` drives the same recorder.
#[derive(Clone)]
struct MockSandboxDriver {
    inner: Arc<MockInner>,
}

impl MockSandboxDriver {
    fn new() -> Self {
        Self::with_config(MockConfig {
            exec_stdout: "sbx-out".into(),
            ..MockConfig::default()
        })
    }

    fn with_config(cfg: MockConfig) -> Self {
        MockSandboxDriver {
            inner: Arc::new(MockInner {
                cfg,
                calls: Mutex::new(Vec::new()),
                next_id: Mutex::new(0),
            }),
        }
    }

    fn record(&self, s: String) {
        self.inner.calls.lock().unwrap().push(s);
    }

    fn calls(&self) -> Vec<String> {
        self.inner.calls.lock().unwrap().clone()
    }

    /// How many recorded calls start with `prefix` (e.g. `"start:node:22"`).
    fn count(&self, prefix: &str) -> usize {
        self.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

impl SandboxDriver for MockSandboxDriver {
    fn ensure_image<'a>(
        &'a self,
        image: &'a str,
    ) -> BoxFuture<'a, Result<EnsureOutcome, SandboxError>> {
        self.record(format!("ensure:{image}"));
        let cfg = self.inner.cfg.clone();
        let image = image.to_owned();
        Box::pin(async move {
            if !cfg.ensure_delay.is_zero() {
                tokio::time::sleep(cfg.ensure_delay).await;
            }
            if cfg.fail_ensure.contains(&image) {
                Err(SandboxError::Image {
                    image: image.clone(),
                    detail: "mock: image pull failed".into(),
                })
            } else {
                Ok(EnsureOutcome::Pulled)
            }
        })
    }

    fn start<'a>(&'a self, image: &'a str) -> BoxFuture<'a, Result<SandboxHandle, SandboxError>> {
        self.record(format!("start:{image}"));
        let fail = self.inner.cfg.fail_start;
        let image = image.to_owned();
        let id = {
            let mut n = self.inner.next_id.lock().unwrap();
            *n += 1;
            format!("mock-container-{n}")
        };
        Box::pin(async move {
            if fail {
                Err(SandboxError::Runtime {
                    detail: "mock: start failed".into(),
                })
            } else {
                Ok(SandboxHandle { id, image })
            }
        })
    }

    fn exec<'a>(
        &'a self,
        handle: &'a SandboxHandle,
        command: &'a str,
    ) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        self.record(format!("exec:{}|{command}", handle.id));
        let cfg = self.inner.cfg.clone();
        Box::pin(async move {
            if cfg.fail_exec {
                Err(SandboxError::Runtime {
                    detail: "mock: exec failed".into(),
                })
            } else {
                Ok(ExecResult {
                    stdout: cfg.exec_stdout,
                    stderr: cfg.exec_stderr,
                    exit_code: cfg.exec_exit,
                })
            }
        })
    }

    fn stop<'a>(&'a self, handle: &'a SandboxHandle) -> BoxFuture<'a, Result<(), SandboxError>> {
        self.record(format!("stop:{}", handle.id));
        let fail = self.inner.cfg.fail_stop;
        Box::pin(async move {
            if fail {
                Err(SandboxError::Runtime {
                    detail: "mock: stop failed".into(),
                })
            } else {
                Ok(())
            }
        })
    }
}

// --- Sandbox-specific TestServer helpers ------------------------------------

impl TestServer {
    /// Create a profile with an `available_sandboxes` image allowlist (and an
    /// optional client-tool allowlist), returning its id.
    async fn create_profile_with_sandboxes(
        &self,
        primary: &str,
        available_sandboxes: Value,
        allowed_tools: Value,
    ) -> String {
        let body = json!({
            "name": format!("profile-{}", generate_id("")),
            "primary_provider": primary,
            "fallback_providers": [],
            "allowed_tools": allowed_tools,
            "available_sandboxes": available_sandboxes,
        });
        let (status, v, raw) = self.admin_post("/admin/v1/profiles", body).await;
        assert_eq!(status, 201, "create profile (sandbox) failed: {raw}");
        v["id"].as_str().unwrap().to_string()
    }

    /// Open a session declaring `sandbox_tools` (Auto-mode) alongside ordinary
    /// `tools`, then register as a driver. Returns `(session_id, session_key)`.
    async fn open_session_with_sandbox_tools(
        &self,
        client_key: &str,
        tools: Value,
        sandbox_tools: Value,
    ) -> (String, String) {
        let (status, v, raw) = self
            .client_post(
                "/api/v1/sessions",
                Some(client_key),
                json!({ "client_version": "1.0.0", "tools": tools, "sandbox_tools": sandbox_tools }),
            )
            .await;
        assert_eq!(status, 201, "open session (sandbox_tools) failed: {raw}");
        let sid = v["session_id"].as_str().unwrap().to_string();
        let skey = v["session_key"].as_str().unwrap().to_string();
        self.register_driver(&sid, &skey).await;
        (sid, skey)
    }

    /// The full persisted event list (each item includes `payload`), oldest
    /// first, via `GET /api/v1/sessions/{id}/events`.
    async fn session_events(&self, session_id: &str, session_key: &str) -> Vec<Value> {
        let (_s, events, _r) = self
            .client_get(
                &format!("/api/v1/sessions/{session_id}/events"),
                Some(session_key),
            )
            .await;
        events["items"].as_array().cloned().unwrap_or_default()
    }

    /// Drive one terminating JSON-RPC method and return its terminal frame (the
    /// object carrying `result` or `error`).
    async fn rpc_terminal(
        &self,
        session_id: &str,
        session_key: &str,
        method: &str,
        params: Value,
    ) -> Value {
        let (status, frames, raw) = self
            .rpc(
                session_id,
                session_key,
                json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }),
            )
            .await;
        assert_eq!(status, 200, "{method} http status: {raw}");
        terminal_frames(&frames)
            .last()
            .map(|f| (*f).clone())
            .unwrap_or_else(|| panic!("{method} produced no terminal frame: {raw}"))
    }
}

/// The ordered `event_type` strings of a persisted event list.
fn ev_types(evs: &[Value]) -> Vec<String> {
    evs.iter()
        .map(|e| e["event_type"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// The first event of type `t`, or a panic naming what was present.
fn find_ev<'a>(evs: &'a [Value], t: &str) -> &'a Value {
    evs.iter()
        .find(|e| e["event_type"] == json!(t))
        .unwrap_or_else(|| panic!("missing {t} event; present: {:?}", ev_types(evs)))
}

/// Assert `first` occurs strictly before `second` in an event-type sequence
/// (both must be present).
fn assert_ordered(evs: &[Value], first: &str, second: &str) {
    let types = ev_types(evs);
    let i = types
        .iter()
        .position(|t| t == first)
        .unwrap_or_else(|| panic!("missing {first}: {types:?}"));
    let j = types
        .iter()
        .position(|t| t == second)
        .unwrap_or_else(|| panic!("missing {second}: {types:?}"));
    assert!(i < j, "expected {first} before {second}: {types:?}");
}

/// Poll `AppState.sandbox_image_status` until `image` on `profile_id` leaves
/// `Pending` (background provisioning finished), or give up after ~5s.
async fn await_provisioned(state: &AppState, profile_id: &str, image: &str) -> SandboxImageStatus {
    for _ in 0..250 {
        let s = state.sandbox_image_status(profile_id, image);
        if s != SandboxImageStatus::Pending {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("image {image} on {profile_id} never left Pending");
}

// --- A) single-profile allowlist enforcement (-32011) -----------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_image_allowlist_rejects_unlisted_image_without_starting() {
    let driver = MockSandboxDriver::new();
    let (ts, _state) = start_server_with_sandbox(Arc::new(driver.clone())).await;

    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;

    // An image not in the profile's allowlist is rejected *before* any container
    // is started — the admin-declared list is the sole trust boundary.
    let t = ts
        .rpc_terminal(
            &sid,
            &skey,
            "session.startRemoteSandbox",
            json!({ "image": "node:22" }),
        )
        .await;
    assert_eq!(t["error"]["code"], json!(-32011), "not-allowed: {t}");
    assert_eq!(
        driver.count("start:"),
        0,
        "the driver's start must never be called for a disallowed image"
    );

    // The allowed image starts normally.
    let t = ts
        .rpc_terminal(
            &sid,
            &skey,
            "session.startRemoteSandbox",
            json!({ "image": "python:3.12" }),
        )
        .await;
    assert!(t["result"]["sandbox_id"].is_string(), "started: {t}");
    assert_eq!(t["result"]["image"], json!("python:3.12"));
    assert_eq!(driver.count("start:python:3.12"), 1);
}

// --- B) cross-profile scoping (the key regression) --------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_scoping_is_per_profile_and_bidirectional() {
    let driver = MockSandboxDriver::new();
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;

    // Two profiles with DISJOINT image sets, both provisioned against the same
    // mock driver, so `sandbox_status` genuinely holds entries for both.
    let prof_a = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let prof_b = ts
        .create_profile_with_sandboxes("text", json!(["node:22"]), json!([]))
        .await;
    let key_a = ts.create_key(&prof_a).await;
    let key_b = ts.create_key(&prof_b).await;

    // Wait until BOTH images are provisioned so the notification reports a real
    // "available" status (not a transient "pending") — proving node:22 is a
    // known, successfully-provisioned image server-wide, yet still absent from
    // A's notification.
    assert_eq!(
        await_provisioned(&state, &prof_a, "python:3.12").await,
        SandboxImageStatus::Available
    );
    assert_eq!(
        await_provisioned(&state, &prof_b, "node:22").await,
        SandboxImageStatus::Available
    );

    // Assert both directions with one closure: session on `own`, with `own_img`
    // in its profile and `other_img` in the *other* profile's list only.
    async fn assert_scoped(
        ts: &TestServer,
        driver: &MockSandboxDriver,
        client_key: &str,
        own_img: &str,
        other_img: &str,
    ) {
        // Open + register (the notification fires on registerDriver).
        let (sid, skey, _) = ts.open_session(client_key, json!([])).await;

        // (1) The driver-connect notification lists ONLY this profile's image.
        let evs = ts.session_events(&sid, &skey).await;
        let avail = find_ev(&evs, "session.sandbox.available");
        let names: Vec<String> = avail["payload"]["images"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec![own_img.to_string()],
            "notification must be scoped to this profile's own image only"
        );
        assert!(
            !names.iter().any(|n| n == other_img),
            "the other profile's image {other_img} must never leak into {names:?}"
        );
        assert_eq!(
            avail["payload"]["images"][0]["status"],
            json!("available"),
            "the scoped image's real provisioning status is reported"
        );

        // (2) startRemoteSandbox with the OTHER profile's image → -32011,
        // and the driver's start is never called for it.
        let before = driver.count(&format!("start:{other_img}"));
        let t = ts
            .rpc_terminal(
                &sid,
                &skey,
                "session.startRemoteSandbox",
                json!({ "image": other_img }),
            )
            .await;
        assert_eq!(
            t["error"]["code"],
            json!(-32011),
            "cross-profile image must be rejected: {t}"
        );
        assert_eq!(
            driver.count(&format!("start:{other_img}")),
            before,
            "start must never be called for a cross-profile image"
        );

        // The own image still starts.
        let t = ts
            .rpc_terminal(
                &sid,
                &skey,
                "session.startRemoteSandbox",
                json!({ "image": own_img }),
            )
            .await;
        assert!(
            t["result"]["sandbox_id"].is_string(),
            "own image starts: {t}"
        );
    }

    // Direction 1: session on A may launch python:3.12, never node:22.
    assert_scoped(&ts, &driver, &key_a, "python:3.12", "node:22").await;
    // Direction 2 (symmetric): session on B may launch node:22, never python:3.12.
    assert_scoped(&ts, &driver, &key_b, "node:22", "python:3.12").await;
}

// --- C) background provisioning is detached; status reflects each outcome ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_provisioning_is_backgrounded_and_records_per_image_status() {
    let logs = log_capture();
    // Unique image names so this test's log lines are unambiguous in the shared
    // capture buffer.
    let good = format!("good-{}", generate_id(""));
    let bad = format!("bad-{}", generate_id(""));

    let mut fail = HashSet::new();
    fail.insert(bad.clone());
    let driver = MockSandboxDriver::with_config(MockConfig {
        fail_ensure: fail,
        // A pull slow enough that awaiting it would blow past the timing
        // assertion below — so the handler returning fast proves it is detached.
        ensure_delay: Duration::from_millis(400),
        exec_stdout: "sbx-out".into(),
        ..MockConfig::default()
    });
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;

    // The profile-create HTTP response must return BEFORE the artificially
    // delayed pull completes — provisioning is spawned, never awaited.
    let started = tokio::time::Instant::now();
    let profile = ts
        .create_profile_with_sandboxes("text", json!([good, bad]), json!([]))
        .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(300),
        "profile create must return before the delayed pull (took {elapsed:?})"
    );

    // Each image resolves to its own terminal status.
    assert_eq!(
        await_provisioned(&state, &profile, &good).await,
        SandboxImageStatus::Available,
        "the good image ends Available"
    );
    match await_provisioned(&state, &profile, &bad).await {
        SandboxImageStatus::Error(d) => assert!(d.contains("pull failed"), "detail: {d}"),
        other => panic!("the bad image must end Error, got {other:?}"),
    }

    // Both images were actually ensured against the driver (sequentially).
    assert_eq!(driver.count(&format!("ensure:{good}")), 1);
    assert_eq!(driver.count(&format!("ensure:{bad}")), 1);

    // The failure is logged at error level (the success path logs at info,
    // which the shared error-level capture does not retain — the Available
    // status above is the success-path assertion).
    let text = captured_text(&logs);
    assert!(
        text.lines()
            .any(|l| l.contains(&bad) && l.contains("failed to ensure sandbox image")),
        "the failed pull must be logged:\n{text}"
    );
}

// --- D) driver-connect notification: present, scoped, absent when empty -----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_available_notification_present_with_status_and_absent_when_empty() {
    let driver = MockSandboxDriver::new();
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;

    // A profile that declares an image → a scoped notification with real status.
    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    assert_eq!(
        await_provisioned(&state, &profile, "python:3.12").await,
        SandboxImageStatus::Available
    );
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;
    let evs = ts.session_events(&sid, &skey).await;
    // The notification fires immediately after the driver-registration event.
    assert_ordered(&evs, "session.driver.register", "session.sandbox.available");
    let avail = find_ev(&evs, "session.sandbox.available");
    assert_eq!(
        avail["payload"]["images"],
        json!([{ "name": "python:3.12", "status": "available" }]),
        "per-image status reported: {avail}"
    );

    // A profile with an EMPTY available_sandboxes emits no such event.
    let empty_profile = ts
        .create_profile_with_sandboxes("text", json!([]), json!([]))
        .await;
    let empty_key = ts.create_key(&empty_profile).await;
    let (esid, eskey, _) = ts.open_session(&empty_key, json!([])).await;
    let eevs = ts.session_events(&esid, &eskey).await;
    assert!(
        !ev_types(&eevs).contains(&"session.sandbox.available".to_string()),
        "no sandbox notification for an empty allowlist: {:?}",
        ev_types(&eevs)
    );
    // ...but the ordinary driver-registration event is still present.
    assert!(ev_types(&eevs).contains(&"session.driver.register".to_string()));
}

// --- E) remote sandbox full lifecycle, success path -------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_sandbox_start_then_explicit_stop_lifecycle() {
    let driver = MockSandboxDriver::new();
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;

    // Start → ordered session.sandbox.start then session.sandbox.running.
    let t = ts
        .rpc_terminal(
            &sid,
            &skey,
            "session.startRemoteSandbox",
            json!({ "image": "python:3.12" }),
        )
        .await;
    let sandbox_id = t["result"]["sandbox_id"].as_str().unwrap().to_string();
    assert!(
        t["result"]["started_at"].is_string(),
        "started_at present: {t}"
    );
    assert!(state.sandbox(&sid).is_some(), "handle retained on AppState");

    let evs = ts.session_events(&sid, &skey).await;
    assert_ordered(&evs, "session.sandbox.start", "session.sandbox.running");
    let start = find_ev(&evs, "session.sandbox.start");
    assert_eq!(start["payload"]["dispatch"], json!("remote"));
    let running = find_ev(&evs, "session.sandbox.running");
    assert_eq!(running["payload"]["dispatch"], json!("remote"));
    assert_eq!(running["payload"]["sandbox_id"], json!(sandbox_id));

    // Stop → ordered session.sandbox.stop then session.sandbox.stopped, and the
    // AppState entry is gone.
    let t = ts
        .rpc_terminal(&sid, &skey, "session.stopRemoteSandbox", json!({}))
        .await;
    assert_eq!(t["result"]["stopped"], json!(true), "stopped: {t}");
    assert!(state.sandbox(&sid).is_none(), "handle removed after stop");
    assert_eq!(driver.count(&format!("stop:{sandbox_id}")), 1);

    let evs = ts.session_events(&sid, &skey).await;
    assert_ordered(&evs, "session.sandbox.stop", "session.sandbox.stopped");
    let stop = find_ev(&evs, "session.sandbox.stop");
    assert_eq!(stop["payload"]["reason"], json!("explicit"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_close_implicitly_stops_running_remote_sandbox() {
    let driver = MockSandboxDriver::new();
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;

    let t = ts
        .rpc_terminal(
            &sid,
            &skey,
            "session.startRemoteSandbox",
            json!({ "image": "python:3.12" }),
        )
        .await;
    let sandbox_id = t["result"]["sandbox_id"].as_str().unwrap().to_string();

    // Close with no explicit stop → the same stop/stopped pair fires with
    // reason "session_close", the driver's stop is called, and the entry clears.
    let (status, _v, raw) = ts
        .client_delete(&format!("/api/v1/sessions/{sid}"), Some(&skey))
        .await;
    assert_eq!(status, 200, "close: {raw}");
    assert_eq!(
        driver.count(&format!("stop:{sandbox_id}")),
        1,
        "close stopped the sandbox"
    );
    assert!(
        state.sandbox(&sid).is_none(),
        "no lingering handle after close"
    );

    // The stop payload the *broadcaster* saw carried reason "session_close".
    // (The session is closed now; assert via the mock's recorded stop above and
    // the reason on the persisted event, read before close removed the channel.)
    // Events are still persisted and readable after close via the session key.
    let evs = ts.session_events(&sid, &skey).await;
    let stop = find_ev(&evs, "session.sandbox.stop");
    assert_eq!(stop["payload"]["reason"], json!("session_close"));
    assert_ordered(&evs, "session.sandbox.stop", "session.sandbox.stopped");
}

// --- F) remote sandbox lifecycle, failure paths -----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_sandbox_start_failure_retains_no_handle() {
    let driver = MockSandboxDriver::with_config(MockConfig {
        fail_start: true,
        exec_stdout: "sbx-out".into(),
        ..MockConfig::default()
    });
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;

    let t = ts
        .rpc_terminal(
            &sid,
            &skey,
            "session.startRemoteSandbox",
            json!({ "image": "python:3.12" }),
        )
        .await;
    assert_eq!(t["error"]["code"], json!(-32012), "start_failed: {t}");
    assert!(
        state.sandbox(&sid).is_none(),
        "no handle retained on failure"
    );

    let evs = ts.session_events(&sid, &skey).await;
    assert_ordered(&evs, "session.sandbox.start", "session.sandbox.error");
    let err = find_ev(&evs, "session.sandbox.error");
    assert_eq!(err["payload"]["phase"], json!("start"));
    assert_eq!(err["payload"]["dispatch"], json!("remote"));
    // A failed start never logs a running event.
    assert!(
        !ev_types(&evs).contains(&"session.sandbox.running".to_string()),
        "no running event when start failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_sandbox_stop_failure_still_removes_the_handle() {
    let driver = MockSandboxDriver::with_config(MockConfig {
        fail_stop: true,
        exec_stdout: "sbx-out".into(),
        ..MockConfig::default()
    });
    let (ts, state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;

    ts.rpc_terminal(
        &sid,
        &skey,
        "session.startRemoteSandbox",
        json!({ "image": "python:3.12" }),
    )
    .await;
    assert!(state.sandbox(&sid).is_some());

    // A failed stop surfaces an error but MUST still remove the handle — no
    // phantom handle other calls could dispatch against.
    let t = ts
        .rpc_terminal(&sid, &skey, "session.stopRemoteSandbox", json!({}))
        .await;
    assert!(
        t.get("error").is_some(),
        "stop failure surfaces an error: {t}"
    );
    assert!(
        state.sandbox(&sid).is_none(),
        "the handle is removed even when the driver stop failed"
    );

    let evs = ts.session_events(&sid, &skey).await;
    assert_ordered(&evs, "session.sandbox.stop", "session.sandbox.error");
    let err = find_ev(&evs, "session.sandbox.error");
    assert_eq!(err["payload"]["phase"], json!("stop"));

    // A second stop now reports nothing-running rather than dispatching again.
    let t = ts
        .rpc_terminal(&sid, &skey, "session.stopRemoteSandbox", json!({}))
        .await;
    assert_eq!(t["error"]["code"], json!(-32013), "no phantom handle: {t}");
}

// --- G) session.reportLocalSandbox ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn report_local_sandbox_maps_state_skips_allowlist_and_gates_on_driver() {
    let driver = MockSandboxDriver::new();
    let (ts, _state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    // The profile declares only python:3.12; the local report uses a totally
    // unrelated image name to prove no allowlist check happens on this path.
    let profile = ts
        .create_profile_with_sandboxes("text", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;

    // Unregistered driver → same -32001 gate as sendMessage.
    let (usid, uskey, _) = ts.open_session_no_register(&key, json!([])).await;
    let t = ts
        .rpc_terminal(
            &usid,
            &uskey,
            "session.reportLocalSandbox",
            json!({ "state": "running", "image": "anything:latest" }),
        )
        .await;
    assert_eq!(
        t["error"]["code"],
        json!(-32001),
        "driver-registration gate: {t}"
    );

    // Registered: each state maps to the matching lifecycle event, all with
    // dispatch:"local", and an arbitrary (unlisted) image is accepted as-is.
    let (sid, skey, _) = ts.open_session(&key, json!([])).await;
    for (state_str, event_type) in [
        ("running", "session.sandbox.running"),
        ("stopped", "session.sandbox.stopped"),
        ("error", "session.sandbox.error"),
    ] {
        let t = ts
            .rpc_terminal(
                &sid,
                &skey,
                "session.reportLocalSandbox",
                json!({
                    "state": state_str,
                    "image": "totally-unlisted:latest",
                    "container_id": "local-abc",
                    "detail": "from the harness",
                }),
            )
            .await;
        assert_eq!(t["result"]["reported"], json!(true), "{state_str}: {t}");
        let evs = ts.session_events(&sid, &skey).await;
        let ev = find_ev(&evs, event_type);
        assert_eq!(ev["payload"]["dispatch"], json!("local"), "{state_str}");
        assert_eq!(ev["payload"]["image"], json!("totally-unlisted:latest"));
        assert_eq!(ev["payload"]["container_id"], json!("local-abc"));
    }

    // No sandbox was ever started server-side; the local report never touches
    // the driver.
    assert_eq!(driver.count("start:"), 0);
    assert_eq!(driver.count("stop:"), 0);
}

// --- H) manual remote dispatch ----------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_remote_dispatch_pauses_then_exec_then_client_continuation() {
    let driver = MockSandboxDriver::with_config(MockConfig {
        exec_stdout: "manual-out".into(),
        exec_stderr: "manual-err".into(),
        exec_exit: 7,
        ..MockConfig::default()
    });
    let (ts, _state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    // Primary "tool" emits a client tool_use (get_current_time); the profile
    // allows that client tool and the python:3.12 sandbox image.
    let profile = ts
        .create_profile_with_sandboxes("tool", json!(["python:3.12"]), json!(["get_current_time"]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey, _) = ts
        .open_session(
            &key,
            json!([{ "name": "get_current_time", "input_schema": { "type": "object" } }]),
        )
        .await;

    // Start the remote sandbox the manual handler will exec against.
    ts.rpc_terminal(
        &sid,
        &skey,
        "session.startRemoteSandbox",
        json!({ "image": "python:3.12" }),
    )
    .await;

    // The turn pauses on the client tool_use (unchanged run_turn behaviour):
    // the assistant message handed back carries a tool_use block.
    let (status, resp, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "what time is it" }))
        .await;
    assert_eq!(status, 200, "{raw}");
    let has_tool_use = resp["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["type"] == json!("tool_use") && b["name"] == json!("get_current_time"));
    assert!(
        has_tool_use,
        "paused turn returns the tool_use to the client: {raw}"
    );

    // The harness fetches raw output via the separate, non-turn RPC utility.
    let t = ts
        .rpc_terminal(
            &sid,
            &skey,
            "session.execRemoteSandbox",
            json!({ "command": "date +%s" }),
        )
        .await;
    assert_eq!(t["result"]["stdout"], json!("manual-out"));
    assert_eq!(t["result"]["stderr"], json!("manual-err"));
    assert_eq!(
        t["result"]["exit_code"],
        json!(7),
        "raw exit passed through: {t}"
    );
    assert_eq!(
        driver.count("exec:"),
        1,
        "manual exec dispatched to the driver"
    );

    // The harness constructs its OWN tool_result and continues the turn via the
    // ordinary sendMessage continuation → the turn completes.
    let (status, resp, raw) = ts
        .send_message(
            &sid,
            &skey,
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_1",
                    "content": [{ "type": "text", "text": "manual-out" }],
                }],
            }),
        )
        .await;
    assert_eq!(status, 200, "{raw}");
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("tool round-trip complete"),
        "client-constructed tool_result drove the turn to completion: {raw}"
    );
}

// --- I) auto remote dispatch (structurally mirrors the MCP round trip) ------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_remote_dispatch_runs_server_side_without_pausing() {
    let driver = MockSandboxDriver::with_config(MockConfig {
        exec_stdout: "auto-out".into(),
        ..MockConfig::default()
    });
    let (ts, _state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    // Primary "sandbox" emits a tool_use for `run_shell_command` carrying a
    // fully-formed command; the client declares it as an Auto-mode sandbox tool.
    let profile = ts
        .create_profile_with_sandboxes("sandbox", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey) = ts
        .open_session_with_sandbox_tools(
            &key,
            json!([]),
            json!([{
                "name": "run_shell_command",
                "input_schema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                },
            }]),
        )
        .await;

    ts.rpc_terminal(
        &sid,
        &skey,
        "session.startRemoteSandbox",
        json!({ "image": "python:3.12" }),
    )
    .await;

    // One sendMessage → the whole turn completes server-side (never pauses,
    // never reaches the client) with the provider's terminal text.
    let (status, resp, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "run it" }))
        .await;
    assert_eq!(status, 200, "{raw}");
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("after sandbox exec"),
        "terminal Completed in one turn: {raw}"
    );

    let evs = resp["events"].as_array().unwrap();

    // tool.call tagged as a sandbox dispatch (mirrors the MCP tool.call shape,
    // minus server_name).
    let call = find_ev(evs, "tool.call");
    assert_eq!(call["payload"]["dispatch"], json!("sandbox"));
    assert_eq!(call["payload"]["name"], json!("run_shell_command"));

    // sandbox.request / sandbox.response bracket the driver exec — the sandbox
    // twins of mcp.request / mcp.response.
    let req = find_ev(evs, "sandbox.request");
    assert_eq!(req["payload"]["tool"], json!("run_shell_command"));
    assert_eq!(req["payload"]["command"], json!("echo hi"));
    let res = find_ev(evs, "sandbox.response");
    assert_eq!(res["payload"]["ok"], json!(true));
    assert_eq!(res["payload"]["result"]["stdout"], json!("auto-out"));

    // tool.result — sandbox dispatch, not an error, carrying the exec output.
    let tr = find_ev(evs, "tool.result");
    assert_eq!(tr["payload"]["dispatch"], json!("sandbox"));
    assert_eq!(tr["payload"]["is_error"], json!(false));
    assert_eq!(tr["payload"]["content"][0]["text"], json!("auto-out"));

    // Ordering mirrors the MCP round trip exactly (call → request → response →
    // result).
    assert_ordered(evs, "tool.call", "sandbox.request");
    assert_ordered(evs, "sandbox.request", "sandbox.response");
    assert_ordered(evs, "sandbox.response", "tool.result");

    // The command really reached the driver, and no lifecycle error fired.
    assert_eq!(driver.count("exec:"), 1);
    assert!(
        !ev_types(evs).contains(&"session.sandbox.error".to_string()),
        "a successful auto exec logs no session.sandbox.error"
    );
}

// --- J) auto tool call with no started sandbox: reuse the no-server shape ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_dispatch_without_started_sandbox_reuses_error_tool_result_shape() {
    let driver = MockSandboxDriver::new();
    let (ts, _state) = start_server_with_sandbox(Arc::new(driver.clone())).await;
    let profile = ts
        .create_profile_with_sandboxes("sandbox", json!(["python:3.12"]), json!([]))
        .await;
    let key = ts.create_key(&profile).await;
    let (sid, skey) = ts
        .open_session_with_sandbox_tools(
            &key,
            json!([]),
            json!([{
                "name": "run_shell_command",
                "input_schema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                },
            }]),
        )
        .await;

    // No startRemoteSandbox: the auto tool_use is handled exactly like the "no
    // MCP server configured" case — an error-shaped tool.result, turn continues.
    let (status, resp, raw) = ts
        .send_message(&sid, &skey, json!({ "content": "run it" }))
        .await;
    assert_eq!(status, 200, "turn completes despite no sandbox: {raw}");
    assert_eq!(
        resp["message"]["content"][0]["text"],
        json!("after sandbox exec"),
        "the turn still reaches its terminal completion"
    );

    let evs = resp["events"].as_array().unwrap();
    let res = find_ev(evs, "sandbox.response");
    assert_eq!(res["payload"]["ok"], json!(false));
    assert_eq!(res["payload"]["sandbox_id"], Value::Null);

    let tr = find_ev(evs, "tool.result");
    assert_eq!(tr["payload"]["dispatch"], json!("sandbox"));
    assert_eq!(
        tr["payload"]["is_error"],
        json!(true),
        "error-shaped result"
    );
    let text = tr["payload"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("no remote sandbox is running")
            && text.contains("call session.startRemoteSandbox first"),
        "reuses the no-server error posture verbatim: {text}"
    );

    // The driver was never touched, and no phantom lifecycle error fired (no
    // driver call actually failed).
    assert_eq!(driver.count("exec:"), 0);
    assert!(!ev_types(evs).contains(&"session.sandbox.error".to_string()));
}
