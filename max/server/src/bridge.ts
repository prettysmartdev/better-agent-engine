//! The live observer bridge (work item 0007, section C).
//!
//! For each session a browser wants to watch, MAX opens exactly ONE upstream
//! `session.subscribe` NDJSON stream against baesrv's client port and fans its
//! events out to every browser WebSocket subscribed to that session id. It:
//!
//!   1. fetches the full persisted history via the admin events endpoint (works
//!      even for `closed`/`error` sessions the client port could never `join`);
//!   2. if the session is still open, mints/reuses a per-profile observer client
//!      key, `join`s once (caching `session_id -> session_key` in memory), and
//!      opens the single upstream `session.subscribe` stream;
//!   3. forwards each live event to all downstream sockets as
//!      `{type: "event", event}`, deduped per-socket against the history it was
//!      already sent;
//!   4. on the `-32000 "lagged"` notification, reconnects the upstream exactly
//!      once with `since_event_id` = the last event id it forwarded;
//!   5. when the upstream ends (session closed) closes downstream sockets cleanly
//!      with a distinguishable `{type: "session_ended"}` signal.
//!
//! It NEVER registers as a driver or sends a message — it only ever observes.

import type { WebSocket } from "ws";

import type { ObserverKeyEntry } from "./observerKeys.js";
import type {
  SessionEvent,
  SubscribeHandle,
  SubscribeHandlers,
} from "./clientPortClient.js";

/** The admin surface the bridge needs: session lookup + history. */
export interface BridgeAdmin {
  listSessions(query: {
    cursor?: string;
    limit?: number;
    state?: string;
  }): Promise<{ items: unknown[]; next_cursor: string | null }>;
  getAllSessionEvents(id: string): Promise<unknown[]>;
}

/** Provides a per-profile observer client key. */
export interface ObserverKeySource {
  observerKey(profileId: string): Promise<ObserverKeyEntry>;
}

/** The client-port operations the bridge uses — join + subscribe, nothing else. */
export interface UpstreamClient {
  join(sessionId: string, clientKey: string): Promise<string>;
  subscribe(
    sessionId: string,
    sessionKey: string,
    sinceEventId: string | undefined,
    handlers: SubscribeHandlers,
  ): SubscribeHandle;
}

/** Every message shape MAX pushes down a browser WebSocket. */
export type DownstreamMessage =
  | { type: "history"; events: SessionEvent[] }
  | { type: "event"; event: SessionEvent }
  | { type: "session_ended"; reason: string; message?: string }
  | { type: "error"; message: string };

/** `ws` numeric ready state for an open socket. */
const WS_OPEN = 1;
/** Normal-closure WebSocket code. */
const WS_NORMAL_CLOSE = 1000;

interface Downstream {
  socket: WebSocket;
  /** True once the initial history batch has been delivered. */
  ready: boolean;
  /** Live events that arrived while history was still loading. */
  pending: SessionEvent[];
  /** Event ids already sent to this socket (history + live), for dedup. */
  sentIds: Set<string>;
}

type SessionMode = "pending" | "live" | "terminal";

interface SessionState {
  downstream: Set<Downstream>;
  mode: SessionMode;
  /** Memoizes the one-time upstream startup so concurrent connects share it. */
  startPromise?: Promise<SessionMode>;
  handle?: SubscribeHandle;
  /** The last event id forwarded downstream — the lagged-reconnect anchor. */
  lastForwardedId?: string;
  ended: boolean;
  endReason?: string;
  endMessage?: string;
}

/**
 * Owns every live session subscription and the process-lifetime session-key
 * cache. Construct one per `max/server` process.
 */
export class ObserverBridge {
  private readonly sessions = new Map<string, SessionState>();
  /** `session_id -> session_key`, in-memory only, for the life of the process. */
  private readonly sessionKeys = new Map<string, string>();
  /** In-flight joins, so N concurrent viewers trigger exactly one `join`. */
  private readonly inflightJoins = new Map<string, Promise<string>>();
  /** `session_id -> profile_id`, learned from the admin sessions list. */
  private readonly profileIds = new Map<string, string>();

  constructor(
    private readonly admin: BridgeAdmin,
    private readonly observerKeys: ObserverKeySource,
    private readonly client: UpstreamClient,
  ) {}

  /**
   * Attach a browser WebSocket to `sessionId`. `profileIdHint`/`stateHint`
   * (from the WS query string) let the bridge skip scanning the admin sessions
   * list to learn the session's profile and state; both are optional. A
   * terminal `stateHint` only ever skips the (doomed) join for a closed/error
   * session — a wrong hint costs nothing beyond what the 409 fallback already
   * handles, since terminal states are final.
   */
  handleConnection(
    sessionId: string,
    socket: WebSocket,
    profileIdHint?: string,
    stateHint?: string,
  ): void {
    const state = this.getOrCreateSession(sessionId);
    const d: Downstream = {
      socket,
      ready: false,
      pending: [],
      sentIds: new Set(),
    };
    state.downstream.add(d);
    socket.on("close", () => this.removeDownstream(sessionId, d));
    socket.on("error", () => this.removeDownstream(sessionId, d));
    void this.initDownstream(sessionId, state, d, profileIdHint, stateHint);
  }

  /** Number of live upstream subscriptions (for diagnostics/tests). */
  activeSessionCount(): number {
    return this.sessions.size;
  }

  private getOrCreateSession(sessionId: string): SessionState {
    let state = this.sessions.get(sessionId);
    if (!state) {
      state = { downstream: new Set(), mode: "pending", ended: false };
      this.sessions.set(sessionId, state);
    }
    return state;
  }

  private async initDownstream(
    sessionId: string,
    state: SessionState,
    d: Downstream,
    profileIdHint?: string,
    stateHint?: string,
  ): Promise<void> {
    try {
      const history = (await this.admin.getAllSessionEvents(
        sessionId,
      )) as SessionEvent[];
      for (const e of history) d.sentIds.add(e.id);
      this.send(d.socket, { type: "history", events: history });

      const lastHistoryId =
        history.length > 0 ? history[history.length - 1]!.id : undefined;
      await this.ensureUpstream(
        sessionId,
        state,
        lastHistoryId,
        profileIdHint,
        stateHint,
      );

      // History is delivered; take everything buffered while it loaded, then
      // switch this socket to live.
      d.ready = true;
      this.flushPending(d);

      if (state.mode === "terminal" || state.ended) {
        this.send(d.socket, {
          type: "session_ended",
          reason: state.endReason ?? "closed",
          ...(state.endMessage ? { message: state.endMessage } : {}),
        });
        this.closeSocket(d.socket);
        this.removeDownstream(sessionId, d);
      }
    } catch (err) {
      this.send(d.socket, { type: "error", message: (err as Error).message });
      this.closeSocket(d.socket);
      this.removeDownstream(sessionId, d);
    }
  }

  private flushPending(d: Downstream): void {
    for (const e of d.pending) {
      if (d.sentIds.has(e.id)) continue;
      d.sentIds.add(e.id);
      this.send(d.socket, { type: "event", event: e });
    }
    d.pending = [];
  }

  /**
   * Start the single upstream stream for a session, at most once. Concurrent
   * callers share the same promise. Returns the resolved session mode.
   */
  private ensureUpstream(
    sessionId: string,
    state: SessionState,
    sinceEventId: string | undefined,
    profileIdHint?: string,
    stateHint?: string,
  ): Promise<SessionMode> {
    if (state.mode !== "pending") return Promise.resolve(state.mode);
    if (state.startPromise) return state.startPromise;
    state.startPromise = this.startUpstream(
      sessionId,
      state,
      sinceEventId,
      profileIdHint,
      stateHint,
    );
    return state.startPromise;
  }

  private async startUpstream(
    sessionId: string,
    state: SessionState,
    sinceEventId: string | undefined,
    profileIdHint?: string,
    stateHint?: string,
  ): Promise<SessionMode> {
    const resolved = await this.resolveSession(
      sessionId,
      profileIdHint,
      stateHint,
    );
    if (resolved === undefined) {
      // Not in the sessions list — nothing to observe live; history-only.
      state.mode = "terminal";
      return "terminal";
    }
    if (isTerminalState(resolved.state)) {
      // Already closed/error: history (delivered above) is the whole story.
      // No join is attempted — it could only 409 session_closed.
      state.mode = "terminal";
      return "terminal";
    }
    let sessionKey: string;
    try {
      const observer = await this.observerKeys.observerKey(resolved.profileId);
      sessionKey = await this.getSessionKey(sessionId, observer.key);
    } catch (err) {
      if (isSessionClosed(err)) {
        // The session went terminal between the state check and the join —
        // history is still fully readable, there is just no live tail.
        state.mode = "terminal";
        return "terminal";
      }
      throw err;
    }
    state.mode = "live";
    state.lastForwardedId = sinceEventId;
    this.openStream(sessionId, state, sessionKey, sinceEventId);
    return "live";
  }

  private openStream(
    sessionId: string,
    state: SessionState,
    sessionKey: string,
    sinceEventId: string | undefined,
  ): void {
    const handlers: SubscribeHandlers = {
      onEvent: (event) => {
        state.lastForwardedId = event.id;
        this.fanout(state, event);
      },
      onLagged: () => {
        // Reconnect exactly once, from the last id we actually forwarded, so no
        // event is gapped or double-delivered.
        state.handle?.close();
        this.openStream(sessionId, state, sessionKey, state.lastForwardedId);
      },
      onEnd: () => this.endSession(sessionId, state, "closed"),
      onError: (err) => this.endSession(sessionId, state, "error", err.message),
    };
    state.handle = this.client.subscribe(
      sessionId,
      sessionKey,
      sinceEventId,
      handlers,
    );
  }

  private fanout(state: SessionState, event: SessionEvent): void {
    for (const d of state.downstream) {
      if (!d.ready) {
        d.pending.push(event);
        continue;
      }
      if (d.sentIds.has(event.id)) continue;
      d.sentIds.add(event.id);
      this.send(d.socket, { type: "event", event });
    }
  }

  private endSession(
    sessionId: string,
    state: SessionState,
    reason: string,
    message?: string,
  ): void {
    if (state.ended) return;
    state.ended = true;
    state.endReason = reason;
    state.endMessage = message;
    for (const d of state.downstream) {
      // A socket still loading history handles its own terminal signal in
      // initDownstream's finalize step (preserving history→live→ended order).
      if (!d.ready) continue;
      this.send(d.socket, {
        type: "session_ended",
        reason,
        ...(message ? { message } : {}),
      });
      this.closeSocket(d.socket);
    }
    this.teardown(sessionId, state);
  }

  private removeDownstream(sessionId: string, d: Downstream): void {
    const state = this.sessions.get(sessionId);
    if (!state) return;
    state.downstream.delete(d);
    // The last viewer left: tear down the upstream stream but keep the cached
    // session key so a later re-observe reuses it without a second `join`.
    if (state.downstream.size === 0) {
      this.teardown(sessionId, state);
    }
  }

  private teardown(sessionId: string, state: SessionState): void {
    state.handle?.close();
    this.sessions.delete(sessionId);
  }

  /**
   * Resolve the profile id (cached) and current state for a session. Uses the
   * hints when given; otherwise pages the admin sessions list until it finds
   * the session. State is never cached — only the profile id is immutable.
   */
  private async resolveSession(
    sessionId: string,
    profileIdHint?: string,
    stateHint?: string,
  ): Promise<{ profileId: string; state?: string } | undefined> {
    if (profileIdHint) {
      this.profileIds.set(sessionId, profileIdHint);
      return { profileId: profileIdHint, state: stateHint };
    }
    const cached = this.profileIds.get(sessionId);
    if (cached) return { profileId: cached, state: stateHint };
    let cursor: string | undefined;
    do {
      const page = await this.admin.listSessions({ cursor, limit: 200 });
      for (const item of page.items as Array<{
        id: string;
        profile_id: string;
        state?: string;
      }>) {
        this.profileIds.set(item.id, item.profile_id);
        if (item.id === sessionId) {
          return { profileId: item.profile_id, state: item.state };
        }
      }
      cursor = page.next_cursor ?? undefined;
    } while (cursor);
    return undefined;
  }

  /**
   * Return the cached session key for a session, or `join` once to mint it.
   * Concurrent callers share a single in-flight join (no redundant
   * `session.join` event per viewer).
   */
  private getSessionKey(
    sessionId: string,
    observerKey: string,
  ): Promise<string> {
    const cached = this.sessionKeys.get(sessionId);
    if (cached) return Promise.resolve(cached);
    const inflight = this.inflightJoins.get(sessionId);
    if (inflight) return inflight;
    const promise = this.client
      .join(sessionId, observerKey)
      .then((key) => {
        this.sessionKeys.set(sessionId, key);
        this.inflightJoins.delete(sessionId);
        return key;
      })
      .catch((err) => {
        this.inflightJoins.delete(sessionId);
        throw err;
      });
    this.inflightJoins.set(sessionId, promise);
    return promise;
  }

  private send(socket: WebSocket, message: DownstreamMessage): void {
    if (socket.readyState !== WS_OPEN) return;
    socket.send(JSON.stringify(message));
  }

  private closeSocket(socket: WebSocket): void {
    try {
      socket.close(WS_NORMAL_CLOSE, "session ended");
    } catch {
      // Already closing/closed; ignore.
    }
  }
}

/** True if an error is baesrv rejecting a join because the session is terminal. */
function isSessionClosed(err: unknown): boolean {
  const message = (err as Error)?.message ?? "";
  return message.includes("409") || message.includes("session_closed");
}

/** True for the final session states, for which a join could only 409. */
function isTerminalState(state: string | undefined): boolean {
  return state === "closed" || state === "error";
}
