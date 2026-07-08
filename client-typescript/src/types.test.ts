import { describe, expect, it } from "vitest";

import {
  describeEvent,
  messageText,
  toMessage,
  toolUses,
  type SessionEvent,
} from "./types.js";

describe("message helpers", () => {
  it("normalizes a string into a user message", () => {
    expect(toMessage("hi")).toEqual({ role: "user", content: "hi" });
    const m = { role: "assistant", content: "kept" };
    expect(toMessage(m)).toBe(m);
  });

  it("messageText concatenates text blocks and passes strings through", () => {
    expect(messageText({ role: "assistant", content: "plain" })).toBe("plain");
    expect(
      messageText({
        role: "assistant",
        content: [
          { type: "text", text: "a" },
          { type: "tool_use", id: "tu", name: "t", input: {} },
          { type: "text", text: "b" },
        ],
      }),
    ).toBe("ab");
  });

  it("toolUses extracts tool_use blocks; empty means the loop ends", () => {
    expect(toolUses({ role: "assistant", content: "no tools" })).toEqual([]);
    expect(
      toolUses({
        role: "assistant",
        content: [
          {
            type: "tool_use",
            id: "tu_1",
            name: "get_time",
            input: { tz: "utc" },
          },
        ],
      }),
    ).toEqual([{ id: "tu_1", name: "get_time", input: { tz: "utc" } }]);
  });
});

describe("describeEvent", () => {
  it("describes each event via the exhaustive discriminated union", () => {
    const open: SessionEvent = {
      id: "evt_1",
      session_id: "ses_1",
      client_key_id: "key_1",
      event_type: "session.open",
      payload: {
        client_version: "1.0.0",
        tools: ["get_time"],
        sandbox_tools: [],
      },
      created_at: "t",
    };
    expect(describeEvent(open)).toBe("session opened");

    const toolCall: SessionEvent = {
      id: "evt_2",
      session_id: "ses_1",
      client_key_id: null,
      event_type: "tool.call",
      payload: { id: "tu_1", name: "get_time", input: {}, dispatch: "client" },
      created_at: "t",
    };
    expect(describeEvent(toolCall)).toBe("tool call get_time (client)");
  });

  it("describes the sandbox lifecycle and dispatch events without throwing", () => {
    const running: SessionEvent = {
      id: "evt_3",
      session_id: "ses_1",
      client_key_id: "key_1",
      event_type: "session.sandbox.running",
      payload: {
        image: "python:3.12",
        sandbox_id: "c1",
        dispatch: "remote",
      },
      created_at: "t",
    };
    expect(describeEvent(running)).toBe(
      "sandbox running (python:3.12, remote)",
    );

    const response: SessionEvent = {
      id: "evt_4",
      session_id: "ses_1",
      client_key_id: null,
      event_type: "sandbox.response",
      payload: {
        sandbox_id: "c1",
        ok: true,
        result: { stdout: "hi", stderr: "", exit_code: 0 },
      },
      created_at: "t",
    };
    expect(describeEvent(response)).toBe("sandbox response (ok=true)");
  });
});
