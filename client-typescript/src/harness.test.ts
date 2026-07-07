import { describe, expect, it, vi } from "vitest";

import { Config } from "./config.js";
import {
  ApiError,
  HookError,
  ProvidersFailedError,
  RpcError,
  ToolError,
  UnknownToolError,
} from "./errors.js";
import { Harness } from "./harness.js";
import type {
  Transport,
  TransportRequest,
  TransportResponse,
} from "./transport.js";
import type {
  ContentBlock,
  JsonRpcFrame,
  JsonRpcRequest,
  McpRequestPayload,
  McpResponsePayload,
  SendMessageParams,
  SessionEvent,
} from "./types.js";
import { messageText } from "./types.js";

/**
 * A scripted, request-recording transport so the whole loop runs offline. REST
 * calls go through `request`; the JSON-RPC session loop goes through `stream`,
 * which yields the scripted NDJSON frames for that `/rpc` call. Both share the
 * `requests` log so a single `call` counter orders the whole exchange.
 */
class MockTransport implements Transport {
  readonly requests: TransportRequest[] = [];
  constructor(
    private readonly onRequest: (
      req: TransportRequest,
      call: number,
    ) => TransportResponse,
    private readonly onStream: (
      req: TransportRequest,
      call: number,
    ) => JsonRpcFrame[] = () => [],
  ) {}
  async request(req: TransportRequest): Promise<TransportResponse> {
    const call = this.requests.length;
    // Deep-copy so later mutation by the harness can't rewrite recorded bodies.
    this.requests.push(structuredClone(req));
    return this.onRequest(req, call);
  }
  async *stream(req: TransportRequest): AsyncIterable<JsonRpcFrame> {
    const call = this.requests.length;
    this.requests.push(structuredClone(req));
    for (const frame of this.onStream(req, call)) yield frame;
  }
}

const config = () =>
  new Config({
    serverUrl: "http://test",
    clientKey: "bae_test",
    clientVersion: "9.9.9",
  });

const openOk: TransportResponse = {
  status: 201,
  body: {
    session_id: "ses_1",
    session_key: "bae_ses_1",
    profile: {
      id: "pro_1",
      name: "main",
      allowed_tools: ["get_time"],
      mcp_servers: [],
      provider: { provider: "anthropic", model: "claude-sonnet-4-6" },
    },
  },
};

/** A single-frame NDJSON reply: the terminal `{message, events}` result. */
function assistantFrames(
  content: ContentBlock[],
  events: SessionEvent[] = [],
): JsonRpcFrame[] {
  return [
    {
      jsonrpc: "2.0",
      id: 1,
      result: { message: { role: "assistant", content }, events },
    },
  ];
}

const textTurn = assistantFrames([{ type: "text", text: "hello" }]);
const toolTurn = assistantFrames([
  { type: "tool_use", id: "tu_1", name: "get_time", input: { tz: "utc" } },
]);

describe("connect", () => {
  it("posts client_version + declared tools and returns a session bound to the profile", async () => {
    const transport = new MockTransport(() => openOk);
    const harness = new Harness(config(), { transport }).registerTool({
      name: "get_time",
      description: "the time",
      input_schema: { type: "object" },
      handler: () => "noop",
    });

    const session = await harness.connect();

    expect(session.id).toBe("ses_1");
    expect(session.profile.name).toBe("main");
    const req = transport.requests[0]!;
    expect(req).toMatchObject({
      method: "POST",
      path: "/api/v1/sessions",
      token: "bae_test",
    });
    expect(req.body).toEqual({
      client_version: "9.9.9",
      tools: [
        {
          name: "get_time",
          description: "the time",
          input_schema: { type: "object" },
        },
      ],
    });
  });

  it("maps a problem-doc error to ApiError with the stable slug", async () => {
    const transport = new MockTransport(() => ({
      status: 403,
      body: {
        type: "tool_not_allowed",
        title: "Tool not allowed",
        status: 403,
        detail: "get_time",
      },
    }));
    const harness = new Harness(config(), { transport });
    await expect(harness.connect()).rejects.toMatchObject({
      constructor: ApiError,
      type: "tool_not_allowed",
      status: 403,
    });
  });
});

describe("send — tool-call loop", () => {
  it("dispatches a tool_use to the registered handler and continues to the final turn", async () => {
    const handler = vi.fn(() => "12:00");
    const transport = new MockTransport(
      () => openOk,
      (_req, call) => (call === 1 ? toolTurn : textTurn),
    );
    const harness = new Harness(config(), { transport }).registerTool({
      name: "get_time",
      description: "the time",
      input_schema: {},
      handler,
    });

    const session = await harness.connect();
    const reply = await session.send("what time?");

    expect(messageText(reply)).toBe("hello");
    expect(handler).toHaveBeenCalledWith({ tz: "utc" });
    // Second /rpc call carries a session.sendMessage with the tool_result.
    const second = transport.requests[2]!;
    expect(second.path).toBe("/api/v1/sessions/ses_1/rpc");
    const envelope = second.body as JsonRpcRequest<SendMessageParams>;
    expect(envelope.method).toBe("session.sendMessage");
    expect(envelope.params.message.content).toEqual([
      { type: "tool_result", tool_use_id: "tu_1", content: "12:00" },
    ]);
  });

  it("returns immediately when the first turn has no tool_use", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => textTurn,
    );
    const session = await new Harness(config(), { transport }).connect();
    const reply = await session.send("hi");
    expect(messageText(reply)).toBe("hello");
    // open + exactly one /rpc call.
    expect(transport.requests).toHaveLength(2);
  });
});

describe("hooks", () => {
  it("fire in loop order and can mutate the outgoing/result payloads", async () => {
    const order: string[] = [];
    const transport = new MockTransport(
      () => openOk,
      (_req, call) => (call === 1 ? toolTurn : textTurn),
    );
    const harness = new Harness(config(), { transport })
      .registerTool({
        name: "get_time",
        description: "t",
        input_schema: {},
        handler: () => "raw",
      })
      .setHooks({
        before_send: () => void order.push("before_send"),
        after_receive: () => void order.push("after_receive"),
        before_tool_call: (tu) =>
          void order.push(`before_tool_call:${tu.name}`),
        after_tool_call: (tr) => {
          order.push("after_tool_call");
          tr.content = "rewritten"; // mutation must reach the wire
        },
      });

    const session = await harness.connect();
    await session.send("go");

    expect(order).toEqual([
      "before_send",
      "after_receive",
      "before_tool_call:get_time",
      "after_tool_call",
      "before_send",
      "after_receive",
    ]);
    const second = transport.requests[2]!;
    const envelope = second.body as JsonRpcRequest<SendMessageParams>;
    expect(envelope.params.message.content).toEqual([
      { type: "tool_result", tool_use_id: "tu_1", content: "rewritten" },
    ]);
  });

  it("aborts the loop when a hook throws, wrapping it in HookError", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => textTurn,
    );
    const session = await new Harness(config(), { transport })
      .setHooks({
        before_send: () => {
          throw new Error("boom");
        },
      })
      .connect();

    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: HookError,
      hook: "before_send",
    });
    // Hook threw before the messages POST, so only the open request was made.
    expect(transport.requests).toHaveLength(1);
  });
});

describe("send — error propagation", () => {
  it("throws UnknownToolError for an unregistered tool", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => toolTurn,
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: UnknownToolError,
      toolName: "get_time",
    });
  });

  it("wraps a throwing handler in ToolError", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => toolTurn,
    );
    const session = await new Harness(config(), { transport })
      .registerTool({
        name: "get_time",
        description: "t",
        input_schema: {},
        handler: () => {
          throw new Error("handler exploded");
        },
      })
      .connect();
    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: ToolError,
      toolName: "get_time",
    });
  });

  it("surfaces an all-providers-failed turn as ProvidersFailedError carrying the events", async () => {
    // The server no longer returns 502: the failure turn arrives as a normal
    // terminal result whose events include session.error/all_providers_failed.
    const events: SessionEvent[] = [
      {
        id: "evt_1",
        session_id: "ses_1",
        client_key_id: null,
        event_type: "session.error",
        payload: { reason: "all_providers_failed" },
        created_at: "t",
      },
    ];
    const transport = new MockTransport(
      () => openOk,
      () =>
        assistantFrames([{ type: "text", text: "provider unavailable" }], events),
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: ProvidersFailedError,
    });
    await session.send("hi").catch((e: ProvidersFailedError) => {
      expect(e.events).toHaveLength(1);
      expect(e.events[0]!.event_type).toBe("session.error");
    });
  });

  it("surfaces a JSON-RPC error object in the stream as RpcError", async () => {
    // session-not-open is a -32000 error frame (HTTP is still 200), not a 409.
    const transport = new MockTransport(
      () => openOk,
      () => [
        {
          jsonrpc: "2.0",
          id: 1,
          error: { code: -32000, message: "session is not open" },
        },
      ],
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: RpcError,
      code: -32000,
    });
  });
});

describe("close", () => {
  it("issues a DELETE with the session key", async () => {
    const transport = new MockTransport((_req, call) =>
      call === 0
        ? openOk
        : { status: 200, body: { session_id: "ses_1", state: "closed" } },
    );
    const session = await new Harness(config(), { transport }).connect();
    await session.close();
    const req = transport.requests[1]!;
    expect(req).toMatchObject({
      method: "DELETE",
      path: "/api/v1/sessions/ses_1",
      token: "bae_ses_1",
    });
  });
});

// ===========================================================================
// Cross-SDK MCP parity
//
// The three client SDKs (Rust, TypeScript, Python) must observe an IDENTICAL
// ordered live event sequence for the same scripted MCP-enabled turn, and must
// parse the real (non-stub) `mcp.request` / `mcp.response` payload shapes. The
// canonical sequence below MUST stay byte-for-byte identical to the arrays in:
//   - client-rust/src/mcp_parity.rs      (MCP_PARITY_SEQUENCE)
//   - client-python/tests/test_mcp_parity.py  (MCP_PARITY_SEQUENCE)
// ===========================================================================

/** The canonical live-notification sequence for the scripted MCP turn. */
const MCP_PARITY_SEQUENCE = [
  "provider.request",
  "provider.response",
  "tool.call",
  "mcp.request",
  "mcp.response",
  "tool.result",
  "provider.request",
  "provider.response",
  "server.message.send",
];

function parityEvent(
  event_type: string,
  payload: Record<string, unknown>,
): SessionEvent {
  return {
    id: `evt_${event_type}_${Math.random().toString(36).slice(2)}`,
    session_id: "ses_1",
    client_key_id: null,
    event_type,
    payload,
    created_at: "t",
  } as SessionEvent;
}

function parityNotification(event: SessionEvent): JsonRpcFrame {
  return { jsonrpc: "2.0", method: "session.event", params: event };
}

/** The scripted MCP turn: N live notifications, then the terminal text result. */
function mcpScenarioFrames(): JsonRpcFrame[] {
  const echoContent = [{ type: "text", text: "echo: x" }];
  const notifs = [
    parityEvent("provider.request", { attempt: 0 }),
    parityEvent("provider.response", { ok: true, status: 200 }),
    parityEvent("tool.call", {
      dispatch: "mcp",
      name: "remote_search",
      server_name: "echo",
      input: { q: "x" },
    }),
    parityEvent("mcp.request", {
      method: "tools/call",
      server_name: "echo",
      tool: "remote_search",
      input: { q: "x" },
    }),
    parityEvent("mcp.response", {
      server_name: "echo",
      ok: true,
      result: { content: echoContent, isError: false },
    }),
    parityEvent("tool.result", {
      tool_use_id: "tu_mcp",
      dispatch: "mcp",
      server_name: "echo",
      is_error: false,
      content: echoContent,
    }),
    parityEvent("provider.request", { attempt: 0 }),
    parityEvent("provider.response", { ok: true, status: 200 }),
    parityEvent("server.message.send", {
      role: "assistant",
      content: [{ type: "text", text: "after mcp" }],
    }),
  ].map(parityNotification);

  const terminal: JsonRpcFrame = {
    jsonrpc: "2.0",
    id: 1,
    result: {
      message: { role: "assistant", content: [{ type: "text", text: "after mcp" }] },
      events: [],
    },
  };
  return [...notifs, terminal];
}

describe("MCP cross-SDK parity", () => {
  it("observes the canonical MCP event sequence and parses real mcp payloads", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => mcpScenarioFrames(),
    );
    const observed: SessionEvent[] = [];
    const harness = new Harness(config(), { transport }).setHooks({
      on_event: (e) => {
        observed.push(e);
      },
    });
    const session = await harness.connect();

    // MCP tools are dispatched server-side, so `send` returns the final text
    // message after a single sendMessage call.
    const final = await session.send("search please");
    expect(messageText(final)).toBe("after mcp");

    // The live sequence is identical to the Rust/Python parity tests.
    expect(observed.map((e) => e.event_type)).toEqual(MCP_PARITY_SEQUENCE);

    // Real (non-stub) mcp.request / mcp.response payloads parse to their shapes.
    const req = observed.find((e) => e.event_type === "mcp.request")!;
    const reqP = req.payload as McpRequestPayload;
    expect(reqP.method).toBe("tools/call");
    expect(reqP.server_name).toBe("echo");
    expect(reqP.tool).toBe("remote_search");
    expect(reqP.input).toEqual({ q: "x" });

    const resp = observed.find((e) => e.event_type === "mcp.response")!;
    const respP = resp.payload as McpResponsePayload;
    expect(respP.ok).toBe(true);
    if (respP.ok) {
      expect(respP.result).toMatchObject({
        content: [{ type: "text", text: "echo: x" }],
      });
    }

    // No trace of the removed stub payload shape.
    expect(JSON.stringify(observed)).not.toContain('"status":"stub"');
  });
});
