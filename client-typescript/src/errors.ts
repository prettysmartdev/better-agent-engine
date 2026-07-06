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
 * A `502` providers-failed outcome. Per the contract the body is the normal
 * `{message, events}` shape (not a problem doc); the session is now `error`.
 * Typically means a provider key was missing or the provider was unreachable.
 */
export class ProvidersFailedError extends BaeError {
  constructor(
    readonly assistantMessage: Message,
    readonly events: SessionEvent[],
  ) {
    super(
      "all providers failed (502) — check the profile's provider config / key",
    );
  }
}

/** The server returned a `tool_use` for a tool that was not registered. */
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
