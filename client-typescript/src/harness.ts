import { Config } from "./config.js";
import { HookError, ToolError, UnknownToolError } from "./errors.js";
import type { HookName, Hooks } from "./hooks.js";
import type { ToolDefinition } from "./tool.js";
import {
  expectOk,
  FetchTransport,
  parseMessagesResponse,
  type Transport,
} from "./transport.js";
import {
  toMessage,
  toolUses,
  type ContentBlock,
  type Message,
  type Profile,
  type ToolResult,
  type ToolUse,
} from "./types.js";

/** Options for constructing a {@link Harness}. */
export interface HarnessOptions {
  /** Override the HTTP transport (used by tests to run offline). */
  transport?: Transport;
}

/** Body returned by `POST /api/v1/sessions`. */
interface OpenSessionResponse {
  session_id: string;
  session_key: string;
  profile: Profile;
}

/**
 * An agent harness: holds a {@link Config}, a registry of client-side tools,
 * and optional {@link Hooks}. `connect()` opens a session and returns a
 * {@link Session} that drives the tool-call loop.
 */
export class Harness {
  private readonly config: Config;
  private readonly transport: Transport;
  private readonly tools = new Map<string, ToolDefinition>();
  private hooks: Hooks = {};

  constructor(config: Config, options: HarnessOptions = {}) {
    this.config = config;
    this.transport = options.transport ?? new FetchTransport(config.serverUrl);
  }

  /** Register a client-side tool. Returns `this` for chaining. */
  registerTool(tool: ToolDefinition): this {
    this.tools.set(tool.name, tool);
    return this;
  }

  /** Set the hook callbacks. Returns `this` for chaining. */
  setHooks(hooks: Hooks): this {
    this.hooks = hooks;
    return this;
  }

  /**
   * Open a session: exchange the client key for a session id + key, declaring
   * the registered tools. Returns a {@link Session} bound to that session key.
   */
  async connect(): Promise<Session> {
    const body = {
      client_version: this.config.clientVersion,
      tools: [...this.tools.values()].map((t) => ({
        name: t.name,
        description: t.description,
        input_schema: t.input_schema,
      })),
    };
    const res = await this.transport.request({
      method: "POST",
      path: "/api/v1/sessions",
      token: this.config.clientKey,
      body,
    });
    const open = expectOk(res) as OpenSessionResponse;
    return new Session(
      this.transport,
      open.session_id,
      open.session_key,
      open.profile,
      this.tools,
      this.hooks,
    );
  }
}

/**
 * A live session handle. `send()` drives the full round-trip — dispatching
 * server-returned tool calls to registered handlers and posting results back
 * until a non-tool-call assistant turn arrives — and `close()` ends it.
 */
export class Session {
  constructor(
    private readonly transport: Transport,
    readonly id: string,
    private readonly sessionKey: string,
    readonly profile: Profile,
    private readonly tools: Map<string, ToolDefinition>,
    private readonly hooks: Hooks,
  ) {}

  /**
   * Send a turn and drive the loop to completion. Returns the final assistant
   * message (one with no `tool_use` blocks).
   */
  async send(input: string | Message): Promise<Message> {
    let message = toMessage(input);

    for (;;) {
      await this.runHook("before_send", (h) => h(message));

      const res = await this.transport.request({
        method: "POST",
        path: `/api/v1/sessions/${this.id}/messages`,
        token: this.sessionKey,
        body: { message },
      });
      const assistant = parseMessagesResponse(res).message;

      await this.runHook("after_receive", (h) => h(assistant));

      const uses = toolUses(assistant);
      if (uses.length === 0) {
        return assistant;
      }

      message = { role: "user", content: await this.dispatchTools(uses) };
    }
  }

  /** Close the session (appends a `session.close` event server-side). */
  async close(): Promise<void> {
    const res = await this.transport.request({
      method: "DELETE",
      path: `/api/v1/sessions/${this.id}`,
      token: this.sessionKey,
    });
    expectOk(res);
  }

  /** Dispatch each `tool_use` to its handler, producing `tool_result` blocks. */
  private async dispatchTools(uses: ToolUse[]): Promise<ContentBlock[]> {
    const blocks: ContentBlock[] = [];
    for (const use of uses) {
      await this.runHook("before_tool_call", (h) => h(use));

      const tool = this.tools.get(use.name);
      if (tool === undefined) {
        throw new UnknownToolError(use.name);
      }

      let content;
      try {
        content = await tool.handler(use.input);
      } catch (cause) {
        throw new ToolError(use.name, cause);
      }

      const result: ToolResult = {
        tool_use_id: use.id,
        name: use.name,
        content,
      };
      await this.runHook("after_tool_call", (h) => h(result));

      blocks.push({
        type: "tool_result",
        tool_use_id: result.tool_use_id,
        content: result.content,
      });
    }
    return blocks;
  }

  /**
   * Invoke a hook if registered, wrapping any throw in a {@link HookError} so a
   * failing hook aborts the loop with a clear origin.
   */
  private async runHook<K extends HookName>(
    name: K,
    call: (hook: NonNullable<Hooks[K]>) => void | Promise<void>,
  ): Promise<void> {
    const hook = this.hooks[name];
    if (hook === undefined) return;
    try {
      await call(hook as NonNullable<Hooks[K]>);
    } catch (cause) {
      throw new HookError(name, cause);
    }
  }
}
