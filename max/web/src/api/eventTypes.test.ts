import { describe, expect, it } from "vitest";
import {
  categoryFor,
  categoryForEvent,
  EVENT_CATEGORIES,
  SYNTHETIC_COMPACTION_PREAMBLE,
  syntheticKind,
  syntheticLabel,
} from "./eventTypes";

// WI 0018 A8 — compaction event mappings (session.compaction.started/completed)
// and the compaction-preamble rendering label.

describe("compaction event category mappings", () => {
  it("maps session.compaction.started and .completed to the Compaction category", () => {
    expect(categoryFor("session.compaction.started")).toEqual(
      categoryFor("session.compaction.completed"),
    );
    const cat = categoryFor("session.compaction.started");
    expect(cat.key).toBe("compaction");
    expect(cat.label).toBe("Compaction");
  });

  it("lists Compaction among the stable legend order", () => {
    expect(EVENT_CATEGORIES.map((c) => c.key)).toContain("compaction");
  });

  it("falls back to Other for an unrecognized event_type", () => {
    expect(categoryFor("totally.new.type").key).toBe("unknown");
  });
});

describe("syntheticKind / syntheticLabel", () => {
  it("returns null for a non server.message.send event", () => {
    const event = {
      event_type: "session.compaction.started",
      payload: { trigger: "client", reason: "manual" },
    };
    expect(syntheticKind(event)).toBeNull();
    expect(syntheticLabel(event)).toBeNull();
  });

  it("returns null for an ordinary (non-synthetic) server.message.send", () => {
    const event = {
      event_type: "server.message.send",
      payload: { role: "assistant", content: [] },
    };
    expect(syntheticKind(event)).toBeNull();
    expect(syntheticLabel(event)).toBeNull();
  });

  it("labels the compaction preamble distinctly", () => {
    const event = {
      event_type: "server.message.send",
      payload: {
        role: "user",
        content: [
          {
            type: "text",
            text: "The earlier part of this conversation was compacted. A summary follows.",
          },
        ],
        synthetic: SYNTHETIC_COMPACTION_PREAMBLE,
      },
    };
    expect(syntheticKind(event)).toBe("compaction_preamble");
    expect(syntheticLabel(event)).toBe("Compaction marker (system-generated)");
  });

  it("labels abandoned tool results distinctly", () => {
    const event = {
      event_type: "server.message.send",
      payload: {
        role: "user",
        content: [],
        synthetic: "abandoned_tool_results",
      },
    };
    expect(syntheticKind(event)).toBe("abandoned_tool_results");
    expect(syntheticLabel(event)).toBe(
      "Abandoned tool results (system-generated)",
    );
  });

  it("falls back to a generic label for an unrecognized synthetic kind", () => {
    const event = {
      event_type: "server.message.send",
      payload: { role: "user", content: [], synthetic: "future_kind" },
    };
    expect(syntheticLabel(event)).toBe(
      "System-generated message (future_kind)",
    );
  });

  it("tolerates a non-object payload without throwing", () => {
    const event = { event_type: "server.message.send", payload: null };
    expect(syntheticKind(event)).toBeNull();
    const eventStr = { event_type: "server.message.send", payload: "oops" };
    expect(syntheticKind(eventStr)).toBeNull();
  });
});

describe("categoryForEvent", () => {
  it("resolves the compaction preamble to Compaction, not Client turn", () => {
    const preamble = {
      event_type: "server.message.send",
      payload: {
        role: "user",
        content: [],
        synthetic: SYNTHETIC_COMPACTION_PREAMBLE,
      },
    };
    expect(categoryForEvent(preamble).key).toBe("compaction");
  });

  it("resolves an ordinary server.message.send to Client turn", () => {
    const ordinary = {
      event_type: "server.message.send",
      payload: { role: "assistant", content: [] },
    };
    expect(categoryForEvent(ordinary).key).toBe("client-turn");
  });

  it("resolves session.compaction.started/completed to Compaction directly", () => {
    expect(
      categoryForEvent({
        event_type: "session.compaction.started",
        payload: {},
      }).key,
    ).toBe("compaction");
    expect(
      categoryForEvent({
        event_type: "session.compaction.completed",
        payload: {},
      }).key,
    ).toBe("compaction");
  });
});
