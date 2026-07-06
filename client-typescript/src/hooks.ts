import type { Message, ToolUse, ToolResult } from "./types.js";

/**
 * Optional harness customization points. Each hook receives the relevant,
 * mutable event and may inspect, mutate, or log it. Hooks may be async. A hook
 * that **throws** aborts the loop (surfaced as a {@link HookError}).
 *
 * Fire order per iteration of the loop:
 *   before_send → (POST) → after_receive →
 *   [ per tool_use:  before_tool_call → handler → after_tool_call ]
 */
export interface Hooks {
  /** Outgoing turn, just before it is POSTed. Mutations are sent. */
  before_send?: (message: Message) => void | Promise<void>;
  /** Assistant turn, immediately after it is received. */
  after_receive?: (message: Message) => void | Promise<void>;
  /** A `tool_use` request, before its handler is dispatched. */
  before_tool_call?: (toolUse: ToolUse) => void | Promise<void>;
  /**
   * A tool result, before it is sent back to the server. Rewriting
   * `toolResult.content` changes what the server receives.
   */
  after_tool_call?: (toolResult: ToolResult) => void | Promise<void>;
}

/** The four hook point names, for diagnostics. */
export type HookName = keyof Hooks;
