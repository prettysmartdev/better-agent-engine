//! The six readiness checks shared by `baectl ready` and `baectl run`.
//!
//! Both verbs reach the admin API the same way `setup`'s launch step does — by
//! exec'ing the in-container `baectl` through the shared [`crate::engine`]
//! wrapper, never a host-side connection to the loopback-only admin port. This
//! module runs the checks, optionally applies the safe (#2/#4) admin-API fixes,
//! and — on full success — produces the [`Resolved`] record the caller persists
//! to `resolved.json`.
//!
//! Each check prints one line: `✓ <label>`, `⚠ <label> — will be fixed by run`
//! (a failing #2/#4 that `run` / `--fix` resolves on its own), or `✗ <label>`
//! (blocking). Fix hints print in the exec form matching how `setup` launched
//! the server (`docker compose exec -T baesrv baectl …` / `container exec bae
//! baectl …`), so they are copy-pasteable as printed.
//!
//! The three [`FixMode`]s are the only behavioural difference between the two
//! verbs' check passes:
//! - [`FixMode::Report`] — `baectl ready` with no `--fix`: print the report and
//!   the command that *would* fix #2/#4, mutate nothing.
//! - [`FixMode::Prompt`] — `baectl ready --fix`: same report, then a single
//!   `Apply the N safe fix(es) above? [y/N]` confirmation (asked even without a
//!   TTY) before applying #2/#4.
//! - [`FixMode::Auto`] — `baectl run`: apply the #2/#4 fixes with no prompt,
//!   printing exactly what was fixed. This is the non-interactive fast path.
//!
//! Every check is evaluated before anything is mutated: checks #3 (MCP
//! registry) and #5 (env vars) are print-only — they need a server restart or a
//! host/`.env` edit these verbs will not do unattended — so an unresolved
//! #3/#5 fails the pass in every mode **before** any #2/#4 fix is applied.
//! (`run` of a container build on a TTY is the one exception for #5: it prompts
//! for the missing values instead.)

use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

use crate::engine::{detect_engine, Engine, EngineKind, Prompter, APPLE_SCRIPT, COMPOSE_FILE};
use crate::error::CliError;
use crate::harness::artifact::{artifact_dir, harness_env_path, resolved_path, write_private};
use crate::harness::manifest::{BuildManifest, Requires, Resolved};
use crate::setup::{
    load_setup_profile, parse_config_registry, parse_max_port, ConfigRegistry, CONFIG_FILE,
    ENV_FILE,
};

/// How the check pass may mutate server state to resolve checks #2 and #4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FixMode {
    /// `ready` (no `--fix`): report only, never mutate.
    Report,
    /// `ready --fix`: report, then confirm-once and apply #2/#4.
    Prompt,
    /// `run`: apply #2/#4 with no prompt (the non-interactive fast path).
    Auto,
}

/// The resolved server the harness verbs act on: which engine to `exec baectl`
/// through, the host port `baesrv` is published on, and (max only) the MAX URL.
pub(crate) struct ServerTarget {
    /// Docker vs Apple `container` — the raw engine binary `run` launches with.
    pub(crate) kind: EngineKind,
    /// The `exec baectl` wrapper for admin-API access.
    pub(crate) engine: Engine,
    /// The host port the launcher published `baesrv`'s client port on.
    pub(crate) host_client_port: u16,
    /// The MAX dashboard URL, when `setup`'s variant was `max`.
    pub(crate) max_url: Option<String>,
}

/// Resolve which engine/service a scaffolded `--dir` targets. `None` means no
/// `baectl setup` was ever run there (check #1's "server unreachable" signal).
pub(crate) fn resolve_server(dir: &Path) -> Option<ServerTarget> {
    let kind = detect_engine(dir)?;
    let launcher = match kind {
        EngineKind::Docker => COMPOSE_FILE,
        EngineKind::Apple => APPLE_SCRIPT,
    };
    let launcher_text = std::fs::read_to_string(dir.join(launcher)).unwrap_or_default();
    // The max variant is the only one that publishes the dashboard's `:3000`
    // container port — the same marker `setup`'s own read-back keys off.
    let is_max = launcher_text.contains(":3000");
    let (service, container) = if is_max {
        ("bae-max", "bae-max")
    } else {
        ("baesrv", "bae")
    };
    let engine = match kind {
        EngineKind::Docker => Engine::docker(service),
        EngineKind::Apple => Engine::apple(container),
    };
    let max_url = is_max.then(|| format!("http://localhost:{}", parse_max_port(&launcher_text)));
    Some(ServerTarget {
        kind,
        engine,
        host_client_port: host_client_port(dir),
        max_url,
    })
}

/// The host port `baesrv`'s client port is published on — `BAE_ADDR_PORT` from
/// `.env` if the operator remapped it, else the image default `8080`.
fn host_client_port(dir: &Path) -> u16 {
    let text = std::fs::read_to_string(dir.join(ENV_FILE)).unwrap_or_default();
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix("BAE_ADDR_PORT=") {
            if let Ok(p) = v.trim().parse::<u16>() {
                return p;
            }
        }
    }
    8080
}

/// Resolve the address in the harness's network namespace. Apple containers
/// share the default network with the server; Docker uses the published host
/// port. Inspect Apple addresses afresh because they can change on restart.
pub(crate) fn default_server_url(
    dir: &Path,
    manifest: &BuildManifest,
    server: &ServerTarget,
) -> Result<String, CliError> {
    let port = server.host_client_port;
    if matches!(manifest, BuildManifest::Local(_)) {
        return Ok(format!("http://localhost:{port}"));
    }
    match &server.engine {
        Engine::Docker { .. } => Ok(format!("http://host.docker.internal:{port}")),
        Engine::Apple { container } => {
            let guidance = format!(
                "could not resolve Apple container {container}'s address on the default network; \
                 check `container inspect {container}` or use `baectl run --server-url <url>`"
            );
            let output = Command::new("container")
                .args(["inspect", container])
                .current_dir(dir)
                .output()
                .map_err(|e| CliError::runtime(format!("{guidance}: {e}")))?;
            if !output.status.success() {
                return Err(CliError::runtime(guidance));
            }
            // Inspect includes environment secrets: never echo its output.
            let inspected: Value = serde_json::from_slice(&output.stdout)
                .map_err(|_| CliError::runtime(guidance.clone()))?;
            let ip = apple_server_ip(&inspected).ok_or_else(|| CliError::runtime(guidance))?;
            // Direct traffic uses the listener port, not BAE_ADDR_PORT's host
            // remapping. setup passes BAE_ADDR through its .env file.
            let env = std::fs::read_to_string(dir.join(ENV_FILE)).unwrap_or_default();
            let port = env
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("BAE_ADDR=")?
                        .trim()
                        .parse::<std::net::SocketAddr>()
                        .ok()
                        .map(|a| a.port())
                })
                .unwrap_or(8080);
            Ok(format!("http://{ip}:{port}"))
        }
    }
}

fn apple_server_ip(inspected: &Value) -> Option<Ipv4Addr> {
    let container = inspected.as_array()?.first()?;
    // Apple 1.x nests attachments in status; older releases use networks.
    let networks = container
        .pointer("/status/networks")
        .or_else(|| container.get("networks"))?
        .as_array()?;
    networks.iter().find_map(|network| {
        if network.get("network")?.as_str()? != "default" {
            return None;
        }
        let address = network
            .get("ipv4Address")
            .or_else(|| network.get("address"))?
            .as_str()?;
        let ip: Ipv4Addr = address.split('/').next()?.parse().ok()?;
        (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
    })
}

/// The result of a check pass.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// Every gating check passed (after any applied fixes): persist/launch.
    Ready(Resolved),
    /// Only auto-fixable (⚠) checks failed and they were not applied — `ready`
    /// without `--fix`, or `--fix` with the confirmation declined. Exit 3.
    Fixable,
    /// At least one blocking (✗) check failed. Nothing was mutated. Exit 1.
    Blocked,
}

/// The ⚠ line suffix for a check `run` / `ready --fix` resolves on its own.
pub(crate) const WILL_BE_FIXED_MARKER: &str = "will be fixed by run";

/// The env var `run` exports naming the provider's auth-token variable, so a
/// harness reads the right key for a non-Anthropic provider. Check #5 validates
/// the variable it names ([`Resolved::provider_env`]); both sides derive it
/// from [`provider_key_env`].
pub(crate) const PROVIDER_KEY_ENV: &str = "BAE_PROVIDER_KEY_ENV";

/// The auth-token env var of the registry provider named `primary` (from
/// `bae-config.toml`'s `auth_token = "${VAR}"`), if known.
pub(crate) fn provider_key_env(registry: Option<&ConfigRegistry>, primary: &str) -> Option<String> {
    registry.and_then(|r| {
        r.providers
            .iter()
            .find(|(n, _)| n == primary)
            .and_then(|(_, e)| e.clone())
    })
}

/// Run the six checks against `manifest`, applying the #2/#4 fixes per `mode`.
///
/// Order (B3): every check is evaluated first; if any **blocking** check (#1,
/// #3, #5, or a #2 that cannot be fixed) fails, the pass returns
/// [`Outcome::Blocked`] **before any admin mutation**, in every mode. Only then
/// are the #2/#4 fixes applied (or, in `Report` mode / on a declined confirm,
/// reported as [`Outcome::Fixable`]). A key is never created unless its secret
/// can be persisted to `resolved.json`.
///
/// `prior` is any existing `resolved.json`: a still-valid profile/key reference
/// in it is *re-validated and reused* (not trusted blindly), so a second
/// `ready`/`run` against a fully provisioned target makes zero admin mutations.
///
/// `env_prompt` (B8): `run` passes an interactive [`Prompter`] for container
/// builds so a check #5 failure prompts for the missing values (saved to the
/// `0600` `harness.env`) instead of aborting. `None`, or a non-interactive
/// prompter, keeps #5 print-only.
/// `server_url_override` lets `run --server-url` bypass address discovery.
pub(crate) fn evaluate(
    dir: &Path,
    manifest: &BuildManifest,
    prior: Option<&Resolved>,
    mode: FixMode,
    env_prompt: Option<&Prompter>,
    server_url_override: Option<&str>,
) -> Result<Outcome, CliError> {
    let requires = manifest.requires();

    // -- Check #1: server reachable ----------------------------------------
    let server = match resolve_server(dir) {
        Some(s) => s,
        None => {
            print_check(Mark::Fail, "server reachable");
            println!(
                "    no `baectl setup` has been run in {} — run `baectl setup` first",
                dir.display()
            );
            return Ok(Outcome::Blocked);
        }
    };
    let profiles = match list_json(&server.engine, dir, "profiles") {
        Ok(p) => {
            print_check(Mark::Ok, "server reachable");
            p
        }
        Err(_) => {
            print_check(Mark::Fail, "server reachable");
            println!(
                "    could not reach the admin API through the container — is the server \
                 running? launch it with `baectl setup` (choose Launch), then retry"
            );
            return Ok(Outcome::Blocked);
        }
    };
    let exec = exec_prefix(&server.engine, dir);
    // Resolve before applying profile/key mutations. An explicit URL also
    // supports operators using a custom Apple network.
    let server_url = match server_url_override {
        Some(url) => url.to_string(),
        None => default_server_url(dir, manifest, &server)?,
    };

    let registry = load_registry(dir)?;

    // -- Check #2: a compatible profile (found, or a fix to make one) -------
    let compatible: Vec<&Value> = profiles
        .iter()
        .filter(|p| profile_compatible(p, requires))
        .collect();
    // Prefer the previously resolved profile when it is still compatible, for
    // stability across runs; else the profile `setup` created/reused in this
    // `--dir`; else the first compatible profile.
    let setup_profile = load_setup_profile(dir);
    let mut resolved_profile: Option<Value> = prior
        .and_then(|pr| compatible.iter().find(|p| field(p, "id") == pr.profile_id))
        .or_else(|| {
            let name = setup_profile.as_deref()?;
            compatible.iter().find(|p| field(p, "name") == name)
        })
        .or_else(|| compatible.first())
        .map(|p| (*p).clone());

    let profile_fix = if resolved_profile.is_some() {
        None
    } else if let Some(target) = pick_widen_target(&profiles, prior, setup_profile.as_deref()) {
        Some(ProfileFix::Update {
            target: target.clone(),
        })
    } else {
        // No profile at all → a create is needed, which requires a provider.
        match registry.as_ref().and_then(|r| r.providers.first()) {
            Some((name, _)) => Some(ProfileFix::Create {
                primary: name.clone(),
            }),
            None => Some(ProfileFix::Impossible),
        }
    };
    let profile_impossible = matches!(profile_fix, Some(ProfileFix::Impossible));

    match (&resolved_profile, &profile_fix) {
        (Some(p), _) => print_check(
            Mark::Ok,
            &format!(
                "compatible profile ({}, {})",
                field(p, "id"),
                field(p, "name")
            ),
        ),
        (None, Some(fix)) => {
            let mark = if profile_impossible {
                Mark::Fail
            } else {
                Mark::Fixable
            };
            print_check(mark, "compatible profile");
            print_profile_fix_hint(fix, requires, manifest, mode, &exec);
        }
        (None, None) => unreachable!("no profile and no fix is not constructed"),
    }

    // -- Check #3: required MCP servers registered (always print-only) ------
    let registered: Vec<String> = registry
        .as_ref()
        .map(|r| r.mcp_server_names.clone())
        .unwrap_or_default();
    let missing_servers: Vec<String> = requires
        .mcp_servers
        .iter()
        .filter(|s| !registered.iter().any(|r| r == *s))
        .cloned()
        .collect();
    let mcp_ok = missing_servers.is_empty();
    if mcp_ok {
        print_check(Mark::Ok, "required MCP servers registered");
    } else {
        print_check(
            Mark::Fail,
            &format!(
                "required MCP servers registered (missing: {})",
                missing_servers.join(", ")
            ),
        );
        print_mcp_registry_hint(dir, &missing_servers, &server.kind);
    }

    // -- Check #4: a client key baectl can hand to `run` --------------------
    // The anticipated profile id: the resolved profile, or an Update fix's
    // target (a Create has no id until applied → a key must be created).
    let anticipated_pid: Option<String> = resolved_profile
        .as_ref()
        .map(|p| field(p, "id").to_string())
        .or_else(|| match &profile_fix {
            Some(ProfileFix::Update { target }) => Some(field(target, "id").to_string()),
            _ => None,
        });
    // A failure to *read* the key list is not evidence that no key exists —
    // treating it as an empty list would let `run`'s auto-fix mint a redundant
    // key off an unreadable admin response. Propagate instead.
    let keys = list_json(&server.engine, dir, "keys").map_err(|e| {
        CliError::runtime(format!(
            "could not list client keys through the admin API: {e} — is the server running? \
             launch it with `baectl setup` (choose Launch), then retry"
        ))
    })?;
    // Reuse a previously persisted key only when it still exists bound to the
    // same profile (re-validated, never trusted blindly).
    let mut key_reuse = key_reuse(&keys, prior, anticipated_pid.as_deref());
    let key_needs_create = key_reuse.is_none();
    if let Some((key_id, _)) = &key_reuse {
        print_check(
            Mark::Ok,
            &format!("client key ({key_id}) — reusing saved credential"),
        );
    } else {
        // A key can only be created once a profile exists for it.
        let mark = if profile_impossible {
            Mark::Fail
        } else {
            Mark::Fixable
        };
        print_check(mark, "client key with a stored secret");
        print_key_fix_hint(anticipated_pid.as_deref(), manifest, mode, &exec);
    }

    // -- Check #5: required env vars resolvable (print-only; `run` may prompt)
    let anticipated_primary: Option<String> = resolved_profile
        .as_ref()
        .map(|p| field(p, "primary_provider").to_string())
        .or_else(|| match &profile_fix {
            Some(ProfileFix::Update { target }) => {
                Some(field(target, "primary_provider").to_string())
            }
            Some(ProfileFix::Create { primary }) => Some(primary.clone()),
            _ => None,
        });
    let provider_env = anticipated_primary
        .as_deref()
        .and_then(|primary| provider_key_env(registry.as_ref(), primary));
    let mut missing_env = missing_env_vars(dir, manifest, requires, provider_env.as_deref());
    // B8: an interactive container `run` prompts for what is missing rather
    // than aborting — but only when nothing else blocks, so a secret is never
    // requested for a launch that will not happen anyway.
    let others_block = profile_impossible || !mcp_ok;
    if !missing_env.is_empty() && !others_block {
        if let (Some(prompt), BuildManifest::Container(_)) = (env_prompt, manifest) {
            if prompt.interactive {
                prompt_missing_env(dir, manifest.id(), &missing_env, prompt)?;
                missing_env = missing_env_vars(dir, manifest, requires, provider_env.as_deref());
            }
        }
    }
    let env_ok = missing_env.is_empty();
    if env_ok {
        print_check(Mark::Ok, "required env vars resolvable");
    } else {
        print_check(
            Mark::Fail,
            &format!(
                "required env vars resolvable (missing: {})",
                missing_env.join(", ")
            ),
        );
        print_env_hint(manifest, &missing_env);
    }

    // -- Check #6: MAX reachability (informational, never ✗) ---------------
    if let Some(url) = &server.max_url {
        println!("ℹ MAX dashboard: {url}");
    }

    // -- Gate (B3): any blocking failure aborts BEFORE any mutation ---------
    if profile_impossible || !mcp_ok || !env_ok {
        return Ok(Outcome::Blocked);
    }

    // -- Apply the safe (#2/#4) fixes per mode -----------------------------
    let fix_count = profile_fix.is_some() as usize + key_needs_create as usize;
    if fix_count > 0 {
        let apply = match mode {
            FixMode::Report => false,
            FixMode::Auto => true,
            FixMode::Prompt => confirm_apply(fix_count),
        };
        if !apply {
            return Ok(Outcome::Fixable);
        }
        // Never mint a key whose one-time plaintext could not then be saved.
        if key_needs_create {
            ensure_resolved_writable(dir, manifest.id())?;
        }

        // #2: create/update the profile (additive).
        match &profile_fix {
            Some(ProfileFix::Update { target }) => {
                let updated = apply_update(&server.engine, dir, target, requires)?;
                println!(
                    "fixed: widened profile {} — allowed_tools/mcp_servers/available_sandboxes \
                     now cover the harness",
                    field(&updated, "id")
                );
                resolved_profile = Some(updated);
            }
            Some(ProfileFix::Create { primary }) => {
                let created = apply_create(&server.engine, dir, manifest, primary, requires)?;
                println!(
                    "fixed: created profile {} ({})",
                    field(&created, "id"),
                    field(&created, "name")
                );
                resolved_profile = Some(created);
            }
            Some(ProfileFix::Impossible) => unreachable!("gated above"),
            None => {}
        }
        // #4: create a key for the now-resolved profile (unless we reused one).
        if key_needs_create {
            if let Some(profile) = &resolved_profile {
                let pid = field(profile, "id").to_string();
                let (key_id, plaintext) = apply_create_key(&server.engine, dir, manifest, &pid)?;
                println!("fixed: created client key {key_id} for profile {pid}");
                key_reuse = Some((key_id, plaintext));
            }
        }
    }

    let (Some(profile), Some((key_id, plaintext))) = (resolved_profile, key_reuse) else {
        return Ok(Outcome::Blocked);
    };
    Ok(Outcome::Ready(Resolved {
        profile_id: field(&profile, "id").to_string(),
        profile_name: field(&profile, "name").to_string(),
        key_id,
        // baectl only ever persists a plaintext it legitimately holds: one it
        // just created, or one carried forward from a prior `resolved.json`. It
        // never fabricates a secret for a pre-existing key it cannot recover.
        client_key_plaintext: Some(plaintext),
        server_url,
        max_url: server.max_url,
        // Recorded by *name* only, never by value: `run` re-reads the value from
        // `.env`/the host env at launch time so a container gets the same
        // provider token check #5 accepted.
        provider_env,
    }))
}

/// Probe that `<dir>/.baectl/builds/<id>/` accepts a write before a key is
/// created: the admin API shows a key's plaintext exactly once, so a key whose
/// `resolved.json` cannot be written would be orphaned.
fn ensure_resolved_writable(dir: &Path, id: &str) -> Result<(), CliError> {
    let probe = artifact_dir(dir, id).join(".resolved.probe");
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(|e| {
            CliError::runtime(format!(
                "cannot write {} ({e}) — refusing to create a client key whose secret could \
                 not be saved",
                resolved_path(dir, id).display()
            ))
        })
}

/// B8: prompt for each missing container env var and merge the answers into
/// the `0600` `harness.env` (where `run`'s env resolution and check #5 both
/// read them). An empty answer aborts.
fn prompt_missing_env(
    dir: &Path,
    id: &str,
    missing: &[String],
    prompt: &Prompter,
) -> Result<(), CliError> {
    let path = harness_env_path(dir, id);
    let mut body = std::fs::read_to_string(&path).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    for var in missing {
        let value = prompt.ask_line(&format!("Value for required env var {var}?"), "");
        let value = value.trim();
        if value.is_empty() {
            return Err(CliError::runtime(format!(
                "no value provided for required env var {var}"
            )));
        }
        body.push_str(&format!("{var}={value}\n"));
    }
    write_private(&path, &body)
}

/// The proposed #2 fix when no compatible profile exists.
enum ProfileFix {
    /// Widen an existing profile's `allowed_tools`/`mcp_servers` (additive).
    Update { target: Value },
    /// Create a new profile (no profile existed at all) using this provider.
    Create { primary: String },
    /// No profile exists and no provider is registered to create one against.
    Impossible,
}

/// Pick the profile to widen when none is compatible: the previously resolved
/// one if it still exists, else the profile `setup` recorded for this `--dir`
/// (`setup_profile`), else a profile named `default`, else the first.
fn pick_widen_target<'a>(
    profiles: &'a [Value],
    prior: Option<&Resolved>,
    setup_profile: Option<&str>,
) -> Option<&'a Value> {
    if profiles.is_empty() {
        return None;
    }
    prior
        .and_then(|pr| profiles.iter().find(|p| field(p, "id") == pr.profile_id))
        .or_else(|| {
            let name = setup_profile?;
            profiles.iter().find(|p| field(p, "name") == name)
        })
        .or_else(|| profiles.iter().find(|p| field(p, "name") == "default"))
        .or_else(|| profiles.first())
}

/// Whether a profile's `allowed_tools`, `mcp_servers` and `available_sandboxes`
/// all cover `requires`.
fn profile_compatible(profile: &Value, requires: &Requires) -> bool {
    let tools = string_array(profile.get("allowed_tools"));
    let servers = string_array(profile.get("mcp_servers"));
    let sandboxes = string_array(profile.get("available_sandboxes"));
    is_superset(&tools, &requires.allowed_tools)
        && is_superset(&servers, &requires.mcp_servers)
        && is_superset(&sandboxes, &requires.sandboxes)
}

/// Whether `have` contains every element of `need`.
fn is_superset(have: &[String], need: &[String]) -> bool {
    need.iter().all(|n| have.iter().any(|h| h == n))
}

/// The additive union of `base` with `extra`, preserving `base`'s order and
/// appending only the names not already present. Pure — the unit-tested core of
/// the "never drop a name another harness depends on" guarantee.
pub(crate) fn union(base: &[String], extra: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    for e in extra {
        if !out.iter().any(|x| x == e) {
            out.push(e.clone());
        }
    }
    out
}

/// Build the `baectl update profile …` argument vector that widens `target`
/// additively. The admin update is a full PUT replacement, so **every** field
/// the profile body carries is re-sent: primary/name/fallbacks verbatim, and
/// tools/servers/sandboxes as the profile's existing list unioned with
/// `requires`. `available_sandboxes` is always re-sent even when the harness
/// declares none (the server treats a missing field as `[]`). `--json` is *not*
/// appended (callers add it for exec; the display path shows it without).
pub(crate) fn update_fix_args(target: &Value, requires: &Requires) -> Vec<String> {
    let id = field(target, "id").to_string();
    let primary = field(target, "primary_provider").to_string();
    let name = field(target, "name").to_string();
    let fallbacks = string_array(target.get("fallback_providers"));
    let tools = union(
        &string_array(target.get("allowed_tools")),
        &requires.allowed_tools,
    );
    let servers = union(
        &string_array(target.get("mcp_servers")),
        &requires.mcp_servers,
    );
    let sandboxes = union(
        &string_array(target.get("available_sandboxes")),
        &requires.sandboxes,
    );

    let mut args = vec![
        "update".into(),
        "profile".into(),
        id,
        primary,
        "--name".into(),
        name,
    ];
    for f in fallbacks {
        args.push("--fallback".into());
        args.push(f);
    }
    for t in tools {
        args.push("--allowed-tool".into());
        args.push(t);
    }
    for s in servers {
        args.push("--mcp-server".into());
        args.push(s);
    }
    for s in sandboxes {
        args.push("--available-sandbox".into());
        args.push(s);
    }
    args
}

/// Build the `baectl create profile …` argument vector for a brand-new,
/// harness-named profile carrying exactly the harness's required tools/servers.
fn create_fix_args(manifest: &BuildManifest, primary: &str, requires: &Requires) -> Vec<String> {
    let mut args = vec![
        "create".into(),
        "profile".into(),
        manifest_name(manifest).to_string(),
        primary.to_string(),
    ];
    for t in &requires.allowed_tools {
        args.push("--allowed-tool".into());
        args.push(t.clone());
    }
    for s in &requires.mcp_servers {
        args.push("--mcp-server".into());
        args.push(s.clone());
    }
    for s in &requires.sandboxes {
        args.push("--available-sandbox".into());
        args.push(s.clone());
    }
    args
}

/// Exec an additive `update profile`, returning the replaced profile object.
///
/// The underlying admin API call is a full PUT, so the union is computed
/// against the profile's *current* body, re-read immediately before the write
/// rather than reused from the snapshot taken at the top of [`evaluate`] (which
/// is several exec round-trips old by the time a fix is applied). That shrinks
/// the read-then-write window to a single admin round trip. It does **not**
/// eliminate it: baectl is a single-writer tool and the admin API offers no
/// compare-and-swap, so a genuinely concurrent writer to the same profile can
/// still lose its update. See `docs/reference/03-baectl.md`.
fn apply_update(
    engine: &Engine,
    dir: &Path,
    target: &Value,
    requires: &Requires,
) -> Result<Value, CliError> {
    let fresh = refresh_profile(engine, dir, target);
    let mut args = update_fix_args(&fresh, requires);
    args.push("--json".into());
    let out = exec_json(engine, dir, &args)?;
    Ok(out)
}

/// Re-read `target`'s current body from the server so the additive union is
/// computed against the freshest available copy. A failed/absent re-read falls
/// back to the snapshot — never to an empty body, which would turn the full PUT
/// into the destructive replacement the union exists to prevent.
fn refresh_profile(engine: &Engine, dir: &Path, target: &Value) -> Value {
    let id = field(target, "id");
    if id.is_empty() {
        return target.clone();
    }
    list_json(engine, dir, "profiles")
        .ok()
        .and_then(|profiles| profiles.into_iter().find(|p| field(p, "id") == id))
        .unwrap_or_else(|| target.clone())
}

/// Exec a `create profile`, returning a profile object carrying the id the
/// server assigned plus the tools/servers/provider we sent (the create response
/// itself is only `{id, name, created_at}`).
fn apply_create(
    engine: &Engine,
    dir: &Path,
    manifest: &BuildManifest,
    primary: &str,
    requires: &Requires,
) -> Result<Value, CliError> {
    let mut args = create_fix_args(manifest, primary, requires);
    args.push("--json".into());
    let created = exec_json(engine, dir, &args)?;
    Ok(json!({
        "id": created.get("id").cloned().unwrap_or(Value::Null),
        "name": manifest_name(manifest),
        "primary_provider": primary,
        "fallback_providers": [],
        "allowed_tools": requires.allowed_tools,
        "mcp_servers": requires.mcp_servers,
        "available_sandboxes": requires.sandboxes,
    }))
}

/// Exec a `create key`, returning `(key_id, plaintext)`. The plaintext is shown
/// by the admin API exactly once — this is the single place baectl captures it.
fn apply_create_key(
    engine: &Engine,
    dir: &Path,
    manifest: &BuildManifest,
    profile_id: &str,
) -> Result<(String, String), CliError> {
    let name = manifest.id();
    let out = exec_json(engine, dir, &["create", "key", name, profile_id, "--json"])?;
    let key_id = out
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::runtime("`create key` output had no id"))?
        .to_string();
    let plaintext = out
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::runtime("`create key` output had no key"))?
        .to_string();
    Ok((key_id, plaintext))
}

/// Find a reusable `(key_id, plaintext)` from a prior `resolved.json`: valid
/// only when it carried a plaintext, is bound to the anticipated profile, and
/// that key still exists on the server bound to that profile.
fn key_reuse(
    keys: &[Value],
    prior: Option<&Resolved>,
    profile_id: Option<&str>,
) -> Option<(String, String)> {
    let prior = prior?;
    let plaintext = prior.client_key_plaintext.as_ref()?;
    let pid = profile_id?;
    if prior.profile_id != pid {
        return None;
    }
    let still_exists = keys
        .iter()
        .any(|k| field(k, "id") == prior.key_id && field(k, "profile_id") == pid);
    still_exists.then(|| (prior.key_id.clone(), plaintext.clone()))
}

/// The env vars check #5 needs resolvable: `requires.env` plus the resolved
/// provider's auth-token var (the variable `run` exports as
/// `BAE_PROVIDER_KEY_ENV`). `local` checks the host process env (what `run`
/// inherits); `container` checks `<dir>/.env`, the build's saved `harness.env`,
/// or the host env (what `run` passes through). `BAE_SERVER_URL`/
/// `BAE_CLIENT_KEY` are excluded — `run` sets them itself.
fn missing_env_vars(
    dir: &Path,
    manifest: &BuildManifest,
    requires: &Requires,
    provider_env: Option<&str>,
) -> Vec<String> {
    let mut needed: Vec<String> = requires.env.clone();
    if let Some(v) = provider_env {
        if !needed.iter().any(|n| n == v) {
            needed.push(v.to_string());
        }
    }
    let dotenv = match manifest {
        BuildManifest::Container(m) => {
            let mut keys = env_file_keys(&dir.join(ENV_FILE));
            keys.extend(env_file_keys(&harness_env_path(dir, &m.id)));
            keys
        }
        BuildManifest::Local(_) => Vec::new(),
    };
    needed
        .into_iter()
        .filter(|var| !env_resolvable(var, &dotenv))
        .collect()
}

/// Whether `var` resolves from the host environment or (container) from an env
/// file.
fn env_resolvable(var: &str, dotenv: &[String]) -> bool {
    if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
        return true;
    }
    dotenv.iter().any(|k| k == var)
}

/// The `KEY` names with a non-empty value in a Docker-style env file (values
/// not needed here). A missing file has none.
fn env_file_keys(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_once('=')
                .and_then(|(key, value)| (!value.trim().is_empty()).then(|| key.trim().to_string()))
        })
        .collect()
}

// -- Admin-API exec helpers --------------------------------------------------

/// Exec `baectl list <resource> --json` in-container and parse the JSON array
/// it prints (auto-paginated; tolerates a `{items: […]}` envelope too).
fn list_json(engine: &Engine, dir: &Path, resource: &str) -> Result<Vec<Value>, CliError> {
    let out = engine.exec_baectl(dir, &["list", resource, "--json"])?;
    let value: Value = serde_json::from_str(out.trim())
        .map_err(|e| CliError::runtime(format!("could not parse `list {resource}` output: {e}")))?;
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(mut obj) => match obj.remove("items") {
            Some(Value::Array(items)) => Ok(items),
            _ => Ok(Vec::new()),
        },
        _ => Ok(Vec::new()),
    }
}

/// Exec `baectl <args>` in-container and parse its stdout as a JSON object.
fn exec_json(engine: &Engine, dir: &Path, args: &[impl AsRef<str>]) -> Result<Value, CliError> {
    let refs: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    let out = engine.exec_baectl(dir, &refs)?;
    serde_json::from_str(out.trim())
        .map_err(|e| CliError::runtime(format!("could not parse admin API output: {e}")))
}

// -- bae-config.toml registry ------------------------------------------------

/// Load `<dir>/bae-config.toml`'s registry view. A missing file is `None`
/// (checks #3/#5 then report what they cannot confirm); a present-but-malformed
/// file is a hard error.
fn load_registry(dir: &Path) -> Result<Option<ConfigRegistry>, CliError> {
    match std::fs::read_to_string(dir.join(CONFIG_FILE)) {
        Ok(text) => Ok(Some(parse_config_registry(&text)?)),
        Err(_) => Ok(None),
    }
}

// -- Fix-hint printers -------------------------------------------------------

/// The copy-pasteable host command prefix that reaches the in-container
/// `baectl`, matching how `setup` launched the server: `docker compose exec -T
/// <service> baectl` (with `-f <dir>/docker-compose.yml` when `dir` is not the
/// current directory) or `container exec <name> baectl`.
pub(crate) fn exec_prefix(engine: &Engine, dir: &Path) -> String {
    match engine {
        Engine::Docker { service } => {
            let here = std::env::current_dir()
                .ok()
                .and_then(|c| c.canonicalize().ok());
            let there = dir.canonicalize().ok();
            if here.is_some() && here == there {
                format!("docker compose exec -T {service} baectl")
            } else {
                let file = dir.join(COMPOSE_FILE).display().to_string();
                format!(
                    "docker compose -f {} exec -T {service} baectl",
                    shell_quote(&file)
                )
            }
        }
        Engine::Apple { container } => format!("container exec {container} baectl"),
    }
}

fn print_profile_fix_hint(
    fix: &ProfileFix,
    requires: &Requires,
    manifest: &BuildManifest,
    mode: FixMode,
    exec: &str,
) {
    let suffix = fix_suffix(mode);
    match fix {
        ProfileFix::Update { target } => {
            let cmd = shell_join(&update_fix_args(target, requires));
            println!("    widen the existing profile (additive): {exec} {cmd}{suffix}");
        }
        ProfileFix::Create { primary } => {
            let cmd = shell_join(&create_fix_args(manifest, primary, requires));
            println!("    create a compatible profile: {exec} {cmd}{suffix}");
        }
        ProfileFix::Impossible => {
            println!(
                "    no providers are registered in {CONFIG_FILE}; run `baectl setup` to \
                 configure one first"
            );
        }
    }
}

fn print_key_fix_hint(
    profile_id: Option<&str>,
    manifest: &BuildManifest,
    mode: FixMode,
    exec: &str,
) {
    // A hand-created key's plaintext never reaches `resolved.json`, so point
    // out that `--fix` (or `run`) also records it.
    let suffix = match mode {
        FixMode::Report => "  (--fix also records the key for `run`)",
        FixMode::Prompt | FixMode::Auto => "",
    };
    match profile_id {
        Some(pid) => println!(
            "    create a client key baectl can hand to `run`: {exec} create key {} {}{suffix}",
            shell_quote(manifest.id()),
            shell_quote(pid)
        ),
        None => {
            println!("    a client key will be created for the new profile once it exists{suffix}")
        }
    }
}

/// The trailing "(or re-run with …)" pointer appropriate to the mode.
fn fix_suffix(mode: FixMode) -> &'static str {
    match mode {
        FixMode::Report => "  (or re-run with --fix)",
        // Prompt has already printed the confirm; Auto applies silently.
        FixMode::Prompt | FixMode::Auto => "",
    }
}

fn print_mcp_registry_hint(dir: &Path, missing: &[String], kind: &EngineKind) {
    let restart: String = match kind {
        EngineKind::Docker => "docker compose restart".to_string(),
        EngineKind::Apple => format!("./{APPLE_SCRIPT}"),
    };
    println!(
        "    add each to {}/{CONFIG_FILE} under [mcp], e.g.:",
        dir.display()
    );
    for name in missing {
        println!("      [[mcp.servers]]");
        println!("      name = \"{name}\"");
        println!(
            "      transport = \"stdio\"   # or \"http\"; see docs/reference/05-configuration.md"
        );
    }
    println!("    then restart the server: {restart}   (never auto-applied — it needs a restart)");
}

fn print_env_hint(manifest: &BuildManifest, missing: &[String]) {
    let where_to = match manifest {
        BuildManifest::Local(_) => "export it in the shell you run `baectl run` from (the host)",
        BuildManifest::Container(_) => {
            "set it in <dir>/.env or the host environment (passed through to the container)"
        }
    };
    for var in missing {
        println!("    {var}: unset — {where_to}");
    }
}

// -- Small value/string helpers ----------------------------------------------

/// Read a string field from a JSON object, or `""` if absent/non-string.
fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Collect a JSON array-of-strings field into an owned `Vec`.
fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The harness name recorded in a build manifest (both kinds carry it).
fn manifest_name(manifest: &BuildManifest) -> &str {
    match manifest {
        BuildManifest::Local(m) => &m.name,
        BuildManifest::Container(m) => &m.name,
    }
}

/// Join argument tokens into a copy-pasteable POSIX shell command line.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote `s` for a POSIX shell unless it is made only of characters
/// that never need quoting.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// A check line's state marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// `✓` — passed.
    Ok,
    /// `⚠ … — will be fixed by run` — failing, but `run`/`--fix` resolves it.
    Fixable,
    /// `✗` — failing and blocking.
    Fail,
}

/// The exact stdout line for one check.
fn check_line(mark: Mark, label: &str) -> String {
    match mark {
        Mark::Ok => format!("✓ {label}"),
        Mark::Fixable => format!("⚠ {label} — {WILL_BE_FIXED_MARKER}"),
        Mark::Fail => format!("✗ {label}"),
    }
}

fn print_check(mark: Mark, label: &str) {
    println!("{}", check_line(mark, label));
}

/// Prompt `Apply the N safe fix(es) above? [y/N]` and read one line — asked even
/// when stdin is not a TTY (a state-mutating fix is never silently applied *or*
/// silently skipped). EOF / anything but yes → `false`.
fn confirm_apply(n: usize) -> bool {
    print!("Apply the {n} safe fix(es) above? [y/N] ");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    match io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::manifest::{LocalManifest, Sdk};
    use std::path::PathBuf;

    fn requires(tools: &[&str], servers: &[&str]) -> Requires {
        Requires {
            allowed_tools: tools.iter().map(|s| s.to_string()).collect(),
            mcp_servers: servers.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
            sandboxes: Vec::new(),
        }
    }

    fn profile(id: &str, name: &str, tools: &[&str], servers: &[&str]) -> Value {
        json!({
            "id": id,
            "name": name,
            "primary_provider": "anthropic-sonnet",
            "fallback_providers": [],
            "allowed_tools": tools,
            "mcp_servers": servers,
        })
    }

    fn local_manifest() -> BuildManifest {
        BuildManifest::Local(LocalManifest {
            id: "ref-rust-local".to_string(),
            name: "reference-assistant".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            harness_dir: PathBuf::from("/tmp/ref"),
            run_command: "true".to_string(),
            working_dir: ".".to_string(),
            prepare: None,
            requires: requires(&["get_current_time"], &[]),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
    }

    #[test]
    fn superset_matching_distinguishes_compatible_profiles() {
        let req = requires(&["get_current_time", "read_file"], &["github"]);
        // Superset on both → compatible.
        assert!(profile_compatible(
            &profile(
                "p1",
                "a",
                &["get_current_time", "read_file", "write_file"],
                &["github", "x"]
            ),
            &req
        ));
        // Missing a tool → not compatible.
        assert!(!profile_compatible(
            &profile("p2", "b", &["get_current_time"], &["github"]),
            &req
        ));
        // Missing the server → not compatible (distinct from a tool gap).
        assert!(!profile_compatible(
            &profile("p3", "c", &["get_current_time", "read_file"], &[]),
            &req
        ));
    }

    #[test]
    fn union_is_additive_and_order_preserving() {
        let base = vec!["a".to_string(), "b".to_string()];
        let extra = vec!["b".to_string(), "c".to_string()];
        assert_eq!(union(&base, &extra), vec!["a", "b", "c"]);
        // Never a subset of what was already there.
        for name in &base {
            assert!(union(&base, &extra).contains(name));
        }
    }

    #[test]
    fn update_fix_is_the_union_never_a_drop() {
        // An existing profile with one tool, one server, and a fallback the fix
        // must preserve (a full PUT would otherwise drop them).
        let target = json!({
            "id": "pro_1",
            "name": "default",
            "primary_provider": "anthropic-sonnet",
            "fallback_providers": ["openai-gpt"],
            "allowed_tools": ["existing_tool"],
            "mcp_servers": ["existing_server"],
        });
        let req = requires(&["get_current_time"], &["github"]);
        let args = update_fix_args(&target, &req);

        // Positional shape.
        assert_eq!(
            &args[0..4],
            &["update", "profile", "pro_1", "anthropic-sonnet"]
        );
        // Name and the pre-existing fallback are preserved.
        assert!(window_has(&args, &["--name", "default"]));
        assert!(window_has(&args, &["--fallback", "openai-gpt"]));
        // Both the pre-existing and the newly required tool/server are present.
        for pair in [
            ["--allowed-tool", "existing_tool"],
            ["--allowed-tool", "get_current_time"],
            ["--mcp-server", "existing_server"],
            ["--mcp-server", "github"],
        ] {
            assert!(window_has(&args, &pair), "missing {pair:?} in {args:?}");
        }

        // Regression (B2): every value of every list field of the input profile
        // is re-sent — including `available_sandboxes`, which the harness does
        // not declare at all (the old args dropped it, and the full PUT then
        // wiped the profile's sandbox images).
        let target = json!({
            "id": "pro_1",
            "name": "default",
            "primary_provider": "anthropic-sonnet",
            "fallback_providers": ["openai-gpt", "anthropic-haiku"],
            "allowed_tools": ["t1", "t2"],
            "mcp_servers": ["s1", "s2"],
            "available_sandboxes": ["python:3.12", "alpine"],
        });
        let args = update_fix_args(&target, &requires(&["get_current_time"], &[]));
        for (field, flag) in [
            ("fallback_providers", "--fallback"),
            ("allowed_tools", "--allowed-tool"),
            ("mcp_servers", "--mcp-server"),
            ("available_sandboxes", "--available-sandbox"),
        ] {
            for value in string_array(target.get(field)) {
                assert!(
                    window_has(&args, &[flag, value.as_str()]),
                    "{field} value {value:?} dropped from {args:?}"
                );
            }
        }
        assert!(window_has(&args, &["--name", "default"]));

        // A harness-declared sandbox is unioned in after the existing images.
        let mut req = requires(&[], &[]);
        req.sandboxes = vec!["alpine".to_string(), "node:22".to_string()];
        let args = update_fix_args(&target, &req);
        let sandboxes: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "--available-sandbox")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(sandboxes, vec!["python:3.12", "alpine", "node:22"]);
    }

    /// B2: `requires.sandboxes` is part of compatibility (check #2), and a
    /// created profile carries it.
    #[test]
    fn required_sandboxes_are_checked_and_carried_by_create() {
        let mut req = requires(&["get_current_time"], &[]);
        req.sandboxes = vec!["alpine".to_string()];
        let mut p = profile("p1", "a", &["get_current_time"], &[]);
        assert!(!profile_compatible(&p, &req), "no sandboxes → incompatible");
        p["available_sandboxes"] = json!(["python:3.12", "alpine"]);
        assert!(profile_compatible(&p, &req));

        let args = create_fix_args(&local_manifest(), "anthropic-sonnet", &req);
        assert!(window_has(&args, &["--available-sandbox", "alpine"]));
    }

    /// B7: the exact check-line markers.
    #[test]
    fn check_lines_use_the_three_markers() {
        assert_eq!(
            check_line(Mark::Ok, "server reachable"),
            "✓ server reachable"
        );
        assert_eq!(
            check_line(Mark::Fixable, "compatible profile"),
            "⚠ compatible profile — will be fixed by run"
        );
        assert_eq!(
            check_line(Mark::Fail, "compatible profile"),
            "✗ compatible profile"
        );
        assert_eq!(WILL_BE_FIXED_MARKER, "will be fixed by run");
    }

    /// B7: hints use the exec form matching the engine `setup` launched.
    #[test]
    fn exec_prefix_matches_the_engine() {
        assert_eq!(
            exec_prefix(&Engine::apple("bae-max"), Path::new("/anywhere")),
            "container exec bae-max baectl"
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            exec_prefix(&Engine::docker("baesrv"), &cwd),
            "docker compose exec -T baesrv baectl"
        );
        let dir = temp_dir("exec-prefix");
        assert_eq!(
            exec_prefix(&Engine::docker("bae-max"), &dir),
            format!(
                "docker compose -f {}/docker-compose.yml exec -T bae-max baectl",
                dir.display()
            )
        );
        // A path needing quoting stays copy-pasteable.
        assert_eq!(shell_quote("/tmp/my dir/x"), "'/tmp/my dir/x'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression (B8): a value saved in the build's `harness.env` (where `run`
    /// persists prompted values) satisfies check #5 for a container build; an
    /// empty one does not, and a local build never reads it.
    #[test]
    fn harness_env_values_satisfy_check_5_for_container_builds() {
        let dir = temp_dir("harness-env-check5");
        let path = harness_env_path(&dir, "ref-rust-api");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "BAECTL_TEST_B8_SAVED=from-harness-env\nBAECTL_TEST_B8_BLANK=\n",
        )
        .unwrap();
        let mut req = requires(&[], &[]);
        req.env = vec![
            "BAECTL_TEST_B8_SAVED".to_string(),
            "BAECTL_TEST_B8_BLANK".to_string(),
        ];

        let missing = missing_env_vars(
            &dir,
            &container_manifest(),
            &req,
            Some("BAECTL_TEST_B8_SAVED"),
        );
        assert_eq!(missing, vec!["BAECTL_TEST_B8_BLANK"]);

        // `local` reads the host env only (`harness.env` is never passed on).
        let mut local = local_manifest();
        if let BuildManifest::Local(m) = &mut local {
            m.id = "ref-rust-api".to_string();
        }
        let missing = missing_env_vars(&dir, &local, &req, None);
        assert_eq!(
            missing,
            vec!["BAECTL_TEST_B8_SAVED", "BAECTL_TEST_B8_BLANK"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B8: the TTY prompt appends answers to the `0600` `harness.env`, after
    /// which check #5 passes; an empty answer aborts naming the variable.
    #[test]
    fn prompted_env_values_are_saved_privately_and_satisfy_check_5() {
        let dir = temp_dir("prompt-env");
        let path = harness_env_path(&dir, "ref-rust-api");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "EXISTING=keep").unwrap();
        let missing = vec![
            "BAECTL_TEST_B8_A".to_string(),
            "BAECTL_TEST_B8_B".to_string(),
        ];

        let prompt = Prompter::scripted(&["value-a", "  value-b  "]);
        prompt_missing_env(&dir, "ref-rust-api", &missing, &prompt).unwrap();

        assert_eq!(prompt.prompt_count.get(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "EXISTING=keep\nBAECTL_TEST_B8_A=value-a\nBAECTL_TEST_B8_B=value-b\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let mut req = requires(&[], &[]);
        req.env = missing.clone();
        assert!(missing_env_vars(&dir, &container_manifest(), &req, None).is_empty());

        let err = prompt_missing_env(
            &dir,
            "ref-rust-api",
            &["BAECTL_TEST_B8_C".to_string()],
            &Prompter::scripted(&[""]),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            err.message(),
            "no value provided for required env var BAECTL_TEST_B8_C"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_fix_names_the_harness_and_carries_requires() {
        let manifest = local_manifest();
        let args = create_fix_args(
            &manifest,
            "anthropic-sonnet",
            &requires(&["get_current_time"], &["github"]),
        );
        assert_eq!(
            &args[0..4],
            &[
                "create",
                "profile",
                "reference-assistant",
                "anthropic-sonnet"
            ]
        );
        assert!(window_has(&args, &["--allowed-tool", "get_current_time"]));
        assert!(window_has(&args, &["--mcp-server", "github"]));
    }

    #[test]
    fn key_reuse_requires_plaintext_matching_profile_and_live_key() {
        let keys = vec![json!({"id": "key_1", "profile_id": "pro_1", "prefix": "bae_x"})];
        let prior = Resolved {
            profile_id: "pro_1".to_string(),
            profile_name: "default".to_string(),
            key_id: "key_1".to_string(),
            client_key_plaintext: Some("bae_secret".to_string()),
            server_url: "http://localhost:8080".to_string(),
            max_url: None,
            provider_env: None,
        };
        // Happy path: plaintext held, profile matches, key live → reuse.
        assert_eq!(
            key_reuse(&keys, Some(&prior), Some("pro_1")),
            Some(("key_1".to_string(), "bae_secret".to_string()))
        );
        // Profile changed → no reuse.
        assert_eq!(key_reuse(&keys, Some(&prior), Some("pro_2")), None);
        // Key deleted out-of-band → no reuse.
        assert_eq!(key_reuse(&[], Some(&prior), Some("pro_1")), None);
        // No plaintext stored → no reuse (baectl cannot recover the secret).
        let no_pt = Resolved {
            client_key_plaintext: None,
            ..prior.clone()
        };
        assert_eq!(key_reuse(&keys, Some(&no_pt), Some("pro_1")), None);
    }

    #[test]
    fn default_server_url_differs_by_kind() {
        let local = local_manifest();
        let mut server = ServerTarget {
            kind: EngineKind::Docker,
            engine: Engine::docker("baesrv"),
            host_client_port: 18080,
            max_url: None,
        };
        let dir = Path::new("/nonexistent");
        assert_eq!(
            default_server_url(dir, &local, &server).unwrap(),
            "http://localhost:18080"
        );
        let container = BuildManifest::Container(crate::harness::manifest::ContainerManifest {
            id: "ref-rust-api".to_string(),
            name: "reference-assistant".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            launcher_type: crate::harness::manifest::Launcher::Api,
            image_tag: "ref-rust-api:latest".to_string(),
            harness_build_image: "ref-rust-api-harness-build".to_string(),
            port: Some(9090),
            requires: Requires::default(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        });
        assert_eq!(
            default_server_url(dir, &container, &server).unwrap(),
            "http://host.docker.internal:18080"
        );
        server.kind = EngineKind::Apple;
        server.engine = Engine::apple("bae");
        assert_eq!(
            default_server_url(dir, &local, &server).unwrap(),
            "http://localhost:18080"
        );
    }

    #[test]
    fn apple_inspect_accepts_current_and_legacy_default_network_addresses() {
        for networks in [
            json!({"status": {"networks": [{"network": "default", "ipv4Address": "192.168.64.3/24"}]}}),
            json!({"networks": [{"network": "default", "ipv4Address": "192.168.64.3/24"}]}),
            json!({"networks": [{"network": "custom", "address": "10.0.0.2/24"}, {"network": "default", "address": "192.168.64.3/24"}]}),
        ] {
            assert_eq!(
                apple_server_ip(&json!([networks])),
                Some(Ipv4Addr::new(192, 168, 64, 3))
            );
        }
        for invalid in [
            json!([]),
            json!([{"networks": []}]),
            json!([{"networks": [{"network": "custom", "ipv4Address": "10.0.0.2/24"}]}]),
            json!([{"networks": [{"network": "default", "ipv4Address": "bad"}]}]),
            json!([{"networks": [{"network": "default", "ipv4Address": "127.0.0.1"}]}]),
        ] {
            assert_eq!(apple_server_ip(&invalid), None);
        }
    }

    #[test]
    fn missing_env_local_checks_host_only() {
        let manifest = local_manifest();
        let req = requires(&[], &[]);
        let req = Requires {
            env: vec!["DEFINITELY_UNSET_VAR_XYZ".to_string()],
            ..req
        };
        let missing = missing_env_vars(Path::new("/nonexistent"), &manifest, &req, None);
        assert_eq!(missing, vec!["DEFINITELY_UNSET_VAR_XYZ"]);
    }

    fn container_manifest() -> BuildManifest {
        BuildManifest::Container(crate::harness::manifest::ContainerManifest {
            id: "ref-rust-api".to_string(),
            name: "reference-assistant".to_string(),
            sdk: Sdk::Rust,
            dev: false,
            launcher_type: crate::harness::manifest::Launcher::Api,
            image_tag: "ref-rust-api:latest".to_string(),
            harness_build_image: "ref-rust-api-harness-build".to_string(),
            port: Some(9090),
            requires: requires(&[], &[]),
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("baectl-checks-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Container-mode env resolution checks `<dir>/.env` *or* the host env
    /// (never only the host, unlike `local`); an empty `.env` value never
    /// counts as resolved.
    #[test]
    fn missing_env_container_checks_dotenv_and_host() {
        let dir = temp_dir("missing-env-container");
        std::fs::write(
            dir.join(ENV_FILE),
            "GITHUB_TOKEN=abc123\nBLANK_VAR=\n# comment\n",
        )
        .unwrap();
        std::env::set_var("BAECTL_TEST_HOST_ONLY_VAR", "present");

        let manifest = container_manifest();
        let req = requires(&[], &[]);
        let req = Requires {
            env: vec![
                "GITHUB_TOKEN".to_string(),              // resolved from .env
                "BAECTL_TEST_HOST_ONLY_VAR".to_string(), // resolved from host env
                "BLANK_VAR".to_string(),                 // present but empty -> unresolved
                "TRIAGE_REPO".to_string(),               // absent entirely -> unresolved
            ],
            ..req
        };
        let mut missing = missing_env_vars(&dir, &manifest, &req, None);
        missing.sort();
        assert_eq!(missing, vec!["BLANK_VAR", "TRIAGE_REPO"]);

        std::env::remove_var("BAECTL_TEST_HOST_ONLY_VAR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Check #2 (profile allowance) and check #3 (registry presence) fail for
    /// distinct reasons and neither implies the other: a profile can allow an
    /// MCP server name the registry never defined, and the registry can define
    /// a server no profile allows yet.
    #[test]
    fn registry_presence_is_distinct_from_profile_allowance() {
        let req = requires(&[], &["github"]);

        // Case A: the profile already allows "github" (check #2 compatible),
        // but `bae-config.toml` declares zero [[mcp.servers]] entries — check
        // #3 must still fail because no registry entry backs the name.
        let profile_allows = profile("p1", "a", &[], &["github"]);
        assert!(profile_compatible(&profile_allows, &req));
        let empty_registry = load_registry_from_text("");
        assert!(!empty_registry
            .mcp_server_names
            .iter()
            .any(|n| n == "github"));

        // Case B: the registry declares "github", but no profile allows it —
        // check #2 fails while check #3 would pass.
        let profile_missing = profile("p2", "b", &[], &[]);
        assert!(!profile_compatible(&profile_missing, &req));
        let full_registry = load_registry_from_text(
            "[[mcp.servers]]\nname = \"github\"\ntransport = \"http\"\nurl = \"https://api.githubcopilot.com/mcp/\"\n",
        );
        assert!(full_registry.mcp_server_names.iter().any(|n| n == "github"));
    }

    fn load_registry_from_text(text: &str) -> ConfigRegistry {
        parse_config_registry(text).expect("valid bae-config.toml registry fragment")
    }

    /// Whether `args` contains `pair` as two adjacent elements.
    fn window_has(args: &[String], pair: &[&str; 2]) -> bool {
        args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }
}
