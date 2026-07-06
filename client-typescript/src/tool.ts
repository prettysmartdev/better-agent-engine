import type { Content } from "./types.js";

/**
 * A tool handler. Receives the `tool_use.input` object and returns the tool
 * result **content** (a string or an array of content blocks). The harness
 * wraps the return value in a `tool_result` block echoing `tool_use.id`.
 * May be sync or async.
 */
export type ToolHandler = (
  input: Record<string, unknown>,
) => Content | Promise<Content>;

/** A client-side tool: its wire declaration plus a callable handler. */
export interface ToolDefinition {
  /** Tool name; must appear in the profile's `allowed_tools`. */
  name: string;
  /** Human-readable description sent to the provider. */
  description: string;
  /** JSON Schema for the tool's input. */
  input_schema: Record<string, unknown>;
  /** Invoked when the server returns a `tool_use` block for this tool. */
  handler: ToolHandler;
}
