//! Command-line entrypoint glue.
//!
//! `main.rs` calls [`run`]; everything here — argument parsing, tracing setup,
//! the tokio runtime, and mapping errors to exit codes — lives in the library so
//! it stays testable. Per `aspec/uxui/cli.md`: exit 0 = success, 1 = runtime
//! error, 2 = usage error; logs go to stderr, command results to stdout.
//!
//! Subcommands implemented at this stage: the default `serve`, plus `migrate`
//! and `version`. `serve` is assumed when no subcommand is given.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Parse arguments, run the selected subcommand, and return a process exit code.
pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let subcommand = args.first().map(String::as_str);

    match subcommand {
        None | Some("serve") => run_serve(),
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

fn run_serve() -> ExitCode {
    let config = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    init_tracing(&config.log);

    // Fail fast on database problems before binding any port.
    let store = match crate::open_store(&config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::from(e.exit_code() as u8);
        }
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

    match runtime.block_on(crate::serve(config, store)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
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
    baesrv [COMMAND]

COMMANDS:
    serve      Run the HTTP server (default)
    migrate    Apply pending database migrations and exit
    version    Print version and supported API versions
    help       Print this message

Configuration is via BAE_* environment variables; see docs/ and
aspec/devops/operations.md.",
        crate::VERSION
    );
}
