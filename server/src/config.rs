//! Environment-driven configuration.
//!
//! The server is configured entirely through `BAE_*` environment variables (see
//! `aspec/uxui/cli.md` and `aspec/devops/operations.md`); there is no config
//! file. Every value has a sensible default so `baesrv` with no environment set
//! still starts a working server.
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
    /// Whether an upstream proxy terminates TLS (`BAE_TLS_ENABLED`). Informational:
    /// the container always speaks plain HTTP internally regardless of this flag.
    pub tls_enabled: bool,
    /// How long to drain in-flight requests on shutdown (`BAE_SHUTDOWN_TIMEOUT`).
    pub shutdown_timeout: Duration,
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
    /// A boolean-valued variable was neither truthy nor falsy.
    InvalidBool { var: &'static str, value: String },
    /// A duration-valued variable was not a non-negative integer of seconds.
    InvalidDuration { var: &'static str, value: String },
    /// `BAE_ADMIN_ADDR` resolved to a non-loopback address. The admin surface
    /// must never be reachable off-host, so we refuse to start.
    AdminNotLoopback { addr: SocketAddr },
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
            ConfigError::InvalidBool { var, value } => write!(
                f,
                "{var}: {value:?} is not a boolean (use true/false, 1/0, yes/no)"
            ),
            ConfigError::InvalidDuration { var, value } => {
                write!(f, "{var}: {value:?} is not a whole number of seconds")
            }
            ConfigError::AdminNotLoopback { addr } => write!(
                f,
                "BAE_ADMIN_ADDR: {addr} is not a loopback address; the admin port \
                 must bind to localhost only"
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
        let tls_enabled = parse_bool(get, "BAE_TLS_ENABLED", false)?;
        let shutdown_timeout = Duration::from_secs(parse_secs(
            get,
            "BAE_SHUTDOWN_TIMEOUT",
            DEFAULT_SHUTDOWN_SECS,
        )?);

        Ok(Config {
            addr,
            admin_addr,
            db_path,
            log,
            tls_enabled,
            shutdown_timeout,
        })
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

fn parse_bool(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    match get(var) {
        None => Ok(default),
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(ConfigError::InvalidBool { var, value: v }),
        },
    }
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
        assert!(!cfg.tls_enabled);
        assert_eq!(cfg.shutdown_timeout, Duration::from_secs(30));
    }

    #[test]
    fn overrides_are_applied() {
        let cfg = Config::resolve(&getter(&[
            ("BAE_ADDR", "127.0.0.1:9000"),
            ("BAE_ADMIN_ADDR", "127.0.0.1:9001"),
            ("BAE_DB_PATH", "/data/x.db"),
            ("BAE_LOG", "debug"),
            ("BAE_TLS_ENABLED", "true"),
            ("BAE_SHUTDOWN_TIMEOUT", "5"),
        ]))
        .unwrap();
        assert_eq!(cfg.addr.to_string(), "127.0.0.1:9000");
        assert_eq!(cfg.db_path.to_str().unwrap(), "/data/x.db");
        assert_eq!(cfg.log, "debug");
        assert!(cfg.tls_enabled);
        assert_eq!(cfg.shutdown_timeout, Duration::from_secs(5));
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
    fn invalid_bool_and_duration_are_rejected() {
        assert!(matches!(
            Config::resolve(&getter(&[("BAE_TLS_ENABLED", "maybe")])).unwrap_err(),
            ConfigError::InvalidBool { .. }
        ));
        assert!(matches!(
            Config::resolve(&getter(&[("BAE_SHUTDOWN_TIMEOUT", "soon")])).unwrap_err(),
            ConfigError::InvalidDuration { .. }
        ));
    }
}
