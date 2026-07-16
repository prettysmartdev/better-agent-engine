//! Process-level OpenTelemetry E2E coverage.
//!
//! Unlike `integration.rs`'s in-memory exporter tests, this starts the real
//! `baesrv` binary with `[telemetry]` enabled and accepts the OTLP/gRPC it
//! exports.  The Rust SDK is given a real (test-only) OTel SDK which exports to
//! that same receiver, proving W3C propagation joins client and server spans
//! in the payload a collector actually receives.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use bae_rs::{Config as ClientConfig, Harness, Tool};
use opentelemetry::global;
use opentelemetry::trace::{FutureExt as _, TraceContextExt as _, Tracer as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry::Context;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_proto::tonic::collector::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer}, ExportTraceServiceRequest,
    ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use opentelemetry_proto::tonic::trace::v1::Span;
use opentelemetry_sdk::trace::SdkTracerProvider;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status};

#[derive(Clone, Default)]
struct OtlpReceiver {
    exports: Arc<Mutex<Vec<ExportTraceServiceRequest>>>,
}

// The Rust OTel API owns its tracer provider and propagator globally. These
// process-level tests therefore cannot run concurrently without one test's
// ambient client spans appearing in the other's receiver.
static E2E_SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn e2e_serial() -> &'static tokio::sync::Mutex<()> {
    E2E_SERIAL.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[tonic::async_trait]
impl TraceService for OtlpReceiver {
    async fn export(
        &self,
        request: GrpcRequest<ExportTraceServiceRequest>,
    ) -> Result<GrpcResponse<ExportTraceServiceResponse>, Status> {
        self.exports.lock().unwrap().push(request.into_inner());
        Ok(GrpcResponse::new(ExportTraceServiceResponse::default()))
    }
}

struct RunningReceiver {
    receiver: OtlpReceiver,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
}

impl RunningReceiver {
    async fn start() -> Self {
        let receiver = OtlpReceiver::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OTLP receiver");
        let addr = listener.local_addr().expect("OTLP receiver address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let service = receiver.clone();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(TraceServiceServer::new(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve OTLP receiver");
        });
        Self {
            receiver,
            endpoint: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
        }
    }

    fn clear(&self) {
        self.receiver.exports.lock().unwrap().clear();
    }

    fn spans(&self) -> Vec<CapturedSpan> {
        self.receiver
            .exports
            .lock()
            .unwrap()
            .iter()
            .flat_map(|export| export.resource_spans.iter())
            .flat_map(|resource| resource.scope_spans.iter())
            .flat_map(|scope| {
                let scope_name = scope.scope.as_ref().map(|scope| scope.name.clone());
                scope.spans.iter().cloned().map(move |span| CapturedSpan {
                    scope: scope_name.clone().unwrap_or_default(),
                    span,
                })
            })
            .collect()
    }
}

impl Drop for RunningReceiver {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[derive(Clone)]
struct CapturedSpan {
    scope: String,
    span: Span,
}

/// A tiny Anthropic-shaped provider.  It is the canonical client-tool parity
/// fixture: the first response asks for one client tool; the result round trip
/// receives a final assistant message.
async fn provider_mock(request: Request) -> Response {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .expect("read provider request");
    let body: Value = serde_json::from_slice(&bytes).expect("provider JSON");
    let has_tool_result = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| messages.last())
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        });
    let reply = if has_tool_result {
        json!({
            "role": "assistant",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "tool round-trip complete"}],
        })
    } else {
        json!({
            "role": "assistant",
            "stop_reason": "tool_use",
            "content": [{
                "type": "tool_use",
                "id": "tu_e2e",
                "name": "get_current_time",
                "input": {},
            }],
        })
    };
    (StatusCode::OK, Json(reply)).into_response()
}

async fn start_provider_mock() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider mock");
    let addr = listener.local_addr().expect("provider mock address");
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(provider_mock))
            .await
            .expect("serve provider mock");
    });
    format!("http://{addr}")
}

struct RunningServer {
    child: Child,
    client_url: String,
    admin_url: String,
    dir: PathBuf,
}

/// The shutdown budget the E2E server runs with. The whole graceful shutdown —
/// draining plus the telemetry flush/shutdown — must complete within this
/// single window (contract §5; work item's graceful-shutdown-flush edge case).
const SHUTDOWN_TIMEOUT_SECS: u64 = 5;

impl RunningServer {
    async fn stop(mut self) {
        // SIGTERM is intentionally used instead of `start_kill`: the server's
        // graceful-shutdown path force-flushes its batch exporter, which is the
        // behavior this test is meant to exercise.
        #[cfg(unix)]
        {
            // `kill` is commonly a shell builtin rather than an executable
            // (including the minimal CI image), so invoke the POSIX shell's
            // builtin and assert delivery. This must be SIGTERM, not
            // `Child::start_kill()`, to exercise baesrv's graceful flush path.
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg("kill -TERM \"$1\"")
                .arg("sh")
                .arg(self.child.id().expect("baesrv process id").to_string())
                .status()
                .expect("run shell builtin kill");
            assert!(status.success(), "send SIGTERM to baesrv");
        }
        #[cfg(not(unix))]
        let _ = self.child.start_kill();
        // The process — drain AND telemetry flush/shutdown together — must exit
        // within the single configured budget (plus a small scheduling margin),
        // never linger and lose buffered spans. A timeout here is a real failure
        // of the "flush within BAE_SHUTDOWN_TIMEOUT" guarantee, so assert it.
        let started = std::time::Instant::now();
        let budget = Duration::from_secs(SHUTDOWN_TIMEOUT_SECS) + Duration::from_secs(3);
        let waited = tokio::time::timeout(budget, self.child.wait()).await;
        assert!(
            waited.is_ok(),
            "baesrv did not shut down within the {SHUTDOWN_TIMEOUT_SECS}s budget (+margin); \
             took at least {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn unused_loopback_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    listener.local_addr().expect("reserved loopback address")
}

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("bae-telemetry-e2e-{label}-{nonce}"))
}

async fn start_server(provider: &str, telemetry: Option<&str>) -> RunningServer {
    let dir = test_dir(if telemetry.is_some() { "enabled" } else { "disabled" });
    std::fs::create_dir_all(&dir).expect("create test directory");
    let config = dir.join("bae-config.toml");
    let telemetry_section = telemetry.unwrap_or("[telemetry]\nenabled = false\n");
    std::fs::write(
        &config,
        format!(
            "{telemetry_section}\n[[providers.entries]]\nname = \"tool\"\nprovider = \"anthropic\"\nbase_url = \"{provider}\"\nmodel = \"claude-e2e\"\nauth_token = \"test-token\"\n"
        ),
    )
    .expect("write bae-config.toml");

    let client_addr = unused_loopback_addr();
    let admin_addr = unused_loopback_addr();
    let mut child = Command::new(env!("CARGO_BIN_EXE_baesrv"))
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .arg("--dangerously-disable-admin-auth")
        .env("BAE_ADDR", client_addr.to_string())
        .env("BAE_ADMIN_ADDR", admin_addr.to_string())
        .env("BAE_DB_PATH", dir.join("test.db"))
        .env("BAE_LOG", "baesrv=warn")
        // A small, explicit shutdown budget so the graceful-shutdown flush test
        // (`stop`) can assert the whole shutdown — drain plus telemetry
        // flush/shutdown — completes within the single window.
        .env("BAE_SHUTDOWN_TIMEOUT", SHUTDOWN_TIMEOUT_SECS.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start baesrv binary");
    let client_url = format!("http://{client_addr}");
    let http = Client::new();
    for _ in 0..100 {
        if let Ok(response) = http.get(format!("{client_url}/healthz")).send().await {
            if response.status().is_success() {
                return RunningServer {
                    child,
                    client_url,
                    admin_url: format!("http://{admin_addr}"),
                    dir,
                };
            }
        }
        if child.try_wait().expect("inspect baesrv process").is_some() {
            panic!("baesrv exited before becoming ready");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = child.start_kill();
    panic!("baesrv did not become ready");
}

async fn create_client_key(server: &RunningServer) -> String {
    let http = Client::new();
    let profile = http
        .post(format!("{}/admin/v1/profiles", server.admin_url))
        .json(&json!({
            "name": "telemetry-e2e",
            "primary_provider": "tool",
            "fallback_providers": [],
            "allowed_tools": ["get_current_time"],
        }))
        .send()
        .await
        .expect("create profile request");
    assert_eq!(profile.status(), StatusCode::CREATED);
    let profile: Value = profile.json().await.expect("profile JSON");
    let profile_id = profile["id"].as_str().expect("profile id");
    let key = http
        .post(format!("{}/admin/v1/keys", server.admin_url))
        .json(&json!({"name": "telemetry-e2e-client", "profile_id": profile_id}))
        .send()
        .await
        .expect("create client key request");
    assert_eq!(key.status(), StatusCode::CREATED);
    key.json::<Value>()
        .await
        .expect("client key JSON")["key"]
        .as_str()
        .expect("plaintext client key")
        .to_string()
}

fn install_client_exporter(endpoint: &str) -> SdkTracerProvider {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .expect("build client OTLP exporter");
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .build();
    global::set_tracer_provider(provider.clone());
    global::set_text_map_propagator(TraceContextPropagator::new());
    provider
}

async fn wait_for_spans(receiver: &RunningReceiver, at_least: usize) -> Vec<CapturedSpan> {
    for _ in 0..100 {
        let spans = receiver.spans();
        if spans.len() >= at_least {
            return spans;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    receiver.spans()
}

fn find_span<'a>(spans: &'a [CapturedSpan], scope: &str, name: &str) -> &'a CapturedSpan {
    spans
        .iter()
        .find(|captured| captured.scope == scope && captured.span.name == name)
        .unwrap_or_else(|| panic!("missing {scope} span {name}; got {:?}", spans.iter().map(|s| (&s.scope, &s.span.name)).collect::<Vec<_>>()))
}

fn direct_child<'a>(spans: &'a [CapturedSpan], parent: &[u8], name: &str) -> &'a CapturedSpan {
    spans
        .iter()
        .find(|captured| captured.span.name == name && captured.span.parent_span_id == parent)
        .unwrap_or_else(|| panic!("missing {name} child of {parent:?}"))
}

/// `http.request` is the internal `tracing` span name; the OTel layer exports
/// its contract-required display name (`"POST /api/v1/..."`) via `otel.name`.
/// Identify exported HTTP spans by their stable semantic attribute instead of
/// incorrectly expecting the internal tracing name in OTLP output.
fn is_http_request(span: &Span) -> bool {
    span.attributes
        .iter()
        .any(|attribute| attribute.key == "http.request.method")
}

fn direct_http_child<'a>(spans: &'a [CapturedSpan], parent: &[u8]) -> &'a CapturedSpan {
    spans
        .iter()
        .find(|captured| {
            captured.scope == "baesrv"
                && is_http_request(&captured.span)
                && captured.span.parent_span_id == parent
        })
        .unwrap_or_else(|| panic!("missing HTTP request child of {parent:?}"))
}

fn has_session_link(turn: &Span, session: &Span) -> bool {
    turn.links.iter().any(|link| {
        link.span_id == session.span_id
            && link.attributes.iter().any(|attribute| {
                attribute.key == "bae.link.kind"
                    && matches!(
                        attribute.value.as_ref().and_then(|value| value.value.as_ref()),
                        Some(AnyValue::StringValue(value)) if value == "session"
                    )
            })
    })
}

/// The real OTLP export path joins the Rust canonical tool-round-trip fixture
/// with server request/turn/tool spans.  It also proves that a disabled server
/// ignores a valid incoming `traceparent` and emits nothing to the receiver.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn otlp_receiver_observes_one_connected_client_server_trace_and_disabled_server_is_silent() {
    let _serial = e2e_serial().lock().await;
    let receiver = RunningReceiver::start().await;
    let provider_url = start_provider_mock().await;
    let client_provider = install_client_exporter(&receiver.endpoint);
    let server = start_server(
        &provider_url,
        Some(&format!(
            "[telemetry]\nenabled = true\notlp_endpoint = \"{}\"\n\n[telemetry.metrics]\nenabled = false\n",
            receiver.endpoint
        )),
    )
    .await;
    let client_key = create_client_key(&server).await;

    // An application-installed root span is the ambient context the SDK is
    // required to respect.  Keep it active across connect/send/close so every
    // request, including session open and close, is part of this one trace.
    let tracer = global::tracer("bae.e2e");
    let root = tracer.start("bae.e2e.root");
    let root_cx = Context::current_with_span(root);
    let client_url = server.client_url.clone();
    async move {
        let tool = Tool::new(
            "get_current_time",
            "Return a deterministic test time",
            json!({"type": "object", "properties": {}}),
            |_input| async move { Ok(json!("2026-07-15T00:00:00Z")) },
        );
        let mut session = Harness::new(ClientConfig::new(client_url, client_key))
            .with_tool(tool)
            .connect()
            .await
            .expect("SDK session connect");
        let reply = session.send("run the canonical tool fixture").await.expect("SDK send");
        assert_eq!(reply.text(), "tool round-trip complete");
        session.close().await.expect("SDK session close");
    }
    .with_context(root_cx.clone())
    .await;
    root_cx.span().end();

    // Server batch export is deliberately flushed through its normal graceful
    // shutdown path.  Client spans use a simple exporter, so no test-only
    // direct-export shortcut is involved on either side.
    server.stop().await;
    let spans = wait_for_spans(&receiver, 10).await;
    client_provider.shutdown().expect("flush client test provider");

    // Admin setup requests precede the app-root span and intentionally have
    // their own trace.  The assertion is about the driven agent interaction:
    // all BAE-named server spans plus the harness spans must share one trace.
    let bae_spans: Vec<_> = spans
        .iter()
        .filter(|captured| {
            captured.scope == "bae.client"
                || (captured.scope == "baesrv" && captured.span.name.starts_with("bae."))
        })
        .collect();
    assert!(!bae_spans.is_empty(), "receiver did not observe BAE spans");
    let trace_ids: HashSet<Vec<u8>> = bae_spans
        .iter()
        .map(|captured| captured.span.trace_id.clone())
        .collect();
    let topology: Vec<_> = bae_spans
        .iter()
        .map(|captured| (captured.scope.as_str(), captured.span.name.as_str(), &captured.span.trace_id))
        .collect();
    assert_eq!(
        trace_ids.len(),
        1,
        "BAE spans formed disjoint trace islands: {topology:?}"
    );

    // The canonical fixture has two send spans. Pick the first iteration by
    // topology rather than OTLP batch arrival order: it is the one whose
    // request owns the server tool-dispatch span.
    let send = spans
        .iter()
        .filter(|captured| captured.scope == "bae.client" && captured.span.name == "bae.client.send")
        .find(|send| {
            spans.iter().any(|request| {
                request.scope == "baesrv"
                    && is_http_request(&request.span)
                    && request.span.parent_span_id == send.span.span_id
                    && spans.iter().any(|turn| {
                        turn.scope == "baesrv"
                            && turn.span.name == "bae.turn"
                            && turn.span.parent_span_id == request.span.span_id
                            && spans.iter().any(|dispatch| {
                                dispatch.scope == "baesrv"
                                    && dispatch.span.name == "bae.tool.dispatch"
                                    && dispatch.span.parent_span_id == turn.span.span_id
                            })
                    })
            })
        })
        .expect("client send span connected to the server tool turn");
    let client_tool = find_span(&spans, "bae.client", "bae.client.tool");
    assert_eq!(client_tool.span.parent_span_id, send.span.span_id);
    let request = direct_http_child(&spans, &send.span.span_id);
    let turn = direct_child(&spans, &request.span.span_id, "bae.turn");
    let dispatch = direct_child(&spans, &turn.span.span_id, "bae.tool.dispatch");
    assert_eq!(dispatch.span.name, "bae.tool.dispatch");
    let session = find_span(&spans, "baesrv", "bae.session");
    assert_eq!(session.span.trace_id, send.span.trace_id);
    assert!(spans.iter().any(|captured| {
        captured.scope == "baesrv"
            && is_http_request(&captured.span)
            && captured.span.span_id == session.span.parent_span_id
    }));
    assert!(has_session_link(&turn.span, &session.span));
    assert!(spans.iter().any(|captured| {
        captured.scope == "baesrv"
            && captured.span.name == "bae.provider.attempt"
            && captured.span.parent_span_id == turn.span.span_id
    }));

    // Negative case: retain the same receiver, but start a server with the
    // master switch off and send a valid sampled W3C parent manually.  No
    // client SDK is involved here, so *any* received span would be proof that
    // the telemetry-disabled server exported despite the contract.
    receiver.clear();
    let disabled = start_server(&provider_url, None).await;
    let response = Client::new()
        .get(format!("{}/api/v1/meta", disabled.client_url))
        .header(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )
        .send()
        .await
        .expect("disabled server request");
    assert!(response.status().is_success());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        receiver.spans().is_empty(),
        "telemetry-disabled server exported spans after receiving traceparent"
    );
    disabled.stop().await;
}

/// A down/misconfigured collector must never add latency to, or fail, a
/// client-facing request: OTLP export is fire-and-forget behind the batch
/// processor / periodic reader (contract §5; work item's "collector unreachable"
/// edge case). Point `otlp_endpoint` at a reserved-then-released loopback port
/// (nothing listening → every export attempt refused) and drive a full agent
/// round trip, asserting it completes normally and well inside the export
/// timeout envelope.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collector_unreachable_does_not_affect_request_latency_or_success() {
    let _serial = e2e_serial().lock().await;
    let provider_url = start_provider_mock().await;
    // Reserve a loopback port and immediately release it: nothing is listening,
    // so the OTLP/gRPC exporter's connection attempts are refused.
    let dead_collector = unused_loopback_addr();
    let server = start_server(
        &provider_url,
        Some(&format!(
            "[telemetry]\nenabled = true\notlp_endpoint = \"http://{dead_collector}\"\n"
        )),
    )
    .await;
    let client_key = create_client_key(&server).await;

    let tool = Tool::new(
        "get_current_time",
        "Return a deterministic test time",
        json!({"type": "object", "properties": {}}),
        |_input| async move { Ok(json!("2026-07-15T00:00:00Z")) },
    );
    let started = std::time::Instant::now();
    let mut session = Harness::new(ClientConfig::new(server.client_url.clone(), client_key))
        .with_tool(tool)
        .connect()
        .await
        .expect("SDK connect despite unreachable collector");
    let reply = session
        .send("run the canonical tool fixture")
        .await
        .expect("SDK send despite unreachable collector");
    assert_eq!(reply.text(), "tool round-trip complete");
    session
        .close()
        .await
        .expect("SDK close despite unreachable collector");
    let elapsed = started.elapsed();

    // Session authentication deliberately uses expensive password hashing, so
    // the normal five-request round trip already takes several seconds. It
    // nevertheless stays well below one 10-second export timeout *per
    // request*: request-path export would make this six-span flow take tens of
    // seconds rather than its normal single-digit duration.
    assert!(
        elapsed < Duration::from_secs(15),
        "connect/send/close took {elapsed:?} against an unreachable collector — \
         export must stay off the request path"
    );
    server.stop().await;
}
