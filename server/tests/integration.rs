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

/// Boot the real routers on an ephemeral client/admin port pair with a fresh
/// temp-file database, exactly as `baesrv::serve` would (minus signal handling).
async fn start_server() -> TestServer {
    start_server_with_registry(std::collections::HashMap::new()).await
}

/// Like [`start_server`], but with a preloaded MCP server registry (as
/// `bae-config.toml` would supply at startup). Used by the MCP integration
/// tests, which build the registry from a fixture `bae-config.toml` through the
/// real loader.
async fn start_server_with_registry(
    registry: std::collections::HashMap<String, baesrv::config_file::McpServerConfig>,
) -> TestServer {
    let dir = std::env::temp_dir().join(format!("baesrv-it-{}", generate_id("")));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.db");

    // A real file-backed store exercises the migration runner on a fresh DB.
    let store = Store::open(&db_path).expect("open temp store");
    let state = AppState::with_mcp_registry(store, registry);

    let client_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_addr = client_listener.local_addr().unwrap();
    let admin_addr = admin_listener.local_addr().unwrap();

    let client_app = baesrv::api::client::router(state.clone());
    // These integration tests predate admin-port auth and drive the admin API
    // without a bearer key; build the admin router with auth disabled so they
    // exercise the endpoints directly. Admin-auth enforcement has its own tests.
    let admin_app = baesrv::api::admin::router(state.clone(), false);
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
    ts
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

    /// Create a profile bound to `base_url`, returning its id.
    async fn create_profile(
        &self,
        base_url: &str,
        fallbacks: Value,
        allowed_tools: Value,
    ) -> String {
        self.create_profile_with_token(base_url, "test-token", fallbacks, allowed_tools)
            .await
    }

    async fn create_profile_with_token(
        &self,
        base_url: &str,
        auth_token: &str,
        fallbacks: Value,
        allowed_tools: Value,
    ) -> String {
        let body = json!({
            "name": format!("profile-{}", generate_id("")),
            "provider_config": {
                "provider": "anthropic",
                "base_url": base_url,
                "model": "claude-mock-1",
                "auth_token": auth_token,
            },
            "fallback_configs": fallbacks,
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
    async fn open_session(&self, client_key: &str, tools: Value) -> (String, String, Value) {
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    // --- Create + read a profile ---
    let profile_id = ts
        .create_profile(&base, json!([]), json!(["get_current_time"]))
        .await;

    let (status, prof, raw) = ts
        .admin_get(&format!("/admin/v1/profiles/{profile_id}"))
        .await;
    assert_eq!(status, 200, "get profile: {raw}");
    assert_eq!(prof["id"], json!(profile_id));
    assert_eq!(prof["allowed_tools"], json!(["get_current_time"]));
    // The admin surface *does* return the literal auth_token template.
    assert_eq!(prof["provider_config"]["auth_token"], json!("test-token"));

    let (status, list, raw) = ts.admin_get("/admin/v1/profiles").await;
    assert_eq!(status, 200);
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "list: {raw}");

    // --- Replace (PUT) bumps updated_at and swaps fields ---
    let put_body = json!({
        "name": prof["name"],
        "provider_config": prof["provider_config"],
        "fallback_configs": [],
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    for _ in 0..3 {
        ts.create_profile(&base, json!([]), json!([])).await;
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
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
    let mock = start_mock().await;

    // Primary is broken (/fail → 500); the single fallback works (/text).
    let primary = format!("{mock}/fail");
    let fallback = format!("{mock}/text");
    let fallbacks = json!([{
        "provider": "anthropic", "base_url": fallback,
        "model": "claude-mock-1", "auth_token": "test-token",
    }]);
    let profile_id = ts.create_profile(&primary, fallbacks, json!([])).await;
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
    let mock = start_mock().await;
    let primary = format!("{mock}/fail");

    let profile_id = ts.create_profile(&primary, json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&session_id, &session_key, json!({ "content": "hello" }))
        .await;
    // On /rpc the HTTP stream is 200; the provider failure rides in the terminal
    // response's result (message + events), session moved to error state.
    assert_eq!(status, 200, "rpc stream opens: {raw}");
    assert!(resp["message"].is_object(), "terminal result carries a message: {raw}");
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
async fn provider_missing_env_var_fails_with_traced_response() {
    let ts = start_server().await;
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    // The auth_token references an environment variable guaranteed to be unset,
    // so token resolution fails *before* any HTTP request and is recorded as a
    // provider.response failure (secret never reaches the wire).
    let profile_id = ts
        .create_profile_with_token(
            &base,
            "${BAE_TEST_DEFINITELY_UNSET_XYZ}",
            json!([]),
            json!([]),
        )
        .await;
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
    let mock = start_mock().await;
    let base = format!("{mock}/tool");

    let profile_id = ts
        .create_profile(&base, json!([]), json!(["get_current_time"]))
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
        .send_message(&session_id, &session_key, json!({ "content": "what time is it?" }))
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
    let mock = start_mock().await;
    let base = format!("{mock}/mcp");

    // The client declares NO tools, so the provider's tool_use for `remote_search`
    // is dispatched server-side as an MCP stub and resolves within one call.
    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&session_id, &session_key, json!({ "content": "search please" }))
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    // Profile allows exactly one tool.
    let allowed_profile = ts
        .create_profile(&base, json!([]), json!(["get_current_time"]))
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
    let empty_profile = ts.create_profile(&base, json!([]), json!([])).await;
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
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
    /// Create a profile bound to `base_url` with an explicit `mcp_servers` list.
    async fn create_profile_with_mcp(
        &self,
        base_url: &str,
        mcp_servers: Value,
        allowed_tools: Value,
    ) -> String {
        let body = json!({
            "name": format!("profile-{}", generate_id("")),
            "provider_config": {
                "provider": "anthropic",
                "base_url": base_url,
                "model": "claude-mock-1",
                "auth_token": "test-token",
            },
            "fallback_configs": [],
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
    async fn rpc_raw(&self, session_id: &str, token: &str, body: String) -> (u16, Vec<Value>, String) {
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
        .map(|f| f["params"]["event_type"].as_str().unwrap_or("?").to_string())
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
    frames
        .iter()
        .find_map(|f| f.get("error").and_then(|e| e.get("code")).and_then(Value::as_i64))
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");
    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
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
        .rpc(&sid, &key, json!({ "id": 1, "method": "session.unsubscribe" }))
        .await;
    assert_eq!(rpc_error_code(&frames), Some(-32600), "missing jsonrpc: {raw}");

    // Missing `method` → -32600.
    let (_s, frames, raw) = ts.rpc(&sid, &key, json!({ "jsonrpc": "2.0", "id": 1 })).await;
    assert_eq!(rpc_error_code(&frames), Some(-32600), "missing method: {raw}");

    // Unknown `method` → -32601, and the response echoes the request id.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 42, "method": "no.such.method", "params": {} }),
        )
        .await;
    assert_eq!(rpc_error_code(&frames), Some(-32601), "unknown method: {raw}");
    let terminals = terminal_frames(&frames);
    assert_eq!(terminals.len(), 1, "exactly one response for an id'd request: {raw}");
    assert_eq!(terminals[0]["id"], json!(42), "response echoes the request id: {raw}");

    // Known method, missing required `params.message` → -32602.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "session.sendMessage", "params": {} }),
        )
        .await;
    assert_eq!(rpc_error_code(&frames), Some(-32602), "missing message param: {raw}");

    // Known method, `params.message` present but the wrong shape → -32602.
    let (_s, frames, raw) = ts
        .rpc(
            &sid,
            &key,
            json!({ "jsonrpc": "2.0", "id": 6, "method": "session.sendMessage",
                    "params": { "message": 5 } }),
        )
        .await;
    assert_eq!(rpc_error_code(&frames), Some(-32602), "invalid message param: {raw}");

    // NOTIFICATION SEMANTICS: an object with no `id` is a JSON-RPC notification.
    // The server performs the method's side effect (here, cancelling any active
    // subscriptions) but MUST NOT send a response — no result, no error frame.
    let (status, frames, raw) = ts
        .rpc(&sid, &key, json!({ "jsonrpc": "2.0", "method": "session.unsubscribe" }))
        .await;
    assert_eq!(status, 200, "notification still opens the 200 stream: {raw}");
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");
    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
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
        vec!["provider.request", "provider.response", "server.message.send"],
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");
    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
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
        vec!["provider.request", "provider.response", "server.message.send"],
        "subscribe delivered the turn's events in order, client-authored events excluded"
    );
}

// ---------------------------------------------------------------------------
// session.subscribe — resume via since_event_id
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_resume_replays_after_since_event_id_without_gap_or_dup() {
    let ts = start_server().await;
    let mock = start_mock().await;
    let base = format!("{mock}/text");
    let profile_id = ts.create_profile(&base, json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    // Persist two turns' worth of events.
    let (s1, _r1, _raw1) = ts.send_message(&sid, &key, json!({ "content": "one" })).await;
    assert_eq!(s1, 200);
    let (s2, _r2, _raw2) = ts.send_message(&sid, &key, json!({ "content": "two" })).await;
    assert_eq!(s2, 200);

    // The authoritative, ordered, *unfiltered* history (replay is unfiltered too,
    // matching this — the documented replay/live seam).
    let (_s, events, _raw) = ts
        .client_get(&format!("/api/v1/sessions/{sid}/events"), Some(&key))
        .await;
    let items = events["items"].as_array().unwrap();
    assert!(items.len() >= 6, "expected several persisted events: {items:?}");
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
    assert_eq!(got, expected_after, "replay covers exactly the events after since_event_id");
    let mut unique = got.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), got.len(), "no duplicate events in the resume replay");
}

// ---------------------------------------------------------------------------
// MCP round trip — real (non-stub) payloads via a local stdio fixture server
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_round_trip_emits_real_payloads() {
    let ts = start_server_with_registry(echo_registry("echo", None)).await;
    let mock = start_mock().await;
    let base = format!("{mock}/mcp");

    // The profile opts into the "echo" MCP server; the client declares no tools,
    // so the provider's tool_use for `remote_search` is dispatched to MCP.
    let profile_id = ts
        .create_profile_with_mcp(&base, json!(["echo"]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .send_message(&sid, &key, json!({ "content": "search please" }))
        .await;
    assert_eq!(status, 200, "mcp turn: {raw}");
    assert_eq!(resp["message"]["content"][0]["text"], json!("after mcp stub"));

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
    let mock = start_mock().await;
    let base = format!("{mock}/mcp");

    // A unique missing name so this test's log lines are unambiguous in the
    // shared capture buffer.
    let ghost = format!("ghost-{}", generate_id(""));
    let profile_id = ts
        .create_profile_with_mcp(&base, json!(["echo", ghost]), json!([]))
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
        assert_eq!(res["payload"]["server_name"], json!("echo"), "attempt {attempt}");
        assert_eq!(res["payload"]["ok"], json!(true), "attempt {attempt}: echo connected");
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
    let mock = start_mock().await;
    let base = format!("{mock}/mcp");

    let profile_id = ts
        .create_profile_with_mcp(&base, json!([bad]), json!([]))
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
    assert_eq!(status, 201, "connect failure must not fail session creation: {raw}");
    let sid = v["session_id"].as_str().unwrap().to_string();
    let key = v["session_key"].as_str().unwrap().to_string();

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
    assert_eq!(tr["payload"]["is_error"], json!(true), "absent server → error result");

    // And the connect failure was logged.
    let text = captured_text(&logs);
    assert!(
        text.lines().any(|l| l.contains(&bad) && l.contains("failed to connect")),
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
    let mock = start_mock().await;
    let base = format!("{mock}/text");

    let profile_id = ts
        .create_profile_with_mcp(&base, json!(["echo"]), json!([]))
        .await;
    let client_key = ts.create_key(&profile_id).await;
    let (sid, key, _) = ts.open_session(&client_key, json!([])).await;

    // The fixture writes its PID at startup; wait for it, then confirm alive.
    let pid = read_pid(&pidfile).await.expect("fixture wrote its PID");
    assert!(proc_exists(pid), "MCP subprocess {pid} should be running while the session is open");

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
    assert!(terminated, "MCP subprocess {pid} must be terminated after DELETE");

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
