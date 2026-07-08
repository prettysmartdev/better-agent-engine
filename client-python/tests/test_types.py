"""Tests for the content model and the closed event union."""

from __future__ import annotations

import pytest

from bae_py import (
    EventType,
    Message,
    SessionEvent,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    describe_event,
)
from bae_py.types import parse_block, parse_content


def test_parse_and_roundtrip_content_blocks() -> None:
    raw = [
        {"type": "text", "text": "hi"},
        {"type": "tool_use", "id": "tu_1", "name": "t", "input": {"a": 1}},
        {"type": "tool_result", "tool_use_id": "tu_1", "content": "done"},
    ]
    blocks = parse_content(raw)
    assert isinstance(blocks[0], TextBlock)
    assert isinstance(blocks[1], ToolUseBlock)
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
    # The exact twenty-two strings from the wire contract (§8); WI 0006 added the
    # eight sandbox events to the original fourteen.
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
        "session.compaction",
        "session.sandbox.available",
        "session.sandbox.start",
        "session.sandbox.running",
        "session.sandbox.stop",
        "session.sandbox.stopped",
        "session.sandbox.error",
        "sandbox.request",
        "sandbox.response",
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
