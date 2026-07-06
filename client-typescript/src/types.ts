/**
 * Wire types for the BAE `/api/v1` client surface.
 *
 * The content model and event catalog mirror the authoritative wire contract
 * (see `/awman/context/workflow/api-contract.md`, §6 and §8). Keys and ids are
 * treated as opaque bearer/identifier strings — never parsed or length-checked.
 */

// ---------------------------------------------------------------------------
// Content model (§6)
// ---------------------------------------------------------------------------

/** A single Anthropic-style content block. Discriminated on `type`. */
export type ContentBlock =
  | { type: "text"; text: string }
  | {
      type: "tool_use";
      id: string;
      name: string;
      input: Record<string, unknown>;
    }
  | { type: "tool_result"; tool_use_id: string; content: Content };

/** A turn's content: either a plain string or an array of content blocks. */
export type Content = string | ContentBlock[];

/** A conversation turn. `role` is typically `"user"` or `"assistant"`. */
export interface Message {
  role: string;
  content: Content;
}

/** A `tool_use` request extracted from an assistant turn. */
export interface ToolUse {
  id: string;
  name: string;
  input: Record<string, unknown>;
}

/**
 * The event handed to the `after_tool_call` hook. `content` is the handler's
 * return value and may be rewritten in place before it is sent back.
 */
export interface ToolResult {
  tool_use_id: string;
  name: string;
  content: Content;
}

/** Normalize a `send()` argument into a `user` `Message`. */
export function toMessage(input: string | Message): Message {
  return typeof input === "string" ? { role: "user", content: input } : input;
}

/** Concatenate all `text` blocks of a message (handy for printing a turn). */
export function messageText(message: Message): string {
  if (typeof message.content === "string") return message.content;
  return message.content
    .filter(
      (b): b is Extract<ContentBlock, { type: "text" }> => b.type === "text",
    )
    .map((b) => b.text)
    .join("");
}

/** Extract the `tool_use` blocks of a message. Empty ⇒ the loop ends. */
export function toolUses(message: Message): ToolUse[] {
  if (typeof message.content === "string") return [];
  return message.content
    .filter(
      (b): b is Extract<ContentBlock, { type: "tool_use" }> =>
        b.type === "tool_use",
    )
    .map((b) => ({ id: b.id, name: b.name, input: b.input }));
}

// ---------------------------------------------------------------------------
// Profile (returned sanitized on session open — no auth token / env names)
// ---------------------------------------------------------------------------

export interface ProfileProvider {
  provider: string;
  model: string;
}

export interface Profile {
  id: string;
  name: string;
  allowed_tools: string[];
  mcp_servers: unknown[];
  provider: ProfileProvider;
}

// ---------------------------------------------------------------------------
// Event catalog (§8) — a closed, discriminated union keyed on `event_type`.
//
// The union is exhaustive: `describeEvent` switches over every arm and passes
// the fall-through to `assertNever`, so introducing a new `event_type` without
// handling it here is a compile error.
// ---------------------------------------------------------------------------

/** The closed set of 12 event type strings. */
export type EventType =
  | "client.message.send"
  | "server.message.send"
  | "provider.request"
  | "provider.response"
  | "tool.call"
  | "tool.result"
  | "mcp.request"
  | "mcp.response"
  | "session.open"
  | "session.close"
  | "session.error"
  | "session.compaction";

export interface ClientMessagePayload {
  role: "user";
  content: Content;
}
export interface ServerMessagePayload {
  role: "assistant";
  content: ContentBlock[];
}
export interface ProviderRequestPayload {
  attempt: number;
  kind: "primary" | "fallback";
  provider: string;
  base_url: string;
  model: string;
  max_tokens: number;
  messages: unknown[];
  tools: unknown[];
}
export type ProviderResponsePayload =
  | {
      attempt: number;
      kind: "primary" | "fallback";
      provider: string;
      ok: true;
      status: number;
      body: Record<string, unknown>;
    }
  | {
      attempt: number;
      kind: "primary" | "fallback";
      provider: string;
      ok: false;
      status: number | null;
      error: string;
      body: string | null;
    };
export interface ToolCallPayload {
  id: string;
  name: string;
  input: Record<string, unknown>;
  dispatch: "client" | "mcp";
}
export interface ToolResultPayload {
  tool_use_id: string;
  dispatch: "client" | "mcp";
  content: unknown;
  status?: "stub";
}
export interface McpRequestPayload {
  status: "stub";
  tool: string;
  input: Record<string, unknown>;
}
export interface McpResponsePayload {
  status: "stub";
  tool: string;
}
export interface SessionOpenPayload {
  client_version: string | null;
  tools: string[];
}
export interface SessionClosePayload {
  reason: "client_close" | "client_key_revoked";
}
export interface SessionErrorPayload {
  reason:
    | "provider_config"
    | "provider_call_failed"
    | "all_providers_failed"
    | "loop_limit"
    | "profile_unavailable";
  [key: string]: unknown;
}
export interface SessionCompactionPayload {
  [key: string]: unknown;
}

/** Maps each `event_type` to its payload shape. */
interface EventPayloads {
  "client.message.send": ClientMessagePayload;
  "server.message.send": ServerMessagePayload;
  "provider.request": ProviderRequestPayload;
  "provider.response": ProviderResponsePayload;
  "tool.call": ToolCallPayload;
  "tool.result": ToolResultPayload;
  "mcp.request": McpRequestPayload;
  "mcp.response": McpResponsePayload;
  "session.open": SessionOpenPayload;
  "session.close": SessionClosePayload;
  "session.error": SessionErrorPayload;
  "session.compaction": SessionCompactionPayload;
}

/** The envelope every event is wrapped in (also the events-endpoint row shape). */
interface EventEnvelope<T extends EventType> {
  id: string;
  session_id: string;
  client_key_id: string | null;
  event_type: T;
  payload: EventPayloads[T];
  created_at: string;
}

/** A single append-only session event — discriminated on `event_type`. */
export type SessionEvent = {
  [T in EventType]: EventEnvelope<T>;
}[EventType];

/** Compile-time exhaustiveness guard. */
export function assertNever(value: never): never {
  throw new Error(`unhandled event: ${JSON.stringify(value)}`);
}

/**
 * A one-line human description of an event. Its exhaustive switch is what makes
 * the `SessionEvent` union closed: a new `event_type` that is not handled here
 * fails to type-check.
 */
export function describeEvent(event: SessionEvent): string {
  switch (event.event_type) {
    case "client.message.send":
      return "client → server: user turn";
    case "server.message.send":
      return "server → client: assistant turn";
    case "provider.request":
      return `provider request (attempt ${event.payload.attempt}, ${event.payload.kind})`;
    case "provider.response":
      return `provider response (ok=${event.payload.ok})`;
    case "tool.call":
      return `tool call ${event.payload.name} (${event.payload.dispatch})`;
    case "tool.result":
      return `tool result (${event.payload.dispatch})`;
    case "mcp.request":
      return `mcp request ${event.payload.tool} (stub)`;
    case "mcp.response":
      return `mcp response ${event.payload.tool} (stub)`;
    case "session.open":
      return "session opened";
    case "session.close":
      return `session closed (${event.payload.reason})`;
    case "session.error":
      return `session error (${event.payload.reason})`;
    case "session.compaction":
      return "session compaction";
    default:
      return assertNever(event);
  }
}
