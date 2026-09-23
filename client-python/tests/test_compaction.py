"""Offline compaction tests: the session-open `compaction` option and
`Session.compact()`, all against the scripted mock transport (WI 0018 A8;
contracts.md §1.11/§1.12).
"""

from __future__ import annotations

from bae_py import (
    AutoCompaction,
    ClientCompaction,
    Config,
    Harness,
    Hooks,
    RpcError,
    SessionCompactionCompleted,
)
from mock_transport import (
    MockTransport,
    connect_response,
    rpc_error_frame,
    rpc_notification,
    rpc_terminal,
)

import pytest


def _config() -> Config:
    return Config(server_url="http://test", client_key="bae_client", client_version="9.9.9")


# The shared `session.compaction.completed` terminal-result fixture (contracts.md §1.12).
COMPLETED_RESULT = {
    "id": "evt_01completed",
    "session_id": "ses_test",
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

STARTED_EVENT = {
    "id": "evt_01startedmanual",
    "session_id": "ses_test",
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

PREAMBLE_EVENT = {
    "id": "evt_01preamble",
    "session_id": "ses_test",
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


# ---------------------------------------------------------------------------
# `compaction` at session-open — omitted entirely when unset, never on join().
# ---------------------------------------------------------------------------


async def test_connect_omits_compaction_key_when_unset() -> None:
    transport = MockTransport(script=[connect_response()])
    await Harness(_config(), transport=transport).connect()
    assert "compaction" not in transport.requests[0].json


async def test_connect_serializes_auto_compaction_exactly() -> None:
    transport = MockTransport(script=[connect_response()])
    await Harness(_config(), transport=transport, compaction=AutoCompaction(size=128_000)).connect()
    assert transport.requests[0].json["compaction"] == {"mode": "auto", "size": 128_000}


async def test_connect_serializes_client_compaction_no_and_with_prompt() -> None:
    transport = MockTransport(script=[connect_response(), connect_response()])
    await Harness(_config(), transport=transport, compaction=ClientCompaction()).connect()
    assert transport.requests[0].json["compaction"] == {"mode": "client"}
    assert "prompt" not in transport.requests[0].json["compaction"]

    await Harness(
        _config(),
        transport=transport,
        compaction=ClientCompaction(prompt="Summarize focusing on open TODOs."),
    ).connect()
    assert transport.requests[1].json["compaction"] == {
        "mode": "client",
        "prompt": "Summarize focusing on open TODOs.",
    }


async def test_set_compaction_updates_what_connect_sends() -> None:
    transport = MockTransport(script=[connect_response()])
    harness = Harness(_config(), transport=transport)
    harness.set_compaction(AutoCompaction(size=5000))
    await harness.connect()
    assert transport.requests[0].json["compaction"] == {"mode": "auto", "size": 5000}


async def test_join_never_sends_compaction_even_when_configured() -> None:
    transport = MockTransport(script=[connect_response()])
    harness = Harness(_config(), transport=transport, compaction=AutoCompaction(size=128_000))
    await harness.join("ses_existing")
    req = transport.requests[0]
    assert req.url == "http://test/api/v1/sessions/ses_existing/join"
    assert "compaction" not in req.json


# ---------------------------------------------------------------------------
# `Session.compact` request/response shape.
# ---------------------------------------------------------------------------


async def test_compact_sends_empty_params_when_no_prompt() -> None:
    transport = MockTransport(script=[connect_response(), [rpc_terminal(COMPLETED_RESULT, id=1)]])
    session = await Harness(_config(), transport=transport).connect()

    await session.compact()

    compact_req = transport.requests[1]
    assert compact_req.json["method"] == "session.compact"
    assert compact_req.json["params"] == {}


async def test_compact_sends_prompt_exactly_when_given() -> None:
    transport = MockTransport(script=[connect_response(), [rpc_terminal(COMPLETED_RESULT, id=1)]])
    session = await Harness(_config(), transport=transport).connect()

    await session.compact("one-off prompt")

    compact_req = transport.requests[1]
    assert compact_req.json["params"] == {"prompt": "one-off prompt"}


async def test_compact_returns_typed_completed_record_from_fixture() -> None:
    transport = MockTransport(script=[connect_response(), [rpc_terminal(COMPLETED_RESULT, id=1)]])
    session = await Harness(_config(), transport=transport).connect()

    completed = await session.compact()

    assert isinstance(completed, SessionCompactionCompleted)
    assert completed.id == "evt_01completed"
    assert completed.payload.preamble_event_id == "evt_01preamble"
    assert completed.payload.summary_event_id == "evt_01summary"
    assert completed.payload.compacted_message_count == 17
    assert completed.payload.input_tokens == 42100
    assert completed.payload.summary_tokens == 900


async def test_compact_runs_on_event_for_every_notification_in_order() -> None:
    seen: list[str] = []
    transport = MockTransport(
        script=[
            connect_response(),
            [
                rpc_notification(STARTED_EVENT),
                rpc_notification(PREAMBLE_EVENT),
                rpc_terminal(COMPLETED_RESULT, id=1),
            ],
        ]
    )
    hooks = Hooks(on_event=lambda ev: seen.append(ev.event_type.value))
    session = await Harness(_config(), transport=transport, hooks=hooks).connect()

    completed = await session.compact()

    assert seen == ["session.compaction.started", "server.message.send"]
    assert completed.payload.compacted_message_count == 17


async def test_compact_surfaces_turn_in_progress_error() -> None:
    transport = MockTransport(
        script=[
            connect_response(),
            [
                rpc_error_frame(
                    -32020, "turn in progress: resolve the paused turn before compacting"
                )
            ],
        ]
    )
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(RpcError) as excinfo:
        await session.compact()
    assert excinfo.value.code == -32020
    assert "turn in progress" in str(excinfo.value)


async def test_compact_raises_when_stream_ends_without_terminal_frame() -> None:
    transport = MockTransport(script=[connect_response(), [rpc_notification(STARTED_EVENT)]])
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(RpcError) as excinfo:
        await session.compact()
    assert excinfo.value.code == -32603


async def test_compact_raises_on_terminal_result_missing_required_fields() -> None:
    malformed = {**COMPLETED_RESULT, "payload": {"preamble_event_id": "evt_01preamble"}}
    transport = MockTransport(script=[connect_response(), [rpc_terminal(malformed)]])
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(RpcError) as excinfo:
        await session.compact()
    assert excinfo.value.code == -32603
    assert "malformed session.compact result" in str(excinfo.value)
