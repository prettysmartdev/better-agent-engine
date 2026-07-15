import { Config } from "./config.js";
import {
  HookError,
  ProvidersFailedError,
  RpcError,
  ToolError,
  UnknownToolError,
} from "./errors.js";
import type { HookName, Hooks } from "./hooks.js";
import {
  SandboxSession,
  type ExecResult,
  type RemoteSandboxStarted,
  type RemoteSandboxStopped,
  type SandboxLifecycleState,
  type SandboxRpc,
  type SandboxTool,
  type SandboxToolDef,
} from "./sandbox.js";
import type { ToolDefinition } from "./tool.js";
import {
  SubagentSession,
  type LocalSubagentReport,
  type SubagentRpc,
  type SubagentTool,
  type SubagentToolDef,
} from "./subagent.js";
import {
  eventFromFrame,
  expectOk,
  FetchTransport,
  isTerminalFrame,
  type Transport,
} from "./transport.js";
import {
  messageToWire,
  toMessage,
  toolUses,
  type ContentBlock,
  type JsonRpcRequest,
  type Message,
  type Profile,
  type RpcMethod,
  type SendMessageResult,
  type SessionEvent,
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
  /** Late-bound sandbox handle, shared with sandbox tools and the Session. */
  private readonly sandbox = new SandboxSession();
  /** Auto-mode sandbox tool declarations, sent in the session-open list. */
  private readonly sandboxDefs: SandboxToolDef[] = [];
  /** Remote subagent declarations sent in the session-open body. */
  private readonly subagentDefs: SubagentToolDef[] = [];
  /** Late-bound local subagent handle shared with registered tools and Session. */
  private readonly subagent = new SubagentSession(this.sandbox);

  constructor(config: Config, options: HarnessOptions = {}) {
    this.config = config;
    this.transport = options.transport ?? new FetchTransport(config.serverUrl);
  }

  /** Register a client-side tool. Returns `this` for chaining. */
  registerTool(tool: ToolDefinition): this {
    this.tools.set(tool.name, tool);
    return this;
  }

  /**
   * A handle to this harness's sandbox capability, for building sandbox tools
   * **before** `connect()`. Its transport is late-bound at connect; see
   * {@link SandboxSession}. Pass it to `runShellCommand`/`runShellNamed`, then
   * register the result with {@link registerSandboxTool}.
   */
  sandboxSession(): SandboxSession {
    return this.sandbox;
  }

  /**
   * Register a builtin sandbox tool, routing it correctly: a client-dispatched
   * tool joins the ordinary registry; an Auto-mode declaration joins the
   * session-open `sandbox_tools` list. Returns `this` for chaining.
   */
  registerSandboxTool(tool: SandboxTool): this {
    if (tool.kind === "tool") {
      this.tools.set(tool.tool.name, tool.tool);
    } else {
      this.sandboxDefs.push(tool.def);
    }
    return this;
  }

  /** A handle for constructing local or remote subagent bindings before connect. */
  subagentSession(): SubagentSession {
    return this.subagent;
  }

  /** Register a builtin subagent tool, routing callable and declaration forms. */
  registerSubagentTool(tool: SubagentTool): this {
    if (tool.kind === "tool") {
      this.tools.set(tool.tool.name, tool.tool);
    } else {
      this.subagentDefs.push(tool.def);
    }
    return this;
  }

  /** Set the hook callbacks. Returns `this` for chaining. */
  setHooks(hooks: Hooks): this {
    this.hooks = hooks;
    return this;
  }

  /**
   * Open a new session: exchange the client key for a session id + key,
   * declaring the registered tools. Registers this connection as a driver
   * (`session.registerDriver`) before returning, so the first `send()` is
   * permitted. Returns a {@link Session} bound to that session key.
   */
  async connect(): Promise<Session> {
    return this.open("/api/v1/sessions");
  }

  /**
   * Join an **existing** session as an additional driver, returning a
   * {@link Session} shaped identically to {@link connect}'s.
   *
   * POSTs to `/api/v1/sessions/{sessionId}/join` with this harness's
   * `client_version` and registered tool declarations (a joining client
   * declares its own, independent tool set, validated against the *same*
   * profile's `allowed_tools`). The joining client key must resolve to the same
   * profile as the session, or the server rejects with `403 profile_mismatch`.
   * Like {@link connect}, registers this connection as a driver before returning.
   */
  async join(sessionId: string): Promise<Session> {
    return this.open(`/api/v1/sessions/${sessionId}/join`);
  }

  /**
   * Shared body of {@link connect} and {@link join}: POST the declared tools to
   * `path` with client-key auth, build the {@link Session}, then register it as
   * a driver before handing it back. Both endpoints return the identical
   * `{session_id, session_key, profile}` shape.
   */
  private async open(path: string): Promise<Session> {
    const body = {
      client_version: this.config.clientVersion,
      tools: [...this.tools.values()].map((t) => ({
        name: t.name,
        description: t.description,
        input_schema: t.input_schema,
      })),
      // Only present when Auto-mode sandbox tools are registered, so a session
      // without them sends the exact same body as before.
      ...(this.sandboxDefs.length > 0
        ? { sandbox_tools: this.sandboxDefs.map((d) => ({ ...d })) }
        : {}),
      ...(this.subagentDefs.length > 0
        ? { subagent_tools: this.subagentDefs.map((d) => ({ ...d })) }
        : {}),
    };
    const res = await this.transport.request({
      method: "POST",
      path,
      token: this.config.clientKey,
      body,
    });
    const open = expectOk(res) as OpenSessionResponse;
    // Register as a driver before any send: session.sendMessage requires it
    // (a -32001 error otherwise). Application code never calls this.
    await this.registerDriver(open.session_id, open.session_key);
    const session = new Session(
      this.transport,
      open.session_id,
      open.session_key,
      open.profile,
      this.tools,
      this.hooks,
      this.sandbox,
      this.subagent,
    );
    this.subagent.setBaseClientTools(
      [...this.tools.values()].map((t) => ({
        name: t.name,
        description: t.description,
        input_schema: t.input_schema,
      })),
    );
    if (this.subagent.hasLocal()) {
      session.registerTool(this.subagent.statusTool());
    }
    // Late-bind the sandbox transport now that the session exists, so any
    // sandbox tool built pre-connect can reach the remote RPC methods.
    this.sandbox.bind(session);
    this.subagent.bind(session);
    return session;
  }

  /**
   * Issue the one-time `session.registerDriver` JSON-RPC call over `…/rpc`,
   * consuming its single terminal `{registered:true}` frame. Kept private: the
   * harness owns driver registration; application code never triggers it.
   */
  private async registerDriver(
    sessionId: string,
    sessionKey: string,
  ): Promise<void> {
    const body: JsonRpcRequest = {
      jsonrpc: "2.0",
      id: 1,
      method: "session.registerDriver",
      params: {},
    };
    const frames = this.transport.stream({
      method: "POST",
      path: `/api/v1/sessions/${sessionId}/rpc`,
      token: sessionKey,
      body,
    });
    for await (const frame of frames) {
      if (frame.error)
        throw new RpcError(frame.error.code, frame.error.message);
      if (isTerminalFrame(frame)) break;
    }
  }
}

/**
 * A live session handle. `send()` drives the full round-trip — dispatching
 * server-returned tool calls to registered handlers and posting results back
 * until a non-tool-call assistant turn arrives — and `close()` ends it.
 */
export class Session implements SandboxRpc, SubagentRpc {
  /** Monotonic JSON-RPC request id, unique per session. */
  private nextRpcId = 1;

  constructor(
    private readonly transport: Transport,
    readonly id: string,
    private readonly sessionKey: string,
    readonly profile: Profile,
    private readonly tools: Map<string, ToolDefinition>,
    private readonly hooks: Hooks,
    private readonly sandbox: SandboxSession = new SandboxSession(),
    private readonly subagent: SubagentSession = new SubagentSession(sandbox),
  ) {}

  /**
   * A handle to this session's sandbox capability, for building sandbox tools
   * after connect (register the resulting client-dispatched tools with
   * {@link registerTool}). Auto-mode declarations must instead be registered on
   * the {@link Harness} before connect.
   */
  sandboxSession(): SandboxSession {
    return this.sandbox;
  }

  /** A handle to this session's local subagent capability. */
  subagentSession(): SubagentSession {
    return this.subagent;
  }

  /** Register an additional client-dispatched tool on the live session. */
  registerTool(tool: ToolDefinition): this {
    this.tools.set(tool.name, tool);
    return this;
  }

  /**
   * Run `command` in the session's remote sandbox (`session.execRemoteSandbox`).
   * Available to any tool handler or application code.
   */
  async execRemoteSandbox(command: string): Promise<ExecResult> {
    return this.sandboxRpc("session.execRemoteSandbox", {
      command,
    }) as Promise<ExecResult>;
  }

  /** Report a local sandbox lifecycle transition (`session.reportLocalSandbox`). */
  async reportLocalSandbox(
    state: SandboxLifecycleState,
    image: string | null,
    containerId: string | null,
    detail: string | null,
  ): Promise<void> {
    await this.sandboxRpc("session.reportLocalSandbox", {
      state,
      image,
      unsandboxed: image === null,
      container_id: containerId,
      detail,
    });
  }

  /** Report a local subagent lifecycle transition (`session.reportLocalSubagent`). */
  async reportLocalSubagent(report: LocalSubagentReport): Promise<void> {
    await this.sandboxRpc("session.reportLocalSubagent", report);
  }

  /** Replace this client's dynamic tool declarations for the next provider call. */
  async updateClientTools(
    tools: Array<Record<string, unknown>>,
  ): Promise<void> {
    await this.sandboxRpc("session.updateClientTools", { tools });
  }

  /** Cancel a server-tracked remote subagent through the RPC seam. */
  async cancelRemoteSubagent(subagentId: string): Promise<unknown> {
    return this.sandboxRpc("session.cancelSubagent", {
      subagent_id: subagentId,
    });
  }

  /** Cancel a local subagent in-process; terminal/unknown ids are no-ops. */
  async cancelSubagent(subagentId: string): Promise<void> {
    await this.subagent.cancelSubagent(subagentId);
  }

  /**
   * Eagerly start a local sandbox for `image` (otherwise it starts lazily on the
   * first local-target tool call), reporting `running` to the server.
   */
  async startLocalSandbox(image: string): Promise<void> {
    await this.sandbox.startLocal(image);
  }

  /** Stop every local sandbox this session started, reporting `stopped`. */
  async stopLocalSandbox(): Promise<void> {
    await this.sandbox.stopAllLocal();
  }

  /**
   * Ask the server to start this session's **remote** sandbox from `image`
   * (`session.startRemoteSandbox`). `image` must be in the session profile's
   * `available_sandboxes`, or the call rejects with an {@link RpcError} code
   * `-32011`. One sandbox per session: a second start while one is running
   * rejects with `-32000`. Required before any `Remote`-target tool
   * (Auto-dispatched or {@link execRemoteSandbox}) can run.
   */
  async startRemoteSandbox(image: string): Promise<RemoteSandboxStarted> {
    return this.sandboxRpc("session.startRemoteSandbox", {
      image,
    }) as Promise<RemoteSandboxStarted>;
  }

  /**
   * Stop this session's remote sandbox (`session.stopRemoteSandbox`). Rejects
   * with an {@link RpcError} code `-32013` if none is running. (Session close
   * also stops a still-running remote sandbox server-side.)
   */
  async stopRemoteSandbox(): Promise<RemoteSandboxStopped> {
    return this.sandboxRpc(
      "session.stopRemoteSandbox",
      {},
    ) as Promise<RemoteSandboxStopped>;
  }

  /**
   * Issue one non-turn sandbox RPC (`execRemoteSandbox`/`reportLocalSandbox`)
   * over `…/rpc` and return its terminal `result`. Shaped like `registerDriver`:
   * a single synchronous utility call, no session-loop involvement.
   */
  private async sandboxRpc(
    method: RpcMethod,
    params: unknown,
  ): Promise<unknown> {
    const frames = this.transport.stream({
      method: "POST",
      path: `/api/v1/sessions/${this.id}/rpc`,
      token: this.sessionKey,
      body: this.rpcRequest(method, params),
    });
    for await (const frame of frames) {
      if (frame.error) {
        throw new RpcError(frame.error.code, frame.error.message);
      }
      if (isTerminalFrame(frame)) return frame.result;
    }
    throw new RpcError(-32603, "stream ended without a terminal response");
  }

  /**
   * Send a turn and drive the loop to completion. Returns the final assistant
   * message (one with no `tool_use` blocks).
   *
   * Each turn is a `session.sendMessage` JSON-RPC call over `…/rpc`: live
   * `session.event` notifications are handed to the `on_event` hook, and the
   * terminal `{message, events}` result drives the loop. When a `tool_use`
   * block's `dispatch` is `"client"` (or, for an older server that omits
   * `dispatch`, when its name is in this harness's registered-tool set), the
   * harness executes it and returns its `tool_result`. Every other block is
   * server-owned: it is never executed and never receives a synthesized
   * result, but the full assistant message — including server-owned blocks —
   * is still passed to `after_receive` so applications can surface it.
   */
  async send(input: string | Message): Promise<Message> {
    let message = toMessage(input);

    for (;;) {
      await this.runHook("before_send", (h) => h(message));

      const { result, notifications } = await this.sendMessage(message);
      for (const event of notifications) {
        await this.runHook("on_event", (h) => h(event));
      }
      const assistant = result.message;

      await this.runHook("after_receive", (h) => h(assistant));

      const uses = toolUses(assistant);
      if (uses.length === 0) {
        return assistant;
      }

      message = { role: "user", content: await this.dispatchTools(uses) };
    }
  }

  /**
   * Subscribe to this session's live `session.event` feed via
   * `session.subscribe`, invoking `handler` for each event in order. With
   * `sinceEventId`, the server first replays persisted events after that id,
   * then streams live ones **indefinitely**.
   *
   * The stream is open-ended: return `false` from `handler` to stop reading
   * (dropping the connection ends the subscription server-side), or call
   * {@link unsubscribe} from elsewhere. Resolves once the stream ends.
   */
  async subscribe(
    handler: (event: SessionEvent) => boolean | void | Promise<boolean | void>,
    opts: { sinceEventId?: string } = {},
  ): Promise<void> {
    const params =
      opts.sinceEventId !== undefined
        ? { since_event_id: opts.sinceEventId }
        : {};
    const frames = this.transport.stream({
      method: "POST",
      path: `/api/v1/sessions/${this.id}/rpc`,
      token: this.sessionKey,
      body: this.rpcRequest("session.subscribe", params),
    });
    for await (const frame of frames) {
      if (frame.error)
        throw new RpcError(frame.error.code, frame.error.message);
      if (isTerminalFrame(frame)) break;
      const event = eventFromFrame(frame);
      if (event !== null && (await handler(event)) === false) break;
    }
  }

  /** End any active {@link subscribe} streams for this session (`session.unsubscribe`). */
  async unsubscribe(): Promise<void> {
    const frames = this.transport.stream({
      method: "POST",
      path: `/api/v1/sessions/${this.id}/rpc`,
      token: this.sessionKey,
      body: this.rpcRequest("session.unsubscribe", {}),
    });
    for await (const frame of frames) {
      if (frame.error)
        throw new RpcError(frame.error.code, frame.error.message);
      if (isTerminalFrame(frame)) break;
    }
  }

  /**
   * Close the session (appends a `session.close` event server-side). Before
   * releasing it, stops any still-running **local** sandboxes this session
   * started — reporting `stopped` for each — mirroring how the server stops its
   * own remote sandbox at session close.
   */
  async close(): Promise<void> {
    await this.subagent.closeAll();
    await this.sandbox.stopAllLocal();
    const res = await this.transport.request({
      method: "DELETE",
      path: `/api/v1/sessions/${this.id}`,
      token: this.sessionKey,
    });
    expectOk(res);
  }

  /**
   * Drive one `session.sendMessage` turn: stream the NDJSON reply, collecting
   * `session.event` notifications and resolving on the terminal frame. An
   * all-providers-failed turn surfaces as {@link ProvidersFailedError}; a
   * JSON-RPC error object as {@link RpcError}.
   */
  private async sendMessage(
    message: Message,
  ): Promise<{ result: SendMessageResult; notifications: SessionEvent[] }> {
    const frames = this.transport.stream({
      method: "POST",
      path: `/api/v1/sessions/${this.id}/rpc`,
      token: this.sessionKey,
      body: this.rpcRequest("session.sendMessage", {
        message: messageToWire(message),
      }),
    });

    const notifications: SessionEvent[] = [];
    for await (const frame of frames) {
      if (isTerminalFrame(frame)) {
        if (frame.error) {
          throw new RpcError(frame.error.code, frame.error.message);
        }
        const result = frame.result as SendMessageResult;
        if (providersFailed(result.events)) {
          throw new ProvidersFailedError(result.message, result.events);
        }
        return { result, notifications };
      }
      if (frame.error) {
        throw new RpcError(frame.error.code, frame.error.message);
      }
      const event = eventFromFrame(frame);
      if (event !== null) notifications.push(event);
    }
    throw new RpcError(-32603, "stream ended without a terminal response");
  }

  private rpcRequest(method: RpcMethod, params: unknown): JsonRpcRequest {
    return { jsonrpc: "2.0", id: this.nextRpcId++, method, params };
  }

  /**
   * Dispatch each client-owned `tool_use` to its handler, producing
   * `tool_result` blocks only for those. A block is client-owned when its
   * `dispatch` is `"client"`, or — for an older server that omits `dispatch`
   * — when its name is in the registered-tool set. Every other block
   * (`dispatch` `"sandbox"`/`"mcp"`/anything else) is server-owned: the
   * server already dispatched and answered it, so it is skipped here without
   * running any hook or producing a result.
   */
  private async dispatchTools(uses: ToolUse[]): Promise<ContentBlock[]> {
    const blocks: ContentBlock[] = [];
    for (const use of uses) {
      // `dispatch` is authoritative whenever a current server supplies it: a
      // client/MCP name collision must still go to the side the server
      // selected. Older servers omit it, so retain registry-membership
      // routing as the compatibility fallback.
      const ownedByClient =
        use.dispatch == null
          ? this.tools.has(use.name)
          : use.dispatch === "client";
      if (!ownedByClient) {
        // The complete assistant message, including this call, was already
        // exposed via after_receive for UI/observability. Server-owned calls
        // must not run client hooks or handlers and must not receive a
        // synthesized tool_result.
        continue;
      }

      await this.runHook("before_tool_call", (h) => h(use));

      const tool = this.tools.get(use.name);
      if (tool === undefined) {
        // Reachable only for client-owned calls: a dispatch:"client" request
        // can reveal a stale local declaration/handler mismatch, while a
        // server-owned request never reaches here.
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

/**
 * Does this turn's event list mark an all-providers-failed outcome? The server
 * no longer returns a `502`: the failure turn arrives as a normal terminal
 * result, distinguished only by a `session.error`/`all_providers_failed` event.
 */
function providersFailed(events: SessionEvent[]): boolean {
  return events.some(
    (e) =>
      e.event_type === "session.error" &&
      e.payload.reason === "all_providers_failed",
  );
}
