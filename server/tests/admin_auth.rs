//! Server-side admin-port authentication tests (work item 0004).
//!
//! These exercise the bootstrap lifecycle in [`baesrv::admin_auth`] and the
//! enforcement middleware in `baesrv::api::admin` against the **real** admin
//! router, bound on an ephemeral loopback port over a fresh temp-file SQLite
//! database — the same harness shape the existing `integration.rs` uses. Every
//! test is fully offline: no real network, no provider keys.
//!
//! Coverage (see `/awman/context/workflow/test-plan.md`):
//! - first-boot self-generate (key file, `0600`, authenticates);
//! - second boot is a no-op (row count + file byte-identical);
//! - pre-provisioned hash ingestion (verbatim, no plaintext file written);
//! - `--rotate-admin-key` (old key dies, fresh material, hash file ignored);
//! - `--dangerously-disable-admin-auth` (no key, no file, open routes);
//! - the `--rotate-admin-key` + disable usage-error combination (exit 2, no FS
//!   or DB touched) — driven through the real `baesrv` binary;
//! - enforcement across every `/admin/v1/*` route, including rejection of a
//!   client-role key on the admin port.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use baesrv::admin_auth::{self, AdminAuthConfig};
use baesrv::api::{admin, AppState};
use baesrv::store::{generate_id, keys, profiles, Store};
use serde_json::json;

/// A temp-DB harness for the admin-auth tests. The temp directory (holding the
/// SQLite database and any key/hash files) is removed on drop.
struct Harness {
    dir: PathBuf,
    store: Store,
    key_file: PathBuf,
    hash_file: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Harness {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("baesrv-adminauth-{}", generate_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("test.db")).expect("open temp store");
        let key_file = dir.join("admin-key.pem");
        let hash_file = dir.join("admin-key-hash.pem");
        Harness {
            dir,
            store,
            key_file,
            hash_file,
        }
    }

    fn config(&self, rotate: bool, disabled: bool) -> AdminAuthConfig {
        AdminAuthConfig {
            key_file: self.key_file.clone(),
            hash_file: self.hash_file.clone(),
            rotate,
            disabled,
        }
    }

    /// Run the bootstrap and return whether enforcement is enabled.
    fn bootstrap(&self, rotate: bool, disabled: bool) -> bool {
        admin_auth::bootstrap(&self.store, &self.config(rotate, disabled)).expect("bootstrap")
    }

    /// Number of active `role='admin'` rows.
    fn active_admin_rows(&self) -> i64 {
        self.store.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM keys WHERE role = 'admin' AND deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        })
    }

    /// The single active admin row's stored `key_hash` (for verbatim-ingest checks).
    fn active_admin_hash(&self) -> String {
        self.store.with_conn(|c| {
            c.query_row(
                "SELECT key_hash FROM keys WHERE role = 'admin' AND deleted_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap()
        })
    }

    /// Read + trim the plaintext admin token from the key file.
    fn read_key(&self) -> String {
        std::fs::read_to_string(&self.key_file)
            .unwrap()
            .trim()
            .to_string()
    }

    /// Seed a profile directly in the store, returning its id.
    fn seed_profile(&self, name: &str) -> String {
        self.store
            .with_conn(|c| {
                profiles::create(
                    c,
                    &profiles::ProfileInput {
                        name: name.to_string(),
                        provider_config: json!("anthropic-sonnet"),
                        fallback_configs: json!([]),
                        mcp_servers: json!([]),
                        allowed_tools: json!([]),
                        available_sandboxes: json!([]),
                    },
                )
            })
            .expect("seed profile")
            .id
    }

    /// Seed a client-role key bound to `profile_id`, returning its plaintext.
    fn seed_client_key(&self, profile_id: &str) -> String {
        let generated = keys::generate_client_key();
        self.store
            .with_conn(|c| keys::insert_client_key(c, "seeded-client", profile_id, &generated))
            .expect("seed client key");
        generated.plaintext
    }

    /// Boot the admin router (auth on/off) on an ephemeral port; returns
    /// `(base_url, client)` once it is accepting connections.
    async fn serve(&self, auth_enabled: bool) -> (String, reqwest::Client) {
        let state = AppState::with_mcp_registry(self.store.clone(), HashMap::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = admin::router(state, auth_enabled);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        let client = reqwest::Client::new();
        wait_ready(&client, &base).await;
        (base, client)
    }
}

/// Poll the admin listener until it accepts a connection (any HTTP status,
/// including 401, means it is up).
async fn wait_ready(client: &reqwest::Client, base: &str) {
    for _ in 0..200 {
        if client
            .get(format!("{base}/admin/v1/mcp-servers"))
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

/// GET `path` with an optional bearer token; returns the HTTP status.
async fn status_of(client: &reqwest::Client, base: &str, path: &str, token: Option<&str>) -> u16 {
    let mut rb = client.get(format!("{base}{path}"));
    if let Some(t) = token {
        rb = rb.bearer_auth(t);
    }
    rb.send().await.unwrap().status().as_u16()
}

// ---------------------------------------------------------------------------
// Bootstrap lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_boot_self_generates_key_file_0600_and_authenticates() {
    let h = Harness::new();
    let enabled = h.bootstrap(false, false);
    assert!(enabled, "enforcement enabled after self-generate");

    // Exactly one admin row, and a key file written.
    assert_eq!(h.active_admin_rows(), 1);
    let token = h.read_key();
    assert!(token.starts_with("bae_admin_"), "token: {token}");

    // 0600 permissions on the plaintext key file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&h.key_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be owner-only");
    }

    // The plaintext authenticates against a real admin endpoint; no/garbage
    // tokens do not.
    let (base, client) = h.serve(enabled).await;
    assert_eq!(
        status_of(&client, &base, "/admin/v1/profiles", Some(&token)).await,
        200
    );
    assert_eq!(
        status_of(&client, &base, "/admin/v1/profiles", None).await,
        401
    );
    assert_eq!(
        status_of(
            &client,
            &base,
            "/admin/v1/profiles",
            Some("bae_admin_garbage")
        )
        .await,
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_boot_is_a_noop() {
    let h = Harness::new();
    h.bootstrap(false, false);
    let content_before = std::fs::read(&h.key_file).unwrap();
    assert_eq!(h.active_admin_rows(), 1);

    // Boot again against the same DB/file.
    let enabled = h.bootstrap(false, false);
    assert!(enabled);
    assert_eq!(h.active_admin_rows(), 1, "no new admin row on second boot");
    let content_after = std::fs::read(&h.key_file).unwrap();
    assert_eq!(
        content_before, content_after,
        "the existing key file must be left untouched"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_provisioned_hash_file_is_ingested_verbatim() {
    let h = Harness::new();
    // Produce a hash file exactly as `baectl auth create key` would: a plaintext
    // and its Argon2id PHC hash. (The cross-crate round trip lives in
    // baectl_cli.rs; here the server's own keys build the fixture.)
    let generated = keys::generate_admin_key();
    let hash = keys::hash_key(&generated.plaintext).unwrap();
    std::fs::write(
        &h.hash_file,
        json!({ "key_hash": hash, "prefix": generated.prefix, "name": "provisioned-admin" })
            .to_string(),
    )
    .unwrap();

    let enabled = h.bootstrap(false, false);
    assert!(enabled);
    assert_eq!(h.active_admin_rows(), 1);
    // The stored hash is the file's hash, byte-for-byte (not regenerated).
    assert_eq!(h.active_admin_hash(), hash);
    // The server never learns the plaintext, so it writes no key file.
    assert!(
        !h.key_file.exists(),
        "no plaintext key file in the ingest path"
    );

    // The paired plaintext authenticates — proves ingest wired the hash in.
    let (base, client) = h.serve(enabled).await;
    assert_eq!(
        status_of(
            &client,
            &base,
            "/admin/v1/profiles",
            Some(&generated.plaintext)
        )
        .await,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_admin_key_mints_fresh_material_and_ignores_hash_file() {
    let h = Harness::new();
    h.bootstrap(false, false);
    let old_token = h.read_key();

    // Drop a valid hash file that rotation must *ignore* (rotation always mints
    // fresh material, never re-ingests).
    let provisioned = keys::generate_admin_key();
    let provisioned_hash = keys::hash_key(&provisioned.plaintext).unwrap();
    std::fs::write(
        &h.hash_file,
        json!({ "key_hash": provisioned_hash, "prefix": provisioned.prefix }).to_string(),
    )
    .unwrap();

    let enabled = h.bootstrap(true, false);
    assert!(enabled);
    let new_token = h.read_key();
    assert_ne!(old_token, new_token, "rotation must mint a new plaintext");
    assert_eq!(
        h.active_admin_rows(),
        1,
        "exactly one active admin key after rotation"
    );
    // Fresh material, not the pre-provisioned hash.
    assert_ne!(
        h.active_admin_hash(),
        provisioned_hash,
        "rotation must not re-ingest the hash file"
    );

    let (base, client) = h.serve(enabled).await;
    // Old token no longer works; new token does.
    assert_eq!(
        status_of(&client, &base, "/admin/v1/profiles", Some(&old_token)).await,
        401
    );
    assert_eq!(
        status_of(&client, &base, "/admin/v1/profiles", Some(&new_token)).await,
        200
    );
    // The ignored hash file's plaintext must NOT authenticate.
    assert_eq!(
        status_of(
            &client,
            &base,
            "/admin/v1/profiles",
            Some(&provisioned.plaintext)
        )
        .await,
        401,
        "the hash file was ignored, so its plaintext is not a valid credential"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangerously_disable_admin_auth_leaves_routes_open() {
    let h = Harness::new();
    let enabled = h.bootstrap(false, true);
    assert!(!enabled, "auth disabled → enforcement off");
    assert_eq!(h.active_admin_rows(), 0, "no admin key row is created");
    assert!(!h.key_file.exists(), "no key file is written when disabled");

    // Every admin route succeeds with NO Authorization header at all.
    let (base, client) = h.serve(enabled).await;
    for path in [
        "/admin/v1/profiles",
        "/admin/v1/keys",
        "/admin/v1/mcp-servers",
    ] {
        assert_eq!(
            status_of(&client, &base, path, None).await,
            200,
            "{path} must be open when auth is disabled"
        );
    }
}

#[test]
fn stale_key_file_is_overwritten_and_clamped_to_0600() {
    // Edge case: a plaintext key file exists (e.g. restored from a backup) but
    // no admin row does. The server never re-hashes a file it finds — it follows
    // the ordinary bootstrap and silently overwrites the stale file with the
    // freshly generated key, clamping permissions back to 0600 even though the
    // stale file was looser.
    let h = Harness::new();
    std::fs::write(&h.key_file, "bae_admin_stale_stale_stale\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&h.key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    let enabled = h.bootstrap(false, false);
    assert!(enabled);
    let token = h.read_key();
    assert_ne!(
        token, "bae_admin_stale_stale_stale",
        "the stale file must be overwritten with fresh material, never trusted"
    );
    assert_eq!(h.active_admin_rows(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&h.key_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "overwrite must clamp permissions to 0600");
    }
}

#[test]
fn malformed_hash_file_is_a_usage_error_exit_2() {
    // A malformed pre-provisioned hash file is an operator authoring/transfer
    // mistake: a startup usage error (exit 2) that leaves the DB and filesystem
    // untouched — never silently ignored or fallen back from.
    let h = Harness::new();
    for bad in [
        "not json at all",
        r#"{"prefix": "bae_admin_1a2b"}"#, // no key_hash
        r#"{"key_hash": "$argon2id$v=19$m=65536,t=3,p=1$c29tZXNhbHQ$c29tZWhhc2g"}"#, // no prefix
        r#"{"key_hash": "", "prefix": "bae_admin_1a2b"}"#, // empty key_hash
        r#"{"key_hash": "not-a-phc-string", "prefix": "bae_admin_1a2b"}"#,
        r#"{"key_hash": "$2b$12$notargon2id", "prefix": "bae_admin_1a2b"}"#, // wrong algorithm
    ] {
        std::fs::write(&h.hash_file, bad).unwrap();
        let err = admin_auth::bootstrap(&h.store, &h.config(false, false))
            .expect_err(&format!("must reject: {bad}"));
        assert!(
            matches!(err, admin_auth::AdminAuthError::MalformedHashFile { .. }),
            "wrong error for {bad}: {err}"
        );
        assert_eq!(err.exit_code(), 2, "usage error for {bad}");
        assert_eq!(h.active_admin_rows(), 0, "no admin row inserted for {bad}");
        assert!(!h.key_file.exists(), "no key file written for {bad}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_active_admin_keys_all_authenticate() {
    // A pre-provisioned replica and a manually recovered key can briefly
    // coexist: the middleware checks the bearer token against EVERY active
    // admin row, so any of them is a valid credential (allowed, not an error).
    let h = Harness::new();
    let enabled = h.bootstrap(false, false);
    let first = h.read_key();

    let second = keys::generate_admin_key();
    let second_hash = keys::hash_key(&second.plaintext).unwrap();
    h.store
        .with_conn(|c| {
            keys::insert_admin_key_from_hash(c, "recovered", &second.prefix, &second_hash)
        })
        .unwrap();
    assert_eq!(h.active_admin_rows(), 2);

    let (base, client) = h.serve(enabled).await;
    for token in [&first, &second.plaintext] {
        assert_eq!(
            status_of(&client, &base, "/admin/v1/profiles", Some(token)).await,
            200,
            "every active admin key must authenticate"
        );
    }
}

// ---------------------------------------------------------------------------
// Enforcement across every admin route + role scoping
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enforcement_rejects_missing_garbage_and_client_keys_on_every_route() {
    let h = Harness::new();
    let enabled = h.bootstrap(false, false);
    let admin_token = h.read_key();

    // A real client-role key (must never be accepted on the admin port).
    let profile_id = h.seed_profile("enforcement");
    let client_key = h.seed_client_key(&profile_id);

    let (base, client) = h.serve(enabled).await;
    for path in [
        "/admin/v1/profiles",
        "/admin/v1/keys",
        "/admin/v1/mcp-servers",
    ] {
        assert_eq!(
            status_of(&client, &base, path, None).await,
            401,
            "{path}: no token"
        );
        assert_eq!(
            status_of(&client, &base, path, Some("not-a-real-key")).await,
            401,
            "{path}: garbage token"
        );
        assert_eq!(
            status_of(&client, &base, path, Some(&client_key)).await,
            401,
            "{path}: a client-role key must be rejected on the admin port"
        );
        assert_eq!(
            status_of(&client, &base, path, Some(&admin_token)).await,
            200,
            "{path}: the correct admin token succeeds"
        );
    }
}

// ---------------------------------------------------------------------------
// Usage-error flag combination (driven through the real binary)
// ---------------------------------------------------------------------------

/// Stage a private copy of the `baesrv` binary under `dir` and return its path.
///
/// Two independent hazards make the shared "uplift" binary
/// (`target/debug/baesrv`) unreliable to read directly:
///
/// 1. **Wrong path.** `env!("CARGO_BIN_EXE_baesrv")` bakes the binary's
///    *compile-time* absolute path into the test. When the compiled test
///    executable is reused across a different mount than it was built under
///    (e.g. built inside the dev container at `/workspace/...`, then run on the
///    host at `/Users/…/worktrees/…`, with Cargo's fingerprint not
///    invalidating on the env change), that baked path names a location that
///    does not exist here — a *permanent* ENOENT, not a transient one. We
///    therefore prefer the path derived from the running test executable
///    (`current_exe()` → `…/target/debug/deps/<test>` → `../baesrv`), which is
///    always correct for the environment we are actually running in, and fall
///    back to the baked path only if that sibling is absent.
/// 2. **Transient relink.** Cargo refreshes the uplift path with a
///    remove-then-write whenever an invocation's build configuration differs
///    (e.g. `make build`/`make lint` running concurrently with `make test`) and
///    holds no build lock while tests run. During that window the source is
///    missing (ENOENT) or briefly busy (ETXTBSY) for the *entire* duration of
///    the link. We retry the copy across a deadline that comfortably outlasts
///    such a build.
///
/// Copying the binary into the test's own temp dir makes the spawns hermetic
/// without changing anything the test asserts.
fn stage_baesrv_copy(dir: &std::path::Path) -> PathBuf {
    // Candidate sources, most-reliable first: the sibling of the running test
    // executable (correct for *this* environment), then Cargo's baked path.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        // exe = …/target/debug/deps/<test-hash>; the uplifted bin is two levels up.
        if let Some(bin) = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|d| d.join("baesrv"))
        {
            candidates.push(bin);
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_BIN_EXE_baesrv")));

    let dst = dir.join("baesrv");
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut last_err = None;
    loop {
        // A candidate that never exists (hazard 1) is a permanent miss; skip
        // straight to the next candidate rather than burning the deadline on it.
        for src in &candidates {
            match std::fs::copy(src, &dst) {
                Ok(_) => return dst,
                Err(e) => last_err = Some((src.clone(), e)),
            }
        }
        if std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        } else {
            let (src, e) = last_err.expect("at least one copy attempt");
            panic!(
                "baesrv never became copyable (tried {}); last source {} within 90s: {e}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                src.display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotate_plus_disable_exits_2_without_touching_db_or_fs() {
    // Both the flag+flag form (rejected at parse time, before config load) and
    // the env-disable + rotate-flag form (rejected in run_serve, before the
    // store opens) must exit 2 and leave the DB file uncreated.
    let dir = std::env::temp_dir().join(format!("baesrv-flagcombo-{}", generate_id("")));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = stage_baesrv_copy(&dir);

    // Case 1: both flags.
    let db1 = dir.join("case1.db");
    let db1c = db1.clone();
    let bin1 = bin.clone();
    let out1 = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&bin1)
            .args([
                "serve",
                "--rotate-admin-key",
                "--dangerously-disable-admin-auth",
            ])
            .env("BAE_DB_PATH", &db1c)
            .env("BAE_LOG", "error")
            .env_remove("BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(out1.status.code(), Some(2), "flag+flag must exit 2");
    assert!(
        !db1.exists(),
        "no DB created before the usage-error exit (flag+flag)"
    );

    // Case 2: env-disable + rotate flag.
    let db2 = dir.join("case2.db");
    let db2c = db2.clone();
    let out2 = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&bin)
            .args(["serve", "--rotate-admin-key"])
            .env("BAE_DB_PATH", &db2c)
            .env("BAE_LOG", "error")
            .env("BAE_DANGEROUSLY_DISABLE_ADMIN_AUTH", "1")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        out2.status.code(),
        Some(2),
        "env-disable + rotate flag must exit 2"
    );
    assert!(
        !db2.exists(),
        "no DB created before the usage-error exit (env+flag)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
