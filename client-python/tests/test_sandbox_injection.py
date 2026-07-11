"""Command-injection resistance for ``run_shell_named`` — the single most
important test in this work item.

The template ``echo {name}`` interpolated with each classic shell-metacharacter
payload must yield a final command string in which the WHOLE argument is one
literal string argument to ``echo``. The payload list is IDENTICAL across the
three SDKs; the expected escaped strings here use Python's ``shlex.quote``
``'"'"'`` quoting (the Rust/TS SDKs hand-roll the semantically-equivalent
``'\\''`` form — see client-rust/src/sandbox.rs and
client-typescript/src/sandbox.test.ts). All offline: a fake local driver
records the exact command; no ``docker``/``container`` binary is touched.
"""

from __future__ import annotations

import asyncio
import json

import pytest

from bae_py import (
    ExecResult,
    RemoteMode,
    SandboxHandle,
    SandboxSession,
    SandboxTarget,
    run_shell_command,
    run_shell_named,
    shell_quote,
)
from bae_py.sandbox import SandboxDriver


class FakeDriver(SandboxDriver):
    """Records each call — and the exact command handed to ``exec`` — offline."""

    def __init__(self, timeline: list[str], exec_stdout: str = "") -> None:
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


# ``(payload, expected final command)`` — the payload list is byte-for-byte the
# same as the Rust/TS injection tests; only the escaped form differs (shlex).
INJECTION_CASES = [
    ("a'; rm -rf / #", "echo 'a'\"'\"'; rm -rf / #'"),
    ("`whoami`", "echo '`whoami`'"),
    ("$(whoami)", "echo '$(whoami)'"),
    ("x && y", "echo 'x && y'"),
    ('he said "hi"', "echo 'he said \"hi\"'"),
]


class RecordingRpc:
    def __init__(self, reports: list[tuple[str, str | None, str | None]]) -> None:
        self.reports = reports

    async def exec_remote_sandbox(self, command: str) -> ExecResult:
        raise AssertionError(f"remote sandbox must not be called: {command}")

    async def report_local_sandbox(
        self,
        state: str,
        image: str | None,
        container_id: str | None,
        detail: str | None,
    ) -> None:
        self.reports.append((state, image, container_id))


class FakeHostProcess:
    returncode = 0

    async def communicate(self) -> tuple[bytes, bytes]:
        return b"none-out", b""


@pytest.mark.parametrize("constructor", ["command", "named"])
async def test_none_dispatch_runs_host_shell_and_never_container_driver(
    constructor: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    host_commands: list[str] = []

    async def fake_create_subprocess_shell(command: str, **_: object) -> FakeHostProcess:
        host_commands.append(command)
        return FakeHostProcess()

    monkeypatch.setattr(asyncio, "create_subprocess_shell", fake_create_subprocess_shell)
    timeline: list[str] = []
    reports: list[tuple[str, str | None, str | None]] = []
    sbx = SandboxSession()
    sbx.bind(RecordingRpc(reports))
    sbx.set_local_driver(FakeDriver(timeline))

    if constructor == "command":
        tool = run_shell_command(sbx, SandboxTarget.none(), RemoteMode.auto())
        command = "printf none-out"
        assert tool.tool is not None
        content = await tool.tool.handler({"command": command})
    else:
        tool = run_shell_named(
            sbx,
            "echo_it",
            "echo the name",
            "echo {name}",
            SandboxTarget.none(),
            RemoteMode.auto(),
        )
        # `hello` needs no shell-quoting, so `shell_quote` (like Python's
        # `shlex.quote`, asserted in `test_shell_quote_wraps_...`) leaves it
        # bare: the interpolated command is `echo hello`, not `echo 'hello'`.
        command = "echo hello"
        assert tool.tool is not None
        content = await tool.tool.handler({"name": "hello"})

    assert json.loads(content) == {"stdout": "none-out", "stderr": "", "exit_code": 0}
    assert host_commands == [command]
    assert timeline == []
    assert reports == [("running", None, None), ("stopped", None, None)]


@pytest.mark.parametrize("target_kind", ["local", "none"])
async def test_run_shell_named_shell_escapes_every_injection_payload(
    target_kind: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    host_commands: list[str] = []

    async def fake_create_subprocess_shell(command: str, **_: object) -> FakeHostProcess:
        host_commands.append(command)
        return FakeHostProcess()

    monkeypatch.setattr(asyncio, "create_subprocess_shell", fake_create_subprocess_shell)
    for payload, expected in INJECTION_CASES:
        timeline: list[str] = []
        sbx = SandboxSession()
        sbx.set_local_driver(FakeDriver(timeline))
        reports: list[tuple[str, str | None, str | None]] = []
        sbx.bind(RecordingRpc(reports))
        target = SandboxTarget.local("alpine") if target_kind == "local" else SandboxTarget.none()
        tool = run_shell_named(
            sbx,
            "echo_it",
            "echo the name",
            "echo {name}",
            target,
            RemoteMode.auto(),
        )
        assert tool.tool is not None, "a local target yields a client-dispatched tool"
        await tool.tool.handler({"name": payload})
        if target_kind == "local":
            exec_line = next(e for e in timeline if e.startswith("exec:"))
            assert exec_line == f"exec:{expected}", f"payload {payload!r}"
        else:
            assert host_commands[-1] == expected, f"payload {payload!r}"
            assert timeline == []


def test_shell_quote_wraps_the_whole_value_as_one_literal_argument() -> None:
    assert shell_quote("a'; rm -rf / #") == "'a'\"'\"'; rm -rf / #'"
    assert shell_quote("$(whoami)") == "'$(whoami)'"
    assert shell_quote("plain") == "plain"
