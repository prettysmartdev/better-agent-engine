//! `baectl setup` — the interactive quickstart wizard.
//!
//! `setup` is the one `baectl` command that is a *local scaffolding tool*: it
//! runs before a server exists to talk to, asks a short series of stdin/stdout
//! Q&A questions (each defaulted, so a bare `enter` walks the whole wizard), and
//! writes three files into `--dir`:
//!
//! 1. a launcher — `docker-compose.yml` (default) or, with `--apple`, a
//!    `bae-setup.sh` script driving Apple's `container` CLI;
//! 2. a `.env` file (mode `0600`) holding every secret and every non-default
//!    `BAE_*` override, referenced by the launcher;
//! 3. a `bae-config.toml` holding the `[providers]`/`[mcp]` registries.
//!
//! At the very end it may *launch* the configuration and, on a fresh setup,
//! create a first profile + client key. That launch step is the only time
//! `setup` drives a live server — and it does so via `docker exec`/`container
//! exec` running `baectl create profile/key` **inside** the container, never by
//! connecting to the loopback-only admin port from the host (see below).
//!
//! # `bae-config.toml` serialization — the dependency-boundary decision
//!
//! The generated `bae-config.toml` must match `server`'s `config_file.rs`
//! shape exactly (`[providers]`/`[[providers.entries]]`, `[mcp]`/
//! `[[mcp.servers]]`). Two options existed:
//!
//! - **(a)** depend on `server`'s `config_file::BaeConfig` types directly and
//!   `toml::to_string` them. Rejected: it crosses a crate boundary `baectl`
//!   has deliberately avoided (see `lib.rs`), and the top-level `BaeConfig`/
//!   `McpConfig`/`ProvidersConfig` structs derive only `Deserialize`, not
//!   `Serialize`, so it would *also* require widening `server`'s public derives.
//! - **(b)** *(chosen)* hand-build the TOML string field-by-field here, with no
//!   runtime dependency on `server`. A test-only round-trip against `server`'s
//!   own `BaeConfig` deserializer (added by the tests step) guards against
//!   silent schema drift. The `toml` crate is pulled in only to *read back* an
//!   existing config on the idempotent Edit path, never to write one.
//!
//! Consequently the wizard emits `[mcp]`/`[providers]` only and **never** a
//! `[telemetry]` table: an absent section keeps telemetry disabled
//! (`config_file.rs`'s "absent → `TelemetryConfig::default()`" contract), and
//! the generated file still deserializes cleanly into the current three-field
//! `BaeConfig`.
//!
//! # Admin-port reachability
//!
//! The admin API is loopback-only *inside* the container by design and the
//! generated launcher deliberately never publishes port `8081`. So `setup`'s
//! post-launch profile/key creation runs `baectl create profile`/`create key`
//! **inside** the container (`docker compose exec <service> …` /
//! `container exec <name> …`) — the same admin auto-configuration path that
//! already works with zero flags in-container — and parses its `--json` output,
//! rather than building a host-side request against an unreachable port.

#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::error::CliError;

// -- Image tags -------------------------------------------------------------

/// Published GHCR standard tag (`aspec/uxui/setup.md`'s "User installation").
const PUBLISHED_STANDARD: &str = "ghcr.io/prettysmartdev/better-agent-engine:latest";
/// Published GHCR max tag.
const PUBLISHED_MAX: &str = "ghcr.io/prettysmartdev/better-agent-engine:max";
/// Local `make image` tag (root `Makefile`'s `IMAGE`).
const DEV_STANDARD: &str = "better-agent-engine:latest";
/// Local `make image-max` tag (root `Makefile`'s `MAX_IMAGE`).
const DEV_MAX: &str = "better-agent-engine:max";

// -- Fixed container-internal paths / names ---------------------------------

/// The container-internal config path the launcher bind-mounts `bae-config.toml`
/// onto, and bakes into the service environment as `BAE_CONFIG`.
const CONTAINER_CONFIG_PATH: &str = "/etc/bae/config.toml";
/// The named data volume both launchers mount at `/var/lib/bae`.
const DATA_VOLUME: &str = "bae-data";

/// The three generated file names (the launcher varies by output mode).
const COMPOSE_FILE: &str = "docker-compose.yml";
const APPLE_SCRIPT: &str = "bae-setup.sh";
const ENV_FILE: &str = ".env";
const CONFIG_FILE: &str = "bae-config.toml";

/// The documented `BAE_*` questions of step 5 and their server defaults
/// (`docs/reference/configuration.md`'s Environment Variables table). Only a
/// value the user actually changes from its default is written to `.env`.
const BAE_ENV_DEFAULTS: &[(&str, &str)] = &[
    ("BAE_ADDR", "0.0.0.0:8080"),
    ("BAE_LOG", "info"),
    ("BAE_SHUTDOWN_TIMEOUT", "30"),
    ("BAE_TURN_TIMEOUT", "120"),
    ("BAE_SANDBOX_DRIVER", "docker"),
];

/// The host port the launcher publishes and `setup` polls `/healthz` on. The
/// container always listens on `8080` internally (the image default); the
/// compose/script publish uses `${BAE_ADDR_PORT:-8080}` so an operator can
/// remap the *host* side via `.env` without editing the launcher.
const CLIENT_PORT: u16 = 8080;

// -- Wizard data model ------------------------------------------------------

/// The image variant (and therefore the service/container name and ports).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Standard,
    Max,
}

impl Variant {
    fn as_str(self) -> &'static str {
        match self {
            Variant::Standard => "standard",
            Variant::Max => "max",
        }
    }

    /// The compose service name (compose derives the container name from it; we
    /// pin no `container_name`, avoiding a clash with the Makefile's `bae`/
    /// `bae-max`). Also used as the `docker compose exec` target.
    fn service_name(self) -> &'static str {
        match self {
            Variant::Standard => "baesrv",
            Variant::Max => "bae-max",
        }
    }

    /// The `--name` given to the Apple `container run`, and the `container exec`
    /// target. The generated script stops/removes any prior container of this
    /// name first, so a re-run is idempotent.
    fn apple_container_name(self) -> &'static str {
        match self {
            Variant::Standard => "bae",
            Variant::Max => "bae-max",
        }
    }
}

/// A provider `[[providers.entries]]` under construction.
#[derive(Debug, Clone)]
enum ProviderKind {
    Anthropic,
    OpenAi,
}

impl ProviderKind {
    fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::OpenAi => "openai",
        }
    }

    fn default_model(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "sonnet-5",
            ProviderKind::OpenAi => "gpt-5.6-luna",
        }
    }

    fn default_auth_env(&self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
            ProviderKind::OpenAi => "OPENAI_API_KEY",
        }
    }

    fn parse(s: &str) -> Option<ProviderKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(ProviderKind::Anthropic),
            "openai" => Some(ProviderKind::OpenAi),
            _ => None,
        }
    }
}

/// One registered provider. The secret value (if supplied) lives in the shared
/// `.env` map, keyed by `auth_env`; here we keep only the `${VAR}` reference.
#[derive(Debug, Clone)]
struct Provider {
    name: String,
    kind: ProviderKind,
    model: String,
    /// The auth-token env var *name* (stored in the config as `${auth_env}`).
    auth_env: String,
}

/// One registered MCP server. Any secret it needs (e.g. `GITHUB_TOKEN`) lives
/// in the shared `.env` map; `headers`/`command`/`url` carry only the `${VAR}`
/// references.
#[derive(Debug, Clone)]
struct McpServer {
    name: String,
    transport: String,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
}

/// The complete set of wizard answers, everything the file generators need.
#[derive(Debug)]
struct SetupConfig {
    variant: Variant,
    dev: bool,
    apple: bool,
    providers: Vec<Provider>,
    mcp: Vec<McpServer>,
    /// Non-default `BAE_*` overrides in the fixed step-5 order.
    bae_overrides: Vec<(String, String)>,
    /// Secret `KEY=value` lines (provider auth tokens, MCP secrets,
    /// `BAE_MAX_PASSWORD`). Only keys the user supplied a value for appear.
    secrets: BTreeMap<String, String>,
    /// Provider/MCP secret env vars the user declined *and* that were not in the
    /// process environment — warned about once, absent from `.env`.
    unresolved: Vec<String>,
    /// Host port MAX's dashboard is published on (max variant only).
    max_port: u16,
    /// Whether the user left `BAE_MAX_PASSWORD` blank (max self-generates).
    max_password_blank: bool,
    /// The port the server listens on *inside* the container (the port half of
    /// `BAE_ADDR`, default `8080`). The launcher publishes and `setup` polls
    /// `/healthz` on this port, so a non-default `BAE_ADDR` port stays coherent
    /// with the published mapping and the health check.
    client_port: u16,
}

impl SetupConfig {
    fn image_tag(&self) -> &'static str {
        match (self.variant, self.dev) {
            (Variant::Standard, false) => PUBLISHED_STANDARD,
            (Variant::Max, false) => PUBLISHED_MAX,
            (Variant::Standard, true) => DEV_STANDARD,
            (Variant::Max, true) => DEV_MAX,
        }
    }
}

// -- Prompt helpers ---------------------------------------------------------

/// Minimal stdin/stdout prompter — no prompting crate (the project's stated
/// minimal-dependency preference). When stdin is not a TTY, every question
/// silently resolves to its default (equivalent to hitting enter through the
/// whole wizard); the launch question is the one documented exception.
struct Prompter {
    interactive: bool,
    /// Kept as a test-visible signal so non-interactive tests can prove that
    /// no question was rendered. Production does not need to count prompts.
    #[cfg(test)]
    prompt_count: Cell<usize>,
    /// Unit tests supply a finite transcript while preserving the exact
    /// validation/re-prompt code used by a terminal invocation.
    #[cfg(test)]
    scripted_answers: RefCell<Option<VecDeque<String>>>,
}

impl Prompter {
    fn new() -> Prompter {
        Prompter {
            interactive: io::stdin().is_terminal(),
            #[cfg(test)]
            prompt_count: Cell::new(0),
            #[cfg(test)]
            scripted_answers: RefCell::new(None),
        }
    }

    #[cfg(test)]
    fn scripted(answers: &[&str]) -> Prompter {
        Prompter {
            interactive: true,
            prompt_count: Cell::new(0),
            scripted_answers: RefCell::new(Some(
                answers.iter().map(|answer| (*answer).to_string()).collect(),
            )),
        }
    }

    #[cfg(test)]
    fn non_interactive() -> Prompter {
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
    fn ask_line(&self, question: &str, default: &str) -> String {
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
    fn ask_validated<T>(
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
    fn ask_yes_no(&self, question: &str, default_yes: bool) -> bool {
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

// -- Existing-file parsing (idempotency summary + Edit pre-fill) -------------

/// A subset of `bae-config.toml`, deserialized to summarize a saved config and
/// to pre-fill Edit-path defaults. Only the fields the wizard itself writes are
/// modeled; unknown fields are ignored.
#[derive(Debug, Default, Deserialize)]
struct ExistingConfig {
    #[serde(default)]
    mcp: Option<ExistingMcp>,
    #[serde(default)]
    providers: Option<ExistingProviders>,
}

#[derive(Debug, Default, Deserialize)]
struct ExistingMcp {
    #[serde(default)]
    servers: Vec<ExistingServer>,
}

#[derive(Debug, Deserialize)]
struct ExistingServer {
    name: String,
    transport: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct ExistingProviders {
    #[serde(default)]
    entries: Vec<ExistingProvider>,
}

#[derive(Debug, Deserialize)]
struct ExistingProvider {
    name: String,
    provider: String,
    model: String,
    auth_token: String,
}

/// Pre-fill values recovered from an existing setup, used to seed Edit-path
/// defaults so accepting every default reproduces the current config unchanged.
struct Prefill {
    variant: Variant,
    providers: Vec<Provider>,
    mcp: Vec<McpServer>,
    /// `BAE_*` values previously written to `.env` (their current values).
    bae_overrides: BTreeMap<String, String>,
    /// Secret `KEY=value` pairs previously written to `.env`.
    secrets: BTreeMap<String, String>,
    max_port: Option<u16>,
}

/// Parse the port half of a `BAE_ADDR` (`<ip>:<port>`), returning `None` if it
/// is not a valid socket address.
fn bae_addr_port(addr: &str) -> Option<u16> {
    addr.trim()
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|a| a.port())
}

/// Validate a `BAE_ADDR` value as a parseable `<ip>:<port>` listen address,
/// returning the raw (trimmed) string plus the port to publish/health-check on.
fn validate_bae_addr(value: &str) -> Result<(String, u16), String> {
    let value = value.trim();
    match value.parse::<std::net::SocketAddr>() {
        Ok(addr) => Ok((value.to_string(), addr.port())),
        Err(_) => Err(format!(
            "{value:?} is not a valid <ip>:<port> listen address (e.g. 0.0.0.0:8080)"
        )),
    }
}

/// Validate a portable environment-variable identifier: a leading letter or
/// underscore, then letters/digits/underscores. Keeps the generated `${VAR}`
/// reference and `.env` line syntactically valid.
fn validate_env_ident(value: &str) -> Result<String, String> {
    let value = value.trim();
    let mut chars = value.chars();
    let head_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if head_ok && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(value.to_string())
    } else {
        Err(format!(
            "{value:?} is not a valid environment variable name \
             (letters, digits, and underscores; not starting with a digit)"
        ))
    }
}

/// Validate that a free-form field is non-empty (after trimming).
fn validate_non_empty(field: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(value.to_string())
    }
}

/// Parse `${VAR}` out of a value like `${ANTHROPIC_API_KEY}` or
/// `Bearer ${GITHUB_TOKEN}`; returns the first bare variable name found.
fn extract_env_var(value: &str) -> Option<String> {
    let start = value.find("${")? + 2;
    let end = value[start..].find('}')? + start;
    Some(value[start..end].to_string())
}

/// Read and parse `.env` into (BAE_* overrides, other secrets). Keys are split
/// on the first `=`; comment and blank lines are skipped.
fn parse_env_file(text: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let mut bae = BTreeMap::new();
    let mut secrets = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_string(), v.trim().to_string());
        // Although it has a BAE_ prefix, MAX's password is a credential, not
        // a regular step-5 server override. Preserve it in the secret map so
        // an Edit run can keep it without displaying its value.
        if k == "BAE_MAX_PASSWORD" {
            secrets.insert(k, v);
        } else if k.starts_with("BAE_") {
            bae.insert(k, v);
        } else {
            secrets.insert(k, v);
        }
    }
    (bae, secrets)
}

/// Recover a [`Prefill`] from the on-disk `bae-config.toml`/`.env`/launcher.
/// A parse failure surfaces as a runtime error (the files are corrupted).
fn load_prefill(dir: &Path, apple: bool) -> Result<Prefill, CliError> {
    let config_text = read_to_string(&dir.join(CONFIG_FILE))?;
    let cfg: ExistingConfig = toml::from_str(&config_text).map_err(|e| {
        CliError::runtime(format!(
            "existing {CONFIG_FILE} is not valid TOML (use a fresh directory or delete it): {e}"
        ))
    })?;

    let providers = cfg
        .providers
        .unwrap_or_default()
        .entries
        .into_iter()
        .map(|p| Provider {
            kind: ProviderKind::parse(&p.provider).unwrap_or(ProviderKind::Anthropic),
            auth_env: extract_env_var(&p.auth_token).unwrap_or_else(|| p.auth_token.clone()),
            name: p.name,
            model: p.model,
        })
        .collect();

    let mcp = cfg
        .mcp
        .unwrap_or_default()
        .servers
        .into_iter()
        .map(|s| McpServer {
            name: s.name,
            transport: s.transport,
            command: s.command,
            args: s.args,
            url: s.url,
            headers: s.headers,
        })
        .collect();

    let (bae_overrides, secrets) = match read_to_string(&dir.join(ENV_FILE)) {
        Ok(text) => parse_env_file(&text),
        Err(_) => (BTreeMap::new(), BTreeMap::new()),
    };

    // Recover the variant + MAX port from the launcher text (the config file
    // itself does not encode the image variant).
    let launcher_name = if apple { APPLE_SCRIPT } else { COMPOSE_FILE };
    let launcher_text = read_to_string(&dir.join(launcher_name)).unwrap_or_default();
    let (variant, max_port) = if launcher_text.contains(":3000") {
        (Variant::Max, Some(parse_max_port(&launcher_text)))
    } else {
        (Variant::Standard, None)
    };

    Ok(Prefill {
        variant,
        providers,
        mcp,
        bae_overrides,
        secrets,
        max_port,
    })
}

/// Best-effort recovery of the MAX host port from a launcher's `<port>:3000`
/// publish entry; defaults to `3000` if not found.
fn parse_max_port(launcher_text: &str) -> u16 {
    for token in launcher_text.split(|c: char| !c.is_ascii_digit() && c != ':') {
        if let Some((host, "3000")) = token.split_once(':') {
            if let Ok(p) = host.parse::<u16>() {
                return p;
            }
        }
    }
    3000
}

// -- Wizard flow ------------------------------------------------------------

/// Entry point routed from `cli::dispatch`.
pub fn run(dev_flag: bool, apple_flag: bool, dir: &Path) -> Result<(), CliError> {
    // `--dir` must exist and be writable *before* any prompting (exit 1).
    validate_dir(dir)?;

    let prompt = Prompter::new();

    // `--dev`/`--apple` are answerable interactively (per WI 0012): a passed
    // flag pre-fills and skips its question, while omitting the flag asks a
    // defaulted question (published image / compose launcher). These resolve
    // *before* the idempotency check below because the output mode decides
    // which launcher file that check looks for.
    let apple = resolve_flag(
        &prompt,
        apple_flag,
        "Use Apple's `container` CLI (bae-setup.sh) instead of docker-compose?",
    );
    let dev = resolve_flag(
        &prompt,
        dev_flag,
        "Use locally-built (`make image`) image tags instead of the published ones?",
    );

    let launcher_name = if apple { APPLE_SCRIPT } else { COMPOSE_FILE };
    let other_launcher = if apple { COMPOSE_FILE } else { APPLE_SCRIPT };

    let has_launcher = dir.join(launcher_name).exists();
    let has_other = dir.join(other_launcher).exists();
    let has_env = dir.join(ENV_FILE).exists();
    let has_config = dir.join(CONFIG_FILE).exists();
    let present = [has_launcher, has_env, has_config]
        .iter()
        .filter(|b| **b)
        .count();

    // Idempotency (step 1). A launcher matching the *other* mode is a
    // flag/artifact mismatch → treat as corrupted partial state.
    if has_other {
        return handle_partial(
            &prompt,
            dir,
            dev,
            apple,
            &format!(
                "found {other_launcher} but this run uses {launcher_name} \
                 (launcher/flag mismatch)"
            ),
        );
    }
    if present == 3 {
        return handle_existing(&prompt, dir, dev, apple);
    }
    if present != 0 {
        let missing: Vec<&str> = [
            (has_launcher, launcher_name),
            (has_env, ENV_FILE),
            (has_config, CONFIG_FILE),
        ]
        .into_iter()
        .filter(|(present, _)| !*present)
        .map(|(_, name)| name)
        .collect();
        return handle_partial(
            &prompt,
            dir,
            dev,
            apple,
            &format!("incomplete setup — missing {}", missing.join(", ")),
        );
    }

    // Fresh setup: no pre-fill.
    let config = run_wizard(&prompt, dev, apple, None)?;
    write_all_files(dir, &config)?;
    finish(&prompt, dir, &config, true)
}

/// Resolve a `--dev`/`--apple`-style boolean: a passed flag forces `true` and
/// skips the question; otherwise ask a defaulted (`N`) yes/no question, which
/// in non-interactive mode silently returns `false` (the flag's absence).
fn resolve_flag(prompt: &Prompter, passed: bool, question: &str) -> bool {
    passed || prompt.ask_yes_no(question, false)
}

/// Existing (all three files) → Launch-or-Edit.
fn handle_existing(prompt: &Prompter, dir: &Path, dev: bool, apple: bool) -> Result<(), CliError> {
    let prefill = load_prefill(dir, apple)?;
    print_summary(&prefill);

    // Two choices only. Non-interactive resolves to Launch (reuse verbatim) —
    // never a wizard, never an overwrite.
    let edit = prompt.ask_yes_no(
        "Edit this configuration? (No = launch the saved config as-is)",
        false,
    );
    if !edit {
        // Launch path: reuse files verbatim, no profile/key creation.
        let config = config_from_prefill(prefill, dev, apple);
        return finish(prompt, dir, &config, false);
    }

    // Edit path: back up, re-run the wizard pre-filled, regenerate, launch.
    backup_existing(dir, apple)?;
    let config = run_wizard(prompt, dev, apple, Some(prefill))?;
    write_all_files(dir, &config)?;
    finish(prompt, dir, &config, true)
}

/// Partial / mismatched state → warn, require confirmation, then fresh overwrite.
fn handle_partial(
    prompt: &Prompter,
    dir: &Path,
    dev: bool,
    apple: bool,
    reason: &str,
) -> Result<(), CliError> {
    eprintln!("baectl: {reason}.");
    eprintln!(
        "        This looks like a corrupted or incomplete setup. Continuing will \
         overwrite whatever partial state is present in {}.",
        dir.display()
    );
    if !prompt.ask_yes_no("Overwrite and run a fresh setup?", false) {
        eprintln!("baectl: aborted; no files were changed.");
        return Ok(());
    }
    let config = run_wizard(prompt, dev, apple, None)?;
    write_all_files(dir, &config)?;
    finish(prompt, dir, &config, true)
}

/// Print the saved-configuration summary shown before the Launch/Edit choice.
fn print_summary(prefill: &Prefill) {
    println!("Found an existing setup:");
    println!("  image variant: {}", prefill.variant.as_str());
    let providers: Vec<&str> = prefill.providers.iter().map(|p| p.name.as_str()).collect();
    println!(
        "  providers:     {}",
        if providers.is_empty() {
            "(none)".to_string()
        } else {
            providers.join(", ")
        }
    );
    let mcp: Vec<&str> = prefill.mcp.iter().map(|s| s.name.as_str()).collect();
    println!(
        "  MCP servers:   {}",
        if mcp.is_empty() {
            "(none)".to_string()
        } else {
            mcp.join(", ")
        }
    );
}

/// Reconstruct a [`SetupConfig`] from a [`Prefill`] for the verbatim re-launch
/// path (no questions, files reused as-is). Only the fields `finish` needs are
/// meaningful here.
fn config_from_prefill(prefill: Prefill, dev: bool, apple: bool) -> SetupConfig {
    // The saved `.env` may carry a non-default `BAE_ADDR`; poll/publish the port
    // it actually names so the verbatim re-launch stays coherent.
    let client_port = prefill
        .bae_overrides
        .get("BAE_ADDR")
        .and_then(|addr| bae_addr_port(addr))
        .unwrap_or(CLIENT_PORT);
    SetupConfig {
        variant: prefill.variant,
        dev,
        apple,
        providers: prefill.providers,
        mcp: prefill.mcp,
        bae_overrides: Vec::new(),
        secrets: prefill.secrets,
        unresolved: Vec::new(),
        max_port: prefill.max_port.unwrap_or(3000),
        max_password_blank: false,
        client_port,
    }
}

/// Run the full top-to-bottom wizard (steps 2–5). `prefill`, when present,
/// seeds every default from the existing files (Edit path).
fn run_wizard(
    prompt: &Prompter,
    dev: bool,
    apple: bool,
    prefill: Option<Prefill>,
) -> Result<SetupConfig, CliError> {
    let prefill = prefill.unwrap_or_else(|| Prefill {
        variant: Variant::Standard,
        providers: Vec::new(),
        mcp: Vec::new(),
        bae_overrides: BTreeMap::new(),
        secrets: BTreeMap::new(),
        max_port: None,
    });

    // Step 2 — image variant.
    let variant_default = prefill.variant.as_str();
    let variant = prompt.ask_validated(
        "Image variant? (standard/max)",
        variant_default,
        |s| match s.trim().to_ascii_lowercase().as_str() {
            "standard" => Ok(Variant::Standard),
            "max" => Ok(Variant::Max),
            other => Err(format!("unknown variant {other:?}; choose standard or max")),
        },
    );

    let mut secrets: BTreeMap<String, String> = BTreeMap::new();
    let mut unresolved: Vec<String> = Vec::new();

    // Step 3 — providers (at least one required).
    let providers = ask_providers(prompt, &prefill, &mut secrets, &mut unresolved);

    // Step 4 — MCP servers (zero is valid).
    let mcp = ask_mcp_servers(prompt, &prefill, &mut secrets, &mut unresolved);

    // Step 5 — other BAE_* env vars. Each value is validated before it is
    // accepted (re-prompting interactively) so a malformed listen address,
    // sandbox driver, or timeout can't reach server startup. `BAE_ADDR`
    // additionally sets the container listen/publish/health-check port.
    let mut bae_overrides: Vec<(String, String)> = Vec::new();
    let mut client_port = CLIENT_PORT;
    for (key, default) in BAE_ENV_DEFAULTS {
        let seed = prefill
            .bae_overrides
            .get(*key)
            .map(String::as_str)
            .unwrap_or(default)
            .to_string();
        let question = format!("{key}?");
        let answer = match *key {
            "BAE_ADDR" => {
                let (raw, port) = prompt.ask_validated(&question, &seed, validate_bae_addr);
                client_port = port;
                raw
            }
            "BAE_SANDBOX_DRIVER" => {
                prompt.ask_validated(&question, &seed, |value| match value.trim() {
                    driver @ ("docker" | "apple-container") => Ok(driver.to_string()),
                    other => Err(format!(
                        "{other:?} is not a supported sandbox driver \
                         (choose docker or apple-container)"
                    )),
                })
            }
            "BAE_SHUTDOWN_TIMEOUT" | "BAE_TURN_TIMEOUT" => {
                prompt.ask_validated(&question, &seed, |value| {
                    value
                        .trim()
                        .parse::<u64>()
                        .map(|seconds| seconds.to_string())
                        .map_err(|_| format!("{value:?} is not a whole number of seconds"))
                })
            }
            _ => prompt.ask_line(&question, &seed),
        };
        if answer != *default {
            bae_overrides.push((key.to_string(), answer));
        }
    }

    // Step 5 (max only) — MAX web port + password.
    let mut max_port = 3000u16;
    let mut max_password_blank = true;
    if variant == Variant::Max {
        let port_seed = prefill.max_port.unwrap_or(3000).to_string();
        max_port = prompt.ask_validated("MAX web port?", &port_seed, |s| {
            s.trim()
                .parse::<u16>()
                .map_err(|_| format!("{s:?} is not a valid port number"))
        });
        // Password: blank → MAX self-generates and writes its own file. On an
        // Edit run retain an existing credential by default without printing
        // it, matching provider/MCP secret handling above.
        if let Some(existing) = prefill.secrets.get("BAE_MAX_PASSWORD") {
            if prompt.ask_yes_no("Keep the existing value for BAE_MAX_PASSWORD?", true) {
                secrets.insert("BAE_MAX_PASSWORD".to_string(), existing.clone());
                max_password_blank = false;
            } else {
                let pw = prompt.ask_line(
                    "MAX password? (blank = MAX generates one on first boot)",
                    "",
                );
                if !pw.is_empty() {
                    secrets.insert("BAE_MAX_PASSWORD".to_string(), pw);
                    max_password_blank = false;
                }
            }
        } else {
            let pw = prompt.ask_line(
                "MAX password? (blank = MAX generates one on first boot)",
                "",
            );
            if !pw.is_empty() {
                secrets.insert("BAE_MAX_PASSWORD".to_string(), pw);
                max_password_blank = false;
            }
        }
    }

    Ok(SetupConfig {
        variant,
        dev,
        apple,
        providers,
        mcp,
        bae_overrides,
        secrets,
        unresolved,
        max_port,
        max_password_blank,
        client_port,
    })
}

/// Step 3 — build `[[providers.entries]]`. Always collects one provider (a
/// profile needs a `primary_provider`), then loops "add another?".
fn ask_providers(
    prompt: &Prompter,
    prefill: &Prefill,
    secrets: &mut BTreeMap<String, String>,
    unresolved: &mut Vec<String>,
) -> Vec<Provider> {
    let mut providers: Vec<Provider> = Vec::new();
    let mut index = 0usize;
    loop {
        // The first provider is mandatory; subsequent ones are opt-in.
        if index > 0 && !prompt.ask_yes_no("Add another provider?", false) {
            break;
        }
        if index == 0 && prompt.interactive {
            println!("At least one provider is required (a profile needs a primary provider).");
        }

        let seed = prefill.providers.get(index);

        // Provider kind.
        let kind_default = seed
            .map(|p| p.kind.as_str())
            .unwrap_or("anthropic")
            .to_string();
        let kind: ProviderKind =
            prompt.ask_validated("  Provider kind? (anthropic/openai)", &kind_default, |s| {
                ProviderKind::parse(s).ok_or_else(|| format!("unknown provider kind {s:?}"))
            });

        // Registry name — unique within the wizard loop.
        let name_default = seed
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("{}-default", kind.as_str()));
        let taken: Vec<String> = providers.iter().map(|p| p.name.clone()).collect();
        let name = prompt.ask_validated("  Registry name?", &name_default, |s| {
            let s = s.trim();
            if s.is_empty() {
                return Err("name cannot be empty".to_string());
            }
            if taken.iter().any(|n| n == s) {
                return Err(format!("provider name {s:?} already used; choose another"));
            }
            Ok(s.to_string())
        });

        // Model (non-empty; not validated against a live model list).
        let model_default = seed
            .map(|p| p.model.clone())
            .unwrap_or_else(|| kind.default_model().to_string());
        let model = prompt.ask_validated("  Model?", &model_default, |value| {
            validate_non_empty("model", value)
        });

        // Auth-token env var name — a portable environment-variable identifier
        // so the generated `${VAR}` reference and `.env` line are both valid.
        let auth_default = seed
            .map(|p| p.auth_env.clone())
            .unwrap_or_else(|| kind.default_auth_env().to_string());
        let auth_env = prompt.ask_validated(
            "  Auth token env var name?",
            &auth_default,
            validate_env_ident,
        );

        // Secret value — only if not already exported in this process env.
        collect_secret(prompt, secrets, unresolved, prefill, &auth_env);

        providers.push(Provider {
            name,
            kind,
            model,
            auth_env,
        });
        index += 1;
    }
    providers
}

/// Step 4 — build `[[mcp.servers]]` from the curated pick-list + custom. Zero
/// servers is a valid answer.
fn ask_mcp_servers(
    prompt: &Prompter,
    prefill: &Prefill,
    secrets: &mut BTreeMap<String, String>,
    unresolved: &mut Vec<String>,
) -> Vec<McpServer> {
    let mut servers: Vec<McpServer> = Vec::new();
    let mut index = 0usize;
    loop {
        if !prompt.ask_yes_no("Add an MCP server?", false) {
            break;
        }
        let seed = prefill.mcp.get(index);
        let kind_default = seed.map(classify_mcp).unwrap_or("filesystem").to_string();
        let choice = prompt.ask_validated(
            "  Which? (filesystem/fetch/github/custom)",
            &kind_default,
            |s| match s.trim().to_ascii_lowercase().as_str() {
                "filesystem" | "fetch" | "github" | "custom" => Ok(s.trim().to_ascii_lowercase()),
                other => Err(format!("unknown choice {other:?}")),
            },
        );

        let taken: Vec<String> = servers.iter().map(|s| s.name.clone()).collect();
        let server = match choice.as_str() {
            "filesystem" => build_filesystem(prompt, seed, &taken),
            "fetch" => build_fetch(prompt, seed, &taken),
            "github" => build_github(prompt, seed, &taken, secrets, unresolved, prefill),
            _ => build_custom(prompt, seed, &taken),
        };
        servers.push(server);
        index += 1;
    }
    servers
}

/// Classify an existing server back to its pick-list label, for Edit defaults.
fn classify_mcp(s: &McpServer) -> &'static str {
    if s.name == "filesystem" {
        "filesystem"
    } else if s.name == "fetch" {
        "fetch"
    } else if s.name == "github" {
        "github"
    } else {
        "custom"
    }
}

fn unique_name(prompt: &Prompter, default: &str, taken: &[String]) -> String {
    let owned: Vec<String> = taken.to_vec();
    prompt.ask_validated("  Server name?", default, move |s| {
        let s = s.trim();
        if s.is_empty() {
            return Err("name cannot be empty".to_string());
        }
        if owned.iter().any(|n| n == s) {
            return Err(format!(
                "MCP server name {s:?} already used; choose another"
            ));
        }
        Ok(s.to_string())
    })
}

fn build_filesystem(prompt: &Prompter, seed: Option<&McpServer>, taken: &[String]) -> McpServer {
    let name = unique_name(
        prompt,
        seed.map(|s| s.name.as_str()).unwrap_or("filesystem"),
        taken,
    );
    let dir_default = seed
        .and_then(|s| s.args.last().cloned())
        .unwrap_or_else(|| "/data".to_string());
    let mount = prompt.ask_line("  Directory to expose?", &dir_default);
    McpServer {
        name,
        transport: "stdio".to_string(),
        command: Some("npx".to_string()),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-filesystem".to_string(),
            mount,
        ],
        url: None,
        headers: BTreeMap::new(),
    }
}

fn build_fetch(prompt: &Prompter, seed: Option<&McpServer>, taken: &[String]) -> McpServer {
    let name = unique_name(
        prompt,
        seed.map(|s| s.name.as_str()).unwrap_or("fetch"),
        taken,
    );
    McpServer {
        name,
        transport: "stdio".to_string(),
        command: Some("uvx".to_string()),
        args: vec!["mcp-server-fetch".to_string()],
        url: None,
        headers: BTreeMap::new(),
    }
}

fn build_github(
    prompt: &Prompter,
    seed: Option<&McpServer>,
    taken: &[String],
    secrets: &mut BTreeMap<String, String>,
    unresolved: &mut Vec<String>,
    prefill: &Prefill,
) -> McpServer {
    let name = unique_name(
        prompt,
        seed.map(|s| s.name.as_str()).unwrap_or("github"),
        taken,
    );
    // Prompt for the GITHUB_TOKEN value the same way providers prompt for secrets.
    collect_secret(prompt, secrets, unresolved, prefill, "GITHUB_TOKEN");
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer ${GITHUB_TOKEN}".to_string(),
    );
    McpServer {
        name,
        transport: "http".to_string(),
        command: None,
        args: Vec::new(),
        url: Some("https://api.githubcopilot.com/mcp/".to_string()),
        headers,
    }
}

fn build_custom(prompt: &Prompter, seed: Option<&McpServer>, taken: &[String]) -> McpServer {
    let name = unique_name(
        prompt,
        seed.map(|s| s.name.as_str()).unwrap_or("custom"),
        taken,
    );
    let transport = prompt.ask_validated(
        "  Transport? (stdio/http/sse)",
        seed.map(|s| s.transport.as_str()).unwrap_or("stdio"),
        |s| match s.trim().to_ascii_lowercase().as_str() {
            t @ ("stdio" | "http" | "sse") => Ok(t.to_string()),
            other => Err(format!(
                "unknown transport {other:?}; choose stdio, http, or sse"
            )),
        },
    );
    if transport == "stdio" {
        let command = prompt.ask_validated(
            "  Command?",
            seed.and_then(|s| s.command.as_deref()).unwrap_or("npx"),
            |value| validate_non_empty("command", value),
        );
        let args_default = seed.map(|s| s.args.join(" ")).unwrap_or_default();
        let args_raw = prompt.ask_line("  Args? (space-separated)", &args_default);
        let args = args_raw
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        McpServer {
            name,
            transport,
            command: Some(command),
            args,
            url: None,
            headers: BTreeMap::new(),
        }
    } else {
        let url = prompt.ask_validated(
            "  URL?",
            seed.and_then(|s| s.url.as_deref()).unwrap_or(""),
            |value| validate_non_empty("url", value),
        );
        // Start with any saved custom headers so an Edit run that accepts the
        // defaults is lossless. Additional headers can be appended below.
        let mut headers = seed
            .map(|server| server.headers.clone())
            .unwrap_or_default();
        while prompt.ask_yes_no("  Add a header?", false) {
            let header_name = prompt.ask_validated("  Header name?", "Authorization", |value| {
                let value = value.trim();
                if value.is_empty() {
                    Err("header name cannot be empty".to_string())
                } else {
                    Ok(value.to_string())
                }
            });
            let header_value = prompt.ask_line("  Header value?", "");
            headers.insert(header_name, header_value);
        }
        McpServer {
            name,
            transport,
            command: None,
            args: Vec::new(),
            url: Some(url),
            headers,
        }
    }
}

/// Collect a secret value for `var` into `.env`:
/// - already exported in this process → capture that value, no prompt;
/// - on the Edit path with an existing value → keep it silently (never echoed);
/// - otherwise prompt; a blank answer leaves the key out of `.env` and records
///   it as unresolved (warned once at the end).
fn collect_secret(
    prompt: &Prompter,
    secrets: &mut BTreeMap<String, String>,
    unresolved: &mut Vec<String>,
    prefill: &Prefill,
    var: &str,
) {
    if secrets.contains_key(var) {
        return; // already captured this run (e.g. two providers share a var)
    }
    // Already exported in the wizard's own environment → capture it, no prompt.
    if let Ok(v) = std::env::var(var) {
        if !v.is_empty() {
            secrets.insert(var.to_string(), v);
            return;
        }
    }
    // Edit path: an existing value is kept without re-echoing it.
    if let Some(existing) = prefill.secrets.get(var) {
        if !existing.is_empty() {
            let keep = prompt.ask_yes_no(&format!("  Keep the existing value for {var}?"), true);
            if keep {
                secrets.insert(var.to_string(), existing.clone());
                return;
            }
        }
    }
    // NOTE (known limitation): the value is echoed to the terminal as typed —
    // there is no masking in this first cut (documented in the work item and
    // docs/reference/baectl.md).
    let value = prompt.ask_line(&format!("  Value for {var}? (blank to skip)"), "");
    if value.is_empty() {
        if !unresolved.iter().any(|u| u == var) {
            unresolved.push(var.to_string());
        }
    } else {
        secrets.insert(var.to_string(), value);
    }
}

// -- File generation --------------------------------------------------------

/// Write all three files (launcher + `.env` + `bae-config.toml`), removing any
/// stale launcher for the *other* output mode so a confirmed mode conversion
/// (e.g. `--apple` over a directory that held a `docker-compose.yml`) never
/// leaves two conflicting launchers behind.
fn write_all_files(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    write_config_toml(dir, config)?;
    write_env_file(dir, config)?;
    let stale_launcher = if config.apple {
        write_apple_script(dir, config)?;
        COMPOSE_FILE
    } else {
        write_compose_file(dir, config)?;
        APPLE_SCRIPT
    };
    remove_if_present(&dir.join(stale_launcher))?;
    Ok(())
}

/// Generate `bae-config.toml` by hand (option (b) — see module doc). Emits
/// `[mcp]`/`[providers]` only, never `[telemetry]`.
fn write_config_toml(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    let mut out = String::new();
    out.push_str(&generated_header(config, "#"));
    out.push('\n');

    // [mcp] section (always emitted; an empty server list is valid).
    out.push_str("[mcp]\n");
    for server in &config.mcp {
        out.push_str("\n[[mcp.servers]]\n");
        out.push_str(&format!("name = {}\n", toml_string(&server.name)));
        out.push_str(&format!("transport = {}\n", toml_string(&server.transport)));
        if let Some(command) = &server.command {
            out.push_str(&format!("command = {}\n", toml_string(command)));
        }
        if !server.args.is_empty() {
            out.push_str(&format!("args = {}\n", toml_string_array(&server.args)));
        }
        if let Some(url) = &server.url {
            out.push_str(&format!("url = {}\n", toml_string(url)));
        }
        if !server.headers.is_empty() {
            out.push_str(&format!(
                "headers = {}\n",
                toml_inline_table(&server.headers)
            ));
        }
    }

    // [providers] section.
    out.push_str("\n[providers]\n");
    for provider in &config.providers {
        out.push_str("\n[[providers.entries]]\n");
        out.push_str(&format!("name = {}\n", toml_string(&provider.name)));
        out.push_str(&format!(
            "provider = {}\n",
            toml_string(provider.kind.as_str())
        ));
        out.push_str(&format!("model = {}\n", toml_string(&provider.model)));
        out.push_str(&format!(
            "auth_token = {}\n",
            toml_string(&format!("${{{}}}", provider.auth_env))
        ));
    }

    write_file(&dir.join(CONFIG_FILE), &out, 0o644)
}

/// Generate `.env`. Only user-changed `BAE_*` values and supplied secrets are
/// written (an unset key means "use the image's built-in default"). Mode `0600`.
fn write_env_file(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    let mut out = String::new();
    out.push_str(&generated_header(config, "#"));
    out.push_str("# Holds secrets and non-default BAE_* overrides. Sourced by the launcher.\n\n");

    // Secrets, in a deterministic order: provider auth vars (in provider order),
    // then MCP secret vars, then any remaining (e.g. BAE_MAX_PASSWORD).
    let mut ordered_keys: Vec<String> = Vec::new();
    for provider in &config.providers {
        ordered_keys.push(provider.auth_env.clone());
    }
    for server in &config.mcp {
        for value in server.headers.values() {
            if let Some(var) = extract_env_var(value) {
                ordered_keys.push(var);
            }
        }
    }
    // Any secrets not yet listed (e.g. BAE_MAX_PASSWORD, custom vars).
    for key in config.secrets.keys() {
        ordered_keys.push(key.clone());
    }
    let mut written: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for key in ordered_keys {
        if written.contains(&key) {
            continue;
        }
        if let Some(value) = config.secrets.get(&key) {
            out.push_str(&format!("{key}={value}\n"));
            written.insert(key);
        }
    }

    // Non-default BAE_* overrides, in the fixed step-5 order.
    for (key, value) in &config.bae_overrides {
        out.push_str(&format!("{key}={value}\n"));
    }

    write_file(&dir.join(ENV_FILE), &out, 0o600)
}

/// Generate `docker-compose.yml`. Publishes the client port (`BAE_ADDR`'s port,
/// default 8080) only (standard), plus the MAX dashboard port (max); **never**
/// 8081 (admin port stays loopback-only inside the container).
fn write_compose_file(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    let service = config.variant.service_name();
    let mut out = String::new();
    out.push_str(&generated_header(config, "#"));
    out.push_str("services:\n");
    out.push_str(&format!("  {service}:\n"));
    out.push_str(&format!("    image: {}\n", config.image_tag()));
    out.push_str(&format!("    env_file: {ENV_FILE}\n"));
    out.push_str("    environment:\n");
    // BAE_CONFIG is a fixed path the compose file controls, not a user choice.
    out.push_str(&format!("      BAE_CONFIG: {CONTAINER_CONFIG_PATH}\n"));
    out.push_str("    volumes:\n");
    out.push_str(&format!("      - {DATA_VOLUME}:/var/lib/bae\n"));
    out.push_str(&format!(
        "      - ./{CONFIG_FILE}:{CONTAINER_CONFIG_PATH}:ro\n"
    ));
    out.push_str("    ports:\n");
    // Publish/health-check the port the server actually listens on (BAE_ADDR's
    // port, default 8080); an operator can still remap the *host* side via
    // BAE_ADDR_PORT in .env, which compose interpolates from the .env file.
    out.push_str(&format!(
        "      - \"${{BAE_ADDR_PORT:-{port}}}:{port}\"\n",
        port = config.client_port
    ));
    if config.variant == Variant::Max {
        out.push_str(&format!("      - \"{}:3000\"\n", config.max_port));
    }
    out.push_str("    restart: unless-stopped\n");
    out.push_str("volumes:\n");
    out.push_str(&format!("  {DATA_VOLUME}:\n"));

    write_file(&dir.join(COMPOSE_FILE), &out, 0o644)
}

/// Generate `bae-setup.sh` (Apple `container` launcher). Functionally
/// equivalent to the compose file; publishes the same ports (never 8081).
fn write_apple_script(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    let name = config.variant.apple_container_name();
    let mut out = String::new();
    out.push_str("#!/usr/bin/env bash\n");
    out.push_str("set -euo pipefail\n");
    out.push_str(&generated_header(config, "#"));
    out.push_str("cd \"$(dirname \"$0\")\"\n\n");
    // Read ONLY the host-port override from .env, literally — never `source`
    // .env, whose values (provider/MCP secrets, arbitrary overrides) may contain
    // shell metacharacters that sourcing would evaluate. The container itself
    // still receives every variable via `--env-file .env` below.
    out.push_str("# Read only BAE_ADDR_PORT from .env, without evaluating the file.\n");
    out.push_str(
        "BAE_ADDR_PORT=\"$(sed -n 's/^BAE_ADDR_PORT=//p' .env 2>/dev/null | tail -n1)\"\n",
    );
    out.push_str(&format!(
        "BAE_ADDR_PORT=\"${{BAE_ADDR_PORT:-{}}}\"\n\n",
        config.client_port
    ));
    out.push_str(&format!(
        "container volume inspect {DATA_VOLUME} >/dev/null 2>&1 || container volume create {DATA_VOLUME}\n"
    ));
    out.push_str(&format!("container stop {name} >/dev/null 2>&1 || true\n"));
    out.push_str(&format!("container rm {name} >/dev/null 2>&1 || true\n\n"));
    out.push_str(&format!("container run -d --name {name} \\\n"));
    out.push_str(&format!(
        "  --publish \"${{BAE_ADDR_PORT}}:{}\" \\\n",
        config.client_port
    ));
    if config.variant == Variant::Max {
        out.push_str(&format!("  --publish \"{}:3000\" \\\n", config.max_port));
    }
    out.push_str(&format!("  --volume {DATA_VOLUME}:/var/lib/bae \\\n"));
    out.push_str(&format!(
        "  --volume \"$(pwd)/{CONFIG_FILE}:{CONTAINER_CONFIG_PATH}:ro\" \\\n"
    ));
    out.push_str(&format!("  --env-file {ENV_FILE} \\\n"));
    out.push_str(&format!("  --env BAE_CONFIG={CONTAINER_CONFIG_PATH} \\\n"));
    out.push_str(&format!("  {}\n", config.image_tag()));

    write_file(&dir.join(APPLE_SCRIPT), &out, 0o755)
}

/// The provenance header comment prepended to every generated file.
fn generated_header(config: &SetupConfig, comment: &str) -> String {
    let mut flags = Vec::new();
    if config.dev {
        flags.push("--dev");
    }
    if config.apple {
        flags.push("--apple");
    }
    let flags = if flags.is_empty() {
        "(none)".to_string()
    } else {
        flags.join(" ")
    };
    // A monotonic-ish stamp without pulling in a date crate. Not used for
    // anything but human provenance; the Edit round-trip test ignores this line.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{comment} Generated by `baectl setup` (variant: {}, flags: {}, unix: {}).\n\
         {comment} Re-run `baectl setup` in this directory to launch or edit it.\n",
        config.variant.as_str(),
        flags,
        stamp,
    )
}

// -- TOML string helpers (hand-built serialization) -------------------------

/// A basic TOML quoted string with the standard escapes.
fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A TOML array of strings: `["a", "b"]`.
fn toml_string_array(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// A TOML inline table: `{ Key = "val", Other = "v2" }`.
fn toml_inline_table(map: &BTreeMap<String, String>) -> String {
    let inner: Vec<String> = map
        .iter()
        .map(|(k, v)| format!("{} = {}", toml_key(k), toml_string(v)))
        .collect();
    format!("{{ {} }}", inner.join(", "))
}

/// A bare TOML key when it is a simple identifier, else a quoted key.
fn toml_key(k: &str) -> String {
    if !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_string()
    } else {
        toml_string(k)
    }
}

// -- Launch and first profile/key -------------------------------------------

/// The tail of every path: warn about unresolved secrets, then either launch
/// (streaming engine output, polling `/healthz`, and — on a fresh setup —
/// creating a first profile/key inside the container) or print the manual
/// launch command.
fn finish(
    prompt: &Prompter,
    dir: &Path,
    config: &SetupConfig,
    fresh: bool,
) -> Result<(), CliError> {
    warn_unresolved(config);

    let launch_default = prompt.interactive;
    let launch = prompt.ask_yes_no("Launch now?", launch_default);
    if !launch {
        print_manual_launch(config);
        return Ok(());
    }

    let engine = if config.apple { "container" } else { "docker" };
    if !binary_on_path(engine) {
        return Err(CliError::runtime(format!(
            "{engine} not found on PATH — install it (or generate here and deploy \
             elsewhere) then re-run `baectl setup` and choose Launch."
        )));
    }

    launch_engine(dir, config)?;
    wait_healthy(config.client_port)?;

    if fresh {
        create_first_profile_and_key(dir, config)?;
    } else {
        println!(
            "Re-launched the saved configuration. (The prior profile/key are \
             assumed to still exist — if they were deleted, re-run and choose \
             Edit to recreate them.)"
        );
    }

    if let Some(hint) = max_password_hint(config) {
        println!("{hint}");
    }
    Ok(())
}

/// The post-launch instruction for retrieving MAX's self-generated dashboard
/// password — returned only when the variant is `max` and the user left the
/// password blank (so `setup` set none itself). Mirrors the admin-key retrieval
/// pattern (`docker exec … cat <path>`) from `docs/guides/max-webapp.md`.
fn max_password_hint(config: &SetupConfig) -> Option<String> {
    if config.variant != Variant::Max || !config.max_password_blank {
        return None;
    }
    let container = if config.apple {
        config.variant.apple_container_name()
    } else {
        config.variant.service_name()
    };
    let exec = if config.apple {
        format!("container exec {container}")
    } else {
        format!("docker compose exec {container}")
    };
    Some(format!(
        "MAX generated its own dashboard password on first boot. Retrieve it with:\n  \
         {exec} cat /var/lib/bae/max-password.pem"
    ))
}

/// Warn once, listing every provider/MCP secret the user declined that was also
/// absent from the process environment (resolution fails at connect time).
fn warn_unresolved(config: &SetupConfig) {
    if config.unresolved.is_empty() {
        return;
    }
    eprintln!(
        "baectl: warning — no value was supplied for: {}",
        config.unresolved.join(", ")
    );
    eprintln!(
        "        These `${{VAR}}` references are still written to {CONFIG_FILE}, but \
         are absent from {ENV_FILE}; the server will fail with an \
         \"unresolved ${{ENV_VAR}}\" error until they are set."
    );
}

/// Print the exact manual launch command when the user declines to launch.
fn print_manual_launch(config: &SetupConfig) {
    if config.apple {
        println!("To launch the configuration:\n  ./{APPLE_SCRIPT}");
    } else {
        println!("To launch the configuration:\n  docker compose up -d");
    }
    println!(
        "Re-running `baectl setup` in this directory offers the launch step again \
         without redoing the Q&A."
    );
}

/// Start the container engine, streaming its stdout/stderr through.
fn launch_engine(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    let status = if config.apple {
        Command::new(dir.join(APPLE_SCRIPT))
            .current_dir(dir)
            .status()
    } else {
        Command::new("docker")
            .args(["compose", "up", "-d"])
            .current_dir(dir)
            .status()
    };
    let status = status.map_err(|e| CliError::runtime(format!("failed to launch: {e}")))?;
    if !status.success() {
        return Err(CliError::runtime(
            "the container engine exited non-zero while launching (see its output above)",
        ));
    }
    Ok(())
}

/// Poll `GET /healthz` with a short backoff before proceeding — a started
/// container is not the same as a server accepting connections.
fn wait_healthy(port: u16) -> Result<(), CliError> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| CliError::runtime(format!("could not build an HTTP client: {e}")))?;
    // ~30 * (up to 2s request + 0.5s sleep) — generous for first-boot migrations.
    for _ in 0..30 {
        if let Ok(resp) = client.get(&url).send() {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(CliError::runtime(format!(
        "server did not become healthy at {url} within the timeout; check the \
         container logs"
    )))
}

/// Create the first profile (`default`) and client key (`default`) *inside* the
/// container — the admin port is never reachable from the host.
fn create_first_profile_and_key(dir: &Path, config: &SetupConfig) -> Result<(), CliError> {
    let primary = config
        .providers
        .first()
        .map(|p| p.name.clone())
        .ok_or_else(|| CliError::runtime("no provider was configured; cannot create a profile"))?;

    let profile_json = exec_baectl(
        dir,
        config,
        &["create", "profile", "default", &primary, "--json"],
    )?;
    let profile: Value = serde_json::from_str(&profile_json)
        .map_err(|e| CliError::runtime(format!("could not parse `create profile` output: {e}")))?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::runtime("`create profile` output had no id"))?
        .to_string();
    println!("created profile 'default' ({profile_id})");

    let key_json = exec_baectl(
        dir,
        config,
        &["create", "key", "default", &profile_id, "--json"],
    )?;
    let key: Value = serde_json::from_str(&key_json)
        .map_err(|e| CliError::runtime(format!("could not parse `create key` output: {e}")))?;
    let plaintext = key
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::runtime("`create key` output had no key"))?;

    // The plaintext key is shown exactly once (same posture as `create key`).
    println!("created client key 'default':");
    println!("  {plaintext}");
    eprintln!("baectl: copy the key now — it cannot be retrieved again");
    println!("Point a client at it with:");
    println!("  export BAE_URL=http://localhost:{}", config.client_port);
    println!("  export BAE_API_KEY={plaintext}");
    Ok(())
}

/// Run `baectl <args>` inside the launched container and return its stdout.
fn exec_baectl(dir: &Path, config: &SetupConfig, args: &[&str]) -> Result<String, CliError> {
    let mut cmd = if config.apple {
        let mut c = Command::new("container");
        c.arg("exec")
            .arg(config.variant.apple_container_name())
            .arg("baectl");
        c
    } else {
        let mut c = Command::new("docker");
        c.args(["compose", "exec", "-T", config.variant.service_name()])
            .arg("baectl");
        c
    };
    cmd.args(args).current_dir(dir);
    let output = cmd
        .output()
        .map_err(|e| CliError::runtime(format!("failed to exec baectl in the container: {e}")))?;
    if !output.status.success() {
        return Err(CliError::runtime(format!(
            "in-container `baectl {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// -- Filesystem + misc helpers ----------------------------------------------

/// Validate `--dir` exists, is a directory, and is writable, before prompting.
fn validate_dir(dir: &Path) -> Result<(), CliError> {
    if !dir.exists() {
        return Err(CliError::runtime(format!(
            "--dir {} does not exist",
            dir.display()
        )));
    }
    if !dir.is_dir() {
        return Err(CliError::runtime(format!(
            "--dir {} is not a directory",
            dir.display()
        )));
    }
    // Cheap writability probe: create then remove a dot-file.
    let probe = dir.join(".baectl-setup-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(CliError::runtime(format!(
            "--dir {} is not writable: {e}",
            dir.display()
        ))),
    }
}

/// Back up the current three files one generation deep before an Edit overwrite.
fn backup_existing(dir: &Path, apple: bool) -> Result<(), CliError> {
    let launcher = if apple { APPLE_SCRIPT } else { COMPOSE_FILE };
    for name in [launcher, ENV_FILE, CONFIG_FILE] {
        let src = dir.join(name);
        if src.exists() {
            let bak = dir.join(format!("{name}.bak"));
            std::fs::copy(&src, &bak)
                .map_err(|e| CliError::runtime(format!("could not back up {name}: {e}")))?;
        }
    }
    Ok(())
}

/// Remove `path` if it exists, treating an already-absent file as success.
fn remove_if_present(path: &Path) -> Result<(), CliError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CliError::runtime(format!(
            "could not remove stale {}: {e}",
            path.display()
        ))),
    }
}

/// Read a file to a string, mapping errors to a clean runtime error.
fn read_to_string(path: &Path) -> Result<String, CliError> {
    std::fs::read_to_string(path)
        .map_err(|e| CliError::runtime(format!("could not read {}: {e}", path.display())))
}

/// Write `contents` to `path`, applying `mode` on Unix (where baectl ships).
fn write_file(path: &Path, contents: &str, mode: u32) -> Result<(), CliError> {
    let map_err =
        |e: std::io::Error| CliError::runtime(format!("could not write {}: {e}", path.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)
            .map_err(map_err)?;
        // `mode(..)` only applies on creation; clamp explicitly so an overwrite
        // of a pre-existing file also gets the intended permissions.
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(map_err)?;
        f.write_all(contents.as_bytes()).map_err(map_err)?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::write(path, contents).map_err(map_err)?;
    }
    Ok(())
}

/// Whether an executable named `name` exists on `PATH`.
fn binary_on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let serial = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "baectl-setup-{label}-{}-{serial}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn provider(name: &str, kind: ProviderKind, auth_env: &str) -> Provider {
        let model = kind.default_model().to_string();
        Provider {
            name: name.to_string(),
            kind,
            model,
            auth_env: auth_env.to_string(),
        }
    }

    fn server(
        name: &str,
        transport: &str,
        command: Option<&str>,
        args: &[&str],
        url: Option<&str>,
        headers: &[(&str, &str)],
    ) -> McpServer {
        McpServer {
            name: name.to_string(),
            transport: transport.to_string(),
            command: command.map(str::to_string),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            url: url.map(str::to_string),
            headers: headers
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    fn config(variant: Variant, dev: bool, apple: bool) -> SetupConfig {
        SetupConfig {
            variant,
            dev,
            apple,
            providers: vec![provider(
                "anthropic-default",
                ProviderKind::Anthropic,
                "ANTHROPIC_API_KEY",
            )],
            mcp: Vec::new(),
            bae_overrides: Vec::new(),
            secrets: BTreeMap::new(),
            unresolved: Vec::new(),
            max_port: 3000,
            max_password_blank: true,
            client_port: CLIENT_PORT,
        }
    }

    fn without_timestamp(text: &str) -> String {
        text.lines().skip(1).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn documented_defaults_and_validation_reprompt() {
        assert_eq!(
            BAE_ENV_DEFAULTS,
            [
                ("BAE_ADDR", "0.0.0.0:8080"),
                ("BAE_LOG", "info"),
                ("BAE_SHUTDOWN_TIMEOUT", "30"),
                ("BAE_TURN_TIMEOUT", "120"),
                ("BAE_SANDBOX_DRIVER", "docker"),
            ]
        );
        assert_eq!(ProviderKind::Anthropic.default_model(), "sonnet-5");
        assert_eq!(ProviderKind::OpenAi.default_model(), "gpt-5.6-luna");
        assert_eq!(
            ProviderKind::Anthropic.default_auth_env(),
            "ANTHROPIC_API_KEY"
        );
        assert_eq!(ProviderKind::OpenAi.default_auth_env(), "OPENAI_API_KEY");

        // Keep this cross-check close to the values, so configuration-doc
        // edits cannot silently move the wizard away from documented defaults.
        let reference = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/reference/configuration.md"
        ))
        .expect("configuration reference exists");
        for (_, value) in BAE_ENV_DEFAULTS {
            assert!(
                reference.contains(value),
                "documented default {value:?} missing"
            );
        }

        let prompt = Prompter::scripted(&["unknown", "openai"]);
        let kind = prompt.ask_validated("kind", "anthropic", |value| {
            ProviderKind::parse(value).ok_or_else(|| "bad provider".to_string())
        });
        assert!(matches!(kind, ProviderKind::OpenAi));
        assert_eq!(prompt.prompt_count.get(), 2);

        let prompt = Prompter::scripted(&["duplicate", "unique"]);
        assert_eq!(
            unique_name(&prompt, "server", &["duplicate".to_string()]),
            "unique"
        );
        assert_eq!(prompt.prompt_count.get(), 2);

        let prompt = Prompter::scripted(&["udp", "http"]);
        let transport = prompt.ask_validated("transport", "stdio", |value| match value {
            "stdio" | "http" | "sse" => Ok(value.to_string()),
            _ => Err("bad transport".to_string()),
        });
        assert_eq!(transport, "http");
        assert_eq!(prompt.prompt_count.get(), 2);

        let prompt = Prompter::scripted(&[
            "remote",
            "sse",
            "https://mcp.example.test/sse",
            "y",
            "X-Token",
            "token",
            "n",
        ]);
        let custom = build_custom(&prompt, None, &[]);
        assert_eq!(custom.transport, "sse");
        assert_eq!(custom.headers.get("X-Token"), Some(&"token".to_string()));

        let prefill = Prefill {
            variant: Variant::Standard,
            providers: Vec::new(),
            mcp: Vec::new(),
            bae_overrides: BTreeMap::new(),
            secrets: BTreeMap::new(),
            max_port: None,
        };
        let token_var = format!("BAECTL_SETUP_TEST_TOKEN_{}", std::process::id());
        let provider_answers = vec![
            "anthropic".to_string(),
            "duplicate".to_string(),
            "sonnet-5".to_string(),
            token_var.clone(),
            "".to_string(),
            "y".to_string(),
            "openai".to_string(),
            "duplicate".to_string(),
            "second".to_string(),
            "gpt-5.6-luna".to_string(),
            token_var,
            "".to_string(),
            "n".to_string(),
        ];
        let provider_answer_refs: Vec<&str> = provider_answers.iter().map(String::as_str).collect();
        let prompt = Prompter::scripted(&provider_answer_refs);
        let providers = ask_providers(&prompt, &prefill, &mut BTreeMap::new(), &mut Vec::new());
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.name.as_str())
                .collect::<Vec<_>>(),
            ["duplicate", "second"]
        );

        let prompt = Prompter::scripted(&[
            "y",
            "fetch",
            "duplicate",
            "y",
            "fetch",
            "duplicate",
            "second",
            "n",
        ]);
        let servers = ask_mcp_servers(&prompt, &prefill, &mut BTreeMap::new(), &mut Vec::new());
        assert_eq!(
            servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["duplicate", "second"]
        );
    }

    #[test]
    fn noninteractive_wizard_uses_defaults_without_rendering_questions() {
        let prompt = Prompter::non_interactive();
        let generated = run_wizard(&prompt, false, false, None).unwrap();

        assert_eq!(generated.variant, Variant::Standard);
        assert_eq!(generated.providers.len(), 1);
        assert_eq!(generated.providers[0].name, "anthropic-default");
        assert!(generated.mcp.is_empty());
        assert!(generated.bae_overrides.is_empty());
        assert!(generated.secrets.is_empty());
        assert_eq!(generated.unresolved, vec!["ANTHROPIC_API_KEY"]);
        assert!(!prompt.ask_yes_no("Launch now?", false));
        assert_eq!(prompt.prompt_count.get(), 0);
    }

    #[test]
    fn generated_toml_round_trips_through_server_schema_without_telemetry() {
        let cases = [
            SetupConfig {
                providers: Vec::new(),
                ..config(Variant::Standard, false, false)
            },
            SetupConfig {
                providers: vec![provider(
                    "openai-main",
                    ProviderKind::OpenAi,
                    "OPENAI_API_KEY",
                )],
                mcp: vec![server(
                    "filesystem",
                    "stdio",
                    Some("npx"),
                    &["-y", "@modelcontextprotocol/server-filesystem", "/data"],
                    None,
                    &[],
                )],
                ..config(Variant::Standard, false, false)
            },
            SetupConfig {
                providers: vec![
                    provider("anthropic-a", ProviderKind::Anthropic, "ANTHROPIC_API_KEY"),
                    provider("openai-b", ProviderKind::OpenAi, "OPENAI_API_KEY"),
                ],
                mcp: vec![
                    server(
                        "fetch",
                        "stdio",
                        Some("uvx"),
                        &["mcp-server-fetch"],
                        None,
                        &[],
                    ),
                    server(
                        "github",
                        "http",
                        None,
                        &[],
                        Some("https://api.githubcopilot.com/mcp/"),
                        &[("Authorization", "Bearer ${GITHUB_TOKEN}")],
                    ),
                    server(
                        "events",
                        "sse",
                        None,
                        &[],
                        Some("https://mcp.example.test/sse"),
                        &[],
                    ),
                ],
                ..config(Variant::Max, true, false)
            },
        ];

        for (index, generated) in cases.iter().enumerate() {
            let dir = TempDir::new(&format!("toml-{index}"));
            write_config_toml(dir.path(), generated).unwrap();
            let text = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
            let parsed: baesrv::config_file::BaeConfig = toml::from_str(&text).unwrap();
            assert!(parsed.telemetry.is_none());
            assert_eq!(
                parsed.providers.unwrap().entries.len(),
                generated.providers.len()
            );
            assert_eq!(parsed.mcp.unwrap().servers.len(), generated.mcp.len());
        }
    }

    #[test]
    fn env_contains_only_supplied_secrets_and_nondefault_bae_values() {
        let dir = TempDir::new("env");
        let mut generated = config(Variant::Standard, false, false);
        generated.providers.push(provider(
            "declined",
            ProviderKind::OpenAi,
            "DECLINED_OPENAI_TOKEN",
        ));
        generated.mcp.push(server(
            "github",
            "http",
            None,
            &[],
            Some("https://api.githubcopilot.com/mcp/"),
            &[("Authorization", "Bearer ${GITHUB_TOKEN}")],
        ));
        generated.secrets.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "provided-anthropic".to_string(),
        );
        generated
            .secrets
            .insert("GITHUB_TOKEN".to_string(), "provided-github".to_string());
        generated
            .bae_overrides
            .push(("BAE_LOG".to_string(), "debug".to_string()));

        write_config_toml(dir.path(), &generated).unwrap();
        write_env_file(dir.path(), &generated).unwrap();
        let env = std::fs::read_to_string(dir.path().join(ENV_FILE)).unwrap();
        let toml = std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap();
        assert!(env.contains("ANTHROPIC_API_KEY=provided-anthropic"));
        assert!(env.contains("GITHUB_TOKEN=provided-github"));
        assert!(env.contains("BAE_LOG=debug"));
        assert!(!env.contains("DECLINED_OPENAI_TOKEN="));
        assert!(!env.contains("BAE_ADDR="));
        assert!(!env.contains("BAE_SANDBOX_DRIVER="));
        assert!(toml.contains("auth_token = \"${DECLINED_OPENAI_TOKEN}\""));
    }

    #[test]
    fn launchers_cover_variants_modes_dev_tags_and_never_expose_admin_port() {
        for variant in [Variant::Standard, Variant::Max] {
            for apple in [false, true] {
                for dev in [false, true] {
                    let dir = TempDir::new("launcher");
                    let generated = config(variant, dev, apple);
                    if apple {
                        write_apple_script(dir.path(), &generated).unwrap();
                    } else {
                        write_compose_file(dir.path(), &generated).unwrap();
                    }
                    let name = if apple { APPLE_SCRIPT } else { COMPOSE_FILE };
                    let launcher = std::fs::read_to_string(dir.path().join(name)).unwrap();
                    assert!(launcher.contains(generated.image_tag()));
                    assert!(launcher.contains(CONTAINER_CONFIG_PATH));
                    if apple {
                        assert!(launcher.contains("--env-file .env"));
                        assert!(launcher.contains("--publish \"${BAE_ADDR_PORT}:8080\""));
                        // The generated script must never `source` .env — its
                        // values may hold shell metacharacters (finding 3).
                        assert!(!launcher.contains(". ./.env"), "apple script sourced .env");
                        assert!(!launcher.contains("set -a"), "apple script sourced .env");
                    } else {
                        assert!(launcher.contains("env_file: .env"));
                        assert!(launcher.contains("${BAE_ADDR_PORT:-8080}:8080"));
                    }
                    assert!(!launcher.contains("8081"), "{name} exposed admin port");
                    if variant == Variant::Max {
                        assert!(launcher.contains("3000"));
                    } else {
                        assert!(!launcher.contains(":3000"));
                    }
                }
            }
        }
    }

    #[test]
    fn idempotency_detects_fresh_existing_partial_and_launcher_mismatch() {
        let fresh = TempDir::new("fresh");
        run(false, false, fresh.path()).unwrap();
        for name in [COMPOSE_FILE, ENV_FILE, CONFIG_FILE] {
            assert!(
                fresh.path().join(name).exists(),
                "fresh setup missed {name}"
            );
        }

        let before: Vec<Vec<u8>> = [COMPOSE_FILE, ENV_FILE, CONFIG_FILE]
            .iter()
            .map(|name| std::fs::read(fresh.path().join(name)).unwrap())
            .collect();
        run(false, false, fresh.path()).unwrap();
        let after: Vec<Vec<u8>> = [COMPOSE_FILE, ENV_FILE, CONFIG_FILE]
            .iter()
            .map(|name| std::fs::read(fresh.path().join(name)).unwrap())
            .collect();
        assert_eq!(before, after, "existing path must reuse files verbatim");

        let partial = TempDir::new("partial");
        std::fs::write(partial.path().join(ENV_FILE), "BAE_LOG=debug\n").unwrap();
        run(false, false, partial.path()).unwrap();
        assert!(partial.path().join(ENV_FILE).exists());
        assert!(!partial.path().join(COMPOSE_FILE).exists());
        assert!(!partial.path().join(CONFIG_FILE).exists());

        let mismatch = TempDir::new("mismatch");
        std::fs::write(mismatch.path().join(COMPOSE_FILE), "services: {}\n").unwrap();
        run(false, true, mismatch.path()).unwrap();
        assert!(mismatch.path().join(COMPOSE_FILE).exists());
        assert!(!mismatch.path().join(APPLE_SCRIPT).exists());
        assert!(!mismatch.path().join(ENV_FILE).exists());
        assert!(!mismatch.path().join(CONFIG_FILE).exists());
    }

    #[test]
    fn edit_defaults_reproduce_files_and_back_up_the_previous_generation() {
        let dir = TempDir::new("edit");
        let mut original_config = config(Variant::Max, false, false);
        original_config.max_port = 3333;
        original_config
            .secrets
            .insert("ANTHROPIC_API_KEY".to_string(), "saved-secret".to_string());
        original_config.secrets.insert(
            "BAE_MAX_PASSWORD".to_string(),
            "saved-max-password".to_string(),
        );
        write_all_files(dir.path(), &original_config).unwrap();
        let original: Vec<Vec<u8>> = [COMPOSE_FILE, ENV_FILE, CONFIG_FILE]
            .iter()
            .map(|name| std::fs::read(dir.path().join(name)).unwrap())
            .collect();

        let prefill = load_prefill(dir.path(), false).unwrap();
        backup_existing(dir.path(), false).unwrap();
        let prompt = Prompter::scripted(&[
            "max",
            "anthropic",
            "anthropic-default",
            "sonnet-5",
            "ANTHROPIC_API_KEY",
            "y",
            "n",
            "n",
            "0.0.0.0:8080",
            "info",
            "30",
            "120",
            "docker",
            "3333",
            "y",
        ]);
        let regenerated = run_wizard(&prompt, false, false, Some(prefill)).unwrap();
        write_all_files(dir.path(), &regenerated).unwrap();

        for (index, name) in [COMPOSE_FILE, ENV_FILE, CONFIG_FILE].iter().enumerate() {
            let current = std::fs::read_to_string(dir.path().join(name)).unwrap();
            let old = String::from_utf8(original[index].clone()).unwrap();
            assert_eq!(
                without_timestamp(&current),
                without_timestamp(&old),
                "{name}"
            );
            assert_eq!(
                std::fs::read(dir.path().join(format!("{name}.bak"))).unwrap(),
                original[index]
            );
        }
    }

    // Finding 2 — a non-default BAE_ADDR drives the published container port and
    // the health-check port, keeping the generated deployment launchable.
    #[test]
    fn bae_addr_port_drives_publish_and_health_check_port() {
        let token = format!("BAECTL_ADDR_TEST_{}", std::process::id());
        let prompt = Prompter::scripted(&[
            "standard",
            "anthropic",
            "anthropic-default",
            "sonnet-5",
            &token,
            "",
            "n",
            "n",
            "0.0.0.0:9090",
            "info",
            "30",
            "120",
            "docker",
        ]);
        let generated = run_wizard(&prompt, false, false, None).unwrap();
        assert_eq!(generated.client_port, 9090);

        let dir = TempDir::new("addr-compose");
        write_compose_file(dir.path(), &generated).unwrap();
        let compose = std::fs::read_to_string(dir.path().join(COMPOSE_FILE)).unwrap();
        assert!(compose.contains("${BAE_ADDR_PORT:-9090}:9090"));
        assert!(
            !compose.contains(":8080"),
            "stale 8080 publish for a 9090 listener"
        );

        let apple_cfg = SetupConfig {
            apple: true,
            ..generated
        };
        let apple_dir = TempDir::new("addr-apple");
        write_apple_script(apple_dir.path(), &apple_cfg).unwrap();
        let script = std::fs::read_to_string(apple_dir.path().join(APPLE_SCRIPT)).unwrap();
        assert!(script.contains("--publish \"${BAE_ADDR_PORT}:9090\""));
        assert!(script.contains("BAE_ADDR_PORT:-9090"));
    }

    // Finding 3 — the Apple launcher must never `source` .env (whose values may
    // hold shell metacharacters); it injects env only via `--env-file`.
    #[test]
    fn apple_script_never_sources_env_even_with_hostile_secret_values() {
        let dir = TempDir::new("apple-safe");
        let mut cfg = config(Variant::Standard, false, true);
        cfg.secrets
            .insert("ANTHROPIC_API_KEY".to_string(), "$(rm -rf /)".to_string());
        write_env_file(dir.path(), &cfg).unwrap();
        write_apple_script(dir.path(), &cfg).unwrap();
        let script = std::fs::read_to_string(dir.path().join(APPLE_SCRIPT)).unwrap();
        assert!(!script.contains(". ./.env"), "script sourced .env");
        assert!(!script.contains("set -a"), "script sourced .env");
        assert!(
            !script.contains("$(rm -rf /)"),
            "secret leaked into script body"
        );
        assert!(script.contains("--env-file .env"));
        // The .env still carries the raw value verbatim for container injection.
        let env = std::fs::read_to_string(dir.path().join(ENV_FILE)).unwrap();
        assert!(env.contains("ANTHROPIC_API_KEY=$(rm -rf /)"));
    }

    // Finding 4 — --dev/--apple are answerable interactively: a passed flag
    // skips its question, an absent flag asks one (default N).
    #[test]
    fn dev_and_apple_flags_are_answerable_interactively() {
        let skip = Prompter::scripted(&[]);
        assert!(resolve_flag(&skip, true, "q"));
        assert_eq!(skip.prompt_count.get(), 0, "passed flag must not prompt");

        let yes = Prompter::scripted(&["y"]);
        assert!(resolve_flag(&yes, false, "q"));
        assert_eq!(yes.prompt_count.get(), 1);

        let no = Prompter::scripted(&["n"]);
        assert!(!resolve_flag(&no, false, "q"));

        let quiet = Prompter::non_interactive();
        assert!(!resolve_flag(&quiet, false, "q"));
        assert!(resolve_flag(&quiet, true, "q"));
        assert_eq!(
            quiet.prompt_count.get(),
            0,
            "non-interactive must not prompt"
        );
    }

    // Finding 5 — a confirmed mode conversion leaves exactly one launcher.
    #[test]
    fn write_all_files_removes_the_other_mode_launcher() {
        let to_apple = TempDir::new("to-apple");
        std::fs::write(to_apple.path().join(COMPOSE_FILE), "services: {}\n").unwrap();
        write_all_files(to_apple.path(), &config(Variant::Standard, false, true)).unwrap();
        assert!(to_apple.path().join(APPLE_SCRIPT).exists());
        assert!(
            !to_apple.path().join(COMPOSE_FILE).exists(),
            "stale compose launcher left behind"
        );

        let to_compose = TempDir::new("to-compose");
        std::fs::write(to_compose.path().join(APPLE_SCRIPT), "#!/bin/sh\n").unwrap();
        write_all_files(to_compose.path(), &config(Variant::Standard, false, false)).unwrap();
        assert!(to_compose.path().join(COMPOSE_FILE).exists());
        assert!(
            !to_compose.path().join(APPLE_SCRIPT).exists(),
            "stale apple launcher left behind"
        );
    }

    // Finding 6 — invalid field values are rejected/re-prompted rather than
    // serialized into a config guaranteed to fail server startup.
    #[test]
    fn wizard_field_validators_reject_invalid_values() {
        assert!(validate_env_ident("ANTHROPIC_API_KEY").is_ok());
        assert!(validate_env_ident("_x1").is_ok());
        assert!(validate_env_ident("1BAD").is_err());
        assert!(validate_env_ident("has space").is_err());
        assert!(validate_env_ident("has-dash").is_err());
        assert!(validate_env_ident("").is_err());

        assert!(validate_non_empty("model", "   ").is_err());
        assert_eq!(
            validate_non_empty("model", " sonnet-5 ").unwrap(),
            "sonnet-5"
        );

        assert_eq!(
            validate_bae_addr("0.0.0.0:8080").unwrap(),
            ("0.0.0.0:8080".to_string(), 8080)
        );
        assert_eq!(validate_bae_addr("[::]:9090").unwrap().1, 9090);
        assert!(validate_bae_addr("not-an-addr").is_err());
        assert!(validate_bae_addr("0.0.0.0").is_err());

        // ask_validated re-prompts on an invalid answer, then accepts a valid one.
        let prompt = Prompter::scripted(&["1bad", "GOOD_VAR"]);
        assert_eq!(
            prompt.ask_validated("v", "X", validate_env_ident),
            "GOOD_VAR"
        );
        assert_eq!(prompt.prompt_count.get(), 2);
    }

    // Finding 6 — step 5 re-prompts a malformed timeout and sandbox driver.
    #[test]
    fn step_five_reprompts_bad_timeout_and_sandbox_driver() {
        let token = format!("BAECTL_S5_TEST_{}", std::process::id());
        let prompt = Prompter::scripted(&[
            "standard",
            "anthropic",
            "anthropic-default",
            "sonnet-5",
            &token,
            "",
            "n",
            "n",
            "0.0.0.0:8080",
            "info",
            "soon", // BAE_SHUTDOWN_TIMEOUT: rejected
            "30",   // accepted (== default → not written)
            "120",
            "podman", // BAE_SANDBOX_DRIVER: rejected
            "docker", // accepted (== default → not written)
        ]);
        let generated = run_wizard(&prompt, false, false, None).unwrap();
        assert!(generated.bae_overrides.is_empty());
        assert_eq!(generated.client_port, 8080);
    }

    // Finding 7 — the .env holding live credentials is written 0600, and the
    // launch-preflight PATH probe behaves.
    #[test]
    fn env_file_is_0600_and_binary_on_path_probe_works() {
        let dir = TempDir::new("perms");
        write_env_file(dir.path(), &config(Variant::Standard, false, false)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(ENV_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        #[cfg(unix)]
        assert!(binary_on_path("sh"), "sh should be on PATH on a Unix host");
        assert!(!binary_on_path("definitely-not-a-real-binary-xyz-123"));
    }

    // Finding 7 — the blank-MAX-password retrieval guidance is emitted only for
    // a max variant whose password the user left blank.
    #[test]
    fn max_blank_password_guidance_targets_the_right_container() {
        let mut compose_max = config(Variant::Max, false, false);
        compose_max.max_password_blank = true;
        let hint = max_password_hint(&compose_max).unwrap();
        assert!(hint.contains("docker compose exec bae-max"));
        assert!(hint.contains("cat /var/lib/bae/max-password.pem"));

        let mut apple_max = config(Variant::Max, false, true);
        apple_max.max_password_blank = true;
        assert!(max_password_hint(&apple_max)
            .unwrap()
            .contains("container exec bae-max"));

        compose_max.max_password_blank = false;
        assert!(max_password_hint(&compose_max).is_none());
        assert!(max_password_hint(&config(Variant::Standard, false, false)).is_none());
    }
}
