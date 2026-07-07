//! reference-assistant — the canonical BAE example agent (Rust).
//!
//! Mirrors the TypeScript and Python examples exactly: it registers one simple
//! client-side tool (`get_current_time`), opens a session, sends a message, and
//! drives the harness loop to a final answer — exercising every hook point on
//! the way. See `aspec/genai/agents.md` (Agent 1) and `api-contract.md` §9.
//!
//! Documentation first, product second: this is the readable end-to-end tour of
//! the harness surface.
//!
//! ## Running
//!
//! ```sh
//! export BAE_CLIENT_KEY=bae_…          # a client key from `POST /admin/v1/keys`
//! export ANTHROPIC_API_KEY=sk-…        # the provider key the profile references
//! export BAE_SERVER_URL=http://localhost:8080   # optional, this is the default
//! cargo run --example reference-assistant -- "What time is it?"
//! ```
//!
//! The provider key must be present in this process's environment: BAE resolves
//! the profile's `${ANTHROPIC_API_KEY}` server-side, but the example checks it
//! up front so a missing key fails fast with a clear message instead of a
//! provider-unavailable turn buried in the session events.

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bae_rs::{Config, Harness, Hooks, McpRequestPayload, McpResponsePayload, Tool};
use serde_json::json;

/// Env var naming the provider key the configured profile references. The
/// reference profile uses `${ANTHROPIC_API_KEY}`; override if your profile
/// points at a different variable.
const PROVIDER_KEY_ENV_DEFAULT: &str = "ANTHROPIC_API_KEY";

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("\nreference-assistant failed: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    // --- 1. Configuration from the environment -----------------------------
    let server_url =
        std::env::var("BAE_SERVER_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let client_key = require_env("BAE_CLIENT_KEY")?;

    // The provider credential is a *server-side* concern, but we fail fast here
    // with a clear message rather than letting the turn come back as a generic
    // provider-unavailable result with the provider.response failure in its events.
    let provider_key_env = std::env::var("BAE_PROVIDER_KEY_ENV")
        .unwrap_or_else(|_| PROVIDER_KEY_ENV_DEFAULT.to_string());
    require_env(&provider_key_env).map_err(|_| {
        format!(
            "provider key env var `{provider_key_env}` is not set — the profile references it \
             and the server needs it to reach the LLM provider. Export it and retry \
             (or set BAE_PROVIDER_KEY_ENV if your profile uses a different variable)."
        )
    })?;

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "What time is it right now?".to_string());

    let config = Config::new(server_url, client_key).with_client_version(bae_rs::VERSION);

    // --- 2. A simple client-side tool --------------------------------------
    let get_current_time = Tool::new(
        "get_current_time",
        "Return the current UTC time as an ISO-8601 string.",
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        |_input| Ok(json!(now_iso8601())),
    );

    // --- 3. Hooks: exercise every customization point at least once --------
    // A shared counter proves each point actually fired by the end of the run.
    let hook_hits = Arc::new(AtomicUsize::new(0));
    let (h1, h2, h3, h4, h5) = (
        hook_hits.clone(),
        hook_hits.clone(),
        hook_hits.clone(),
        hook_hits.clone(),
        hook_hits.clone(),
    );

    let hooks = Hooks::default()
        .before_send(move |msg| {
            h1.fetch_add(1, Ordering::SeqCst);
            eprintln!("[hook] before_send   role={}", msg.role);
            Ok(())
        })
        .after_receive(move |msg| {
            h2.fetch_add(1, Ordering::SeqCst);
            let tool_uses = msg.tool_uses().len();
            eprintln!(
                "[hook] after_receive role={} tool_uses={tool_uses}",
                msg.role
            );
            Ok(())
        })
        .before_tool_call(move |call| {
            h3.fetch_add(1, Ordering::SeqCst);
            eprintln!(
                "[hook] before_tool_call name={} input={}",
                call.name, call.input
            );
            Ok(())
        })
        .after_tool_call(move |result| {
            h4.fetch_add(1, Ordering::SeqCst);
            eprintln!(
                "[hook] after_tool_call  name={} content={}",
                result.name, result.content
            );
            Ok(())
        })
        // on_event observes the live `session.event` stream delivered over the
        // `/rpc` NDJSON notifications. We give MCP events special treatment to
        // show the real (non-stub) `mcp.request` / `mcp.response` payloads.
        .on_event(move |event| {
            h5.fetch_add(1, Ordering::SeqCst);
            match event.event_type.as_str() {
                "mcp.request" => {
                    if let Ok(p) = serde_json::from_value::<McpRequestPayload>(event.payload.clone())
                    {
                        eprintln!(
                            "[event] mcp.request  server={} tool={} method={}",
                            p.server_name.as_deref().unwrap_or("<unrouted>"),
                            p.tool,
                            p.method
                        );
                    }
                }
                "mcp.response" => {
                    if let Ok(p) =
                        serde_json::from_value::<McpResponsePayload>(event.payload.clone())
                    {
                        eprintln!(
                            "[event] mcp.response server={} ok={}{}",
                            p.server_name.as_deref().unwrap_or("<unrouted>"),
                            p.ok,
                            p.error.map(|e| format!(" error={e}")).unwrap_or_default()
                        );
                    }
                }
                other => eprintln!("[event] {other}"),
            }
            Ok(())
        });

    // --- 4. Open a session and run the loop --------------------------------
    let mut session = Harness::new(config)
        .with_tool(get_current_time)
        .with_hooks(hooks)
        .connect()
        .await
        .map_err(explain)?;

    eprintln!(
        "opened session {} against profile '{}'",
        session.session_id(),
        session.profile().name
    );
    eprintln!("> {prompt}\n");

    let reply = session.send(prompt).await.map_err(explain)?;

    // --- 5. Print the final assistant turn ---------------------------------
    println!("{}", reply.text());

    // --- 5b. Optional: tap the live event feed via session.subscribe -------
    // Opt-in (set BAE_SUBSCRIBE_DEMO) so the example stays a quick one-shot.
    // A bogus `since_event_id` forces a replay from the start of the session;
    // we stop after the first event (returning `false`) so the demo terminates
    // — a real observer would keep the stream open for live notifications.
    if std::env::var("BAE_SUBSCRIBE_DEMO").is_ok() {
        eprintln!("[subscribe] replaying session events (stopping after the first)…");
        session
            .subscribe(Some("evt_replay_from_start"), |event| {
                eprintln!("[subscribe] {} {}", event.event_type, event.id);
                false
            })
            .await
            .map_err(explain)?;
    }

    // Best-effort close; a failure here shouldn't mask a successful run.
    if let Err(err) = session.close().await {
        eprintln!("[warn] closing session failed: {err}");
    }

    let hits = hook_hits.load(Ordering::SeqCst);
    eprintln!("\n(hook callbacks fired {hits} times across the round-trip)");
    Ok(())
}

/// Read a required environment variable or return a clear error.
fn require_env(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name).map_err(|_| format!("environment variable `{name}` is required").into())
}

/// Turn a harness [`bae_rs::Error`] into a friendlier message for the common
/// provider-failure case, so operators know to check their provider key/config.
fn explain(err: bae_rs::Error) -> Box<dyn Error> {
    match err {
        bae_rs::Error::ProvidersFailed { events } => format!(
            "the server could not reach any LLM provider. This usually means the \
             profile's provider key is unset/invalid server-side, or the provider is down. \
             {} event(s) were recorded for this turn; inspect the `provider.response` \
             failures via GET /api/v1/sessions/<id>/events.",
            events.len()
        )
        .into(),
        other => Box::new(other),
    }
}

/// Format the current time as a UTC ISO-8601 string (e.g. `2026-07-06T12:34:56Z`)
/// with no external date crate. Uses Howard Hinnant's days-from-civil algorithm.
fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // days -> civil date (proleptic Gregorian), epoch = 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}
