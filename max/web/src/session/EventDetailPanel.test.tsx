import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import EventDetailPanel from "./EventDetailPanel";
import type { SessionEvent } from "../api/types";

const preambleEvent: SessionEvent = {
  id: 1,
  session_id: "sess_1",
  client_key_id: "key_1",
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
  created_at: "2026-07-08T00:00:00Z",
};

const ordinaryEvent: SessionEvent = {
  id: 2,
  session_id: "sess_1",
  client_key_id: "key_1",
  event_type: "server.message.send",
  payload: { role: "assistant", content: [{ type: "text", text: "hi" }] },
  created_at: "2026-07-08T00:00:01Z",
};

describe("EventDetailPanel — compaction preamble rendering", () => {
  it("shows an Origin row with the compaction-marker label", () => {
    render(<EventDetailPanel event={preambleEvent} onClose={vi.fn()} />);
    const origin = screen.getByTestId("event-synthetic");
    expect(origin.textContent).toBe("Compaction marker (system-generated)");
  });

  it("labels the category Compaction for a compaction preamble", () => {
    render(<EventDetailPanel event={preambleEvent} onClose={vi.fn()} />);
    expect(screen.getByText("Compaction")).toBeTruthy();
  });

  it("omits the Origin row for an ordinary (non-synthetic) event", () => {
    render(<EventDetailPanel event={ordinaryEvent} onClose={vi.fn()} />);
    expect(screen.queryByTestId("event-synthetic")).toBeNull();
  });
});
