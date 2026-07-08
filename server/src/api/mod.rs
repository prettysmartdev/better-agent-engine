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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::config_file::McpServerConfig;
use crate::engine::broadcast::EventBroadcaster;
use crate::engine::mcp::McpSession;
use crate::engine::provider::ProviderConfig;
use crate::engine::sandbox::{DockerDriver, SandboxDriver, SandboxHandle, SandboxImageStatus};
use crate::store::Store;

/// Request-logging middleware shared by both routers: one line per request with
/// method, path, response status, and latency. `/healthz` is logged at DEBUG —
/// load balancers and container health checks hit it every few seconds, which
/// would drown an INFO-level log; everything else logs at INFO.
pub async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();

    let response = next.run(request).await;

    let status = response.status().as_u16();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if path == "/healthz" {
        tracing::debug!(%method, path, status, elapsed_ms, "http request");
    } else {
        tracing::info!(%method, path, status, elapsed_ms, "http request");
    }
    response
}

/// Live MCP connections, keyed by session id. A session gets an entry only if at
/// least one of its profile's configured MCP servers connected successfully; the
/// entry is spawned at session creation and dropped (subprocesses killed) on
/// session close. Each value is behind a [`tokio::sync::Mutex`] because a
/// `tools/call` dispatch holds it across `.await`.
type McpSessions = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<McpSession>>>>>;

/// The one live remote sandbox per session, keyed by session id. Same shape as
/// [`McpSessions`] (an `exec` dispatch holds the inner lock across an
/// `.await`). Started by `session.startRemoteSandbox`, removed by
/// `session.stopRemoteSandbox` or session close.
type Sandboxes = Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<SandboxHandle>>>>>;

/// Pull status of every profile-declared sandbox image, keyed
/// `profile_id -> image -> status`. In-memory only (like [`McpSessions`]):
/// rebuilt from `Pending` and re-provisioned for every declaring profile at
/// server startup, so status is never permanently stale. **Consumers must
/// always index by a specific `profile_id`** — never flatten/scan the whole
/// map — because it spans every profile on the server and per-profile scoping
/// is the trust boundary (see `session.sandbox.available` and
/// `session.startRemoteSandbox`).
type SandboxStatusMap = Arc<Mutex<HashMap<String, HashMap<String, SandboxImageStatus>>>>;

/// A paused turn whose FIFO-gate guard is parked between HTTP requests.
///
/// Created when a `session.sendMessage` turn ends [`Paused`]
/// (crate::engine::session::Outcome::Paused): the owner keeps holding the
/// session's turn gate — across requests — until it returns with the
/// continuation, so no other driver's message can interleave with its
/// in-flight tool round trip. If the owner stays away past `deadline`
/// (`BAE_TURN_TIMEOUT`), the next `session.sendMessage` arrival treats the
/// turn as abandoned: it drops the parked guard (releasing the gate to the
/// next FIFO waiter) and logs a `session.error` with reason
/// `driver_turn_abandoned`.
pub struct PendingTurn {
    /// The client key that owns the paused turn — only it may resume without
    /// queuing.
    pub owner_client_key_id: String,
    /// The session's turn-gate guard, held across HTTP requests.
    pub guard: tokio::sync::OwnedMutexGuard<()>,
    /// When the paused turn becomes abandonable.
    pub deadline: tokio::time::Instant,
}

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
    /// Configured LLM providers, keyed by name, parsed from `bae-config.toml`
    /// at startup. Same lifecycle as [`AppState::mcp_registry`]: read-only
    /// after startup, empty without a config file, never persisted. Profiles
    /// reference providers by name (`primary_provider` / `fallback_providers`),
    /// resolved at session-creation and message time.
    pub provider_registry: Arc<HashMap<String, ProviderConfig>>,
    /// Live per-session MCP connections. Populated at session creation and torn
    /// down at session close; keyed by session id. See [`McpSessions`].
    pub mcp_sessions: McpSessions,
    /// Session-scoped event broadcasting: the registry of live
    /// [`tokio::sync::broadcast`] channels feeding `session.sendMessage` and
    /// `session.subscribe` watchers. Channels are created lazily on first
    /// subscribe and dropped on session close.
    pub broadcaster: EventBroadcaster,
    /// Registered drivers, keyed by session id: the client key ids that have
    /// called `session.registerDriver` and may therefore call
    /// `session.sendMessage`. In-memory only (lost on restart, like
    /// [`AppState::mcp_sessions`]); torn down on session close.
    pub drivers: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    /// Per-session FIFO turn gates, created lazily on first
    /// `session.sendMessage`. `tokio::sync::Mutex` grants `lock_owned()`
    /// acquisitions in request order, which is the whole FIFO queue. In-memory
    /// only; torn down on session close.
    pub turn_gates: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// The currently-parked paused turn per session (set only while a turn is
    /// `Paused` awaiting its owner's continuation). In-memory only; torn down
    /// on session close.
    pub pending_turns: Arc<Mutex<HashMap<String, PendingTurn>>>,
    /// How long a paused turn may await its owner's continuation before being
    /// treated as abandoned (`BAE_TURN_TIMEOUT`).
    pub turn_timeout: Duration,
    /// The host-wide sandbox driver (`BAE_SANDBOX_DRIVER`), constructed once
    /// at startup — same `Arc<...>`, cheap-to-clone shape as
    /// [`AppState::mcp_registry`] / [`AppState::provider_registry`].
    pub sandbox_driver: Arc<dyn SandboxDriver>,
    /// Live remote sandboxes, keyed by session id. See [`Sandboxes`].
    pub sandboxes: Sandboxes,
    /// Sandbox image provisioning status per profile. See [`SandboxStatusMap`].
    pub sandbox_status: SandboxStatusMap,
}

impl AppState {
    /// Build application state from the store, with a default provider HTTP
    /// client and **empty** MCP/provider registries.
    pub fn new(store: Store) -> Self {
        Self::with_registries(store, HashMap::new(), HashMap::new())
    }

    /// Build application state with a preloaded MCP server registry and an
    /// empty provider registry.
    pub fn with_mcp_registry(store: Store, mcp_registry: HashMap<String, McpServerConfig>) -> Self {
        Self::with_registries(store, mcp_registry, HashMap::new())
    }

    /// Build application state with preloaded MCP server and provider
    /// registries (both parsed from `bae-config.toml` at startup).
    pub fn with_registries(
        store: Store,
        mcp_registry: HashMap<String, McpServerConfig>,
        provider_registry: HashMap<String, ProviderConfig>,
    ) -> Self {
        AppState {
            store,
            http: reqwest::Client::new(),
            mcp_registry: Arc::new(mcp_registry),
            provider_registry: Arc::new(provider_registry),
            mcp_sessions: Arc::new(Mutex::new(HashMap::new())),
            broadcaster: EventBroadcaster::new(),
            drivers: Arc::new(Mutex::new(HashMap::new())),
            turn_gates: Arc::new(Mutex::new(HashMap::new())),
            pending_turns: Arc::new(Mutex::new(HashMap::new())),
            turn_timeout: Duration::from_secs(crate::config::DEFAULT_TURN_TIMEOUT_SECS),
            sandbox_driver: Arc::new(DockerDriver::new()),
            sandboxes: Arc::new(Mutex::new(HashMap::new())),
            sandbox_status: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Retain the live MCP connections for a session, keyed by its id. Called at
    /// session creation once at least one configured server has connected.
    pub fn insert_mcp_session(&self, session_id: &str, session: McpSession) {
        self.mcp_sessions
            .lock()
            .expect("mcp_sessions mutex poisoned")
            .insert(
                session_id.to_owned(),
                Arc::new(tokio::sync::Mutex::new(session)),
            );
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
    pub fn take_mcp_session(
        &self,
        session_id: &str,
    ) -> Option<Arc<tokio::sync::Mutex<McpSession>>> {
        self.mcp_sessions
            .lock()
            .expect("mcp_sessions mutex poisoned")
            .remove(session_id)
    }

    /// Retain the live remote sandbox for a session. Called by
    /// `session.startRemoteSandbox` once the driver reports the container up.
    pub fn insert_sandbox(&self, session_id: &str, handle: SandboxHandle) {
        self.sandboxes
            .lock()
            .expect("sandboxes mutex poisoned")
            .insert(
                session_id.to_owned(),
                Arc::new(tokio::sync::Mutex::new(handle)),
            );
    }

    /// The session's live remote sandbox, if one was started.
    pub fn sandbox(&self, session_id: &str) -> Option<Arc<tokio::sync::Mutex<SandboxHandle>>> {
        self.sandboxes
            .lock()
            .expect("sandboxes mutex poisoned")
            .get(session_id)
            .cloned()
    }

    /// Remove and return a session's remote sandbox (for the shared stop
    /// helper). The entry is removed up front — before the driver's `stop`
    /// call — so a failed stop can never leave a phantom handle behind.
    pub fn take_sandbox(&self, session_id: &str) -> Option<Arc<tokio::sync::Mutex<SandboxHandle>>> {
        self.sandboxes
            .lock()
            .expect("sandboxes mutex poisoned")
            .remove(session_id)
    }

    /// Seed every image in `images` at [`SandboxImageStatus::Pending`] for
    /// `profile_id`, replacing the profile's previous entry. Called
    /// synchronously at profile write (and per-profile at startup), *before*
    /// the provisioning task is spawned, so a client connecting immediately
    /// after sees `pending` rather than nothing.
    pub fn seed_sandbox_status(&self, profile_id: &str, images: &[String]) {
        let seeded: HashMap<String, SandboxImageStatus> = images
            .iter()
            .map(|i| (i.clone(), SandboxImageStatus::Pending))
            .collect();
        self.sandbox_status
            .lock()
            .expect("sandbox_status mutex poisoned")
            .insert(profile_id.to_owned(), seeded);
    }

    /// Record the provisioning outcome for one profile-declared image.
    pub fn set_sandbox_status(&self, profile_id: &str, image: &str, status: SandboxImageStatus) {
        self.sandbox_status
            .lock()
            .expect("sandbox_status mutex poisoned")
            .entry(profile_id.to_owned())
            .or_default()
            .insert(image.to_owned(), status);
    }

    /// The provisioning status of one image **on one specific profile** —
    /// deliberately the only read shape offered, so callers cannot
    /// accidentally scan across profiles. An unknown profile/image reads as
    /// [`SandboxImageStatus::Pending`].
    pub fn sandbox_image_status(&self, profile_id: &str, image: &str) -> SandboxImageStatus {
        self.sandbox_status
            .lock()
            .expect("sandbox_status mutex poisoned")
            .get(profile_id)
            .and_then(|m| m.get(image))
            .cloned()
            .unwrap_or(SandboxImageStatus::Pending)
    }

    /// Record a `session.registerDriver` call. Returns whether the client key
    /// was newly registered (false on a repeat registration).
    pub fn register_driver(&self, session_id: &str, client_key_id: &str) -> bool {
        self.drivers
            .lock()
            .expect("drivers mutex poisoned")
            .entry(session_id.to_owned())
            .or_default()
            .insert(client_key_id.to_owned())
    }

    /// Whether `client_key_id` has registered as a driver for `session_id`.
    pub fn is_registered_driver(&self, session_id: &str, client_key_id: &str) -> bool {
        self.drivers
            .lock()
            .expect("drivers mutex poisoned")
            .get(session_id)
            .is_some_and(|set| set.contains(client_key_id))
    }

    /// The client key ids currently registered as drivers for a session,
    /// sorted for a deterministic response shape.
    pub fn registered_drivers(&self, session_id: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .drivers
            .lock()
            .expect("drivers mutex poisoned")
            .get(session_id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// The session's FIFO turn gate, created lazily on first use.
    pub fn turn_gate(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.turn_gates
            .lock()
            .expect("turn_gates mutex poisoned")
            .entry(session_id.to_owned())
            .or_default()
            .clone()
    }

    /// Tear down a session's in-memory multi-client state — its driver
    /// registrations, turn gate, and any parked paused turn (whose dropped
    /// guard releases the gate to any queued waiter). Called at session close
    /// alongside the broadcaster/MCP teardown; idempotent.
    pub fn remove_session_runtime(&self, session_id: &str) {
        self.drivers
            .lock()
            .expect("drivers mutex poisoned")
            .remove(session_id);
        self.turn_gates
            .lock()
            .expect("turn_gates mutex poisoned")
            .remove(session_id);
        self.pending_turns
            .lock()
            .expect("pending_turns mutex poisoned")
            .remove(session_id);
    }
}
