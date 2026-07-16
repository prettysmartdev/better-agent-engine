//! OpenTelemetry SDK bring-up and the server-side span vocabulary.
//!
//! `baesrv` is the SDK owner (the client SDKs are API-only guests): this module
//! builds the OTLP exporter, tracer provider, sampler, and W3C propagator from
//! the validated `[telemetry]` config, and hands back a `tracing`-layer to
//! compose into the process-wide subscriber in [`crate::cli::init_tracing`]. All
//! spans are authored with the `tracing` crate at the boundaries the code
//! already marks (`api::mod::log_requests`, `sessions::create`,
//! `engine::session::run_turn`, …) and bridged to OTel by the composed
//! `tracing-opentelemetry` layer; the 64 existing `tracing::*!` log call sites
//! become span events automatically, unchanged.
//!
//! **Disabled by default.** When `[telemetry].enabled` (or `traces.enabled`) is
//! false no layer is composed and [`active`] stays `false`, so every span
//! constructor here returns [`tracing::Span::none()`] — no spans are created,
//! logs are byte-for-byte what they were before this work item, and there is no
//! per-call-site `if telemetry` scattered across the request paths. The single
//! flag mirrors layer-composition state; it is not a second policy.
//!
//! The attribute keys, span names, link kinds, and the sampler shape are all
//! fixed by the telemetry contract (WI 0013). Do not invent new ones here.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider as _, NoopMeterProvider};
use opentelemetry::propagation::Extractor;
use opentelemetry::trace::{SpanContext, Status, TraceContextExt, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::Layer;

use crate::config_file::TelemetryConfig;

/// Instrumentation scope name for every server-side tracer/meter. Parity tests
/// compare this literal against the clients' `"bae.client"`.
pub const SCOPE_NAME: &str = "baesrv";

// --- Span attribute keys (contract §1.1) -----------------------------------
pub const ATTR_SESSION_ID: &str = "bae.session.id";
pub const ATTR_PROFILE_ID: &str = "bae.profile.id";
pub const ATTR_CLIENT_KEY_ID: &str = "bae.client_key.id";
pub const ATTR_TURN_OUTCOME: &str = "bae.turn.outcome";
pub const ATTR_TURN_RESUMED: &str = "bae.turn.resumed";
pub const ATTR_PROVIDER_NAME: &str = "bae.provider.name";
pub const ATTR_PROVIDER_KIND: &str = "bae.provider.kind";
pub const ATTR_PROVIDER_MODEL: &str = "bae.provider.model";
pub const ATTR_PROVIDER_ATTEMPT: &str = "bae.provider.attempt";
pub const ATTR_PROVIDER_ATTEMPT_KIND: &str = "bae.provider.attempt_kind";
pub const ATTR_TOOL_NAME: &str = "bae.tool.name";
pub const ATTR_TOOL_DISPATCH: &str = "bae.tool.dispatch";
pub const ATTR_TOOL_IS_ERROR: &str = "bae.tool.is_error";
pub const ATTR_TOOL_INPUT_BYTES: &str = "bae.tool.input.bytes";
pub const ATTR_TOOL_OUTPUT_BYTES: &str = "bae.tool.output.bytes";
pub const ATTR_MCP_SERVER: &str = "bae.mcp.server";
pub const ATTR_SANDBOX_EXIT_CODE: &str = "bae.sandbox.exit_code";
pub const ATTR_SUBAGENT_ID: &str = "bae.subagent.id";
pub const ATTR_SUBAGENT_OUTCOME: &str = "bae.subagent.outcome";
pub const ATTR_HTTP_METHOD: &str = "http.request.method";
pub const ATTR_HTTP_ROUTE: &str = "http.route";
pub const ATTR_URL_PATH: &str = "url.path";
pub const ATTR_HTTP_STATUS: &str = "http.response.status_code";

/// The single Link attribute (contract §0.9); its value is one of the kinds
/// below.
const ATTR_LINK_KIND: &str = "bae.link.kind";
pub const LINK_SESSION: &str = "session";
pub const LINK_RESUME: &str = "resume";
pub const LINK_SUBAGENT: &str = "subagent";

/// Whether the OTel trace layer was actually composed into the subscriber.
/// Read by every span constructor so a disabled server pays nothing and emits
/// no spans. Set once at startup by [`mark_active`].
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// True when trace export is live. Cheap; safe to call on any hot path.
#[inline]
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Record whether the trace layer was composed. Called once from
/// [`crate::cli::init_tracing`] after the subscriber is installed.
pub fn mark_active(value: bool) {
    ACTIVE.store(value, Ordering::Relaxed);
}

/// A boxed `tracing` layer, composed into the registry in `init_tracing`.
pub type BoxTraceLayer = Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync + 'static>;

/// The outputs of [`init`]: the layer to compose (if traces are enabled) and a
/// guard that flushes/shuts the provider down at graceful shutdown.
pub struct Telemetry {
    /// The `tracing-opentelemetry` layer, or `None` when export is off.
    pub layer: Option<BoxTraceLayer>,
    /// Held by `serve` to flush buffered spans within the shutdown window.
    pub guard: TelemetryGuard,
}

/// Owns the SDK providers so `serve` can flush and shut them down inside the
/// `BAE_SHUTDOWN_TIMEOUT` window (contract §5). Cheap/empty when telemetry is
/// disabled.
#[derive(Default)]
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    /// Register metric instruments after `AppState` exists. Observable gauges
    /// retain clones of this state and are invoked by the SDK reader, not by a
    /// BAE-owned interval task.
    pub fn register_metrics(
        &self,
        config: &TelemetryConfig,
        state: crate::api::AppState,
    ) -> Metrics {
        match &self.meter_provider {
            Some(provider) if config.enabled && config.metrics.enabled => {
                Metrics::register(provider.meter(SCOPE_NAME), &config.metrics.disabled, state)
            }
            _ => Metrics::noop(),
        }
    }

    /// Flush and shut both providers down, bounding the **whole** operation
    /// (tracer + meter together) by `timeout` — the caller passes the shutdown
    /// budget still remaining after draining, so total process shutdown stays
    /// within the single `BAE_SHUTDOWN_TIMEOUT`. A no-op when telemetry is
    /// disabled. Errors are logged, never propagated — shutdown must not fail
    /// the process.
    ///
    /// `shutdown_with_timeout` drives a final export of buffered spans/metrics
    /// as part of shutting the batch processor / periodic reader down, so no
    /// separate (unbounded) `force_flush()` is needed — adding one would let the
    /// operation exceed the budget, which is exactly the loss window the work
    /// item requires avoiding.
    pub fn shutdown(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        // `PeriodicReader::shutdown_with_timeout` in the pinned OTel SDK
        // currently ignores its timeout and waits up to its own five-second
        // internal limit. Shut the independent trace and metric providers down
        // concurrently, and wait only until our single shared deadline. This
        // gives both pipelines the full remaining budget to flush while never
        // serialising two potentially-blocking shutdowns into a budget overrun.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut pending = 0;
        if let Some(provider) = &self.tracer_provider {
            let tx = tx.clone();
            let provider = provider.clone();
            pending += 1;
            std::thread::spawn(move || {
                let _ = tx.send(("tracer", provider.shutdown_with_timeout(timeout)));
            });
        }
        if let Some(provider) = &self.meter_provider {
            let tx = tx.clone();
            let provider = provider.clone();
            pending += 1;
            std::thread::spawn(move || {
                let _ = tx.send(("meter", provider.shutdown_with_timeout(timeout)));
            });
        }
        drop(tx);

        for _ in 0..pending {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok((kind, Err(e))) => tracing::warn!("telemetry {kind} shutdown failed: {e}"),
                Ok((_kind, Ok(()))) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        "telemetry shutdown timed out; remaining exports may be dropped"
                    );
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::warn!("telemetry shutdown worker exited unexpectedly");
                    break;
                }
            }
        }
    }
}

/// A failure building the OTel exporter/provider at startup (a usage error,
/// exit code 2 — same posture as an unresolvable provider/MCP secret).
#[derive(Debug)]
pub struct TelemetryInitError(pub String);

impl std::fmt::Display for TelemetryInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "telemetry initialization failed: {}", self.0)
    }
}

impl std::error::Error for TelemetryInitError {}

/// Tracks collector reachability so a sustained export **outage** logs exactly
/// one warning — on the healthy→failing transition — rather than one per failed
/// batch, which would flood the logs under a prolonged outage (contract §5;
/// `docs/reference/05-configuration.md` "Collector unreachable"). Export stays
/// fire-and-forget regardless: this only governs the log line, never the
/// request path. The error text itself is never logged (it could echo config),
/// only the fact of failure.
#[derive(Debug, Default)]
struct ExportHealth {
    /// `true` while the collector is currently considered unreachable.
    failing: AtomicBool,
}

impl ExportHealth {
    /// Record one export result, emitting at most one warning per outage.
    fn record(&self, result: &opentelemetry_sdk::error::OTelSdkResult, kind: &str) {
        match result {
            Ok(()) => {
                // Healthy (or recovered): clear the flag so the NEXT outage
                // warns again — "once per outage", not "once per process".
                self.failing.store(false, Ordering::Relaxed);
            }
            Err(_) => {
                // Warn only on the healthy→failing edge (rate-limited to one
                // line per outage). The error detail is deliberately omitted.
                if !self.failing.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        "telemetry: OTLP {kind} export is failing (collector unreachable or \
                         misconfigured). Export is fire-and-forget and does not affect request \
                         latency or success; this is logged once per outage."
                    );
                }
            }
        }
    }
}

/// Wraps the OTLP span exporter to log one rate-limited warning per collector
/// outage (see [`ExportHealth`]). Delegates every other operation unchanged, so
/// export remains fully asynchronous and off the request path.
#[derive(Debug)]
struct WarnOnFailureSpanExporter<E> {
    inner: E,
    health: Arc<ExportHealth>,
}

impl<E: opentelemetry_sdk::trace::SpanExporter> opentelemetry_sdk::trace::SpanExporter
    for WarnOnFailureSpanExporter<E>
{
    async fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.health.record(&result, "trace");
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

/// Wraps the OTLP metric exporter with the same once-per-outage warning as the
/// span exporter above.
#[derive(Debug)]
struct WarnOnFailureMetricExporter<E> {
    inner: E,
    health: Arc<ExportHealth>,
}

impl<E: opentelemetry_sdk::metrics::exporter::PushMetricExporter>
    opentelemetry_sdk::metrics::exporter::PushMetricExporter for WarnOnFailureMetricExporter<E>
{
    async fn export(
        &self,
        metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        let result = self.inner.export(metrics).await;
        self.health.record(&result, "metric");
        result
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
        self.inner.temporality()
    }
}

/// Build the OTel SDK from `config`. Trace and metric pipelines are independent:
/// disabling traces returns no layer while leaving enabled metrics live.
///
/// **Must be called inside a Tokio runtime**: the OTLP/gRPC (tonic) exporter
/// captures the ambient runtime handle when its channel is created, and the
/// batch processor's background thread drives exports on it. `cli::run_serve`
/// therefore builds this on the same runtime it later serves on.
///
/// `${VAR}` tokens in `otlp_headers` are resolved here (and only here) via the
/// existing provider token helper — a collector bearer token never sits
/// resolved in a config value. An unresolvable token is a startup error.
pub async fn init(config: &TelemetryConfig) -> Result<Telemetry, TelemetryInitError> {
    // Trace and metric exporters talk to the same configured collector. Share
    // one outage state between them so a collector outage produces exactly one
    // warning for the service, rather than one warning per pipeline.
    let export_health = Arc::new(ExportHealth::default());
    let meter_provider = if config.enabled && config.metrics.enabled {
        Some(init_meter_provider(config, export_health.clone())?)
    } else {
        None
    };

    // The master switch, and the trace-specific switch, both gate the tracer.
    // Metrics have their own provider, so disabling traces alone must not
    // disable their collection/export path.
    if !config.enabled || !config.traces.enabled {
        return Ok(Telemetry {
            layer: None,
            guard: TelemetryGuard {
                tracer_provider: None,
                meter_provider: meter_provider.clone(),
            },
        });
    }

    // Validation already guaranteed a non-empty http(s) endpoint when enabled.
    let endpoint = config
        .otlp_endpoint
        .clone()
        .ok_or_else(|| TelemetryInitError("otlp_endpoint missing".to_string()))?;

    // Resolve `${VAR}` tokens in the OTLP headers now, at exporter init — the
    // exact mechanism provider/MCP secrets use. Never store them resolved.
    let metadata = build_metadata(config)?;

    let mut exporter_builder = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT);
    if let Some(metadata) = metadata {
        exporter_builder = exporter_builder.with_metadata(metadata);
    }
    let exporter = exporter_builder
        .build()
        .map_err(|e| TelemetryInitError(format!("OTLP span exporter: {e}")))?;
    // Wrap so a sustained collector outage logs exactly one warning, not one
    // per batch. Export remains fully asynchronous — the batch processor still
    // buffers off the request path.
    let exporter = WarnOnFailureSpanExporter {
        inner: exporter,
        health: export_health,
    };

    let sampler = build_sampler(config.sample_ratio);

    // Batch span processor: fire-and-forget from the request path's view. A
    // down collector buffers/drops asynchronously and never adds latency to or
    // fails a client-facing request.
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource(config))
        .build();

    // Register the W3C propagator globally so `log_requests` extraction and any
    // future injection speak `traceparent`/`tracestate`.
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let tracer = provider.tracer(SCOPE_NAME);
    // Existing tracing events can contain request/provider/tool payloads that
    // are safe for the local event log but forbidden in OTel telemetry. Never
    // copy an event's fields or message into an exported span; all approved
    // telemetry is set explicitly by this module's allowlisted span attributes.
    let layer: BoxTraceLayer = Box::new(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.is_span()
            })),
    );

    tracing::info!(
        endpoint = %config.otlp_endpoint.as_deref().unwrap_or_default(),
        sample_ratio = config.sample_ratio,
        "telemetry: OTLP trace export enabled"
    );

    Ok(Telemetry {
        layer: Some(layer),
        guard: TelemetryGuard {
            tracer_provider: Some(provider),
            meter_provider: meter_provider.clone(),
        },
    })
}

/// Build a metrics pipeline independent of the trace sampler. The SDK's
/// `PeriodicReader` invokes observable callbacks and exports their values on
/// its own interval; there is intentionally no BAE timer loop for metrics.
fn init_meter_provider(
    config: &TelemetryConfig,
    health: Arc<ExportHealth>,
) -> Result<SdkMeterProvider, TelemetryInitError> {
    let endpoint = config
        .otlp_endpoint
        .clone()
        .ok_or_else(|| TelemetryInitError("otlp_endpoint missing".to_string()))?;
    let metadata = build_metadata(config)?;
    let mut exporter_builder = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT);
    if let Some(metadata) = metadata {
        exporter_builder = exporter_builder.with_metadata(metadata);
    }
    let exporter = exporter_builder
        .build()
        .map_err(|e| TelemetryInitError(format!("OTLP metric exporter: {e}")))?;
    // Same once-per-outage warning wrapper as the span path; the PeriodicReader
    // still pulls/exports on its own interval, off the request path.
    let exporter = WarnOnFailureMetricExporter {
        inner: exporter,
        health,
    };
    let reader = PeriodicReader::builder(exporter).build();
    let provider = SdkMeterProvider::builder()
        .with_resource(resource(config))
        .with_reader(reader)
        .build();
    tracing::info!(
        endpoint = %config.otlp_endpoint.as_deref().unwrap_or_default(),
        "telemetry: OTLP metric export enabled"
    );
    Ok(provider)
}

/// The production trace sampler: `ParentBased(TraceIdRatioBased(ratio))`.
///
/// ParentBased means an incoming `traceparent`'s sampled decision is honoured in
/// **both** directions — a client-sampled trace is never un-sampled, and a
/// client-unsampled trace is never spuriously sampled — and the `ratio` applies
/// only when `baesrv` is itself the trace root (no incoming parent). Extracted
/// so tests exercise the exact construction production uses (contract §6).
fn build_sampler(sample_ratio: f64) -> Sampler {
    Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(sample_ratio)))
}

fn resource(config: &TelemetryConfig) -> opentelemetry_sdk::Resource {
    let service_name = config
        .service_name
        .clone()
        .unwrap_or_else(|| SCOPE_NAME.to_string());
    opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new("service.version", crate::VERSION))
        .build()
}

/// Per-export deadline for a single OTLP batch. Short so a hung collector does
/// not tie the batch thread up; failures are dropped (fire-and-forget).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the tonic metadata map from the (token-resolved) OTLP headers, or
/// `None` when there are none. Header names/values are operator config, never
/// telemetry data — but their **values** are secrets and must never become span
/// attributes (they don't: they live only on the exporter).
fn build_metadata(
    config: &TelemetryConfig,
) -> Result<Option<opentelemetry_otlp::tonic_types::metadata::MetadataMap>, TelemetryInitError> {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use opentelemetry_otlp::tonic_types::metadata::MetadataMap;

    let Some(headers) = &config.otlp_headers else {
        return Ok(None);
    };
    if headers.is_empty() {
        return Ok(None);
    }
    // Build a validated `http::HeaderMap` (the crate tonic shares), then convert
    // to the tonic `MetadataMap` the exporter takes — no direct `tonic` dep.
    let mut map = HeaderMap::new();
    for (name, raw_value) in headers {
        let value = crate::engine::provider::resolve_tokens(raw_value)
            .map_err(|e| TelemetryInitError(format!("otlp_headers[{name}]: {e}")))?;
        let name: HeaderName = name
            .parse()
            .map_err(|_| TelemetryInitError(format!("invalid OTLP header name {name:?}")))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|_| TelemetryInitError(format!("invalid OTLP header value for {name:?}")))?;
        map.insert(name, value);
    }
    Ok(Some(MetadataMap::from_headers(map)))
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Metric instrument names are also the exact values accepted by
/// `[telemetry].metrics.disabled` (telemetry contract §3).
pub const METRIC_SESSIONS_OPEN: &str = "bae.sessions.open";
pub const METRIC_SESSIONS_TOTAL: &str = "bae.sessions.total";
pub const METRIC_EVENTS_TOTAL: &str = "bae.events.total";
pub const METRIC_PROFILES_COUNT: &str = "bae.profiles.count";
pub const METRIC_KEYS_COUNT: &str = "bae.keys.count";
pub const METRIC_MCP_SESSIONS_LIVE: &str = "bae.mcp.sessions.live";
pub const METRIC_TURNS_PENDING: &str = "bae.turns.pending";
pub const METRIC_SANDBOXES_LIVE: &str = "bae.sandboxes.live";
pub const METRIC_SUBAGENTS_ACTIVE: &str = "bae.subagents.active";
pub const METRIC_DRIVERS_REGISTERED: &str = "bae.drivers.registered";
pub const METRIC_TURNS_COMPLETED: &str = "bae.turns.completed";
pub const METRIC_PROVIDER_REQUESTS: &str = "bae.provider.requests";
pub const METRIC_PROVIDER_LATENCY: &str = "bae.provider.latency";
pub const METRIC_TOOL_CALLS: &str = "bae.tool.calls";
pub const METRIC_TOOL_LATENCY: &str = "bae.tool.latency";

/// Per-server metric handles. Disabled instruments are no-op handles made at
/// startup, so request-path recording does not re-check telemetry config.
#[derive(Clone)]
pub struct Metrics {
    turns_completed: Counter<u64>,
    provider_requests: Counter<u64>,
    provider_latency: Histogram<f64>,
    tool_calls: Counter<u64>,
    tool_latency: Histogram<f64>,
}

impl Metrics {
    pub fn noop() -> Self {
        let meter = NoopMeterProvider::new().meter(SCOPE_NAME);
        Self {
            turns_completed: meter.u64_counter(METRIC_TURNS_COMPLETED).build(),
            provider_requests: meter.u64_counter(METRIC_PROVIDER_REQUESTS).build(),
            provider_latency: meter.f64_histogram(METRIC_PROVIDER_LATENCY).build(),
            tool_calls: meter.u64_counter(METRIC_TOOL_CALLS).build(),
            tool_latency: meter.f64_histogram(METRIC_TOOL_LATENCY).build(),
        }
    }

    fn register(meter: Meter, disabled: &[String], state: crate::api::AppState) -> Self {
        let disabled: HashSet<&str> = disabled.iter().map(String::as_str).collect();
        let no_op = NoopMeterProvider::new().meter(SCOPE_NAME);
        let instrument_meter = |name: &str| {
            if !disabled.contains(&name) {
                meter.clone()
            } else {
                no_op.clone()
            }
        };

        register_gauges(&meter, &disabled, state);

        Self {
            turns_completed: instrument_meter(METRIC_TURNS_COMPLETED)
                .u64_counter(METRIC_TURNS_COMPLETED)
                .with_unit("1")
                .build(),
            provider_requests: instrument_meter(METRIC_PROVIDER_REQUESTS)
                .u64_counter(METRIC_PROVIDER_REQUESTS)
                .with_unit("1")
                .build(),
            provider_latency: instrument_meter(METRIC_PROVIDER_LATENCY)
                .f64_histogram(METRIC_PROVIDER_LATENCY)
                .with_unit("ms")
                .build(),
            tool_calls: instrument_meter(METRIC_TOOL_CALLS)
                .u64_counter(METRIC_TOOL_CALLS)
                .with_unit("1")
                .build(),
            tool_latency: instrument_meter(METRIC_TOOL_LATENCY)
                .f64_histogram(METRIC_TOOL_LATENCY)
                .with_unit("ms")
                .build(),
        }
    }
}

/// The five SQLite-backed gauges share this cache. OpenTelemetry invokes all
/// observable callbacks synchronously during one collection; the short cache
/// window therefore turns that callback batch into one `activity_counts`
/// query, while avoiding a bespoke polling task.
#[derive(Default)]
struct ActivityCache {
    last: Option<(std::time::Instant, Option<crate::store::ActivityCounts>)>,
}

fn activity_counts(
    state: &crate::api::AppState,
    cache: &Arc<Mutex<ActivityCache>>,
) -> Option<crate::store::ActivityCounts> {
    const CALLBACK_BATCH_WINDOW: Duration = Duration::from_millis(250);
    let mut cache = cache
        .lock()
        .expect("telemetry activity cache mutex poisoned");
    if let Some((at, counts)) = cache.last {
        if at.elapsed() < CALLBACK_BATCH_WINDOW {
            return counts;
        }
    }
    match state.store.with_conn(crate::store::activity_counts) {
        Ok(counts) => {
            cache.last = Some((std::time::Instant::now(), Some(counts)));
            Some(counts)
        }
        Err(e) => {
            // Leave no stale value behind: a failed observation is omitted,
            // never incorrectly reported as zero. The cache timestamp still
            // deduplicates the warning across the five callback invocations.
            cache.last = Some((std::time::Instant::now(), None));
            tracing::warn!("telemetry activity gauges skipped: count query failed: {e}");
            None
        }
    }
}

fn register_gauges(meter: &Meter, disabled: &HashSet<&str>, state: crate::api::AppState) {
    let active = |name: &str| !disabled.contains(name);
    let cache = Arc::new(Mutex::new(ActivityCache::default()));

    macro_rules! activity_gauge {
        ($name:expr, $field:ident) => {
            if active($name) {
                let state = state.clone();
                let cache = cache.clone();
                meter
                    .i64_observable_gauge($name)
                    .with_callback(move |observer| {
                        if let Some(counts) = activity_counts(&state, &cache) {
                            observer.observe(counts.$field, &[]);
                        }
                    })
                    .build();
            }
        };
    }
    activity_gauge!(METRIC_SESSIONS_OPEN, open_sessions);
    activity_gauge!(METRIC_SESSIONS_TOTAL, total_sessions);
    activity_gauge!(METRIC_EVENTS_TOTAL, events);
    activity_gauge!(METRIC_PROFILES_COUNT, profiles);
    activity_gauge!(METRIC_KEYS_COUNT, client_keys);

    macro_rules! state_gauge {
        ($name:expr, |$captured:ident| $value:expr) => {
            if active($name) {
                let state_clone = state.clone();
                meter
                    .u64_observable_gauge($name)
                    .with_callback(move |observer| {
                        let $captured = &state_clone;
                        observer.observe(($value) as u64, &[]);
                    })
                    .build();
            }
        };
    }
    state_gauge!(METRIC_MCP_SESSIONS_LIVE, |state| state
        .mcp_sessions
        .lock()
        .expect("mcp_sessions mutex poisoned")
        .len());
    state_gauge!(METRIC_TURNS_PENDING, |state| state
        .pending_turns
        .lock()
        .expect("pending_turns mutex poisoned")
        .len());
    state_gauge!(METRIC_SANDBOXES_LIVE, |state| state
        .sandboxes
        .lock()
        .expect("sandboxes mutex poisoned")
        .len());
    state_gauge!(METRIC_SUBAGENTS_ACTIVE, |state| state
        .subagents
        .lock()
        .expect("subagents mutex poisoned")
        .values()
        .map(|tasks| tasks
            .values()
            .filter(|task| task.status == crate::engine::subagent::SubagentStatus::Running)
            .count())
        .sum::<usize>());
    state_gauge!(METRIC_DRIVERS_REGISTERED, |state| state
        .drivers
        .lock()
        .expect("drivers mutex poisoned")
        .values()
        .map(std::collections::HashSet::len)
        .sum::<usize>());
}

/// Counter/histogram recorders used at the already-instrumented turn/provider/
/// tool boundaries. Their attribute values are deliberately the contract's
/// small closed sets only; no session, tool, server, or key identifiers enter
/// metric attributes.
impl Metrics {
    pub fn record_turn_completed(&self, outcome: &'static str) {
        self.turns_completed
            .add(1, &[KeyValue::new("outcome", outcome)]);
    }

    pub fn record_provider_attempt(
        &self,
        provider: &'static str,
        outcome: &'static str,
        latency: Duration,
    ) {
        self.provider_requests.add(
            1,
            &[
                KeyValue::new("provider", provider),
                KeyValue::new("outcome", outcome),
            ],
        );
        self.provider_latency.record(
            latency.as_secs_f64() * 1_000.0,
            &[KeyValue::new("provider", provider)],
        );
    }

    pub fn record_tool_call(&self, dispatch: &'static str) {
        self.tool_calls
            .add(1, &[KeyValue::new("dispatch", dispatch)]);
    }

    pub fn record_tool_latency(&self, dispatch: &'static str, latency: Duration) {
        self.tool_latency.record(
            latency.as_secs_f64() * 1_000.0,
            &[KeyValue::new("dispatch", dispatch)],
        );
    }
}

// ---------------------------------------------------------------------------
// Propagation
// ---------------------------------------------------------------------------

/// Extract a W3C parent context from incoming request headers via the global
/// propagator. Absence or a malformed `traceparent` yields an empty (root)
/// context — never an error (contract §6).
pub fn extract_context(headers: &HeaderMap) -> Context {
    let extractor = HeaderExtractor(headers);
    opentelemetry::global::get_text_map_propagator(|p| p.extract(&extractor))
}

/// `Extractor` over axum request headers (avoids an `opentelemetry-http` dep).
struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

// ---------------------------------------------------------------------------
// Span constructors — every one returns Span::none() when telemetry is off.
// ---------------------------------------------------------------------------

/// The top-level `http.request` server span. `route` is axum's `MatchedPath`
/// (the templated route, no embedded ids); `path` is the concrete URL path.
/// The OTel display name is `"{METHOD} {route}"`, or just the method when the
/// route is unavailable — never the raw path (contract §1.1).
pub fn http_request_span(method: &axum::http::Method, route: Option<&str>, path: &str) -> Span {
    if !active() {
        return Span::none();
    }
    let otel_name = match route {
        Some(r) => format!("{method} {r}"),
        None => method.to_string(),
    };
    let span = tracing::info_span!(
        "http.request",
        otel.name = otel_name.as_str(),
        otel.kind = "server",
    );
    span.set_attribute(ATTR_HTTP_METHOD, method.to_string());
    span.set_attribute(ATTR_URL_PATH, path.to_string());
    if let Some(r) = route {
        span.set_attribute(ATTR_HTTP_ROUTE, r.to_string());
    }
    span
}

/// The long-lived `bae.session` anchor span (contract §1.1). Created contextually
/// under the creating request's `http.request` span; stored on `AppState` and
/// dropped at session close/error.
pub fn session_span(session_id: &str, profile_id: &str, client_key_id: &str) -> Span {
    if !active() {
        return Span::none();
    }
    let span = tracing::info_span!("bae.session", otel.kind = "internal");
    span.set_attribute(ATTR_SESSION_ID, session_id.to_string());
    span.set_attribute(ATTR_PROFILE_ID, profile_id.to_string());
    span.set_attribute(ATTR_CLIENT_KEY_ID, client_key_id.to_string());
    span
}

/// One `bae.turn` span. Parented to the current request's `http.request` span
/// (via `parent`, captured before the turn task was spawned), always Linked to
/// the session span, and — on a `PendingTurn` resume — additionally Linked to
/// the paused turn's stored context. Two linked spans across a pause is the
/// deliberate topology (contract §2.2), not one span idle for the timeout.
pub fn turn_span(
    parent: &Context,
    session_ctx: Option<&SpanContext>,
    resume_ctx: Option<&SpanContext>,
    session_id: &str,
    client_key_id: &str,
) -> Span {
    if !active() {
        return Span::none();
    }
    let span = tracing::info_span!("bae.turn", otel.kind = "internal");
    // Parent is the request span, captured across the task spawn — never the
    // session span (see the turn-parent DECISION, contract §1.1).
    let _ = span.set_parent(parent.clone());
    span.set_attribute(ATTR_SESSION_ID, session_id.to_string());
    span.set_attribute(ATTR_CLIENT_KEY_ID, client_key_id.to_string());
    span.set_attribute(ATTR_TURN_RESUMED, resume_ctx.is_some());
    if let Some(ctx) = session_ctx {
        if ctx.is_valid() {
            span.add_link_with_attributes(
                ctx.clone(),
                vec![KeyValue::new(ATTR_LINK_KIND, LINK_SESSION)],
            );
        }
    }
    if let Some(ctx) = resume_ctx {
        if ctx.is_valid() {
            span.add_link_with_attributes(
                ctx.clone(),
                vec![KeyValue::new(ATTR_LINK_KIND, LINK_RESUME)],
            );
        }
    }
    span
}

/// A `bae.provider.attempt` child span (one per fallback-walk iteration).
pub fn provider_attempt_span(
    name: &str,
    kind: &str,
    model: &str,
    attempt: usize,
    attempt_kind: &str,
) -> Span {
    if !active() {
        return Span::none();
    }
    let span = tracing::info_span!("bae.provider.attempt", otel.kind = "client");
    span.set_attribute(ATTR_PROVIDER_NAME, name.to_string());
    span.set_attribute(ATTR_PROVIDER_KIND, kind.to_string());
    span.set_attribute(ATTR_PROVIDER_MODEL, model.to_string());
    span.set_attribute(ATTR_PROVIDER_ATTEMPT, attempt as i64);
    span.set_attribute(ATTR_PROVIDER_ATTEMPT_KIND, attempt_kind.to_string());
    span
}

/// A `bae.tool.dispatch` child span (one per dispatched `tool_use` block).
/// `input_bytes` is the JSON-serialized input size — size metadata only, never
/// the payload (contract §4).
pub fn tool_dispatch_span(name: &str, dispatch: &str, input_bytes: i64) -> Span {
    if !active() {
        return Span::none();
    }
    let span = tracing::info_span!("bae.tool.dispatch", otel.kind = "internal");
    span.set_attribute(ATTR_TOOL_NAME, name.to_string());
    span.set_attribute(ATTR_TOOL_DISPATCH, dispatch.to_string());
    span.set_attribute(ATTR_TOOL_INPUT_BYTES, input_bytes);
    span
}

/// A `bae.mcp.call` grandchild span, parented to its `bae.tool.dispatch` span.
pub fn mcp_call_span(parent: &Span, server: Option<&str>, tool: &str) -> Span {
    if !active() {
        return Span::none();
    }
    let span = tracing::info_span!("bae.mcp.call", otel.kind = "client");
    reparent(&span, parent);
    if let Some(server) = server {
        span.set_attribute(ATTR_MCP_SERVER, server.to_string());
    }
    span.set_attribute(ATTR_TOOL_NAME, tool.to_string());
    span
}

/// A `bae.sandbox.exec` grandchild span, parented to its `bae.tool.dispatch`
/// span.
pub fn sandbox_exec_span(parent: &Span, tool: &str) -> Span {
    if !active() {
        return Span::none();
    }
    let span = tracing::info_span!("bae.sandbox.exec", otel.kind = "internal");
    reparent(&span, parent);
    span.set_attribute(ATTR_TOOL_NAME, tool.to_string());
    span
}

/// The `bae.subagent` span — its **own root trace** (never a child), carrying a
/// Link back to the launching `bae.tool.dispatch` span (contract §2.1). Opened
/// in the detached task; ended when its terminal event fires.
pub fn subagent_span(session_id: &str, subagent_id: &str, link: Option<&SpanContext>) -> Span {
    if !active() {
        return Span::none();
    }
    // `parent: None` forces a root even if some ambient span leaked into the
    // spawned task — a subagent is deliberately not a child of anything.
    let span = tracing::info_span!(parent: None, "bae.subagent", otel.kind = "internal");
    span.set_attribute(ATTR_SESSION_ID, session_id.to_string());
    span.set_attribute(ATTR_SUBAGENT_ID, subagent_id.to_string());
    if let Some(ctx) = link {
        if ctx.is_valid() {
            span.add_link_with_attributes(
                ctx.clone(),
                vec![KeyValue::new(ATTR_LINK_KIND, LINK_SUBAGENT)],
            );
        }
    }
    span
}

/// Subagent outcome attribute values (contract §1.1).
pub const SUBAGENT_OUTCOME_COMPLETED: &str = "completed";
pub const SUBAGENT_OUTCOME_FAILED: &str = "failed";
pub const SUBAGENT_OUTCOME_CANCELLED: &str = "cancelled";

/// Owns the detached `bae.subagent` span for its whole lifetime so its terminal
/// outcome is always recorded — including the cancellation case the raw span
/// cannot cover on its own.
///
/// The subagent span lives inside a `tokio::spawn`ed task. On explicit cancel /
/// session-close teardown that task is `abort()`ed, dropping its future
/// mid-await; a bare span would then end with no `bae.subagent.outcome` and an
/// Unset status, disagreeing with the durable `SubagentCancelled` event
/// (contract §1.1, §2.1). This guard closes that gap: [`finish`] records the
/// real terminal outcome, and if the task is dropped before calling it, `Drop`
/// records `outcome="cancelled"` — so an aborted subagent span always carries
/// the `cancelled` value the contract requires.
///
/// [`finish`]: SubagentSpanGuard::finish
pub struct SubagentSpanGuard {
    span: Span,
    finalized: bool,
}

impl SubagentSpanGuard {
    /// Open the `bae.subagent` root span (own root + subagent Link) and take
    /// ownership of it. A no-op span when telemetry is disabled.
    pub fn new(session_id: &str, subagent_id: &str, link: Option<&SpanContext>) -> Self {
        Self {
            span: subagent_span(session_id, subagent_id, link),
            finalized: false,
        }
    }

    /// Record the natural terminal outcome and end the span. `error` sets the
    /// span status to Error (contract §0.7). After this, `Drop` does nothing.
    pub fn finish(mut self, outcome: &'static str, error: bool) {
        set_str(&self.span, ATTR_SUBAGENT_OUTCOME, outcome);
        if error {
            set_error(&self.span, "subagent failed");
        }
        self.finalized = true;
        // `self` drops here; `finalized` is set, so `Drop` adds nothing.
    }
}

impl Drop for SubagentSpanGuard {
    fn drop(&mut self) {
        if !self.finalized {
            // The launching task was aborted (explicit cancel or session-close
            // teardown) before a terminal outcome was recorded — the durable
            // event is `SubagentCancelled`, so the span must agree.
            set_str(
                &self.span,
                ATTR_SUBAGENT_OUTCOME,
                SUBAGENT_OUTCOME_CANCELLED,
            );
        }
    }
}

/// The live `bae.session` span plus its extracted `SpanContext`, stored on
/// `AppState` for the session's lifetime (contract §1.1). Dropping it ends the
/// span; the retained context is what every `bae.turn` links to.
pub struct SessionSpanHandle {
    /// Held so the span stays open until session close/error teardown.
    span: Span,
    /// The linkable context, snapshotted at creation.
    span_context: SpanContext,
}

impl SessionSpanHandle {
    /// Wrap a freshly-created session span, or `None` when telemetry is off /
    /// the span has no valid context (so nothing is stored and no link target
    /// exists — links are then silently omitted per §2.5).
    pub fn new(span: Span) -> Option<Self> {
        let span_context = span_context(&span)?;
        Some(Self { span, span_context })
    }

    /// The session span's context, for a `bae.turn`'s session Link.
    pub fn context(&self) -> &SpanContext {
        &self.span_context
    }

    /// End the span, marking it Error first when the session ended in error.
    pub fn end(self, error: Option<&str>) {
        if let Some(msg) = error {
            set_error(&self.span, msg.to_string());
        }
        drop(self.span);
    }
}

// ---------------------------------------------------------------------------
// Late-attribute / status / link setters (all no-ops on Span::none()).
// ---------------------------------------------------------------------------

/// Set the child span's OTel parent explicitly to `parent`'s context. Used
/// where the contextual parent (the turn span) is not the desired one — e.g. a
/// grandchild `bae.mcp.call` whose parent is its `bae.tool.dispatch` span.
fn reparent(child: &Span, parent: &Span) {
    let cx = parent.context();
    if cx.span().span_context().is_valid() {
        let _ = child.set_parent(cx);
    }
}

/// The current span's OTel context — captured in a request handler (where the
/// `http.request` span is active) and threaded into a spawned turn task so the
/// turn span can be parented back to the request. Empty when telemetry is off.
pub fn current_context() -> Context {
    if !active() {
        return Context::new();
    }
    Span::current().context()
}

/// Set a live span's OTel parent to `cx` (e.g. the extracted request context,
/// or a captured span context threaded across a task spawn). No-op when off.
pub fn set_parent(span: &Span, cx: Context) {
    if active() {
        let _ = span.set_parent(cx);
    }
}

/// The remote-linkable `SpanContext` of a live span, or `None` when telemetry
/// is off or the span has no valid context (contract §2.5 — omit links
/// silently rather than synthesize).
pub fn span_context(span: &Span) -> Option<SpanContext> {
    if !active() {
        return None;
    }
    let ctx = span.context().span().span_context().clone();
    ctx.is_valid().then_some(ctx)
}

/// Record a string attribute on a live span (no-op when off).
pub fn set_str(span: &Span, key: &'static str, value: impl Into<String>) {
    if active() {
        span.set_attribute(key, value.into());
    }
}

/// Record an i64 attribute on a live span.
pub fn set_i64(span: &Span, key: &'static str, value: i64) {
    if active() {
        span.set_attribute(key, value);
    }
}

/// Record a bool attribute on a live span.
pub fn set_bool(span: &Span, key: &'static str, value: bool) {
    if active() {
        span.set_attribute(key, value);
    }
}

/// Set span status to Error with a message (contract §0.7). No-op when off.
pub fn set_error(span: &Span, message: impl Into<String>) {
    if active() {
        span.set_status(Status::error(message.into()));
    }
}

/// Add a Link to `ctx` with the single `bae.link.kind` attribute. No-op when
/// off or when `ctx` is invalid.
pub fn add_link(span: &Span, ctx: &SpanContext, kind: &'static str) {
    if active() && ctx.is_valid() {
        span.add_link_with_attributes(ctx.clone(), vec![KeyValue::new(ATTR_LINK_KIND, kind)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

    fn metric_names(exporter: &InMemoryMetricExporter) -> std::collections::HashSet<String> {
        exporter
            .get_finished_metrics()
            .expect("read in-memory metrics")
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .map(|metric| metric.name().to_string())
            .collect()
    }

    #[test]
    fn disabled_metric_is_not_registered_but_other_instruments_still_export() {
        let store = crate::store::Store::open_in_memory().expect("in-memory store");
        let state = crate::api::AppState::new(store);
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = Metrics::register(
            provider.meter(SCOPE_NAME),
            &[METRIC_TOOL_CALLS.to_string()],
            state,
        );

        metrics.record_turn_completed("completed");
        metrics.record_tool_call("client");
        provider.force_flush().expect("flush metrics");
        let names = metric_names(&exporter);

        assert!(names.contains(METRIC_TURNS_COMPLETED));
        assert!(names.contains(METRIC_SESSIONS_OPEN));
        assert!(!names.contains(METRIC_TOOL_CALLS));
    }

    #[test]
    fn registered_gauges_include_the_activity_counts_catalogue() {
        let store = crate::store::Store::open_in_memory().expect("in-memory store");
        let state = crate::api::AppState::new(store);
        // Keep the store query itself as the oracle: callbacks must observe
        // the exact ActivityCounts fields rather than an independent cache.
        let expected = state
            .store
            .with_conn(crate::store::activity_counts)
            .expect("activity counts");
        assert_eq!(expected.open_sessions, 0);
        assert_eq!(expected.total_sessions, 0);

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let _metrics = Metrics::register(provider.meter(SCOPE_NAME), &[], state);
        provider.force_flush().expect("flush gauges");
        let names = metric_names(&exporter);

        for name in [
            METRIC_SESSIONS_OPEN,
            METRIC_SESSIONS_TOTAL,
            METRIC_EVENTS_TOTAL,
            METRIC_PROFILES_COUNT,
            METRIC_KEYS_COUNT,
            METRIC_MCP_SESSIONS_LIVE,
            METRIC_TURNS_PENDING,
            METRIC_SANDBOXES_LIVE,
            METRIC_SUBAGENTS_ACTIVE,
            METRIC_DRIVERS_REGISTERED,
        ] {
            assert!(names.contains(name), "missing observable gauge {name}");
        }
    }

    /// Read a single observable gauge's exported value (handling both the i64
    /// `ActivityCounts` gauges and the u64 `AppState`-map gauges).
    fn gauge_value(exporter: &InMemoryMetricExporter, name: &str) -> Option<i64> {
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        let metrics = exporter.get_finished_metrics().expect("read metrics");
        for rm in &metrics {
            for scope in rm.scope_metrics() {
                for m in scope.metrics() {
                    if m.name() == name {
                        return match m.data() {
                            AggregatedMetrics::I64(MetricData::Gauge(g)) => {
                                g.data_points().next().map(|dp| dp.value())
                            }
                            AggregatedMetrics::U64(MetricData::Gauge(g)) => {
                                g.data_points().next().map(|dp| dp.value() as i64)
                            }
                            _ => None,
                        };
                    }
                }
            }
        }
        None
    }

    /// The value-transition test the metric coverage was missing (Finding 15):
    /// gauges must report the *actual* state — a created session, a parked
    /// `PendingTurn`, a registered driver — not merely exist by name. The
    /// `ActivityCounts` query is reused directly as the oracle, since the metric
    /// literally re-runs it (contract §3 / work item test considerations).
    #[tokio::test]
    async fn observable_gauges_report_live_state_values() {
        let store = crate::store::Store::open_in_memory().expect("in-memory store");
        // Seed the profile/key parents the session foreign keys require, then
        // create one OPEN session — driving sessions.open/total, keys, profiles.
        store
            .with_conn(|c| {
                c.execute_batch(
                    "INSERT INTO profiles (id, name) VALUES ('pro_1', 'p');\n\
                     INSERT INTO keys (id, role) VALUES ('key_a', 'client');",
                )?;
                crate::store::sessions::create_session(
                    c,
                    "key_a",
                    "pro_1",
                    crate::store::sessions::STATE_OPEN,
                    Some("1.0.0"),
                    &serde_json::json!([]),
                    &serde_json::json!([]),
                    &serde_json::json!([]),
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .expect("seed session state");
        let state = crate::api::AppState::new(store);

        // Park a PendingTurn (drives turns.pending) — its gate guard is a real
        // owned mutex guard, as in production.
        let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let guard = gate
            .clone()
            .try_lock_owned()
            .expect("acquire test turn-gate guard");
        state.pending_turns.lock().unwrap().insert(
            "ses_pending".to_string(),
            crate::api::PendingTurn {
                owner_client_key_id: "key_a".to_string(),
                guard,
                deadline: tokio::time::Instant::now() + Duration::from_secs(60),
                server_tool_results: Vec::new(),
                span_context: None,
            },
        );

        // Register two drivers on one session (drives drivers.registered = 2).
        state.drivers.lock().unwrap().insert(
            "ses_driven".to_string(),
            ["drv_1".to_string(), "drv_2".to_string()]
                .into_iter()
                .collect(),
        );

        // The oracle: the exact query the ActivityCounts gauges re-run.
        let counts = state
            .store
            .with_conn(crate::store::activity_counts)
            .expect("activity counts oracle");
        assert_eq!(counts.open_sessions, 1);
        assert_eq!(counts.total_sessions, 1);

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let _metrics = Metrics::register(provider.meter(SCOPE_NAME), &[], state.clone());
        // One collection after all state mutations (a single flush avoids the
        // gauges' short activity-count cache window returning a stale zero).
        provider.force_flush().expect("flush gauges");

        assert_eq!(
            gauge_value(&exporter, METRIC_SESSIONS_OPEN),
            Some(counts.open_sessions),
            "bae.sessions.open must match the ActivityCounts oracle"
        );
        assert_eq!(
            gauge_value(&exporter, METRIC_SESSIONS_TOTAL),
            Some(counts.total_sessions)
        );
        assert_eq!(
            gauge_value(&exporter, METRIC_PROFILES_COUNT),
            Some(counts.profiles)
        );
        assert_eq!(
            gauge_value(&exporter, METRIC_KEYS_COUNT),
            Some(counts.client_keys)
        );
        assert_eq!(
            gauge_value(&exporter, METRIC_TURNS_PENDING),
            Some(1),
            "bae.turns.pending must reflect the parked PendingTurn"
        );
        assert_eq!(
            gauge_value(&exporter, METRIC_DRIVERS_REGISTERED),
            Some(2),
            "bae.drivers.registered must sum per-session driver set sizes"
        );
    }

    /// Serialize every exported metric — names, data-point values, AND every
    /// attribute key/value — into one string, for the secrets regression below.
    fn metric_export_debug(exporter: &InMemoryMetricExporter) -> String {
        format!(
            "{:#?}",
            exporter
                .get_finished_metrics()
                .expect("read in-memory metrics")
        )
    }

    /// Metric-side companion to `integration.rs`'s span secrets regression
    /// (Finding 17): metrics must NEVER carry a high-cardinality or secret
    /// value. Gauges export counts only (never the map keys/values they read),
    /// and counters/histograms are labelled only with the closed enum sets — so
    /// even when the `AppState` maps hold secret-shaped ids, none reaches the
    /// exported metric payload (contract §3.3, §4).
    #[tokio::test]
    async fn metrics_never_export_secret_or_high_cardinality_values() {
        // Secret-shaped identifiers seeded into the state maps the gauges read.
        const SECRET_SESSION: &str = "bae_ses_wi0013-metric-secret-session";
        const SECRET_DRIVER: &str = "wi0013-metric-secret-driver-id";
        const SECRET_TOOL: &str = "wi0013-metric-secret-tool-name";

        let store = crate::store::Store::open_in_memory().expect("in-memory store");
        let state = crate::api::AppState::new(store);
        // A driver registered under a secret-shaped session id with a
        // secret-shaped driver id: the gauge must export only the COUNT (2 here
        // via two drivers), never these strings.
        state.drivers.lock().unwrap().insert(
            SECRET_SESSION.to_string(),
            [SECRET_DRIVER.to_string(), format!("{SECRET_DRIVER}-2")]
                .into_iter()
                .collect(),
        );

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let metrics = Metrics::register(provider.meter(SCOPE_NAME), &[], state);
        // Exercise every counter/histogram with their closed-set labels — a
        // secret tool name is deliberately NOT passable (the API takes only the
        // `dispatch` enum), which is itself the cardinality guarantee.
        metrics.record_turn_completed("completed");
        metrics.record_provider_attempt("anthropic", "ok", Duration::from_millis(5));
        metrics.record_tool_call("mcp");
        metrics.record_tool_latency("mcp", Duration::from_millis(3));
        provider.force_flush().expect("flush metrics");

        let payload = metric_export_debug(&exporter);
        for secret in [SECRET_SESSION, SECRET_DRIVER, SECRET_TOOL] {
            assert!(
                !payload.contains(secret),
                "a secret/high-cardinality value reached the metric export: {secret:?}"
            );
        }
        // Sanity: the driver gauge still exported (as a count), proving the
        // absence above is real coverage, not an empty payload.
        assert_eq!(gauge_value(&exporter, METRIC_DRIVERS_REGISTERED), Some(2));
    }

    /// ParentBased(TraceIdRatioBased) must honour an incoming client sampling
    /// decision in BOTH directions, and apply its own ratio only as the root
    /// (Finding 14; contract §6 / edge case). Exercises the exact
    /// [`build_sampler`] production uses.
    #[test]
    fn parent_based_sampler_respects_incoming_decision_both_directions() {
        use opentelemetry::trace::{
            SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId, TraceState,
        };
        use opentelemetry_sdk::trace::{SamplingDecision, ShouldSample};

        fn remote_parent(sampled: bool) -> Context {
            let flags = if sampled {
                TraceFlags::SAMPLED
            } else {
                TraceFlags::default()
            };
            let sc = SpanContext::new(
                TraceId::from_bytes([1u8; 16]),
                SpanId::from_bytes([1u8; 8]),
                flags,
                true, // remote
                TraceState::default(),
            );
            Context::new().with_remote_span_context(sc)
        }

        fn decide(sampler: &Sampler, parent: Option<&Context>) -> SamplingDecision {
            sampler
                .should_sample(
                    parent,
                    TraceId::from_bytes([2u8; 16]),
                    "http.request",
                    &SpanKind::Server,
                    &[],
                    &[],
                )
                .decision
        }

        // Direction 1: a client-SAMPLED parent is respected even when the root
        // ratio is 0.0 — the server never un-samples a client-sampled trace.
        assert_eq!(
            decide(&build_sampler(0.0), Some(&remote_parent(true))),
            SamplingDecision::RecordAndSample,
        );
        // Direction 2: a client-UNSAMPLED parent is respected even when the root
        // ratio is 1.0 — the server never spuriously samples a fragment.
        assert_eq!(
            decide(&build_sampler(1.0), Some(&remote_parent(false))),
            SamplingDecision::Drop,
        );
        // As the root (no incoming parent) the ratio applies: 1.0 samples...
        assert_eq!(
            decide(&build_sampler(1.0), None),
            SamplingDecision::RecordAndSample,
        );
        // ...and 0.0 drops.
        assert_eq!(decide(&build_sampler(0.0), None), SamplingDecision::Drop);
    }
}
