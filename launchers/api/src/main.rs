//! Thin entrypoint for the `baeapi` binary.
//!
//! All logic lives in the library ([`launcher_api`]); this just delegates to the
//! runner and returns its process exit code (per `aspec/uxui/cli.md`: 0 success,
//! 1 runtime error, 2 usage error).

use std::process::ExitCode;

fn main() -> ExitCode {
    launcher_api::run()
}
