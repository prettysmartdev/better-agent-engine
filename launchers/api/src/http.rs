//! The axum HTTP surface: router, handlers, auth, and the streamed trigger
//! response.
//!
//! One shared [`Router`] on one port carries every route:
//!
//! | Route                          | Auth\*         | Purpose                         |
//! |--------------------------------|----------------|---------------------------------|
//! | `POST /agents/{name}/trigger`  | Bearer if set  | Validate, template, spawn, stream |
//! | `GET  /healthz`                | never          | Liveness                        |
//! | `GET  /_launcher/agents`       | never          | List agents (safe fields only)  |
//! | `GET  /_launcher/agents/{name}`| never          | One agent's detail              |
//!
//! \* Auth applies only when `BAE_LAUNCHER_API_TOKEN` is set, and then only to
//! `/agents/*` — `/healthz` and `/_launcher/*` are always open.
//!
//! Concurrency: every trigger spawns its own `tokio::process::Command` and
//! streams independently — there is no per-agent lock, so two triggers of the
//! same agent, or of different agents, run fully concurrently, and a hung child
//! (its request future simply parks) never blocks another route.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio_stream::{Stream, StreamExt};
use tower_http::services::{ServeDir, ServeFile};

use launcher_core::LogLine;

use crate::config::{Agent, LoadedConfig, PromptConfig};
use crate::error::ApiError;
use crate::{template, DEFAULT_ADDR, ENV_ADDR, ENV_TOKEN, ENV_WEBAPP_STATIC_DIR};

/// Shared, cheaply-cloneable router state.
#[derive(Clone)]
pub struct AppState {
    /// Agents keyed by `name` for O(1) trigger/detail lookup.
    agents: Arc<HashMap<String, Arc<Agent>>>,
    /// Agent names in config order, for the introspection list.
    order: Arc<Vec<String>>,
    /// The bearer token, if `BAE_LAUNCHER_API_TOKEN` was set. `None` = open port.
    token: Option<Arc<String>>,
}

impl AppState {
    /// Construct router state for the running server and offline integration
    /// tests. `token` is passed explicitly so tests never need to mutate the
    /// process environment to exercise route-scoped authentication.
    pub fn new(loaded: LoadedConfig, token: Option<String>) -> Self {
        let order: Vec<String> = loaded
            .agents
            .iter()
            .map(|a| a.config.name.clone())
            .collect();
        let agents: HashMap<String, Arc<Agent>> = loaded
            .agents
            .into_iter()
            .map(|a| (a.config.name.clone(), a))
            .collect();
        AppState {
            agents: Arc::new(agents),
            order: Arc::new(order),
            token: token.map(Arc::new),
        }
    }
}

/// Build the router, bind the listener, and serve until a shutdown signal.
///
/// On `SIGTERM`/`SIGINT` the server drains in-flight requests gracefully, but
/// only for up to `shutdown_timeout` (`BAE_LAUNCHER_API_SHUTDOWN_TIMEOUT`,
/// default 30s): a hung child agent holding a trigger request open must never
/// keep the launcher itself alive indefinitely. When the bound elapses the
/// process exits anyway; dropping the runtime drops every in-flight trigger's
/// stream, and `kill_on_drop` (in `launcher_core`) reaps the children.
///
/// Returns the process [`ExitCode`] (1 on a bind failure, otherwise 0 — a
/// timed-out drain is still a clean, deliberate shutdown).
pub async fn serve(loaded: LoadedConfig, shutdown_timeout: std::time::Duration) -> ExitCode {
    // Address precedence: BAE_LAUNCHER_API_ADDR > [server] addr > default.
    let addr = std::env::var(ENV_ADDR)
        .ok()
        .or_else(|| loaded.addr.clone())
        .unwrap_or_else(|| DEFAULT_ADDR.to_string());

    let token = std::env::var(ENV_TOKEN).ok().filter(|t| !t.is_empty());
    if token.is_some() {
        tracing::info!("BAE_LAUNCHER_API_TOKEN set — /agents/* routes require a Bearer token");
    } else {
        tracing::warn!(
            "BAE_LAUNCHER_API_TOKEN is UNSET — every /agents/* trigger route is OPEN (no auth). \
             Set it, and keep this port behind a TLS-terminating reverse proxy on an internal \
             network (see aspec/architecture/security.md)."
        );
    }

    let agent_count = loaded.agents.len();
    let state = AppState::new(loaded, token);
    let static_dir = std::env::var(ENV_WEBAPP_STATIC_DIR)
        .ok()
        .filter(|dir| !dir.trim().is_empty());
    let app = match static_dir {
        Some(dir) => {
            let index = PathBuf::from(&dir).join("index.html");
            tracing::info!(static_dir = %dir, "serving webapp static SPA with index.html fallback");
            router(state).fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
        }
        None => router(state),
    };

    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr, "failed to bind listener: {e}");
            return ExitCode::from(1);
        }
    };
    tracing::info!(
        addr,
        agents = agent_count,
        "baeapi listening (plain HTTP; TLS terminates upstream)"
    );

    // `draining_rx` resolves once the shutdown signal has fired, letting the
    // select below start the drain-bound timer at the right moment.
    let (draining_tx, draining_rx) = tokio::sync::oneshot::channel::<()>();
    // `WithGracefulShutdown` is IntoFuture (not Future), so convert explicitly
    // for use inside select!.
    let server = std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = draining_tx.send(());
        }),
    );
    tokio::pin!(server);

    let drain_deadline = async move {
        // If the sender was dropped without firing (server ended first), park
        // forever — the other select arm completes.
        if draining_rx.await.is_err() {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(shutdown_timeout).await;
    };

    tokio::select! {
        served = &mut server => match served {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                tracing::error!("server error: {e}");
                ExitCode::from(1)
            }
        },
        _ = drain_deadline => {
            tracing::warn!(
                timeout_secs = shutdown_timeout.as_secs(),
                "graceful drain did not finish within the shutdown timeout; \
                 exiting and force-killing in-flight agent invocations"
            );
            ExitCode::from(0)
        }
    }
}

/// Assemble the shared router. `/agents/*` gets the bearer-auth layer only when a
/// token is configured; the fixed liveness/introspection routes never do.
pub fn router(state: AppState) -> Router {
    let mut agent_routes = Router::new().route("/agents/{name}/trigger", post(trigger));
    if state.token.is_some() {
        // `route_layer` scopes the auth to these routes only — a 404 fallback is
        // never gated behind it, and the fixed routes below never see it.
        agent_routes = agent_routes.route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    }

    Router::new()
        .route("/healthz", get(healthz))
        .route("/_launcher/agents", get(list_agents))
        .route("/_launcher/agents/{name}", get(get_agent))
        .merge(agent_routes)
        .layer(axum::middleware::from_fn(log_requests))
        .with_state(state)
}

/// Unauthenticated liveness probe — always 200, no body.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Resolve when the process receives `SIGTERM` or `SIGINT`. The graceful drain
/// this starts waits for in-flight requests — including a hung child's
/// still-open trigger request — so [`serve`] bounds it with the shutdown
/// timeout rather than waiting forever; only when the process then exits do the
/// dropped streams' `kill_on_drop` children get reaped.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return,
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    tracing::info!("shutdown signal received; draining in-flight requests");
}

// ---------------------------------------------------------------------------
// Introspection (never exposes env / env_template values or resolved secrets)
// ---------------------------------------------------------------------------

/// The safe, read-only view of an agent. Deliberately omits `command`, `args`,
/// `env`, `env_template`, `arg_template`, and `working_dir` — only presentation
/// metadata and the request schema are exposed (work item 0014 sections C/D).
#[derive(Debug, Serialize)]
struct AgentView<'a> {
    name: &'a str,
    display_name: Option<&'a str>,
    description: Option<&'a str>,
    icon: Option<&'a str>,
    request_schema: Option<&'a serde_json::Value>,
    chat_input_field: &'a str,
    prompts: &'a [PromptConfig],
}

impl<'a> AgentView<'a> {
    fn of(agent: &'a Agent) -> Self {
        let c = &agent.config;
        AgentView {
            name: &c.name,
            display_name: c.display_name.as_deref(),
            description: c.description.as_deref(),
            icon: c.icon.as_deref(),
            request_schema: c.request_schema.as_ref(),
            chat_input_field: &c.chat_input_field,
            prompts: &c.prompts,
        }
    }
}

/// `GET /_launcher/agents` — every configured agent in config order.
async fn list_agents(State(state): State<AppState>) -> Response {
    let views: Vec<AgentView> = state
        .order
        .iter()
        .filter_map(|name| state.agents.get(name))
        .map(|a| AgentView::of(a))
        .collect();
    Json(views).into_response()
}

/// `GET /_launcher/agents/{name}` — one agent's detail, or 404.
async fn get_agent(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.agents.get(&name) {
        Some(agent) => Json(AgentView::of(agent)).into_response(),
        None => ApiError::not_found(format!("no agent named {name:?}")).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

/// `POST /agents/{name}/trigger` — validate → template → spawn → stream.
async fn trigger(State(state): State<AppState>, Path(name): Path<String>, body: Bytes) -> Response {
    let Some(agent) = state.agents.get(&name).cloned() else {
        // With a single `{name}` route, an unknown agent is a 404 here rather
        // than a router miss (the trigger route pattern still matched).
        return ApiError::not_found(format!("no agent named {name:?}")).into_response();
    };

    // Parse the body as JSON. An empty body is treated as `{}` (some agents have
    // an all-optional schema, or none at all).
    let value: serde_json::Value = if body.is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return ApiError::bad_request(format!("request body is not valid JSON: {e}"))
                    .into_response()
            }
        }
    };

    // Validate BEFORE templating; a schema failure returns 400 and never spawns.
    if let Some(validator) = &agent.validator {
        let failures: Vec<String> = validator
            .iter_errors(&value)
            .map(|e| format!("{}: {}", instance_path(&e), e))
            .collect();
        if !failures.is_empty() {
            return ApiError::schema_validation(&failures).into_response();
        }
    }

    // Template into a spawn spec. An unset `${VAR}` in static env → 500.
    let spec = match template::build_spec(&agent.config, &value, &|k| std::env::var(k).ok()) {
        Ok(spec) => spec,
        Err(e) => return ApiError::from_launcher(&e).into_response(),
    };

    // Spawn and peek the first stream item so a spawn failure (bad `command`)
    // becomes a clean 500 instead of a 200 with an error buried in the body.
    // The cost is that the response headers wait for the child's first output
    // (or its exit, if silent) — an acceptable trade for a correct status code,
    // and one that never blocks other routes (this future simply parks).
    let mut stream: Pin<Box<dyn Stream<Item = LogLine> + Send>> =
        Box::pin(launcher_core::spawn_and_stream(&spec));

    match stream.next().await {
        Some(LogLine::SpawnFailed { message }) => {
            ApiError::internal(format!("failed to start agent {name:?}: {message}")).into_response()
        }
        Some(first) => {
            // Re-prepend the peeked item, then render every LogLine to bytes.
            // Each captured output line is ALSO forwarded to the launcher's own
            // stdout/stderr (already `[name]`-prefixed by launcher_core), so
            // `docker logs` stays the common, attributed log surface for every
            // launcher — the streamed response is a copy, not the only record.
            let body_stream = tokio_stream::once(first).chain(stream).map(|line| {
                forward_to_launcher_logs(&line);
                Ok::<Bytes, Infallible>(render_line(line))
            });
            Response::builder()
                .header(CONTENT_TYPE, "application/x-ndjson")
                .body(Body::from_stream(body_stream))
                .expect("static content-type header is always valid")
        }
        // A stream that yields nothing is not expected (spawn_and_stream always
        // ends with one terminal item), but handle it as an empty 200.
        None => Response::builder()
            .header(CONTENT_TYPE, "application/x-ndjson")
            .body(Body::empty())
            .expect("static content-type header is always valid"),
    }
}

/// Copy one captured child output line to the launcher's own stdout/stderr
/// (matching the stream it came from), so a multi-agent image's `docker logs`
/// carries every agent's attributed output exactly like the schedule launcher's.
/// Terminal items (exit / spawn-failure) are response-body metadata only.
fn forward_to_launcher_logs(line: &LogLine) {
    if let LogLine::Output { stream, line } = line {
        match stream {
            launcher_core::OutputStream::Stdout => println!("{line}"),
            launcher_core::OutputStream::Stderr => eprintln!("{line}"),
        }
    }
}

/// Render one [`LogLine`] as an NDJSON response chunk.
///
/// Output lines are forwarded verbatim (already `[name] …`-prefixed and capped
/// by `launcher_core`) with a newline; the terminal item becomes a trailing
/// NDJSON object carrying the exit code (`null` if the child was signalled).
fn render_line(line: LogLine) -> Bytes {
    match line {
        LogLine::Output { line, .. } => Bytes::from(format!("{line}\n")),
        LogLine::Exited { code } => {
            let obj = serde_json::json!({ "exit_code": code });
            Bytes::from(format!("{obj}\n"))
        }
        // Only reachable if a spawn failure arrives as a NON-first item, which
        // `spawn_and_stream` never does (spawn failure is the sole terminal
        // item). Rendered defensively so the body still terminates cleanly.
        LogLine::SpawnFailed { message } => {
            let obj = serde_json::json!({ "error": message, "exit_code": serde_json::Value::Null });
            Bytes::from(format!("{obj}\n"))
        }
    }
}

/// Format a validation error's instance path for the 400 body. An empty path
/// (the root value itself failed, e.g. a missing required property) renders as
/// `<root>`.
fn instance_path(error: &jsonschema::ValidationError<'_>) -> String {
    let path = error.instance_path().to_string();
    if path.is_empty() {
        // `required` violations point at the containing object in the
        // validator, but the useful failing location is the missing property
        // itself. Preserve the RFC 7807 detail's path-oriented contract for
        // that common case.
        if let jsonschema::error::ValidationErrorKind::Required { property } = error.kind() {
            if let Some(property) = property.as_str() {
                return format!("/{property}");
            }
        }
        "<root>".to_string()
    } else {
        path
    }
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Enforce `Authorization: Bearer <token>` on `/agents/*` (only installed when a
/// token is configured). The comparison is constant-time (timing-oracle
/// resistant), mirroring the server's key verification.
async fn require_bearer(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // Defensive: this layer is only added when the token is `Some`.
    let Some(expected) = state.token.as_ref() else {
        return next.run(request).await;
    };
    let provided = match bearer_token(&headers) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        next.run(request).await
    } else {
        ApiError::unauthorized("invalid bearer token").into_response()
    }
}

/// Extract the `Authorization: Bearer <token>` value, or a 401. Case-insensitive
/// scheme, non-empty token — mirrors the server's `bearer_token`.
fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .ok_or_else(|| ApiError::unauthorized("Authorization header must be a Bearer token"))?;
    if token.is_empty() {
        return Err(ApiError::unauthorized("empty bearer token"));
    }
    Ok(token.to_owned())
}

/// Constant-time byte-slice equality. Length is not secret (bearer tokens are a
/// fixed operator choice), but the content comparison must not short-circuit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// One INFO log line per request (method, path, status, latency). `/healthz` is
/// logged at DEBUG so container health checks don't drown the log, matching the
/// server's `log_requests`.
async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let is_health = path == "/healthz";
    let started = std::time::Instant::now();

    let response = next.run(request).await;

    let status = response.status().as_u16();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if is_health {
        tracing::debug!(%method, path, status, elapsed_ms, "http request");
    } else {
        tracing::info!(%method, path, status, elapsed_ms, "http request");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;
    use tokio::time::{timeout, Duration};
    use tower::ServiceExt;

    fn agent(name: &str, field: &str, command: &str, args: Vec<String>) -> Arc<Agent> {
        let mut properties = serde_json::Map::new();
        properties.insert(field.to_string(), json!({"type": "string"}));
        let schema = json!({
            "type": "object",
            "required": [field],
            "properties": properties
        });
        let config = crate::config::AgentConfig {
            name: name.to_string(),
            command: command.to_string(),
            args,
            working_dir: None,
            env: HashMap::new(),
            request_schema: Some(schema.clone()),
            env_template: vec![crate::config::EnvTemplate {
                field: field.to_string(),
                env: format!("{name}_VALUE"),
            }],
            arg_template: Vec::new(),
            display_name: Some(format!("{name} display")),
            description: Some(format!("{name} description")),
            icon: Some("⭐".to_string()),
            chat_input_field: field.to_string(),
            prompts: vec![crate::config::PromptConfig {
                label: "Example".to_string(),
                prompt: "example".to_string(),
            }],
        };
        Arc::new(Agent {
            validator: Some(jsonschema::validator_for(&schema).expect("test schema")),
            config,
        })
    }

    fn app(token: Option<&str>, agents: Vec<Arc<Agent>>) -> Router {
        let loaded = LoadedConfig { addr: None, agents };
        router(AppState::new(loaded, token.map(str::to_string)))
    }

    async fn body_string(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("response body")
                .to_vec(),
        )
        .expect("utf8 response")
    }

    fn trigger_request(name: &str, value: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/agents/{name}/trigger"))
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .expect("request")
    }

    #[tokio::test]
    async fn each_route_uses_its_own_schema_and_rejects_before_spawn() {
        let marker =
            std::env::temp_dir().join(format!("baeapi-schema-not-spawned-{}", std::process::id()));
        fs::remove_file(&marker).ok();
        let mut alpha = agent("alpha", "prompt", "printf", vec!["alpha\n".to_string()]);
        let alpha_config = &mut Arc::get_mut(&mut alpha)
            .expect("unique fixture agent")
            .config;
        alpha_config.command = "sh".to_string();
        alpha_config.args = vec![
            "-c".to_string(),
            format!("echo spawned > {}", marker.display()),
        ];
        let app = app(
            None,
            vec![
                alpha,
                agent("beta", "text", "printf", vec!["beta\n".to_string()]),
            ],
        );

        let alpha_wrong = app
            .clone()
            .oneshot(trigger_request("alpha", json!({"text": "wrong field"})))
            .await
            .expect("response");
        assert_eq!(alpha_wrong.status(), StatusCode::BAD_REQUEST);
        let problem = body_string(alpha_wrong).await;
        assert!(problem.contains("/prompt"));
        assert!(problem.contains("bad_request"));
        assert!(!marker.exists(), "schema-invalid body spawned a child");

        let beta_right = app
            .oneshot(trigger_request("beta", json!({"text": "right field"})))
            .await
            .expect("response");
        assert_eq!(beta_right.status(), StatusCode::OK);
        assert!(body_string(beta_right).await.contains("[beta] beta"));
        fs::remove_file(marker).ok();
    }

    #[tokio::test]
    async fn trigger_body_streams_incrementally_and_is_attributed() {
        let app = app(
            None,
            vec![
                agent(
                    "alpha",
                    "prompt",
                    "sh",
                    vec![
                        "-c".to_string(),
                        "printf 'first\\n'; sleep 1; printf 'second\\n'".to_string(),
                    ],
                ),
                agent("beta", "text", "printf", vec!["unused\\n".to_string()]),
            ],
        );
        let response = app
            .oneshot(trigger_request("alpha", json!({"prompt": "hello"})))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let first = timeout(Duration::from_millis(300), body.frame())
            .await
            .expect("first output arrives without waiting for child exit")
            .expect("first frame")
            .expect("valid frame")
            .into_data()
            .expect("data frame");
        assert!(String::from_utf8_lossy(&first).contains("[alpha] first"));

        let rest = to_bytes(body, usize::MAX).await.expect("remaining body");
        let rest = String::from_utf8(rest.to_vec()).expect("utf8");
        assert!(rest.contains("[alpha] second"));
        assert!(rest.contains("exit_code"));
    }

    #[tokio::test]
    async fn bearer_token_only_gates_agent_routes() {
        let app = app(
            Some("correct-token"),
            vec![
                agent("alpha", "prompt", "printf", vec!["alpha\n".to_string()]),
                agent("beta", "text", "printf", vec!["beta\n".to_string()]),
            ],
        );
        for name in ["alpha", "beta"] {
            let unauthorized = app
                .clone()
                .oneshot(trigger_request(name, json!({"prompt": "x", "text": "x"})))
                .await
                .expect("response");
            assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        }

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/_launcher/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);

        let mut authorized_request = trigger_request("alpha", json!({"prompt": "x"}));
        authorized_request
            .headers_mut()
            .insert("authorization", "Bearer correct-token".parse().unwrap());
        let authorized = app.oneshot(authorized_request).await.unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn introspection_lists_all_agents_without_secret_fields() {
        let mut alpha = agent("alpha", "prompt", "printf", vec!["alpha\n".to_string()]);
        Arc::get_mut(&mut alpha)
            .expect("unique fixture agent")
            .config
            .env
            .insert("SECRET".to_string(), "${TEST_LAUNCHER_SECRET}".to_string());
        let app = app(
            None,
            vec![
                alpha,
                agent("beta", "text", "printf", vec!["beta\n".to_string()]),
            ],
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/_launcher/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_string(response).await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 2);
        assert_eq!(value[0]["name"], "alpha");
        assert_eq!(value[1]["name"], "beta");
        assert!(!body.contains("TEST_LAUNCHER_SECRET"));
        assert!(!body.contains("env_template"));
        assert!(!body.contains("${"));
    }

    #[tokio::test]
    async fn concurrent_triggers_of_the_same_agent_spawn_independently() {
        // Two overlapping triggers of ONE agent must both run, concurrently,
        // with no de-dup or locking — deliberately asymmetric with the schedule
        // launcher's same-agent overlap-skip (work item 0014 edge cases).
        let app = app(
            None,
            vec![agent(
                "same",
                "prompt",
                "sh",
                vec!["-c".to_string(), "sleep 0.7; printf 'ran\\n'".to_string()],
            )],
        );
        let started = std::time::Instant::now();
        let (a, b) = tokio::join!(
            app.clone()
                .oneshot(trigger_request("same", json!({"prompt": "one"}))),
            app.oneshot(trigger_request("same", json!({"prompt": "two"}))),
        );
        let (a, b) = (a.expect("first response"), b.expect("second response"));
        assert_eq!(a.status(), StatusCode::OK);
        assert_eq!(b.status(), StatusCode::OK);
        let (a, b) = (body_string(a).await, body_string(b).await);
        assert!(a.contains("[same] ran") && a.contains("\"exit_code\":0"));
        assert!(b.contains("[same] ran") && b.contains("\"exit_code\":0"));
        // Serialized execution would take >= 1.4s; concurrent ~0.7s.
        assert!(
            started.elapsed() < Duration::from_millis(1300),
            "same-agent triggers must not serialize (took {:?})",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn unset_env_ref_is_a_problem_json_500_and_never_spawns() {
        let marker = std::env::temp_dir().join(format!(
            "baeapi-unset-env-not-spawned-{}",
            std::process::id()
        ));
        fs::remove_file(&marker).ok();
        let mut broken = agent("broken", "prompt", "sh", Vec::new());
        {
            let config = &mut Arc::get_mut(&mut broken)
                .expect("unique fixture agent")
                .config;
            config.args = vec![
                "-c".to_string(),
                format!("echo spawned > {}", marker.display()),
            ];
            config.env.insert(
                "SECRET".to_string(),
                "${DEFINITELY_UNSET_LAUNCHER_TEST_SECRET}".to_string(),
            );
        }
        let app = app(None, vec![broken]);
        let response = app
            .oneshot(trigger_request("broken", json!({"prompt": "x"})))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // RFC 7807 problem details carry their own media type.
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
        let body = body_string(response).await;
        assert!(body.contains("DEFINITELY_UNSET_LAUNCHER_TEST_SECRET"));
        assert!(body.contains("\"status\":500"));
        assert!(!marker.exists(), "unset ${{VAR}} must fail before spawning");
    }

    #[tokio::test]
    async fn nonzero_child_exit_is_reported_in_stream_and_leaves_routes_healthy() {
        let app = app(
            None,
            vec![agent(
                "flaky",
                "prompt",
                "sh",
                vec![
                    "-c".to_string(),
                    "printf 'about to fail\\n'; exit 5".to_string(),
                ],
            )],
        );
        let response = app
            .clone()
            .oneshot(trigger_request("flaky", json!({"prompt": "x"})))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        assert!(body.contains("[flaky] about to fail"));
        assert!(body.contains("\"exit_code\":5"));

        // A crashed child is a per-invocation event; the server keeps serving.
        let health = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn hung_trigger_does_not_block_a_different_agent_route() {
        let app = app(
            None,
            vec![
                agent(
                    "hung",
                    "prompt",
                    "sh",
                    vec!["-c".to_string(), "sleep 60".to_string()],
                ),
                agent("fast", "text", "printf", vec!["fast\n".to_string()]),
            ],
        );
        let hung = tokio::spawn({
            let app = app.clone();
            async move {
                app.oneshot(trigger_request("hung", json!({"prompt": "wait"})))
                    .await
            }
        });
        let fast = timeout(
            Duration::from_millis(500),
            app.oneshot(trigger_request("fast", json!({"text": "now"}))),
        )
        .await
        .expect("fast route is independent")
        .expect("fast response");
        assert_eq!(fast.status(), StatusCode::OK);
        assert!(body_string(fast).await.contains("[fast] fast"));
        hung.abort();
        let _ = hung.await;
    }
}
