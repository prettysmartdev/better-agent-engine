//! Shared container-engine plumbing used by both `setup` and the
//! `build`/`ready`/`run` harness verbs.
//!
//! All four commands are *host-invoked* tools that drive a local container
//! engine from outside — they never link an admin client against the
//! loopback-only admin port. Instead they run `baectl …` **inside** the running
//! container (`docker compose exec <service> baectl …` / `container exec
//! <name> baectl …`), the same admin auto-configuration path that works with
//! zero flags in-container.
//!
//! This module owns the three pieces that were first written for `setup` and
//! that `build`/`ready`/`run` need again, extracted here so there is a single
//! implementation of each:
//!
//! 1. [`Engine`] — the `docker compose exec` / `container exec` subprocess
//!    wrapper ([`Engine::exec_baectl`]).
//! 2. [`EngineKind`] + [`detect_engine`] — which engine a scaffolded `--dir`
//!    targets, read from which launcher file it holds (`docker-compose.yml`
//!    vs. `bae-setup.sh`).
//! 3. [`Prompter`] + [`extract_env_var`] — the minimal stdin/stdout prompter
//!    and the `${VAR}` extractor behind interactive secret collection.

#[cfg(test)]
use std::cell::{Cell, RefCell};
#[cfg(test)]
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use crate::error::CliError;

// -- Launcher file names -----------------------------------------------------

/// The compose launcher `setup` writes (default output mode).
pub const COMPOSE_FILE: &str = "docker-compose.yml";
/// The Apple `container` launcher script `setup --apple` writes.
pub const APPLE_SCRIPT: &str = "bae-setup.sh";

// -- Container engine --------------------------------------------------------

/// A container engine paired with the name of the running BAE service/container
/// to target. Constructed from a [`Variant`](crate::setup)-derived name plus the
/// [`EngineKind`] a `--dir` was scaffolded for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// `docker compose exec -T <service> …`; `service` is the compose service
    /// name (`baesrv` / `bae-max`).
    Docker { service: String },
    /// Apple's `container exec <name> …`; `container` is the running container
    /// name (`bae` / `bae-max`).
    Apple { container: String },
}

impl Engine {
    /// Docker-compose engine targeting compose service `service`.
    pub fn docker(service: impl Into<String>) -> Engine {
        Engine::Docker {
            service: service.into(),
        }
    }

    /// Apple `container` engine targeting the running container `container`.
    pub fn apple(container: impl Into<String>) -> Engine {
        Engine::Apple {
            container: container.into(),
        }
    }

    /// Run `baectl <args>` inside the running container and return its stdout.
    ///
    /// The admin API is loopback-only *inside* the container and the generated
    /// launcher never publishes port `8081`, so admin calls are made by exec'ing
    /// the in-container `baectl` (which auto-configures its address + token),
    /// never by a host-side request against an unreachable port.
    pub fn exec_baectl(&self, dir: &Path, args: &[&str]) -> Result<String, CliError> {
        let mut cmd = match self {
            Engine::Apple { container } => {
                let mut c = Command::new("container");
                c.arg("exec").arg(container).arg("baectl");
                c
            }
            Engine::Docker { service } => {
                let mut c = Command::new("docker");
                c.args(["compose", "exec", "-T", service]).arg("baectl");
                c
            }
        };
        cmd.args(args).current_dir(dir);
        let output = cmd.output().map_err(|e| {
            CliError::runtime(format!("failed to exec baectl in the container: {e}"))
        })?;
        if !output.status.success() {
            return Err(CliError::runtime(format!(
                "in-container `baectl {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Which container engine a scaffolded `--dir` targets, independent of the
/// service/container name (which the caller derives from the image variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// A `docker-compose.yml` launcher is present.
    Docker,
    /// A `bae-setup.sh` (Apple `container`) launcher is present.
    Apple,
}

/// Detect which engine a `--dir` was scaffolded for from its launcher file:
/// `docker-compose.yml` → [`EngineKind::Docker`], `bae-setup.sh` →
/// [`EngineKind::Apple`]. Returns `None` when neither launcher is present
/// (i.e. no `baectl setup` has been run in `dir`).
pub fn detect_engine(dir: &Path) -> Option<EngineKind> {
    if dir.join(COMPOSE_FILE).exists() {
        Some(EngineKind::Docker)
    } else if dir.join(APPLE_SCRIPT).exists() {
        Some(EngineKind::Apple)
    } else {
        None
    }
}

// -- `${VAR}` extraction -----------------------------------------------------

/// Parse `${VAR}` out of a value like `${ANTHROPIC_API_KEY}` or
/// `Bearer ${GITHUB_TOKEN}`; returns the first bare variable name found.
pub fn extract_env_var(value: &str) -> Option<String> {
    let start = value.find("${")? + 2;
    let end = value[start..].find('}')? + start;
    Some(value[start..end].to_string())
}

// -- Interactive prompter ----------------------------------------------------

/// Minimal stdin/stdout prompter — no prompting crate (the project's stated
/// minimal-dependency preference). When stdin is not a TTY, every question
/// silently resolves to its default (equivalent to hitting enter through the
/// whole wizard); the launch question is the one documented exception.
pub struct Prompter {
    pub(crate) interactive: bool,
    /// Kept as a test-visible signal so non-interactive tests can prove that
    /// no question was rendered. Production does not need to count prompts.
    #[cfg(test)]
    pub(crate) prompt_count: Cell<usize>,
    /// Unit tests supply a finite transcript while preserving the exact
    /// validation/re-prompt code used by a terminal invocation.
    #[cfg(test)]
    scripted_answers: RefCell<Option<VecDeque<String>>>,
}

impl Prompter {
    pub fn new() -> Prompter {
        Prompter {
            interactive: io::stdin().is_terminal(),
            #[cfg(test)]
            prompt_count: Cell::new(0),
            #[cfg(test)]
            scripted_answers: RefCell::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn scripted(answers: &[&str]) -> Prompter {
        Prompter {
            interactive: true,
            prompt_count: Cell::new(0),
            scripted_answers: RefCell::new(Some(
                answers.iter().map(|answer| (*answer).to_string()).collect(),
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn non_interactive() -> Prompter {
        Prompter {
            interactive: false,
            prompt_count: Cell::new(0),
            scripted_answers: RefCell::new(None),
        }
    }

    #[cfg(test)]
    fn record_prompt(&self) {
        self.prompt_count.set(self.prompt_count.get() + 1);
    }

    #[cfg(test)]
    fn next_scripted_answer(&self) -> Option<String> {
        self.scripted_answers
            .borrow_mut()
            .as_mut()
            .and_then(VecDeque::pop_front)
    }

    /// Read one line, returning the shown default on a bare enter / EOF. In
    /// non-interactive mode the default is returned without printing anything.
    pub fn ask_line(&self, question: &str, default: &str) -> String {
        if !self.interactive {
            return default.to_string();
        }
        #[cfg(test)]
        self.record_prompt();
        #[cfg(test)]
        if self.scripted_answers.borrow().is_some() {
            return self.next_scripted_answer().unwrap_or_default();
        }
        print!("{question} [{default}]: ");
        let _ = io::stdout().flush();
        let mut buf = String::new();
        match io::stdin().read_line(&mut buf) {
            Ok(0) | Err(_) => default.to_string(),
            Ok(_) => {
                let t = buf.trim();
                if t.is_empty() {
                    default.to_string()
                } else {
                    t.to_string()
                }
            }
        }
    }

    /// Ask a validated free-form question, re-prompting on invalid input rather
    /// than aborting. Defaults are always valid by construction, so the
    /// non-interactive path (which only ever yields the default) validates once
    /// and returns; it never loops.
    pub fn ask_validated<T>(
        &self,
        question: &str,
        default: &str,
        mut validate: impl FnMut(&str) -> Result<T, String>,
    ) -> T {
        loop {
            let answer = self.ask_line(question, default);
            match validate(&answer) {
                Ok(v) => return v,
                // In non-interactive mode `answer` is always the default, which
                // is valid by construction; re-prompting would loop forever, so
                // fall back to the raw default string as the value is unusable
                // only if a caller passed an invalid default (a programmer bug).
                Err(_) if !self.interactive => {
                    // Retry once against the default; if the default is itself
                    // invalid this is a bug, but we must not hang a CI run.
                    return validate(default)
                        .unwrap_or_else(|msg| panic!("invalid non-interactive default: {msg}"));
                }
                Err(msg) => eprintln!("  {msg}"),
            }
        }
    }

    /// Ask a `[y/N]`-style question. Non-interactive resolves to `default`.
    pub fn ask_yes_no(&self, question: &str, default_yes: bool) -> bool {
        if !self.interactive {
            return default_yes;
        }
        let hint = if default_yes { "Y/n" } else { "y/N" };
        loop {
            #[cfg(test)]
            self.record_prompt();
            #[cfg(test)]
            if self.scripted_answers.borrow().is_some() {
                match self
                    .next_scripted_answer()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "" => return default_yes,
                    "y" | "yes" => return true,
                    "n" | "no" => return false,
                    _ => continue,
                }
            }
            print!("{question} [{hint}]: ");
            let _ = io::stdout().flush();
            let mut buf = String::new();
            match io::stdin().read_line(&mut buf) {
                Ok(0) | Err(_) => return default_yes,
                Ok(_) => match buf.trim().to_ascii_lowercase().as_str() {
                    "" => return default_yes,
                    "y" | "yes" => return true,
                    "n" | "no" => return false,
                    _ => eprintln!("  please answer y or n"),
                },
            }
        }
    }
}

impl Default for Prompter {
    fn default() -> Self {
        Prompter::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_env_var_finds_first_bare_variable() {
        assert_eq!(
            extract_env_var("${ANTHROPIC_API_KEY}").as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(
            extract_env_var("Bearer ${GITHUB_TOKEN}").as_deref(),
            Some("GITHUB_TOKEN")
        );
        assert_eq!(extract_env_var("no placeholder here"), None);
        assert_eq!(extract_env_var("${unterminated"), None);
    }

    #[test]
    fn detect_engine_reads_the_launcher_file() {
        let dir = std::env::temp_dir().join(format!("baectl-engine-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(detect_engine(&dir), None);

        std::fs::write(dir.join(COMPOSE_FILE), "services: {}\n").unwrap();
        assert_eq!(detect_engine(&dir), Some(EngineKind::Docker));

        std::fs::remove_file(dir.join(COMPOSE_FILE)).unwrap();
        std::fs::write(dir.join(APPLE_SCRIPT), "#!/bin/sh\n").unwrap();
        assert_eq!(detect_engine(&dir), Some(EngineKind::Apple));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_constructors_carry_the_target_name() {
        assert_eq!(
            Engine::docker("baesrv"),
            Engine::Docker {
                service: "baesrv".to_string()
            }
        );
        assert_eq!(
            Engine::apple("bae-max"),
            Engine::Apple {
                container: "bae-max".to_string()
            }
        );
    }

    #[test]
    fn non_interactive_prompter_returns_defaults_without_asking() {
        let p = Prompter::non_interactive();
        assert_eq!(p.ask_line("Q?", "the-default"), "the-default");
        assert!(p.ask_yes_no("Y?", true));
        assert!(!p.ask_yes_no("Y?", false));
        assert_eq!(p.prompt_count.get(), 0);
    }

    #[test]
    fn scripted_prompter_consumes_answers_in_order() {
        let p = Prompter::scripted(&["typed", "y"]);
        assert_eq!(p.ask_line("Q?", "def"), "typed");
        assert!(p.ask_yes_no("Y?", false));
        assert_eq!(p.prompt_count.get(), 2);
    }
}
