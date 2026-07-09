import { describe, expect, it, vi } from "vitest";
import {
  ClientPortClient,
  dispatchLine,
  MAX_CLIENT_VERSION,
  type SessionEvent,
  type SubscribeHandlers,
} from "./clientPortClient.js";

function handlers(): SubscribeHandlers & {
  events: SessionEvent[];
  lagged: number;
  ended: number;
  errors: Error[];
} {
  const rec = {
    events: [] as SessionEvent[],
    lagged: 0,
    ended: 0,
    errors: [] as Error[],
    onEvent(e: SessionEvent) {
      rec.events.push(e);
    },
    onLagged() {
      rec.lagged += 1;
    },
    onEnd() {
      rec.ended += 1;
    },
    onError(err: Error) {
      rec.errors.push(err);
    },
  };
  return rec;
}

/** Build a Response whose body streams the given NDJSON lines. */
function ndjsonResponse(lines: string[], status = 200): Response {
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const enc = new TextEncoder();
      for (const line of lines) controller.enqueue(enc.encode(line + "\n"));
      controller.close();
    },
  });
  return new Response(stream, { status });
}

describe("dispatchLine", () => {
  it("routes a session.event notification to onEvent", () => {
    const h = handlers();
    const stop = dispatchLine(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "session.event",
        params: { id: "e1", event_type: "session.open" },
      }),
      h,
    );
    expect(stop).toBe(false);
    expect(h.events[0]!.id).toBe("e1");
  });

  it("treats an id-less -32000 lagged error as terminal", () => {
    const h = handlers();
    const stop = dispatchLine(
      JSON.stringify({
        jsonrpc: "2.0",
        error: {
          code: -32000,
          message: "lagged; reconnect with since_event_id",
        },
      }),
      h,
    );
    expect(stop).toBe(true);
    expect(h.lagged).toBe(1);
  });

  it("does not treat a -32000 response WITH an id as lagged", () => {
    const h = handlers();
    const stop = dispatchLine(
      JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        error: { code: -32000, message: "session is closed" },
      }),
      h,
    );
    expect(stop).toBe(false);
    expect(h.lagged).toBe(0);
  });

  it("ignores malformed lines without throwing", () => {
    const h = handlers();
    expect(dispatchLine("{not json", h)).toBe(false);
    expect(h.errors).toHaveLength(0);
  });
});

describe("ClientPortClient.join", () => {
  it("POSTs join with the observer client_version and empty tools, returns session_key", async () => {
    const fetchImpl = vi.fn(
      async () =>
        new Response(
          JSON.stringify({ session_id: "ses_1", session_key: "sk_1" }),
          {
            status: 201,
          },
        ),
    );
    const client = new ClientPortClient(
      "127.0.0.1:8080",
      fetchImpl as unknown as typeof fetch,
    );
    const key = await client.join("ses_1", "bae_observer");
    expect(key).toBe("sk_1");
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(String(url)).toBe(
      "http://127.0.0.1:8080/api/v1/sessions/ses_1/join",
    );
    const body = JSON.parse((init as RequestInit).body as string);
    expect(body).toEqual({ client_version: MAX_CLIENT_VERSION, tools: [] });
    expect((init as RequestInit).headers).toMatchObject({
      authorization: "Bearer bae_observer",
    });
  });

  it("throws when join is rejected", async () => {
    const fetchImpl = vi.fn(
      async () => new Response('{"type":"session_closed"}', { status: 409 }),
    );
    const client = new ClientPortClient(
      "h:1",
      fetchImpl as unknown as typeof fetch,
    );
    await expect(client.join("ses_1", "k")).rejects.toThrow(/409/);
  });
});

describe("ClientPortClient.subscribe", () => {
  it("passes since_event_id and streams events then onEnd", async () => {
    const fetchImpl = vi.fn(async () =>
      ndjsonResponse([
        JSON.stringify({
          jsonrpc: "2.0",
          method: "session.event",
          params: { id: "e2" },
        }),
        JSON.stringify({
          jsonrpc: "2.0",
          method: "session.event",
          params: { id: "e3" },
        }),
      ]),
    );
    const client = new ClientPortClient(
      "h:1",
      fetchImpl as unknown as typeof fetch,
    );
    const h = handlers();
    client.subscribe("ses_1", "sk_1", "e1", h);
    await vi.waitFor(() => expect(h.ended).toBe(1));
    expect(h.events.map((e) => e.id)).toEqual(["e2", "e3"]);

    const [, init] = fetchImpl.mock.calls[0]!;
    const rpc = JSON.parse((init as RequestInit).body as string);
    expect(rpc.method).toBe("session.subscribe");
    expect(rpc.params).toEqual({ since_event_id: "e1" });
  });

  it("stops on a lagged notification and reports it via onLagged (no onEnd)", async () => {
    const fetchImpl = vi.fn(async () =>
      ndjsonResponse([
        JSON.stringify({
          jsonrpc: "2.0",
          method: "session.event",
          params: { id: "e2" },
        }),
        JSON.stringify({
          jsonrpc: "2.0",
          error: { code: -32000, message: "lagged; reconnect" },
        }),
        JSON.stringify({
          jsonrpc: "2.0",
          method: "session.event",
          params: { id: "e3" },
        }),
      ]),
    );
    const client = new ClientPortClient(
      "h:1",
      fetchImpl as unknown as typeof fetch,
    );
    const h = handlers();
    client.subscribe("ses_1", "sk_1", undefined, h);
    await vi.waitFor(() => expect(h.lagged).toBe(1));
    // Events after the lagged frame are not delivered; the stream is dead.
    expect(h.events.map((e) => e.id)).toEqual(["e2"]);
    expect(h.ended).toBe(0);
  });

  it("reports a non-ok subscribe response via onError", async () => {
    const fetchImpl = vi.fn(async () => new Response("nope", { status: 403 }));
    const client = new ClientPortClient(
      "h:1",
      fetchImpl as unknown as typeof fetch,
    );
    const h = handlers();
    client.subscribe("ses_1", "sk_1", undefined, h);
    await vi.waitFor(() => expect(h.errors).toHaveLength(1));
    expect(h.errors[0]!.message).toMatch(/403/);
  });
});
