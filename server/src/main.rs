//! Thin entrypoint for the `baesrv` binary.
//!
//! All logic lives in the library ([`baesrv`]); this just delegates to the CLI
//! runner and returns its process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    baesrv::cli::run()
}
