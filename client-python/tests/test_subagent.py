"""Offline tests for :mod:`bae_py.subagent`: shell-escaping, the
immediate-return contract, status-tool visibility transitions, eviction and
unknown-id behavior, truncation, spawn failure, session-close teardown, and
remote-shape safety.

Ported directly from ``client-rust/src/subagent.rs``'s ``#[cfg(test)] mod
tests`` (the WI 0010 reference implementation) — see
``aspec/work-items/0010-cli-subagents.md``'s "Test Considerations" for the
scenario list this covers. Cross-SDK parity (the eighth scenario) lives in
``test_subagent_parity.py``.

Test doubles mirror the Rust reference's ``FakeRunner``/``FakeSubagentRpc``
pair, adapted to asyncio idioms: an ``asyncio.Event`` stands in for
``tokio::sync::Notify`` (``.wait()`` blocks, ``.set()`` releases), and
``FakeRunner.run`` catches its own ``asyncio.CancelledError`` — exactly like
the production :class:`~bae_py.subagent.ProcessSubagentRunner` — so
cancellation tests observe a real kill, not merely an abandoned future.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import shlex
from typing import Any

import pytest

from bae_py import (
    LAUNCH_SUBAGENT_TOOL,
    LOCAL_SUBAGENT_STATUS_TOOL,
    SUBAGENT_OUTPUT_CAP_BYTES,
    RunnerOutput,
    SandboxError,
    SandboxSession,
    SandboxTarget,
    SubagentDef,
    SubagentLaunch,
    SubagentSession,
    launch_subagent,
)
from bae_py.subagent import _read_capped
from test_sandbox_injection import INJECTION_CASES


class FakeRunner:
    """A fake :class:`~bae_py.subagent.SubagentRunner` recording every
    ``(program, args, stdin)`` call. ``outcome`` is a :class:`RunnerOutput`
    (settle ok), an :class:`Exception` (settle as a spawn failure), or
    ``None`` (never settles on its own — only ends via cancellation).
    Optionally gated behind an ``asyncio.Event`` so a test can control exactly
    when the subprocess "exits". A cancellation while blocked sets
    ``drop_event`` before re-raising, proving real kill-on-cancel semantics.
    """

    def __init__(
        self,
        outcome: RunnerOutput | Exception | None,
        *,
        gate: asyncio.Event | None = None,
        drop_event: asyncio.Event | None = None,
    ) -> None:
        self.calls: list[tuple[str, list[str], bytes | None]] = []
        self.gate = gate
        self.outcome = outcome
        self.drop_event = drop_event

    async def run(self, program: str, args: list[str], stdin: bytes | None) -> RunnerOutput:
        self.calls.append((program, list(args), stdin))
        try:
            if self.gate is not None:
                await self.gate.wait()
            if isinstance(self.outcome, Exception):
                raise self.outcome
            if self.outcome is None:
                # Never resolves on its own; only killed by a cancelled watcher.
                await asyncio.get_running_loop().create_future()
            assert isinstance(self.outcome, RunnerOutput)
            return self.outcome
        except asyncio.CancelledError:
            if self.drop_event is not None:
                self.drop_event.set()
            raise


class FakeSubagentRpc:
    """A fake :class:`~bae_py.subagent.SubagentRpc` recording every
    ``report_local_subagent``/``update_client_tools``/``cancel_remote_subagent``
    call — the call-recording technique ``test_sandbox_parity.py``'s
    ``RpcMock`` uses for ``register_driver``/``report_local_sandbox``.
    ``terminal_event`` is set once a terminal (``completed``/``failed``/
    ``cancelled``) report arrives — the test's synchronization point for the
    detached watcher task, the ``asyncio.Event`` equivalent of the Rust
    reference's ``tokio::sync::Notify`` gate.
    """

    def __init__(self) -> None:
        self.reports: list[dict[str, Any]] = []
        self.update_calls: list[list[dict[str, Any]]] = []
        self.cancel_calls: list[str] = []
        self.terminal_event = asyncio.Event()

    async def report_local_subagent(
        self,
        *,
        state: str,
        subagent_id: str,
        harness: str,
        model: str,
        detail: str | None = None,
        reason: str | None = None,
        exit_code: int | None = None,
    ) -> None:
        self.reports.append(
            {
                "state": state,
                "subagent_id": subagent_id,
                "harness": harness,
                "model": model,
                "detail": detail,
                "reason": reason,
                "exit_code": exit_code,
            }
        )
        if state in ("completed", "failed", "cancelled"):
            self.terminal_event.set()

    async def update_client_tools(self, tools: list[dict[str, Any]]) -> None:
        self.update_calls.append(tools)

    async def cancel_remote_subagent(self, subagent_id: str) -> dict[str, Any]:
        self.cancel_calls.append(subagent_id)
        return {"cancelled": True, "subagent_id": subagent_id, "was_running": True}


def _session_with_rpc() -> tuple[SubagentSession, FakeSubagentRpc]:
    session = SubagentSession(SandboxSession())
    rpc = FakeSubagentRpc()
    session.bind(rpc)
    session.set_base_client_tools(
        [{"name": LAUNCH_SUBAGENT_TOOL, "description": "launch", "input_schema": {}}]
    )
    return session, rpc


async def _call(tool: Any, input: dict[str, Any]) -> Any:
    raw = await tool.handler(input)
    assert isinstance(raw, str), "client tools deliver a plain JSON string"
    return json.loads(raw)


def _claude_def(template: str, prompt_via: str) -> SubagentDef:
    return SubagentDef("claude", template, prompt_via=prompt_via)  # type: ignore[arg-type]


# -----------------------------------------------------------------------
# 1. Shell-escaping for {model}/{prompt}, parametrized arg vs. stdin.
#    Reuses the identical injection payload list from test_sandbox_injection.py.
# -----------------------------------------------------------------------


async def test_arg_mode_shell_escapes_every_injection_payload_into_argv() -> None:
    for payload, expected in INJECTION_CASES:
        session, rpc = _session_with_rpc()
        runner = FakeRunner(RunnerOutput(stdout="", stderr="", exit_code=0))
        session.set_runner(runner)
        subtool = launch_subagent(
            session,
            [_claude_def("echo {model} {prompt}", "arg")],
            SubagentLaunch.local(SandboxTarget.none()),
        )
        tool = subtool.tool
        assert tool is not None, "local launch yields a client-dispatched tool"

        await _call(tool, {"harness": "claude", "model": payload, "prompt": payload})
        # Wait for the detached watcher to actually invoke the runner and
        # report its terminal state before inspecting the call log.
        await rpc.terminal_event.wait()

        assert len(runner.calls) == 1, "exactly one subprocess spawned"
        program, args, stdin = runner.calls[0]
        assert program == "/bin/sh"
        assert args == ["-c", f"{expected} {shlex.quote(payload)}"]
        assert stdin is None, f"Arg mode never writes to stdin (payload {payload!r})"

        await session.close_all()


async def test_stdin_mode_never_places_the_raw_prompt_in_argv() -> None:
    for payload, _expected in INJECTION_CASES:
        prompt = f"prompt:\n{payload}"
        session, rpc = _session_with_rpc()
        runner = FakeRunner(RunnerOutput(stdout="", stderr="", exit_code=0))
        session.set_runner(runner)
        # No `{prompt}` placeholder at all under stdin (construction would
        # raise otherwise) — the command is fixed regardless of payload.
        subtool = launch_subagent(
            session,
            [_claude_def("cat --model {model}", "stdin")],
            SubagentLaunch.local(SandboxTarget.none()),
        )
        tool = subtool.tool
        assert tool is not None, "local launch yields a client-dispatched tool"

        await _call(tool, {"harness": "claude", "model": payload, "prompt": prompt})
        await rpc.terminal_event.wait()

        assert len(runner.calls) == 1
        program, args, stdin = runner.calls[0]
        assert program == "/bin/sh"
        assert args == ["-c", f"cat --model {shlex.quote(payload)}"]
        # The constructed argv carries no trace of the payload anywhere.
        assert all(prompt not in a for a in args)
        # The raw (unescaped) prompt reaches the child only via stdin.
        assert stdin == prompt.encode()

        await session.close_all()


# -----------------------------------------------------------------------
# 2. Immediate-return contract: launch_subagent's result is exactly
#    {"subagent_id","harness","model","status":"started"}, never the
#    subagent's output — even when the fake subprocess has already produced
#    output by the time the handler's own future resolves.
# -----------------------------------------------------------------------


async def test_launch_result_is_exactly_the_started_shape_never_output() -> None:
    session, rpc = _session_with_rpc()
    runner = FakeRunner(
        RunnerOutput(stdout="SECRET_SUBAGENT_OUTPUT", stderr="SECRET_STDERR", exit_code=0)
    )
    session.set_runner(runner)
    subtool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    )
    tool = subtool.tool
    assert tool is not None

    raw = await tool.handler({"harness": "claude", "model": "claude-sonnet-5", "prompt": "hi"})
    assert isinstance(raw, str)
    result = json.loads(raw)

    assert list(result.keys()) == ["subagent_id", "harness", "model", "status"], result
    assert result["harness"] == "claude"
    assert result["model"] == "claude-sonnet-5"
    assert result["status"] == "started"
    assert result["subagent_id"].startswith("sba_")
    assert "SECRET_SUBAGENT_OUTPUT" not in raw
    assert "SECRET_STDERR" not in raw

    await rpc.terminal_event.wait()
    await session.close_all()


# -----------------------------------------------------------------------
# 3. Status-tool visibility — updateClientTools fires exactly on the
#    empty->non-empty and non-empty->empty transitions, never redundantly
#    (a second concurrent launch does not re-send).
# -----------------------------------------------------------------------


async def test_update_client_tools_fires_exactly_on_transitions_never_redundantly() -> None:
    session, rpc = _session_with_rpc()
    # A runner that never settles on its own — we only care about the
    # launch-side transition here, not completion.
    runner = FakeRunner(None)
    session.set_runner(runner)
    subtool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    )
    tool = subtool.tool
    assert tool is not None

    assert len(rpc.update_calls) == 0

    # First launch: empty -> non-empty. Fires once, includes the status tool.
    await _call(tool, {"harness": "claude", "model": "m", "prompt": "first"})
    assert len(rpc.update_calls) == 1
    assert any(t["name"] == LOCAL_SUBAGENT_STATUS_TOOL for t in rpc.update_calls[0])
    assert any(t["name"] == LAUNCH_SUBAGENT_TOOL for t in rpc.update_calls[0])

    # Second concurrent launch: non-empty -> non-empty. No re-send.
    await _call(tool, {"harness": "claude", "model": "m", "prompt": "second"})
    assert len(rpc.update_calls) == 1, (
        "a second concurrent launch must not re-send updateClientTools"
    )

    await session.close_all()


# -----------------------------------------------------------------------
# 4. Eviction-after-acknowledgment + unknown-id error, plus the
#    non-empty->empty updateClientTools transition on the evicting read.
# -----------------------------------------------------------------------


async def test_terminal_entry_reported_once_then_evicted_and_unknown_id_errors() -> None:
    session, rpc = _session_with_rpc()
    status = session.status_tool()

    # Unknown id before anything was ever launched.
    err = await _call(status, {"subagent_id": "sba_doesnotexist"})
    assert err == {"error": "unknown subagent_id"}

    gate = asyncio.Event()
    runner = FakeRunner(RunnerOutput(stdout="done", stderr="", exit_code=0), gate=gate)
    session.set_runner(runner)

    subtool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    )
    tool = subtool.tool
    assert tool is not None

    started = await _call(tool, {"harness": "claude", "model": "m", "prompt": "hi"})
    subagent_id = started["subagent_id"]

    # While running: listed, but not evicted, no output yet.
    running = await _call(status, {})
    entries = running["subagents"]
    assert len(entries) == 1
    assert entries[0]["status"] == "running"
    assert entries[0]["stdout"] is None

    # Let the fake subprocess "exit" and wait for the watcher's terminal report.
    gate.set()
    await rpc.terminal_event.wait()

    # First poll after completion: included exactly once, terminal.
    first = await _call(status, {})
    entries = first["subagents"]
    assert len(entries) == 1
    assert entries[0]["subagent_id"] == subagent_id
    assert entries[0]["status"] == "completed"
    assert entries[0]["stdout"] == "done"

    # Second poll: the map has emptied — omitted entirely (evict-on-report).
    second = await _call(status, {})
    assert second["subagents"] == []

    # Querying that id now answers the unknown-id error.
    by_id = await _call(status, {"subagent_id": subagent_id})
    assert by_id == {"error": "unknown subagent_id"}

    # The evicting read fired the non-empty->empty updateClientTools removal:
    # the last update no longer includes the status tool.
    updates = rpc.update_calls
    assert len(updates) == 2, "one at launch, one at eviction"
    assert any(t["name"] == LOCAL_SUBAGENT_STATUS_TOOL for t in updates[0])
    assert not any(t["name"] == LOCAL_SUBAGENT_STATUS_TOOL for t in updates[1])


# -----------------------------------------------------------------------
# 5. Truncation, plus the bonus spawn-failure test.
# -----------------------------------------------------------------------


async def test_output_past_the_cap_is_truncated_and_flagged() -> None:
    session, rpc = _session_with_rpc()
    huge = "a" * (SUBAGENT_OUTPUT_CAP_BYTES + 1000)
    runner = FakeRunner(RunnerOutput(stdout=huge, stderr="", exit_code=0))
    session.set_runner(runner)
    subtool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    )
    tool = subtool.tool
    assert tool is not None
    await _call(tool, {"harness": "claude", "model": "m", "prompt": "hi"})
    await rpc.terminal_event.wait()

    status = session.status_tool()
    result = await _call(status, {})
    entry = result["subagents"][0]
    assert entry["truncated"] is True
    stdout = entry["stdout"]
    assert len(stdout.encode("utf-8")) == SUBAGENT_OUTPUT_CAP_BYTES
    assert huge.startswith(stdout)
    assert len(stdout) < len(huge), "output was actually cut"


async def test_spawn_failure_reports_failed_with_spawn_failed_reason() -> None:
    """A spawn/io failure surfaces as ``failed`` with ``reason:"spawn_failed"``
    and a ``null`` exit code — not an exception escaping to the caller, not a
    dropped turn."""
    session, rpc = _session_with_rpc()
    runner = FakeRunner(OSError("no such file"))
    session.set_runner(runner)
    subtool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    )
    tool = subtool.tool
    assert tool is not None
    await _call(tool, {"harness": "claude", "model": "m", "prompt": "hi"})
    await rpc.terminal_event.wait()

    status = session.status_tool()
    result = await _call(status, {})
    entry = result["subagents"][0]
    assert entry["status"] == "failed"
    assert entry["reason"] == "spawn_failed"
    assert entry["exit_code"] is None
    assert "no such file" in entry["detail"]


async def test_timeout_kills_work_and_reports_timed_out_status() -> None:
    session, rpc = _session_with_rpc()
    dropped = asyncio.Event()
    session.set_runner(FakeRunner(None, drop_event=dropped))
    tool = launch_subagent(
        session,
        [SubagentDef("claude", "cat", timeout_secs=0.001)],
        SubagentLaunch.local(SandboxTarget.none()),
    ).tool
    assert tool is not None
    await _call(tool, {"harness": "claude", "model": "m", "prompt": "p"})
    await rpc.terminal_event.wait()
    assert dropped.is_set()
    result = await _call(session.status_tool(), {})
    assert result["subagents"][0]["status"] == "timed_out"
    assert result["subagents"][0]["reason"] == "timeout"
    assert [report["state"] for report in rpc.reports] == ["start", "running", "failed"]


async def test_explicit_cancel_kills_work_and_remains_visible_until_status() -> None:
    session, rpc = _session_with_rpc()
    dropped = asyncio.Event()
    session.set_runner(FakeRunner(None, drop_event=dropped))
    tool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    ).tool
    assert tool is not None
    started = await _call(tool, {"harness": "claude", "model": "m", "prompt": "p"})
    await asyncio.sleep(0)
    await session.cancel_subagent(started["subagent_id"])
    await rpc.terminal_event.wait()
    for _ in range(5):
        if dropped.is_set():
            break
        await asyncio.sleep(0)
    assert dropped.is_set()
    result = await _call(session.status_tool(), {"subagent_id": started["subagent_id"]})
    assert result["subagents"][0]["status"] == "cancelled"
    assert result["subagents"][0]["reason"] == "explicit"


# -----------------------------------------------------------------------
# 6. Session close teardown: a still-running local subagent is killed (its
#    watcher task cancelled) and reported cancelled{reason:"session_close"};
#    the removal transition fires because the map emptied.
# -----------------------------------------------------------------------


async def test_close_all_kills_running_subagent_and_reports_session_close() -> None:
    session, rpc = _session_with_rpc()
    dropped = asyncio.Event()
    runner = FakeRunner(None, drop_event=dropped)
    session.set_runner(runner)
    subtool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    )
    tool = subtool.tool
    assert tool is not None
    started = await _call(tool, {"harness": "claude", "model": "m", "prompt": "hi"})
    subagent_id = started["subagent_id"]

    # Give the spawned watcher a chance to actually start polling the
    # never-settling future before we tear down.
    await asyncio.sleep(0)

    assert not dropped.is_set(), "still running before close"

    # Capture the watcher task directly so we can deterministically await its
    # cancellation below, rather than polling for it.
    watcher_task = session._tasks[subagent_id].watcher
    assert watcher_task is not None

    await session.close_all()
    with contextlib.suppress(asyncio.CancelledError):
        await watcher_task

    assert dropped.is_set(), "close_all must cancel the watcher, killing the subprocess future"
    assert any(r["state"] == "cancelled" and r["reason"] == "session_close" for r in rpc.reports)
    updates = rpc.update_calls
    # Launch fired the empty->non-empty transition; close_all fires the
    # non-empty->empty removal.
    assert len(updates) == 2
    assert not any(t["name"] == LOCAL_SUBAGENT_STATUS_TOOL for t in updates[-1])


# -----------------------------------------------------------------------
# 7. Remote-shape safety: no remote-unsandboxed SubagentLaunch value is
#    constructible/expressible — SubagentLaunch.remote(image) always yields a
#    declaration-only shape, never a callable tool.
# -----------------------------------------------------------------------


def test_remote_launch_is_always_sandboxed_and_never_a_client_tool() -> None:
    session = SubagentSession(SandboxSession())
    subtool = launch_subagent(
        session,
        [SubagentDef("claude", "claude --model {model} --print {prompt}", prompt_via="arg")],
        SubagentLaunch.remote("bae-subagents:latest"),
    )
    # The ONLY constructible remote shape carries an image; there is no
    # "Remote(Unsandboxed)" value the type permits — `SubagentLaunch.remote`'s
    # sole extra field is `image: str` (see the `SubagentLaunch` definition).
    assert subtool.tool is None, "a Remote launch must never yield a client-dispatched tool"
    assert subtool.definition is not None
    definition = subtool.definition
    assert definition.image == "bae-subagents:latest"
    declaration = definition.declaration()
    assert declaration["image"] == "bae-subagents:latest"
    assert "harness" in declaration["subagents"][0]

    # A second construction confirms the same holds generally, not just once.
    session2 = SubagentSession(SandboxSession())
    subtool2 = launch_subagent(
        session2,
        [SubagentDef("claude", "claude --print {prompt}", prompt_via="arg")],
        SubagentLaunch.remote("img"),
    )
    assert subtool2.tool is None


def test_subagent_launch_rejects_every_malformed_runtime_shape() -> None:
    with pytest.raises(TypeError, match="private"):
        SubagentLaunch("bogus", target=SandboxTarget.none())
    with pytest.raises(TypeError, match="SandboxTarget"):
        SubagentLaunch.local("none")  # type: ignore[arg-type]
    with pytest.raises(ValueError, match="non-empty image"):
        SubagentLaunch.remote("  ")
    with pytest.raises(ValueError, match="execRemoteSandbox carries no stdin"):
        launch_subagent(
            SubagentSession(SandboxSession()),
            [SubagentDef("claude", "cat")],
            SubagentLaunch.local(SandboxTarget.remote()),
        )


async def test_local_tools_fail_before_the_session_is_connected() -> None:
    session = SubagentSession(SandboxSession())
    tool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    ).tool
    assert tool is not None
    with pytest.raises(SandboxError, match="before the session was connected"):
        await tool.handler({"harness": "claude", "model": "m", "prompt": "p"})
    with pytest.raises(SandboxError, match="before the session was connected"):
        await session.status_tool().handler({})
    assert session._tasks == {}


async def test_prompt_and_model_boundary_whitespace_is_delivered_verbatim() -> None:
    prompt = "  keep this indentation\n"
    model = " model-with-spaces "
    for definition in (
        SubagentDef("claude", "cli --model {model}", prompt_via="stdin"),
        SubagentDef("claude", "cli --model {model} --prompt {prompt}", prompt_via="arg"),
    ):
        session, rpc = _session_with_rpc()
        runner = FakeRunner(RunnerOutput(stdout="", stderr="", exit_code=0))
        session.set_runner(runner)
        tool = launch_subagent(
            session, [definition], SubagentLaunch.local(SandboxTarget.none())
        ).tool
        assert tool is not None
        started = await _call(tool, {"harness": "claude", "model": model, "prompt": prompt})
        assert started["model"] == model
        await rpc.terminal_event.wait()
        _, args, stdin = runner.calls[0]
        assert shlex.quote(model) in args[-1]
        if definition.prompt_via == "stdin":
            assert stdin == prompt.encode()
            assert prompt not in args[-1]
        else:
            assert stdin is None
            assert shlex.quote(prompt) in args[-1]
        await session.close_all()


async def test_nine_genuinely_concurrent_launches_reserve_only_eight() -> None:
    class BlockingStartRpc(FakeSubagentRpc):
        def __init__(self) -> None:
            super().__init__()
            self.first_start = asyncio.Event()
            self.release = asyncio.Event()

        async def report_local_subagent(self, **kwargs: Any) -> None:
            await super().report_local_subagent(**kwargs)
            if kwargs["state"] == "start" and not self.release.is_set():
                self.first_start.set()
                await self.release.wait()

    session = SubagentSession(SandboxSession())
    rpc = BlockingStartRpc()
    session.bind(rpc)
    session.set_base_client_tools([])
    session.set_runner(FakeRunner(None))
    tool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    ).tool
    assert tool is not None

    launches = [
        asyncio.create_task(_call(tool, {"harness": "claude", "model": "m", "prompt": f"p{i}"}))
        for i in range(9)
    ]
    await rpc.first_start.wait()
    for _ in range(5):
        await asyncio.sleep(0)
    rpc.release.set()
    results = await asyncio.gather(*launches)

    assert sum(result.get("status") == "started" for result in results) == 8
    assert sum("limit reached" in result.get("error", "") for result in results) == 1
    assert len(session._tasks) == 8
    assert len(rpc.update_calls) == 1
    await session.close_all()


async def test_terminal_report_cannot_overtake_delayed_running_report() -> None:
    class BlockingRunningRpc(FakeSubagentRpc):
        def __init__(self) -> None:
            super().__init__()
            self.running_entered = asyncio.Event()
            self.release_running = asyncio.Event()

        async def report_local_subagent(self, **kwargs: Any) -> None:
            await super().report_local_subagent(**kwargs)
            if kwargs["state"] == "running":
                self.running_entered.set()
                await self.release_running.wait()

    session = SubagentSession(SandboxSession())
    rpc = BlockingRunningRpc()
    session.bind(rpc)
    session.set_base_client_tools([])
    session.set_runner(FakeRunner(RunnerOutput(stdout="done", stderr="", exit_code=0)))
    tool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    ).tool
    assert tool is not None

    launch = asyncio.create_task(tool.handler({"harness": "claude", "model": "m", "prompt": "p"}))
    await rpc.running_entered.wait()
    for _ in range(3):
        await asyncio.sleep(0)
    assert [report["state"] for report in rpc.reports] == ["start", "running"]
    rpc.release_running.set()
    await launch
    await rpc.terminal_event.wait()
    assert [report["state"] for report in rpc.reports] == ["start", "running", "completed"]
    await session.close_all()


async def test_evicting_status_update_cannot_commit_after_a_new_launch() -> None:
    class BlockingRemoveRpc(FakeSubagentRpc):
        def __init__(self) -> None:
            super().__init__()
            self.remove_started = asyncio.Event()
            self.release_remove = asyncio.Event()

        async def update_client_tools(self, tools: list[dict[str, Any]]) -> None:
            self.update_calls.append(tools)
            if not any(tool["name"] == LOCAL_SUBAGENT_STATUS_TOOL for tool in tools):
                self.remove_started.set()
                await self.release_remove.wait()

    session = SubagentSession(SandboxSession())
    rpc = BlockingRemoveRpc()
    session.bind(rpc)
    session.set_base_client_tools([])
    session.set_runner(FakeRunner(RunnerOutput(stdout="done", stderr="", exit_code=0)))
    tool = launch_subagent(
        session, [SubagentDef("claude", "cat")], SubagentLaunch.local(SandboxTarget.none())
    ).tool
    assert tool is not None
    await _call(tool, {"harness": "claude", "model": "m", "prompt": "first"})
    await rpc.terminal_event.wait()

    eviction = asyncio.create_task(_call(session.status_tool(), {}))
    await rpc.remove_started.wait()
    relaunch = asyncio.create_task(
        _call(tool, {"harness": "claude", "model": "m", "prompt": "second"})
    )
    for _ in range(3):
        await asyncio.sleep(0)
    assert not relaunch.done(), "launch must wait behind the in-flight full-replace removal"
    assert len(rpc.update_calls) == 2

    rpc.release_remove.set()
    await eviction
    await relaunch
    assert len(rpc.update_calls) == 3
    assert not any(
        tool_decl["name"] == LOCAL_SUBAGENT_STATUS_TOOL for tool_decl in rpc.update_calls[1]
    )
    assert any(tool_decl["name"] == LOCAL_SUBAGENT_STATUS_TOOL for tool_decl in rpc.update_calls[2])
    await session.close_all()


async def test_production_pipe_collector_retains_only_cap_plus_marker() -> None:
    reader = asyncio.StreamReader()
    reader.feed_data(b"x" * (SUBAGENT_OUTPUT_CAP_BYTES + 50_000))
    reader.feed_eof()
    retained = await _read_capped(reader)
    assert len(retained) == SUBAGENT_OUTPUT_CAP_BYTES + 1
