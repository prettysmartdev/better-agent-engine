"""Cross-SDK local-subagent parity (WI 0010).

The three client SDKs must observe an IDENTICAL ordered live event sequence
for the same scripted local launch -> poll(running) -> poll(completed) flow,
driven entirely through client-dispatched tools (no server-side subagent
dispatch is exercised here — that is the server suite's job). The canonical
sequence below MUST stay byte-for-byte identical to the arrays in the Rust
and TypeScript SDK subagent parity tests:
  - client-rust/src/harness.rs (LOCAL_SUBAGENT_PARITY_SEQUENCE)
  - client-typescript/src/subagent.test.ts (same name)

Fully offline via a purpose-built RPC-recording transport (mirrors
``test_sandbox_parity.py``'s ``RpcMock``) and a gated fake subprocess runner.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any, AsyncIterator, Mapping

from bae_py import (
    Config,
    Harness,
    Hooks,
    RunnerOutput,
    SandboxTarget,
    SubagentDef,
    SubagentLaunch,
    launch_subagent,
)
from bae_py.harness.transport import TransportResponse


def _config() -> Config:
    return Config(server_url="http://test", client_key="bae_client", client_version="9.9.9")


class GatedSubagentRunner:
    """A :class:`~bae_py.subagent.SubagentRunner` whose single subprocess
    blocks on an ``asyncio.Event`` gate until the test releases it, so the
    launch -> poll(running) -> poll(completed) ordering is deterministic —
    the ``asyncio.Event`` equivalent of the Rust reference's
    ``tokio::sync::Notify`` gate."""

    def __init__(self, gate: asyncio.Event) -> None:
        self.gate = gate

    async def run(self, program: str, args: list[str], stdin: bytes | None) -> RunnerOutput:
        await self.gate.wait()
        return RunnerOutput(stdout="subagent done", stderr="", exit_code=0)


class RpcMock:
    """A transport answering each JSON-RPC method by name and recording every
    outbound request, plus every ``reportLocalSubagent``/``updateClientTools``
    call — the same call-recording technique ``test_sandbox_parity.py``'s
    ``RpcMock`` uses for ``register_driver``/``report_local_sandbox``.
    ``send_queue`` supplies one turn of frames per ``session.sendMessage``.
    """

    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []
        self.report_calls: list[dict[str, Any]] = []
        self.update_calls: list[list[dict[str, Any]]] = []
        self.send_queue: list[list[dict[str, Any]]] = []
        # Fires once per terminal (completed/failed/cancelled) report — the
        # test's synchronization point for the detached watcher task.
        self.terminal_event = asyncio.Event()
        self.closed = False

    async def request(
        self, method: str, url: str, *, headers: Mapping[str, str], json: Any | None = None
    ) -> TransportResponse:
        self.requests.append({"method": method, "url": url, "json": json})
        if method == "POST" and url.endswith("/api/v1/sessions"):
            return TransportResponse(
                status=201,
                body={
                    "session_id": "ses_test",
                    "session_key": "bae_ses_test",
                    "profile": {
                        "id": "pro_test",
                        "name": "main",
                        "allowed_tools": ["launch_subagent", "local_subagent_status"],
                        "mcp_servers": [],
                        "provider": {"provider": "anthropic", "model": "claude-sonnet-4-6"},
                    },
                },
            )
        if method == "DELETE":
            return TransportResponse(status=200, body={"session_id": "ses_test", "state": "closed"})
        return TransportResponse(status=200, body={})

    async def stream(
        self, method: str, url: str, *, headers: Mapping[str, str], json: Any | None = None
    ) -> AsyncIterator[dict[str, Any]]:
        body = json or {}
        rpc_method = body.get("method")
        rid = body.get("id", 1)
        params = body.get("params") or {}
        if rpc_method == "session.registerDriver":
            yield {"jsonrpc": "2.0", "id": rid, "result": {"registered": True}}
            return
        self.requests.append({"method": method, "url": url, "json": json})
        if rpc_method == "session.reportLocalSubagent":
            self.report_calls.append(params)
            if params.get("state") in ("completed", "failed", "cancelled"):
                self.terminal_event.set()
            yield {"jsonrpc": "2.0", "id": rid, "result": {"reported": True}}
        elif rpc_method == "session.updateClientTools":
            self.update_calls.append(params.get("tools") or [])
            yield {"jsonrpc": "2.0", "id": rid, "result": {"updated": True}}
        elif rpc_method == "session.sendMessage":
            frames = self.send_queue.pop(0) if self.send_queue else []
            for frame in frames:
                yield frame

    async def aclose(self) -> None:
        self.closed = True


def _rpc_calls(mock: RpcMock, method: str) -> list[dict[str, Any]]:
    return [r for r in mock.requests if (r["json"] or {}).get("method") == method]


def _notif(event_type: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "method": "session.event",
        "params": {
            "id": f"evt_{event_type}",
            "session_id": "ses_test",
            "client_key_id": None,
            "event_type": event_type,
            "payload": payload,
            "created_at": "t",
        },
    }


def _terminal(content: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": 1,
        "result": {"message": {"role": "assistant", "content": content}, "events": []},
    }


# ---------------------------------------------------------------------------
# The canonical 18-event sequence — byte-for-byte identical to the Rust/TS arrays.
# ---------------------------------------------------------------------------

LOCAL_SUBAGENT_PARITY_SEQUENCE = [
    "provider.request",
    "provider.response",
    "server.message.send",  # assistant: tool_use launch_subagent
    "client.message.send",  # tool_result: {"status":"started",...}
    "provider.request",
    "provider.response",
    "server.message.send",  # assistant: tool_use local_subagent_status
    "client.message.send",  # tool_result: {"subagents":[{"status":"running",...}]}
    "provider.request",
    "provider.response",
    "server.message.send",  # assistant: final text; first send() ends
    "provider.request",
    "provider.response",
    "server.message.send",  # assistant: tool_use local_subagent_status (2nd send())
    "client.message.send",  # tool_result: {"subagents":[{"status":"completed",...}]}
    "provider.request",
    "provider.response",
    "server.message.send",  # assistant: final text; second send() ends
]


def _tool_use(id_: str, name: str, input: dict[str, Any]) -> dict[str, Any]:
    return {"type": "tool_use", "id": id_, "name": name, "input": input}


def _text(text: str) -> dict[str, Any]:
    return {"type": "text", "text": text}


def _local_subagent_parity_send_queue() -> list[list[dict[str, Any]]]:
    """The five scripted sendMessage turns backing LOCAL_SUBAGENT_PARITY_SEQUENCE."""
    launch_input = {"harness": "claude", "model": "claude-sonnet-5", "prompt": "do the task"}
    return [
        # Turn A1: assistant launches a subagent.
        [
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "server.message.send",
                {
                    "role": "assistant",
                    "content": [_tool_use("tu_launch", "launch_subagent", launch_input)],
                },
            ),
            _terminal([_tool_use("tu_launch", "launch_subagent", launch_input)]),
        ],
        # Turn A2: assistant polls; the fake subprocess is still gated.
        [
            _notif(
                "client.message.send",
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu_launch"}]},
            ),
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "server.message.send",
                {
                    "role": "assistant",
                    "content": [_tool_use("tu_poll1", "local_subagent_status", {})],
                },
            ),
            _terminal([_tool_use("tu_poll1", "local_subagent_status", {})]),
        ],
        # Turn A3: assistant reports back and the first send() ends.
        [
            _notif(
                "client.message.send",
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu_poll1"}]},
            ),
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "server.message.send",
                {"role": "assistant", "content": [_text("still running, I'll check back")]},
            ),
            _terminal([_text("still running, I'll check back")]),
        ],
        # Turn B1: a fresh send() polls again; by now the subagent completed.
        [
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "server.message.send",
                {
                    "role": "assistant",
                    "content": [_tool_use("tu_poll2", "local_subagent_status", {})],
                },
            ),
            _terminal([_tool_use("tu_poll2", "local_subagent_status", {})]),
        ],
        # Turn B2: assistant reports completion; second send() ends.
        [
            _notif(
                "client.message.send",
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu_poll2"}]},
            ),
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif("server.message.send", {"role": "assistant", "content": [_text("done")]}),
            _terminal([_text("done")]),
        ],
    ]


async def test_local_subagent_scenario_matches_canonical_sequence_across_two_sends() -> None:
    mock = RpcMock()
    mock.send_queue = _local_subagent_parity_send_queue()

    gate = asyncio.Event()
    observed: list[str] = []
    hooks = Hooks(on_event=lambda e: observed.append(e.event_type.value))
    harness = Harness(_config(), hooks=hooks, transport=mock)

    subagent_session = harness.subagent_session()
    subagent_session.set_runner(GatedSubagentRunner(gate))
    launch_tool = launch_subagent(
        subagent_session,
        [SubagentDef("claude", "cat")],
        SubagentLaunch.local(SandboxTarget.none()),
    )
    harness.register_subagent_tool(launch_tool)

    session = await harness.connect()

    # First send(): launch, then poll while still running.
    reply1 = await session.send("please launch a subagent")
    assert reply1.text() == "still running, I'll check back"

    # Let the fake subprocess "exit" and wait for the watcher's terminal report.
    gate.set()
    await mock.terminal_event.wait()

    # Second send(): poll again, now completed.
    reply2 = await session.send("check again")
    assert reply2.text() == "done"

    assert observed == LOCAL_SUBAGENT_PARITY_SEQUENCE

    # Structural parity of the actual tool_result content exchanged with the
    # server at each turn (not just the event-type skeleton) — per the
    # contract's "structural comparison, not raw bytes" note.
    sent = [r["json"]["params"]["message"] for r in _rpc_calls(mock, "session.sendMessage")]

    def tool_result_content(turn: int) -> Any:
        content = sent[turn]["content"]
        assert isinstance(content, list), f"expected blocks, got {content!r}"
        block = content[0]
        assert block["type"] == "tool_result", f"expected tool_result, got {block!r}"
        return json.loads(block["content"])

    # sent[0] is the first user turn; [1] the launch tool_result; [2] the
    # running-poll tool_result; [3] the second send()'s user turn; [4] the
    # completed-poll tool_result.
    started = tool_result_content(1)
    assert started["status"] == "started"
    assert started["harness"] == "claude"
    assert started["model"] == "claude-sonnet-5"
    subagent_id = started["subagent_id"]

    running = tool_result_content(2)
    assert running["subagents"][0]["status"] == "running"
    assert running["subagents"][0]["subagent_id"] == subagent_id

    completed = tool_result_content(4)
    assert completed["subagents"][0]["status"] == "completed"
    assert completed["subagents"][0]["subagent_id"] == subagent_id
    assert completed["subagents"][0]["stdout"] == "subagent done"

    # updateClientTools fired exactly on the two transitions — never
    # redundantly (the eviction on the completed poll removes it again).
    assert len(mock.update_calls) == 2, "empty->non-empty at launch, non-empty->empty at eviction"
    assert any(t["name"] == "local_subagent_status" for t in mock.update_calls[0])
    assert not any(t["name"] == "local_subagent_status" for t in mock.update_calls[1])
