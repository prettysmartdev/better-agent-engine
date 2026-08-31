//! The six readiness checks shared by `baectl ready` and `baectl run`.
//!
//! Both verbs reach the admin API the same way `setup`'s launch step does — by
//! exec'ing the in-container `baectl` through the shared [`crate::engine`]
//! wrapper, never a host-side connection to the loopback-only admin port. This
//! module runs the checks, optionally applies the safe (#2/#4) admin-API fixes,
//! and — on full success — produces the [`Resolved`] record the caller persists
//! to `resolved.json`.
//!
//! The three [`FixMode`]s are the only behavioural difference between the two
//! verbs' check passes:
//! - [`FixMode::Report`] — `baectl ready` with no `--fix`: print ✓/✗ and the
//!   command that *would* fix #2/#4, mutate nothing.
//! - [`FixMode::Prompt`] — `baectl ready --fix`: same report, then a single
//!   `Apply the N safe fix(es) above? [y/N]` confirmation (asked even without a
//!   TTY) before applying #2/#4.
//! - [`FixMode::Auto`] — `baectl run`: apply the #2/#4 fixes with no prompt,
//!   printing exactly what was fixed. This is the non-interactive fast path.
//!
//! Checks #3 (MCP registry) and #5 (env vars) are always print-only — they
//! require a server restart or a host/​`.env` edit that these verbs will not do
//! unattended — so an unresolved #3/#5 makes the whole pass fail in every mode.

use std::io::{self, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::engine::{detect_engine, Engine, EngineKind, APPLE_SCRIPT, COMPOSE_FILE};
use crate::error::CliError;
use crate::harness::manifest::{BuildManifest, Requires, Resolved};
use crate::setup::{parse_config_registry, ConfigRegistry, CONFIG_FILE, ENV_FILE};

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

/// The URL a `run`-launched harness uses to reach `baesrv`:
/// - `kind: "local"` runs on the host → the published `localhost` port.
/// - `kind: "container"` runs in a standalone container off `setup`'s network →
///   the host gateway alias (`host.docker.internal`, which `run` maps in on
///   Linux via `--add-host`). This is the documented rough edge `--server-url`
///   overrides when detection guesses wrong.
pub(crate) fn default_server_url(manifest: &BuildManifest, port: u16) -> String {
    match manifest {
        BuildManifest::Local(_) => format!("http://localhost:{port}"),
        BuildManifest::Container(_) => format!("http://host.docker.internal:{port}"),
    }
}

/// Run the six checks against `manifest`, applying the #2/#4 fixes per `mode`.
/// Returns `Some(Resolved)` when every gating check passed (the caller then
/// persists it / launches), or `None` when an unresolved check means exit 1.
///
/// `prior` is any existing `resolved.json`: a still-valid profile/key reference
/// in it is *re-validated and reused* (not trusted blindly), so a second
/// `ready`/`run` against a fully provisioned target makes zero admin mutations.
pub(crate) fn evaluate(
    dir: &Path,
    manifest: &BuildManifest,
    prior: Option<&Resolved>,
    mode: FixMode,
) -> Result<Option<Resolved>, CliError> {
    let requires = manifest.requires();

    // -- Check #1: server reachable ----------------------------------------
    let server = match resolve_server(dir) {
        Some(s) => s,
        None => {
            print_check(false, "server reachable");
            println!(
                "    no `baectl setup` has been run in {} — run `baectl setup` first",
                dir.display()
            );
            return Ok(None);
        }
    };
    let profiles = match list_json(&server.engine, dir, "profiles") {
        Ok(p) => {
            print_check(true, "server reachable");
            p
        }
        Err(_) => {
            print_check(false, "server reachable");
            println!(
                "    could not reach the admin API through the container — is the server \
                 running? launch it with `baectl setup` (choose Launch), then retry"
            );
            return Ok(None);
        }
    };

    let registry = load_registry(dir)?;

    // -- Check #2: a compatible profile (found, or a fix to make one) -------
    let compatible: Vec<&Value> = profiles
        .iter()
        .filter(|p| profile_compatible(p, requires))
        .collect();
    // Prefer the previously resolved profile when it is still compatible, for
    // stability across runs; else the first compatible profile.
    let mut resolved_profile: Option<Value> = prior
        .and_then(|pr| compatible.iter().find(|p| field(p, "id") == pr.profile_id))
        .or_else(|| compatible.first())
        .map(|p| (*p).clone());

    let profile_fix = if resolved_profile.is_some() {
        None
    } else if let Some(target) = pick_widen_target(&profiles, prior) {
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

    match (&resolved_profile, &profile_fix) {
        (Some(p), _) => print_check(
            true,
            &format!(
                "compatible profile ({}, {})",
                field(p, "id"),
                field(p, "name")
            ),
        ),
        (None, Some(fix)) => {
            print_check(false, "compatible profile");
            print_profile_fix_hint(fix, requires, manifest, mode);
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
        print_check(true, "required MCP servers registered");
    } else {
        print_check(
            false,
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
            true,
            &format!("client key ({key_id}) — reusing saved credential"),
        );
    } else {
        print_check(false, "client key with a stored secret");
        print_key_fix_hint(anticipated_pid.as_deref(), manifest, mode);
    }

    // -- Check #5: required env vars resolvable (always print-only) ---------
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
    let provider_env = anticipated_primary.as_ref().and_then(|primary| {
        registry.as_ref().and_then(|r| {
            r.providers
                .iter()
                .find(|(n, _)| n == primary)
                .and_then(|(_, e)| e.clone())
        })
    });
    let missing_env = missing_env_vars(dir, manifest, requires, provider_env.as_deref());
    let env_ok = missing_env.is_empty();
    if env_ok {
        print_check(true, "required env vars resolvable");
    } else {
        print_check(
            false,
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

    // -- Apply the safe (#2/#4) fixes per mode -----------------------------
    let fix_count = profile_fix
        .as_ref()
        .map(|f| matches!(f, ProfileFix::Update { .. } | ProfileFix::Create { .. }) as usize)
        .unwrap_or(0)
        + key_needs_create as usize;

    let apply = match mode {
        FixMode::Report => false,
        FixMode::Auto => fix_count > 0,
        FixMode::Prompt => {
            if fix_count == 0 {
                false
            } else {
                confirm_apply(fix_count)
            }
        }
    };

    if apply {
        // #2: create/update the profile (additive). An Impossible fix cannot be
        // applied — surface it and fail.
        if let Some(fix) = &profile_fix {
            match fix {
                ProfileFix::Update { target } => {
                    let updated = apply_update(&server.engine, dir, target, requires)?;
                    println!(
                        "fixed: widened profile {} — allowed_tools/mcp_servers now cover the harness",
                        field(&updated, "id")
                    );
                    resolved_profile = Some(updated);
                }
                ProfileFix::Create { primary } => {
                    let created = apply_create(&server.engine, dir, manifest, primary, requires)?;
                    println!(
                        "fixed: created profile {} ({})",
                        field(&created, "id"),
                        field(&created, "name")
                    );
                    resolved_profile = Some(created);
                }
                ProfileFix::Impossible => {
                    return Err(CliError::runtime(
                        "cannot create a compatible profile: no providers are registered in \
                         bae-config.toml — run `baectl setup` to configure one",
                    ));
                }
            }
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

    // -- Gate: every gating check must be resolved -------------------------
    let profile_resolved = resolved_profile.is_some();
    let key_resolved = key_reuse.is_some();
    if !(profile_resolved && mcp_ok && key_resolved && env_ok) {
        return Ok(None);
    }

    let profile = resolved_profile.expect("gated on Some");
    let (key_id, plaintext) = key_reuse.expect("gated on Some");
    Ok(Some(Resolved {
        profile_id: field(&profile, "id").to_string(),
        profile_name: field(&profile, "name").to_string(),
        key_id,
        // baectl only ever persists a plaintext it legitimately holds: one it
        // just created, or one carried forward from a prior `resolved.json`. It
        // never fabricates a secret for a pre-existing key it cannot recover.
        client_key_plaintext: Some(plaintext),
        server_url: default_server_url(manifest, server.host_client_port),
        max_url: server.max_url,
        // Recorded by *name* only, never by value: `run` re-reads the value from
        // `.env`/the host env at launch time so a container gets the same
        // provider token check #5 accepted.
        provider_env,
    }))
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
/// one if it still exists, else a profile named `default`, else the first.
fn pick_widen_target<'a>(profiles: &'a [Value], prior: Option<&Resolved>) -> Option<&'a Value> {
    if profiles.is_empty() {
        return None;
    }
    prior
        .and_then(|pr| profiles.iter().find(|p| field(p, "id") == pr.profile_id))
        .or_else(|| profiles.iter().find(|p| field(p, "name") == "default"))
        .or_else(|| profiles.first())
}

/// Whether a profile's `allowed_tools` and `mcp_servers` both cover `requires`.
fn profile_compatible(profile: &Value, requires: &Requires) -> bool {
    let tools = string_array(profile.get("allowed_tools"));
    let servers = string_array(profile.get("mcp_servers"));
    is_superset(&tools, &requires.allowed_tools) && is_superset(&servers, &requires.mcp_servers)
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
/// additively: the profile's existing primary/name/fallbacks preserved verbatim
/// (a full PUT replacement would otherwise drop them), and its tools/servers
/// unioned with `requires`. `--json` is *not* appended (callers add it for
/// exec; the display path shows it without).
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
/// provider's auth-token var. `local` checks the host process env (what `run`
/// inherits); `container` checks `<dir>/.env` or the host env (what `run`
/// passes through). `BAE_SERVER_URL`/`BAE_CLIENT_KEY` are excluded — `run` sets
/// them itself.
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
        BuildManifest::Container(_) => dotenv_keys(dir),
        BuildManifest::Local(_) => Vec::new(),
    };
    needed
        .into_iter()
        .filter(|var| !env_resolvable(var, &dotenv))
        .collect()
}

/// Whether `var` resolves from the host environment or (container) from `.env`.
fn env_resolvable(var: &str, dotenv: &[String]) -> bool {
    if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
        return true;
    }
    dotenv.iter().any(|k| k == var)
}

/// The `KEY` names present in `<dir>/.env` (values not needed here).
fn dotenv_keys(dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(dir.join(ENV_FILE)).unwrap_or_default();
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

fn print_profile_fix_hint(
    fix: &ProfileFix,
    requires: &Requires,
    manifest: &BuildManifest,
    mode: FixMode,
) {
    let suffix = fix_suffix(mode);
    match fix {
        ProfileFix::Update { target } => {
            let cmd = shell_join(&update_fix_args(target, requires));
            println!("    widen the existing profile (additive): baectl {cmd}{suffix}");
        }
        ProfileFix::Create { primary } => {
            let cmd = shell_join(&create_fix_args(manifest, primary, requires));
            println!("    create a compatible profile: baectl {cmd}{suffix}");
        }
        ProfileFix::Impossible => {
            println!(
                "    no providers are registered in {CONFIG_FILE}; run `baectl setup` to \
                 configure one first"
            );
        }
    }
}

fn print_key_fix_hint(profile_id: Option<&str>, manifest: &BuildManifest, mode: FixMode) {
    let suffix = fix_suffix(mode);
    match profile_id {
        Some(pid) => println!(
            "    create a client key baectl can hand to `run`: baectl create key {} {pid}{suffix}",
            manifest.id()
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

/// Join argument tokens for display, quoting any that contain whitespace.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars().any(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn print_check(ok: bool, label: &str) {
    println!("{} {}", if ok { "✓" } else { "✗" }, label);
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
        assert_eq!(default_server_url(&local, 8080), "http://localhost:8080");
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
            default_server_url(&container, 8080),
            "http://host.docker.internal:8080"
        );
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
