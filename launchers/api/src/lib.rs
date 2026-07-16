//! `launcher-api` — the `baeapi` binary powering the `bae-launcher-api` and
//! `bae-launcher-webapp` base images.
//!
//! `baeapi` reads a `bae-api.toml` (or, in the webapp image, a `bae-app.toml` —
//! the exact same binary, a superset config) describing one or more agents, then
//! serves a single [`axum`] router on one shared port with:
//!
//! - `POST /agents/{name}/trigger` — **one per configured agent** (dispatched by
//!   the `{name}` path segment). Validates the JSON body against that agent's
//!   `request_schema`, templates the validated fields into the child's
//!   env/args, spawns it, and streams its prefixed stdout/stderr back as a
//!   chunked NDJSON body ending with a trailing exit-code line.
//! - `GET /healthz` — unauthenticated liveness (present on every instance).
//! - `GET /_launcher/agents` and `GET /_launcher/agents/{name}` — read-only
//!   introspection (present on every instance), returning only presentation
//!   metadata and the request schema — **never** `env`/`env_template` values or
//!   resolved `${VAR}` secrets.
//!
//! The heavy lifting — `${VAR}` resolution, spawning, per-agent log-line
//! prefixing, unique-name validation, logging init, and the shared error/
//! exit-code mapping — lives in [`launcher_core`]; this crate only adds the
//! HTTP surface, TOML config model, JSON Schema validation, and templating.
//!
//! When [`ENV_WEBAPP_STATIC_DIR`] is set, the same router additionally serves
//! that directory as a single-page application at `/`, with `index.html` as
//! the fallback for client-side routes. The plain API launcher leaves it unset,
//! so it serves no static assets.

use std::process::ExitCode;

pub mod config;
pub mod error;
pub mod http;
pub mod template;

/// Env var naming the config path. Its default differs per base image (baked
/// into each Dockerfile's `ENV`): `/etc/bae/bae-api.toml` for `bae-launcher-api`,
/// `/etc/bae/bae-app.toml` for `bae-launcher-webapp`. The code default below is
/// only the fallback when the env var is unset entirely.
pub const ENV_CONFIG: &str = "BAE_LAUNCHER_API_CONFIG";
/// Code-level default config path when [`ENV_CONFIG`] is unset.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/bae/bae-api.toml";

/// Env var overriding the listen address (wins over `[server] addr` in the
/// config file).
pub const ENV_ADDR: &str = "BAE_LAUNCHER_API_ADDR";
/// Default listen address — distinct from `baesrv`'s `8080`/`8081` so a launcher
/// and a `baesrv`/`bae-max` container can coexist on one host.
pub const DEFAULT_ADDR: &str = "0.0.0.0:9090";

/// Env var carrying the optional bearer token. Unset by default (open port, loud
/// startup warning); when set, every `/agents/*` route requires
/// `Authorization: Bearer <token>` — but never `/healthz` or `/_launcher/*`.
pub const ENV_TOKEN: &str = "BAE_LAUNCHER_API_TOKEN";

/// Optional directory containing the webapp launcher's built static SPA. This
/// is intentionally unset in the plain API launcher image; a non-empty value
/// enables static serving with an `index.html` client-side-routing fallback.
pub const ENV_WEBAPP_STATIC_DIR: &str = "BAE_LAUNCHER_WEBAPP_STATIC_DIR";

/// Env var bounding the graceful-shutdown drain, in whole seconds (default
/// [`DEFAULT_SHUTDOWN_TIMEOUT_SECS`]) — the same pattern as `baesched`'s
/// `BAE_SCHEDULES_SHUTDOWN_TIMEOUT`. On `SIGTERM`/`SIGINT`, in-flight trigger
/// requests get this long to finish; after it elapses the process exits anyway,
/// force-killing any still-running child invocations (`kill_on_drop`). Without
/// the bound, one hung child agent could block shutdown forever — exactly the
/// "a hung child never blocks the launcher" guarantee the launchers make.
pub const ENV_SHUTDOWN_TIMEOUT: &str = "BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT";
/// Default graceful-shutdown drain bound, in seconds.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

/// Run the `baeapi` binary: initialise logging, load the config (fatal on a
/// malformed one, exit 2; a *missing* one is a warning + zero agents), then
/// build and serve the router until a shutdown signal.
///
/// Returns the process [`ExitCode`] (0 success, 1 runtime error, 2 usage error).
pub fn run() -> ExitCode {
    launcher_core::init_logging("info");

    let config_path = std::env::var(ENV_CONFIG).unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_string());

    let loaded = match config::load(&config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            tracing::error!("configuration error: {e}");
            return ExitCode::from(e.exit_code());
        }
    };

    // Invalid timeout syntax is a usage error at startup (exit 2), the same
    // posture as `baesched`'s BAE_SCHEDULES_SHUTDOWN_TIMEOUT.
    let shutdown_timeout = match shutdown_timeout() {
        Ok(timeout) => timeout,
        Err(value) => {
            tracing::error!(
                "{ENV_SHUTDOWN_TIMEOUT} must be a whole number of seconds, got {value:?}"
            );
            return ExitCode::from(2);
        }
    };

    // Build the async runtime by hand (rather than `#[tokio::main]`) so `run`
    // can stay a plain `fn -> ExitCode` and own the config-load exit codes above
    // before any runtime is spun up.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to start async runtime: {e}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(http::serve(loaded, shutdown_timeout))
}

/// Read [`ENV_SHUTDOWN_TIMEOUT`] as a whole number of seconds, defaulting to
/// [`DEFAULT_SHUTDOWN_TIMEOUT_SECS`]. Returns the offending raw value on a
/// parse failure so the caller can report it as a usage error.
fn shutdown_timeout() -> Result<std::time::Duration, String> {
    match std::env::var(ENV_SHUTDOWN_TIMEOUT) {
        Err(_) => Ok(std::time::Duration::from_secs(
            DEFAULT_SHUTDOWN_TIMEOUT_SECS,
        )),
        Ok(value) => value
            .parse::<u64>()
            .map(std::time::Duration::from_secs)
            .map_err(|_| value),
    }
}
