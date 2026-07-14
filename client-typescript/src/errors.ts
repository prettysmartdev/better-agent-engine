import type { HookName } from "./hooks.js";
import type { Message, SessionEvent } from "./types.js";

/** Base class for every error thrown by the SDK. */
export class BaeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/**
 * A non-2xx response carrying an RFC 7807 problem document. `type` is the
 * stable slug to match on (e.g. `unauthorized`, `tool_not_allowed`).
 */
export class ApiError extends BaeError {
  constructor(
    readonly type: string,
    readonly title: string,
    readonly status: number,
    readonly detail: string,
  ) {
    super(`${status} ${type}: ${detail || title}`);
  }

  /** Build from a parsed problem-doc body, tolerating missing fields. */
  static fromBody(status: number, body: unknown): ApiError {
    const doc = (body ?? {}) as Record<string, unknown>;
    return new ApiError(
      typeof doc.type === "string" ? doc.type : "unknown",
      typeof doc.title === "string" ? doc.title : "Error",
      typeof doc.status === "number" ? doc.status : status,
      typeof doc.detail === "string" ? doc.detail : "",
    );
  }
}

/**
 * A providers-failed outcome: every provider (primary + fallbacks) failed
 * server-side during a `session.sendMessage` turn, and the session is now
 * `error`. The `/rpc` loop delivers this as a normal terminal `{message,
 * events}` result (not a 502 or a JSON-RPC error); the harness recognises the
 * `session.error`/`all_providers_failed` event in the turn and surfaces it here
 * for continuity. Typically a provider key was missing or the provider was
 * unreachable.
 */
export class ProvidersFailedError extends BaeError {
  constructor(
    readonly assistantMessage: Message,
    readonly events: SessionEvent[],
  ) {
    super("all providers failed — check the profile's provider config / key");
  }
}

/**
 * The `/rpc` stream carried a JSON-RPC 2.0 error object (HTTP was still `200`).
 * Reserved for parse / invalid-request / method-not-found / invalid-params /
 * internal errors and `-32000` application errors (session-not-open,
 * profile-unavailable-mid-session, `lagged`). Distinct from {@link ApiError},
 * which is a pre-stream HTTP/RFC-7807 failure (e.g. auth).
 */
export class RpcError extends BaeError {
  constructor(
    readonly code: number,
    readonly rpcMessage: string,
  ) {
    super(`JSON-RPC error ${code}: ${rpcMessage}`);
  }
}

/**
 * A client-owned tool call has no local handler. This can only occur for a
 * `dispatch: "client"` block (or an older untagged block selected by the
 * local registry) — server-owned `sandbox`/`mcp` blocks never raise this
 * error.
 */
export class UnknownToolError extends BaeError {
  constructor(readonly toolName: string) {
    super(`no handler registered for tool "${toolName}"`);
  }
}

/** A registered tool handler threw. */
export class ToolError extends BaeError {
  constructor(
    readonly toolName: string,
    readonly cause: unknown,
  ) {
    super(`tool "${toolName}" handler failed: ${describeCause(cause)}`);
  }
}

/** A hook threw, aborting the loop. */
export class HookError extends BaeError {
  constructor(
    readonly hook: HookName,
    readonly cause: unknown,
  ) {
    super(`hook "${hook}" failed: ${describeCause(cause)}`);
  }
}

/** A transport-level failure (network error, non-JSON body, etc.). */
export class TransportError extends BaeError {}

function describeCause(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
