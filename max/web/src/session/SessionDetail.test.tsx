import { describe, expect, it } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import SessionDetail from "./SessionDetail";
import type { SessionListItem } from "../api/types";
import type { ConnectFn, SocketLike } from "./useSessionStream";

class FakeSocket implements SocketLike {
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  close() {
    this.closed = true;
  }
  emit(frame: unknown) {
    act(() => this.onmessage?.({ data: JSON.stringify(frame) }));
  }
  fireClose() {
    act(() => this.onclose?.());
  }
}

const SESSION: SessionListItem = {
  id: "sess_1",
  profile_id: "pro_1",
  state: "open",
  client_version: "1.0",
  created_at: "2026-07-08T00:00:00Z",
  closed_at: null,
};

function ev(id: number, event_type: string, payload: unknown) {
  return {
    id,
    session_id: "sess_1",
    client_key_id: "key_1",
    event_type,
    payload,
    created_at: "2026-07-08T00:00:01Z",
  };
}

function renderDetail() {
  const socket = new FakeSocket();
  const connect: ConnectFn = () => socket;
  render(
    <SessionDetail session={SESSION} onBack={() => {}} connect={connect} />,
  );
  return socket;
}

describe("SessionDetail event graph", () => {
  it("opens the detail panel on both click and keyboard (Enter/Space)", () => {
    const socket = renderDetail();
    socket.emit({
      type: "history",
      events: [
        ev(1, "client.message.send", { foo: "bar" }),
        ev(2, "tool.call", { tool: "search" }),
      ],
    });

    // Click opens the panel for node 1.
    fireEvent.click(
      screen.getByRole("button", { name: /client.message.send event 1/ }),
    );
    expect(screen.getByTestId("event-payload")).toHaveTextContent(
      '"foo": "bar"',
    );

    // Keyboard (Enter) opens the SAME panel for node 2 — no pointer required.
    fireEvent.keyDown(
      screen.getByRole("button", { name: /tool.call event 2/ }),
      { key: "Enter" },
    );
    expect(screen.getByTestId("event-payload")).toHaveTextContent(
      '"tool": "search"',
    );

    // Space also activates.
    fireEvent.keyDown(
      screen.getByRole("button", { name: /client.message.send event 1/ }),
      { key: " " },
    );
    expect(screen.getByTestId("event-payload")).toHaveTextContent(
      '"foo": "bar"',
    );
  });

  it("appends live event nodes as they arrive", () => {
    const socket = renderDetail();
    socket.emit({ type: "history", events: [ev(1, "session.open", {})] });
    expect(
      screen.getByRole("button", { name: /session.open event 1/ }),
    ).toBeInTheDocument();

    socket.emit({
      type: "event",
      event: ev(2, "provider.request", { model: "x" }),
    });
    expect(
      screen.getByRole("button", { name: /provider.request event 2/ }),
    ).toBeInTheDocument();
  });

  it("shows 'session ended' when the stream ends, and never hangs", () => {
    const socket = renderDetail();
    socket.emit({ type: "history", events: [ev(1, "session.open", {})] });
    expect(screen.getByTestId("stream-status")).toHaveTextContent("live");

    socket.emit({ type: "session_ended", reason: "closed" });
    expect(screen.getByTestId("stream-status")).toHaveTextContent(
      "session ended",
    );
  });

  it("falls back to 'session ended' if the socket simply closes", () => {
    const socket = renderDetail();
    socket.emit({ type: "history", events: [] });
    socket.fireClose();
    expect(screen.getByTestId("stream-status")).toHaveTextContent(
      "session ended",
    );
  });
});
