//! `launcher-core` — the primitives shared by every BAE harness launcher.
//!
//! The three launcher images (`bae-launcher-schedule`, `bae-launcher-api`,
//! `bae-launcher-webapp`) all reduce to the same underlying job: take a
//! per-agent config, resolve any `${VAR}` secrets in it against the launcher's
//! own environment, spawn a child process with the resulting env/args, and
//! forward its output — agent-name-prefixed — to the launcher's logs (and, for
//! the API launcher, to an HTTP response body). This crate owns exactly that
//! shared primitive and nothing else.
//!
//! # Deliberately scope-limited
//!
//! `launcher-core` has **no knowledge of HTTP or cron**. Everything axum/JSON
//! Schema/streamed-response lives in `launcher-api`; everything
//! `tokio-cron-scheduler`/overlap-skip lives in `launcher-schedule`. It also
//! depends on nothing outside `launchers/` — never `server/`, a client SDK, or
//! `baectl/`. Sharing *within* the launchers family is encouraged; reaching
//! outside it is not (see work item 0014 section A).
//!
//! # Surface
//!
//! - [`SpawnSpec`] — the fully-resolved, ready-to-spawn shape both launcher
//!   crates convert their own richer per-agent config into.
//! - [`resolve_env_refs`] — `${VAR}` resolution with an injectable environment
//!   lookup; an unset referenced variable is a hard error, never an
//!   empty-string substitution.
//! - [`spawn_and_stream`] — spawn a [`SpawnSpec`] and stream its length-capped,
//!   agent-name-prefixed output as [`LogLine`]s.
//! - [`validate_unique_names`] — the duplicate-`name` startup check both
//!   configs need.
//! - [`init_logging`] — the shared `tracing`/`tracing-subscriber` setup,
//!   reading `BAE_LOG`.
//! - [`LauncherError`] — the shared error with an [`exit_code`](LauncherError::exit_code)
//!   mapping usage errors to 2 and runtime errors to 1.

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use tokio::io::AsyncRead;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

/// Maximum number of characters carried from a single child-output line before
/// it is truncated (with a trailing `…`). Mirrors the captured-output capping in
/// `server/src/engine/sandbox.rs` (there `STDERR_DETAIL_MAX`): one chatty agent
/// invocation must not be able to flood `docker logs` — or, for the API
/// launcher, the streamed response body — with an unbounded single line. The cap
/// is applied to the child's own line content; the `[name] ` attribution prefix
/// is added afterwards and is bounded by the (short) agent name.
pub const MAX_LINE_LEN: usize = 8 * 1024;

/// Buffer depth of the channel backing [`spawn_and_stream`]'s output stream. A
/// slow consumer applies natural backpressure to the child's drain tasks rather
/// than letting output accumulate unboundedly in memory.
const OUTPUT_CHANNEL_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure originating in shared launcher logic.
///
/// The [`exit_code`](LauncherError::exit_code) mapping follows the project-wide
/// CLI convention (`aspec/uxui/cli.md`, and `ConfigError::exit_code` /
/// `ConfigFileError::exit_code` in `server/`): **usage** errors — a config the
/// operator authored wrong — map to `2`; **runtime** errors — an environment
/// failure at spawn time — map to `1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherError {
    /// A `${VAR}` reference could not be resolved because the variable is unset
    /// in the launcher's environment. A **runtime** failure (exit code 1) for
    /// that one invocation — the secret is *never* silently substituted with an
    /// empty string, exactly the failure mode `server/src/config_file.rs`'s
    /// secret-handling discipline is written to avoid.
    MissingEnv { var: String },
    /// A `${` token was opened but never closed with a matching `}`. A
    /// config-authoring (**usage**) error, exit code 2.
    UnterminatedEnvRef { value: String },
    /// Two agents share a `name`. A config-authoring (**usage**) error, exit
    /// code 2. `name` uniqueness is what makes multi-agent images safe: it is
    /// simultaneously the log-prefix key, the URL path segment, and the
    /// webapp's card key.
    DuplicateName { name: String },
    /// An agent has a blank (empty or whitespace-only) `name`. A config-authoring
    /// (**usage**) error, exit code 2 — a blank name is meaningless as a
    /// log-prefix, URL segment, or card key.
    EmptyName,
}

impl LauncherError {
    /// Process exit code for this error, per `aspec/uxui/cli.md`'s convention
    /// (0 success, 1 runtime error, 2 usage error) — matching
    /// `ConfigError::exit_code` in `server/`.
    pub fn exit_code(&self) -> i32 {
        match self {
            // Environment missing a referenced variable at spawn time: runtime.
            LauncherError::MissingEnv { .. } => 1,
            // Everything else is an operator-authored config mistake: usage.
            LauncherError::UnterminatedEnvRef { .. }
            | LauncherError::DuplicateName { .. }
            | LauncherError::EmptyName => 2,
        }
    }
}

impl std::fmt::Display for LauncherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LauncherError::MissingEnv { var } => write!(
                f,
                "environment variable {var:?} referenced by ${{{var}}} is not set"
            ),
            LauncherError::UnterminatedEnvRef { value } => write!(
                f,
                "unterminated ${{...}} reference in value {value:?} (missing closing '}}')"
            ),
            LauncherError::DuplicateName { name } => {
                write!(f, "duplicate agent name {name:?}")
            }
            LauncherError::EmptyName => write!(f, "an agent has an empty name"),
        }
    }
}

impl std::error::Error for LauncherError {}

// ---------------------------------------------------------------------------
// SpawnSpec
// ---------------------------------------------------------------------------

/// A fully-resolved, ready-to-spawn child process.
///
/// This is the shared handoff shape: each launcher crate keeps its own richer
/// per-agent config struct (`ScheduleAgentConfig` with a `schedule`,
/// `ApiAgentConfig` with `request_schema`/`env_template`/`arg_template`) and
/// converts it — after cron/HTTP templating and [`resolve_env_refs`] — into a
/// `SpawnSpec` before handing off to [`spawn_and_stream`]. Only this
/// post-templating shape is shared; the divergent config lives upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The agent's unique name. Used as the `[name]` attribution prefix on every
    /// forwarded output line (see [`spawn_and_stream`]).
    pub name: String,
    /// The executable to run. Resolved against `PATH` when not absolute, the
    /// same resolution `tokio::process::Command::new` gives for free.
    pub command: String,
    /// Command-line arguments, passed through verbatim.
    pub args: Vec<String>,
    /// Extra environment variables layered onto the launcher's own environment
    /// for the child. Expected to already be `${VAR}`-resolved via
    /// [`resolve_env_refs`]; the child otherwise inherits the launcher's env.
    pub env: HashMap<String, String>,
    /// Working directory for the child, if any. `None` inherits the launcher's.
    pub working_dir: Option<String>,
}

impl SpawnSpec {
    /// Construct a spec with no extra env and no working directory — the common
    /// minimal case (a `command` plus `args`).
    pub fn new(name: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        SpawnSpec {
            name: name.into(),
            command: command.into(),
            args,
            env: HashMap::new(),
            working_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ${VAR} resolution
// ---------------------------------------------------------------------------

/// Resolve `${VAR}` references in every value of an env map against `environ`.
///
/// The keys are copied through unchanged; each value has its `${VAR}` tokens
/// replaced with the corresponding variable's value looked up via `environ`.
/// `environ` is injected (rather than reading `std::env` directly) so callers —
/// and tests — control the environment, mirroring `max/server/src/config.ts`'s
/// `loadConfig(env, ...)` pattern and `Config::resolve` in `server/`.
///
/// An **unset** referenced variable is a hard [`LauncherError::MissingEnv`],
/// never an empty-string substitution — the secret-handling discipline
/// `server/src/config_file.rs` documents. This is called immediately before
/// spawning the child so a resolved secret is never persisted or logged.
///
/// - `${NAME}` → the variable's value, or [`LauncherError::MissingEnv`].
/// - A literal `$` **not** followed by `{` is passed through unchanged.
/// - An opening `${` with no closing `}` is [`LauncherError::UnterminatedEnvRef`].
pub fn resolve_env_refs(
    env: &HashMap<String, String>,
    environ: &dyn Fn(&str) -> Option<String>,
) -> Result<HashMap<String, String>, LauncherError> {
    let mut resolved = HashMap::with_capacity(env.len());
    for (key, value) in env {
        resolved.insert(key.clone(), resolve_one(value, environ)?);
    }
    Ok(resolved)
}

/// Resolve every `${VAR}` token in a single string. Kept private: the public
/// surface is the map-shaped [`resolve_env_refs`] (work item 0014 section A,
/// point 2); a caller needing to resolve one string can pass a one-entry map.
fn resolve_one(
    input: &str,
    environ: &dyn Fn(&str) -> Option<String>,
) -> Result<String, LauncherError> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let end = match input[start..].find('}') {
                Some(rel) => start + rel,
                None => {
                    return Err(LauncherError::UnterminatedEnvRef {
                        value: input.to_owned(),
                    })
                }
            };
            let name = &input[start..end];
            let val = environ(name).ok_or_else(|| LauncherError::MissingEnv {
                var: name.to_owned(),
            })?;
            out.push_str(&val);
            i = end + 1;
        } else {
            // Push this UTF-8 character whole (handles multi-byte correctly).
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Unique-name validation
// ---------------------------------------------------------------------------

/// Ensure every agent `name` in an iterator is present and unique.
///
/// A duplicate `name` is [`LauncherError::DuplicateName`] and a blank one is
/// [`LauncherError::EmptyName`] — both usage errors (exit code 2). Both launcher
/// configs run this at startup: because multi-agent configs are the norm, a
/// copy-pasted `[[agents]]` block with an unchanged `name` is the most likely
/// way this error actually gets hit, and a collision is unsafe (it would make
/// the log prefix, URL path, and card key ambiguous).
pub fn validate_unique_names<'a>(
    names: impl Iterator<Item = &'a str>,
) -> Result<(), LauncherError> {
    let mut seen: HashSet<&'a str> = HashSet::new();
    for name in names {
        if name.trim().is_empty() {
            return Err(LauncherError::EmptyName);
        }
        if !seen.insert(name) {
            return Err(LauncherError::DuplicateName {
                name: name.to_owned(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Spawn + stream
// ---------------------------------------------------------------------------

/// Which of a child's standard streams a captured [`LogLine`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// The child's standard output.
    Stdout,
    /// The child's standard error.
    Stderr,
}

/// One item yielded by [`spawn_and_stream`].
///
/// [`Output`](LogLine::Output) lines carry the child's text **already**
/// agent-name-prefixed (`[name] …`) and length-capped to [`MAX_LINE_LEN`], so a
/// consumer forwards them verbatim: the scheduler to its own stdout, the API
/// server to its own stdout *and* the HTTP response body. Exactly one terminal
/// item — [`Exited`](LogLine::Exited) or [`SpawnFailed`](LogLine::SpawnFailed) —
/// ends every stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLine {
    /// A captured line, already `[name]`-prefixed and length-capped.
    Output {
        /// Which stream the line came from.
        stream: OutputStream,
        /// The rendered `[name] <content>` line, ready to forward verbatim.
        line: String,
    },
    /// The child exited (terminal). `code` is the process exit code, or `None`
    /// when the child was terminated by a signal (no numeric code).
    Exited {
        /// The child's exit code, or `None` if it was signalled.
        code: Option<i32>,
    },
    /// The child could not be spawned at all (terminal) — e.g. `command` not
    /// found or not executable. Never crashes the launcher: a bad agent
    /// invocation is reported in-stream, not by taking the process down.
    SpawnFailed {
        /// A human-readable description of the spawn failure.
        message: String,
    },
}

/// The stream returned by [`spawn_and_stream`].
///
/// It owns the background driver that owns the child process. Dropping an
/// in-flight stream therefore aborts that driver, drops its `Child`, and uses
/// the command's `kill_on_drop(true)` behavior to terminate a hung child. This
/// lets launchers force-kill their outstanding invocation tasks at shutdown
/// without knowing process IDs or duplicating process management.
struct SpawnStream {
    receiver: ReceiverStream<LogLine>,
    driver: JoinHandle<()>,
}

impl Stream for SpawnStream {
    type Item = LogLine;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // ReceiverStream and JoinHandle are Unpin, so this projection is safe.
        let this = self.get_mut();
        Pin::new(&mut this.receiver).poll_next(cx)
    }
}

impl Drop for SpawnStream {
    fn drop(&mut self) {
        if !self.driver.is_finished() {
            self.driver.abort();
        }
    }
}

/// Spawn `spec` and stream its output as [`LogLine`]s.
///
/// The child is launched with `kill_on_drop(true)` — dropping the returned
/// stream reliably reaps a still-running child, so a wedged agent can never
/// outlive its consumer. stdout and stderr are drained **concurrently** (each on
/// its own task) so a child that fills one pipe while the reader waits on the
/// other can never deadlock; both streams' lines are interleaved into the
/// returned stream in arrival order, each prefixed with `spec.name` and capped
/// to [`MAX_LINE_LEN`]. The child's stdin is closed (`Stdio::null`).
///
/// The stream ends with exactly one terminal [`LogLine`]:
/// [`LogLine::Exited`] once the child exits and both pipes reach EOF, or
/// [`LogLine::SpawnFailed`] if the child could not be started. A spawn failure
/// is reported through the stream rather than as an `Err`, so one bad agent
/// invocation never takes the launcher down — the whole point of the launchers'
/// "a hung or crashed child never crashes the launcher" guarantee.
pub fn spawn_and_stream(spec: &SpawnSpec) -> impl Stream<Item = LogLine> {
    let (tx, rx) = mpsc::channel::<LogLine>(OUTPUT_CHANNEL_CAPACITY);

    // Own everything the driver task needs so it is `'static`.
    let name = spec.name.clone();
    let command = spec.command.clone();
    let args = spec.args.clone();
    let env = spec.env.clone();
    let working_dir = spec.working_dir.clone();

    let driver = tokio::spawn(async move {
        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .envs(&env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &working_dir {
            cmd.current_dir(dir);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let _ = tx
                    .send(LogLine::SpawnFailed {
                        message: format!("failed to spawn {command:?}: {err}"),
                    })
                    .await;
                return;
            }
        };

        // Piped stdio is present because we requested it above.
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Drain both pipes concurrently: awaiting one while the child fills the
        // other would deadlock (same reasoning as sandbox.rs's stdin handling).
        let out_task = tokio::spawn(drain(
            stdout,
            OutputStream::Stdout,
            name.clone(),
            tx.clone(),
        ));
        let err_task = tokio::spawn(drain(
            stderr,
            OutputStream::Stderr,
            name.clone(),
            tx.clone(),
        ));

        let status = child.wait().await;
        // Ensure all captured output is enqueued before the terminal item.
        let _ = out_task.await;
        let _ = err_task.await;

        let code = match status {
            Ok(status) => status.code(),
            // A wait error is treated as an unknown (signalled) exit rather than
            // a panic — the launcher keeps running regardless.
            Err(_) => None,
        };
        let _ = tx.send(LogLine::Exited { code }).await;
    });

    SpawnStream {
        receiver: ReceiverStream::new(rx),
        driver,
    }
}

/// Upper bound, in bytes, accumulated for a single line before it is emitted in
/// its capped form. [`MAX_LINE_LEN`] counts characters and a UTF-8 character is
/// at most 4 bytes, so this many bytes always contain at least `MAX_LINE_LEN`
/// complete characters — enough to render the capped line without ever holding
/// more of a runaway newline-free line in memory.
const LINE_BYTES_MAX: usize = 4 * MAX_LINE_LEN + 4;

/// Read one child pipe line-by-line, prefixing each with `[name] ` and capping
/// its length, forwarding into `tx`. Stops on EOF, a read error, or once the
/// receiver is dropped (backpressure/cancellation).
///
/// The cap is enforced **while reading**, not after: once a line exceeds
/// [`LINE_BYTES_MAX`] accumulated bytes its capped form is emitted immediately
/// and the rest of that line is discarded up to the next newline. A child
/// flushing an arbitrarily large newline-free blob therefore costs a bounded
/// buffer (never the whole blob) and its consumer sees the capped line as soon
/// as the cap is reached, not only when the line finally ends.
async fn drain<R: AsyncRead + Unpin>(
    reader: R,
    stream: OutputStream,
    name: String,
    tx: mpsc::Sender<LogLine>,
) {
    use tokio::io::AsyncReadExt;

    let mut reader = reader;
    let mut chunk = [0u8; 8192];
    // Raw bytes of the line being assembled; converted with `from_utf8_lossy`
    // only at emission so non-UTF-8 output never aborts the drain.
    let mut line: Vec<u8> = Vec::new();
    // True once the current line was emitted in capped form: skip its remaining
    // bytes until the next newline.
    let mut discarding = false;

    'read: loop {
        let mut rest = match reader.read(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => &chunk[..n],
            Err(_) => break,
        };
        while !rest.is_empty() {
            match rest.iter().position(|&b| b == b'\n') {
                Some(idx) => {
                    if !discarding {
                        line.extend_from_slice(&rest[..idx]);
                        // Strip a trailing CR (CRLF line endings).
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        if emit(&tx, stream, &name, &line).await.is_err() {
                            break 'read; // consumer gone
                        }
                    }
                    line.clear();
                    discarding = false;
                    rest = &rest[idx + 1..];
                }
                None => {
                    if !discarding {
                        line.extend_from_slice(rest);
                        if line.len() > LINE_BYTES_MAX {
                            // The line is already longer than the cap: emit its
                            // capped form now and drop the rest of it.
                            if emit(&tx, stream, &name, &line).await.is_err() {
                                break 'read;
                            }
                            line.clear();
                            discarding = true;
                        }
                    }
                    break;
                }
            }
        }
    }

    // A final line without a trailing newline still gets forwarded.
    if !discarding && !line.is_empty() {
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let _ = emit(&tx, stream, &name, &line).await;
    }
}

/// Render `bytes` as a prefixed, capped [`LogLine::Output`] and send it.
async fn emit(
    tx: &mpsc::Sender<LogLine>,
    stream: OutputStream,
    name: &str,
    bytes: &[u8],
) -> Result<(), ()> {
    let content = String::from_utf8_lossy(bytes);
    let line = format!("[{name}] {}", cap_line(&content));
    tx.send(LogLine::Output { stream, line })
        .await
        .map_err(|_| ())
}

/// Truncate `content` to [`MAX_LINE_LEN`] characters, appending `…` when it was
/// longer. Counts by `char` (never splitting a multi-byte UTF-8 sequence),
/// mirroring the truncate-then-mark shape in `server/src/engine/sandbox.rs`.
fn cap_line(content: &str) -> String {
    match content.char_indices().nth(MAX_LINE_LEN) {
        None => content.to_owned(),
        Some((byte_idx, _)) => {
            let mut out = content[..byte_idx].to_owned();
            out.push('…');
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Initialise the shared `tracing` subscriber for a launcher binary.
///
/// The stderr fmt subscriber's filter is read from `BAE_LOG` (the same env var
/// and default posture as `server/src/config.rs`'s `DEFAULT_LOG`), falling back
/// to `default_filter` when `BAE_LOG` is unset or unparseable, and finally to
/// `info` if `default_filter` itself is invalid. Both launcher binaries call
/// this so they initialise logging identically. The launcher's own logs go to
/// **stderr**, leaving stdout free for forwarded child output.
///
/// Idempotent-safe: a second call is ignored rather than panicking (a global
/// subscriber can only be installed once).
pub fn init_logging(default_filter: &str) {
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("BAE_LOG")
        .ok()
        .and_then(|value| EnvFilter::try_new(value).ok())
        .or_else(|| EnvFilter::try_new(default_filter).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    #[test]
    fn resolves_multiple_references_with_injected_environment() {
        let env = HashMap::from([
            ("TOKEN".to_string(), "prefix-${A}-${B}".to_string()),
            ("LITERAL".to_string(), "cost=$5".to_string()),
        ]);
        let resolved = resolve_env_refs(&env, &|name| match name {
            "A" => Some("one".to_string()),
            "B" => Some("two".to_string()),
            _ => None,
        })
        .expect("all references resolve");

        assert_eq!(resolved["TOKEN"], "prefix-one-two");
        assert_eq!(resolved["LITERAL"], "cost=$5");
    }

    #[test]
    fn unset_reference_is_a_runtime_error_and_never_empty() {
        let env = HashMap::from([("TOKEN".to_string(), "${MISSING}".to_string())]);
        let error = resolve_env_refs(&env, &|_| None).expect_err("unset refs must fail");
        assert_eq!(
            error,
            LauncherError::MissingEnv {
                var: "MISSING".to_string()
            }
        );
        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("MISSING"));
    }

    #[test]
    fn malformed_reference_is_a_usage_error() {
        let env = HashMap::from([("TOKEN".to_string(), "before-${BROKEN".to_string())]);
        let error = resolve_env_refs(&env, &|_| Some("unused".to_string()))
            .expect_err("unterminated refs must fail");
        assert!(matches!(error, LauncherError::UnterminatedEnvRef { .. }));
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn validates_empty_and_duplicate_names() {
        assert_eq!(
            validate_unique_names(["first", "second"].into_iter()),
            Ok(())
        );

        let duplicate = validate_unique_names(["first", "first"].into_iter()).unwrap_err();
        assert_eq!(
            duplicate,
            LauncherError::DuplicateName {
                name: "first".to_string()
            }
        );
        assert_eq!(duplicate.exit_code(), 2);

        let empty = validate_unique_names(["first", "  "].into_iter()).unwrap_err();
        assert_eq!(empty, LauncherError::EmptyName);
        assert_eq!(empty.exit_code(), 2);
    }

    #[test]
    fn spawn_stream_prefixes_both_agents_and_caps_lines() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let long_line = "x".repeat(MAX_LINE_LEN + 17);
            let one = SpawnSpec::new(
                "one",
                "sh",
                vec![
                    "-c".to_string(),
                    format!(
                        "printf 'one-out\\n'; printf 'one-err\\n' >&2; printf '{long_line}\\n'"
                    ),
                ],
            );
            let two = SpawnSpec::new(
                "two",
                "sh",
                vec![
                    "-c".to_string(),
                    "printf 'two-out\\n'; printf 'two-err\\n' >&2".to_string(),
                ],
            );

            // Start both drivers before collecting either stream. This keeps
            // the two child processes concurrent while avoiding a `tokio::select!`
            // dependency in this deliberately small core crate's test build.
            let first =
                tokio::spawn(async move { spawn_and_stream(&one).collect::<Vec<_>>().await });
            let second =
                tokio::spawn(async move { spawn_and_stream(&two).collect::<Vec<_>>().await });
            let first = first.await.expect("first stream task");
            let second = second.await.expect("second stream task");
            let lines: Vec<String> = first
                .into_iter()
                .chain(second)
                .filter_map(|item| match item {
                    LogLine::Output { line, .. } => Some(line),
                    LogLine::Exited { .. } | LogLine::SpawnFailed { .. } => None,
                })
                .collect();

            assert!(lines.iter().any(|line| line == "[one] one-out"));
            assert!(lines.iter().any(|line| line == "[one] one-err"));
            assert!(lines.iter().any(|line| line == "[two] two-out"));
            assert!(lines.iter().any(|line| line == "[two] two-err"));
            let capped = lines
                .iter()
                .find(|line| line.starts_with("[one] ") && line.contains('…'))
                .expect("long line is capped");
            assert_eq!(
                capped.chars().count(),
                "[one] ".chars().count() + MAX_LINE_LEN + 1
            );
        });
    }

    #[test]
    fn newline_free_flood_is_capped_during_read_and_emitted_incrementally() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            // The child flushes far more than LINE_BYTES_MAX without a newline,
            // then stays alive well past the assertion window. The capped line
            // must arrive while the child is still running (incremental, not
            // buffered until the line/child ends), holding only a bounded
            // buffer, never the whole blob.
            let spec = SpawnSpec::new(
                "big",
                "sh",
                vec![
                    "-c".to_string(),
                    "head -c 262144 /dev/zero | tr '\\0' x; sleep 2; echo; echo tail".to_string(),
                ],
            );
            let mut stream = std::pin::pin!(spawn_and_stream(&spec));
            let first = tokio::time::timeout(std::time::Duration::from_millis(1500), stream.next())
                .await
                .expect("capped line arrives while the child is still mid-line")
                .expect("stream item");
            match &first {
                LogLine::Output { line, .. } => {
                    assert!(line.starts_with("[big] xxx"));
                    assert!(line.ends_with('…'));
                    assert_eq!(
                        line.chars().count(),
                        "[big] ".chars().count() + MAX_LINE_LEN + 1
                    );
                }
                other => panic!("expected capped output line, got {other:?}"),
            }

            // The rest of the flooded line is discarded; the next real line and
            // the terminal exit still come through.
            let rest: Vec<_> = stream.collect().await;
            assert!(rest
                .iter()
                .any(|item| matches!(item, LogLine::Output { line, .. } if line == "[big] tail")));
            assert!(matches!(
                rest.last(),
                Some(LogLine::Exited { code: Some(0) })
            ));
        });
    }

    #[test]
    fn byte_interleaved_concurrent_agents_never_cross_attribute() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            // Agent "a" flushes a partial line, pauses (so "b"'s bytes land in
            // between at the byte level), then completes its line. Both streams
            // are consumed merged, the way a launcher would see them.
            let a = SpawnSpec::new(
                "a",
                "sh",
                vec![
                    "-c".to_string(),
                    "printf 'AAA'; sleep 0.3; printf 'AA\\n'".to_string(),
                ],
            );
            let b = SpawnSpec::new(
                "b",
                "sh",
                vec![
                    "-c".to_string(),
                    "sleep 0.1; printf 'BBBBB\\n'; printf 'bb\\n' >&2".to_string(),
                ],
            );
            let merged = spawn_and_stream(&a).merge(spawn_and_stream(&b));
            let items: Vec<_> = std::pin::pin!(merged).collect().await;
            let lines: Vec<&str> = items
                .iter()
                .filter_map(|item| match item {
                    LogLine::Output { line, .. } => Some(line.as_str()),
                    _ => None,
                })
                .collect();
            // "b" completed its line strictly inside "a"'s partial write, yet
            // every line is whole and attributed to its own agent.
            assert_eq!(
                lines.iter().filter(|l| **l == "[a] AAAAA").count(),
                1,
                "a's split line must be reassembled exactly once: {lines:?}"
            );
            assert!(lines.contains(&"[b] BBBBB"));
            assert!(lines.contains(&"[b] bb"));
            assert_eq!(lines.len(), 3, "no cross-attributed fragments: {lines:?}");
            // b's complete line was consumed before a's line completed.
            let b_pos = lines.iter().position(|l| *l == "[b] BBBBB").unwrap();
            let a_pos = lines.iter().position(|l| *l == "[a] AAAAA").unwrap();
            assert!(
                b_pos < a_pos,
                "b's bytes should interleave inside a's line: {lines:?}"
            );
        });
    }

    #[test]
    fn failed_spawn_is_terminal_and_does_not_panic() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let spec = SpawnSpec::new("bad-agent", "/definitely/not/a/command", Vec::new());
            let items: Vec<_> = spawn_and_stream(&spec).collect().await;
            assert_eq!(items.len(), 1);
            assert!(matches!(items[0], LogLine::SpawnFailed { .. }));
        });
    }
}
