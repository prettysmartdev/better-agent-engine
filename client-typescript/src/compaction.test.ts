import { describe, expect, it } from "vitest";

import { Config } from "./config.js";
import { RpcError } from "./errors.js";
import { Harness } from "./harness.js";
import type {
  Transport,
  TransportRequest,
  TransportResponse,
} from "./transport.js";
import type {
  CompactParams,
  JsonRpcFrame,
  JsonRpcRequest,
  SessionCompactionCompleted,
  SessionEvent,
} from "./types.js";

/**
 * Same scripted, request-recording transport shape used in harness.test.ts
 * (each test file that drives the loop offline defines its own copy — see
 * sandbox.test.ts's/subagent.test.ts's `RpcMock`), specialized here to script
 * `session.compact` in isolation from `session.sendMessage`.
 */
class MockTransport implements Transport {
  readonly requests: TransportRequest[] = [];
  constructor(
    private readonly onRequest: (req: TransportRequest) => TransportResponse,
    private readonly onStream: (
      req: TransportRequest,
    ) => JsonRpcFrame[] = () => [],
  ) {}
  async request(req: TransportRequest): Promise<TransportResponse> {
    this.requests.push(structuredClone(req));
    return this.onRequest(req);
  }
  async *stream(req: TransportRequest): AsyncIterable<JsonRpcFrame> {
    const body = req.body as JsonRpcRequest | undefined;
    if (body?.method === "session.registerDriver") {
      yield { jsonrpc: "2.0", id: body.id, result: { registered: true } };
      return;
    }
    this.requests.push(structuredClone(req));
    for (const frame of this.onStream(req)) yield frame;
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
    session_id: "ses_01example",
    session_key: "bae_ses_1",
    profile: {
      id: "pro_1",
      name: "main",
      allowed_tools: [],
      mcp_servers: [],
      provider: { provider: "anthropic", model: "claude-sonnet-5" },
    },
  },
};

// ---------------------------------------------------------------------------
// `compaction` at session-open — contracts.md §1.11: byte-exact shapes, key
// omitted entirely (never `null`) when unset, never sent on join().
// ---------------------------------------------------------------------------

describe("connect — compaction option", () => {
  it("omits the compaction key entirely when unset", async () => {
    const transport = new MockTransport(() => openOk);
    await new Harness(config(), { transport }).connect();
    const req = transport.requests[0]!;
    expect(req.body).not.toHaveProperty("compaction");
  });

  it("serializes mode:auto exactly", async () => {
    const transport = new MockTransport(() => openOk);
    await new Harness(config(), {
      transport,
      compaction: { mode: "auto", size: 128000 },
    }).connect();
    const req = transport.requests[0]!;
    expect((req.body as { compaction: unknown }).compaction).toEqual({
      mode: "auto",
      size: 128000,
    });
  });

  it("serializes mode:client with no prompt key when prompt is omitted", async () => {
    const transport = new MockTransport(() => openOk);
    await new Harness(config(), {
      transport,
      compaction: { mode: "client" },
    }).connect();
    const req = transport.requests[0]!;
    const compaction = (req.body as { compaction: Record<string, unknown> })
      .compaction;
    expect(compaction).toEqual({ mode: "client" });
    expect(compaction).not.toHaveProperty("prompt");
  });

  it("serializes mode:client with prompt exactly", async () => {
    const transport = new MockTransport(() => openOk);
    await new Harness(config(), {
      transport,
      compaction: {
        mode: "client",
        prompt: "Summarize focusing on open TODOs.",
      },
    }).connect();
    const req = transport.requests[0]!;
    expect((req.body as { compaction: unknown }).compaction).toEqual({
      mode: "client",
      prompt: "Summarize focusing on open TODOs.",
    });
  });

  it("setCompaction updates what connect() sends", async () => {
    const transport = new MockTransport(() => openOk);
    const harness = new Harness(config(), { transport });
    harness.setCompaction({ mode: "auto", size: 5000 });
    await harness.connect();
    const req = transport.requests[0]!;
    expect((req.body as { compaction: unknown }).compaction).toEqual({
      mode: "auto",
      size: 5000,
    });
  });
});

describe("join — compaction is never sent", () => {
  it("omits compaction on join() even when the harness has one configured", async () => {
    const transport = new MockTransport(() => openOk);
    const harness = new Harness(config(), {
      transport,
      compaction: { mode: "auto", size: 128000 },
    });
    await harness.join("ses_existing");
    const req = transport.requests[0]!;
    expect(req.path).toBe("/api/v1/sessions/ses_existing/join");
    expect(req.body).not.toHaveProperty("compaction");
  });
});

// ---------------------------------------------------------------------------
// `session.compact` request/response shape — contracts.md §1.12, matching the
// shared fixtures in tests/fixtures/.
// ---------------------------------------------------------------------------

/** The shared `session.compaction.completed` terminal-frame fixture (contracts.md §1.12). */
const COMPLETED_TERMINAL_FRAME: JsonRpcFrame = {
  jsonrpc: "2.0",
  id: 7,
  result: {
    id: "evt_01completed",
    session_id: "ses_01example",
    client_key_id: "key_01example",
    event_type: "session.compaction.completed",
    payload: {
      preamble_event_id: "evt_01preamble",
      summary_event_id: "evt_01summary",
      compacted_message_count: 17,
      input_tokens: 42100,
      summary_tokens: 900,
    },
    created_at: "2026-09-23T18:26:10.000Z",
  },
};

/** The shared `-32020 turn in progress` error-frame fixture. */
const TURN_IN_PROGRESS_ERROR_FRAME: JsonRpcFrame = {
  jsonrpc: "2.0",
  id: 7,
  error: {
    code: -32020,
    message: "turn in progress: resolve the paused turn before compacting",
  },
};

const STARTED_NOTIFICATION: JsonRpcFrame = {
  jsonrpc: "2.0",
  method: "session.event",
  params: {
    id: "evt_01startedmanual",
    session_id: "ses_01example",
    client_key_id: "key_01example",
    event_type: "session.compaction.started",
    payload: {
      trigger: "client",
      reason: "manual",
      token_count: 1100,
      threshold_tokens: null,
    },
    created_at: "2026-09-23T18:26:08.000Z",
  },
};

const PREAMBLE_NOTIFICATION: JsonRpcFrame = {
  jsonrpc: "2.0",
  method: "session.event",
  params: {
    id: "evt_01preamble",
    session_id: "ses_01example",
    client_key_id: "key_01example",
    event_type: "server.message.send",
    payload: {
      role: "user",
      content: [
        {
          type: "text",
          text: "The earlier part of this conversation was compacted. A summary follows.",
        },
      ],
      synthetic: "compaction_preamble",
    },
    created_at: "2026-09-23T18:26:09.998Z",
  },
};

describe("Session.compact", () => {
  it("sends params:{} when no prompt is given", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => [COMPLETED_TERMINAL_FRAME],
    );
    const session = await new Harness(config(), { transport }).connect();
    await session.compact();
    const req = transport.requests.find(
      (r) => (r.body as JsonRpcRequest).method === "session.compact",
    )!;
    expect((req.body as JsonRpcRequest<CompactParams>).params).toEqual({});
  });

  it("sends {prompt} exactly when a prompt is given", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => [COMPLETED_TERMINAL_FRAME],
    );
    const session = await new Harness(config(), { transport }).connect();
    await session.compact("one-off prompt");
    const req = transport.requests.find(
      (r) => (r.body as JsonRpcRequest).method === "session.compact",
    )!;
    expect((req.body as JsonRpcRequest<CompactParams>).params).toEqual({
      prompt: "one-off prompt",
    });
  });

  it("resolves with the typed session.compaction.completed record from the fixture", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => [COMPLETED_TERMINAL_FRAME],
    );
    const session = await new Harness(config(), { transport }).connect();
    const completed: SessionCompactionCompleted = await session.compact();

    expect(completed.id).toBe("evt_01completed");
    expect(completed.event_type).toBe("session.compaction.completed");
    expect(completed.payload.preamble_event_id).toBe("evt_01preamble");
    expect(completed.payload.summary_event_id).toBe("evt_01summary");
    expect(completed.payload.compacted_message_count).toBe(17);
    expect(completed.payload.input_tokens).toBe(42100);
    expect(completed.payload.summary_tokens).toBe(900);
  });

  it("runs on_event for every streamed notification, in order, before resolving", async () => {
    const seen: SessionEvent[] = [];
    const transport = new MockTransport(
      () => openOk,
      () => [
        STARTED_NOTIFICATION,
        PREAMBLE_NOTIFICATION,
        COMPLETED_TERMINAL_FRAME,
      ],
    );
    const harness = new Harness(config(), { transport });
    harness.setHooks({ on_event: (ev) => void seen.push(ev) });
    const session = await harness.connect();

    const completed = await session.compact();

    expect(seen.map((e) => e.event_type)).toEqual([
      "session.compaction.started",
      "server.message.send",
    ]);
    expect((seen[1]!.payload as { synthetic?: string }).synthetic).toBe(
      "compaction_preamble",
    );
    expect(completed.payload.compacted_message_count).toBe(17);
  });

  it("surfaces the turn-in-progress error frame as RpcError(-32020, ...)", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => [TURN_IN_PROGRESS_ERROR_FRAME],
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.compact()).rejects.toMatchObject({
      constructor: RpcError,
      code: -32020,
      rpcMessage: "turn in progress: resolve the paused turn before compacting",
    });
  });

  it("raises RpcError(-32603) when the stream ends without a terminal frame", async () => {
    const transport = new MockTransport(
      () => openOk,
      () => [STARTED_NOTIFICATION],
    );
    const session = await new Harness(config(), { transport }).connect();
    await expect(session.compact()).rejects.toMatchObject({
      constructor: RpcError,
      code: -32603,
    });
  });
});
