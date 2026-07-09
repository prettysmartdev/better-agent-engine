//! A minimal, purpose-built client-port client for observer use only
//! (work item 0007, section C).
//!
//! This deliberately does NOT depend on `@prettysmartdev/bae-ts`
//! (`Harness.connect()`/`join()` unconditionally register a driver, which would
//! pollute the event log with a phantom participant on every session MAX merely
//! observes). It calls exactly two things and nothing else:
//!
//!   1. `POST /api/v1/sessions/{id}/join`      (REST)   — mint a session key.
//!   2. `session.subscribe` / `session.unsubscribe` over
//!      `POST /api/v1/sessions/{id}/rpc`       (NDJSON JSON-RPC) — live tail.
//!
//! It NEVER calls `session.registerDriver` or `session.sendMessage`.

import { NdjsonBuffer } from "./ndjson.js";

/** A `session.event` notification payload forwarded to browsers. */
export interface SessionEvent {
  id: string;
  session_id: string;
  client_key_id: string | null;
  event_type: string;
  payload: unknown;
  created_at: string;
}

/** Callbacks a subscribe stream drives. */
export interface SubscribeHandlers {
  /** A live `session.event` notification arrived. */
  onEvent(event: SessionEvent): void;
  /**
   * The broadcast buffer overran: the server sent `-32000 "lagged"`. The stream
   * is now dead; the caller must reconnect with `since_event_id`.
   */
  onLagged(): void;
  /** The upstream stream ended cleanly (session closed / server hung up). */
  onEnd(): void;
  /** A transport/parse error killed the stream. */
  onError(err: Error): void;
}

/** A handle to an active subscribe stream. */
export interface SubscribeHandle {
  /** Abort the upstream stream (does not send `session.unsubscribe`). */
  close(): void;
}

export class ClientPortClient {
  private readonly base: string;

  constructor(
    clientAddr: string,
    private readonly fetchImpl: typeof fetch = fetch,
  ) {
    this.base = clientAddr.includes("://")
      ? clientAddr
      : `http://${clientAddr}`;
  }

  /**
   * `POST /api/v1/sessions/{id}/join` with an empty tool set. Returns the minted
   * session key. Observer-only: it declares no tools and never registers as a
   * driver.
   */
  async join(sessionId: string, clientKey: string): Promise<string> {
    const resp = await this.fetchImpl(
      `${this.base}/api/v1/sessions/${sessionId}/join`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${clientKey}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ client_version: MAX_CLIENT_VERSION, tools: [] }),
      },
    );
    const text = await resp.text();
    if (!resp.ok) {
      throw new Error(
        `join ${sessionId} failed: HTTP ${resp.status} ${text.slice(0, 200)}`,
      );
    }
    const parsed = JSON.parse(text) as { session_key?: string };
    if (!parsed.session_key) {
      throw new Error(`join ${sessionId} returned no session_key`);
    }
    return parsed.session_key;
  }

  /**
   * Open a `session.subscribe` NDJSON stream. `sinceEventId`, when set, is
   * passed as `params.since_event_id` so the server replays persisted events
   * after that id before switching to live — MAX relies on the server's own
   * replay→live dedup so no event is gapped or doubled.
   *
   * Returns a handle to abort the stream. Notifications and the lagged/end/error
   * lifecycle are delivered through `handlers`.
   */
  subscribe(
    sessionId: string,
    sessionKey: string,
    sinceEventId: string | undefined,
    handlers: SubscribeHandlers,
  ): SubscribeHandle {
    const controller = new AbortController();
    const params: Record<string, unknown> = {};
    if (sinceEventId !== undefined) params.since_event_id = sinceEventId;
    const rpc = {
      jsonrpc: "2.0",
      id: 1,
      method: "session.subscribe",
      params,
    };

    void this.runStream(sessionId, sessionKey, rpc, controller, handlers);
    return { close: () => controller.abort() };
  }

  private async runStream(
    sessionId: string,
    sessionKey: string,
    rpc: unknown,
    controller: AbortController,
    handlers: SubscribeHandlers,
  ): Promise<void> {
    let resp: globalThis.Response;
    try {
      resp = await this.fetchImpl(
        `${this.base}/api/v1/sessions/${sessionId}/rpc`,
        {
          method: "POST",
          headers: {
            authorization: `Bearer ${sessionKey}`,
            "content-type": "application/json",
            accept: "application/x-ndjson",
          },
          body: JSON.stringify(rpc),
          signal: controller.signal,
        },
      );
    } catch (err) {
      if (controller.signal.aborted) return;
      handlers.onError(err as Error);
      return;
    }

    if (!resp.ok || !resp.body) {
      const detail = resp.body ? await safeText(resp) : "";
      handlers.onError(
        new Error(
          `subscribe ${sessionId} failed: HTTP ${resp.status} ${detail}`,
        ),
      );
      return;
    }

    const buffer = new NdjsonBuffer();
    const decoder = new TextDecoder();
    const reader = resp.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        const chunk = decoder.decode(value, { stream: true });
        for (const line of buffer.push(chunk)) {
          if (dispatchLine(line, handlers)) return; // lagged → stop reading
        }
      }
      const tail = buffer.flush();
      if (tail && dispatchLine(tail, handlers)) return;
      handlers.onEnd();
    } catch (err) {
      if (controller.signal.aborted) return;
      handlers.onError(err as Error);
    } finally {
      reader.releaseLock();
    }
  }
}

/** Read a response body as text, swallowing any read error (best-effort detail). */
async function safeText(resp: globalThis.Response): Promise<string> {
  try {
    return await resp.text();
  } catch {
    return "";
  }
}

/** The `client_version` MAX declares on join — an observer sentinel. */
export const MAX_CLIENT_VERSION = "bae-max-observer/1";

/**
 * Route one NDJSON line to the right handler. Returns `true` if it was the
 * terminal "lagged" error (the caller must stop reading and reconnect).
 */
export function dispatchLine(
  line: string,
  handlers: SubscribeHandlers,
): boolean {
  let obj: Record<string, unknown>;
  try {
    obj = JSON.parse(line) as Record<string, unknown>;
  } catch {
    // A malformed line shouldn't kill the stream; skip it.
    return false;
  }

  // A `-32000 "lagged"` error notification carries no `id`.
  const error = obj.error as { code?: number; message?: string } | undefined;
  if (error && error.code === -32000 && obj.id === undefined) {
    if (typeof error.message === "string" && error.message.includes("lagged")) {
      handlers.onLagged();
      return true;
    }
  }

  if (obj.method === "session.event" && obj.params) {
    handlers.onEvent(obj.params as unknown as SessionEvent);
  }
  return false;
}
