"""Tests for the content model and the closed event union."""

from __future__ import annotations

import pytest

from bae_py import (
    AutoCompaction,
    ClientCompaction,
    EventType,
    Message,
    ServerMessagePayload,
    SessionCompactionCompleted,
    SessionCompactionStarted,
    SessionEvent,
    SYNTHETIC_COMPACTION_PREAMBLE,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    describe_event,
)
from bae_py.types import parse_block, parse_content


def test_parse_and_roundtrip_content_blocks() -> None:
    raw = [
        {"type": "text", "text": "hi"},
        {
            "type": "tool_use",
            "id": "tu_1",
            "name": "t",
            "input": {"a": 1},
            "dispatch": "mcp",
        },
        {"type": "tool_result", "tool_use_id": "tu_1", "content": "done"},
    ]
    blocks = parse_content(raw)
    assert isinstance(blocks[0], TextBlock)
    assert isinstance(blocks[1], ToolUseBlock)
    assert blocks[1].dispatch == "mcp"
    assert isinstance(blocks[2], ToolResultBlock)
    # to_wire is faithful.
    assert blocks[1].to_wire() == {
        "type": "tool_use",
        "id": "tu_1",
        "name": "t",
        "input": {"a": 1},
    }


def test_string_content_is_passed_through() -> None:
    assert parse_content("plain text") == "plain text"
    assert parse_content(None) == []


def test_unknown_block_type_fails_loudly() -> None:
    with pytest.raises(ValueError, match="unknown content block type"):
        parse_block({"type": "image", "url": "x"})


def test_message_from_wire_and_to_wire() -> None:
    msg = Message.from_wire({"role": "assistant", "content": [{"type": "text", "text": "yo"}]})
    assert msg.role == "assistant"
    assert msg.text() == "yo"
    assert msg.to_wire()["content"][0]["type"] == "text"


def test_event_type_is_closed_and_complete() -> None:
    # The exact twenty-eight strings from the wire contract (§8); WI 0006 added
    # the eight sandbox events to the original fourteen, WI 0010 the five
    # subagent lifecycle events.
    assert {e.value for e in EventType} == {
        "client.message.send",
        "server.message.send",
        "provider.request",
        "provider.response",
        "tool.call",
        "tool.result",
        "mcp.request",
        "mcp.response",
        "session.open",
        "session.join",
        "session.driver.register",
        "session.close",
        "session.error",
        "session.compaction.started",
        "session.compaction.completed",
        "session.sandbox.available",
        "session.sandbox.start",
        "session.sandbox.running",
        "session.sandbox.stop",
        "session.sandbox.stopped",
        "session.sandbox.error",
        "sandbox.request",
        "sandbox.response",
        "session.subagent.start",
        "session.subagent.running",
        "session.subagent.completed",
        "session.subagent.failed",
        "session.subagent.cancelled",
    }


def test_unknown_event_type_fails_loudly() -> None:
    with pytest.raises(ValueError):
        SessionEvent.from_wire(
            {
                "id": "evt_1",
                "session_id": "ses_1",
                "client_key_id": None,
                "event_type": "totally.new.type",
                "payload": {},
                "created_at": "2026-07-06T00:00:00Z",
            }
        )


def test_describe_event_covers_every_type() -> None:
    # describe_event's match is exhaustive; every member yields a non-empty
    # description (and reaching the assert_never arm is impossible).
    for et in EventType:
        event = SessionEvent(
            id="evt_1",
            session_id="ses_1",
            client_key_id=None,
            event_type=et,
            payload={},
            created_at="2026-07-06T00:00:00Z",
        )
        assert describe_event(event)


# ---------------------------------------------------------------------------
# Compaction (WI 0018 A8) — contracts.md §1.4/§1.5/§1.11/§1.12, wire shapes
# shared with the server and the other two SDKs.
# ---------------------------------------------------------------------------


def test_auto_compaction_serializes_exactly() -> None:
    assert AutoCompaction(size=128_000).to_wire() == {"mode": "auto", "size": 128_000}


def test_client_compaction_omits_prompt_key_when_none() -> None:
    wire = ClientCompaction().to_wire()
    assert wire == {"mode": "client"}
    assert "prompt" not in wire


def test_client_compaction_with_prompt_serializes_exactly() -> None:
    wire = ClientCompaction(prompt="Summarize focusing on open TODOs.").to_wire()
    assert wire == {"mode": "client", "prompt": "Summarize focusing on open TODOs."}


# The shared `session.compaction.completed` fixture (contracts.md §1.4).
COMPLETED_EVENT_WIRE = {
    "id": "evt_01completed",
    "session_id": "ses_01example",
    "client_key_id": "key_01example",
    "event_type": "session.compaction.completed",
    "payload": {
        "preamble_event_id": "evt_01preamble",
        "summary_event_id": "evt_01summary",
        "compacted_message_count": 17,
        "input_tokens": 42100,
        "summary_tokens": 900,
    },
    "created_at": "2026-09-23T18:26:10.000Z",
}

# The pre-A1 shape: no `preamble_event_id` key at all.
COMPLETED_EVENT_LEGACY_WIRE = {
    "id": "evt_01completedold",
    "session_id": "ses_01example",
    "client_key_id": "key_01example",
    "event_type": "session.compaction.completed",
    "payload": {
        "summary_event_id": "evt_01summaryold",
        "compacted_message_count": 4,
        "input_tokens": None,
        "summary_tokens": None,
    },
    "created_at": "2026-09-01T10:00:00.000Z",
}

# The `synthetic` compaction-preamble fixture (contracts.md §1.2).
PREAMBLE_EVENT_WIRE = {
    "id": "evt_01preamble",
    "session_id": "ses_01example",
    "client_key_id": "key_01example",
    "event_type": "server.message.send",
    "payload": {
        "role": "user",
        "content": [
            {
                "type": "text",
                "text": "The earlier part of this conversation was compacted. A summary follows.",
            }
        ],
        "synthetic": "compaction_preamble",
    },
    "created_at": "2026-09-23T18:26:09.998Z",
}

# The ordinary summary event: no `synthetic` key at all.
SUMMARY_EVENT_WIRE = {
    "id": "evt_01summary",
    "session_id": "ses_01example",
    "client_key_id": "key_01example",
    "event_type": "server.message.send",
    "payload": {
        "role": "assistant",
        "content": [{"type": "text", "text": "SUMMARY of everything"}],
    },
    "created_at": "2026-09-23T18:26:09.999Z",
}

STARTED_MANUAL_WIRE = {
    "id": "evt_01startedmanual",
    "session_id": "ses_01example",
    "client_key_id": "key_01example",
    "event_type": "session.compaction.started",
    "payload": {
        "trigger": "client",
        "reason": "manual",
        "token_count": 1100,
        "threshold_tokens": None,
    },
    "created_at": "2026-09-23T18:26:08.000Z",
}

STARTED_RETRY_AFTER_FAILURE_WIRE = {
    "id": "evt_01started",
    "session_id": "ses_01example",
    "client_key_id": "key_01example",
    "event_type": "session.compaction.started",
    "payload": {
        "trigger": "auto",
        "reason": "retry_after_failure",
        "token_count": 140010,
        "threshold_tokens": 128000,
    },
    "created_at": "2026-09-23T18:26:08.000Z",
}


def test_completed_payload_preamble_event_id_round_trips() -> None:
    completed = SessionCompactionCompleted.from_wire(COMPLETED_EVENT_WIRE)
    assert completed.id == "evt_01completed"
    assert completed.payload.preamble_event_id == "evt_01preamble"
    assert completed.payload.summary_event_id == "evt_01summary"
    assert completed.payload.compacted_message_count == 17
    assert completed.payload.input_tokens == 42100
    assert completed.payload.summary_tokens == 900


def test_completed_payload_missing_preamble_event_id_defaults_to_none() -> None:
    # A pre-A1 event never dropped mid-parse: `preamble_event_id` just defaults.
    completed = SessionCompactionCompleted.from_wire(COMPLETED_EVENT_LEGACY_WIRE)
    assert completed.payload.preamble_event_id is None
    assert completed.payload.summary_event_id == "evt_01summaryold"
    assert completed.payload.input_tokens is None
    assert completed.payload.summary_tokens is None


def test_started_manual_matches_fixture() -> None:
    started = SessionCompactionStarted.from_event(SessionEvent.from_wire(STARTED_MANUAL_WIRE))
    assert started.payload.trigger == "client"
    assert started.payload.reason == "manual"
    assert started.payload.token_count == 1100
    assert started.payload.threshold_tokens is None


def test_started_retry_after_failure_matches_fixture() -> None:
    started = SessionCompactionStarted.from_event(
        SessionEvent.from_wire(STARTED_RETRY_AFTER_FAILURE_WIRE)
    )
    assert started.payload.trigger == "auto"
    assert started.payload.reason == "retry_after_failure"
    assert started.payload.token_count == 140010
    assert started.payload.threshold_tokens == 128000


def test_preamble_payload_has_synthetic_and_user_role() -> None:
    event = SessionEvent.from_wire(PREAMBLE_EVENT_WIRE)
    payload = ServerMessagePayload.from_payload(event.payload)
    assert payload.role == "user"
    assert payload.synthetic == SYNTHETIC_COMPACTION_PREAMBLE
    assert payload.is_compaction_preamble
    assert describe_event(event) == "server wrote the compaction preamble (synthetic)"


def test_summary_payload_has_no_synthetic_field() -> None:
    event = SessionEvent.from_wire(SUMMARY_EVENT_WIRE)
    payload = ServerMessagePayload.from_payload(event.payload)
    assert payload.role == "assistant"
    assert payload.synthetic is None
    assert not payload.is_compaction_preamble
    assert describe_event(event) == "server sent an assistant turn"
