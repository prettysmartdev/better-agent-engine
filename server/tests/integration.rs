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
//! - provider fallback (failure + success), all-providers-failed (502), and a
//!   missing-env-var provider failure;
//! - client-side and MCP tool dispatch round trips;
//! - the tool allowlist;
//! - the revoke cascade invalidating a live session.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{json, Value};

use baesrv::api::AppState;
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
    let dir = std::env::temp_dir().join(format!("baesrv-it-{}", generate_id("")));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("test.db");

    // A real file-backed store exercises the migration runner on a fresh DB.
    let store = Store::open(&db_path).expect("open temp store");
    let state = AppState::new(store);

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
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "role": "user", "content": "hello" } }),
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

    // A wholly bogus session key → 401.
    let (status, _v, _raw) = ts
        .client_post(
            &format!("/api/v1/sessions/{session1}/messages"),
            Some("bae_ses_bogus"),
            json!({ "message": { "content": "hi" } }),
        )
        .await;
    assert_eq!(status, 401, "bogus session key");

    // A *valid* session key used on the wrong session → 401 (the lookup is
    // scoped to the session id, so a mismatched key finds no row).
    let (status, _v, _raw) = ts
        .client_post(
            &format!("/api/v1/sessions/{session1}/messages"),
            Some(&session2_key),
            json!({ "message": { "content": "hi" } }),
        )
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
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": "hello" } }),
        )
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
async fn all_providers_failed_returns_502() {
    let ts = start_server().await;
    let mock = start_mock().await;
    let primary = format!("{mock}/fail");

    let profile_id = ts.create_profile(&primary, json!([]), json!([])).await;
    let client_key = ts.create_key(&profile_id).await;
    let (session_id, session_key, _) = ts.open_session(&client_key, json!([])).await;

    let (status, resp, raw) = ts
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": "hello" } }),
        )
        .await;
    // 502 with the normal {message, events} body (NOT a problem doc).
    assert_eq!(status, 502, "all providers failed: {raw}");
    assert!(resp["message"].is_object());
    let types = event_types(&resp["events"]);
    assert!(types.contains(&"provider.response".to_string()));
    assert!(types.contains(&"session.error".to_string()));
    // The session is now in error state → further sends are a conflict.
    let (status, _v, _raw) = ts
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": "again" } }),
        )
        .await;
    assert_eq!(status, 409, "error-state session rejects new turns");
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
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": "hello" } }),
        )
        .await;
    assert_eq!(
        status, 502,
        "missing env var should fail the provider: {raw}"
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
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": "what time is it?" } }),
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
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": "12:00 UTC",
            }] } }),
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
        .client_post(
            &format!("/api/v1/sessions/{session_id}/messages"),
            Some(&session_key),
            json!({ "message": { "content": "search please" } }),
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
