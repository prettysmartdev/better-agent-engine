import { useEffect, useRef, useState } from "react";
import type { SessionEvent, WsFrame } from "../api/types";

export type StreamStatus = "connecting" | "streaming" | "ended" | "error";

/** Minimal socket surface, so tests can inject a fake in place of WebSocket. */
export interface SocketLike {
  onmessage: ((ev: { data: string }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  close(): void;
}

export type ConnectFn = (url: string) => SocketLike;

const defaultConnect: ConnectFn = (url) =>
  new WebSocket(url) as unknown as SocketLike;

/**
 * Build the observer WebSocket URL for a session on the same origin. The
 * profile id and state, both already known from the sessions list, ride along
 * as hints so the bridge can skip the admin-list scan — and, for a session
 * already closed/errored, skip the join it knows would 409.
 */
export function streamUrl(
  sessionId: string,
  profileId?: string,
  state?: string,
): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  const base = `${proto}//${window.location.host}/ws/sessions/${encodeURIComponent(sessionId)}`;
  const params = new URLSearchParams();
  if (profileId) params.set("profile_id", profileId);
  if (state) params.set("state", state);
  const query = params.toString();
  return query ? `${base}?${query}` : base;
}

export interface StreamState {
  events: SessionEvent[];
  status: StreamStatus;
  /** Present once the stream ends: why the upstream session finished. */
  endedReason?: "closed" | "error";
  errorMessage?: string;
}

/**
 * Subscribes to a session's observer WebSocket. The server sends a `history`
 * batch first, then live `event` frames (deduped server-side, so we append
 * blindly), and finally `session_ended` when the upstream stops. The socket
 * closing for any reason resolves to a terminal state — the view never hangs.
 */
export function useSessionStream(
  sessionId: string,
  profileId?: string,
  sessionState?: string,
  connect: ConnectFn = defaultConnect,
): StreamState {
  const [state, setState] = useState<StreamState>({
    events: [],
    status: "connecting",
  });
  const socketRef = useRef<SocketLike | null>(null);

  useEffect(() => {
    setState({ events: [], status: "connecting" });
    let done = false;

    let socket: SocketLike;
    try {
      socket = connect(streamUrl(sessionId, profileId, sessionState));
    } catch (e) {
      setState({
        events: [],
        status: "error",
        errorMessage:
          e instanceof Error ? e.message : "Could not open the event stream.",
      });
      return;
    }
    socketRef.current = socket;

    socket.onmessage = (ev) => {
      let frame: WsFrame;
      try {
        frame = JSON.parse(ev.data) as WsFrame;
      } catch {
        return;
      }
      setState((prev) => {
        switch (frame.type) {
          case "history":
            return { ...prev, events: frame.events, status: "streaming" };
          case "event":
            return {
              ...prev,
              events: [...prev.events, frame.event],
              status: "streaming",
            };
          case "session_ended":
            done = true;
            return {
              ...prev,
              status: "ended",
              endedReason: frame.reason,
              errorMessage: frame.message,
            };
          case "error":
            done = true;
            return { ...prev, status: "error", errorMessage: frame.message };
          default:
            return prev;
        }
      });
    };

    socket.onclose = () => {
      // Any close is terminal. If we didn't already get an explicit end frame,
      // treat it as the session having ended so the UI never hangs.
      if (!done) {
        setState((prev) =>
          prev.status === "ended" || prev.status === "error"
            ? prev
            : {
                ...prev,
                status: "ended",
                endedReason: prev.endedReason ?? "closed",
              },
        );
      }
    };

    socket.onerror = () => {
      if (!done) {
        setState((prev) =>
          prev.status === "ended"
            ? prev
            : {
                ...prev,
                status: "error",
                errorMessage: prev.errorMessage ?? "Stream error.",
              },
        );
      }
    };

    return () => {
      done = true;
      try {
        socket.close();
      } catch {
        /* ignore */
      }
      socketRef.current = null;
    };
  }, [sessionId, profileId, sessionState, connect]);

  return state;
}
