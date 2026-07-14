# Work Item: Feature

Title: configuration tab in MAX
Issue: issuelink

## Summary:
- Add a new tab in the MAX webapp which is a READ-ONLY view on the configuration that was passed via the bae-config.yaml file at server startup.

The tab should be a single page with a distinct section for each of the top-level fields in the config file showing what is configured. It should be a nice, rich, easily readable layout. Ensure all secrets are not actually returned from the server and are shown only as dots in the webapp.

This should be powered by an endpoint on the admin server (if one doesn't exist already).

The tab order should be Sessions, Profiles, Keys, Config

**Naming note:** the file-driven config that actually exists on `main` is `bae-config.toml` (TOML, not YAML) — see `server/src/config_file.rs` and `docs/reference/configuration.md`'s "`bae-config.toml` schema" section. This work item targets that file. Its top-level layout (`BaeConfig`, `config_file.rs:71-79`) has exactly two sections today — `[mcp]` and `[providers]` — so "a distinct section for each of the top-level fields" means two sections, `MCP Servers` and `Providers`, with room in the layout for a third section if a future top-level key (e.g. `[logging]`, called out as an example in `config_file.rs:16`) is added later. This is deliberately scoped to the **file**-driven config only — the separate env-driven `crate::config::Config` (`BAE_ADDR`, `BAE_DB_PATH`, `BAE_SANDBOX_DRIVER`, etc., `server/src/config.rs`) is out of scope, since the summary specifically names the config **file** and `AppState` does not currently hold that struct in a form an admin endpoint could read back out (see Edge Case Considerations).

**Endpoint note:** two read-only admin endpoints already exist that touch this data — `GET /admin/v1/mcp-servers` and `GET /admin/v1/providers` (`server/src/api/admin/mcp.rs`, `admin/providers.rs`) — but both are deliberately **minimal**: they omit `command`/`args`/`url`/`headers` and `auth_token` entirely rather than masking them (`docs/reference/admin-api.md:395-396,431-432`). The summary asks for secrets to be "shown only as dots," which requires the field to be *present* (so the UI has something to render as dots) rather than absent — omission and masking are different contracts, and changing the existing endpoints' shape would risk the `ProfilesTab` pickers that already depend on their current minimal shape (`max/web/src/tabs/ProfilesTab.tsx:5-19`, `max/server/src/adminClient.ts:182-188`). So this work item adds a **new, additive** `GET /admin/v1/config` endpoint rather than modifying the existing two.

## User Stories

### User Story 1:
As a: Platform Operator

I want to:
open a single "Config" tab in MAX and see every MCP server and LLM provider `bae-config.toml` configured this server with — transport, command/args or URL, model, effective base URL — laid out in clearly labeled sections

So I can:
confirm what a running server actually has available without `docker exec`-ing in to read the file on disk or cross-referencing two separate list-only admin endpoints by hand

### User Story 2:
As a: Platform Operator

I want to:
see that any secret-bearing value (MCP server `headers`, provider `auth_token`) is rendered as a fixed dot placeholder — never the literal token, resolved or unresolved — even though I can see which fields *have* a secret configured

So I can:
verify a server's configuration at a glance from a browser (potentially off-loopback, per MAX's own auth model in `aspec/work-items/0007-max-webapp.md` section D) without that browser ever holding, transmitting a second time, or being able to leak the real secret value

### User Story 3:
As an: Agent Developer troubleshooting a profile

I want to:
cross-check a profile's `primary_provider`/`mcp_servers` names against the full Config tab in one glance, right next to the Profiles tab

So I can:
catch a typo'd or removed registry name without leaving the dashboard, using the same tab bar that already covers Sessions/Profiles/Keys

## Implementation Details:

This is a small, mostly additive feature: (A) one new read-only admin-port endpoint; (B) a redaction convention for the two known secret-bearing fields; (C) a thin MAX-server proxy route; (D) the new frontend tab; (E) docs.

### A. New admin endpoint: `GET /admin/v1/config`

- New module `server/src/api/admin/config.rs`, mirroring the doc-comment style and no-secrets framing of `admin/mcp.rs`/`admin/providers.rs`.
- Reads from the same `AppState` fields those two handlers already read — `state.mcp_registry: Arc<HashMap<String, McpServerConfig>>` and `state.provider_registry: Arc<HashMap<String, ProviderConfig>>` (`server/src/api/mod.rs:122,128`) — so it requires **no new server-side state**, just a richer view over data already loaded at startup from `bae-config.toml`.
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
    }
  }
  ```
  - `mcp.servers[].command`/`args`/`url` are exposed **in full** — not secret, and today omitted by `/admin/v1/mcp-servers` only for brevity, not for safety (`docs/reference/admin-api.md:395`). This is the "richer" part of the summary's "nice, rich, easily readable layout."
  - `mcp.servers[].headers` keeps every header **key** but replaces every value with a fixed redaction marker (see B) — so an operator can see *that* `Authorization` is set without ever seeing its value.
  - `providers.entries[].auth_token` is always present and always the redaction marker (`ProviderConfig.auth_token` is a required, non-optional `String`, `engine/provider.rs:99` — never absent for a defined entry).
  - `providers.entries[].base_url` is the **effective** value (`ProviderConfig::effective_base_url()`), matching `/admin/v1/providers`' existing convention (`admin/providers.rs:34`).
  - Both `servers` and `entries` sorted by `name` for stable output, matching every existing admin list handler's convention (`mcp.rs:27`, `providers.rs:26`, `sandbox.rs:55`).
  - A missing config file, or a file with no `[mcp]`/`[providers]` table, yields `{"mcp": {"servers": []}, "providers": {"entries": []}}` — never an error, matching `mcp_registry`/`provider_registry`'s own "absent → empty" contract (`config_file.rs:264-266`).
- Register the route in `server/src/api/admin/mod.rs`'s router (alongside the other `get(...)` routes, `admin/mod.rs:52-70`) and add `config` to that file's top doc-comment endpoint list (`admin/mod.rs:14-24`). It picks up the same `require_admin_auth` layer as every other admin route automatically, since that layer wraps the whole router (`admin/mod.rs:74-79`) — no new auth wiring needed, but see Test Considerations for a regression check that this is actually true for the new route.

### B. Redaction convention

- A single `pub const REDACTED: &str = "••••••••"` in `admin/config.rs`, used for **every** secret-bearing value regardless of whether it happens to look like an unresolved `${ENV_VAR}` token or a literal secret typed directly into the TOML. This matters because `state.mcp_registry`/`state.provider_registry` hold the **raw, unresolved** config exactly as parsed (`config_file.rs`'s "Secrets" doc section: `${ENV_VAR}` tokens are "not resolved here" — resolution happens later, at connect/call time, via `resolve_tokens`) — an operator is free to skip the `${...}` indirection entirely and write a literal secret straight into `headers`/`auth_token`, and the masking must not special-case on string shape (e.g. "only mask things matching `${...}`") or it would leak literal secrets verbatim. Always redact the whole field unconditionally; never attempt partial masking (e.g. show a prefix) that could leak length or a recognizable substring.
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
  export interface ConfigResponse {
    mcp: { servers: McpServerConfigView[] };
    providers: { entries: ProviderConfigView[] };
  }
  ```
- `max/web/src/api/client.ts`: add `export function getConfig(): Promise<ConfigResponse> { return request("GET", "/api/config"); }`, following the existing `request<T>()` wrapper (`client.ts:46-97`) — no new error handling needed, 401 is already handled centrally.
- `ConfigTab.tsx` layout: two `<section>`s, "MCP Servers" and "Providers" (in that order, matching `bae-config.toml`'s own `[mcp]`-then-`[providers]` layout, `config_file.rs`'s doc example), each rendered as a grid/list of cards (reusing the app's existing `panel`/`data-table` CSS classes already used by `ProfilesTab`/`KeysTab`, not inventing a new visual language). Each MCP server card shows name, transport (as a small badge, mirroring how `EventGraph`/`ShapeMarker` already color-codes categorical values), `command`+`args` (stdio) or `url` (sse/http), and a "Headers" sub-list rendering each key with its value replaced by the literal string MAX received from the server (never regenerated client-side — the browser simply displays what it was sent, so there is no client-side redaction logic to get wrong or bypass). Each provider card shows name, provider kind, model, effective base URL, and an "Auth token: ••••••••" row.
- Empty states: if `mcp.servers` is empty, the MCP section shows an explanatory empty state ("No MCP servers are configured in `bae-config.toml`."); same pattern for an empty `providers.entries`, both following the existing "every empty list explains what the resource is" convention already used in `ProfilesTab.tsx:83-99`.
- Loading/error states reuse `Spinner`/`ErrorBanner` from `components/ui.tsx`, matching every other tab.
- `max/web/src/components/Layout.tsx`: extend `TabId` from `"keys" | "profiles" | "sessions"` to include `"config"` (`Layout.tsx:4`), and append `{ id: "config", label: "Config" }` to the end of the `TABS` array (`Layout.tsx:6-10`). The existing array order is already `sessions, profiles, keys` — the summary's requested order (Sessions, Profiles, Keys, Config) is satisfied by appending, no reordering of the existing three.
- `max/web/src/App.tsx`: import `ConfigTab` and add `{tab === "config" && <ConfigTab />}` alongside the other three tab branches (`App.tsx:52-57`).

### E. Documentation

- `docs/reference/admin-api.md`: new `## Config` section (after `## Providers`, before `## Key security`) documenting `GET /admin/v1/config`, its combined response shape, and the redaction convention, mirroring the existing MCP Servers/Providers sections' structure (`admin-api.md:370-441`).
- `docs/reference/configuration.md`: add a fourth `## Admin endpoint: GET /admin/v1/config` section alongside the existing three (`configuration.md:351,375,400`).
- `docs/guides/max-webapp.md`: mention the Config tab in the dashboard walkthrough.
- Update `admin/mod.rs`'s top doc-comment endpoint list (`admin/mod.rs:14-24`) to add the new `config` bullet, following its existing one-line-per-endpoint format.

## Edge Case Considerations:

- **Literal secret typed directly into TOML (no `${ENV_VAR}` indirection).** Covered in B — masking is unconditional on the field, never conditional on the value's shape, so a literal `headers = { Authorization = "Bearer sk-abc123" }` is masked exactly like a `${TOKEN}` reference.
- **Empty `headers` map on a stdio server, or a `url`-only sse/http server with no `headers` at all.** Renders as "no headers configured" rather than an empty, confusing sub-list — there is nothing to redact when the map itself is empty.
- **A header key configured with an empty-string value (`headers = { "X-Custom" = "" }`).** Still redacted uniformly like every other header value — the endpoint never distinguishes "set but empty" from "set to something," since making that distinction would itself leak information about the underlying value.
- **No config file, or a file with neither `[mcp]` nor `[providers]`.** Both sections render their empty state; this is not an error state (`config_file.rs`'s established "missing file → empty registry, no error" contract) and must not look like a fetch failure.
- **Config is a startup snapshot, not live.** `mcp_registry`/`provider_registry` are read-only after startup and rebuilt only on restart (`api/mod.rs:118-127`'s existing doc comments) — editing `bae-config.toml` on disk without restarting `baesrv` has no effect on this tab. State this explicitly in the tab (a small caption under each section heading, e.g. "as loaded at server startup"), matching the existing admin-api.md language ("rebuilt on restart; this endpoint reflects the current in-memory state").
- **New route bypassing the admin-auth layer by accident.** The auth middleware wraps the whole router (`admin/mod.rs:74-79`), so a route added inside `router()` before that `.layer(...)` call is protected automatically — but this is easy to get wrong if the route were added after layering or in a separate `Router::merge`. Verify with an explicit auth-matrix test (see Test Considerations) rather than relying on code inspection alone.
- **`--dangerously-disable-admin-auth`.** Same posture as every other admin route: with auth disabled, `/admin/v1/config` is open too — an existing, documented tradeoff (`admin/mod.rs:7-12`), not a new one introduced by this work item.
- **Env-driven runtime config (`BAE_ADDR`, `BAE_DB_PATH`, `BAE_SANDBOX_DRIVER`, etc.) is out of scope**, per the Summary's naming note — `AppState` doesn't currently retain the full `crate::config::Config` struct (only `turn_timeout: Duration` and a constructed `sandbox_driver: Arc<dyn SandboxDriver>` trait object survive into `AppState`, `api/mod.rs:153-161`), so surfacing it would need new state plumbing this work item deliberately does not add. If a future work item wants a "Runtime Config" section too, `AppState` would need to start holding the resolved `Config` itself.
- **MAX's own proxy must not introduce a new leak path.** Since the admin server already redacts before responding, `max/server`'s proxy (`routes.ts`'s `proxy()` helper) forwards the body verbatim with no transformation — there is no second place secrets could leak from on the MAX side, but an `AdminApiError`'s `detail` string must still never echo request/response bodies verbatim in a way that could reintroduce a leak (existing `proxy()` behavior already only forwards `{type, detail}` from the upstream error, `routes.ts:149-157` — confirm this holds for a hypothetical malformed-response case here too).

## Test Considerations:

- **Server unit/integration test — `GET /admin/v1/config` shape**: boot the real admin router against a config fixture with a stdio MCP server (with `args`), an sse MCP server (with a `${ENV_VAR}`-style header value), and two providers (one with an explicit `base_url`, one relying on the kind default) — assert `command`/`args`/`url` appear in full, `headers` keeps keys but every value equals the redaction marker, `auth_token` equals the redaction marker for both providers, and `base_url` is the *effective* value for the default-endpoint provider. Mirrors the existing `missing_primary_provider_blocks_session_creation` test's assertion style at `server/tests/integration.rs:1266-1277`.
- **Server test — literal-secret masking**: a fixture where a header value or `auth_token` is a literal string with no `${...}` syntax at all — assert the response does not contain that literal string anywhere in the raw body (same `assert!(!raw.contains(...))` pattern as `integration.rs:1277`), proving masking is unconditional rather than pattern-matched.
- **Server test — empty config**: no config file (or a file with neither table) → `{"mcp": {"servers": []}, "providers": {"entries": []}}`, `200 OK`, not an error.
- **Server regression — admin-auth matrix**: add `/admin/v1/config` to the table-driven route list `server/tests/admin_auth.rs` already exercises for 401-without-key / 200-with-key / open-when-disabled behavior (`admin_auth.rs:350,464`), so the new route is proven to inherit the same auth layer as every existing admin route rather than trusting code review alone.
- **MAX-server unit test — `adminClient.getConfig()`**: request-building test mirroring the existing `listProviders`/`listMcpServers` coverage, asserting the built request targets `GET /admin/v1/config` with the admin bearer token attached.
- **MAX-server unit test — `/api/config` route**: `routes.test.ts` coverage proving the route requires the session cookie (falls under `requireAuth`) and passes upstream `AdminApiError`s through with their original status, mirroring existing `/api/providers`/`/api/mcp-servers` route tests.
- **Frontend component test — `ConfigTab.test.tsx`**: mirroring `KeysTab.test.tsx`/`ProfilesTab.test.tsx` conventions — renders both sections from a mocked `getConfig()` response, renders the redaction marker verbatim for a header value and for `auth_token` (asserting the component never tries to re-derive or further transform that string), and renders each section's empty state when its array is empty.
- **End-to-end / manual verification**: run `bae-max` against a `bae-config.toml` containing at least one MCP server of each transport and two providers; open the Config tab; visually confirm the four requested tabs appear in order Sessions → Profiles → Keys → Config; use browser devtools' Network panel to confirm the `/api/config` response body itself contains no literal secret value, not just that the rendered DOM doesn't show one.

## Codebase Integration:
- follow established conventions, best practices, testing, and architecture patterns from the project's aspec.
- New admin handler module `server/src/api/admin/config.rs`, registered in `server/src/api/admin/mod.rs` exactly like `mcp.rs`/`providers.rs`/`sandbox.rs` already are — no new router-construction pattern, no new pagination helper (this endpoint is intentionally unpaginated, matching its two closest siblings).
- No new `AppState` fields — reuses `mcp_registry`/`provider_registry` verbatim, keeping this a purely additive, low-risk change to the admin surface.
- Frontend follows the existing `max/web/src/tabs/*Tab.tsx` + `Layout.tsx` `TabId` + `App.tsx` branch pattern exactly, and the existing `max/server/src/{adminClient,routes}.ts` "add one more read-only proxy method + route" pattern exactly — both are additive, no restructuring of either file.
- Update `docs/reference/admin-api.md`, `docs/reference/configuration.md`, and `docs/guides/max-webapp.md` per Implementation Details section E.
- Preserve the "UI is a pure API client" mandate (`aspec/uxui/interface.md`) — the Config tab reaches this data only through the new documented `/admin/v1/config` → `/api/config` path, never any direct file or DB access from `max/server`.
