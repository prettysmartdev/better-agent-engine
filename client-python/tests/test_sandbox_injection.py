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

from bae_py import (
    ExecResult,
    RemoteMode,
    SandboxHandle,
    SandboxSession,
    SandboxTarget,
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


async def test_run_shell_named_shell_escapes_every_injection_payload() -> None:
    for payload, expected in INJECTION_CASES:
        timeline: list[str] = []
        sbx = SandboxSession()
        sbx.set_local_driver(FakeDriver(timeline))
        tool = run_shell_named(
            sbx,
            "echo_it",
            "echo the name",
            "echo {name}",
            SandboxTarget.local("alpine"),
            RemoteMode.auto(),
        )
        assert tool.tool is not None, "a local target yields a client-dispatched tool"
        await tool.tool.handler({"name": payload})
        exec_line = next(e for e in timeline if e.startswith("exec:"))
        assert exec_line == f"exec:{expected}", f"payload {payload!r}"


def test_shell_quote_wraps_the_whole_value_as_one_literal_argument() -> None:
    assert shell_quote("a'; rm -rf / #") == "'a'\"'\"'; rm -rf / #'"
    assert shell_quote("$(whoami)") == "'$(whoami)'"
    assert shell_quote("plain") == "plain"
