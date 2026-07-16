// "No OTel SDK installed" regression guard — the disabled-by-default contract
// (WI 0013; telemetry contract §0.3, §6). This is a SEPARATE file from
// `telemetry.test.ts` on purpose: vitest isolates each file in its own process,
// so here the OTel globals are genuinely empty (no TracerProvider, no
// ContextManager, no non-noop Propagator) — exactly an embedding app that never
// installed an SDK. Every harness span is then a no-op and the no-op propagator
// injects nothing, so NO `traceparent` header must appear on ANY outbound
// request.
import { describe, expect, it } from "vitest";

import { Config } from "./config.js";
import { Harness } from "./harness.js";
import { FetchTransport } from "./transport.js";
import type { JsonRpcRequest } from "./types.js";

const config = () =>
  new Config({
    serverUrl: "http://test",
    clientKey: "bae_test",
    clientVersion: "9.9.9",
  });

const openBody = {
  session_id: "ses_1",
  session_key: "bae_ses_1",
  profile: {
    id: "pro_1",
    name: "main",
    allowed_tools: [],
    mcp_servers: [],
    provider: { provider: "anthropic", model: "claude-sonnet-4-6" },
  },
};

describe("traceparent propagation — no SDK installed", () => {
  it("sends no traceparent/tracestate header on any outbound request", async () => {
    const captured: {
      path: string;
      method: string;
      headers: Record<string, string>;
    }[] = [];
    const fetchStub = async (
      url: string | URL,
      init?: RequestInit,
    ): Promise<Response> => {
      const u = new URL(String(url));
      captured.push({
        path: u.pathname,
        method: init?.method ?? "GET",
        headers: init?.headers as Record<string, string>,
      });
      const body =
        init?.body !== undefined
          ? (JSON.parse(String(init.body)) as JsonRpcRequest)
          : undefined;
      if (u.pathname === "/api/v1/sessions" && init?.method === "POST") {
        return new Response(JSON.stringify(openBody), { status: 201 });
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
      const harness = new Harness(config(), {
        transport: new FetchTransport("http://test"),
      });
      const session = await harness.connect();
      await session.send("hi");
      await session.close();
    } finally {
      globalThis.fetch = realFetch;
    }

    // open + registerDriver + sendMessage + close all happened...
    expect(captured.length).toBeGreaterThanOrEqual(4);
    // ...and none of them carried trace context.
    for (const c of captured) {
      expect(c.headers.traceparent, `${c.method} ${c.path}`).toBeUndefined();
      expect(c.headers.tracestate).toBeUndefined();
    }
  });
});
