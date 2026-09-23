import { describe, expect, it } from "vitest";

import {
  describeEvent,
  messageText,
  toMessage,
  toolUses,
  type CompactionConfig,
  type CompactParams,
  type ProviderResponsePayload,
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
        unsandboxed: false,
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

  it("labels the compaction preamble distinctly from other synthetic messages", () => {
    const preamble: SessionEvent = {
      id: "evt_01preamble",
      session_id: "ses_1",
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
      created_at: "t",
    };
    expect(describeEvent(preamble)).toBe(
      "server: compaction preamble (synthetic)",
    );

    const abandoned: SessionEvent = {
      id: "evt_5",
      session_id: "ses_1",
      client_key_id: "key_1",
      event_type: "server.message.send",
      payload: {
        role: "user",
        content: [],
        synthetic: "abandoned_tool_results",
      },
      created_at: "t",
    };
    expect(describeEvent(abandoned)).toBe(
      "server: synthetic user message (abandoned_tool_results)",
    );

    const ordinary: SessionEvent = {
      id: "evt_6",
      session_id: "ses_1",
      client_key_id: "key_1",
      event_type: "server.message.send",
      payload: { role: "assistant", content: [] },
      created_at: "t",
    };
    expect(describeEvent(ordinary)).toBe("server → client: assistant turn");
  });
});

// ---------------------------------------------------------------------------
// Compaction wire types (WI 0018 A8) — contracts.md §1.4/§1.5/§1.11/§1.12.
// ---------------------------------------------------------------------------

describe("CompactionConfig / CompactParams shapes", () => {
  it("accepts and round-trips the three configured forms exactly", () => {
    const auto: CompactionConfig = { mode: "auto", size: 128000 };
    expect(JSON.parse(JSON.stringify(auto))).toEqual({
      mode: "auto",
      size: 128000,
    });

    // A `{mode:"client"}` literal has no `prompt` key at all — the type
    // permits omitting it entirely, never `prompt: null`.
    const clientNoPrompt: CompactionConfig = { mode: "client" };
    expect(clientNoPrompt).not.toHaveProperty("prompt");
    expect(JSON.parse(JSON.stringify(clientNoPrompt))).toEqual({
      mode: "client",
    });

    const clientWithPrompt: CompactionConfig = {
      mode: "client",
      prompt: "Summarize focusing on open TODOs.",
    };
    expect(JSON.parse(JSON.stringify(clientWithPrompt))).toEqual({
      mode: "client",
      prompt: "Summarize focusing on open TODOs.",
    });
  });

  it("CompactParams omits prompt when absent and carries it verbatim when set", () => {
    const noPrompt: CompactParams = {};
    expect(JSON.parse(JSON.stringify(noPrompt))).toEqual({});

    const withPrompt: CompactParams = { prompt: "…" };
    expect(JSON.parse(JSON.stringify(withPrompt))).toEqual({ prompt: "…" });
  });
});

describe("session.compaction.* payload round-trips (shared fixtures)", () => {
  it("preserves preamble_event_id when present", () => {
    const completed: SessionEvent = {
      id: "evt_01completed",
      session_id: "ses_01example",
      client_key_id: "key_01example",
      event_type: "session.compaction.completed",
      payload: {
        preamble_event_id: "evt_01preamble",
        summary_event_id: "evt_01summary",
        compacted_message_count: 17,
        input_tokens: 42100,
        summary_tokens: 900,
      },
      created_at: "2026-09-23T18:26:10.000Z",
    };
    expect(
      JSON.parse(JSON.stringify(completed)).payload.preamble_event_id,
    ).toBe("evt_01preamble");
    expect(describeEvent(completed)).toBe("session compaction completed");
  });

  it("tolerates a pre-A1 event with no preamble_event_id key at all", () => {
    // Legacy fixture: `preamble_event_id` is optional, so a server that
    // predates it is a perfectly valid `SessionCompactionCompletedPayload`.
    const legacy: SessionEvent = {
      id: "evt_01completedold",
      session_id: "ses_01example",
      client_key_id: "key_01example",
      event_type: "session.compaction.completed",
      payload: {
        summary_event_id: "evt_01summaryold",
        compacted_message_count: 4,
        input_tokens: null,
        summary_tokens: null,
      },
      created_at: "2026-09-01T10:00:00.000Z",
    };
    expect(legacy.payload.preamble_event_id).toBeUndefined();
    expect(JSON.parse(JSON.stringify(legacy)).payload).not.toHaveProperty(
      "preamble_event_id",
    );
  });

  it("started carries retry_after_failure only with trigger:auto", () => {
    const retry: SessionEvent = {
      id: "evt_01started",
      session_id: "ses_01example",
      client_key_id: "key_01example",
      event_type: "session.compaction.started",
      payload: {
        trigger: "auto",
        reason: "retry_after_failure",
        token_count: 140010,
        threshold_tokens: 128000,
      },
      created_at: "2026-09-23T18:26:08.000Z",
    };
    expect(describeEvent(retry)).toBe("session compaction started");

    const manual: SessionEvent = {
      id: "evt_01startedmanual",
      session_id: "ses_01example",
      client_key_id: "key_01example",
      event_type: "session.compaction.started",
      payload: {
        trigger: "client",
        reason: "manual",
        token_count: 1100,
        threshold_tokens: null,
      },
      created_at: "2026-09-23T18:26:08.000Z",
    };
    expect(manual.payload.threshold_tokens).toBeNull();
  });
});

describe("ProviderResponsePayload ok-variant exposes usage (type test)", () => {
  it("assigns a usage-bearing object to the ok:true arm with no cast", () => {
    // The assignment itself is the type assertion: if `usage` were removed
    // from the `ok:true` arm of `ProviderResponsePayload`, this literal would
    // stop type-checking. The runtime expectations below back it up.
    const okWithUsage: Extract<ProviderResponsePayload, { ok: true }> = {
      attempt: 0,
      kind: "primary",
      provider: "anthropic",
      ok: true,
      status: 200,
      body: { role: "assistant", stop_reason: "end_turn" },
      usage: { input_tokens: 900, output_tokens: 200 },
      purpose: "compaction",
    };
    const usage:
      { input_tokens: number; output_tokens: number } | null | undefined =
      okWithUsage.usage;
    expect(usage).toEqual({ input_tokens: 900, output_tokens: 200 });

    // `usage` is optional/nullable — both forms must still type-check.
    const okNullUsage: Extract<ProviderResponsePayload, { ok: true }> = {
      attempt: 0,
      kind: "fallback",
      provider: "openai",
      ok: true,
      status: 200,
      body: {},
      usage: null,
    };
    expect(okNullUsage.usage).toBeNull();
  });
});
