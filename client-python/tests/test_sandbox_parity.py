"""Cross-SDK sandbox dispatch parity, local-lifecycle reporting, and the remote
start/stop wrappers — all fully offline via a purpose-built RPC-recording
transport and a fake local driver.

The two canonical event sequences below MUST stay byte-for-byte identical to the
arrays in the Rust and TypeScript SDK sandbox parity tests:
  - client-rust/src/harness.rs         (SANDBOX_AUTO_PARITY_SEQUENCE /
                                         SANDBOX_MANUAL_PARITY_SEQUENCE)
  - client-typescript/src/sandbox.test.ts  (same two names)
"""

from __future__ import annotations

import json
from typing import Any, AsyncIterator, Mapping

import pytest

from bae_py import (
    Config,
    ExecResult,
    Harness,
    Hooks,
    RemoteMode,
    RpcError,
    SandboxHandle,
    SandboxTarget,
    run_shell_command,
)
from bae_py.harness.transport import TransportResponse
from bae_py.sandbox import SandboxDriver


def _config() -> Config:
    return Config(server_url="http://test", client_key="bae_client", client_version="9.9.9")


class FakeDriver(SandboxDriver):
    """A fake local driver recording each call into a shared timeline."""

    def __init__(self, timeline: list[str], exec_stdout: str = "hi") -> None:
        self.timeline = timeline
        self.exec_stdout = exec_stdout

    async def ensure_image(self, image: str) -> None:
        self.timeline.append("ensure")

    async def start(self, image: str) -> SandboxHandle:
        self.timeline.append("start")
        return SandboxHandle(id="cid-1", image=image)

    async def exec(self, handle: SandboxHandle, command: str) -> ExecResult:
        self.timeline.append(f"exec:{command}")
        return ExecResult(stdout=self.exec_stdout, stderr="", exit_code=0)

    async def stop(self, handle: SandboxHandle) -> None:
        self.timeline.append("stop")


class RpcMock:
    """A transport answering each JSON-RPC method by name and recording every
    outbound request. ``send_queue`` supplies one turn of frames per
    ``session.sendMessage``; the sandbox utility RPCs get canned results.
    """

    def __init__(self, timeline: list[str] | None = None) -> None:
        self.requests: list[dict[str, Any]] = []
        self.report_calls: list[dict[str, Any]] = []
        self.timeline = timeline if timeline is not None else []
        self.exec_result: dict[str, Any] = {"stdout": "remote-out", "stderr": "", "exit_code": 0}
        self.start_result = {"sandbox_id": "sbx-1", "image": "python:3.12", "started_at": "t0"}
        self.stop_result = {"stopped": True, "image": "python:3.12", "sandbox_id": "sbx-1"}
        self.start_error: dict[str, Any] | None = None
        self.send_queue: list[list[dict[str, Any]]] = []
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
                        "allowed_tools": ["run_shell_command"],
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
        if rpc_method == "session.reportLocalSandbox":
            self.report_calls.append(params)
            self.timeline.append(f"report:{params.get('state')}")
            yield {"jsonrpc": "2.0", "id": rid, "result": {"reported": True}}
        elif rpc_method == "session.execRemoteSandbox":
            yield {"jsonrpc": "2.0", "id": rid, "result": self.exec_result}
        elif rpc_method == "session.startRemoteSandbox":
            if self.start_error is not None:
                yield {"jsonrpc": "2.0", "id": rid, "error": self.start_error}
            else:
                yield {"jsonrpc": "2.0", "id": rid, "result": self.start_result}
        elif rpc_method == "session.stopRemoteSandbox":
            yield {"jsonrpc": "2.0", "id": rid, "result": self.stop_result}
        elif rpc_method == "session.sendMessage":
            frames = self.send_queue.pop(0) if self.send_queue else []
            for frame in frames:
                yield frame

    async def aclose(self) -> None:
        self.closed = True


def _rpc_calls(mock: RpcMock, method: str) -> list[dict[str, Any]]:
    return [r for r in mock.requests if (r["json"] or {}).get("method") == method]


# ---------------------------------------------------------------------------
# Local sandbox lifecycle reporting (running-before-exec, stopped-on-close).
# ---------------------------------------------------------------------------


async def test_local_lifecycle_reports_running_before_exec_and_stopped_on_close() -> None:
    timeline: list[str] = []
    mock = RpcMock(timeline)
    harness = Harness(_config(), transport=mock)
    sbx = harness.sandbox_session()
    sbx.set_local_driver(FakeDriver(timeline))
    tool = run_shell_command(sbx, SandboxTarget.local("alpine"), RemoteMode.auto())
    harness.register_sandbox_tool(tool)

    session = await harness.connect()
    assert tool.tool is not None
    await tool.tool.handler({"command": "echo hi"})
    await session.close()

    running = timeline.index("report:running")
    exec_i = next(i for i, e in enumerate(timeline) if e.startswith("exec:"))
    stopped = timeline.index("report:stopped")
    assert running < exec_i < stopped, timeline

    # Verified via the recorded outbound reportLocalSandbox RPC calls.
    assert mock.report_calls[0]["state"] == "running"
    assert mock.report_calls[0]["container_id"] == "cid-1"
    assert mock.report_calls[-1]["state"] == "stopped"


# ---------------------------------------------------------------------------
# Cross-SDK sandbox dispatch parity.
# ---------------------------------------------------------------------------

SANDBOX_AUTO_PARITY_SEQUENCE = [
    "provider.request",
    "provider.response",
    "tool.call",
    "sandbox.request",
    "sandbox.response",
    "tool.result",
    "provider.request",
    "provider.response",
    "server.message.send",
]

SANDBOX_MANUAL_PARITY_SEQUENCE = [
    "provider.request",
    "provider.response",
    "server.message.send",
    "client.message.send",
    "provider.request",
    "provider.response",
    "server.message.send",
]


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


async def test_auto_dispatch_matches_canonical_sequence() -> None:
    mock = RpcMock()
    mock.send_queue = [
        [
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "tool.call",
                {
                    "dispatch": "sandbox",
                    "name": "run_shell_command",
                    "input": {"command": "echo hi"},
                },
            ),
            _notif(
                "sandbox.request",
                {
                    "tool": "run_shell_command",
                    "input": {"command": "echo hi"},
                    "command": "echo hi",
                },
            ),
            _notif(
                "sandbox.response",
                {
                    "sandbox_id": "cid-1",
                    "ok": True,
                    "result": {"stdout": "hi\n", "stderr": "", "exit_code": 0},
                },
            ),
            _notif(
                "tool.result",
                {
                    "tool_use_id": "tu_sbx",
                    "dispatch": "sandbox",
                    "is_error": False,
                    "content": [{"type": "text", "text": "hi\n"}],
                },
            ),
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "server.message.send",
                {"role": "assistant", "content": [{"type": "text", "text": "ran it"}]},
            ),
            _terminal([{"type": "text", "text": "ran it"}]),
        ]
    ]
    observed: list[str] = []
    hooks = Hooks(on_event=lambda e: observed.append(e.event_type.value))
    session = await Harness(_config(), hooks=hooks, transport=mock).connect()

    reply = await session.send("run it")
    assert reply.text() == "ran it"
    assert observed == SANDBOX_AUTO_PARITY_SEQUENCE
    # Server-dispatched: exactly one sendMessage turn.
    assert len(_rpc_calls(mock, "session.sendMessage")) == 1


async def test_manual_dispatch_matches_canonical_sequence_and_dispatches_client_side() -> None:
    mock = RpcMock()
    mock.exec_result = {"stdout": "manual-out", "stderr": "", "exit_code": 0}
    tool_use = {
        "type": "tool_use",
        "id": "tu_manual",
        "name": "run_shell_command",
        "input": {"command": "ls -la"},
    }
    mock.send_queue = [
        [
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif("server.message.send", {"role": "assistant", "content": [tool_use]}),
            _terminal([tool_use]),
        ],
        [
            _notif(
                "client.message.send",
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu_manual"}]},
            ),
            _notif("provider.request", {"attempt": 0}),
            _notif("provider.response", {"ok": True, "status": 200}),
            _notif(
                "server.message.send",
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
            ),
            _terminal([{"type": "text", "text": "done"}]),
        ],
    ]
    observed: list[str] = []
    hooks = Hooks(on_event=lambda e: observed.append(e.event_type.value))
    harness = Harness(_config(), hooks=hooks, transport=mock)
    sbx = harness.sandbox_session()
    tool = run_shell_command(
        sbx,
        SandboxTarget.remote(),
        RemoteMode.manual(lambda r: json.dumps({"stdout": r.stdout})),
    )
    harness.register_sandbox_tool(tool)

    session = await harness.connect()
    reply = await session.send("list files")
    assert reply.text() == "done"
    assert observed == SANDBOX_MANUAL_PARITY_SEQUENCE

    # The client harness actually dispatched the tool, issuing the fully
    # interpolated command over session.execRemoteSandbox.
    execs = _rpc_calls(mock, "session.execRemoteSandbox")
    assert execs[0]["json"]["params"]["command"] == "ls -la"
    # Manual dispatch pauses: two sendMessage turns.
    assert len(_rpc_calls(mock, "session.sendMessage")) == 2


# ---------------------------------------------------------------------------
# Remote start/stop wrappers (D-gap-1).
# ---------------------------------------------------------------------------


async def test_start_remote_sandbox_issues_rpc_and_returns_result() -> None:
    mock = RpcMock()
    session = await Harness(_config(), transport=mock).connect()
    started = await session.start_remote_sandbox("python:3.12")
    assert started.sandbox_id == "sbx-1"
    assert started.image == "python:3.12"
    req = _rpc_calls(mock, "session.startRemoteSandbox")[0]
    assert req["json"]["params"] == {"image": "python:3.12"}


async def test_start_remote_sandbox_surfaces_not_allowed_error() -> None:
    mock = RpcMock()
    mock.start_error = {"code": -32011, "message": "sandbox_image_not_allowed"}
    session = await Harness(_config(), transport=mock).connect()
    with pytest.raises(RpcError) as excinfo:
        await session.start_remote_sandbox("evil:latest")
    assert excinfo.value.code == -32011


async def test_stop_remote_sandbox_issues_rpc_and_returns_result() -> None:
    mock = RpcMock()
    session = await Harness(_config(), transport=mock).connect()
    stopped = await session.stop_remote_sandbox()
    assert stopped.stopped is True
    assert stopped.sandbox_id == "sbx-1"
    req = _rpc_calls(mock, "session.stopRemoteSandbox")[0]
    assert req["json"]["params"] == {}
