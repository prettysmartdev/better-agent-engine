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
      /**
       * Server-selected owner for this invocation. This receive-only routing
       * tag is omitted by older servers; see {@link ToolUse.dispatch}.
       */
      dispatch?: string | null;
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
  /**
   * Server-selected owner for this invocation, when supplied. `"client"`
   * means this harness must execute it; every other present value is
   * server-owned and informational. When absent, the harness falls back to
   * local registry membership for compatibility with older servers.
   *
   * The full assistant {@link Message} (including server-owned calls) is
   * passed to `Hooks.after_receive`, allowing an application or UI to
   * display informational calls without executing them.
   */
  dispatch?: string | null;
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

/** Clone a message for outbound transport, removing receive-only routing tags. */
export function messageToWire(message: Message): Message {
  if (typeof message.content === "string") return { ...message };
  return {
    ...message,
    content: message.content.map((block) => {
      if (block.type !== "tool_use") return block;
      const { dispatch: _dispatch, ...wireBlock } = block;
      return wireBlock;
    }),
  };
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
    .map((b) => ({
      id: b.id,
      name: b.name,
      input: b.input,
      dispatch: b.dispatch,
    }));
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

/** The closed set of 27 event type strings. */
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
  | "session.join"
  | "session.driver.register"
  | "session.close"
  | "session.error"
  | "session.compaction"
  | "session.sandbox.available"
  | "session.sandbox.start"
  | "session.sandbox.running"
  | "session.sandbox.stop"
  | "session.sandbox.stopped"
  | "session.sandbox.error"
  | "sandbox.request"
  | "sandbox.response"
  | "session.subagent.start"
  | "session.subagent.running"
  | "session.subagent.completed"
  | "session.subagent.failed"
  | "session.subagent.cancelled";

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
  dispatch: "client" | "sandbox" | "mcp" | "subagent";
}
export interface ToolResultPayload {
  tool_use_id: string;
  dispatch: "client" | "sandbox" | "mcp" | "subagent";
  content: unknown;
  /** Present on `mcp`-dispatched results: the server that produced it. */
  server_name?: string | null;
  /** Present on `mcp`/`sandbox`-dispatched results: whether the call errored. */
  is_error?: boolean;
}
/** Payload of an `mcp.request` event: the engine calling a configured server. */
export interface McpRequestPayload {
  /** The MCP method invoked (currently always `"tools/call"`). */
  method: string;
  /** The server the call was routed to, or null if the tool was unroutable. */
  server_name: string | null;
  /** The requested tool name. */
  tool: string;
  /** The JSON arguments passed to the tool. */
  input: Record<string, unknown>;
}
/** Payload of an `mcp.response` event. `ok` discriminates success vs failure. */
export type McpResponsePayload =
  | {
      server_name: string | null;
      ok: true;
      /** The MCP `result` object (`{content, isError?}`). */
      result: Record<string, unknown>;
    }
  | {
      server_name: string | null;
      ok: false;
      /** The error description. */
      error: string;
    };
export interface SessionOpenPayload {
  client_version: string | null;
  tools: string[];
  /** Names of the client's Auto-mode sandbox tools (server-dispatched). */
  sandbox_tools: string[];
  /** Names of the client's remote subagent launch declarations. */
  subagent_tools?: string[];
}
/**
 * Payload of a `session.join` event: a second (or further) client key minted a
 * session key for an existing session via `POST …/join`. Same shape as
 * `session.open`; the joining client is the event's `client_key_id`.
 */
export interface SessionJoinPayload {
  client_version: string | null;
  tools: string[];
  /** Names of the client's Auto-mode sandbox tools (server-dispatched). */
  sandbox_tools: string[];
  /** Names of the client's remote subagent launch declarations. */
  subagent_tools?: string[];
}
/**
 * Payload of a `session.driver.register` event: a client key registered as a
 * driver via `session.registerDriver`. The actor is the event's
 * `client_key_id`; the payload itself carries no fields.
 */
export type SessionDriverRegisterPayload = Record<string, never>;
export interface SessionClosePayload {
  reason: "client_close" | "client_key_revoked";
}
export interface SessionErrorPayload {
  reason:
    | "provider_config"
    | "provider_call_failed"
    | "all_providers_failed"
    | "primary_provider_unavailable"
    | "driver_turn_abandoned"
    | "loop_limit"
    | "profile_unavailable";
  [key: string]: unknown;
}
export interface SessionCompactionPayload {
  [key: string]: unknown;
}

// --- Sandbox lifecycle + dispatch payloads (§ sandboxes guide) --------------

/**
 * Payload of a `session.sandbox.available` event: the driver-connect
 * notification listing the session's own profile's declared images and each
 * one's provisioning status (never any other profile's).
 */
export interface SandboxAvailablePayload {
  images: {
    name: string;
    status: "pending" | "available" | "error";
    /** Present only on `error` entries. */
    detail?: string;
  }[];
}
/** Payload of a `session.sandbox.start` event (server-authored, remote only). */
export interface SandboxStartPayload {
  image: string;
  dispatch: "remote";
}
/**
 * Payload of a `session.sandbox.running`/`session.sandbox.stopped`/
 * `session.sandbox.error` event — `dispatch` discriminates the server-authored
 * remote lifecycle from client-reported (`session.reportLocalSandbox`,
 * unverified telemetry) local lifecycle.
 */
export type SandboxLifecyclePayload =
  | {
      dispatch: "remote";
      image: string;
      /** Remote execution is always containerized. */
      unsandboxed: false;
      /** Absent on a `phase: "start"` error (no container was created). */
      sandbox_id?: string;
      /** Present on `session.sandbox.error`: the driver call that failed. */
      phase?: "start" | "stop" | "exec";
      /** Present on `session.sandbox.error`: the failure message. */
      detail?: string;
      /** Present on stop-initiated events (`stopped`). */
      reason?: "explicit" | "session_close";
    }
  | {
      dispatch: "local";
      image: string | null;
      /** True when the command ran directly on the harness host. */
      unsandboxed: boolean;
      container_id: string | null;
      detail: string | null;
    };
/** Payload of a `session.sandbox.stop` event (server-authored, remote only). */
export interface SandboxStopPayload {
  image: string;
  sandbox_id: string;
  reason: "explicit" | "session_close";
  dispatch: "remote";
}
/** Payload of a `sandbox.request` event: one Auto-mode dispatch in run_turn. */
export interface SandboxRequestPayload {
  tool: string;
  input: Record<string, unknown>;
  /** The `input.command` string the server will exec, or null if missing. */
  command: string | null;
}
/** Payload of a `sandbox.response` event. `ok` discriminates success vs failure. */
export type SandboxResponsePayload =
  | {
      sandbox_id: string;
      ok: boolean;
      /** The raw exec result (`ok` is false when `exit_code` is non-zero). */
      result: { stdout: string; stderr: string; exit_code: number };
    }
  | {
      /** Null when no remote sandbox was started or `command` was missing. */
      sandbox_id: string | null;
      ok: false;
      error: string;
    };

/** Common payload carried by every local/remote subagent lifecycle event. */
export interface SubagentCommonPayload {
  dispatch: "local" | "remote";
  subagent_id: string;
  harness: string;
  model: string;
  detail: string | null;
}
export type SubagentStartPayload = SubagentCommonPayload;
export type SubagentRunningPayload = SubagentCommonPayload;
export type SubagentCompletedPayload = SubagentCommonPayload & {
  exit_code: number;
};
export type SubagentFailedPayload = SubagentCommonPayload & {
  reason: "nonzero_exit" | "spawn_failed" | "timeout";
  exit_code: number | null;
};
export type SubagentCancelledPayload = SubagentCommonPayload & {
  reason: "explicit" | "session_close";
};

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
  "session.join": SessionJoinPayload;
  "session.driver.register": SessionDriverRegisterPayload;
  "session.close": SessionClosePayload;
  "session.error": SessionErrorPayload;
  "session.compaction": SessionCompactionPayload;
  "session.sandbox.available": SandboxAvailablePayload;
  "session.sandbox.start": SandboxStartPayload;
  "session.sandbox.running": SandboxLifecyclePayload;
  "session.sandbox.stop": SandboxStopPayload;
  "session.sandbox.stopped": SandboxLifecyclePayload;
  "session.sandbox.error": SandboxLifecyclePayload;
  "sandbox.request": SandboxRequestPayload;
  "sandbox.response": SandboxResponsePayload;
  "session.subagent.start": SubagentStartPayload;
  "session.subagent.running": SubagentRunningPayload;
  "session.subagent.completed": SubagentCompletedPayload;
  "session.subagent.failed": SubagentFailedPayload;
  "session.subagent.cancelled": SubagentCancelledPayload;
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
      return `mcp request ${event.payload.tool} → ${event.payload.server_name ?? "<unrouted>"}`;
    case "mcp.response":
      return `mcp response from ${event.payload.server_name ?? "<unrouted>"} (ok=${event.payload.ok})`;
    case "session.open":
      return "session opened";
    case "session.join":
      return "driver joined the session";
    case "session.driver.register":
      return "driver registered";
    case "session.close":
      return `session closed (${event.payload.reason})`;
    case "session.error":
      return `session error (${event.payload.reason})`;
    case "session.compaction":
      return "session compaction";
    case "session.sandbox.available":
      return `sandbox images available (${event.payload.images.length})`;
    case "session.sandbox.start":
      return `sandbox start requested (${event.payload.image})`;
    case "session.sandbox.running":
      return `sandbox running (${event.payload.image}, ${event.payload.dispatch})`;
    case "session.sandbox.stop":
      return `sandbox stop requested (${event.payload.reason})`;
    case "session.sandbox.stopped":
      return `sandbox stopped (${event.payload.image}, ${event.payload.dispatch})`;
    case "session.sandbox.error":
      return `sandbox error (${event.payload.image}, ${event.payload.dispatch})`;
    case "sandbox.request":
      return `sandbox request ${event.payload.tool}`;
    case "sandbox.response":
      return `sandbox response (ok=${event.payload.ok})`;
    case "session.subagent.start":
      return `subagent start (${event.payload.harness}, ${event.payload.dispatch})`;
    case "session.subagent.running":
      return `subagent running (${event.payload.harness}, ${event.payload.dispatch})`;
    case "session.subagent.completed":
      return `subagent completed (${event.payload.harness}, ${event.payload.dispatch})`;
    case "session.subagent.failed":
      return `subagent failed (${event.payload.reason}, ${event.payload.dispatch})`;
    case "session.subagent.cancelled":
      return `subagent cancelled (${event.payload.reason}, ${event.payload.dispatch})`;
    default:
      return assertNever(event);
  }
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 envelopes for the session loop (`POST …/rpc`)
//
// The management routes (session open/close, events replay) stay plain REST;
// only the message loop is JSON-RPC. A request is POSTed to
// `POST /api/v1/sessions/{id}/rpc` and the reply is an `application/x-ndjson`
// stream of these envelopes. A frame with no `id` is a notification (its
// `params` carry a `session.event`); the frame carrying the request `id` is the
// terminal response (`result` on success, `error` on failure).
// ---------------------------------------------------------------------------

/** JSON-RPC methods the session loop understands. */
export type RpcMethod =
  | "session.sendMessage"
  | "session.registerDriver"
  | "session.subscribe"
  | "session.unsubscribe"
  | "session.execRemoteSandbox"
  | "session.reportLocalSandbox"
  | "session.reportLocalSubagent"
  | "session.cancelSubagent"
  | "session.updateClientTools"
  | "session.startRemoteSandbox"
  | "session.stopRemoteSandbox";

/** A JSON-RPC 2.0 request envelope. */
export interface JsonRpcRequest<P = unknown> {
  jsonrpc: "2.0";
  /** Correlation id echoed back on the terminal response. */
  id: number;
  method: RpcMethod;
  params: P;
}

/** A JSON-RPC 2.0 error object (terminal, or a mid-stream notice like `lagged`). */
export interface JsonRpcErrorObject {
  code: number;
  message: string;
  data?: unknown;
}

/**
 * A single JSON-RPC 2.0 frame decoded from the NDJSON stream. Branch on `id`:
 * a frame with no `id` is a notification (`method`/`params`, e.g. a
 * `session.event`); the frame with the request `id` is the terminal response
 * (`result` or `error`).
 */
export interface JsonRpcFrame {
  jsonrpc?: "2.0";
  id?: number | string | null;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: JsonRpcErrorObject;
}

/** Params for `session.sendMessage`. */
export interface SendMessageParams {
  message: Message;
}

/** Params for `session.subscribe`. */
export interface SubscribeParams {
  since_event_id?: string;
}

/**
 * The terminal `result` of a `session.sendMessage` call — the same
 * `{message, events}` body the legacy synchronous message route returned.
 * `events` is the full turn event list; the live `session.event` notifications
 * are an additive, filtered subset of it.
 */
export interface SendMessageResult {
  message: Message;
  events: SessionEvent[];
}
