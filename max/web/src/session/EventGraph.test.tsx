import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import EventGraph from "./EventGraph";
import type { SessionEvent } from "../api/types";

function makeEvents(n: number): SessionEvent[] {
  return Array.from({ length: n }, (_, i) => ({
    id: i + 1,
    session_id: "sess_1",
    client_key_id: "key_1",
    event_type: "provider.request",
    payload: { i },
    created_at: "2026-07-08T00:00:00Z",
  }));
}

describe("EventGraph", () => {
  it("virtualizes: thousands of events do not all mount at once", () => {
    render(
      <EventGraph
        events={makeEvents(5000)}
        selectedId={null}
        onSelect={() => {}}
      />,
    );
    // Only a small viewport-sized window (plus overscan) is mounted.
    const rendered = screen.getAllByRole("button");
    expect(rendered.length).toBeGreaterThan(0);
    expect(rendered.length).toBeLessThan(60);
  });

  it("activates a node with Enter and Space via keyboard", () => {
    const onSelect = vi.fn();
    render(
      <EventGraph
        events={makeEvents(3)}
        selectedId={null}
        onSelect={onSelect}
      />,
    );
    const node = screen.getByRole("button", {
      name: /provider.request event 1/,
    });

    fireEvent.keyDown(node, { key: "Enter" });
    fireEvent.keyDown(node, { key: " " });
    expect(onSelect).toHaveBeenCalledTimes(2);
    expect(onSelect).toHaveBeenLastCalledWith(
      expect.objectContaining({ id: 1 }),
    );
  });
});
