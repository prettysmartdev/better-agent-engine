# Work Item: Feature

Title: opentelemetry metrics and traces
Issue: issuelink

## Summary:
- This work item aims to add OpenTelemetry metrics and traces to all of BAE.

both baesrv and all 3 bae client harness SDKs will gain OpenTelemetry traces that are nearly invisible to the user. All actions within baesrv and the client harness SDKs will gain instrumentation via OpenTelemetry, and baesrv will natively export to a configured OpenTelemetry collector.

The clients will create parent spans for each and every request, and baesrv will link them to session-level spans as well.

baesrv will maintain metrics for things like number of open sessions, number of connected clients, number of server events per hour, and anything else that is relevant to platform operators

OpenTelemetry exporting will be configured via bae-config.toml and is disabled by default unless enabled from the config file.

The client harnesses and baesrv must collaborate to fully instrument agents automatically for "magic" telemetry for operators

In addition, users can optionally add additional use-case specific traces within the implementations of their lifecycle hooks or custom tools they attach to their harness implementations using simple language-natural methods that the design agent can determine.

Knobs like sampling, enabling/disabling specific metrics, etc. should all be possible via the config file.

## User Stories

### User Story 1:
As a: Platform Operator

I want to:
turn on OpenTelemetry export in `bae-config.toml` (an OTLP endpoint, a sampling
ratio, which metrics I care about) and point it at the collector my team
already runs, with zero code changes and zero effect on `baesrv` if I leave it
unset

So I can:
see session/turn/tool traces and platform-health metrics (open sessions,
connected clients, events/hour) in the observability stack I already operate,
the same way I'd instrument any other service, without `baesrv` phoning home
anywhere by default or paying any tracing overhead until I opt in

### User Story 2:
As an: Agent Developer

I want to:
have my client harness's per-request span automatically become the parent of
(or linked to) `baesrv`'s session- and turn-level spans for that same request,
with no manual trace-context plumbing on my part

So I can:
open my observability backend and see one connected trace per user
interaction — client work, the HTTP round trip, and everything `baesrv` did
server-side (provider calls, tool dispatch, MCP calls) — instead of two
disjoint trace islands I'd have to correlate by hand

### User Story 3:
As an: Agent Developer building a custom tool or lifecycle hook

I want to:
add my own OpenTelemetry span inside my `before_tool_call`/`after_tool_call`
hook or my tool's handler function, using the same OTel API calls I'd use in
any other instrumented code in my language

So I can:
capture use-case-specific detail (e.g. "time spent validating this tool's
input" or "rows scanned by this database call") that nests correctly under
BAE's own automatic turn/tool spans, without learning a BAE-specific tracing
API or wiring anything extra into my harness setup

## Implementation Details:

This is a four-part change spanning `baesrv` and all three client SDKs: (A) a
new `[telemetry]` `bae-config.toml` section and server-side OTel SDK wiring;
(B) a server-side span hierarchy covering session/turn/tool/provider work; (C)
server-side metrics sourced from state that already exists (`ActivityCounts`,
`AppState`'s in-memory maps); (D) client-side spans plus W3C `traceparent`
propagation so client and server spans join into one trace. A fifth piece (E)
covers the "user adds their own span" story, which turns out to need no new
BAE API at all — see below.

**Design decision: server config vs. client config are two different
mechanisms, deliberately.** `baesrv` is a standalone deployed binary with
nothing to inherit tracing setup from, so it needs its own explicit,
config-file-driven OTel SDK bring-up (`[telemetry]` in `bae-config.toml`,
disabled unless present and `enabled = true`, per the summary). The three
client SDKs, by contrast, are libraries embedded in someone else's
application — the "language-natural method" the summary asks for is for each
harness to call its language's OpenTelemetry **API** package (not the SDK)
against whatever **global/ambient `TracerProvider`** the embedding
application has installed. If the embedding app never installed an OTel SDK
(the common case), every API call the harness makes resolves to that
language's built-in no-op tracer — genuinely zero-cost and "disabled by
default" with no BAE-specific client config surface required at all. This is
also exactly the mechanism that makes User Story 3 free: a user's own span
calls inside a hook/tool closure use the *same* ambient API, so they
automatically nest under whatever the harness's own spans left active on the
context stack — no BAE-specific span API to learn.

### A. `[telemetry]` config section (server)

- Add `pub telemetry: Option<TelemetryConfig>` to `BaeConfig`
  (`server/src/config_file.rs:71-79`), following the exact pattern the module
  doc comment already anticipates (`config_file.rs:14-16`: *"further sections
  ... can be added without restructuring"*). Unknown/absent section →
  `None` → telemetry fully off, matching `[mcp]`/`[providers]`'s existing
  "absent is valid, not an error" convention (`config_file.rs:68-70`).
- `TelemetryConfig` fields, `#[serde(deny_unknown_fields)]` like `McpConfig`/
  `ProvidersConfig` (`config_file.rs:82-88`, `118-124`):
  - `enabled: bool` (default `false` — the master switch; every other field
    is inert unless this is `true`).
  - `otlp_endpoint: String` (e.g. `http://otel-collector:4317`; required if
    `enabled = true`, validated at load time — empty/malformed URL is a
    `ConfigFileError`-shaped usage error, exit code 2, mirroring the
    existing validation posture at `config_file.rs:170-174`).
  - `otlp_headers: Option<HashMap<String, String>>` — extra headers (e.g. an
    auth token for a hosted collector) sent with every OTLP export. Values
    follow the file's existing `${ENV_VAR}` convention **unresolved at parse
    time**, resolved only at exporter-init time via
    `crate::engine::provider::resolve_tokens`/`resolve_tokens_with`
    (`config_file.rs:48-54`) — the exact mechanism `ProviderConfig.auth_token`
    and `McpServerConfig.headers` already use, so a collector bearer token
    never sits resolved in a `BaeConfig` value any longer than a provider
    or MCP secret does.
  - `sample_ratio: f64` (default `1.0`, range `[0.0, 1.0]`) — fed into a
    `ParentBased(TraceIdRatioBased)` sampler: the server *respects* the
    incoming `traceparent`'s sampled flag when a client already made a
    sampling decision (User Story 2's joined trace must not be sampled
    inconsistently), and only applies its own ratio when it is itself the
    root (a request with no incoming `traceparent`, or `[telemetry]` enabled
    server-side while the client isn't sending one at all).
  - `service_name: Option<String>` (default `"baesrv"`) — the OTel
    `service.name` resource attribute; lets an operator running multiple
    `baesrv` instances distinguish them in their backend.
  - `traces: TracesConfig { enabled: bool (default true) }` and
    `metrics: MetricsConfig { enabled: bool (default true), disabled: Vec<String> (default empty) }`
    — `metrics.disabled` names specific metric instruments to suppress
    (e.g. `["bae.sandbox.events"]`) without turning off traces or the rest
    of the metrics, satisfying the summary's "enabling/disabling specific
    metrics" knob without needing a metric-by-metric boolean field each.
- Loading/validation slots into the existing `load_registries_from` path
  (`cli.rs:395-424`) alongside `mcp_registry()`/`provider_registry()` — add a
  sibling `telemetry_config()` accessor on `BaeConfigFile` that validates and
  returns `TelemetryConfig::default()` (i.e., `enabled: false`) when the
  section is absent, so callers never have to match on `Option` themselves.
- Round-trip/unit tests follow the exact shape of the existing
  `mcp_registry`/`provider_registry` tests (`config_file.rs:364-692`).

### B. Server-side span hierarchy

No span infrastructure exists today (`grep` for `tracing::span`/`Span::`/
`.instrument(`/`#[tracing::instrument]` across `server/src` returns nothing —
confirmed greenfield); this is a new hierarchy layered onto existing code
paths, not a retrofit of anything that already exists. Proposed nesting,
mapped onto the code that already marks each of these boundaries today:

1. **HTTP request span** — opened in `log_requests`
   (`server/src/api/mod.rs:40-55`), the one middleware **both** routers
   already share (`client/mod.rs:42`, `admin/mod.rs:82`). This is also where
   an incoming `traceparent` header is extracted (via the `opentelemetry`
   crate's `TextMapPropagator`) to seed the span's parent context — the join
   point for User Story 2.
2. **Session span** — opened when a session is created
   (`api::client::sessions::create`, around `sessions.rs:293`, which already
   logs `EventType::SessionOpen` at `sessions.rs:383`) and closed on
   `session.close`/`session.error`. This is the span a *joined* driver
   (WI 0005 multi-driver sessions) attaches to as a **Link**, not a
   reparent — the session was opened by whichever driver's request created
   it first; a second driver's `session.driver.register` request gets its
   own request span, linked to the session span, not nested under it.
3. **Turn span** — one per `session::run_turn` invocation
   (`server/src/engine/session.rs:147-765`), child of the session span. Its
   own doc comment (`session.rs:1-41`) already describes the 6-step
   lifecycle this hierarchy mirrors.
4. **Provider attempt spans** — a child span per fallback-walk attempt
   within a turn (`session.rs:301-359`), wrapping the `provider.request`/
   `provider.response` pair already logged there, so a turn that falls back
   across providers shows each attempt distinctly rather than one flat span.
5. **Tool dispatch spans** — a child span per tool call
   (`session.rs:406-742`), tagged with the same `dispatch` value
   (`"client"`/`"sandbox"`/`"mcp"`/`"subagent"` — four buckets as of the
   WI 0009/0010 partition, `session.rs:406-421`) already computed for the
   `tool.call` event, plus grandchild spans for `mcp.request`/`mcp.response`
   (`session.rs:633-694`) and `sandbox.request`/`sandbox.response`
   (`session.rs:481-593`) where applicable.
6. **Remote subagent spans — deliberately NOT a normal child.** Subagents
   are fire-and-forget (`launch_remote_subagent`, `session.rs:824-1090`,
   `tokio::spawn`ed at `session.rs:961`) and their terminal events
   (`SubagentCompleted`/`SubagentFailed`) fire from a detached background
   task (`session.rs:1054-1063`) well after the launching turn's span has
   already ended. A subagent span must be its **own root span**, associated
   with the launching turn via an OTel **Link** (carrying `session_id`/
   `subagent_id` attributes), never a parent/child edge — a child span
   outliving its parent is invalid in OTel's model and would show up as a
   broken/orphaned span in most backends.
7. **Paused/resumed turns (WI 0009's `PendingTurn`) — trace continuity
   across two HTTP requests.** A mixed client+server turn pauses
   mid-`run_turn` and resumes on a *separate* HTTP request when the client
   sends its tool results back (`api/client/rpc.rs::drive_send_message`,
   `PendingTurn` machinery, `rpc.rs:398-450`/`619-624`). The turn span
   opened in step 3 cannot literally stay "open" across two HTTP requests
   (no in-process task is alive holding it). Store the turn span's own
   `SpanContext` (trace-id + span-id, W3C-serializable) in `PendingTurn`
   alongside its existing guard/owner/deadline fields; on resume, open the
   **continuation** as a new span linked back to (not reparented under) the
   stored context. Document this explicitly as a deliberate trace-topology
   choice, not a bug: a paused turn shows as two linked spans, not one
   span mysteriously idle for up to `BAE_TURN_TIMEOUT` seconds.

Implementation mechanics: compose a `tracing-opentelemetry` `Layer` into the
existing `tracing_subscriber::registry()` set up in `cli.rs::init_tracing`
(`cli.rs:455-463`), side-by-side with the current `fmt()` layer (registry
supports multiple layers) — this means the 64 existing `tracing::info!`/
`warn!`/etc. call sites across `server/src` automatically become span events
under whichever span is active when they fire, with **no changes needed to
any of those call sites**. New spans are opened with `tracing::info_span!`/
`#[tracing::instrument]` at the boundaries listed above (1-6), guarded by a
no-op layer when `[telemetry].enabled` is `false` so the "disabled by
default" contract is enforced at the layer-composition level, not scattered
across `if telemetry_enabled` checks at every call site.

### C. Server-side metrics

Metrics ride on the same OTel SDK bring-up as traces (`opentelemetry_sdk`'s
`MeterProvider`, exported via OTLP on the interval implied by the SDK's
`PeriodicReader`, independent of the trace `sample_ratio` knob — metrics are
not sampled).

- **Gauges from existing state, via `ObservableGauge` callbacks registered
  once at startup** (not a periodic push loop — the SDK's own reader pulls
  on its export interval):
  - `bae.sessions.open`, `bae.sessions.total`, `bae.events.total`,
    `bae.profiles.count`, `bae.keys.count` — straight from `ActivityCounts`
    (`store/mod.rs:143-177`), the same query `log_activity_summary`
    (`lib.rs:239-258`) already runs hourly today; the callback just runs
    that query on the SDK's own schedule instead of a bespoke timer.
  - `bae.mcp.sessions.live` — `state.mcp_sessions.lock().len()`
    (`lib.rs:241-245`, already computed for the summary log line).
  - `bae.turns.pending` — `AppState.pending_turns` length (`api/mod.rs:155`)
    — a genuinely new signal (paused turns awaiting client resume), not
    previously surfaced anywhere.
  - `bae.sandboxes.live`, `bae.subagents.active`, `bae.drivers.registered`
    — from `AppState.sandboxes`/`subagents`/`drivers`
    (`api/mod.rs:164`/`168`/`146`), same `.lock().len()` pattern.
- **Counters/histograms from the span-boundary instrumentation in (B)**:
  `bae.turns.completed` (counter, labeled `outcome` = completed/paused/
  provider_failed, matching `Outcome`'s three variants, `session.rs:63-72`),
  `bae.provider.requests` (counter, labeled `provider`/`outcome`),
  `bae.provider.latency` (histogram), `bae.tool.calls` (counter, labeled
  `dispatch`), `bae.tool.latency` (histogram, labeled `dispatch`).
- **Cardinality discipline**: no metric attribute may be a high-cardinality
  value — `session_id`, `client_key_id`, tool names, and MCP server names
  are fine as **span** attributes (traces are already per-request) but must
  **never** be metric labels, since every distinct label value is a
  permanent new time series in most backends. Metric labels are limited to
  the small closed sets already named above (`outcome`, `dispatch`,
  `provider` kind).
- `[telemetry].metrics.disabled` (from A) is enforced by simply not
  registering the named instrument at startup — cheapest possible
  implementation of a per-metric off switch, no runtime branch per
  data point.

### D. Client SDK spans and wire-level trace propagation

All three SDKs get the identical treatment (parity is a hard project
invariant — see Codebase Integration):

- **New dependency: the OTel *API* package only, not the SDK** —
  `opentelemetry` (Rust, API surface only — `opentelemetry_sdk` stays a
  dev-dependency for the harness's own tests, never a runtime dep of the
  library), `@opentelemetry/api` (TypeScript — this is client-typescript's
  **first ever runtime dependency**, a deliberate break from its documented
  zero-runtime-deps design principle (`aspec/architecture/design.md:70`);
  the API package has no exporter/SDK weight and is a no-op without a host
  app's SDK installed, which is why it's the one exception worth making —
  call this out explicitly in the PR/docs as an intentional, scoped
  exception), `opentelemetry-api` (Python).
- **Harness-level automatic spans**, opened in the one choke point each SDK
  already has for its request/response loop: `run_loop`
  (`client-rust/src/harness.rs:96-182`), the equivalent inline loop in
  `Session.send()` (TypeScript `harness.ts`, Python `core.py`) — one span
  per `session.sendMessage` round trip (**not** one per logical "turn" the
  way the server nests it, since from the client's side a paused/resumed
  turn genuinely is two separate round trips; do not try to fake continuity
  the client can't see), child spans per tool dispatch
  (`harness.rs:147-176` and TS/Python equivalents), tagged with the tool's
  `dispatch` value so a client-side trace view also shows "these blocks
  were mine to run, these were informational" per WI 0009's routing
  contract.
- **`traceparent` header injection** at each SDK's single outbound-request
  choke point: `HttpTransport::open_rpc` (`client-rust/src/harness.rs:
  209-224`, right next to the existing `.bearer_auth(&session_key)` call),
  `FetchTransport.request()`/`.stream()` (`client-typescript/src/
  transport.ts:36-69`/`76-128`, the inline `headers` object), and
  `HttpxTransport.request()`/`.stream()` (`client-python/src/bae_py/
  harness/transport.py:66-84`/`86-121`, the `headers` parameter). Use the
  OTel API's own `TextMapPropagator`/`inject` call in each language so the
  header value always matches whatever propagation format
  (`traceparent`/`tracestate`) the ambient SDK is configured for, rather
  than BAE hand-rolling W3C Trace Context serialization three times.
- Session-open and session-close calls get the same header injection (they
  go through the same transport choke points), so the whole session
  lifecycle — not just turns — is traceable end to end.
- Document the header on the wire in `docs/reference/wire-protocol.md`
  (parallel to how WI 0009 documented the `dispatch` field there): every
  client request **may** carry `traceparent` (and `tracestate`); the server
  **must** accept its absence gracefully (older clients, or a client with no
  OTel SDK installed) by simply starting a new root trace, never erroring.

### E. User-added custom spans — no new BAE API needed

This is the direct answer to the summary's "simple language-natural methods
that the design agent can determine": **there isn't a new BAE method** — a
user's tool handler or hook closure (`Tool::new(...)`'s handler,
`Hooks::before_tool_call`/`after_tool_call`/etc. — `client-rust/src/
hooks.rs:38-88`, `client-typescript/src/hooks.ts`, `client-python/src/
bae_py/hooks.py`, all confirmed identical five-hook shape across the three
SDKs) is just user code running while one of (D)'s spans is the
currently-active span on that language's ambient context. The user calls
their own language's standard OTel API (`tracing::info_span!(...)` in Rust,
`trace.getTracer(...).startActiveSpan(...)` in TypeScript,
`tracer.start_as_current_span(...)` in Python) from inside their closure,
and it nests automatically — no BAE-specific span-creation call, no hook
argument threading a "current span" object around. The one thing the
implementation must get right for this to actually work: the active span
context must survive whatever async boundary sits between "the harness
enters its span" and "the user's closure body runs" (Rust: `tracing`'s
task-local span stack across the `.await` inside `run_loop`'s tool-dispatch
call; TypeScript: `@opentelemetry/api`'s context propagation relies on
`AsyncLocalStorage`, which must actually be wired up as the active
`ContextManager`; Python: `contextvars`-based propagation across `await`).
Verify this explicitly per SDK during implementation — it is the one place
"nesting is automatic" could silently fail.

## Edge Case Considerations:

- **Secrets must never become span/metric attributes.** Session keys,
  client keys, the admin key, and resolved provider/MCP `${VAR}` secrets are
  already documented as never logged (`aspec/architecture/security.md:11-12`,
  and the explicit "auth token never reaches this module" comment at
  `session.rs:40-41`). Extend that same posture to spans: maintain an
  explicit denylist/allowlist of what may become a span attribute (session
  *id* yes, session *key* never; tool *name* yes, tool *input* only if it
  can't carry provider-forwarded secrets — treat tool input as untrusted the
  same way request bodies are) and add a regression test asserting none of
  the fixture secrets used in telemetry integration tests appear in exported
  span/metric payloads.
- **Collector unreachable or misconfigured.** OTLP export must be
  fire-and-forget from the request path's perspective — a batch span
  processor buffers and exports asynchronously; a down/unreachable collector
  must never add latency to or fail a client-facing request. Log the export
  failure once (not per-span, which would flood logs under a sustained
  outage) via the existing `tracing::warn!` machinery.
- **Graceful shutdown must flush buffered spans/metrics.** `baesrv` already
  has a documented shutdown budget (`BAE_SHUTDOWN_TIMEOUT`, default 30s,
  `docs/reference/configuration.md`); the OTel SDK's batch processor
  `shutdown()`/`force_flush()` must run within that same window before
  process exit, or the last spans of a session are silently lost on every
  restart/deploy.
- **`sample_ratio` vs. an incoming client `traceparent`'s sampled flag.**
  Covered in (A)/(B) — `ParentBased` sampling means the server never
  "un-samples" a trace the client already decided to sample, and vice versa
  a client that sampled out should not have the server spuriously start
  sampling a fragment of it. Test both directions explicitly.
- **`[telemetry]` changes require a restart.** Like every other
  `bae-config.toml` section, config is loaded once at `run_serve` startup
  (`cli.rs:395-424`) — there is no hot-reload anywhere in the codebase
  today, so this is consistent, not a new limitation, but state it plainly
  in the docs since "flip a knob for sampling" language in the summary could
  otherwise imply live reconfiguration.
- **Multi-driver sessions (WI 0005) and the session span.** Only the driver
  whose request created the session owns the session span as its logical
  root; a second/third driver joining the same session
  (`session.driver.register`) gets its own request span **linked** to (not
  reparented under) the session span — otherwise a long-lived session with
  many drivers joining over time would produce an ever-growing single span
  tree that never closes cleanly.
- **Old client talking to a telemetry-enabled server, or a telemetry-capable
  client talking to an old/telemetry-disabled server.** Both directions must
  degrade silently: no `traceparent` header present → server starts a fresh
  root trace (never errors on the missing header); server has `[telemetry]`
  disabled → it simply never opens spans/exports, regardless of what headers
  arrive (no partial/broken traces from a server that acknowledges the
  header but doesn't actually instrument anything).
- **Subagent spans surviving their launching turn.** Already covered in (B)
  as a Link-not-child design; call out explicitly in tests (see Test
  Considerations) since this is the one place a naive
  "just nest everything" implementation would produce structurally invalid
  spans.
- **client-typescript's zero-runtime-dependency precedent.** This work item
  is the first to add a runtime dependency to that package. Document the
  exception explicitly (API-only package, no-op by default, matches the
  "invisible unless configured" goal) rather than silently breaking the
  precedent — future work items should not read this as license to add
  further runtime deps without the same scrutiny.
- **High-cardinality metric labels.** Explicitly forbidden per (C) —
  `session_id`/tool names/MCP server names are span attributes only, never
  metric labels. Add this as a lint/review checklist item, not just prose,
  since it's an easy mistake to reintroduce later when a new metric is
  added.
- **Tool/hook input containing user- or provider-forwarded data becoming a
  span attribute.** Treat tool `input`/`output` payloads the same as any
  other request body — do not blanket-attach them to spans by default (an
  operator can already see them via the existing `tool.call`/`tool.result`
  event log if `[telemetry]` is off or they need the full payload); at most
  attach size/shape metadata (byte length, block count) automatically, and
  document that full-payload span attributes are exactly the kind of
  "additional use-case specific trace" a user adds themselves per (E) if
  they've judged it safe for their own tool's data.

## Test Considerations:

- **Server unit — `[telemetry]` config round-trip and validation**, mirroring
  the existing `mcp_registry`/`provider_registry` test shape
  (`config_file.rs:364-692`): section absent → `enabled: false`, no error;
  `enabled = true` with a missing/malformed `otlp_endpoint` → usage error,
  exit code 2; `sample_ratio` outside `[0.0, 1.0]` → usage error;
  `otlp_headers` values keep their `${VAR}` tokens unresolved after parse
  (resolution only happens at exporter-init, per A).
- **Server integration — span tree shape**, extending `integration.rs`'s
  existing `log_capture`/`CaptureWriter` pattern (`integration.rs:2622-2649`,
  today used to assert specific log lines) with an in-memory OTel span
  exporter test double: drive a full turn through the real router and assert
  the resulting span tree matches (B)'s hierarchy — session → turn →
  provider-attempt → tool-dispatch, each tagged with the expected
  `dispatch` attribute for a mixed MCP+client turn (reusing WI 0009's own
  `mixed_mcp_client_turn_pauses_with_server_result_and_annotated_message`
  scenario, `integration.rs:1561`, as the driving fixture).
- **Server integration — subagent span topology.** Launch a remote subagent,
  assert its span is a distinct root carrying a Link back to the launching
  turn's span context, and specifically assert it is **not** a child of a
  span that has already closed (the structurally-invalid case called out in
  Edge Cases).
- **Server integration — paused/resumed turn continuity.** Drive a
  mixed-turn pause-then-resume (two separate HTTP requests, per WI 0009's
  `PendingTurn` flow) and assert the resume's span is linked to the paused
  turn's stored `SpanContext`, not silently disconnected or double-parented.
- **Server integration — secrets never exported.** For a fixture using a
  real session key, client key, and a `${VAR}`-resolved provider token,
  assert none of those literal values appear anywhere in the captured
  span/metric export payloads.
- **Server integration — collector-down does not affect request latency or
  success.** Point `otlp_endpoint` at a closed port; assert client-facing
  requests still complete normally and within their usual latency envelope.
- **Server integration — graceful shutdown flush.** Assert buffered spans
  are exported (to a live in-test collector double) before process exit
  within `BAE_SHUTDOWN_TIMEOUT`.
- **Server unit — metrics reflect `ActivityCounts`/`AppState` state.**
  Create/close sessions, open/park a paused turn, register a driver; assert
  the corresponding `ObservableGauge` callbacks report the expected values
  at each point (reusing `ActivityCounts`' existing query directly as the
  assertion oracle, since the metric literally re-runs it).
- **Server unit — `metrics.disabled` suppresses only the named
  instrument(s).** No other metric or any trace is affected.
- **Client unit (all three SDKs, kept identical per the parity invariant)**:
  with no OTel SDK installed by the embedding app, assert **zero**
  measurable overhead signal (no `traceparent` header sent, since the
  no-op propagator injects nothing) — this is the regression guard for
  "disabled by default." With a test-double OTel SDK installed, assert
  `traceparent` is present on every outbound request (session open, RPC
  turn call, session close) and that a tool-dispatch child span is created
  only for `dispatch:client` blocks, matching WI 0009's existing
  informational-vs-executed split.
- **Client parity test**, extending the existing
  `client-python/tests/test_*_parity.py` pattern (and its Rust/TS
  equivalents) with an OTel case: given the same mock-transport-driven
  scenario, all three SDKs produce the same span names/attribute keys (not
  necessarily identical span *ids*, but identical shape) — the concrete
  regression guard for the "three SDKs behave identically" invariant
  extended to this new surface.
- **Client unit — ambient context survives the async boundary into hook/tool
  closures**, per (E)'s explicit risk call-out: a hook closure that starts
  its own child span asserts (via a test-double SDK) that its span's parent
  is the harness's own currently-active span, in each of the three
  languages' async models.
- **End-to-end — real OTLP collector receiver in CI**, mirroring how
  `integration.rs` already spins up a mock LLM provider server
  (`start_mock()`, `integration.rs:442`): enable `[telemetry]` against a
  lightweight mock OTLP receiver, drive one of the SDK example agents
  (`issue-triage`, already used as the cross-SDK parity fixture per
  `aspec/genai/agents.md`) end to end, and assert the receiver observes a
  single connected trace spanning the client's spans and the server's
  session/turn/tool spans, joined via `traceparent`.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- `server/src/config_file.rs`: add `TelemetryConfig` following the exact
  `McpConfig`/`ProvidersConfig` shape (`deny_unknown_fields`, `${VAR}` tokens
  preserved unresolved through parsing, resolved via the existing
  `resolve_tokens`/`resolve_tokens_with` helpers at use time,
  `config_file.rs:48-54`). Do not invent a second `${VAR}` resolution
  mechanism.
- `server/src/cli.rs::init_tracing` (`cli.rs:455-463`): compose a
  `tracing-opentelemetry` `Layer` into the existing
  `tracing_subscriber::registry()` alongside the current `fmt()` layer —
  this is a layer-composition change, not a rewrite of the 64 existing log
  call sites across `server/src`, all of which keep working unchanged as
  span events.
- `server/src/lib.rs`'s `summary_task`/`log_activity_summary` pattern
  (`lib.rs:155-162`, `239-258`) is the template for where periodic-metrics
  registration happens at startup, but prefer `ObservableGauge` callbacks
  (pull-based, on the SDK's own export interval) over adding a second
  bespoke timer loop alongside the existing hourly summary log.
- `server/src/api/mod.rs::log_requests` (`api/mod.rs:40-55`) is the single
  shared middleware both routers already layer
  (`client/mod.rs:42`/`admin/mod.rs:82`) — the natural, and only necessary,
  place to extract an incoming `traceparent` and open the top-level HTTP
  request span for both `/api/v1` and `/admin/v1` traffic.
- `server/src/engine/session.rs`: instrument `run_turn` and its tool-dispatch
  partition (`session.rs:147-765`, `406-742`) directly — per WI 0009's own
  stated convention ("extend the closed `EventType` set only if a genuinely
  new event is needed"), this work needs **no new `EventType` variants**
  (`server/src/events.rs`); tracing is a parallel concern layered onto the
  same code paths that already emit `tool.call`/`mcp.request`/etc., not a
  new event category recorded in the SQLite event log.
- New server deps (`server/Cargo.toml`): `tracing-opentelemetry`,
  `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` — mirror the
  existing pattern of also adding `tracing`/`tracing-subscriber` as
  dev-dependencies (`Cargo.toml:50-55`, there specifically so integration
  tests can capture output) with an equivalent in-memory span/metric
  exporter test double for the new integration tests above.
- Client deps: `opentelemetry` (Rust, API-surface features only — verify the
  crate's feature flags let a consumer depend on the API without pulling in
  SDK/exporter code), `@opentelemetry/api` (TypeScript — first runtime
  dependency for this package, an explicit, documented exception to
  `aspec/architecture/design.md:70`'s zero-runtime-deps principle),
  `opentelemetry-api` (Python).
- `docs/reference/configuration.md`: new `## [telemetry]` section following
  the exact template the `[mcp]`/`[providers]` sections already establish
  (top-level layout example, field reference table, startup validation
  errors list — `configuration.md:86-…`).
- `docs/reference/wire-protocol.md`: document the `traceparent`/`tracestate`
  header convention on every client request, parallel to how WI 0009
  documented the `dispatch` field there — including the explicit "absence
  is always valid, never an error" contract.
- `aspec/architecture/design.md`: short scope notes on Components 1-4
  (`design.md:52-78` — baesrv, client-rust, client-typescript,
  client-python) mentioning OTel instrumentation as part of each component's
  scope, following the exact pattern WI 0012 used to update Component 5's
  entry. Component 6 (max, `design.md:89-96`): explicitly note that
  visualizing trace/metric data is **out of scope** for this work item (MAX
  remains an event-log/session dashboard) unless the user wants that folded
  in — flag rather than silently deciding either way.
- `aspec/architecture/security.md`: extend the existing "never logged"
  guarantees for the admin key and provider/MCP secrets
  (`security.md:11-12`) to explicitly cover span and metric attributes, not
  just log lines.
- `aspec/genai/agents.md`: note the OTel span-shape parity requirement
  alongside the existing "identical behavior across Rust, TypeScript, and
  Python" language, since this work item adds a new dimension (span
  names/attributes) that parity now covers.
- Preserve the "three SDKs behave identically" invariant end to end — same
  hook semantics (E), same span-shape (D), same reliance on each language's
  own standard OTel env-var auto-configuration rather than a BAE-specific
  client config surface, so operators configuring client-side export use
  the OTel ecosystem's own familiar mechanism in whichever language they're
  in.
- No new `Makefile` verb — plug new tests into the existing per-component
  `make test-server`/`make test-client-rust`/`make test-client-typescript`/
  `make test-client-python` targets (`Makefile:203-210`).
