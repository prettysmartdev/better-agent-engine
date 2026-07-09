import { describe, expect, it, vi } from "vitest";
import type { WebSocket } from "ws";
import {
  ObserverBridge,
  type BridgeAdmin,
  type DownstreamMessage,
  type ObserverKeySource,
  type UpstreamClient,
} from "./bridge.js";
import type {
  SessionEvent,
  SubscribeHandle,
  SubscribeHandlers,
} from "./clientPortClient.js";

/** A fake browser WebSocket capturing every message MAX pushes down it. */
class FakeSocket {
  readyState = 1; // OPEN
  messages: DownstreamMessage[] = [];
  closed?: { code: number; reason: string };
  private handlers: Record<string, Array<(...a: unknown[]) => void>> = {};

  send(data: string): void {
    this.messages.push(JSON.parse(data) as DownstreamMessage);
  }
  close(code: number, reason: string): void {
    this.closed = { code, reason };
    this.readyState = 3; // CLOSED
    this.emit("close");
  }
  on(event: string, cb: (...a: unknown[]) => void): this {
    (this.handlers[event] ??= []).push(cb);
    return this;
  }
  emit(event: string): void {
    for (const cb of this.handlers[event] ?? []) cb();
  }
  /** Simulate the browser disconnecting. */
  disconnect(): void {
    this.readyState = 3;
    this.emit("close");
  }
  types(): string[] {
    return this.messages.map((m) => m.type);
  }
  liveEventIds(): string[] {
    return this.messages
      .filter(
        (m): m is { type: "event"; event: SessionEvent } => m.type === "event",
      )
      .map((m) => m.event.id);
  }
  asWs(): WebSocket {
    return this as unknown as WebSocket;
  }
}

function evt(id: string): SessionEvent {
  return {
    id,
    session_id: "ses_1",
    client_key_id: null,
    event_type: "provider.request",
    payload: {},
    created_at: "2026-07-08T00:00:00Z",
  };
}

interface Captured {
  sessionId: string;
  sessionKey: string;
  sinceEventId: string | undefined;
  handlers: SubscribeHandlers;
  handle: { closed: boolean } & SubscribeHandle;
}

/** Assemble a bridge with controllable fakes and return the handles to poke. */
function setup(opts: {
  history?: SessionEvent[];
  joinImpl?: (id: string, key: string) => Promise<string>;
  sessions?: Array<{ id: string; profile_id: string; state?: string }>;
}) {
  const history = opts.history ?? [];
  const joinCalls: Array<{ id: string; key: string }> = [];
  const subscribes: Captured[] = [];
  const observerKey = vi.fn(async (profileId: string) => ({
    key: `bae_observer_${profileId}`,
    key_id: `key_${profileId}`,
  }));

  const admin: BridgeAdmin = {
    getAllSessionEvents: vi.fn(async () => history),
    listSessions: vi.fn(async () => ({
      items: opts.sessions ?? [],
      next_cursor: null,
    })),
  };
  const observerKeys: ObserverKeySource = { observerKey };
  const client: UpstreamClient = {
    join: vi.fn(async (id: string, key: string) => {
      joinCalls.push({ id, key });
      if (opts.joinImpl) return opts.joinImpl(id, key);
      return `sk_${id}`;
    }),
    subscribe: vi.fn(
      (
        sessionId: string,
        sessionKey: string,
        sinceEventId: string | undefined,
        handlers: SubscribeHandlers,
      ): SubscribeHandle => {
        const handle = { closed: false, close: () => {} } as Captured["handle"];
        handle.close = () => {
          handle.closed = true;
        };
        subscribes.push({
          sessionId,
          sessionKey,
          sinceEventId,
          handlers,
          handle,
        });
        return handle;
      },
    ),
  };

  const bridge = new ObserverBridge(admin, observerKeys, client);
  return { bridge, admin, client, observerKey, joinCalls, subscribes };
}

describe("ObserverBridge", () => {
  it("delivers history then opens one upstream subscribe since the last history id", async () => {
    const { bridge, subscribes } = setup({ history: [evt("e1"), evt("e2")] });
    const sock = new FakeSocket();

    bridge.handleConnection("ses_1", sock.asWs(), "pro_1");

    await vi.waitFor(() => expect(subscribes).toHaveLength(1));
    expect(sock.messages[0]).toEqual({
      type: "history",
      events: [evt("e1"), evt("e2")],
    });
    expect(subscribes[0]!.sinceEventId).toBe("e2");
    expect(subscribes[0]!.sessionKey).toBe("sk_ses_1");
  });

  it("fans one upstream stream out to two viewers with a single join/subscribe", async () => {
    const { bridge, subscribes, joinCalls, client } = setup({
      history: [evt("e1")],
    });
    const a = new FakeSocket();
    const b = new FakeSocket();

    bridge.handleConnection("ses_1", a.asWs(), "pro_1");
    await vi.waitFor(() => expect(subscribes).toHaveLength(1));
    bridge.handleConnection("ses_1", b.asWs(), "pro_1");
    await vi.waitFor(() => expect(b.types()).toContain("history"));

    // A live event fans out to both.
    subscribes[0]!.handlers.onEvent(evt("e2"));
    expect(a.liveEventIds()).toEqual(["e2"]);
    expect(b.liveEventIds()).toEqual(["e2"]);

    // Exactly one join and one subscribe despite two viewers.
    expect(joinCalls).toHaveLength(1);
    expect(client.subscribe).toHaveBeenCalledTimes(1);
  });

  it("dedups a late viewer's history against subsequent live replay", async () => {
    // The shared stream already forwarded e2; a late viewer's history includes
    // e2, so a re-forward of e2 must not double-render for that viewer.
    const { bridge, subscribes } = setup({ history: [evt("e1")] });
    const a = new FakeSocket();
    bridge.handleConnection("ses_1", a.asWs(), "pro_1");
    await vi.waitFor(() => expect(subscribes).toHaveLength(1));
    subscribes[0]!.handlers.onEvent(evt("e2"));

    // Late viewer B: its history now covers e1+e2.
    const b = new FakeSocket();
    // Re-point history for the late join by swapping the admin mock's return:
    (bridge as unknown as { admin: BridgeAdmin }).admin.getAllSessionEvents =
      vi.fn(async () => [evt("e1"), evt("e2")]);
    bridge.handleConnection("ses_1", b.asWs(), "pro_1");
    await vi.waitFor(() => expect(b.types()).toContain("history"));

    // A replayed e2 must be suppressed for B (already in its history).
    subscribes[0]!.handlers.onEvent(evt("e2"));
    expect(b.liveEventIds()).toEqual([]);
  });

  it("reconnects exactly once on lagged, from the last forwarded id", async () => {
    const { bridge, subscribes } = setup({ history: [evt("e1")] });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs(), "pro_1");
    await vi.waitFor(() => expect(subscribes).toHaveLength(1));

    subscribes[0]!.handlers.onEvent(evt("e5"));
    subscribes[0]!.handlers.onLagged();

    expect(subscribes).toHaveLength(2);
    expect(subscribes[1]!.sinceEventId).toBe("e5");
    expect(subscribes[0]!.handle.closed).toBe(true);
    // No further reconnect without another lagged frame.
    expect(subscribes).toHaveLength(2);
  });

  it("does not re-join for a cached session on reconnect", async () => {
    const { bridge, subscribes, joinCalls } = setup({ history: [] });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs(), "pro_1");
    await vi.waitFor(() => expect(subscribes).toHaveLength(1));
    subscribes[0]!.handlers.onLagged();
    // Reconnect reused the cached session key — still one join.
    expect(joinCalls).toHaveLength(1);
  });

  it("signals session_ended and closes downstream when the upstream ends", async () => {
    const { bridge, subscribes } = setup({ history: [evt("e1")] });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs(), "pro_1");
    await vi.waitFor(() => expect(subscribes).toHaveLength(1));

    subscribes[0]!.handlers.onEnd();

    const last = sock.messages[sock.messages.length - 1];
    expect(last).toEqual({ type: "session_ended", reason: "closed" });
    expect(sock.closed?.code).toBe(1000);
  });

  it("serves history then session_ended for a known-terminal session (no join attempted)", async () => {
    const { bridge, subscribes, joinCalls } = setup({ history: [evt("e1")] });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs(), "pro_1", "closed");

    await vi.waitFor(() => expect(sock.types()).toContain("session_ended"));
    expect(sock.types()).toEqual(["history", "session_ended"]);
    expect(subscribes).toHaveLength(0);
    expect(joinCalls).toHaveLength(0); // known terminal: never joined
  });

  it("skips the join for a terminal session found via the sessions-list scan", async () => {
    const { bridge, subscribes, joinCalls } = setup({
      history: [evt("e1")],
      sessions: [{ id: "ses_1", profile_id: "pro_9", state: "error" }],
    });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs()); // no hints

    await vi.waitFor(() => expect(sock.types()).toContain("session_ended"));
    expect(sock.types()).toEqual(["history", "session_ended"]);
    expect(subscribes).toHaveLength(0);
    expect(joinCalls).toHaveLength(0);
  });

  it("falls back to history-only when the session closes between state check and join (409)", async () => {
    const { bridge, subscribes, joinCalls } = setup({
      history: [evt("e1")],
      joinImpl: async () => {
        throw new Error("join ses_1 failed: HTTP 409 session_closed");
      },
    });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs(), "pro_1", "open");

    await vi.waitFor(() => expect(sock.types()).toContain("session_ended"));
    expect(sock.types()).toEqual(["history", "session_ended"]);
    expect(subscribes).toHaveLength(0);
    expect(joinCalls).toHaveLength(1); // attempted once, rejected 409
  });

  it("resolves the profile id from the sessions list when no hint is given", async () => {
    const { bridge, subscribes, observerKey } = setup({
      history: [],
      sessions: [{ id: "ses_1", profile_id: "pro_9" }],
    });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs()); // no hint

    await vi.waitFor(() => expect(subscribes).toHaveLength(1));
    expect(observerKey).toHaveBeenCalledWith("pro_9");
  });

  it("tears down the upstream when the last viewer disconnects", async () => {
    const { bridge, subscribes } = setup({ history: [] });
    const sock = new FakeSocket();
    bridge.handleConnection("ses_1", sock.asWs(), "pro_1");
    await vi.waitFor(() => expect(subscribes).toHaveLength(1));

    sock.disconnect();
    expect(subscribes[0]!.handle.closed).toBe(true);
    expect(bridge.activeSessionCount()).toBe(0);
  });
});
