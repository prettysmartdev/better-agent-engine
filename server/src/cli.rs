//! Command-line entrypoint glue.
//!
//! `main.rs` calls [`run`]; everything here — argument parsing, tracing setup,
//! the tokio runtime, and mapping errors to exit codes — lives in the library so
//! it stays testable. Per `aspec/uxui/cli.md`: exit 0 = success, 1 = runtime
//! error, 2 = usage error; logs go to stderr, command results to stdout.
//!
//! Subcommands implemented at this stage: the default `serve`, plus `migrate`
//! and `version`. `serve` is assumed when no subcommand is given.
//!
//! The single global flag is `--config <path>`, giving the path to an optional
//! `bae-config.toml` (the MCP server and LLM provider registries). It may appear before or after
//! the subcommand (`baesrv --config x`, `baesrv serve --config x`). When both
//! `--config` and the `BAE_CONFIG` env var are set, `--config` wins — the
//! flag-beats-env-var precedence `aspec/uxui/cli.md` commits to.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::config::{Config, SandboxDriverKind};
use crate::config_file::BaeConfigFile;
use crate::engine::sandbox::{
    AppleContainerDriver, DockerDriver, SandboxDriver, UnsupportedDriver,
};
use crate::store::{profiles, Store};

/// Parse arguments, run the selected subcommand, and return a process exit code.
pub fn run() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (config_flag, args) = match parse_config_flag(&raw) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("baesrv: {msg}");
            print_usage();
            // Usage error.
            return ExitCode::from(2);
        }
    };
    let subcommand = args.first().map(String::as_str);

    match subcommand {
        None | Some("serve") => run_serve(config_flag),
        Some("migrate") => run_migrate(),
        Some("version") | Some("--version") | Some("-V") => {
            print_version();
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") | Some("help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("baesrv: unknown command {other:?}");
            print_usage();
            // Usage error.
            ExitCode::from(2)
        }
    }
}

/// Pull an optional `--config <path>` (or `--config=<path>`) out of `raw`,
/// returning the resolved path (if any) and the remaining args with the flag
/// removed. An error string is returned if `--config` is given without a value.
fn parse_config_flag(raw: &[String]) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut path: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--config" {
            let value = raw
                .get(i + 1)
                .ok_or_else(|| "--config requires a path argument".to_string())?;
            path = Some(PathBuf::from(value));
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--config=") {
            if value.is_empty() {
                return Err("--config requires a path argument".to_string());
            }
            path = Some(PathBuf::from(value));
            i += 1;
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    Ok((path, rest))
}

/// Resolve the config-file path: `--config` wins over `BAE_CONFIG`; neither set
/// yields `None` (empty MCP registry, not an error).
fn resolve_config_path(flag: Option<PathBuf>) -> Option<PathBuf> {
    resolve_config_path_with(flag, std::env::var_os("BAE_CONFIG").map(PathBuf::from))
}

/// The precedence rule, split from the live environment read so it can be unit
/// tested exhaustively without mutating (racy) process-global `BAE_CONFIG`:
/// `flag` (`--config`) wins over `env` (`BAE_CONFIG`); neither set → `None`.
fn resolve_config_path_with(flag: Option<PathBuf>, env: Option<PathBuf>) -> Option<PathBuf> {
    flag.or(env)
}

fn run_serve(config_flag: Option<PathBuf>) -> ExitCode {
    let config = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    init_tracing(&config.log);

    // Load the optional MCP server + provider registries from bae-config.toml
    // (via --config or BAE_CONFIG). A missing file is not an error; a
    // malformed one is.
    let (mcp_registry, provider_registry) = match load_registries(config_flag) {
        Ok(r) => r,
        Err(code) => return code,
    };

    // Fail fast on database problems before binding any port.
    let store = match crate::open_store(&config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::from(e.exit_code() as u8);
        }
    };

    // Build the host-wide sandbox driver. An unusable selection (apple-container
    // on a non-macOS host) is fatal if any profile already declares
    // available_sandboxes — the operator authored a config the server cannot
    // honour, the same usage-error posture as ConfigFileError.
    let sandbox_driver = match build_sandbox_driver(&config, &store) {
        Ok(d) => d,
        Err(code) => return code,
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(crate::serve(
        config,
        store,
        mcp_registry,
        provider_registry,
        sandbox_driver,
    )) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

/// Turn the configured [`SandboxDriverKind`] into the one server-wide
/// `Arc<dyn SandboxDriver>` (next to [`crate::open_store`] in startup order).
///
/// `apple-container` selected on a non-macOS host cannot work
/// ([`AppleContainerDriver::new`] returns `Unsupported`):
/// - if any active profile declares `available_sandboxes`, this is fatal —
///   usage error, exit 2 — since the server would be unable to honour
///   already-authored config;
/// - otherwise the server starts with an [`UnsupportedDriver`] that surfaces
///   the misconfiguration as a structured error on first use, logged once
///   here at startup.
fn build_sandbox_driver(
    config: &Config,
    store: &Store,
) -> Result<Arc<dyn SandboxDriver>, ExitCode> {
    match config.sandbox_driver {
        SandboxDriverKind::Docker => Ok(Arc::new(DockerDriver::new())),
        SandboxDriverKind::AppleContainer => {
            match AppleContainerDriver::new(std::env::consts::OS) {
                Ok(d) => Ok(Arc::new(d)),
                Err(e) => {
                    let declaring =
                        store
                            .with_conn(profiles::list_with_sandboxes)
                            .map_err(|db| {
                                tracing::error!("failed to check profiles for sandboxes: {db}");
                                ExitCode::FAILURE
                            })?;
                    if !declaring.is_empty() {
                        eprintln!(
                            "baesrv: configuration error: {e} — {} profile(s) declare \
                             available_sandboxes that this driver cannot serve",
                            declaring.len()
                        );
                        tracing::error!(
                            error = %e,
                            profiles = declaring.len(),
                            "BAE_SANDBOX_DRIVER=apple-container is unusable on this host \
                             while profiles declare available_sandboxes; refusing to start"
                        );
                        // Usage error, per aspec/uxui/cli.md.
                        return Err(ExitCode::from(2));
                    }
                    tracing::warn!(
                        error = %e,
                        "BAE_SANDBOX_DRIVER=apple-container is unusable on this host; \
                         starting anyway (no profile declares available_sandboxes) — \
                         sandbox calls will fail as unsupported"
                    );
                    Ok(Arc::new(UnsupportedDriver::new(e.to_string())))
                }
            }
        }
    }
}

/// The `(mcp, providers)` registry pair parsed from one `bae-config.toml`.
type Registries = (
    std::collections::HashMap<String, crate::config_file::McpServerConfig>,
    std::collections::HashMap<String, crate::engine::provider::ProviderConfig>,
);

/// Resolve `--config`/`BAE_CONFIG`, load and validate the MCP server and
/// provider registries. A missing file (or neither source set) yields empty
/// registries with no error; a malformed file or a structural error (duplicate
/// name, unsupported transport/provider kind) is a usage error (exit 2).
/// Tracing is already initialised here, so authoring errors are also echoed to
/// stderr like other config errors.
fn load_registries(config_flag: Option<PathBuf>) -> Result<Registries, ExitCode> {
    let path = resolve_config_path(config_flag);
    load_registries_from(path.as_deref()).map_err(|e| {
        eprintln!("baesrv: configuration error: {e}");
        ExitCode::from(e.exit_code() as u8)
    })
}

/// Path-driven half of [`load_registries`], split out for testability. The
/// file is loaded once and both registries are built from the same parse.
fn load_registries_from(
    path: Option<&Path>,
) -> Result<Registries, crate::config_file::ConfigFileError> {
    let file = BaeConfigFile::load(path)?;
    let mcp = file.mcp_registry()?;
    if !mcp.is_empty() {
        tracing::info!(
            count = mcp.len(),
            "loaded MCP server registry from bae-config.toml"
        );
    }
    let providers = file.provider_registry()?;
    if !providers.is_empty() {
        tracing::info!(
            count = providers.len(),
            "loaded provider registry from bae-config.toml"
        );
    }
    Ok((mcp, providers))
}

fn run_migrate() -> ExitCode {
    let config = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    init_tracing(&config.log);

    match crate::open_store(&config) {
        Ok(_store) => {
            // Opening the store applies pending migrations; drop it to close.
            println!("migrations up to date at {}", config.db_path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

/// Load config, printing usage errors to stderr and returning the exit code to
/// use on failure.
fn load_config() -> Result<Config, ExitCode> {
    Config::from_env().map_err(|e| {
        eprintln!("baesrv: configuration error: {e}");
        ExitCode::from(e.exit_code() as u8)
    })
}

/// Initialise tracing to stderr using the `BAE_LOG` filter. Idempotent-safe:
/// a second call is ignored rather than panicking.
fn init_tracing(filter: &str) {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn print_version() {
    println!("baesrv {}", crate::VERSION);
    println!("api versions: {}", crate::API_VERSIONS.join(", "));
}

fn print_usage() {
    eprintln!(
        "baesrv {} — Better Agent Engine server

USAGE:
    baesrv [COMMAND] [--config <path>]

COMMANDS:
    serve      Run the HTTP server (default)
    migrate    Apply pending database migrations and exit
    version    Print version and supported API versions
    help       Print this message

OPTIONS:
    --config <path>   Path to an optional bae-config.toml (MCP server and
                      LLM provider registries). May also be set via the
                      BAE_CONFIG env var;
                      when both are set, --config wins. A missing file is not
                      an error (empty registry).

Server configuration is via BAE_* environment variables; see docs/ and
aspec/devops/operations.md.",
        crate::VERSION
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_config_flag_space_form() {
        let (path, rest) =
            parse_config_flag(&["serve".into(), "--config".into(), "/etc/bae.toml".into()])
                .unwrap();
        assert_eq!(path, Some(PathBuf::from("/etc/bae.toml")));
        assert_eq!(rest, vec!["serve".to_string()]);
    }

    #[test]
    fn extracts_config_flag_equals_form_before_subcommand() {
        let (path, rest) =
            parse_config_flag(&["--config=/tmp/x.toml".into(), "migrate".into()]).unwrap();
        assert_eq!(path, Some(PathBuf::from("/tmp/x.toml")));
        assert_eq!(rest, vec!["migrate".to_string()]);
    }

    #[test]
    fn missing_config_value_is_error() {
        assert!(parse_config_flag(&["--config".into()]).is_err());
        assert!(parse_config_flag(&["--config=".into()]).is_err());
    }

    #[test]
    fn no_flag_leaves_args_untouched() {
        let (path, rest) = parse_config_flag(&["serve".into()]).unwrap();
        assert!(path.is_none());
        assert_eq!(rest, vec!["serve".to_string()]);
    }

    #[test]
    fn flag_wins_over_env_var() {
        // With a flag present, BAE_CONFIG is not consulted.
        let resolved = resolve_config_path(Some(PathBuf::from("/from/flag.toml")));
        assert_eq!(resolved, Some(PathBuf::from("/from/flag.toml")));
    }

    #[test]
    fn config_precedence_all_four_combinations() {
        let flag = || Some(PathBuf::from("/from/flag.toml"));
        let env = || Some(PathBuf::from("/from/env.toml"));

        // --config alone → the flag path.
        assert_eq!(resolve_config_path_with(flag(), None), flag());
        // BAE_CONFIG alone → the env path.
        assert_eq!(resolve_config_path_with(None, env()), env());
        // Both set → --config wins.
        assert_eq!(resolve_config_path_with(flag(), env()), flag());
        // Neither set → None (empty registry, not an error).
        assert_eq!(resolve_config_path_with(None, None), None);
    }

    #[test]
    fn missing_registry_file_is_empty_no_error() {
        let path = std::env::temp_dir().join("baesrv-cli-absent.toml");
        let (mcp, providers) = load_registries_from(Some(&path)).unwrap();
        assert!(mcp.is_empty());
        assert!(providers.is_empty());
        // Neither source set → None → empty.
        let (mcp, providers) = load_registries_from(None).unwrap();
        assert!(mcp.is_empty());
        assert!(providers.is_empty());
    }
}
