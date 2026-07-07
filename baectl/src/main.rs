//! Thin entrypoint for the `baectl` binary.
//!
//! All logic lives in the library ([`baectl`]); this just delegates to the CLI
//! runner and returns its process exit code (mirrors `server/src/main.rs`).

use std::process::ExitCode;

fn main() -> ExitCode {
    baectl::cli::run()
}
