# Work Item: Feature

Title: configuration tab in MAX
Issue: issuelink

## Summary:
- Add a new tab in the MAX webapp which is a READ-ONLY view on the configuration that was passed via the bae-config.yaml file at server startup.

The tab should be a single page with a distinct section for each of the top-level fields in the config file showing what is configured. It should be a nice, rich, easily readable layout. Ensure all secrets are not actually returned from the server and are shown only as dots in the webapp.

This should be powered by an endpoint on the admin server (if one doesn't exist already).

The tab order should be Sessions, Profiles, Keys, Config

**Naming note:** the file-driven config that actually exists on `main` is `bae-config.toml` (TOML, not YAML) — see `server/src/config_file.rs` and `docs/reference/configuration.md`'s "`bae-config.toml` schema" section. This work item targets that file. Its top-level layout (`BaeConfig`, `config_file.rs:71-82`) now has **three** sections — `[mcp]`, `[providers]`, and `[telemetry]` (the last added by work item 0013, OpenTelemetry metrics and traces) — so "a distinct section for each of the top-level fields" means three sections: `MCP Servers`, `Providers`, and `Telemetry`, with room in the layout for a further section if a future top-level key (e.g. `[logging]`/`[limits]`, called out as examples in `config_file.rs:16`) is added later. This is deliberately scoped to the **file**-driven config only — the separate env-driven `crate::config::Config` (`BAE_ADDR`, `BAE_DB_PATH`, `BAE_SANDBOX_DRIVER`, etc., `server/src/config.rs`) is out of scope, since the summary specifically names the config **file** and `AppState` does not currently hold that struct in a form an admin endpoint could read back out (see Edge Case Considerations).

**Post-WI-0013 note:** `[telemetry]` (`TelemetryConfig`, `config_file.rs:86-124`) is genuine file-driven config, so it is in scope for this tab exactly like `[mcp]`/`[providers]`. It carries one secret-bearing field — `otlp_headers` (a collector auth token, e.g. `{ Authorization = "Bearer ${OTEL_TOKEN}" }`) — which must be redacted with the same dots convention as MCP `headers`/provider `auth_token`; every other telemetry field (`enabled`, `otlp_endpoint`, `sample_ratio`, `service_name`, `traces.enabled`, `metrics.enabled`, `metrics.disabled`) is non-secret and shown in full. The one wrinkle versus `[mcp]`/`[providers]`: `AppState` does **not** currently retain the parsed `TelemetryConfig` — WI 0013 consumes it at startup into opaque metric/trace handles (`server/src/lib.rs:127,148`; `AppState.telemetry_metrics`/`session_spans`, `api/mod.rs:219`/`218`) and drops the raw config. Surfacing telemetry therefore requires **one small, additive new `AppState` field** holding the parsed config (see A/Codebase Integration), unlike the MCP/provider sections which reuse existing state verbatim. Note that WI 0013 kept *visualizing trace/metric data* out of MAX (`aspec/architecture/design.md`, MAX "remains an event-log/session dashboard") — showing the telemetry **configuration** here is a different, compatible concern and does not reopen that decision.

**Endpoint note:** two read-only admin endpoints already exist that touch this data — `GET /admin/v1/mcp-servers` and `GET /admin/v1/providers` (`server/src/api/admin/mcp.rs`, `admin/providers.rs`) — but both are deliberately **minimal**: they omit `command`/`args`/`url`/`headers` and `auth_token` entirely rather than masking them (`docs/reference/admin-api.md:395-396,431-432`). The summary asks for secrets to be "shown only as dots," which requires the field to be *present* (so the UI has something to render as dots) rather than absent — omission and masking are different contracts, and changing the existing endpoints' shape would risk the `ProfilesTab` pickers that already depend on their current minimal shape (`max/web/src/tabs/ProfilesTab.tsx:5-19`, `max/server/src/adminClient.ts:182-188`). So this work item adds a **new, additive** `GET /admin/v1/config` endpoint rather than modifying the existing two. There is likewise **no** existing admin endpoint that exposes `[telemetry]` — WI 0013 added the config section and server wiring but no read-back endpoint — so the new `/admin/v1/config` is the sole surface for all three sections.

## User Stories

### User Story 1:
As a: Platform Operator

I want to:
open a single "Config" tab in MAX and see every MCP server, LLM provider, and the telemetry export settings `bae-config.toml` configured this server with — transport, command/args or URL, model, effective base URL, OTLP endpoint/sampling — laid out in clearly labeled sections

So I can:
confirm what a running server actually has available without `docker exec`-ing in to read the file on disk or cross-referencing two separate list-only admin endpoints by hand

### User Story 2:
As a: Platform Operator

I want to:
see that any secret-bearing value (MCP server `headers`, provider `auth_token`, telemetry `otlp_headers`) is rendered as a fixed dot placeholder — never the literal token, resolved or unresolved — even though I can see which fields *have* a secret configured

So I can:
verify a server's configuration at a glance from a browser (potentially off-loopback, per MAX's own auth model in `aspec/work-items/0007-max-webapp.md` section D) without that browser ever holding, transmitting a second time, or being able to leak the real secret value

### User Story 3:
As an: Agent Developer troubleshooting a profile

I want to:
cross-check a profile's `primary_provider`/`mcp_servers` names against the full Config tab in one glance, right next to the Profiles tab

So I can:
catch a typo'd or removed registry name without leaving the dashboard, using the same tab bar that already covers Sessions/Profiles/Keys

## Implementation Details:

This is a small, mostly additive feature: (A) one new read-only admin-port endpoint (plus one new `AppState` field so the telemetry config is readable back); (B) a redaction convention for the three known secret-bearing fields (MCP `headers`, provider `auth_token`, telemetry `otlp_headers`); (C) a thin MAX-server proxy route; (D) the new frontend tab; (E) docs.

### A. New admin endpoint: `GET /admin/v1/config`

- New module `server/src/api/admin/config.rs`, mirroring the doc-comment style and no-secrets framing of `admin/mcp.rs`/`admin/providers.rs`.
- Reads from the same `AppState` fields the MCP/provider handlers already read — `state.mcp_registry: Arc<HashMap<String, McpServerConfig>>` and `state.provider_registry: Arc<HashMap<String, ProviderConfig>>` (`server/src/api/mod.rs:172,178`) — plus **one new `AppState` field** for telemetry (see below). No data is newly *loaded*; the MCP/provider registries and the telemetry config are all already parsed from `bae-config.toml` at startup, this endpoint just exposes a richer, redacted view over them.
- **New `AppState.telemetry_config: Arc<TelemetryConfig>` field.** Unlike `mcp_registry`/`provider_registry`, the parsed `TelemetryConfig` is not currently retained on `AppState` — `serve()` receives it as an argument (`server/src/lib.rs:127`) and consumes it into telemetry handles (`lib.rs:148`, `register_metrics(&telemetry_config, …)`) without storing the raw config. Add `pub telemetry_config: Arc<TelemetryConfig>` to `AppState` (`api/mod.rs`), default `Arc::new(TelemetryConfig::default())` in `AppState::new`/`with_registries` (matching how `mcp_registry`/`provider_registry` default to empty), and set it in `serve()` right where the other startup-derived fields are assigned (`lib.rs:139-143`, alongside `state.turn_timeout = …`) — clone it there since `register_metrics` at `lib.rs:148` still needs `&telemetry_config`. This is the one new piece of server state this work item adds; everything else reuses existing state verbatim.
- Response shape (unpaginated, like `mcp-servers`/`providers` today — there is no cursor convention here since the whole registry is already bounded and held in memory):
  ```json
  {
    "mcp": {
      "servers": [
        {
          "name": "filesystem",
          "transport": "stdio",
          "command": "npx",
          "args": ["-y", "@modelcontextprotocol/server-filesystem", "/data"],
          "url": null,
          "headers": {}
        },
        {
          "name": "search",
          "transport": "sse",
          "command": null,
          "args": [],
          "url": "https://mcp.example.com/sse",
          "headers": { "Authorization": "••••••••" }
        }
      ]
    },
    "providers": {
      "entries": [
        {
          "name": "anthropic-sonnet",
          "provider": "anthropic",
          "model": "claude-sonnet-4-6",
          "base_url": "https://api.anthropic.com",
          "auth_token": "••••••••"
        }
      ]
    },
    "telemetry": {
      "enabled": true,
      "otlp_endpoint": "http://otel-collector:4317",
      "otlp_headers": { "Authorization": "••••••••" },
      "sample_ratio": 1.0,
      "service_name": "baesrv",
      "traces": { "enabled": true },
      "metrics": { "enabled": true, "disabled": ["bae.events.total"] }
    }
  }
  ```
  - `mcp.servers[].command`/`args`/`url` are exposed **in full** — not secret, and today omitted by `/admin/v1/mcp-servers` only for brevity, not for safety (`docs/reference/admin-api.md:395`). This is the "richer" part of the summary's "nice, rich, easily readable layout."
  - `mcp.servers[].headers` keeps every header **key** but replaces every value with a fixed redaction marker (see B) — so an operator can see *that* `Authorization` is set without ever seeing its value.
  - `providers.entries[].auth_token` is always present and always the redaction marker (`ProviderConfig.auth_token` is a required, non-optional `String`, `engine/provider.rs:99` — never absent for a defined entry).
  - `providers.entries[].base_url` is the **effective** value (`ProviderConfig::effective_base_url()`), matching `/admin/v1/providers`' existing convention (`admin/providers.rs:34`).
  - `telemetry` mirrors the `TelemetryConfig` shape (`config_file.rs:86-124`) with exactly one redaction: `telemetry.otlp_headers` keeps every header **key** but replaces every value with the redaction marker, identical to MCP `headers` handling (an OTLP collector bearer token is exactly as secret as an MCP one — see B). Every other telemetry field is non-secret and exposed in full: `enabled`, `otlp_endpoint` (the collector URL is not a secret; the token in `otlp_headers` is), `sample_ratio`, `service_name` (the effective name — emit `"baesrv"` when `service_name` is `None`, matching how `base_url` emits the *effective* value rather than the raw `Option`), `traces.enabled`, `metrics.enabled`, `metrics.disabled`. `otlp_headers` is emitted as `{}` when absent (`None`), the same as an MCP server with no headers.
  - Both `servers` and `entries` sorted by `name` for stable output, matching every existing admin list handler's convention (`mcp.rs:27`, `providers.rs:26`, `sandbox.rs:55`). `telemetry` is a single object, not a list, so no sort applies; serialize its `otlp_headers`/`metrics.disabled` in a stable order (e.g. sorted keys / preserved input order) so the response body is deterministic for the snapshot tests in Test Considerations.
  - A missing config file, or a file with no `[mcp]`/`[providers]`/`[telemetry]` table, yields `{"mcp": {"servers": []}, "providers": {"entries": []}, "telemetry": {"enabled": false, …}}` — never an error, matching `mcp_registry`/`provider_registry`/`telemetry_config`'s own "absent → default" contract (`config_file.rs:427,475,504`; `telemetry_config()` returns `TelemetryConfig::default()`, i.e. `enabled: false`, when the section is absent). An absent `[telemetry]` therefore renders as a present-but-disabled Telemetry section, not an empty/missing one.
- Register the route in `server/src/api/admin/mod.rs`'s router (alongside the other `get(...)` routes, `admin/mod.rs:52-70`) and add `config` to that file's top doc-comment endpoint list (`admin/mod.rs:14-24`). It picks up the same `require_admin_auth` layer as every other admin route automatically, since that layer wraps the whole router (`admin/mod.rs:74-79`) — no new auth wiring needed, but see Test Considerations for a regression check that this is actually true for the new route.

### B. Redaction convention

- A single `pub const REDACTED: &str = "••••••••"` in `admin/config.rs`, used for **every** secret-bearing value — MCP `headers` values, provider `auth_token`, and telemetry `otlp_headers` values — regardless of whether it happens to look like an unresolved `${ENV_VAR}` token or a literal secret typed directly into the TOML. This matters because `state.mcp_registry`/`state.provider_registry`/`state.telemetry_config` all hold the **raw, unresolved** config exactly as parsed (`config_file.rs`'s "Secrets" doc section: `${ENV_VAR}` tokens are "not resolved here" — resolution happens later, at connect/call/export time, via `resolve_tokens`; `TelemetryConfig.otlp_headers` follows the identical unresolved-at-parse contract per WI 0013's design) — an operator is free to skip the `${...}` indirection entirely and write a literal secret straight into `headers`/`auth_token`/`otlp_headers`, and the masking must not special-case on string shape (e.g. "only mask things matching `${...}`") or it would leak literal secrets verbatim. Always redact the whole field unconditionally; never attempt partial masking (e.g. show a prefix) that could leak length or a recognizable substring. `otlp_headers` is masked value-by-value exactly like MCP `headers`: keys preserved, every value replaced with `REDACTED`.
- A fixed-length marker (not one dot per character of the real value) is deliberate: it avoids leaking the secret's length as a side channel.

### C. MAX-server proxy route

- `max/server/src/adminClient.ts`: add `getConfig(): Promise<unknown>` calling `GET /admin/v1/config`, mirroring `listProviders()`/`listMcpServers()` (`adminClient.ts:182-188`) exactly — no new method category needed, just one more read-only passthrough.
- `max/server/src/routes.ts`: add `router.get("/config", proxy(() => admin.getConfig()))` next to the existing `/providers`/`/mcp-servers` routes (`routes.ts:93-101`), reusing the same `proxy()` helper (`routes.ts:136-159`) that already translates `AdminApiError`s to the upstream status/body.
- No new auth wiring here either — the whole `/api` router already requires a valid MAX session cookie past the login/`/session` routes (`routes.ts:44-45`), and this new route sits below that line like everything else.

### D. New frontend tab

- New `max/web/src/tabs/ConfigTab.tsx`, the simplest tab in the app — **no** create/edit/delete affordances, purely a fetch-and-render page, unlike `ProfilesTab`/`KeysTab`.
- `max/web/src/api/types.ts`: add
  ```ts
  export interface McpServerConfigView {
    name: string;
    transport: "stdio" | "sse" | "http";
    command: string | null;
    args: string[];
    url: string | null;
    headers: Record<string, string>; // values are always the redaction marker
  }
  export interface ProviderConfigView {
    name: string;
    provider: string;
    model: string;
    base_url: string;
    auth_token: string; // always the redaction marker
  }
  export interface TelemetryConfigView {
    enabled: boolean;
    otlp_endpoint: string | null;
    otlp_headers: Record<string, string>; // values are always the redaction marker
    sample_ratio: number;
    service_name: string; // effective name ("baesrv" when unset)
    traces: { enabled: boolean };
    metrics: { enabled: boolean; disabled: string[] };
  }
  export interface ConfigResponse {
    mcp: { servers: McpServerConfigView[] };
    providers: { entries: ProviderConfigView[] };
    telemetry: TelemetryConfigView;
  }
  ```
- `max/web/src/api/client.ts`: add `export function getConfig(): Promise<ConfigResponse> { return request("GET", "/api/config"); }`, following the existing `request<T>()` wrapper (`client.ts:46-97`) — no new error handling needed, 401 is already handled centrally.
- `ConfigTab.tsx` layout: three `<section>`s, "MCP Servers", "Providers", and "Telemetry" (in that order, matching `bae-config.toml`'s own `[mcp]`-then-`[providers]`-then-`[telemetry]` layout, `config_file.rs`'s doc example), each rendered as a grid/list of cards (reusing the app's existing `panel`/`data-table` CSS classes already used by `ProfilesTab`/`KeysTab`, not inventing a new visual language). Each MCP server card shows name, transport (as a small badge, mirroring how `EventGraph`/`ShapeMarker` already color-codes categorical values), `command`+`args` (stdio) or `url` (sse/http), and a "Headers" sub-list rendering each key with its value replaced by the literal string MAX received from the server (never regenerated client-side — the browser simply displays what it was sent, so there is no client-side redaction logic to get wrong or bypass). Each provider card shows name, provider kind, model, effective base URL, and an "Auth token: ••••••••" row.
- The **Telemetry** section is a single card (not a list — telemetry is one object, not a registry). When `telemetry.enabled` is `false`, show an "Enabled: no" state with a short caption ("OpenTelemetry export is disabled") and, at most, the other fields greyed/secondary — do **not** render it as an empty/missing section (an absent `[telemetry]` table is a valid, disabled configuration, not a fetch failure). When enabled, show `enabled` (a badge), `otlp_endpoint`, `sample_ratio`, `service_name`, `traces.enabled`, `metrics.enabled`, `metrics.disabled` (as a small list, or "none" when empty), and an "OTLP headers" sub-list rendering each key with its dotted value exactly like the MCP "Headers" sub-list (same component/pattern — displaying the redaction marker MAX received, never re-derived client-side). "No OTLP headers configured" empty-state for an empty `otlp_headers` map, mirroring the MCP no-headers case.
- Empty states: if `mcp.servers` is empty, the MCP section shows an explanatory empty state ("No MCP servers are configured in `bae-config.toml`."); same pattern for an empty `providers.entries`, both following the existing "every empty list explains what the resource is" convention already used in `ProfilesTab.tsx:83-99`. Telemetry has no "empty list" state (it is always a present object); its analogue is the "Enabled: no" state described above.
- Loading/error states reuse `Spinner`/`ErrorBanner` from `components/ui.tsx`, matching every other tab.
- `max/web/src/components/Layout.tsx`: extend `TabId` from `"keys" | "profiles" | "sessions"` to include `"config"` (`Layout.tsx:4`), and append `{ id: "config", label: "Config" }` to the end of the `TABS` array (`Layout.tsx:6-10`). The existing array order is already `sessions, profiles, keys` — the summary's requested order (Sessions, Profiles, Keys, Config) is satisfied by appending, no reordering of the existing three.
- `max/web/src/App.tsx`: import `ConfigTab` and add `{tab === "config" && <ConfigTab />}` alongside the other three tab branches (`App.tsx:52-57`).

### E. Documentation

- `docs/reference/admin-api.md`: new `## Config` section (after `## Providers` at `admin-api.md:404`, before `## Key security` at `admin-api.md:443`) documenting `GET /admin/v1/config`, its combined response shape (all three of `mcp`/`providers`/`telemetry`), and the redaction convention, mirroring the existing MCP Servers/Providers sections' structure (`admin-api.md:370-442`).
- `docs/reference/configuration.md`: add a new `## Admin endpoint: GET /admin/v1/config` section alongside the existing three (`configuration.md:463` MCP servers, `487` providers, `512` sandbox-status — the WI 0013 rewrite shifted these down from their pre-0013 positions). The section documents the combined shape and notes it reflects the same startup snapshot as the `[mcp]`/`[providers]`/`[telemetry]` sections it mirrors.
- `docs/guides/max-webapp.md`: mention the Config tab in the dashboard walkthrough.
- Update `admin/mod.rs`'s top doc-comment endpoint list (`admin/mod.rs:14-24`) to add the new `config` bullet, following its existing one-line-per-endpoint format.

## Edge Case Considerations:

- **Literal secret typed directly into TOML (no `${ENV_VAR}` indirection).** Covered in B — masking is unconditional on the field, never conditional on the value's shape, so a literal `headers = { Authorization = "Bearer sk-abc123" }` is masked exactly like a `${TOKEN}` reference.
- **Empty `headers` map on a stdio server, or a `url`-only sse/http server with no `headers` at all.** Renders as "no headers configured" rather than an empty, confusing sub-list — there is nothing to redact when the map itself is empty.
- **A header key configured with an empty-string value (`headers = { "X-Custom" = "" }`).** Still redacted uniformly like every other header value — the endpoint never distinguishes "set but empty" from "set to something," since making that distinction would itself leak information about the underlying value.
- **No config file, or a file with none of `[mcp]`/`[providers]`/`[telemetry]`.** The MCP and Providers sections render their empty state and the Telemetry section renders its "Enabled: no" state; none of this is an error state (`config_file.rs`'s established "missing file → empty registry / default-disabled telemetry, no error" contract) and none must look like a fetch failure.
- **Telemetry present but disabled (`enabled = false`), or absent entirely.** Both resolve to the same server-side value — `telemetry_config()` returns `TelemetryConfig::default()` (`enabled: false`) for an absent section — so the tab cannot and need not distinguish "no `[telemetry]` table" from "`[telemetry]` with `enabled = false`". Render both as the disabled Telemetry card; do not attempt to show "not configured" vs "configured but off" differently, since the endpoint deliberately collapses them (matching how it collapses absent-vs-empty for MCP/providers).
- **Telemetry `otlp_headers` secret leaking.** Handled by B exactly as MCP `headers`: value-by-value unconditional redaction, keys preserved. A literal collector token typed straight into `otlp_headers` (no `${...}`) is masked identically to a `${OTEL_TOKEN}` reference — same "never pattern-match on shape" rule. `otlp_endpoint` is *not* redacted (a collector URL is not a secret); only `otlp_headers` values are.
- **Config is a startup snapshot, not live.** `mcp_registry`/`provider_registry` are read-only after startup and rebuilt only on restart (`api/mod.rs`'s existing doc comments, now at `api/mod.rs:168-179`) — and the new `telemetry_config` field is set once at startup with the identical lifecycle (`[telemetry]`, like every other section, is read once at load with no hot-reload, per `docs/reference/configuration.md`'s "No hot reload" note added by WI 0013). Editing `bae-config.toml` on disk without restarting `baesrv` has no effect on this tab. State this explicitly in the tab (a small caption under each section heading, e.g. "as loaded at server startup"), matching the existing admin-api.md language ("rebuilt on restart; this endpoint reflects the current in-memory state").
- **New route bypassing the admin-auth layer by accident.** The auth middleware wraps the whole router (`admin/mod.rs:74-79`), so a route added inside `router()` before that `.layer(...)` call is protected automatically — but this is easy to get wrong if the route were added after layering or in a separate `Router::merge`. Verify with an explicit auth-matrix test (see Test Considerations) rather than relying on code inspection alone.
- **`--dangerously-disable-admin-auth`.** Same posture as every other admin route: with auth disabled, `/admin/v1/config` is open too — an existing, documented tradeoff (`admin/mod.rs:7-12`), not a new one introduced by this work item.
- **Env-driven runtime config (`BAE_ADDR`, `BAE_DB_PATH`, `BAE_SANDBOX_DRIVER`, `BAE_OTEL_LOG`, etc.) is out of scope**, per the Summary's naming note — `AppState` doesn't currently retain the full `crate::config::Config` struct (only a handful of derived fields — `turn_timeout`, `sandbox_driver`, etc. — survive into `AppState`), so surfacing it would need broad new state plumbing this work item deliberately does not add. If a future work item wants a "Runtime Config" section too, `AppState` would need to start holding the resolved `Config` itself. **Note the deliberate asymmetry with telemetry:** `[telemetry]` *is* in scope even though it, too, isn't currently on `AppState` — because it is **file**-driven config (the exact `bae-config.toml` this tab targets), the summary's "a section for each top-level field" names it directly, and retaining it costs exactly **one** additive `Arc<TelemetryConfig>` field (see A), whereas retaining the whole env-driven `Config` is a much larger surface the summary never asked for. The line to draw is file-config-vs-env-config, not "already on `AppState` vs not."
- **MAX's own proxy must not introduce a new leak path.** Since the admin server already redacts before responding, `max/server`'s proxy (`routes.ts`'s `proxy()` helper) forwards the body verbatim with no transformation — there is no second place secrets could leak from on the MAX side, but an `AdminApiError`'s `detail` string must still never echo request/response bodies verbatim in a way that could reintroduce a leak (existing `proxy()` behavior already only forwards `{type, detail}` from the upstream error, `routes.ts:149-157` — confirm this holds for a hypothetical malformed-response case here too).

## Test Considerations:

- **Server unit/integration test — `GET /admin/v1/config` shape**: boot the real admin router against a config fixture with a stdio MCP server (with `args`), an sse MCP server (with a `${ENV_VAR}`-style header value), two providers (one with an explicit `base_url`, one relying on the kind default), **and an enabled `[telemetry]` section with an `otlp_endpoint`, an `otlp_headers` entry, a non-default `sample_ratio`/`service_name`, and a `metrics.disabled` entry** — assert `command`/`args`/`url` appear in full, `headers` keeps keys but every value equals the redaction marker, `auth_token` equals the redaction marker for both providers, `base_url` is the *effective* value for the default-endpoint provider, and for `telemetry`: `enabled`/`otlp_endpoint`/`sample_ratio`/`traces`/`metrics` appear in full, `service_name` is the effective name, and `otlp_headers` keeps its keys but every value equals the redaction marker. Mirrors the existing `missing_primary_provider_blocks_session_creation` test's assertion style at `server/tests/integration.rs:1266-1277`.
- **Server test — literal-secret masking**: a fixture where an MCP header value, an `auth_token`, **and a telemetry `otlp_headers` value** are each a literal string with no `${...}` syntax at all — assert the response does not contain any of those literal strings anywhere in the raw body (same `assert!(!raw.contains(...))` pattern as `integration.rs:1277`), proving masking is unconditional rather than pattern-matched across all three secret-bearing fields.
- **Server test — empty config**: no config file (or a file with none of the three tables) → `{"mcp": {"servers": []}, "providers": {"entries": []}, "telemetry": {"enabled": false, …}}`, `200 OK`, not an error. Assert `telemetry.enabled` is `false` and no secret/endpoint values are invented for the absent-telemetry case.
- **Server test — telemetry `service_name` effective default**: a fixture with `[telemetry]` enabled but no `service_name` set → response `telemetry.service_name` is `"baesrv"` (the effective default), never `null`, matching the `base_url`-effective-value convention.
- **Server regression — admin-auth matrix**: add `/admin/v1/config` to the table-driven route list `server/tests/admin_auth.rs` already exercises for 401-without-key / 200-with-key / open-when-disabled behavior (`admin_auth.rs:350,464`), so the new route is proven to inherit the same auth layer as every existing admin route rather than trusting code review alone.
- **MAX-server unit test — `adminClient.getConfig()`**: request-building test mirroring the existing `listProviders`/`listMcpServers` coverage, asserting the built request targets `GET /admin/v1/config` with the admin bearer token attached.
- **MAX-server unit test — `/api/config` route**: `routes.test.ts` coverage proving the route requires the session cookie (falls under `requireAuth`) and passes upstream `AdminApiError`s through with their original status, mirroring existing `/api/providers`/`/api/mcp-servers` route tests.
- **Frontend component test — `ConfigTab.test.tsx`**: mirroring `KeysTab.test.tsx`/`ProfilesTab.test.tsx` conventions — renders all three sections from a mocked `getConfig()` response, renders the redaction marker verbatim for an MCP header value, for `auth_token`, and for a telemetry `otlp_headers` value (asserting the component never tries to re-derive or further transform that string), renders each list section's empty state when its array is empty, renders the enabled Telemetry card's non-secret fields (`otlp_endpoint`, `sample_ratio`, `service_name`, `metrics.disabled`) in full, and renders the disabled Telemetry state ("Enabled: no") when the mocked response has `telemetry.enabled === false` — asserting the disabled case is a present card, not a missing/empty section.
- **End-to-end / manual verification**: run `bae-max` against a `bae-config.toml` containing at least one MCP server of each transport, two providers, and an enabled `[telemetry]` section with an `otlp_headers` token; open the Config tab; visually confirm the four requested tabs appear in order Sessions → Profiles → Keys → Config and that the Config tab shows all three sections (MCP Servers, Providers, Telemetry); use browser devtools' Network panel to confirm the `/api/config` response body itself contains no literal secret value (MCP header, `auth_token`, or `otlp_headers` token), not just that the rendered DOM doesn't show one.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New admin handler module `server/src/api/admin/config.rs`, registered in `server/src/api/admin/mod.rs` exactly like `mcp.rs`/`providers.rs`/`sandbox.rs` already are — no new router-construction pattern, no new pagination helper (this endpoint is intentionally unpaginated, matching its two closest siblings).
- Exactly **one** new `AppState` field — `telemetry_config: Arc<TelemetryConfig>` (see Implementation Details A) — set once at startup in `serve()` (`server/src/lib.rs:139-148`) with the same read-only-after-startup lifecycle as `mcp_registry`/`provider_registry`, which are reused verbatim. This is the minimal additive plumbing WI 0013 left unavailable (it consumes the parsed `TelemetryConfig` into telemetry handles without retaining the raw config); the change stays additive and low-risk — no existing field or handler is modified, and the field defaults to `TelemetryConfig::default()` so every existing `AppState::new`/`with_registries` call site keeps compiling unchanged.
- Frontend follows the existing `max/web/src/tabs/*Tab.tsx` + `Layout.tsx` `TabId` + `App.tsx` branch pattern exactly, and the existing `max/server/src/{adminClient,routes}.ts` "add one more read-only proxy method + route" pattern exactly — both are additive, no restructuring of either file.
- Update `docs/reference/admin-api.md`, `docs/reference/configuration.md`, and `docs/guides/max-webapp.md` per Implementation Details section E.
- Preserve the "UI is a pure API client" mandate (`aspec/uxui/interface.md`) — the Config tab reaches this data only through the new documented `/admin/v1/config` → `/api/config` path, never any direct file or DB access from `max/server`.
