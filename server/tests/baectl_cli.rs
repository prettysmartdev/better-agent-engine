//! Cross-crate integration tests: the real `baectl` binary driven as a
//! subprocess against the real admin router (work item 0004).
//!
//! The admin router boots in-process on an ephemeral loopback port over a fresh
//! temp-file SQLite database (auth enabled, a bootstrapped admin key), and the
//! `baectl` binary — built once as a test prerequisite from the sibling
//! `baectl/` crate — is exec'd against it. This proves the two independently
//! built binaries interoperate over HTTP, and (in the `auth create key` round
//! trip) that their two independent Argon2id implementations are compatible.
//! Everything is fully offline: no real network, no provider keys.
//!
//! Coverage (see `/awman/context/workflow/test-plan.md`):
//! - full CRUD lifecycle (create/get/list/update/delete profile; create/list/
//!   delete key), asserting output at each step;
//! - error surfaces (duplicate name, delete-with-active-keys, key-against-
//!   missing-profile, get/delete of a bogus id) with clean non-JSON messages;
//! - auto-pagination of `list profiles`/`list keys` vs. `--limit`/`--cursor`
//!   single-page behavior;
//! - auto-discovery precedence (key file vs. explicit `--admin-token`);
//! - `baectl auth create key` → server `BAE_ADMIN_KEY_HASH_FILE` ingest round
//!   trip (cross-crate Argon2id compatibility).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::Duration;

use baesrv::admin_auth::{self, AdminAuthConfig};
use baesrv::api::{admin, AppState};
use baesrv::store::{generate_id, keys, profiles, Store};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Building + locating the baectl binary (once for the whole test binary)
// ---------------------------------------------------------------------------

/// Build `baectl` (native/debug) from the sibling crate the first time it is
/// needed and return the path to the produced binary. Building it as a test
/// prerequisite is intentional: the two crates share no workspace, and
/// `COMPONENTS` runs `server` before `baectl`, so the binary may not exist yet.
fn baectl_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = workspace.join("baectl/Cargo.toml");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(&cargo)
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest)
            .status()
            .expect("spawn cargo build for baectl");
        assert!(status.success(), "building baectl failed");
        let bin = workspace.join("baectl/target/debug/baectl");
        assert!(bin.exists(), "baectl binary missing at {}", bin.display());
        bin
    })
    .clone()
}

/// A fresh `baectl` command with ambient BAE_* configuration scrubbed, so each
/// test controls address/token entirely via explicit flags/env.
fn baectl_cmd() -> Command {
    let mut c = Command::new(baectl_bin());
    c.env_remove("BAE_ADMIN_TOKEN")
        .env_remove("BAE_ADMIN_ADDR")
        .env_remove("BAE_ADMIN_KEY_FILE");
    c
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout was not JSON ({e}): {:?}", stdout_of(out)))
}
fn code(out: &Output) -> i32 {
    out.status.code().expect("process exited via a signal")
}

// ---------------------------------------------------------------------------
// Admin-router harness
// ---------------------------------------------------------------------------

/// A running admin router (auth enabled) with a bootstrapped admin key, over a
/// temp-file DB. The temp directory is removed on drop.
struct Harness {
    dir: PathBuf,
    store: Store,
    addr: String,
    /// The self-generated admin token, or empty for an ingest-only harness whose
    /// plaintext the caller supplies itself.
    token: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    /// Self-generating boot: the server mints an admin key and writes the
    /// plaintext file; `token` is read back from it.
    async fn new() -> Self {
        Self::boot(None).await
    }

    /// Ingest boot: the server ingests `hash_src` (a `baectl`-produced
    /// `admin-key-hash.pem`) and writes no plaintext file. `token` is left empty;
    /// the caller drives baectl with the paired plaintext.
    async fn ingesting(hash_src: &Path) -> Self {
        Self::boot(Some(hash_src.to_path_buf())).await
    }

    async fn boot(ingest: Option<PathBuf>) -> Self {
        let dir = std::env::temp_dir().join(format!("baesrv-baectl-{}", generate_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("test.db")).expect("open temp store");
        let key_file = dir.join("admin-key.pem");
        let hash_file = dir.join("admin-key-hash.pem");
        if let Some(src) = &ingest {
            std::fs::copy(src, &hash_file).expect("stage hash file");
        }
        let cfg = AdminAuthConfig {
            key_file: key_file.clone(),
            hash_file,
            rotate: false,
            disabled: false,
        };
        let enabled = admin_auth::bootstrap(&store, &cfg).expect("bootstrap");
        assert!(enabled);
        let token = if ingest.is_some() {
            String::new()
        } else {
            std::fs::read_to_string(&key_file)
                .unwrap()
                .trim()
                .to_string()
        };

        let state = AppState::with_mcp_registry(store.clone(), HashMap::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = admin::router(state, enabled);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let h = Harness {
            dir,
            store,
            addr: addr.to_string(),
            token,
        };
        h.wait_ready().await;
        h
    }

    async fn wait_ready(&self) {
        let client = reqwest::Client::new();
        for _ in 0..200 {
            if client
                .get(format!("http://{}/admin/v1/mcp-servers", self.addr))
                .send()
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("admin server did not become ready");
    }

    /// Run `baectl <args>` with the harness address + admin token.
    fn baectl(&self, args: &[&str]) -> Output {
        baectl_cmd()
            .arg("--admin-addr")
            .arg(&self.addr)
            .arg("--admin-token")
            .arg(&self.token)
            .args(args)
            .output()
            .expect("run baectl")
    }

    fn seed_profile(&self, name: &str) -> String {
        self.store
            .with_conn(|c| {
                profiles::create(
                    c,
                    &profiles::ProfileInput {
                        name: name.to_string(),
                        provider_config: json!({
                            "provider": "anthropic",
                            "base_url": "https://api.anthropic.com",
                            "model": "claude-mock-1",
                            "auth_token": "test-token",
                        }),
                        fallback_configs: json!([]),
                        mcp_servers: json!([]),
                        allowed_tools: json!([]),
                    },
                )
            })
            .expect("seed profile")
            .id
    }

    fn seed_client_key(&self, profile_id: &str, name: &str) {
        let generated = keys::generate_client_key();
        self.store
            .with_conn(|c| keys::insert_client_key(c, name, profile_id, &generated))
            .expect("seed client key");
    }
}

/// A CLI error message must be a clean one-liner (prefixed `baectl:`), never a
/// raw JSON body dumped to the user.
fn assert_clean_error(out: &Output) {
    assert_eq!(code(out), 1, "expected a runtime error (exit 1): {out:?}");
    let err = stderr_of(out);
    assert!(
        err.starts_with("baectl:"),
        "stderr must be a baectl message: {err:?}"
    );
    let body = err.trim_start_matches("baectl:").trim_start();
    assert!(
        !body.starts_with('{') && !body.starts_with('['),
        "stderr must not be a raw JSON dump: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Full CRUD lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_crud_lifecycle() {
    let h = Harness::new().await;

    // create profile
    let out = h.baectl(&[
        "create",
        "profile",
        "main",
        "anthropic",
        "claude-sonnet-4-6",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "create profile: {}", stderr_of(&out));
    let created = stdout_json(&out);
    let pid = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["name"], json!("main"));

    // get profile
    let out = h.baectl(&["get", "profile", &pid, "--json"]);
    assert_eq!(code(&out), 0);
    let got = stdout_json(&out);
    assert_eq!(got["id"], json!(pid));
    assert_eq!(got["provider_config"]["provider"], json!("anthropic"));
    assert_eq!(got["provider_config"]["model"], json!("claude-sonnet-4-6"));

    // list profiles (human table mentions the id and name)
    let out = h.baectl(&["list", "profiles"]);
    assert_eq!(code(&out), 0);
    let table = stdout_of(&out);
    assert!(
        table.contains(&pid) && table.contains("main"),
        "list: {table}"
    );

    // update profile (no --name → current name preserved; model replaced)
    let out = h.baectl(&[
        "update",
        "profile",
        &pid,
        "anthropic",
        "claude-opus-4-8",
        "--json",
    ]);
    assert_eq!(code(&out), 0, "update: {}", stderr_of(&out));
    let updated = stdout_json(&out);
    assert_eq!(updated["name"], json!("main"), "name is preserved");
    assert_eq!(
        updated["provider_config"]["model"],
        json!("claude-opus-4-8")
    );

    // create key
    let out = h.baectl(&["create", "key", "agent-1", &pid, "--json"]);
    assert_eq!(code(&out), 0, "create key: {}", stderr_of(&out));
    let key = stdout_json(&out);
    let kid = key["id"].as_str().unwrap().to_string();
    assert!(key["key"].as_str().unwrap().starts_with("bae_"));
    assert_eq!(key["profile_id"], json!(pid));
    // The "copy the key now" reminder goes to stderr, not stdout.
    assert!(stderr_of(&out).contains("copy the key now"));

    // list keys
    let out = h.baectl(&["list", "keys", "--json"]);
    assert_eq!(code(&out), 0);
    let arr = stdout_json(&out);
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["id"], json!(kid));

    // delete key, then delete profile (now unreferenced)
    let out = h.baectl(&["delete", "key", &kid]);
    assert_eq!(code(&out), 0);
    assert!(stdout_of(&out).contains("revoked key"));

    let out = h.baectl(&["delete", "profile", &pid]);
    assert_eq!(code(&out), 0);
    assert!(stdout_of(&out).contains("deleted profile"));

    // the profile is gone
    let out = h.baectl(&["get", "profile", &pid]);
    assert_eq!(code(&out), 1, "get of a deleted profile is a runtime error");
}

// ---------------------------------------------------------------------------
// Error surfaces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_surfaces_are_clean_and_correctly_coded() {
    let h = Harness::new().await;

    // duplicate profile name
    assert_eq!(
        code(&h.baectl(&["create", "profile", "dup", "anthropic", "m"])),
        0
    );
    let dup = h.baectl(&["create", "profile", "dup", "anthropic", "m"]);
    assert_clean_error(&dup);

    // delete-with-active-keys → profile_in_use, with the revoke suggestion
    let out = h.baectl(&["create", "profile", "inuse", "anthropic", "m", "--json"]);
    let pid = stdout_json(&out)["id"].as_str().unwrap().to_string();
    assert_eq!(code(&h.baectl(&["create", "key", "k", &pid])), 0);
    let blocked = h.baectl(&["delete", "profile", &pid]);
    assert_clean_error(&blocked);
    let msg = stderr_of(&blocked);
    assert!(
        msg.contains("baectl list keys"),
        "profile_in_use guidance: {msg}"
    );
    assert!(
        msg.contains("baectl delete key"),
        "profile_in_use guidance: {msg}"
    );

    // key against a missing profile → profile_unavailable, with the hint
    let missing = h.baectl(&["create", "key", "k2", "pro_does_not_exist"]);
    assert_clean_error(&missing);
    assert!(
        stderr_of(&missing).contains("does not exist or was deleted"),
        "profile_unavailable hint: {}",
        stderr_of(&missing)
    );

    // get / delete of a bogus id → not_found
    assert_clean_error(&h.baectl(&["get", "profile", "pro_bogus"]));
    assert_clean_error(&h.baectl(&["delete", "profile", "pro_bogus"]));
    assert_clean_error(&h.baectl(&["delete", "key", "key_bogus"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_lists_print_a_clear_message() {
    // Zero items → a clear empty-state line, not an empty table of headers.
    let h = Harness::new().await;
    let out = h.baectl(&["list", "profiles"]);
    assert_eq!(code(&out), 0, "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("no profiles found"), "{out:?}");
    let out = h.baectl(&["list", "keys"]);
    assert_eq!(code(&out), 0, "{}", stderr_of(&out));
    assert!(stdout_of(&out).contains("no keys found"), "{out:?}");
}

#[test]
fn unreachable_server_is_a_clean_connect_error() {
    // No server bound → "could not connect" guidance on stderr, exit 1 — never
    // a raw reqwest error/backtrace. Port 9 (discard) is reliably closed.
    let out = baectl_cmd()
        .args(["--admin-addr", "127.0.0.1:9", "list", "profiles"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    let err = stderr_of(&out);
    assert!(
        err.contains("could not connect to admin API at 127.0.0.1:9"),
        "clean connect message expected: {err}"
    );
    assert!(
        !err.contains("reqwest") && !err.to_lowercase().contains("panic"),
        "must not leak a raw transport error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Auto-pagination vs. single-page opt-out
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_profiles_auto_paginates_and_limit_opts_out() {
    let h = Harness::new().await;
    // Seed more than one default page (default limit is 50).
    for i in 0..51 {
        h.seed_profile(&format!("p{i:03}"));
    }

    // Default: auto-paginate → a flat JSON array of ALL items.
    let out = h.baectl(&["list", "profiles", "--json"]);
    assert_eq!(code(&out), 0);
    let all = stdout_json(&out);
    assert_eq!(
        all.as_array().unwrap().len(),
        51,
        "auto-pagination returns the full set"
    );

    // `--limit` opts into raw single-page behavior: exactly one page + a cursor.
    let out = h.baectl(&["list", "profiles", "--limit", "20", "--json"]);
    assert_eq!(code(&out), 0);
    let page = stdout_json(&out);
    assert_eq!(page["items"].as_array().unwrap().len(), 20);
    let cursor = page["next_cursor"]
        .as_str()
        .expect("a non-null cursor on a partial page");

    // `--cursor` fetches the next single page (still raw, no auto-follow).
    let out = h.baectl(&[
        "list", "profiles", "--limit", "20", "--cursor", cursor, "--json",
    ]);
    assert_eq!(code(&out), 0);
    let page2 = stdout_json(&out);
    assert_eq!(page2["items"].as_array().unwrap().len(), 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_keys_auto_paginates_and_limit_opts_out() {
    let h = Harness::new().await;
    let pid = h.seed_profile("keys-pagination");
    for i in 0..51 {
        h.seed_client_key(&pid, &format!("k{i:03}"));
    }

    let out = h.baectl(&["list", "keys", "--json"]);
    assert_eq!(code(&out), 0);
    assert_eq!(stdout_json(&out).as_array().unwrap().len(), 51);

    let out = h.baectl(&["list", "keys", "--limit", "20", "--json"]);
    assert_eq!(code(&out), 0);
    let page = stdout_json(&out);
    assert_eq!(page["items"].as_array().unwrap().len(), 20);
    assert!(
        page["next_cursor"].is_string(),
        "partial page carries a cursor"
    );
}

// ---------------------------------------------------------------------------
// Auto-discovery precedence
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn token_discovery_precedence() {
    let h = Harness::new().await;
    let real = &h.token;

    // A key file at BAE_ADMIN_KEY_FILE, with no explicit token, is auto-read
    // (written with a trailing newline, which baectl must trim).
    let good_file = h.dir.join("discovered-key.pem");
    std::fs::write(&good_file, format!("{real}\n")).unwrap();
    let out = baectl_cmd()
        .arg("--admin-addr")
        .arg(&h.addr)
        .env("BAE_ADMIN_KEY_FILE", &good_file)
        .args(["list", "profiles"])
        .output()
        .unwrap();
    assert_eq!(
        code(&out),
        0,
        "key file should authenticate: {}",
        stderr_of(&out)
    );

    // Point the key file at a WRONG token; with no explicit token that fails —
    // proving the file is really consulted.
    let stale_file = h.dir.join("stale-key.pem");
    std::fs::write(&stale_file, "bae_admin_wrongwrongwrong\n").unwrap();
    let out = baectl_cmd()
        .arg("--admin-addr")
        .arg(&h.addr)
        .env("BAE_ADMIN_KEY_FILE", &stale_file)
        .args(["list", "profiles"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 1, "a stale key file must fail auth");

    // An explicit --admin-token OVERRIDES the (stale) key file → success.
    let out = baectl_cmd()
        .arg("--admin-addr")
        .arg(&h.addr)
        .arg("--admin-token")
        .arg(real)
        .env("BAE_ADMIN_KEY_FILE", &stale_file)
        .args(["list", "profiles"])
        .output()
        .unwrap();
    assert_eq!(
        code(&out),
        0,
        "explicit token must win over a stale key file: {}",
        stderr_of(&out)
    );
}

// ---------------------------------------------------------------------------
// `baectl auth create key` → server hash ingest round trip (Argon2id compat)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auth_create_key_hash_ingest_round_trip() {
    // Generate a key pair locally with baectl (no server involved).
    let outdir = std::env::temp_dir().join(format!("baectl-keypair-{}", generate_id("")));
    std::fs::create_dir_all(&outdir).unwrap();
    let out = baectl_cmd()
        .args([
            "auth",
            "create",
            "key",
            "--name",
            "provisioned-admin",
            "--out-dir",
        ])
        .arg(&outdir)
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "auth create key: {}", stderr_of(&out));
    let key_file = outdir.join("admin-key.pem");
    let hash_file = outdir.join("admin-key-hash.pem");
    assert!(
        key_file.exists() && hash_file.exists(),
        "both artifacts written"
    );
    // Both artifacts are written with restrictive owner-only permissions —
    // admin-key.pem is a live credential; the hash file still deserves care.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for f in [&key_file, &hash_file] {
            let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} must be owner-only", f.display());
        }
    }

    // Boot a FRESH server that ingests baectl's hash file, then prove the paired
    // plaintext authenticates — the server verifies baectl's Argon2id hash with
    // no shared code.
    let plaintext = std::fs::read_to_string(&key_file)
        .unwrap()
        .trim()
        .to_string();
    let h = Harness::ingesting(&hash_file).await;
    let out = baectl_cmd()
        .arg("--admin-addr")
        .arg(&h.addr)
        .arg("--admin-token")
        .arg(&plaintext)
        .args(["list", "profiles"])
        .output()
        .unwrap();
    assert_eq!(
        code(&out),
        0,
        "baectl's plaintext must authenticate against the server that ingested its hash: {}",
        stderr_of(&out)
    );

    let _ = std::fs::remove_dir_all(&outdir);
}
