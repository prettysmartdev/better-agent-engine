//! The clap derive command tree and the runner that dispatches it.
//!
//! Command surface (verb-first, resource-typed positional), mapping 1:1 onto the
//! admin API's CRUD surface:
//!
//! ```text
//! baectl create profile <name> <primary_provider> [flags]
//! baectl list   profiles [--limit --cursor --json]
//! baectl get    profile  <id> [--json]
//! baectl update profile  <id> <primary_provider> [flags]
//! baectl delete profile  <id>
//! baectl create key <name> <profile_id> [--json]
//! baectl list   keys [--limit --cursor --json]
//! baectl delete key <id>
//! baectl auth   create key [--name --out-dir]   (local only — no API call)
//! ```
//!
//! `<primary_provider>` (and each `--fallback`) is the **name** of a
//! `[providers]` entry declared in `bae-config.toml` — the same
//! opt-in-by-name model `--mcp-server` already uses. baectl never builds or
//! sends provider config (URL, auth token, max tokens); that is entirely
//! operator-managed server-side config, resolved by the server at
//! session-creation time.
//!
//! Auto-configuration (zero flags in the documented `docker exec` deployment):
//! - **address** — `--admin-addr` > `BAE_ADMIN_ADDR` > `127.0.0.1:8081`.
//! - **token** — `--admin-token`/`BAE_ADMIN_TOKEN` > `--admin-key-file`/
//!   `BAE_ADMIN_KEY_FILE` > the default key file `/var/lib/bae/admin-key.pem`.
//!
//! Exit codes (per `aspec/uxui/cli.md`): 0 success / 1 runtime / 2 usage. clap
//! emits `2` for missing positionals and unknown flags for free; the value
//! validation we do ourselves ([`CliError::usage`]) matches it.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use crate::admin_client::{page_document, AdminClient, KeyBody, Page, ProfileBody};
use crate::error::CliError;
use crate::{keygen, output};

/// Environment-variable and default constants for auto-configuration.
const ENV_ADMIN_ADDR: &str = "BAE_ADMIN_ADDR";
const ENV_ADMIN_TOKEN: &str = "BAE_ADMIN_TOKEN";
const ENV_ADMIN_KEY_FILE: &str = "BAE_ADMIN_KEY_FILE";
const DEFAULT_ADMIN_ADDR: &str = "127.0.0.1:8081";
const DEFAULT_ADMIN_KEY_FILE: &str = "/var/lib/bae/admin-key.pem";

#[derive(Parser)]
#[command(
    name = "baectl",
    version,
    about = "Command-line client for the BAE admin API",
    long_about = "baectl is an HTTP wrapper over the BAE admin API (/admin/v1). \
                  When run inside the same container as baesrv it auto-configures \
                  its address and admin token, so no flags are needed."
)]
struct Cli {
    /// Admin API address `host:port` (or a full URL for an SSH tunnel).
    /// Overrides `BAE_ADMIN_ADDR`; defaults to `127.0.0.1:8081`.
    #[arg(long, global = true, value_name = "HOST:PORT")]
    admin_addr: Option<String>,

    /// Admin bearer token, sent verbatim as `Authorization: Bearer <token>`.
    /// Overrides `BAE_ADMIN_TOKEN` and any key file.
    #[arg(long, global = true, value_name = "TOKEN")]
    admin_token: Option<String>,

    /// Path to a file holding the plaintext admin key. Overrides
    /// `BAE_ADMIN_KEY_FILE`; the default probed path is
    /// `/var/lib/bae/admin-key.pem`.
    #[arg(long, global = true, value_name = "PATH")]
    admin_key_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a resource (profile or key).
    Create(CreateCmd),
    /// List resources (profiles or keys), auto-paginating by default.
    List(ListCmd),
    /// Get a single resource.
    Get(GetCmd),
    /// Replace a resource (full replacement).
    Update(UpdateCmd),
    /// Delete a resource.
    Delete(DeleteCmd),
    /// Local admin-key utilities (no API call).
    Auth(AuthCmd),
}

#[derive(Args)]
struct CreateCmd {
    #[command(subcommand)]
    resource: CreateResource,
}

#[derive(Subcommand)]
enum CreateResource {
    /// Create a profile.
    Profile(CreateProfileArgs),
    /// Create a client key bound to a profile.
    Key(CreateKeyArgs),
}

#[derive(Args)]
struct CreateProfileArgs {
    /// Unique profile name.
    name: String,
    #[command(flatten)]
    config: ProfileConfigArgs,
}

#[derive(Args)]
struct CreateKeyArgs {
    /// Human label for the key.
    name: String,
    /// Id of the profile the key is bound to.
    profile_id: String,
    /// Print the raw JSON response instead of a human summary.
    #[arg(long)]
    json: bool,
}

/// The provider-reference positional and flags shared by `create`/`update profile`.
#[derive(Args)]
struct ProfileConfigArgs {
    /// Name of a `[providers]` entry in `bae-config.toml` to use as this
    /// profile's primary provider. Not validated against the live registry at
    /// write time (mirrors `--mcp-server`) — an unresolvable name is only
    /// caught, fatally, at session-creation time.
    primary_provider: String,
    /// Fallback provider registry name, tried in order after the primary
    /// fails (repeatable).
    #[arg(long = "fallback", value_name = "NAME")]
    fallback: Vec<String>,
    /// MCP server name to enable (repeatable).
    #[arg(long = "mcp-server", value_name = "NAME")]
    mcp_server: Vec<String>,
    /// Client-side tool name to allow (repeatable).
    #[arg(long = "allowed-tool", value_name = "NAME")]
    allowed_tool: Vec<String>,
    /// Print the raw JSON response instead of a human summary.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListCmd {
    #[command(subcommand)]
    resource: ListResource,
}

#[derive(Subcommand)]
enum ListResource {
    /// List profiles.
    Profiles(ListArgs),
    /// List client keys.
    Keys(ListArgs),
}

#[derive(Args)]
struct ListArgs {
    /// Fetch a single page of at most N items (opts out of auto-pagination).
    #[arg(long, value_name = "N")]
    limit: Option<u32>,
    /// Fetch a single page starting at this opaque cursor (opts out of
    /// auto-pagination).
    #[arg(long, value_name = "C")]
    cursor: Option<String>,
    /// Print the raw JSON document instead of a human table.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct GetCmd {
    #[command(subcommand)]
    resource: GetResource,
}

#[derive(Subcommand)]
enum GetResource {
    /// Get one profile by id.
    Profile(GetArgs),
}

#[derive(Args)]
struct GetArgs {
    /// Resource id.
    id: String,
    /// Print the raw JSON document instead of a human summary.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct UpdateCmd {
    #[command(subcommand)]
    resource: UpdateResource,
}

#[derive(Subcommand)]
enum UpdateResource {
    /// Replace a profile (full replacement).
    Profile(UpdateProfileArgs),
}

#[derive(Args)]
struct UpdateProfileArgs {
    /// Id of the profile to replace.
    id: String,
    /// New name for the profile. When omitted, the existing name is preserved
    /// (a full PUT replacement still requires a name, so baectl fetches the
    /// current profile to keep it rather than dropping it).
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
    #[command(flatten)]
    config: ProfileConfigArgs,
}

#[derive(Args)]
struct DeleteCmd {
    #[command(subcommand)]
    resource: DeleteResource,
}

#[derive(Subcommand)]
enum DeleteResource {
    /// Delete (soft-delete) a profile by id.
    Profile(DeleteArgs),
    /// Revoke a client key by id.
    Key(DeleteArgs),
}

#[derive(Args)]
struct DeleteArgs {
    /// Resource id.
    id: String,
}

#[derive(Args)]
struct AuthCmd {
    #[command(subcommand)]
    action: AuthAction,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Create admin-key material locally (no API call).
    Create(AuthCreateCmd),
}

#[derive(Args)]
struct AuthCreateCmd {
    #[command(subcommand)]
    resource: AuthCreateResource,
}

#[derive(Subcommand)]
enum AuthCreateResource {
    /// Generate a pre-provisioned admin key pair (plaintext + hash files).
    Key(AuthCreateKeyArgs),
}

#[derive(Args)]
struct AuthCreateKeyArgs {
    /// Name recorded in the hash file (server display only).
    #[arg(long, default_value = "provisioned-admin", value_name = "NAME")]
    name: String,
    /// Directory to write `admin-key.pem` and `admin-key-hash.pem` into.
    #[arg(long, default_value = ".", value_name = "DIR")]
    out_dir: PathBuf,
}

/// Parse arguments, run the selected command, and return a process exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("baectl: {}", e.message());
            ExitCode::from(e.exit_code())
        }
    }
}

fn dispatch(cli: Cli) -> Result<(), CliError> {
    // `auth create key` is purely local — resolve nothing about the server.
    if let Command::Auth(AuthCmd {
        action: AuthAction::Create(AuthCreateCmd { resource }),
    }) = &cli.command
    {
        let AuthCreateResource::Key(args) = resource;
        return run_auth_create_key(args);
    }

    let client = build_client(&cli)?;
    match cli.command {
        Command::Create(CreateCmd { resource }) => match resource {
            CreateResource::Profile(args) => run_create_profile(&client, args),
            CreateResource::Key(args) => run_create_key(&client, args),
        },
        Command::List(ListCmd { resource }) => match resource {
            ListResource::Profiles(args) => run_list(
                &client,
                "/admin/v1/profiles",
                args,
                output::print_profiles_table,
            ),
            ListResource::Keys(args) => {
                run_list(&client, "/admin/v1/keys", args, output::print_keys_table)
            }
        },
        Command::Get(GetCmd { resource }) => match resource {
            GetResource::Profile(args) => {
                let profile = client.get_profile(&args.id)?;
                render(args.json, &profile, output::print_profile);
                Ok(())
            }
        },
        Command::Update(UpdateCmd { resource }) => match resource {
            UpdateResource::Profile(args) => run_update_profile(&client, args),
        },
        Command::Delete(DeleteCmd { resource }) => match resource {
            DeleteResource::Profile(args) => {
                client.delete_profile(&args.id)?;
                println!("deleted profile {}", args.id);
                Ok(())
            }
            DeleteResource::Key(args) => {
                client.delete_key(&args.id)?;
                println!("revoked key {}", args.id);
                Ok(())
            }
        },
        // `auth` is fully handled above.
        Command::Auth(_) => unreachable!("auth handled before client build"),
    }
}

/// Resolve the admin address and token, then construct the client.
fn build_client(cli: &Cli) -> Result<AdminClient, CliError> {
    let addr = cli
        .admin_addr
        .clone()
        .or_else(|| std::env::var(ENV_ADMIN_ADDR).ok())
        .unwrap_or_else(|| DEFAULT_ADMIN_ADDR.to_string());
    let token = resolve_token(cli)?;
    Ok(AdminClient::new(&addr, token))
}

/// Resolve the admin token by precedence: explicit `--admin-token`/env, then an
/// explicit key file (flag/env), then the default probed key-file path.
///
/// An *explicitly* named key file that cannot be read is a hard error (the
/// operator asked for it); the default path silently yields `None` if absent
/// (baectl then relies on the server not enforcing auth, or surfaces the
/// three-option 401 guidance when it does).
fn resolve_token(cli: &Cli) -> Result<Option<String>, CliError> {
    if let Some(t) = &cli.admin_token {
        return Ok(Some(t.clone()));
    }
    if let Ok(t) = std::env::var(ENV_ADMIN_TOKEN) {
        if !t.is_empty() {
            return Ok(Some(t));
        }
    }
    // Explicit key file (flag beats env).
    let explicit_file = cli
        .admin_key_file
        .clone()
        .or_else(|| std::env::var(ENV_ADMIN_KEY_FILE).ok().map(PathBuf::from));
    if let Some(path) = explicit_file {
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            CliError::runtime(format!(
                "could not read admin key file {}: {e}",
                path.display()
            ))
        })?;
        return Ok(Some(trim_token(&raw)));
    }
    // Default probed path — absent is fine.
    match std::fs::read_to_string(DEFAULT_ADMIN_KEY_FILE) {
        Ok(raw) => Ok(Some(trim_token(&raw))),
        Err(_) => Ok(None),
    }
}

/// Trim surrounding whitespace/newline from a key file's contents. The server
/// writes the plaintext with a trailing newline, so readers MUST trim.
fn trim_token(raw: &str) -> String {
    raw.trim().to_string()
}

/// `create profile`.
fn run_create_profile(client: &AdminClient, args: CreateProfileArgs) -> Result<(), CliError> {
    let body = build_profile_body(args.name, &args.config);
    let json = args.config.json;
    let created = client.create_profile(&body)?;
    render(json, &created, output::print_profile_created);
    Ok(())
}

/// `update profile` — full replacement. Preserves the current name unless
/// `--name` is given (a PUT body always needs a name).
fn run_update_profile(client: &AdminClient, args: UpdateProfileArgs) -> Result<(), CliError> {
    let name = match args.name.clone() {
        Some(n) => n,
        None => {
            let current = client.get_profile(&args.id)?;
            current
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    CliError::runtime(
                        "could not determine the profile's current name to preserve; \
                         pass --name explicitly",
                    )
                })?
        }
    };
    let body = build_profile_body(name, &args.config);
    let json = args.config.json;
    let updated = client.replace_profile(&args.id, &body)?;
    render(json, &updated, output::print_profile);
    Ok(())
}

/// `create key`.
fn run_create_key(client: &AdminClient, args: CreateKeyArgs) -> Result<(), CliError> {
    let body = KeyBody {
        name: args.name,
        profile_id: args.profile_id,
    };
    let created = client.create_key(&body)?;
    render(args.json, &created, output::print_key_created);
    // The plaintext `key` is shown exactly once (in both modes). Warn on stderr
    // so the reminder never lands in stdout / a captured result.
    eprintln!("baectl: copy the key now — it cannot be retrieved again");
    Ok(())
}

/// `list profiles` / `list keys`, dispatching to the right table renderer.
fn run_list(
    client: &AdminClient,
    path: &str,
    args: ListArgs,
    table: fn(&[Value]),
) -> Result<(), CliError> {
    // `--limit`/`--cursor` opt into raw single-page behavior for scripting;
    // otherwise auto-paginate and return the full set.
    if args.limit.is_some() || args.cursor.is_some() {
        let page: Page = client.list_page(path, args.cursor.as_deref(), args.limit)?;
        if args.json {
            output::print_json(&page_document(&page));
        } else {
            table(&page.items);
        }
    } else {
        let items = client.list_all(path)?;
        if args.json {
            output::print_json(&Value::Array(items));
        } else {
            table(&items);
        }
    }
    Ok(())
}

/// `auth create key` — local keygen, no network.
fn run_auth_create_key(args: &AuthCreateKeyArgs) -> Result<(), CliError> {
    let material = keygen::generate().map_err(CliError::runtime)?;

    let key_path = args.out_dir.join("admin-key.pem");
    let hash_path = args.out_dir.join("admin-key-hash.pem");

    // `admin-key.pem` — plaintext token, single line + newline, matching the
    // server's own file format (readers trim). Live credential → 0600.
    write_secret(&key_path, &format!("{}\n", material.plaintext))?;

    // `admin-key-hash.pem` — the JSON the server ingests at boot. Field names
    // (`key_hash`, `prefix`, `name`) match server/src/admin_auth.rs exactly.
    let doc = serde_json::json!({
        "key_hash": material.key_hash,
        "prefix": material.prefix,
        "name": args.name,
    });
    let hash_body = serde_json::to_string_pretty(&doc)
        .map_err(|e| CliError::runtime(format!("failed to serialize admin-key-hash.pem: {e}")))?;
    write_secret(&hash_path, &format!("{hash_body}\n"))?;

    // stdout: the two paths (scriptable). stderr: the handling guidance.
    println!("{}", key_path.display());
    println!("{}", hash_path.display());
    eprintln!(
        "baectl: wrote admin key pair.\n\
         - {key}: the plaintext admin token — LIVE CREDENTIAL, keep it secret; \
         place it where baectl/operators run (at BAE_ADMIN_KEY_FILE).\n\
         - {hash}: the Argon2id hash — drop onto every replica's data volume at \
         BAE_ADMIN_KEY_HASH_FILE before first boot.",
        key = key_path.display(),
        hash = hash_path.display(),
    );
    Ok(())
}

/// Write a file with owner-only (0600) permissions where the platform supports
/// it. baectl ships as a Linux static binary, so this is the normal path.
fn write_secret(path: &std::path::Path, contents: &str) -> Result<(), CliError> {
    let map_err =
        |e: std::io::Error| CliError::runtime(format!("could not write {}: {e}", path.display()));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(map_err)?;
        // `mode(0o600)` only applies when the file is *created*; overwriting a
        // pre-existing file would keep its old (possibly looser) permissions,
        // so clamp explicitly before the secret is written.
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(map_err)?;
        f.write_all(contents.as_bytes()).map_err(map_err)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents).map_err(map_err)?;
    }
    Ok(())
}

/// Render a value as raw JSON (when `json`) or via a human printer.
fn render(json: bool, value: &Value, human: fn(&Value)) {
    if json {
        output::print_json(value);
    } else {
        human(value);
    }
}

/// Build the admin-API profile body from CLI args. `primary_provider` and
/// `fallback_providers` are passed through as registry name references — no
/// validation, defaulting, or shape-building is needed here (unlike the old
/// inline-config design), mirroring how `mcp_server`/`allowed_tool` are
/// already passed through untouched.
fn build_profile_body(name: String, config: &ProfileConfigArgs) -> ProfileBody {
    ProfileBody {
        name,
        primary_provider: config.primary_provider.clone(),
        fallback_providers: config.fallback.clone(),
        mcp_servers: config.mcp_server.clone(),
        allowed_tools: config.allowed_tool.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Parse a full `create profile` invocation and return its args.
    fn parse_create_profile(args: &[&str]) -> CreateProfileArgs {
        match Cli::try_parse_from(args).unwrap().command {
            Command::Create(CreateCmd {
                resource: CreateResource::Profile(a),
            }) => a,
            _ => panic!("expected `create profile`"),
        }
    }

    /// Expect a clap parse failure and return it (`Cli` is not `Debug`, so we
    /// cannot use `unwrap_err`).
    fn parse_err(args: &[&str]) -> clap::Error {
        match Cli::try_parse_from(args) {
            Ok(_) => panic!("expected a parse error for {args:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn cli_parses_full_create_profile() {
        let cli = Cli::try_parse_from([
            "baectl",
            "create",
            "profile",
            "main",
            "anthropic-sonnet",
            "--allowed-tool",
            "get_current_time",
            "--mcp-server",
            "filesystem",
            "--fallback",
            "anthropic-haiku",
        ])
        .unwrap();
        match cli.command {
            Command::Create(CreateCmd {
                resource: CreateResource::Profile(a),
            }) => {
                assert_eq!(a.name, "main");
                assert_eq!(a.config.primary_provider, "anthropic-sonnet");
                assert_eq!(a.config.allowed_tool, vec!["get_current_time"]);
                assert_eq!(a.config.mcp_server, vec!["filesystem"]);
                assert_eq!(a.config.fallback, vec!["anthropic-haiku"]);
            }
            _ => panic!("wrong command parsed"),
        }
    }

    #[test]
    fn missing_positional_is_usage_error() {
        // `create profile main` with no primary_provider → clap usage error (2).
        // `Cli` is not `Debug`, so match rather than `unwrap_err`.
        let err = match Cli::try_parse_from(["baectl", "create", "profile", "main"]) {
            Ok(_) => panic!("expected a missing-argument error"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        // clap maps this to process exit code 2 (usage error).
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn every_subcommand_rejects_omitted_positionals() {
        // Each subcommand's required positionals, when omitted, are a clap usage
        // error mapped to process exit code 2.
        for args in [
            vec!["baectl", "create", "profile"], // name, primary_provider
            vec!["baectl", "create", "profile", "only-name"], // primary_provider
            vec!["baectl", "get", "profile"],    // id
            vec!["baectl", "update", "profile", "pro_1"], // primary_provider
            vec!["baectl", "delete", "profile"], // id
            vec!["baectl", "create", "key", "only-name"], // profile_id
            vec!["baectl", "delete", "key"],     // id
        ] {
            let e = parse_err(&args);
            assert_eq!(
                e.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "args {args:?} should be a missing-argument error"
            );
            assert_eq!(e.exit_code(), 2, "args {args:?} should exit 2");
        }
    }

    #[test]
    fn repeatable_flags_collect_all_occurrences() {
        let a = parse_create_profile(&[
            "baectl",
            "create",
            "profile",
            "m",
            "anthropic-sonnet",
            "--fallback",
            "anthropic-haiku",
            "--fallback",
            "openai-gpt",
            "--mcp-server",
            "filesystem",
            "--mcp-server",
            "fetch",
            "--allowed-tool",
            "a",
            "--allowed-tool",
            "b",
            "--allowed-tool",
            "c",
        ]);
        assert_eq!(a.config.fallback, vec!["anthropic-haiku", "openai-gpt"]);
        assert_eq!(a.config.mcp_server, vec!["filesystem", "fetch"]);
        assert_eq!(a.config.allowed_tool, vec!["a", "b", "c"]);
    }

    #[test]
    fn create_profile_body_matches_documented_shape() {
        // The JSON built for `POST /admin/v1/profiles` must match
        // docs/reference/admin-api.md field-for-field.
        let a = parse_create_profile(&[
            "baectl",
            "create",
            "profile",
            "main",
            "anthropic-sonnet",
            "--mcp-server",
            "filesystem",
            "--allowed-tool",
            "get_current_time",
            "--fallback",
            "openai-gpt",
        ]);
        let body = build_profile_body(a.name, &a.config);
        let v = serde_json::to_value(&body).unwrap();

        // Exactly the documented top-level keys — no more, no fewer.
        let keys: std::collections::BTreeSet<&str> =
            v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "allowed_tools",
                "fallback_providers",
                "mcp_servers",
                "name",
                "primary_provider",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(v["name"], json!("main"));
        assert_eq!(v["primary_provider"], json!("anthropic-sonnet"));
        assert_eq!(v["mcp_servers"], json!(["filesystem"]));
        assert_eq!(v["allowed_tools"], json!(["get_current_time"]));
        assert_eq!(v["fallback_providers"], json!(["openai-gpt"]));
    }

    #[test]
    fn update_profile_body_uses_name_flag_and_full_shape() {
        let a = match Cli::try_parse_from([
            "baectl",
            "update",
            "profile",
            "pro_1",
            "openai-gpt",
            "--name",
            "renamed",
        ])
        .unwrap()
        .command
        {
            Command::Update(UpdateCmd {
                resource: UpdateResource::Profile(a),
            }) => a,
            _ => panic!("expected `update profile`"),
        };
        let name = a.name.clone().expect("--name was passed");
        let body = build_profile_body(name, &a.config);
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["name"], json!("renamed"));
        assert_eq!(v["primary_provider"], json!("openai-gpt"));
        // Unset repeatable/list fields serialize as empty arrays (a full PUT
        // replacement, so the server receives explicit empties).
        assert_eq!(v["fallback_providers"], json!([]));
        assert_eq!(v["mcp_servers"], json!([]));
        assert_eq!(v["allowed_tools"], json!([]));
    }

    #[test]
    fn create_key_body_matches_documented_shape() {
        let body = KeyBody {
            name: "my-agent".to_string(),
            profile_id: "pro_x".to_string(),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v, json!({ "name": "my-agent", "profile_id": "pro_x" }));
    }

    #[test]
    fn empty_repeatable_flags_are_empty_vecs() {
        let cli = Cli::try_parse_from(["baectl", "create", "profile", "main", "anthropic-sonnet"])
            .unwrap();
        if let Command::Create(CreateCmd {
            resource: CreateResource::Profile(a),
        }) = cli.command
        {
            assert!(a.config.allowed_tool.is_empty());
            assert!(a.config.mcp_server.is_empty());
            assert!(a.config.fallback.is_empty());
        } else {
            panic!("wrong command");
        }
    }
}
