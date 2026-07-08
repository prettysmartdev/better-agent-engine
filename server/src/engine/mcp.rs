//! MCP (Model Context Protocol) client.
//!
//! Mirrors the shape of [`super::provider`]: a configuration type comes in
//! (here [`crate::config_file::McpServerConfig`], resolved from `bae-config.toml`
//! at startup), and this module owns the outbound connection and the wire calls.
//! Where `provider` speaks the Anthropic Messages API over HTTP, this module
//! speaks the JSON-RPC 2.0 subset of MCP that the session loop actually needs:
//! `initialize` + `notifications/initialized`, `tools/list`, and `tools/call`.
//!
//! # Why hand-rolled
//!
//! The official Rust SDK (`rmcp`) is a large, fast-moving surface; the engine
//! only needs three request methods and one notification, so — exactly as
//! `provider.rs` hand-rolls the one Anthropic call rather than pulling an SDK —
//! this hand-rolls the JSON-RPC subset. That keeps the dependency graph small
//! and the transport behaviour fully under our control (per-session subprocess
//! isolation, explicit teardown, deterministic offline test builds).
//!
//! # Lifecycle
//!
//! An [`McpSession`] holds every connected server for one BAE session. It is
//! built at session-creation time ([`McpSession::connect`] once per configured
//! server), retained on `AppState` for the life of the session so `run_turn`
//! can dispatch `tools/call` across message requests, and torn down on session
//! close ([`McpSession::shutdown`], which kills any spawned stdio subprocess).
//! Each stdio child is also spawned with `kill_on_drop(true)` as a backstop so
//! an abandoned session cannot leak a subprocess indefinitely.
//!
//! # Transports
//!
//! - `stdio` — spawn one subprocess per session and speak newline-delimited
//!   JSON-RPC over its stdin/stdout. Fully implemented and the primary path.
//! - `http` — the MCP Streamable HTTP transport: POST each JSON-RPC message to
//!   the configured `url`, parsing either a single `application/json` reply or
//!   an SSE (`text/event-stream`) framed reply, and threading the
//!   `Mcp-Session-Id` header the server hands back at `initialize`.
//! - `sse` — routed through the same POST-based Streamable-HTTP client. A server
//!   that only speaks the legacy two-endpoint SSE handshake (a `GET` event
//!   stream that advertises a separate POST endpoint) will fail to connect and
//!   be skipped, non-fatally, by the caller.
//!
//! # Secrets
//!
//! `http`/`sse` header values may contain `${ENV_VAR}` tokens. They are resolved
//! by [`super::provider::resolve_tokens`] immediately before connecting, held
//! only for the request, and never written to an event, a log line, or the
//! database — the same convention as provider `auth_token`.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::provider::resolve_tokens;
use crate::config_file::{McpServerConfig, McpTransport};

/// The MCP protocol revision we advertise at `initialize`.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Upper bound on any single MCP request round-trip, so a wedged server cannot
/// hang a turn forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A failure connecting to, or talking with, an MCP server.
#[derive(Debug)]
pub enum McpError {
    /// Spawning/opening the connection or completing the `initialize` handshake
    /// failed. The caller treats this exactly like "server not found": log and
    /// skip, non-fatal to session creation.
    Connect(String),
    /// The connection was live but a request failed at the transport layer
    /// (subprocess exited, socket dropped, timed out). Mid-turn, this becomes an
    /// error-shaped `tool.result` so the model can adjust; the turn continues.
    Transport(String),
    /// The server returned a JSON-RPC error object or a malformed response.
    Protocol(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Connect(m) => write!(f, "MCP connect failed: {m}"),
            McpError::Transport(m) => write!(f, "MCP transport error: {m}"),
            McpError::Protocol(m) => write!(f, "MCP protocol error: {m}"),
        }
    }
}

impl std::error::Error for McpError {}

/// Every MCP server connected for one BAE session, plus the merged tool
/// advertisement and the `tool_name -> server_name` routing table.
///
/// Held on `AppState` keyed by session id for the life of the session.
#[derive(Default)]
pub struct McpSession {
    /// Live connections, keyed by configured server name.
    servers: HashMap<String, Conn>,
    /// `tool_name -> server_name`, used to route a provider `tool_use` block to
    /// the server that advertised it. Last writer wins on a name collision.
    routes: HashMap<String, String>,
    /// Provider-shaped tool definitions (`{name, description, input_schema}`)
    /// merged across all connected servers, advertised alongside client tools.
    tools: Vec<Value>,
}

impl McpSession {
    /// An empty session with no connected servers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any MCP server is connected. An [`McpSession`] with no servers is
    /// not worth retaining on `AppState`.
    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    /// The merged, provider-shaped tool definitions to advertise to the LLM.
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// The server that advertised `tool_name`, if any.
    pub fn server_for(&self, tool_name: &str) -> Option<&str> {
        self.routes.get(tool_name).map(String::as_str)
    }

    /// A copy of the `tool_name -> server_name` routing table, so a caller can
    /// tag events without holding the session lock across a whole turn.
    pub fn routes_snapshot(&self) -> HashMap<String, String> {
        self.routes.clone()
    }

    /// Connect one configured server: open its transport, run the
    /// `initialize` / `notifications/initialized` handshake, call `tools/list`,
    /// and — only on full success — retain the connection and merge its tools.
    ///
    /// On any failure the freshly-opened connection is dropped (killing a
    /// spawned subprocess via `kill_on_drop`) and nothing is retained, so the
    /// caller can log-and-skip without leaking a process.
    pub async fn connect(&mut self, cfg: &McpServerConfig) -> Result<(), McpError> {
        let mut conn = Conn::open(cfg).await?;

        // initialize handshake.
        let init = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "baesrv", "version": env!("CARGO_PKG_VERSION") },
        });
        conn.request("initialize", init).await?;
        conn.notify("notifications/initialized", json!({})).await?;

        // tools/list — merge into the advertisement and routing table.
        let listed = conn.request("tools/list", json!({})).await?;
        let tools = listed
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for t in &tools {
            let Some(name) = t.get("name").and_then(Value::as_str) else {
                continue;
            };
            self.routes.insert(name.to_owned(), cfg.name.clone());
            self.tools.push(to_provider_tool(t));
        }

        self.servers.insert(cfg.name.clone(), conn);
        Ok(())
    }

    /// Dispatch a `tools/call` to the server that owns `tool_name`.
    ///
    /// Returns the MCP `result` object (`{content, isError?}`). A dead
    /// connection surfaces as [`McpError::Transport`]; the caller synthesizes an
    /// error `tool.result` and continues the turn without reconnecting.
    pub async fn call_tool(&mut self, tool_name: &str, input: &Value) -> Result<Value, McpError> {
        let server = self.routes.get(tool_name).cloned().ok_or_else(|| {
            McpError::Protocol(format!("no MCP server advertises tool {tool_name:?}"))
        })?;
        let conn = self
            .servers
            .get_mut(&server)
            .ok_or_else(|| McpError::Protocol(format!("MCP server {server:?} is not connected")))?;
        let params = json!({ "name": tool_name, "arguments": input });
        conn.request("tools/call", params).await
    }

    /// Terminate every connection: kill spawned stdio subprocesses and drop
    /// HTTP state. Idempotent; safe to call once at session close.
    pub async fn shutdown(&mut self) {
        for (_name, conn) in self.servers.drain() {
            conn.shutdown().await;
        }
        self.routes.clear();
        self.tools.clear();
    }
}

/// Map an MCP tool definition (`{name, description, inputSchema}`) to the
/// provider (Anthropic) tool shape (`{name, description, input_schema}`).
fn to_provider_tool(t: &Value) -> Value {
    let name = t.get("name").cloned().unwrap_or(Value::Null);
    let description = t
        .get("description")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    // Anthropic requires an object schema; default to an open object if absent.
    let input_schema = t
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    json!({ "name": name, "description": description, "input_schema": input_schema })
}

/// A live connection to one MCP server.
enum Conn {
    Stdio(StdioConn),
    Http(HttpConn),
}

impl Conn {
    /// Open the transport named by `cfg`. Does no MCP handshake — that is the
    /// caller's next step.
    async fn open(cfg: &McpServerConfig) -> Result<Conn, McpError> {
        match cfg.transport {
            McpTransport::Stdio => Ok(Conn::Stdio(StdioConn::spawn(cfg)?)),
            McpTransport::Http | McpTransport::Sse => Ok(Conn::Http(HttpConn::open(cfg)?)),
        }
    }

    /// Send a JSON-RPC request and return its `result`, or an [`McpError`].
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        match self {
            Conn::Stdio(c) => c.request(method, params).await,
            Conn::Http(c) => c.request(method, params).await,
        }
    }

    /// Send a JSON-RPC notification (no `id`, no reply expected).
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        match self {
            Conn::Stdio(c) => c.notify(method, params).await,
            Conn::Http(c) => c.notify(method, params).await,
        }
    }

    /// Terminate this connection.
    async fn shutdown(self) {
        match self {
            Conn::Stdio(c) => c.shutdown().await,
            Conn::Http(_) => { /* nothing to tear down: stateless POSTs */ }
        }
    }
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

/// A subprocess speaking newline-delimited JSON-RPC over stdin/stdout.
struct StdioConn {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl StdioConn {
    /// Spawn `command args...` with piped stdin/stdout (stderr discarded) and
    /// `kill_on_drop` so an abandoned session cannot leak the process.
    fn spawn(cfg: &McpServerConfig) -> Result<StdioConn, McpError> {
        let command = cfg
            .command
            .as_deref()
            .ok_or_else(|| McpError::Connect("stdio transport requires `command`".into()))?;
        let mut cmd = Command::new(command);
        cmd.args(&cfg.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Connect(format!("spawn {command:?} failed: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Connect("child stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Connect("child stdout unavailable".into()))?;
        Ok(StdioConn {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    async fn write_message(&mut self, msg: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(msg)
            .map_err(|e| McpError::Protocol(format!("serialize request: {e}")))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("write to MCP server: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("flush to MCP server: {e}")))?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_message(&req).await?;

        // Read lines until we see the response carrying our id, skipping any
        // interleaved notifications or unrelated responses.
        loop {
            let mut buf = String::new();
            let read = tokio::time::timeout(REQUEST_TIMEOUT, self.stdout.read_line(&mut buf))
                .await
                .map_err(|_| McpError::Transport(format!("timed out awaiting {method} response")))?
                .map_err(|e| McpError::Transport(format!("read from MCP server: {e}")))?;
            if read == 0 {
                return Err(McpError::Transport(
                    "MCP server closed the connection".into(),
                ));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                // A non-JSON line (stray server logging on stdout) is skipped.
                Err(_) => continue,
            };
            match msg.get("id").and_then(Value::as_i64) {
                Some(rid) if rid == id => return interpret_response(&msg),
                _ => continue, // notification or a different id: keep reading.
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let note = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write_message(&note).await
    }

    async fn shutdown(mut self) {
        // Best-effort: ask the process to exit, then ensure it is reaped. With
        // kill_on_drop set, dropping would also kill it, but do it explicitly so
        // close is prompt and synchronous with the request.
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

// ---------------------------------------------------------------------------
// Streamable HTTP transport (also serves the `sse` transport value)
// ---------------------------------------------------------------------------

/// A Streamable-HTTP MCP connection: each JSON-RPC message is one POST to `url`.
struct HttpConn {
    http: reqwest::Client,
    url: String,
    /// Resolved (secret-substituted) request headers. Never persisted.
    headers: Vec<(String, String)>,
    /// `Mcp-Session-Id` handed back at `initialize`, echoed on later requests.
    mcp_session_id: Option<String>,
    next_id: i64,
}

impl HttpConn {
    fn open(cfg: &McpServerConfig) -> Result<HttpConn, McpError> {
        let url = cfg
            .url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| McpError::Connect("http/sse transport requires `url`".into()))?;
        // Resolve `${ENV_VAR}` tokens in header values immediately before use.
        let mut headers = Vec::with_capacity(cfg.headers.len());
        for (k, v) in &cfg.headers {
            let resolved =
                resolve_tokens(v).map_err(|e| McpError::Connect(format!("header {k:?}: {e}")))?;
            headers.push((k.clone(), resolved));
        }
        Ok(HttpConn {
            http: reqwest::Client::new(),
            url,
            headers,
            mcp_session_id: None,
            next_id: 1,
        })
    }

    /// POST one JSON-RPC message and return the raw HTTP response, applying the
    /// configured headers, the JSON-RPC content negotiation, and any captured
    /// `Mcp-Session-Id`.
    async fn post(&self, body: &Value) -> Result<reqwest::Response, McpError> {
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(sid) = &self.mcp_session_id {
            req = req.header("mcp-session-id", sid);
        }
        req.json(body)
            .send()
            .await
            .map_err(|e| McpError::Transport(sanitize(e)))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let resp = tokio::time::timeout(REQUEST_TIMEOUT, self.post(&req))
            .await
            .map_err(|_| McpError::Transport(format!("timed out awaiting {method} response")))??;

        let status = resp.status();
        // Capture the server-assigned session id from initialize onward.
        if self.mcp_session_id.is_none() {
            if let Some(sid) = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|h| h.to_str().ok())
            {
                self.mcp_session_id = Some(sid.to_owned());
            }
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Transport(sanitize(e)))?;

        if !status.is_success() {
            return Err(McpError::Transport(format!(
                "MCP server returned HTTP {}",
                status.as_u16()
            )));
        }

        let msg = if content_type.contains("text/event-stream") {
            sse_find_response(&text, id)?
        } else {
            serde_json::from_str(&text)
                .map_err(|e| McpError::Protocol(format!("non-JSON MCP response: {e}")))?
        };
        interpret_response(&msg)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let note = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        // A notification expects no reply; a 2xx (commonly 202 Accepted) is enough.
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, self.post(&note))
            .await
            .map_err(|_| McpError::Transport(format!("timed out sending {method}")))??;
        if !resp.status().is_success() {
            return Err(McpError::Transport(format!(
                "MCP server returned HTTP {} for notification {method}",
                resp.status().as_u16()
            )));
        }
        Ok(())
    }
}

/// Pull the JSON-RPC object carrying `id` out of an SSE (`text/event-stream`)
/// body: scan `data:` lines, parse each as JSON, and return the first with a
/// matching id. Notifications (no id) are skipped.
fn sse_find_response(body: &str, id: i64) -> Result<Value, McpError> {
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if msg.get("id").and_then(Value::as_i64) == Some(id) {
            return Ok(msg);
        }
    }
    Err(McpError::Protocol(
        "no matching JSON-RPC response in SSE stream".into(),
    ))
}

/// reqwest errors can embed the request URL (and thus a token-bearing header is
/// never in the URL, but be defensive and strip it anyway).
fn sanitize(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

/// Interpret a JSON-RPC response object: return its `result`, or map its `error`
/// object to [`McpError::Protocol`].
fn interpret_response(msg: &Value) -> Result<Value, McpError> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(McpError::Protocol(format!("[{code}] {message}")));
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_provider_tool_renames_input_schema() {
        let mcp_tool = json!({
            "name": "read_file",
            "description": "Read a file",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } },
        });
        let provider = to_provider_tool(&mcp_tool);
        assert_eq!(provider["name"], json!("read_file"));
        assert_eq!(provider["description"], json!("Read a file"));
        assert!(provider.get("input_schema").is_some());
        assert!(provider.get("inputSchema").is_none());
        assert_eq!(provider["input_schema"]["type"], json!("object"));
    }

    #[test]
    fn to_provider_tool_defaults_missing_schema() {
        let provider = to_provider_tool(&json!({ "name": "noargs" }));
        assert_eq!(provider["input_schema"], json!({ "type": "object" }));
        assert_eq!(provider["description"], json!(""));
    }

    #[test]
    fn interpret_response_extracts_result() {
        let ok = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
        let result = interpret_response(&ok).unwrap();
        assert_eq!(result, json!({ "tools": [] }));
    }

    #[test]
    fn interpret_response_maps_error() {
        let err =
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32601, "message": "no" } });
        let e = interpret_response(&err).unwrap_err();
        assert!(matches!(e, McpError::Protocol(m) if m.contains("-32601") && m.contains("no")));
    }

    #[test]
    fn sse_find_response_picks_matching_id() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"x/notify\"}\n\n\
                    event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let msg = sse_find_response(body, 7).unwrap();
        assert_eq!(msg["result"]["ok"], json!(true));
    }

    #[test]
    fn sse_find_response_errors_when_absent() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n";
        assert!(sse_find_response(body, 99).is_err());
    }

    #[test]
    fn empty_session_has_no_servers_and_no_tools() {
        let s = McpSession::new();
        assert!(!s.has_servers());
        assert!(s.tools().is_empty());
        assert!(s.server_for("anything").is_none());
    }
}
