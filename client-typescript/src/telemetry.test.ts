// OpenTelemetry client-span + `traceparent`-propagation tests, with a
// test-double OTel SDK installed (WI 0013 Parts D/E; telemetry contract §1.2,
// §6, §7, and the canonical parity fixture §9).
//
// vitest isolates each test file in its own process, so the global
// TracerProvider/ContextManager/Propagator installed here never leak into the
// other suites (and theirs never leak in). The companion "no OTel SDK
// installed" regression guard lives in `telemetry-noop.test.ts`, deliberately a
// separate file so it runs with genuinely-empty OTel globals.
import { context, propagation, SpanKind, trace } from "@opentelemetry/api";
import { AsyncLocalStorageContextManager } from "@opentelemetry/context-async-hooks";
import {
  CompositePropagator,
  W3CBaggagePropagator,
  W3CTraceContextPropagator,
} from "@opentelemetry/core";
import {
  BasicTracerProvider,
  InMemorySpanExporter,
  type ReadableSpan,
  SimpleSpanProcessor,
} from "@opentelemetry/sdk-trace-base";
import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";

import { Config } from "./config.js";
import { Harness } from "./harness.js";
import { FetchTransport } from "./transport.js";
import type {
  Transport,
  TransportRequest,
  TransportResponse,
} from "./transport.js";
import type { ContentBlock, JsonRpcFrame, JsonRpcRequest } from "./types.js";
import { messageText } from "./types.js";

const exporter = new InMemorySpanExporter();

beforeAll(() => {
  // Model an embedding application that installed an OTel SDK: a real context
  // manager (required for span nesting across `await`), a recording tracer
  // provider, and the W3C propagator so `traceparent` is actually serialized.
  context.setGlobalContextManager(new AsyncLocalStorageContextManager());
  trace.setGlobalTracerProvider(
    new BasicTracerProvider({
      spanProcessors: [new SimpleSpanProcessor(exporter)],
    }),
  );
  propagation.setGlobalPropagator(new W3CTraceContextPropagator());
});

beforeEach(() => exporter.reset());
afterEach(() => exporter.reset());

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
      allowed_tools: ["get_current_time"],
      mcp_servers: [],
      provider: { provider: "anthropic", model: "claude-sonnet-4-6" },
    },
  },
};

/** A single-frame NDJSON reply: the terminal `{message, events}` result. */
function assistantFrames(content: ContentBlock[]): JsonRpcFrame[] {
  return [
    {
      jsonrpc: "2.0",
      id: 1,
      result: { message: { role: "assistant", content }, events: [] },
    },
  ];
}

/**
 * A scripted, request-recording transport (no network). `request` covers the
 * REST routes; `stream` drives the JSON-RPC loop. `registerDriver` is
 * auto-answered and kept out of `requests` so the `call` index tracks the
 * ordinary open + sendMessage exchange, matching `harness.test.ts`.
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
    ) => JsonRpcFrame[],
  ) {}
  async request(req: TransportRequest): Promise<TransportResponse> {
    const call = this.requests.length;
    this.requests.push(structuredClone(req));
    return this.onRequest(req, call);
  }
  async *stream(req: TransportRequest): AsyncIterable<JsonRpcFrame> {
    const body = req.body as JsonRpcRequest | undefined;
    if (body?.method === "session.registerDriver") {
      yield { jsonrpc: "2.0", id: body.id, result: { registered: true } };
      return;
    }
    const call = this.requests.length;
    this.requests.push(structuredClone(req));
    for (const frame of this.onStream(req, call)) yield frame;
  }
}

const named = (name: string): ReadableSpan[] =>
  exporter.getFinishedSpans().filter((s) => s.name === name);

describe("client spans — canonical parity fixture (§9)", () => {
  it("emits bae.client.send per round trip and bae.client.tool only for dispatch:client blocks", async () => {
    // Turn 1: one dispatch:"client" tool_use + one dispatch:"mcp" tool_use.
    // Turn 2: final text. The client executes only its own tool.
    const transport = new MockTransport(
      () => openOk,
      (_req, call) =>
        call === 1
          ? assistantFrames([
              {
                type: "tool_use",
                id: "tu_client",
                name: "get_current_time",
                input: {},
                dispatch: "client",
              },
              {
                type: "tool_use",
                id: "tu_mcp",
                name: "remote_search",
                input: { q: "x" },
                dispatch: "mcp",
              },
            ])
          : assistantFrames([{ type: "text", text: "done" }]),
    );
    const harness = new Harness(config(), { transport }).registerTool({
      name: "get_current_time",
      description: "the time",
      input_schema: {},
      handler: () => "2026-07-06T00:00:00Z",
    });

    const session = await harness.connect();
    const reply = await session.send("go");
    expect(messageText(reply)).toBe("done");

    // Two send spans (iteration 0, 1); exactly one tool span (mcp gets none).
    const sends = named("bae.client.send");
    const tools = named("bae.client.tool");
    expect(sends).toHaveLength(2);
    expect(tools).toHaveLength(1);

    // `bae.client.send` attribute KEYS + literal values (contract §1.2), plus
    // SpanKind and the instrumentation scope name+version — the cross-SDK parity
    // dimensions (contract §0.2). These must match the Rust/Python assertions.
    for (const s of sends) {
      expect(Object.keys(s.attributes).sort()).toEqual([
        "bae.client.iteration",
        "bae.rpc.method",
        "bae.session.id",
      ]);
      expect(s.attributes["bae.session.id"]).toBe("ses_1");
      expect(s.attributes["bae.rpc.method"]).toBe("session.sendMessage");
      expect(s.kind).toBe(SpanKind.CLIENT);
      expect(s.instrumentationLibrary.name).toBe("bae.client");
      expect(s.instrumentationLibrary.version).toBe("0.1.0");
    }
    expect(
      sends.map((s) => s.attributes["bae.client.iteration"]).sort(),
    ).toEqual([0, 1]);

    // `bae.client.tool` attribute KEYS + literal values.
    const tool = tools[0]!;
    expect(Object.keys(tool.attributes).sort()).toEqual([
      "bae.tool.dispatch",
      "bae.tool.name",
    ]);
    expect(tool.attributes["bae.tool.name"]).toBe("get_current_time");
    expect(tool.attributes["bae.tool.dispatch"]).toBe("client");
    expect(tool.kind).toBe(SpanKind.INTERNAL);
    expect(tool.instrumentationLibrary.name).toBe("bae.client");
    expect(tool.instrumentationLibrary.version).toBe("0.1.0");

    // Parentage: the tool span is a child of the iteration-0 send span.
    const send0 = sends.find(
      (s) => s.attributes["bae.client.iteration"] === 0,
    )!;
    expect(tool.parentSpanId).toBe(send0.spanContext().spanId);
  });
});

describe("traceparent propagation — with SDK", () => {
  it("injects traceparent on every outbound request (open, registerDriver, sendMessage, close)", async () => {
    const captured: {
      path: string;
      method: string;
      headers: Record<string, string>;
    }[] = [];

    // Stub global fetch so FetchTransport's real injection choke points run; the
    // whole lifecycle is wrapped in an ambient app span, so session open/close
    // (which get no BAE span of their own) still carry the app's context.
    const fetchStub = async (
      url: string | URL,
      init?: RequestInit,
    ): Promise<Response> => {
      const u = new URL(String(url));
      const headers = init?.headers as Record<string, string>;
      captured.push({
        path: u.pathname,
        method: init?.method ?? "GET",
        headers,
      });
      const body =
        init?.body !== undefined
          ? (JSON.parse(String(init.body)) as JsonRpcRequest)
          : undefined;
      if (u.pathname === "/api/v1/sessions" && init?.method === "POST") {
        return new Response(JSON.stringify(openOk.body), { status: 201 });
      }
      if (u.pathname.endsWith("/rpc")) {
        const frame =
          body?.method === "session.registerDriver"
            ? { jsonrpc: "2.0", id: body.id, result: { registered: true } }
            : {
                jsonrpc: "2.0",
                id: body?.id ?? 1,
                result: {
                  message: {
                    role: "assistant",
                    content: [{ type: "text", text: "hi" }],
                  },
                  events: [],
                },
              };
        return new Response(JSON.stringify(frame) + "\n", { status: 200 });
      }
      // DELETE close.
      return new Response("", { status: 200 });
    };
    const realFetch = globalThis.fetch;
    globalThis.fetch = fetchStub as typeof fetch;
    try {
      await trace.getTracer("app").startActiveSpan("app-root", async (root) => {
        const harness = new Harness(config(), {
          transport: new FetchTransport("http://test"),
        });
        const session = await harness.connect();
        await session.send("hi");
        await session.close();
        root.end();
      });
    } finally {
      globalThis.fetch = realFetch;
    }

    // Every captured request — open (POST /sessions), registerDriver (POST
    // /rpc), sendMessage (POST /rpc), close (DELETE /sessions) — carries it.
    expect(captured.length).toBeGreaterThanOrEqual(4);
    expect(
      captured.some(
        (c) => c.path === "/api/v1/sessions" && c.method === "POST",
      ),
    ).toBe(true);
    expect(captured.some((c) => c.method === "DELETE")).toBe(true);
    expect(captured.filter((c) => c.path.endsWith("/rpc"))).toHaveLength(2);
    for (const c of captured) {
      expect(
        c.headers.traceparent,
        `${c.method} ${c.path} must carry traceparent`,
      ).toMatch(/^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$/);
    }
  });
});

describe("traceparent propagation — wire allowlist (§6)", () => {
  it("drops baggage the ambient propagator would inject, keeping only trace context", async () => {
    // Model a host app whose global propagator ALSO carries baggage (a common
    // real-world setup). BAE must still put only traceparent/tracestate on the
    // wire — never baggage, which could hold a token/tenant id/prompt fragment.
    const original = propagation;
    propagation.setGlobalPropagator(
      new CompositePropagator({
        propagators: [
          new W3CTraceContextPropagator(),
          new W3CBaggagePropagator(),
        ],
      }),
    );
    try {
      const captured: Record<string, string>[] = [];
      const fetchStub = async (
        url: string | URL,
        init?: RequestInit,
      ): Promise<Response> => {
        const u = new URL(String(url));
        captured.push(init?.headers as Record<string, string>);
        const body =
          init?.body !== undefined
            ? (JSON.parse(String(init.body)) as JsonRpcRequest)
            : undefined;
        if (u.pathname === "/api/v1/sessions" && init?.method === "POST") {
          return new Response(JSON.stringify(openOk.body), { status: 201 });
        }
        if (u.pathname.endsWith("/rpc")) {
          const frame =
            body?.method === "session.registerDriver"
              ? { jsonrpc: "2.0", id: body.id, result: { registered: true } }
              : {
                  jsonrpc: "2.0",
                  id: body?.id ?? 1,
                  result: {
                    message: {
                      role: "assistant",
                      content: [{ type: "text", text: "hi" }],
                    },
                    events: [],
                  },
                };
          return new Response(JSON.stringify(frame) + "\n", { status: 200 });
        }
        return new Response("", { status: 200 });
      };
      const realFetch = globalThis.fetch;
      globalThis.fetch = fetchStub as typeof fetch;
      try {
        // Set a baggage entry active while the whole lifecycle runs.
        const ctx = propagation.setBaggage(
          context.active(),
          propagation.createBaggage({
            api_token: { value: "fixture-secret" },
          }),
        );
        await context.with(ctx, async () => {
          await trace
            .getTracer("app")
            .startActiveSpan("app-root", async (root) => {
              const harness = new Harness(config(), {
                transport: new FetchTransport("http://test"),
              });
              const session = await harness.connect();
              await session.send("hi");
              await session.close();
              root.end();
            });
        });
      } finally {
        globalThis.fetch = realFetch;
      }

      expect(captured.length).toBeGreaterThanOrEqual(4);
      for (const headers of captured) {
        // trace context is present (an app span was active)…
        expect(headers.traceparent).toBeDefined();
        // …but baggage never reaches the wire.
        expect(headers.baggage).toBeUndefined();
        expect(
          JSON.stringify(headers).includes("fixture-secret"),
          "no baggage value may appear in any outbound header",
        ).toBe(false);
      }
    } finally {
      // Restore the plain W3C propagator for the remaining tests in this file.
      original.setGlobalPropagator(new W3CTraceContextPropagator());
    }
  });
});

describe("ambient context — survives the async boundary into hooks/handlers (§7)", () => {
  it("nests a hook's and a handler's own span under bae.client.tool", async () => {
    const userTracer = trace.getTracer("user.code");
    const transport = new MockTransport(
      () => openOk,
      (_req, call) =>
        call === 1
          ? assistantFrames([
              {
                type: "tool_use",
                id: "tu_1",
                name: "probe",
                input: {},
                dispatch: "client",
              },
            ])
          : assistantFrames([{ type: "text", text: "done" }]),
    );
    const harness = new Harness(config(), { transport })
      .registerTool({
        name: "probe",
        description: "opens a user span",
        input_schema: {},
        handler: async () => {
          await new Promise((r) => setTimeout(r, 0));
          userTracer.startSpan("user.handler.span").end();
          return "ok";
        },
      })
      .setHooks({
        before_tool_call: async () => {
          await new Promise((r) => setTimeout(r, 0));
          userTracer.startSpan("user.hook.span").end();
        },
      });

    const session = await harness.connect();
    await session.send("go");

    const toolId = named("bae.client.tool")[0]!.spanContext().spanId;
    expect(named("user.hook.span")[0]!.parentSpanId).toBe(toolId);
    expect(named("user.handler.span")[0]!.parentSpanId).toBe(toolId);
  });
});
