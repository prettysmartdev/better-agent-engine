import { describe, expect, it, vi } from "vitest";

import { Config } from "./config.js";
import {
  ApiError,
  HookError,
  ProvidersFailedError,
  ToolError,
  UnknownToolError,
} from "./errors.js";
import { Harness } from "./harness.js";
import type {
  Transport,
  TransportRequest,
  TransportResponse,
} from "./transport.js";
import type { ContentBlock, Message } from "./types.js";
import { messageText } from "./types.js";

/** A scripted, request-recording transport so the whole loop runs offline. */
class MockTransport implements Transport {
  readonly requests: TransportRequest[] = [];
  constructor(
    private readonly script: (
      req: TransportRequest,
      call: number,
    ) => TransportResponse,
  ) {}
  async request(req: TransportRequest): Promise<TransportResponse> {
    const call = this.requests.length;
    // Deep-copy so later mutation by the harness can't rewrite recorded bodies.
    this.requests.push(structuredClone(req));
    return this.script(req, call);
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

function assistant(content: ContentBlock[]): TransportResponse {
  return {
    status: 200,
    body: { message: { role: "assistant", content }, events: [] },
  };
}

const textTurn = assistant([{ type: "text", text: "hello" }]);
const toolTurn = assistant([
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
    const transport = new MockTransport((_req, call) => {
      if (call === 0) return openOk;
      if (call === 1) return toolTurn;
      return textTurn;
    });
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
    // Second messages POST carries the tool_result echoing tu_1.
    const second = transport.requests[2]!;
    expect(second.path).toBe("/api/v1/sessions/ses_1/messages");
    expect((second.body as { message: Message }).message.content).toEqual([
      { type: "tool_result", tool_use_id: "tu_1", content: "12:00" },
    ]);
  });

  it("returns immediately when the first turn has no tool_use", async () => {
    const transport = new MockTransport((_req, call) =>
      call === 0 ? openOk : textTurn,
    );
    const session = await new Harness(config(), { transport }).connect();
    const reply = await session.send("hi");
    expect(messageText(reply)).toBe("hello");
    // open + exactly one messages POST.
    expect(transport.requests).toHaveLength(2);
  });
});

describe("hooks", () => {
  it("fire in loop order and can mutate the outgoing/result payloads", async () => {
    const order: string[] = [];
    const transport = new MockTransport((_req, call) => {
      if (call === 0) return openOk;
      if (call === 1) return toolTurn;
      return textTurn;
    });
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
    expect((second.body as { message: Message }).message.content).toEqual([
      { type: "tool_result", tool_use_id: "tu_1", content: "rewritten" },
    ]);
  });

  it("aborts the loop when a hook throws, wrapping it in HookError", async () => {
    const transport = new MockTransport((_req, call) =>
      call === 0 ? openOk : textTurn,
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
    const transport = new MockTransport((_req, call) =>
      call === 0 ? openOk : toolTurn,
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: UnknownToolError,
      toolName: "get_time",
    });
  });

  it("wraps a throwing handler in ToolError", async () => {
    const transport = new MockTransport((_req, call) =>
      call === 0 ? openOk : toolTurn,
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

  it("surfaces a 502 as ProvidersFailedError carrying the events", async () => {
    const events = [
      {
        id: "evt_1",
        session_id: "ses_1",
        client_key_id: null,
        event_type: "session.error",
        payload: { reason: "all_providers_failed" },
        created_at: "t",
      },
    ];
    const transport = new MockTransport((_req, call) =>
      call === 0
        ? openOk
        : {
            status: 502,
            body: {
              message: { role: "assistant", content: "provider unavailable" },
              events,
            },
          },
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

  it("maps other non-2xx to ApiError", async () => {
    const transport = new MockTransport((_req, call) =>
      call === 0
        ? openOk
        : {
            status: 409,
            body: {
              type: "session_closed",
              title: "Closed",
              status: 409,
              detail: "gone",
            },
          },
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.send("hi")).rejects.toMatchObject({
      constructor: ApiError,
      type: "session_closed",
      status: 409,
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
