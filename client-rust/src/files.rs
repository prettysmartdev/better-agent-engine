//! Builtin **file tools** — give an agent scoped filesystem access (`read_file`,
//! `write_file`, `explore_files`) behind security constraints the harness
//! developer chooses.
//!
//! Unlike the [sandbox tools](crate::sandbox), file tools need **no**
//! [`Session`](crate::Session): they touch the local filesystem directly, so
//! they are constructed once from a [`FileToolConfig`] and registered through the
//! ordinary pre-connect builder ([`Harness::with_tool`](crate::Harness::with_tool)).
//! They are opt-in — never auto-registered.
//!
//! # Security model
//!
//! All three tools funnel every requested path through **one** shared
//! [`validate_path`] so their notions of "permitted" can never drift. The
//! validation order is deliberate and security-critical:
//!
//! 1. **Canonicalize first** — resolve `..` and symlinks *before* any allow/deny
//!    check. Checking an allowlist against the raw, uncanonicalized string is the
//!    classic path-traversal bug (`../../etc/passwd`, or a symlink escaping the
//!    allowed root); doing it after canonicalization closes that hole.
//! 2. The canonical path must sit under one of the canonicalized
//!    [`FileToolConfig::allowed_dirs`] (an **empty** list permits *nothing*).
//! 3. [`FileToolConfig::denied_extensions`] rejects (wins even over an allow).
//! 4. [`FileToolConfig::allowed_extensions`], if set, must match.
//! 5. [`FileToolConfig::deny_regex`] rejects the filename.
//! 6. [`FileToolConfig::allow_regex`], if set, must match the filename.
//!
//! [`write_file_tool`] additionally refuses to write under a **missing parent
//! directory** unless [`FileToolConfig::create_parents`] is set — no implicit
//! `mkdir -p` staging files somewhere unexpected inside the allowed tree.
//!
//! # Why validation failures are tool *results*, not errors
//!
//! A rejected path returns an **in-band, error-shaped tool result**
//! (`{"error": "path not permitted: …"}`), never an `Err` that aborts the loop.
//! A security-boundary rejection is an *expected, foreseeable* model behaviour
//! (the LLM guessing at a path outside its sandbox), not a program bug — so the
//! model should see it as tool output it can read and retry from, exactly as
//! `run_turn` turns MCP/sandbox failures into error-shaped `tool.result`s rather
//! than aborting the turn. This is the one place a builtin tool deliberately
//! catches-and-wraps rather than propagates; a harness developer copying the
//! pattern should make that choice on purpose, not by accident.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{json, Value};

use crate::tool::Tool;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The security constraints a harness developer chooses for a set of file tools.
///
/// Build one with [`FileToolConfig::new`] and the builder setters, or set the
/// public fields directly. The same config can be reused for all three
/// constructors (it is [`Clone`]).
#[derive(Clone, Debug)]
pub struct FileToolConfig {
    /// Only paths that canonicalize to somewhere under one of these directories
    /// are permitted. **Required** — an empty list permits *nothing* (an
    /// explicit, developer-visible "nothing is allowed" rather than an implicit
    /// filesystem-wide default).
    pub allowed_dirs: Vec<PathBuf>,
    /// If set, only these extensions (without the leading dot, case-insensitive)
    /// are permitted.
    pub allowed_extensions: Option<Vec<String>>,
    /// These extensions are always rejected, even if `allowed_extensions` would
    /// otherwise permit them (e.g. block `.env` while allowing everything else).
    pub denied_extensions: Vec<String>,
    /// If set, the path's filename must match this regex.
    pub allow_regex: Option<Regex>,
    /// If set, a filename matching this regex is always rejected.
    pub deny_regex: Option<Regex>,
    /// Whether [`write_file_tool`] may create missing parent directories.
    /// Defaults to `false`: a write under a non-existent parent is rejected.
    pub create_parents: bool,
}

impl FileToolConfig {
    /// A config scoped to `allowed_dirs` with every other constraint at its
    /// permissive default (no extension/regex filters, `create_parents` off).
    pub fn new(allowed_dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            allowed_dirs: allowed_dirs.into_iter().map(Into::into).collect(),
            allowed_extensions: None,
            denied_extensions: Vec::new(),
            allow_regex: None,
            deny_regex: None,
            create_parents: false,
        }
    }

    /// Restrict permitted extensions to `exts` (without leading dots).
    pub fn allowed_extensions(mut self, exts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_extensions = Some(exts.into_iter().map(Into::into).collect());
        self
    }

    /// Always reject these extensions (without leading dots).
    pub fn denied_extensions(mut self, exts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.denied_extensions = exts.into_iter().map(Into::into).collect();
        self
    }

    /// Require the filename to match `re`.
    pub fn allow_regex(mut self, re: Regex) -> Self {
        self.allow_regex = Some(re);
        self
    }

    /// Always reject a filename matching `re`.
    pub fn deny_regex(mut self, re: Regex) -> Self {
        self.deny_regex = Some(re);
        self
    }

    /// Allow [`write_file_tool`] to create missing parent directories.
    pub fn create_parents(mut self, yes: bool) -> Self {
        self.create_parents = yes;
        self
    }
}

// ---------------------------------------------------------------------------
// Shared path validation (the single source of truth for all three tools)
// ---------------------------------------------------------------------------

/// A permitted path: its fully canonical form plus the canonicalized
/// `allowed_dirs` root it matched (used to make [`explore_files_tool`] entries
/// relative).
struct Resolved {
    canonical: PathBuf,
    root: PathBuf,
}

/// Canonicalize `path`, resolving `..` and symlinks. Unlike [`fs::canonicalize`]
/// this also works for a **not-yet-existing** leaf (needed by `write_file`): it
/// canonicalizes the longest existing prefix — so every symlink is still
/// resolved — and appends the not-yet-created tail components literally. A tail
/// component that is `.`/`..` (no `file_name`) is rejected as unresolvable.
fn resolve_canonical(path: &Path) -> std::io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(p) => Ok(p),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let file = path.file_name().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path has no final component",
                )
            })?;
            let parent = path.parent().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no existing parent directory")
            })?;
            let cparent = resolve_canonical(parent)?;
            Ok(cparent.join(file))
        }
        Err(e) => Err(e),
    }
}

/// Extension of `path`'s file name, lowercased, if any. Unlike
/// [`Path::extension`] this treats a **dotfile**'s suffix as its extension
/// (`.env` → `env`), so a `denied_extensions = ["env"]` constraint blocks `.env`
/// exactly as the file-tools guide's worked example promises. Returns `None` for
/// a name with no `.` or a trailing `.` (an empty extension).
fn extension_of(path: &Path) -> Option<String> {
    let name = path.file_name().and_then(|s| s.to_str())?;
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => Some(name[i + 1..].to_ascii_lowercase()),
        _ => None,
    }
}

/// Normalise a developer-supplied extension (`".env"` or `"env"`) for matching.
fn norm_ext(ext: &str) -> String {
    ext.trim_start_matches('.').to_ascii_lowercase()
}

/// Validate `requested` against `config`, returning its resolved canonical form.
///
/// The single choke point every file tool shares (see the [module docs](self)
/// for the ordered rules). A `Err(reason)` here is turned by each tool into an
/// in-band error-shaped result — it is never propagated as a loop-aborting error.
fn validate_path(config: &FileToolConfig, requested: &str) -> Result<Resolved, String> {
    // Make relative paths absolute against the process cwd before resolving, so
    // the parent-walk in `resolve_canonical` always terminates at the root.
    let raw = PathBuf::from(requested);
    let abs = if raw.is_absolute() {
        raw
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot resolve current directory: {e}"))?
            .join(raw)
    };

    // (1) Canonicalize BEFORE any allow/deny check, then prefix-match against the
    // canonicalized allowed_dirs. An empty allowed_dirs matches nothing.
    let canonical =
        resolve_canonical(&abs).map_err(|e| format!("cannot resolve `{requested}`: {e}"))?;
    let root = config
        .allowed_dirs
        .iter()
        .filter_map(|d| fs::canonicalize(d).ok())
        .find(|cdir| canonical.starts_with(cdir))
        .ok_or_else(|| format!("`{requested}` is not under any allowed directory"))?;

    let file_name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let ext = extension_of(&canonical);

    // (2) denied_extensions — always wins.
    if let Some(ext) = &ext {
        if config.denied_extensions.iter().any(|d| norm_ext(d) == *ext) {
            return Err(format!("extension `.{ext}` is denied"));
        }
    }

    // (3) allowed_extensions must match if set (a path with no extension never
    // matches a non-empty allow set).
    if let Some(allowed) = &config.allowed_extensions {
        let ok = ext
            .as_ref()
            .is_some_and(|ext| allowed.iter().any(|a| norm_ext(a) == *ext));
        if !ok {
            return Err("extension is not in the allowed set".to_string());
        }
    }

    // (4) deny_regex rejects the filename.
    if let Some(re) = &config.deny_regex {
        if re.is_match(file_name) {
            return Err("filename matches the deny pattern".to_string());
        }
    }

    // (5) allow_regex must match the filename if set.
    if let Some(re) = &config.allow_regex {
        if !re.is_match(file_name) {
            return Err("filename does not match the allow pattern".to_string());
        }
    }

    Ok(Resolved { canonical, root })
}

// ---------------------------------------------------------------------------
// Result shaping (every tool result is a JSON *string* content block)
// ---------------------------------------------------------------------------

/// An in-band, error-shaped result: the JSON string `{"error": <reason>}`. A
/// path rejection is prefixed `path not permitted:` so the model can tell a
/// security rejection from an I/O failure.
fn error_result(reason: impl std::fmt::Display) -> Value {
    Value::String(json!({ "error": reason.to_string() }).to_string())
}

/// A success result: `value` serialized to a JSON string (a plain-string content
/// block, wire-consistent with the TypeScript/Python SDKs).
fn ok_result(value: &Value) -> Value {
    Value::String(serde_json::to_string(value).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Tool constructors
// ---------------------------------------------------------------------------

fn path_input_schema(extra: Value) -> Value {
    let mut props = json!({
        "path": { "type": "string", "description": "Filesystem path, validated against the tool's security constraints." }
    });
    if let (Some(base), Value::Object(more)) = (props.as_object_mut(), extra) {
        base.extend(more);
    }
    props
}

/// A tool that reads a UTF-8 text file at `path`, if `config` permits it. On a
/// permitted, readable path the result is the JSON string
/// `{"path", "content"}`; a rejected path or read error is an error-shaped
/// result (see the [module docs](self)).
pub fn read_file_tool(config: FileToolConfig) -> Tool {
    let input_schema = json!({
        "type": "object",
        "properties": path_input_schema(json!({})),
        "required": ["path"],
        "additionalProperties": false
    });
    Tool::new(
        "read_file",
        "Read a UTF-8 text file. The path must satisfy the tool's configured \
         directory/extension/filename constraints; a disallowed path returns an error result.",
        input_schema,
        move |input| {
            let config = config.clone();
            async move {
                let path = match input.get("path").and_then(Value::as_str) {
                    Some(p) => p.to_string(),
                    None => return Ok(error_result("read_file requires a string `path`")),
                };
                let resolved = match validate_path(&config, &path) {
                    Ok(r) => r,
                    Err(reason) => {
                        return Ok(error_result(format!("path not permitted: {reason}")))
                    }
                };
                match fs::read_to_string(&resolved.canonical) {
                    Ok(content) => Ok(ok_result(&json!({ "path": path, "content": content }))),
                    Err(e) => Ok(error_result(format!("read failed: {e}"))),
                }
            }
        },
    )
}

/// A tool that writes `content` to a file at `path`, if `config` permits it.
/// Refuses a missing parent directory unless
/// [`FileToolConfig::create_parents`] is set. On success the result is the JSON
/// string `{"path", "bytes_written"}`.
pub fn write_file_tool(config: FileToolConfig) -> Tool {
    let input_schema = json!({
        "type": "object",
        "properties": path_input_schema(json!({
            "content": { "type": "string", "description": "The UTF-8 text to write." }
        })),
        "required": ["path", "content"],
        "additionalProperties": false
    });
    Tool::new(
        "write_file",
        "Write a UTF-8 text file. The path must satisfy the tool's configured constraints, and \
         (unless create_parents is enabled) its parent directory must already exist.",
        input_schema,
        move |input| {
            let config = config.clone();
            async move {
                let path = match input.get("path").and_then(Value::as_str) {
                    Some(p) => p.to_string(),
                    None => return Ok(error_result("write_file requires a string `path`")),
                };
                let content = input.get("content").and_then(Value::as_str).unwrap_or("");
                let resolved = match validate_path(&config, &path) {
                    Ok(r) => r,
                    Err(reason) => {
                        return Ok(error_result(format!("path not permitted: {reason}")))
                    }
                };
                if let Some(parent) = resolved.canonical.parent() {
                    if !parent.exists() {
                        if config.create_parents {
                            if let Err(e) = fs::create_dir_all(parent) {
                                return Ok(error_result(format!("could not create parents: {e}")));
                            }
                        } else {
                            return Ok(error_result(
                                "path not permitted: parent directory does not exist \
                                 (enable create_parents to allow)",
                            ));
                        }
                    }
                }
                match fs::write(&resolved.canonical, content) {
                    Ok(()) => Ok(ok_result(
                        &json!({ "path": path, "bytes_written": content.len() }),
                    )),
                    Err(e) => Ok(error_result(format!("write failed: {e}"))),
                }
            }
        },
    )
}

/// A tool that lists the files under a permitted directory, **non-recursive by
/// default** (`{"recursive": true}` descends). Every discovered entry is itself
/// filtered through [`validate_path`], so the listing can never surface a path
/// [`read_file_tool`] would reject. The result is a JSON string of an array of
/// `{"path", "is_dir", "size_bytes"}` entries, each `path` relative to the
/// matched `allowed_dirs` root.
pub fn explore_files_tool(config: FileToolConfig) -> Tool {
    let input_schema = json!({
        "type": "object",
        "properties": path_input_schema(json!({
            "recursive": {
                "type": "boolean",
                "description": "Descend into subdirectories (default false)."
            }
        })),
        "required": ["path"],
        "additionalProperties": false
    });
    Tool::new(
        "explore_files",
        "List files under a permitted directory (non-recursive unless recursive=true). Only \
         entries that satisfy the same constraints as read_file are returned.",
        input_schema,
        move |input| {
            let config = config.clone();
            async move {
                let path = match input.get("path").and_then(Value::as_str) {
                    Some(p) => p.to_string(),
                    None => return Ok(error_result("explore_files requires a string `path`")),
                };
                let recursive = input
                    .get("recursive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let resolved = match validate_path(&config, &path) {
                    Ok(r) => r,
                    Err(reason) => {
                        return Ok(error_result(format!("path not permitted: {reason}")))
                    }
                };
                if !resolved.canonical.is_dir() {
                    return Ok(error_result(format!("`{path}` is not a directory")));
                }
                let mut entries: Vec<Value> = Vec::new();
                walk(
                    &resolved.canonical,
                    &resolved.root,
                    &config,
                    recursive,
                    &mut entries,
                );
                // Sort by relative path so the listing is deterministic and
                // byte-identical across the Rust/TS/Python SDKs (each engine's
                // directory-read order differs).
                entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                Ok(ok_result(&Value::Array(entries)))
            }
        },
    )
}

/// Depth-first walk emitting one entry per discovered path that passes
/// [`validate_path`]. Descent is gated on the canonical child still living under
/// `root`, so a symlinked subdirectory pointing outside the allowed tree is
/// neither listed nor followed.
fn walk(dir: &Path, root: &Path, config: &FileToolConfig, recursive: bool, out: &mut Vec<Value>) {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in read.flatten() {
        // Canonicalize each child so a symlink escaping `root` is caught here and
        // never listed or descended into.
        let canonical = match fs::canonicalize(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !canonical.starts_with(root) {
            continue;
        }
        let is_dir = canonical.is_dir();
        // Emit only entries a subsequent read_file would also accept.
        if let (true, Some(rel)) = (
            validate_path(config, &canonical.to_string_lossy()).is_ok(),
            canonical.strip_prefix(root).ok(),
        ) {
            let size_bytes = if is_dir {
                0
            } else {
                fs::metadata(&canonical).map(|m| m.len()).unwrap_or(0)
            };
            out.push(json!({
                "path": rel.to_string_lossy(),
                "is_dir": is_dir,
                "size_bytes": size_bytes
            }));
        }
        if is_dir && recursive {
            walk(&canonical, root, config, recursive, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A throwaway temp directory, removed on drop. Avoids a `tempfile` dev-dep
    /// (the test suite is fully offline, so no crate download).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("bae_files_{tag}_{}_{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Drive a builtin file tool's handler and parse its JSON-string result into
    /// the inner `Value` the model would see.
    async fn call(tool: &Tool, input: Value) -> Value {
        let raw = tool.call(input).await.expect("file tools never Err");
        let s = raw.as_str().expect("file tool result is a JSON string");
        serde_json::from_str(s).expect("file tool result parses as JSON")
    }

    /// Did the tool reject the path with an in-band `path not permitted:` error?
    fn is_not_permitted(result: &Value) -> bool {
        result
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|e| e.starts_with("path not permitted:"))
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn allowed_dir_and_allowed_extension_is_permitted() {
        let dir = TempDir::new("ok");
        let file = dir.path.join("notes.txt");
        write(&file, "hello");

        let config = FileToolConfig::new([dir.path.clone()]).allowed_extensions(["txt"]);
        let result = call(
            &read_file_tool(config),
            json!({ "path": file.to_str().unwrap() }),
        )
        .await;

        assert_eq!(result["content"], json!("hello"));
        assert!(result.get("error").is_none());
    }

    #[tokio::test]
    async fn parent_traversal_escape_is_rejected() {
        let dir = TempDir::new("traverse");
        let allowed = dir.path.join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        // A real secret sits OUTSIDE the allowed subdir but inside the temp root.
        write(&dir.path.join("secret.txt"), "top secret");

        let config = FileToolConfig::new([allowed.clone()]);
        // `../secret.txt` resolves above the allowed root.
        let escape = allowed.join("../secret.txt");
        let result = call(
            &read_file_tool(config),
            json!({ "path": escape.to_str().unwrap() }),
        )
        .await;

        assert!(is_not_permitted(&result), "got {result}");
    }

    #[tokio::test]
    async fn symlink_escaping_allowed_dir_is_rejected() {
        let dir = TempDir::new("symlink");
        let allowed = dir.path.join("allowed");
        let outside = dir.path.join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        write(&outside.join("secret.txt"), "top secret");

        // A symlink INSIDE the allowed dir pointing at the outside secret.
        let link = allowed.join("link.txt");
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).unwrap();

        let config = FileToolConfig::new([allowed.clone()]);
        let result = call(
            &read_file_tool(config),
            json!({ "path": link.to_str().unwrap() }),
        )
        .await;

        // Canonicalize-before-check resolves the symlink to `outside/…`, which is
        // not under the allowed root → rejected (we assert on behaviour, not on
        // the string form of the path).
        assert!(is_not_permitted(&result), "got {result}");
    }

    #[tokio::test]
    async fn denied_extension_overrides_allowed_extension() {
        let dir = TempDir::new("denyext");
        write(&dir.path.join("app.env"), "SECRET=1");
        write(&dir.path.join("app.txt"), "ok");

        // `.env` is in the allow set, yet the deny set must still win.
        let config = FileToolConfig::new([dir.path.clone()])
            .allowed_extensions(["env", "txt"])
            .denied_extensions(["env"]);
        let tool = read_file_tool(config);

        let denied = call(
            &tool,
            json!({ "path": dir.path.join("app.env").to_str().unwrap() }),
        )
        .await;
        assert!(is_not_permitted(&denied), "got {denied}");

        let allowed = call(
            &tool,
            json!({ "path": dir.path.join("app.txt").to_str().unwrap() }),
        )
        .await;
        assert_eq!(allowed["content"], json!("ok"));
    }

    #[tokio::test]
    async fn deny_regex_overrides_allow_regex() {
        let dir = TempDir::new("denyre");
        write(&dir.path.join("public.txt"), "ok");
        write(&dir.path.join("secret.txt"), "no");

        let config = FileToolConfig::new([dir.path.clone()])
            .allow_regex(Regex::new(r".*\.txt$").unwrap())
            .deny_regex(Regex::new(r"secret").unwrap());
        let tool = read_file_tool(config);

        let denied = call(
            &tool,
            json!({ "path": dir.path.join("secret.txt").to_str().unwrap() }),
        )
        .await;
        assert!(is_not_permitted(&denied), "got {denied}");

        let allowed = call(
            &tool,
            json!({ "path": dir.path.join("public.txt").to_str().unwrap() }),
        )
        .await;
        assert_eq!(allowed["content"], json!("ok"));
    }

    #[tokio::test]
    async fn empty_allowed_dirs_rejects_everything() {
        let dir = TempDir::new("empty");
        let file = dir.path.join("f.txt");
        write(&file, "hello");

        let config = FileToolConfig::new(Vec::<PathBuf>::new());
        let result = call(
            &read_file_tool(config),
            json!({ "path": file.to_str().unwrap() }),
        )
        .await;

        assert!(is_not_permitted(&result), "got {result}");
    }

    #[tokio::test]
    async fn write_file_parent_directory_guard() {
        let dir = TempDir::new("write");
        let target = dir.path.join("newdir").join("out.txt");

        // create_parents defaults false → a write under a missing parent fails.
        let strict = write_file_tool(FileToolConfig::new([dir.path.clone()]));
        let rejected = call(
            &strict,
            json!({ "path": target.to_str().unwrap(), "content": "x" }),
        )
        .await;
        assert!(is_not_permitted(&rejected), "got {rejected}");
        assert!(!target.exists());

        // With create_parents enabled the same write succeeds.
        let lax = write_file_tool(FileToolConfig::new([dir.path.clone()]).create_parents(true));
        let ok = call(
            &lax,
            json!({ "path": target.to_str().unwrap(), "content": "hello" }),
        )
        .await;
        assert_eq!(ok["bytes_written"], json!(5));
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
    }

    #[tokio::test]
    async fn explore_only_returns_paths_read_file_would_accept() {
        // Real files scattered inside and outside the allowed tree, plus a
        // symlink inside the tree escaping it: explore must return only entries
        // that read_file's own validation (identical config) also permits.
        let dir = TempDir::new("explore");
        let allowed = dir.path.join("allowed");
        let outside = dir.path.join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        write(&allowed.join("a.txt"), "a");
        write(&allowed.join("sub").join("b.txt"), "b");
        write(&outside.join("c.txt"), "c");
        std::os::unix::fs::symlink(outside.join("c.txt"), allowed.join("escape.txt")).unwrap();

        let config = FileToolConfig::new([allowed.clone()]);
        let listing = call(
            &explore_files_tool(config.clone()),
            json!({ "path": allowed.to_str().unwrap(), "recursive": true }),
        )
        .await;
        let entries = listing.as_array().expect("explore returns an array");

        // The escaping symlink and the outside file never appear.
        let rels: Vec<&str> = entries
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert!(rels.contains(&"a.txt"));
        assert!(rels.iter().any(|p| p.ends_with("b.txt")));
        assert!(!rels.iter().any(|p| p.contains("escape")));
        assert!(!rels.iter().any(|p| p.contains("c.txt")));

        // Property: every path explore returned passes read_file's validation
        // under the identical config (a directory yields a read error, never a
        // `path not permitted:` rejection).
        let reader = read_file_tool(config);
        let root = allowed.canonicalize().unwrap();
        for rel in rels {
            let abs = root.join(rel);
            let result = call(&reader, json!({ "path": abs.to_str().unwrap() })).await;
            assert!(
                !is_not_permitted(&result),
                "explore surfaced `{rel}` that read_file rejected: {result}"
            );
        }
    }
}
