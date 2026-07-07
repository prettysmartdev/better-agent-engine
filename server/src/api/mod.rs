//! HTTP surface.
//!
//! The server exposes **two separate axum [`Router`](axum::Router) instances**,
//! each bound to its own listener — they are never merged and admin routes are
//! never gated behind middleware on a shared router:
//!
//! - [`client`] — the client-facing router (`BAE_ADDR`), serving `/healthz`,
//!   `/api/v1/meta`, and (in a later step) the `/api/v1/sessions` API.
//! - [`admin`] — the admin-only router (`BAE_ADMIN_ADDR`), bound strictly to
//!   loopback, serving (in a later step) the `/admin/v1` API.
//!
//! Both routers share the same [`AppState`] so they read/write one database.

pub mod admin;
pub mod client;
pub mod error;
pub mod pagination;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config_file::McpServerConfig;
use crate::engine::broadcast::EventBroadcaster;
use crate::engine::mcp::McpSession;
use crate::store::Store;

/// Live MCP connections, keyed by session id. A session gets an entry only if at
/// least one of its profile's configured MCP servers connected successfully; the
/// entry is spawned at session creation and dropped (subprocesses killed) on
/// session close. Each value is behind a [`tokio::sync::Mutex`] because a
/// `tools/call` dispatch holds it across `.await`.
type McpSessions = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<McpSession>>>>>;

/// Shared state handed to both routers. Cloneable and cheap (the [`Store`] is an
/// `Arc` internally, the [`reqwest::Client`] is itself an `Arc` of a connection
/// pool, and the MCP registry is behind an `Arc`).
#[derive(Clone)]
pub struct AppState {
    /// The SQLite-backed store.
    pub store: Store,
    /// Shared outbound HTTP client for provider (LLM) calls. Reusing one client
    /// keeps the connection pool warm across sessions.
    pub http: reqwest::Client,
    /// Configured MCP servers, keyed by name, parsed from `bae-config.toml` at
    /// startup. Read-only after startup and empty when no config file is
    /// provided; never persisted (rebuilt on restart). Profiles opt in to a
    /// subset of these by name, resolved at session-creation time.
    pub mcp_registry: Arc<HashMap<String, McpServerConfig>>,
    /// Live per-session MCP connections. Populated at session creation and torn
    /// down at session close; keyed by session id. See [`McpSessions`].
    pub mcp_sessions: McpSessions,
    /// Session-scoped event broadcasting: the registry of live
    /// [`tokio::sync::broadcast`] channels feeding `session.sendMessage` and
    /// `session.subscribe` watchers. Channels are created lazily on first
    /// subscribe and dropped on session close.
    pub broadcaster: EventBroadcaster,
}

impl AppState {
    /// Build application state from the store, with a default provider HTTP
    /// client and an **empty** MCP registry.
    pub fn new(store: Store) -> Self {
        Self::with_mcp_registry(store, HashMap::new())
    }

    /// Build application state with a preloaded MCP server registry.
    pub fn with_mcp_registry(
        store: Store,
        mcp_registry: HashMap<String, McpServerConfig>,
    ) -> Self {
        AppState {
            store,
            http: reqwest::Client::new(),
            mcp_registry: Arc::new(mcp_registry),
            mcp_sessions: Arc::new(Mutex::new(HashMap::new())),
            broadcaster: EventBroadcaster::new(),
        }
    }

    /// Retain the live MCP connections for a session, keyed by its id. Called at
    /// session creation once at least one configured server has connected.
    pub fn insert_mcp_session(&self, session_id: &str, session: McpSession) {
        self.mcp_sessions
            .lock()
            .expect("mcp_sessions mutex poisoned")
            .insert(session_id.to_owned(), Arc::new(tokio::sync::Mutex::new(session)));
    }

    /// The live MCP connections for a session, if any were retained.
    pub fn mcp_session(&self, session_id: &str) -> Option<Arc<tokio::sync::Mutex<McpSession>>> {
        self.mcp_sessions
            .lock()
            .expect("mcp_sessions mutex poisoned")
            .get(session_id)
            .cloned()
    }

    /// Remove and return a session's MCP connections (for teardown at close).
    pub fn take_mcp_session(&self, session_id: &str) -> Option<Arc<tokio::sync::Mutex<McpSession>>> {
        self.mcp_sessions
            .lock()
            .expect("mcp_sessions mutex poisoned")
            .remove(session_id)
    }
}
