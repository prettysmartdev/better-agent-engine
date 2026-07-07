"""Cross-SDK MCP parity.

The three client SDKs (Rust, TypeScript, Python) must observe an IDENTICAL
ordered live event sequence for the same scripted MCP-enabled turn, and must
parse the real (non-stub) ``mcp.request`` / ``mcp.response`` payload shapes. The
canonical sequence below MUST stay byte-for-byte identical to the arrays in:

  - client-rust/src/mcp_parity.rs           (MCP_PARITY_SEQUENCE)
  - client-typescript/src/harness.test.ts   (MCP_PARITY_SEQUENCE)

All offline: a scripted mock transport, no server and no API keys.
"""

from __future__ import annotations

from typing import Any

from bae_py import (
    Config,
    Harness,
    Hooks,
    McpRequestPayload,
    McpResponsePayload,
    SessionEvent,
)
from mock_transport import MockTransport, connect_response, rpc_notification, rpc_terminal

# The canonical live-notification sequence for the scripted MCP turn.
MCP_PARITY_SEQUENCE = [
    "provider.request",
    "provider.response",
    "tool.call",
    "mcp.request",
    "mcp.response",
    "tool.result",
    "provider.request",
    "provider.response",
    "server.message.send",
]


def _config() -> Config:
    return Config(server_url="http://test", client_key="bae_client", client_version="9.9.9")


def _event(event_type: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": f"evt_{event_type}",
        "session_id": "ses_test",
        "client_key_id": None,
        "event_type": event_type,
        "payload": payload,
        "created_at": "t",
    }


def _mcp_scenario_frames() -> list[dict[str, Any]]:
    """The scripted MCP turn: the live notifications, then the terminal result."""
    echo_content = [{"type": "text", "text": "echo: x"}]
    notifs = [
        rpc_notification(_event("provider.request", {"attempt": 0})),
        rpc_notification(_event("provider.response", {"ok": True, "status": 200})),
        rpc_notification(
            _event(
                "tool.call",
                {
                    "dispatch": "mcp",
                    "name": "remote_search",
                    "server_name": "echo",
                    "input": {"q": "x"},
                },
            )
        ),
        rpc_notification(
            _event(
                "mcp.request",
                {
                    "method": "tools/call",
                    "server_name": "echo",
                    "tool": "remote_search",
                    "input": {"q": "x"},
                },
            )
        ),
        rpc_notification(
            _event(
                "mcp.response",
                {
                    "server_name": "echo",
                    "ok": True,
                    "result": {"content": echo_content, "isError": False},
                },
            )
        ),
        rpc_notification(
            _event(
                "tool.result",
                {
                    "tool_use_id": "tu_mcp",
                    "dispatch": "mcp",
                    "server_name": "echo",
                    "is_error": False,
                    "content": echo_content,
                },
            )
        ),
        rpc_notification(_event("provider.request", {"attempt": 0})),
        rpc_notification(_event("provider.response", {"ok": True, "status": 200})),
        rpc_notification(
            _event(
                "server.message.send",
                {"role": "assistant", "content": [{"type": "text", "text": "after mcp"}]},
            )
        ),
    ]
    terminal = rpc_terminal(
        {
            "message": {"role": "assistant", "content": [{"type": "text", "text": "after mcp"}]},
            "events": [],
        }
    )
    return [*notifs, terminal]


async def test_mcp_scenario_matches_canonical_sequence_and_parses_real_payloads() -> None:
    observed: list[SessionEvent] = []

    transport = MockTransport(script=[connect_response(), _mcp_scenario_frames()])
    hooks = Hooks(on_event=lambda e: observed.append(e))
    session = await Harness(_config(), hooks=hooks, transport=transport).connect()

    # MCP tools are dispatched server-side, so `send` returns the final text
    # message after a single sendMessage call.
    reply = await session.send("search please")
    assert reply.text() == "after mcp"

    # The live sequence is identical to the Rust/TypeScript parity tests.
    assert [e.event_type.value for e in observed] == MCP_PARITY_SEQUENCE

    # Real (non-stub) mcp.request / mcp.response payloads parse to their shapes.
    req = next(e for e in observed if e.event_type.value == "mcp.request")
    req_payload = McpRequestPayload.from_payload(req.payload)
    assert req_payload.method == "tools/call"
    assert req_payload.server_name == "echo"
    assert req_payload.tool == "remote_search"
    assert req_payload.input == {"q": "x"}

    resp = next(e for e in observed if e.event_type.value == "mcp.response")
    resp_payload = McpResponsePayload.from_payload(resp.payload)
    assert resp_payload.ok is True
    assert resp_payload.result == {
        "content": [{"type": "text", "text": "echo: x"}],
        "isError": False,
    }

    # No trace of the removed stub payload shape.
    assert not any(e.payload.get("status") == "stub" for e in observed)
