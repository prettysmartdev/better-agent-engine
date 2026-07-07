//! Better Agent Engine (BAE) — server library.
//!
//! The binary in `main.rs` is a thin entrypoint; all server logic lives here so
//! it stays unit-testable. Modules (see `aspec/architecture/design.md`):
//!
//! - [`config`] — environment-driven configuration and its validation.
//! - [`api`]    — HTTP surface: separate client and admin routers.
//! - [`store`]  — SQLite persistence, the migration runner, and key operations.
//! - [`engine`] — the agent/session/run engine (session loop stubbed for now).
//! - [`events`] — the closed message-type schema for `session_events`.
//!
//! [`serve`] is the top-level entrypoint: open the database and migrate, bind
//! both listeners, serve until a shutdown signal, drain, then close the database.

pub mod api;
pub mod cli;
pub mod config;
pub mod config_file;
pub mod engine;
pub mod events;
pub mod store;

pub use config::Config;

use std::collections::HashMap;
use std::net::SocketAddr;

use config_file::McpServerConfig;

use tokio::net::TcpListener;
use tokio::sync::watch;

use api::AppState;
use store::{Store, StoreError};

/// Server version, from the crate manifest.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// API versions this server supports. Reported at `GET /api/v1/meta`.
pub const API_VERSIONS: &[&str] = &["v1"];

/// A runtime failure while starting or running the server.
///
/// These are distinct from [`config::ConfigError`] (usage errors, exit 2): a
/// [`RunError`] is an operational failure and maps to exit code 1.
#[derive(Debug)]
pub enum RunError {
    /// The database could not be opened or migrated.
    Store(StoreError),
    /// A listener could not bind (e.g. the address/port is already in use). If
    /// the admin port is in use we refuse to start rather than skip it.
    Bind {
        which: &'static str,
        addr: SocketAddr,
        source: std::io::Error,
    },
}

impl RunError {
    /// Process exit code — always 1 (runtime error) per `aspec/uxui/cli.md`.
    pub fn exit_code(&self) -> i32 {
        1
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Store(e) => write!(f, "{e}"),
            RunError::Bind {
                which,
                addr,
                source,
            } => {
                write!(f, "cannot bind {which} listener on {addr}: {source}")
            }
        }
    }
}

impl std::error::Error for RunError {}

impl From<StoreError> for RunError {
    fn from(e: StoreError) -> Self {
        RunError::Store(e)
    }
}

/// Open the database and run migrations. Kept separate from [`serve`] so the
/// `migrate` subcommand can reuse it, and so DB failures surface *before* any
/// port is bound (per the startup edge cases in the work item).
pub fn open_store(config: &Config) -> Result<Store, StoreError> {
    Store::open(&config.db_path)
}

/// Run the server: bind both listeners, serve until a shutdown signal, then
/// drain and close the database.
///
/// `store` is passed in (rather than opened here) so the caller can fail fast on
/// database problems before we touch the network.
///
/// `mcp_registry` is the (possibly empty) set of MCP servers parsed from
/// `bae-config.toml`; it is held in-memory on [`AppState`] and never persisted.
pub async fn serve(
    config: Config,
    store: Store,
    mcp_registry: HashMap<String, McpServerConfig>,
) -> Result<(), RunError> {
    tracing::info!(
        version = VERSION,
        api_versions = ?API_VERSIONS,
        db_path = %config.db_path.display(),
        "Better Agent Engine (BAE) starting — welcome!"
    );

    let state = AppState::with_mcp_registry(store, mcp_registry);

    // Periodic activity summary. The first tick fires immediately, so one
    // summary also lands at startup; after that, one per interval.
    let summary_state = state.clone();
    let summary_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(SUMMARY_INTERVAL);
        loop {
            tick.tick().await;
            log_activity_summary(&summary_state);
        }
    });

    // Bind the client listener. Plain HTTP — TLS terminates upstream; this port
    // must sit behind a reverse proxy on an internal network, never exposed
    // directly to the internet (see aspec/architecture/security.md).
    let client_listener =
        TcpListener::bind(config.addr)
            .await
            .map_err(|source| RunError::Bind {
                which: "client",
                addr: config.addr,
                source,
            })?;

    // Bind the admin listener. Bound to loopback (config validation guarantees
    // this); if the admin port is already in use we refuse to start rather than
    // silently skip the admin surface.
    let admin_listener = TcpListener::bind(config.admin_addr)
        .await
        .map_err(|source| RunError::Bind {
            which: "admin",
            addr: config.admin_addr,
            source,
        })?;

    tracing::info!(addr = %config.addr, "client listener bound (plain HTTP; TLS terminates upstream)");
    tracing::info!(addr = %config.admin_addr, "admin listener bound (loopback only; plain HTTP)");

    // One shutdown signal fans out to both listeners via a watch channel.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let client_app = api::client::router(state.clone());
    let admin_app = api::admin::router(state.clone());

    let client_srv = axum::serve(client_listener, client_app)
        .with_graceful_shutdown(wait_for_flag(shutdown_rx.clone()));
    let admin_srv =
        axum::serve(admin_listener, admin_app).with_graceful_shutdown(wait_for_flag(shutdown_rx));

    let client_handle = tokio::spawn(async move { client_srv.await });
    let admin_handle = tokio::spawn(async move { admin_srv.await });

    // Block until SIGTERM/SIGINT, then tell both listeners to stop accepting.
    wait_for_signal().await;
    tracing::info!(
        timeout = ?config.shutdown_timeout,
        "shutdown signal received; draining in-flight requests"
    );
    let _ = shutdown_tx.send(true);

    // Bound the drain: axum's graceful shutdown waits for in-flight requests
    // indefinitely, so cap it with the configured timeout.
    let drained = tokio::time::timeout(config.shutdown_timeout, async {
        let _ = client_handle.await;
        let _ = admin_handle.await;
    })
    .await;
    match drained {
        Ok(()) => tracing::info!("both listeners drained cleanly"),
        Err(_) => tracing::warn!(
            timeout = ?config.shutdown_timeout,
            "drain timed out; shutting down anyway"
        ),
    }

    // Dropping the last `Store` clone here closes the SQLite connection.
    summary_task.abort();
    drop(state);
    tracing::info!("database closed; shutdown complete");
    Ok(())
}

/// How often the activity summary is logged.
const SUMMARY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// One INFO line summarising what the server currently holds: active profiles,
/// client keys, open/total sessions, logged events, and live in-process MCP
/// connections. Fired hourly (and once at startup) by the task [`serve`] spawns.
fn log_activity_summary(state: &AppState) {
    let live_mcp_sessions = state
        .mcp_sessions
        .lock()
        .expect("mcp_sessions mutex poisoned")
        .len();
    match state.store.with_conn(store::activity_counts) {
        Ok(c) => tracing::info!(
            profiles = c.profiles,
            client_keys = c.client_keys,
            open_sessions = c.open_sessions,
            total_sessions = c.total_sessions,
            events = c.events,
            live_mcp_sessions,
            "activity summary"
        ),
        Err(e) => tracing::warn!("activity summary skipped: count query failed: {e}"),
    }
}

/// Resolve once the shutdown flag flips to `true`.
async fn wait_for_flag(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            // Sender dropped: treat as shutdown.
            break;
        }
    }
}

/// Resolve on the first SIGTERM or SIGINT (Ctrl-C on non-Unix).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::debug!("received SIGTERM"),
            _ = interrupt.recv() => tracing::debug!("received SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matches_manifest() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn api_versions_non_empty() {
        assert!(API_VERSIONS.contains(&"v1"));
    }
}
