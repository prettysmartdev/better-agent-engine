//! Wire-level W3C trace-context propagation and the harness's two OTel spans
//! (`bae.client.send`, `bae.client.tool`) — see the telemetry contract §1.2,
//! §6, §7.
//!
//! This module is the client's entire OpenTelemetry surface. It depends only
//! on the `opentelemetry` **API** crate (never `opentelemetry_sdk` at
//! runtime — that stays a dev-dependency for this crate's own tests) and
//! never installs, configures, or mutates any global OTel state itself: no
//! global `TracerProvider`, no global propagator, no context-manager
//! registration. Every call here resolves against whatever the *embedding
//! application* has installed. With no OTel SDK installed by that
//! application, `global::tracer(..)` returns the crate's built-in no-op
//! tracer and `global::get_text_map_propagator` returns the built-in no-op
//! propagator, so every span/injection call in this module costs nothing and
//! writes nothing to the wire — the "disabled by default, zero overhead"
//! contract.

use opentelemetry::propagation::Injector;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{global, Context, InstrumentationScope, KeyValue};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Tracer scope name — identical across all three SDKs (telemetry contract
/// §0.2); parity tests compare this literal string.
const SCOPE: &str = "bae.client";

/// Scope version — the SDK package version, matching TypeScript/Python so the
/// instrumentation scope is identical across SDKs (telemetry contract §0.2).
const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The wire allowlist: the only headers BAE ever injects for trace context
/// (telemetry contract §6 — W3C Trace Context only, no baggage). A custom
/// ambient propagator that emits anything else (baggage, tenant ids, tokens)
/// must not have those values leak onto BAE requests.
const ALLOWED_PROPAGATION_HEADERS: [&str; 2] = ["traceparent", "tracestate"];

/// Build the `bae.client` tracer carrying the SDK package version as its scope
/// version. Resolves against the embedding app's global provider (no-op if
/// none installed).
fn tracer() -> opentelemetry::global::BoxedTracer {
    let scope = InstrumentationScope::builder(SCOPE)
        .with_version(SCOPE_VERSION)
        .build();
    global::tracer_with_scope(scope)
}

/// Open the `bae.client.send` span for one `run_loop` iteration (one
/// `session.sendMessage` round trip) and return the [`Context`] with it
/// attached as current. Its parent is whatever the embedding application's
/// ambient context is (a no-op/root context if none).
///
/// Await every downstream future for this iteration — the transport call,
/// the tool-dispatch futures — under the returned context via
/// [`opentelemetry::trace::FutureExt::with_context`]; the OTel `Context` does
/// not survive `.await` on its own in Rust.
pub(crate) fn start_send_span(session_id: &str, iteration: u64) -> Context {
    let tracer = tracer();
    let span = tracer
        .span_builder("bae.client.send")
        .with_kind(SpanKind::Client)
        .with_attributes([
            KeyValue::new("bae.session.id", session_id.to_string()),
            KeyValue::new("bae.rpc.method", "session.sendMessage"),
            KeyValue::new("bae.client.iteration", iteration as i64),
        ])
        .start(&tracer);
    Context::current_with_span(span)
}

/// Open the `bae.client.tool` span for one client-owned tool dispatch
/// (`before_tool_call` hook -> handler -> `after_tool_call` hook), as a child
/// of `send_cx`'s span. Await the dispatch future under the returned context.
pub(crate) fn start_tool_span(send_cx: &Context, tool_name: &str) -> Context {
    let tracer = tracer();
    let span = tracer
        .span_builder("bae.client.tool")
        .with_kind(SpanKind::Internal)
        .with_attributes([
            KeyValue::new("bae.tool.name", tool_name.to_string()),
            KeyValue::new("bae.tool.dispatch", "client"),
        ])
        .start_with_context(&tracer, send_cx);
    send_cx.with_span(span)
}

/// Mark `cx`'s span failed with a fixed, category-only status message and end
/// it. Called on transport/RPC failure for `bae.client.send` and on
/// hook/handler failure for `bae.client.tool`.
///
/// The raw error text is deliberately **not** exported (no `record_error`, no
/// error string in the status description): a transport/hook/handler error can
/// carry provider-forwarded secrets, tokens, or prompt fragments, and the
/// telemetry contract (§4) forbids any untrusted payload text — including error
/// text — anywhere in telemetry. Full error detail remains available to the
/// embedding application through the returned `Result`.
pub(crate) fn fail_span(cx: &Context, category: &'static str) {
    let span = cx.span();
    span.set_status(Status::error(category));
    span.end();
}

/// End the span carried by `cx` (the non-error path).
pub(crate) fn end_span(cx: &Context) {
    cx.span().end();
}

/// A `reqwest`-header [`Injector`] restricted to the W3C Trace Context wire
/// allowlist (`traceparent`/`tracestate`). Any other key an ambient
/// propagator tries to write — baggage most notably, but also any custom
/// header a non-standard global propagator emits — is dropped, so no baggage
/// value (token, tenant id, prompt fragment) ever leaks onto a BAE request
/// (telemetry contract §6).
struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if !ALLOWED_PROPAGATION_HEADERS
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
        {
            return;
        }
        if let (Ok(name), Ok(val)) = (HeaderName::try_from(key), HeaderValue::try_from(value)) {
            self.0.insert(name, val);
        }
    }
}

/// Inject the current ambient OTel context into `builder`'s headers using the
/// embedding application's global `TextMapPropagator` — never a hand-rolled
/// W3C serializer. This is the single choke point every outbound request in
/// this crate routes through (`HttpTransport::open_rpc`, the session-open
/// POST, and the session-close DELETE), so `session.sendMessage`, every other
/// `/rpc` method, and session open/close all carry the same propagation.
///
/// With no OTel SDK installed by the embedding app, the global propagator is
/// a no-op and this writes no headers at all — the "zero overhead when
/// disabled" contract (assert the header's absence, not just span absence).
pub(crate) fn inject_traceparent(builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let mut headers = HeaderMap::new();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&Context::current(), &mut HeaderInjector(&mut headers));
    });
    if headers.is_empty() {
        builder
    } else {
        builder.headers(headers)
    }
}
