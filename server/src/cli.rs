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
//! The `--config <path>` flag gives the path to an optional `bae-config.toml`
//! (the MCP server and LLM provider registries). It may appear before or after
//! the subcommand (`baesrv --config x`, `baesrv serve --config x`). When both
//! `--config` and the `BAE_CONFIG` env var are set, `--config` wins — the
//! flag-beats-env-var precedence `aspec/uxui/cli.md` commits to.
//!
//! `serve` additionally accepts the admin-auth flags `--admin-key-file`,
//! `--admin-key-hash-file`, `--rotate-admin-key`, and
//! `--dangerously-disable-admin-auth` (see [`crate::admin_auth`]). Passing
//! `--rotate-admin-key` together with `--dangerously-disable-admin-auth` is a
//! usage error (exit 2), caught here at parse time before anything is opened.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::admin_auth::{
    self, AdminAuthConfig, DEFAULT_ADMIN_KEY_FILE, DEFAULT_ADMIN_KEY_HASH_FILE, ENV_ADMIN_KEY_FILE,
    ENV_ADMIN_KEY_HASH_FILE, ENV_DISABLE_ADMIN_AUTH,
};
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
    let (admin_flags, args) = match parse_admin_flags(&args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("baesrv: {msg}");
            print_usage();
            // Usage error.
            return ExitCode::from(2);
        }
    };
    // Rotating a key that nothing will enforce is contradictory; reject the
    // combination at parse time, before opening the store or touching any file.
    if admin_flags.rotate && admin_flags.disable {
        eprintln!(
            "baesrv: --rotate-admin-key cannot be combined with \
             --dangerously-disable-admin-auth"
        );
        print_usage();
        return ExitCode::from(2);
    }
    let subcommand = args.first().map(String::as_str);

    match subcommand {
        None | Some("serve") => run_serve(config_flag, admin_flags),
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

/// The admin-auth switches parsed off the command line (before the env-var and
/// default fallbacks are applied in [`resolve_admin_auth_config`]).
#[derive(Debug, Default)]
struct AdminFlags {
    /// `--admin-key-file <path>`; overrides `BAE_ADMIN_KEY_FILE` when set.
    key_file: Option<PathBuf>,
    /// `--admin-key-hash-file <path>`; overrides `BAE_ADMIN_KEY_HASH_FILE`.
    hash_file: Option<PathBuf>,
    /// `--rotate-admin-key` — one-shot, no env-var equivalent by design.
    rotate: bool,
    /// `--dangerously-disable-admin-auth`.
    disable: bool,
}

/// Pull the admin-auth flags out of `raw`, returning them plus the remaining
/// args with those flags removed. Value flags accept both `--flag value` and
/// `--flag=value`; the two boolean flags take no value. An error string is
/// returned if a value flag is given without a value.
fn parse_admin_flags(raw: &[String]) -> Result<(AdminFlags, Vec<String>), String> {
    let mut flags = AdminFlags::default();
    let mut rest: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if let Some((value, next)) = take_path_flag(raw, i, "--admin-key-file")? {
            flags.key_file = Some(value);
            i = next;
            continue;
        }
        if let Some((value, next)) = take_path_flag(raw, i, "--admin-key-hash-file")? {
            flags.hash_file = Some(value);
            i = next;
            continue;
        }
        if arg == "--rotate-admin-key" {
            flags.rotate = true;
            i += 1;
        } else if arg == "--dangerously-disable-admin-auth" {
            flags.disable = true;
            i += 1;
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    Ok((flags, rest))
}

/// Match a `--name <value>` / `--name=<value>` path flag at index `i`. Returns
/// `Ok(Some((value, next_index)))` on a match, `Ok(None)` if `raw[i]` is not
/// this flag, and `Err` if the flag is present without a value.
fn take_path_flag(
    raw: &[String],
    i: usize,
    name: &str,
) -> Result<Option<(PathBuf, usize)>, String> {
    let arg = &raw[i];
    if arg == name {
        let value = raw
            .get(i + 1)
            .ok_or_else(|| format!("{name} requires a path argument"))?;
        Ok(Some((PathBuf::from(value), i + 2)))
    } else if let Some(value) = arg.strip_prefix(&format!("{name}=")) {
        if value.is_empty() {
            return Err(format!("{name} requires a path argument"));
        }
        Ok(Some((PathBuf::from(value), i + 1)))
    } else {
        Ok(None)
    }
}

/// Resolve the effective admin-auth configuration: each path is the flag value,
/// else the env var, else the built-in default (flag-beats-env, matching
/// `--config`). `disabled` is true if the flag is set *or* the
/// `BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH` env var is truthy.
fn resolve_admin_auth_config(flags: AdminFlags) -> AdminAuthConfig {
    let key_file = flags
        .key_file
        .or_else(|| std::env::var_os(ENV_ADMIN_KEY_FILE).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ADMIN_KEY_FILE));
    let hash_file = flags
        .hash_file
        .or_else(|| std::env::var_os(ENV_ADMIN_KEY_HASH_FILE).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ADMIN_KEY_HASH_FILE));
    let disabled = flags.disable || env_truthy(ENV_DISABLE_ADMIN_AUTH);
    AdminAuthConfig {
        key_file,
        hash_file,
        rotate: flags.rotate,
        disabled,
    }
}

/// A `BAE_*` boolean env var is "on" when set to `1` or (case-insensitively)
/// `true`. Anything else — unset, empty, `0`, `false` — is off.
fn env_truthy(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

fn run_serve(config_flag: Option<PathBuf>, admin_flags: AdminFlags) -> ExitCode {
    let config = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };

    // Build the async runtime up front. Two things need it before the server
    // loop starts: the OTLP/gRPC (tonic) telemetry exporter must be constructed
    // within a Tokio runtime, and its `tracing` layer must be composed into the
    // subscriber *at* init_tracing time (the subscriber is set once, globally).
    // The same runtime serves the request loop below.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("baesrv: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Parse bae-config.toml once (via --config or BAE_CONFIG). A missing file is
    // not an error; a malformed file — or an invalid `[telemetry]` section — is
    // a usage error (exit 2). Telemetry is validated first so its export layer
    // can be composed when tracing is initialised.
    let bae_config = match load_bae_config(config_flag) {
        Ok(f) => f,
        Err(code) => return code,
    };
    let telemetry_config = match bae_config.telemetry_config() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("baesrv: configuration error: {e}");
            return ExitCode::from(e.exit_code() as u8);
        }
    };

    // Bring up the OTel SDK inside the runtime, then compose its trace layer
    // into the global subscriber. When telemetry is disabled this yields no
    // layer and init_tracing behaves exactly as before (fmt to stderr only).
    let telemetry = match runtime.block_on(crate::telemetry::init(&telemetry_config)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("baesrv: {e}");
            // Startup usage error, same posture as an unresolvable provider secret.
            return ExitCode::from(2);
        }
    };
    let telemetry_guard = telemetry.guard;
    init_tracing(&config.log, telemetry.layer);

    // Build the MCP server + provider registries from the same parse. Their
    // info logs land now that the subscriber (with any telemetry layer) is set.
    let (mcp_registry, provider_registry) = match build_registries(&bae_config) {
        Ok(r) => r,
        Err(code) => return code,
    };

    // Resolve the admin-auth settings (flag > env > default). Re-check the
    // rotate/disable contradiction here too, since `disabled` may come from the
    // BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH env var (not only the flag) — still
    // caught before the store is opened or any file is touched.
    let admin_cfg = resolve_admin_auth_config(admin_flags);
    if admin_cfg.rotate && admin_cfg.disabled {
        eprintln!(
            "baesrv: --rotate-admin-key cannot be combined with admin auth being \
             disabled (--dangerously-disable-admin-auth / \
             BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH)"
        );
        return ExitCode::from(2);
    }

    // Fail fast on database problems before binding any port.
    let store = match crate::open_store(&config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::from(e.exit_code() as u8);
        }
    };

    // Bootstrap admin authentication after the store opens and before either
    // listener binds: ensure an active admin key exists (self-generate or ingest
    // a pre-provisioned hash) unless auth is disabled. Returns whether the admin
    // router should enforce a bearer key.
    let admin_auth_enabled = match admin_auth::bootstrap(&store, &admin_cfg) {
        Ok(enabled) => enabled,
        Err(e) => {
            tracing::error!("admin-auth bootstrap failed: {e}");
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

    // Reuse the runtime built at the top of this function (the one the telemetry
    // exporter was constructed on) for the server loop itself.
    match runtime.block_on(crate::serve(
        config,
        store,
        mcp_registry,
        provider_registry,
        telemetry_config,
        admin_auth_enabled,
        sandbox_driver,
        telemetry_guard,
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

/// The `(mcp, providers)` registries parsed from one `bae-config.toml`.
type Registries = (
    std::collections::HashMap<String, crate::config_file::McpServerConfig>,
    std::collections::HashMap<String, crate::engine::provider::ProviderConfig>,
);

/// Resolve `--config`/`BAE_CONFIG` and parse `bae-config.toml` once. A missing
/// file (or neither source set) yields an empty config with no error; a
/// malformed file is a usage error (exit 2). Callers pull the telemetry config
/// (before tracing init) and the MCP/provider registries (after) off the same
/// parse, so the file is read exactly once. Errors are echoed to stderr since
/// tracing may not be initialised yet.
fn load_bae_config(
    config_flag: Option<PathBuf>,
) -> Result<crate::config_file::BaeConfig, ExitCode> {
    let path = resolve_config_path(config_flag);
    BaeConfigFile::load(path.as_deref()).map_err(|e| {
        eprintln!("baesrv: configuration error: {e}");
        ExitCode::from(e.exit_code() as u8)
    })
}

/// Build and validate the MCP server + provider registries from an
/// already-parsed config file. A structural validation error is a usage error
/// (exit 2). Tracing is initialised by the time this runs, so the "loaded …"
/// info lines land.
fn build_registries(file: &crate::config_file::BaeConfig) -> Result<Registries, ExitCode> {
    let to_code = |e: crate::config_file::ConfigFileError| {
        eprintln!("baesrv: configuration error: {e}");
        ExitCode::from(e.exit_code() as u8)
    };
    let mcp = file.mcp_registry().map_err(to_code)?;
    if !mcp.is_empty() {
        tracing::info!(
            count = mcp.len(),
            "loaded MCP server registry from bae-config.toml"
        );
    }
    let providers = file.provider_registry().map_err(to_code)?;
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
    // `migrate` has no server loop and needs no telemetry export.
    init_tracing(&config.log, None);

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

/// Initialise tracing to stderr using the `BAE_LOG` filter, composing the
/// optional OpenTelemetry trace layer alongside the fmt layer. When
/// `otel_layer` is `None` (telemetry disabled, or the `migrate` path) this is
/// exactly the previous fmt-only behaviour and no spans are emitted.
/// Idempotent-safe: a second call is ignored rather than panicking.
fn init_tracing(filter: &str, otel_layer: Option<crate::telemetry::BoxTraceLayer>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let fmt_filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    // Trace export must NOT be gated by the stderr log threshold (`BAE_LOG`):
    // an operator running with the ordinary production setting
    // `BAE_LOG=baesrv=warn` must still get the info-level BAE spans exported,
    // otherwise every span constructor (all `info_span!`) is filtered out and
    // the collector receives nothing. Export volume is controlled by the OTel
    // sampler (`sample_ratio`), never by the log level. The OTel layer therefore
    // carries its own filter, taken from `BAE_OTEL_LOG` when set and defaulting
    // to `info` so every span BAE opens is captured regardless of `BAE_LOG`.
    let otel_filter = std::env::var("BAE_OTEL_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let composed = otel_layer.is_some();

    // Order matters: the OTel layer is added first so its subscriber type is the
    // bare `Registry`, keeping the composed type nameable; the fmt layer wraps
    // around it. The fmt layer keeps its `BAE_LOG` filter (stderr output is
    // unchanged); the OTel layer uses its own, decoupled filter (above).
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(fmt_filter);
    let initialized = tracing_subscriber::registry()
        .with(otel_layer.with_filter(otel_filter))
        .with(fmt_layer)
        .try_init()
        .is_ok();

    // The span constructors gate on this: real spans only when the layer is live.
    crate::telemetry::mark_active(composed && initialized);
}

fn print_version() {
    println!("baesrv {}", crate::VERSION);
    println!("api versions: {}", crate::API_VERSIONS.join(", "));
}

fn print_usage() {
    eprintln!(
        "baesrv {} — Better Agent Engine server

USAGE:
    baesrv [COMMAND] [OPTIONS]

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

SERVE OPTIONS (admin-port authentication):
    --admin-key-file <path>        Plaintext admin-key file the server writes on
                                   self-generate and baectl reads. Default
                                   /var/lib/bae/admin-key.pem; also BAE_ADMIN_KEY_FILE.
    --admin-key-hash-file <path>   Pre-provisioned Argon2id hash file to ingest
                                   (read-only). Default
                                   /var/lib/bae/admin-key-hash.pem; also
                                   BAE_ADMIN_KEY_HASH_FILE.
    --rotate-admin-key             Revoke the current admin key and mint a fresh
                                   one this boot (one-shot; no env-var equivalent).
    --dangerously-disable-admin-auth
                                   Serve the admin port with NO authentication
                                   (also BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH=1).

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
        let file = BaeConfigFile::load(Some(&path)).unwrap();
        let (mcp, providers) = build_registries(&file).unwrap();
        assert!(mcp.is_empty());
        assert!(providers.is_empty());
        assert!(!file.telemetry_config().unwrap().enabled);
        // Neither source set → None → empty.
        let file = BaeConfigFile::load(None).unwrap();
        let (mcp, providers) = build_registries(&file).unwrap();
        assert!(mcp.is_empty());
        assert!(providers.is_empty());
        assert!(!file.telemetry_config().unwrap().enabled);
    }
}
