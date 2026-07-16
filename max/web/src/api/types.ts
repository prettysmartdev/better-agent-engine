// Shared API types. These mirror the browser-facing surface documented in
// max-api-contract.md — bodies pass through unchanged from BAE's admin port.

/** A cursor-paginated list response. */
export interface Page<T> {
  items: T[];
  next_cursor: string | null;
}

/** A configuration profile (create/edit shape used by the form). */
export interface Profile {
  id: string;
  name: string;
  primary_provider: string;
  fallback_providers: string[];
  mcp_servers: string[];
  allowed_tools: string[];
  created_at?: string;
}

/** Fields the create/edit profile form submits. */
export interface ProfileInput {
  name: string;
  primary_provider: string;
  fallback_providers: string[];
  mcp_servers: string[];
  allowed_tools: string[];
}

/** A client key as it appears in the list (no plaintext). */
export interface KeyListItem {
  id: string;
  name: string;
  prefix: string;
  profile_id: string;
  created_at?: string;
  last_used_at?: string | null;
}

/**
 * The response to creating a key. `key` is the one-time plaintext — it is never
 * returned again and must be shown to the operator exactly once.
 */
export interface KeyCreated {
  id: string;
  name: string;
  key: string;
  prefix: string;
  profile_id: string;
  created_at?: string;
}

/** A named registry entry (provider or MCP server) used to populate pickers. */
export interface RegistryEntry {
  name: string;
  [key: string]: unknown;
}

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

export type SessionState = "open" | "closed" | "error";

/** A session row from the sessions list. */
export interface SessionListItem {
  id: string;
  profile_id: string;
  state: SessionState;
  client_version: string | null;
  created_at: string;
  closed_at: string | null;
}

/** A single recorded event (graph node + detail-panel payload). */
export interface SessionEvent {
  id: number;
  session_id: string;
  client_key_id: string | null;
  event_type: string;
  payload: unknown;
  created_at: string;
}

/** Frames the observer WebSocket sends the browser (see the API contract). */
export type WsFrame =
  | { type: "history"; events: SessionEvent[] }
  | { type: "event"; event: SessionEvent }
  | { type: "session_ended"; reason: "closed" | "error"; message?: string }
  | { type: "error"; message: string };

/** True for MAX's own auto-provisioned observer keys. */
export function isObserverKey(name: string): boolean {
  return name.startsWith("max-observer-");
}
