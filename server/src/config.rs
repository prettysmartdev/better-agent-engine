//! Environment-driven server configuration.
//!
//! The server's own runtime settings (listen addresses, DB path, log filter,
//! shutdown timeout) are configured entirely through `BAE_*` environment
//! variables (see `aspec/uxui/cli.md` and `aspec/devops/operations.md`). Every
//! value has a sensible default so `baesrv` with no environment set still starts
//! a working server.
//!
//! There is one optional *file*-based input, kept deliberately separate from
//! this module because it has different failure semantics: the MCP server
//! registry in `bae-config.toml`, loaded via `--config <path>` or `BAE_CONFIG`
//! (see [`crate::config_file`]). A missing config file is not an error; a
//! malformed `BAE_*` value here is.
//!
//! Validation happens at startup. A malformed value is a **usage error**
//! (process exit code 2 per `aspec/uxui/cli.md`); see [`ConfigError::exit_code`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Default client-facing listen address.
pub const DEFAULT_ADDR: &str = "0.0.0.0:8080";
/// Default admin listen address — loopback only.
pub const DEFAULT_ADMIN_ADDR: &str = "127.0.0.1:8081";
/// Default SQLite database path (matches the image's storage location).
pub const DEFAULT_DB_PATH: &str = "/var/lib/bae/bae.db";
/// Default tracing filter.
pub const DEFAULT_LOG: &str = "info";
/// Default graceful-shutdown drain timeout, in seconds.
pub const DEFAULT_SHUTDOWN_SECS: u64 = 30;
/// Default abandoned-turn timeout, in seconds: how long a paused turn's owner
/// may stay away before the next `session.sendMessage` arrival treats the turn
/// as abandoned and releases the session's FIFO gate.
pub const DEFAULT_TURN_TIMEOUT_SECS: u64 = 120;
/// Default remote-subagent timeout, in seconds.
pub const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;
/// Default count of concurrently running remote subagents per session.
pub const DEFAULT_MAX_SUBAGENTS_PER_SESSION: usize = 8;

/// Which container engine backs sandbox execution (`BAE_SANDBOX_DRIVER`).
///
/// One driver, chosen server-wide — not per-profile — since it reflects what
/// container engine is actually installed on *this host*; a profile's
/// `available_sandboxes` is the per-profile image allowlist on top of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDriverKind {
    /// The `docker` CLI (the default).
    Docker,
    /// Apple's `container` CLI (macOS only).
    AppleContainer,
}

impl SandboxDriverKind {
    /// The `BAE_SANDBOX_DRIVER` value naming this driver.
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxDriverKind::Docker => "docker",
            SandboxDriverKind::AppleContainer => "apple-container",
        }
    }
}

/// Fully-validated server configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Client-facing listener (`BAE_ADDR`). Plain HTTP — TLS terminates upstream.
    pub addr: SocketAddr,
    /// Admin-only listener (`BAE_ADMIN_ADDR`). Always a loopback address.
    pub admin_addr: SocketAddr,
    /// SQLite database path (`BAE_DB_PATH`).
    pub db_path: PathBuf,
    /// Tracing filter (`BAE_LOG`), e.g. `info` or `baesrv=debug,tower=warn`.
    pub log: String,
    /// How long to drain in-flight requests on shutdown (`BAE_SHUTDOWN_TIMEOUT`).
    pub shutdown_timeout: Duration,
    /// How long a paused turn may await its owner's continuation before being
    /// treated as abandoned (`BAE_TURN_TIMEOUT`).
    pub turn_timeout: Duration,
    /// Default timeout for one remote subagent (`BAE_SUBAGENT_TIMEOUT`).
    pub subagent_timeout: Duration,
    /// Concurrent remote-subagent cap (`BAE_MAX_SUBAGENTS_PER_SESSION`).
    pub max_subagents_per_session: usize,
    /// Which container engine backs sandbox execution (`BAE_SANDBOX_DRIVER`,
    /// `docker` by default or `apple-container`).
    pub sandbox_driver: SandboxDriverKind,
}

/// A configuration problem detected at startup.
///
/// All variants are usage errors (exit code 2): the operator supplied a value
/// the server cannot use. Runtime failures (unwritable DB, port in use) are
/// reported separately — see [`crate::RunError`].
#[derive(Debug)]
pub enum ConfigError {
    /// A `*_ADDR` value did not parse as `host:port`.
    InvalidAddr {
        var: &'static str,
        value: String,
        source: std::net::AddrParseError,
    },
    /// A duration-valued variable was not a non-negative integer of seconds.
    InvalidDuration { var: &'static str, value: String },
    /// `BAE_ADMIN_ADDR` resolved to a non-loopback address. The admin surface
    /// must never be reachable off-host, so we refuse to start.
    AdminNotLoopback { addr: SocketAddr },
    /// `BAE_SANDBOX_DRIVER` named something other than `docker` or
    /// `apple-container`.
    InvalidSandboxDriver { value: String },
}

impl ConfigError {
    /// Process exit code for this error — always 2 (usage error) per
    /// `aspec/uxui/cli.md`.
    pub fn exit_code(&self) -> i32 {
        2
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::InvalidAddr { var, value, source } => write!(
                f,
                "{var}: {value:?} is not a valid host:port address ({source})"
            ),
            ConfigError::InvalidDuration { var, value } => {
                write!(f, "{var}: {value:?} is not a whole number of seconds")
            }
            ConfigError::AdminNotLoopback { addr } => write!(
                f,
                "BAE_ADMIN_ADDR: {addr} is not a loopback address; the admin port \
                 must bind to localhost only"
            ),
            ConfigError::InvalidSandboxDriver { value } => write!(
                f,
                "BAE_SANDBOX_DRIVER: {value:?} is not a supported sandbox driver \
                 (expected \"docker\" or \"apple-container\")"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Build a [`Config`] from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        Config::resolve(&|key| std::env::var(key).ok())
    }

    /// Build a [`Config`] from an arbitrary getter. Kept separate from
    /// [`Config::from_env`] so tests can supply values without touching (and
    /// racing on) the real process environment.
    fn resolve(get: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let addr = parse_addr(get, "BAE_ADDR", DEFAULT_ADDR)?;
        let admin_addr = parse_addr(get, "BAE_ADMIN_ADDR", DEFAULT_ADMIN_ADDR)?;
        if !admin_addr.ip().is_loopback() {
            return Err(ConfigError::AdminNotLoopback { addr: admin_addr });
        }

        let db_path = get("BAE_DB_PATH")
            .unwrap_or_else(|| DEFAULT_DB_PATH.to_owned())
            .into();
        let log = get("BAE_LOG").unwrap_or_else(|| DEFAULT_LOG.to_owned());
        let shutdown_timeout = Duration::from_secs(parse_secs(
            get,
            "BAE_SHUTDOWN_TIMEOUT",
            DEFAULT_SHUTDOWN_SECS,
        )?);
        let turn_timeout = Duration::from_secs(parse_secs(
            get,
            "BAE_TURN_TIMEOUT",
            DEFAULT_TURN_TIMEOUT_SECS,
        )?);
        let subagent_timeout = Duration::from_secs(parse_secs(
            get,
            "BAE_SUBAGENT_TIMEOUT",
            DEFAULT_SUBAGENT_TIMEOUT_SECS,
        )?);
        let max_subagents_per_session = parse_secs(
            get,
            "BAE_MAX_SUBAGENTS_PER_SESSION",
            DEFAULT_MAX_SUBAGENTS_PER_SESSION as u64,
        )? as usize;
        let sandbox_driver = parse_sandbox_driver(get)?;

        Ok(Config {
            addr,
            admin_addr,
            db_path,
            log,
            shutdown_timeout,
            turn_timeout,
            subagent_timeout,
            max_subagents_per_session,
            sandbox_driver,
        })
    }
}

fn parse_sandbox_driver(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<SandboxDriverKind, ConfigError> {
    match get("BAE_SANDBOX_DRIVER") {
        None => Ok(SandboxDriverKind::Docker),
        Some(v) => match v.trim() {
            "docker" => Ok(SandboxDriverKind::Docker),
            "apple-container" => Ok(SandboxDriverKind::AppleContainer),
            _ => Err(ConfigError::InvalidSandboxDriver { value: v }),
        },
    }
}

fn parse_addr(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
    default: &str,
) -> Result<SocketAddr, ConfigError> {
    let value = get(var).unwrap_or_else(|| default.to_owned());
    value
        .parse::<SocketAddr>()
        .map_err(|source| ConfigError::InvalidAddr { var, value, source })
}

fn parse_secs(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    match get(var) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .map_err(|_| ConfigError::InvalidDuration { var, value: v }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn defaults_when_unset() {
        let cfg = Config::resolve(&getter(&[])).unwrap();
        assert_eq!(cfg.addr.to_string(), "0.0.0.0:8080");
        assert_eq!(cfg.admin_addr.to_string(), "127.0.0.1:8081");
        assert_eq!(cfg.db_path.to_str().unwrap(), DEFAULT_DB_PATH);
        assert_eq!(cfg.log, "info");
        assert_eq!(cfg.shutdown_timeout, Duration::from_secs(30));
        assert_eq!(cfg.turn_timeout, Duration::from_secs(120));
        assert_eq!(cfg.sandbox_driver, SandboxDriverKind::Docker);
    }

    #[test]
    fn overrides_are_applied() {
        let cfg = Config::resolve(&getter(&[
            ("BAE_ADDR", "127.0.0.1:9000"),
            ("BAE_ADMIN_ADDR", "127.0.0.1:9001"),
            ("BAE_DB_PATH", "/data/x.db"),
            ("BAE_LOG", "debug"),
            ("BAE_SHUTDOWN_TIMEOUT", "5"),
            ("BAE_TURN_TIMEOUT", "7"),
        ]))
        .unwrap();
        assert_eq!(cfg.addr.to_string(), "127.0.0.1:9000");
        assert_eq!(cfg.db_path.to_str().unwrap(), "/data/x.db");
        assert_eq!(cfg.log, "debug");
        assert_eq!(cfg.shutdown_timeout, Duration::from_secs(5));
        assert_eq!(cfg.turn_timeout, Duration::from_secs(7));
    }

    #[test]
    fn invalid_addr_is_usage_error() {
        let err = Config::resolve(&getter(&[("BAE_ADDR", "not-an-addr")])).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAddr { .. }));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn non_loopback_admin_addr_is_rejected() {
        let err = Config::resolve(&getter(&[("BAE_ADMIN_ADDR", "0.0.0.0:8081")])).unwrap_err();
        assert!(matches!(err, ConfigError::AdminNotLoopback { .. }));
    }

    #[test]
    fn invalid_duration_is_rejected() {
        assert!(matches!(
            Config::resolve(&getter(&[("BAE_SHUTDOWN_TIMEOUT", "soon")])).unwrap_err(),
            ConfigError::InvalidDuration { .. }
        ));
    }

    #[test]
    fn sandbox_driver_defaults_to_docker_when_unset() {
        let cfg = Config::resolve(&getter(&[])).unwrap();
        assert_eq!(cfg.sandbox_driver, SandboxDriverKind::Docker);
    }

    #[test]
    fn sandbox_driver_parses_both_supported_values() {
        let docker = Config::resolve(&getter(&[("BAE_SANDBOX_DRIVER", "docker")])).unwrap();
        assert_eq!(docker.sandbox_driver, SandboxDriverKind::Docker);
        assert_eq!(docker.sandbox_driver.as_str(), "docker");

        let apple = Config::resolve(&getter(&[("BAE_SANDBOX_DRIVER", "apple-container")])).unwrap();
        assert_eq!(apple.sandbox_driver, SandboxDriverKind::AppleContainer);
        assert_eq!(apple.sandbox_driver.as_str(), "apple-container");
    }

    #[test]
    fn sandbox_driver_value_is_trimmed() {
        // Surrounding whitespace (a common copy-paste artefact in env files) is
        // tolerated, matching the `parse_secs` trimming posture.
        let cfg =
            Config::resolve(&getter(&[("BAE_SANDBOX_DRIVER", "  apple-container  ")])).unwrap();
        assert_eq!(cfg.sandbox_driver, SandboxDriverKind::AppleContainer);
    }

    #[test]
    fn invalid_sandbox_driver_is_usage_error() {
        let err = Config::resolve(&getter(&[("BAE_SANDBOX_DRIVER", "podman")])).unwrap_err();
        match &err {
            ConfigError::InvalidSandboxDriver { value } => assert_eq!(value, "podman"),
            other => panic!("expected InvalidSandboxDriver, got {other:?}"),
        }
        // A malformed driver name is a usage error (process exit code 2).
        assert_eq!(err.exit_code(), 2);
    }
}
