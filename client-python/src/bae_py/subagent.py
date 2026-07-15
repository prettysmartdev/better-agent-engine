"""Builtin **subagent tools** — let an agent hand a prompt off to an external
CLI coding agent (``claude``, ``codex``, …) that runs in the background, then
poll for its result.

Mirrors :mod:`bae_py.sandbox` one axis further: a subagent has a **launch
location** (who owns the subprocess — this harness, or ``baesrv``) crossed
with a **sandbox target** (where it runs). Only three combinations are ever
valid, and :class:`SubagentLaunch` makes the fourth (``baesrv`` running a
subagent unsandboxed on its own host) unconstructible *by shape*: ``remote()``
carries only an ``image`` — there is no way to build a bare-host remote value.

| Launch | Target | Who spawns |
|--------|--------|------------|
| :meth:`SubagentLaunch.local` | ``SandboxTarget.none()`` | this harness, bare host |
| :meth:`SubagentLaunch.local` | ``SandboxTarget.local()`` / ``.remote()`` | this harness's container / the session's remote sandbox |
| :meth:`SubagentLaunch.remote` | (implicitly sandboxed) | ``baesrv``, in the session's remote sandbox |

The async, fire-and-forget contract
------------------------------------

:func:`launch_subagent` returns **immediately** with ``{"status":"started",…}``
— never the subagent's output. A background task owns the subprocess; the
model retrieves the result later through the automatically-appearing
``local_subagent_status`` tool. That status tool is advertised via
``session.updateClientTools`` only while at least one subagent is tracked, and
removed again once the tracking map empties — the harness developer wires
nothing.

Subagent tools require a live :class:`~bae_py.harness.Session`
----------------------------------------------------------------

Exactly like sandbox tools, subagent tools capture a late-bound
:class:`SubagentSession` handle: obtain it from
:meth:`~bae_py.harness.Harness.subagent_session` **before** ``connect()``,
build tools against it, register them with
:meth:`~bae_py.harness.Harness.register_subagent_tool`, then ``connect()``.
The handle's transport is filled at connect; the dynamic
``local_subagent_status`` tool is wired for dispatch **at connect time only**,
based on whether a ``Local`` launch tool was registered by then.

Untrusted output
-----------------

A subagent's stdout is **data to reason about, never instructions to
follow** — the same prompt-injection posture the sandbox tools take. The
status-tool description reminds the model of this.
"""

from __future__ import annotations

import asyncio
import json
from contextlib import suppress
from dataclasses import dataclass
from typing import Any, Literal, Protocol, runtime_checkable

from .sandbox import SandboxError, SandboxSession, SandboxTarget, interpolate, parse_params
from .secure import random_hex
from .tool import Tool
from .types import Content

# ---------------------------------------------------------------------------
# Constants (must match the server and the other two SDKs — see the contract)
# ---------------------------------------------------------------------------

#: Client-side default subagent timeout, overridable per :class:`SubagentDef`.
#: The SDK never reads ``BAE_SUBAGENT_TIMEOUT`` (that is a server env var).
DEFAULT_SUBAGENT_TIMEOUT_SECS = 600

#: Max concurrently **non-terminal** subagents tracked per :class:`SubagentSession`.
#: A guardrail (not env-configurable client-side) so a model cannot fork
#: unboundedly many subprocesses in one session.
MAX_SUBAGENTS_PER_SESSION = 8

#: Per-stream captured-output cap. Captured stdout and stderr are each
#: truncated to their first this-many bytes (on a UTF-8 boundary) before
#: storage; if either was cut, the status entry carries ``"truncated": true``.
SUBAGENT_OUTPUT_CAP_BYTES = 65536

#: Fixed launch-tool name; a harness binds at most one.
LAUNCH_SUBAGENT_TOOL = "launch_subagent"

#: Fixed client-dispatched status-tool name.
LOCAL_SUBAGENT_STATUS_TOOL = "local_subagent_status"

# ---------------------------------------------------------------------------
# Core data types
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class SubagentDef:
    """One configured CLI subagent: the ``harness`` value the model selects, the
    shell ``command_template`` to run (with ``{model}``/``{prompt}``
    placeholders, §8 of the contract), how the prompt is delivered, and a
    per-subagent timeout.

    ``prompt_via="stdin"`` (the default) pipes the raw prompt to the child's
    stdin — it never appears in the constructed argv, sidestepping argv length
    limits and most of the escaping surface. ``"arg"`` interpolates a
    shell-escaped ``{prompt}`` into the command template.
    """

    #: The enum value the LLM selects, e.g. ``"claude"``.
    harness: str
    #: e.g. ``"claude --model {model} --print"``. ``{model}`` is optional;
    #: ``{prompt}`` is required iff ``prompt_via == "arg"`` and forbidden
    #: otherwise.
    command_template: str
    #: How the prompt is handed to the subprocess.
    prompt_via: Literal["arg", "stdin"] = "stdin"
    #: Wall-clock timeout in seconds; on expiry the process is killed and the
    #: subagent becomes ``"timed_out"``.
    timeout_secs: int = DEFAULT_SUBAGENT_TIMEOUT_SECS

    def __post_init__(self) -> None:
        if self.prompt_via not in ("arg", "stdin"):
            raise ValueError(f'prompt_via must be "arg" or "stdin", got {self.prompt_via!r}')


@dataclass(frozen=True, slots=True, init=False)
class SubagentLaunch:
    """Where a launched subagent runs — the one and only place the
    "no remote-unsandboxed" invariant is enforced, **by construction**. Build
    with :meth:`local`/:meth:`remote`; there is no other constructor.

    :meth:`local` carries any :class:`~bae_py.sandbox.SandboxTarget` (including
    ``SandboxTarget.none()``, this harness's own risk to accept). :meth:`remote`
    carries **only** an image — there is no way to express a bare-host remote
    launch.
    """

    kind: str  # "local" | "remote"
    target: SandboxTarget | None = None
    image: str | None = None

    def __init__(self, *_args: object, **_kwargs: object) -> None:
        raise TypeError("SubagentLaunch is private; use SubagentLaunch.local() or .remote()")

    @classmethod
    def _validated(
        cls,
        kind: Literal["local", "remote"],
        *,
        target: SandboxTarget | None = None,
        image: str | None = None,
    ) -> "SubagentLaunch":
        value = object.__new__(cls)
        object.__setattr__(value, "kind", kind)
        object.__setattr__(value, "target", target)
        object.__setattr__(value, "image", image)
        return value

    @classmethod
    def local(cls, target: SandboxTarget) -> "SubagentLaunch":
        """The client harness owns the subprocess; it runs per ``target``."""
        if not isinstance(target, SandboxTarget):
            raise TypeError("SubagentLaunch.local() requires a SandboxTarget")
        return cls._validated("local", target=target)

    @classmethod
    def remote(cls, image: str) -> "SubagentLaunch":
        """``baesrv`` owns the subprocess; it runs inside the session's
        already-started remote sandbox (``image``). The only remote shape —
        always sandboxed."""
        if not isinstance(image, str) or not image.strip():
            raise ValueError("SubagentLaunch.remote() requires a non-empty image")
        return cls._validated("remote", image=image)


# ---------------------------------------------------------------------------
# The Tool-vs-Def split (verbatim the sandbox rationale)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class SubagentToolDef:
    """A **remote**-launch subagent declaration destined for the session-open
    ``subagent_tools`` list. It carries no handler (the server dispatches it)
    and is a distinct type from :class:`~bae_py.tool.Tool` so it can never be
    registered as a callable tool.
    """

    name: str
    description: str
    input_schema: dict[str, Any]
    #: The image the server's sandbox must be running.
    image: str
    #: The ``{harness, command_template, prompt_via, timeout_secs}`` config array.
    subagents: list[dict[str, Any]]

    def declaration(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
            "image": self.image,
            "subagents": self.subagents,
        }


@dataclass(slots=True)
class SubagentTool:
    """The result of :func:`launch_subagent`: a client-dispatched
    :class:`~bae_py.tool.Tool` (a ``Local`` launch) **or** a
    :class:`SubagentToolDef` (a ``Remote`` launch). Exactly one of the two
    fields is set. Register it with
    :meth:`~bae_py.harness.Harness.register_subagent_tool`.
    """

    tool: Tool | None = None
    definition: SubagentToolDef | None = None

    @classmethod
    def dispatched(cls, tool: Tool) -> "SubagentTool":
        return cls(tool=tool)

    @classmethod
    def declared(cls, definition: SubagentToolDef) -> "SubagentTool":
        return cls(definition=definition)


# ---------------------------------------------------------------------------
# Runner seam + RPC seam (fake-able offline, exactly like the sandbox seams)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class RunnerOutput:
    """The captured result of one subagent subprocess."""

    stdout: str
    stderr: str
    #: Process exit code (``-1`` if killed by a signal).
    exit_code: int


@runtime_checkable
class SubagentRunner(Protocol):
    """The injectable subprocess seam. The production
    :class:`ProcessSubagentRunner` shells out via ``asyncio.subprocess``; a
    test injects a fake so no real ``claude``/``codex`` binary is ever
    required.
    """

    async def run(self, program: str, args: list[str], stdin: bytes | None) -> RunnerOutput:
        """Run ``program`` with ``args``, optionally writing ``stdin`` to the
        child before waiting, and capture ``{stdout, stderr, exit_code}``. A
        spawn/io failure is an exception (surfaced as
        ``failed{reason:"spawn_failed"}``)."""
        ...


class ProcessSubagentRunner:
    """The production runner: ``asyncio.create_subprocess_exec``.

    Python subprocesses have no ``kill_on_drop`` — cancelling the awaiting task
    (timeout or explicit cancel) does not by itself reap the child, so this
    runner catches its own cancellation, kills the process, and reaps it before
    re-raising. ``asyncio.subprocess.Process.communicate()`` pumps stdin/stdout/
    stderr concurrently while retaining only the first cap+1 bytes per stream.
    """

    async def run(self, program: str, args: list[str], stdin: bytes | None) -> RunnerOutput:
        try:
            proc = await asyncio.create_subprocess_exec(
                program,
                *args,
                stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        except OSError as exc:
            raise OSError(f"failed to spawn `{program}`: {exc}") from exc
        assert proc.stdout is not None and proc.stderr is not None
        stdout_task = asyncio.create_task(_read_capped(proc.stdout))
        stderr_task = asyncio.create_task(_read_capped(proc.stderr))

        async def write_stdin() -> None:
            if stdin is None or proc.stdin is None:
                return
            with suppress(BrokenPipeError, ConnectionResetError):
                proc.stdin.write(stdin)
                await proc.stdin.drain()
                proc.stdin.close()
                await proc.stdin.wait_closed()

        stdin_task = asyncio.create_task(write_stdin())
        try:
            await proc.wait()
            out, err = await asyncio.gather(stdout_task, stderr_task)
            await stdin_task
        except asyncio.CancelledError:
            if proc.returncode is None:
                proc.kill()
            await proc.wait()
            for task in (stdout_task, stderr_task, stdin_task):
                task.cancel()
            await asyncio.gather(stdout_task, stderr_task, stdin_task, return_exceptions=True)
            raise
        return RunnerOutput(
            stdout=out.decode("utf-8", "replace"),
            stderr=err.decode("utf-8", "replace"),
            exit_code=proc.returncode if proc.returncode is not None else -1,
        )


async def _read_capped(reader: asyncio.StreamReader) -> bytes:
    """Drain ``reader`` to EOF while retaining only cap+1 marker bytes."""
    retained = bytearray()
    while chunk := await reader.read(8192):
        remaining = SUBAGENT_OUTPUT_CAP_BYTES + 1 - len(retained)
        if remaining > 0:
            retained.extend(chunk[:remaining])
    return bytes(retained)


@runtime_checkable
class SubagentRpc(Protocol):
    """The object-safe transport seam a :class:`SubagentSession` calls into.
    Implemented on :class:`~bae_py.harness.Session`; a test can supply its own
    recorder.
    """

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
        """``session.reportLocalSubagent`` — mirror a local subagent lifecycle
        transition into the session's event log (pure telemetry)."""
        ...

    async def update_client_tools(self, tools: list[dict[str, Any]]) -> None:
        """``session.updateClientTools`` — full-replace this client's advertised
        tool list (used to make ``local_subagent_status`` appear/disappear)."""
        ...

    async def cancel_remote_subagent(self, subagent_id: str) -> dict[str, Any]:
        """``session.cancelSubagent`` — cancel a **remote** (server-tracked)
        subagent. Exposed for application code via
        :meth:`~bae_py.harness.Session.cancel_remote_subagent`; local
        cancellation never calls this."""
        ...


# ---------------------------------------------------------------------------
# Tracked-task state
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class _SubagentTask:
    """One tracked local subagent."""

    #: Monotonic insertion order, so the status tool can list in launch order.
    seq: int
    harness: str
    model: str
    status: str = "running"  # "running" | "completed" | "failed" | "timed_out" | "cancelled"
    exit_code: int | None = None
    #: Truncated captured stdout/stderr (terminal states only).
    stdout: str | None = None
    stderr: str | None = None
    truncated: bool = False
    reason: str | None = None
    detail: str | None = None
    #: The watcher task; ``cancel()`` reaps the child (see :class:`ProcessSubagentRunner`).
    watcher: "asyncio.Task[None] | None" = None

    def entry(self, subagent_id: str) -> dict[str, Any]:
        return {
            "subagent_id": subagent_id,
            "harness": self.harness,
            "model": self.model,
            "status": self.status,
            "exit_code": self.exit_code,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "truncated": self.truncated,
            "reason": self.reason,
            "detail": self.detail,
        }


def _is_terminal(status: str) -> bool:
    return status != "running"


def _report_state(status: str) -> str:
    """The telemetry (``reportLocalSubagent``) ``state`` string — ``timed_out``
    folds into ``"failed"`` (its ``reason`` is ``"timeout"``); distinct only in
    the *status tool*."""
    return "failed" if status == "timed_out" else status


def _json_string(value: Any) -> str:
    return json.dumps(value)


def _error_result(msg: str) -> str:
    return _json_string({"error": msg})


# ---------------------------------------------------------------------------
# SubagentSession — the late-bound handle subagent tools capture
# ---------------------------------------------------------------------------


class SubagentSession:
    """A cheap handle to a live session's subagent capability: the transport for
    the RPC methods, the injectable subprocess runner, and the tracked-task map.

    Obtain one from :meth:`~bae_py.harness.Harness.subagent_session` (before
    connect) or :meth:`~bae_py.harness.Session.subagent_session` (after); its
    transport is late-bound. See the module docs for ordering.
    """

    def __init__(self, sandbox: SandboxSession) -> None:
        self._rpc: SubagentRpc | None = None
        self._runner: SubagentRunner = ProcessSubagentRunner()
        #: Shared sandbox handle: container start/reuse for ``Local{image}``
        #: targets and ``execRemoteSandbox`` for ``Remote`` targets.
        self._sandbox = sandbox
        self._tasks: dict[str, _SubagentTask] = {}
        self._lock = asyncio.Lock()
        self._seq = 0
        #: The harness's declared client-tool list (no status tool), captured at
        #: connect so ``updateClientTools`` can full-replace it plus/minus the
        #: status tool.
        self._base_client_tools: list[dict[str, Any]] = []
        #: Whether a ``Local`` launch tool was built against this session (read
        #: at connect to decide whether to wire the dynamic status tool).
        self._has_local = False

    def bind(self, rpc: SubagentRpc) -> None:
        """Bind the transport once the session is connected. The first bind wins."""
        if self._rpc is None:
            self._rpc = rpc

    def set_runner(self, runner: SubagentRunner) -> None:
        """Replace the subprocess runner (default :class:`ProcessSubagentRunner`);
        use to inject a fake in tests, exactly like
        :meth:`~bae_py.sandbox.SandboxSession.set_local_driver`."""
        self._runner = runner

    def mark_local(self) -> None:
        """Mark this session as backing a ``Local`` launch tool (called by
        :func:`launch_subagent`); the harness reads this at connect to decide
        whether to wire the dynamic status tool."""
        self._has_local = True

    def has_local(self) -> bool:
        return self._has_local

    def set_base_client_tools(self, tools: list[dict[str, Any]]) -> None:
        """Capture the harness's declared client-tool list (no status tool) at
        connect, so ``updateClientTools`` can full-replace it."""
        self._base_client_tools = tools

    def _require_rpc(self) -> SubagentRpc:
        if self._rpc is None:
            raise SandboxError(
                "runtime",
                "subagent tool used before the session was connected; build subagent "
                "tools from Harness.subagent_session() and register them, then connect()",
            )
        return self._rpc

    async def _report(
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
        """Best-effort telemetry mirror — an unbound/failed transport is
        ignored, so telemetry never fails a launch or a status call."""
        if self._rpc is None:
            return
        try:
            await self._rpc.report_local_subagent(
                state=state,
                subagent_id=subagent_id,
                harness=harness,
                model=model,
                detail=detail,
                reason=reason,
                exit_code=exit_code,
            )
        except Exception:
            pass

    async def _sync_client_tools(self, include_status: bool) -> None:
        """Best-effort ``updateClientTools``: full-replace the client's tool
        list with the base list, plus the status tool iff ``include_status``. A
        failure is swallowed (retried at the next transition); it never fails
        the caller."""
        tools = list(self._base_client_tools)
        if include_status:
            tools.append(_status_tool_declaration())
        if self._rpc is None:
            return
        try:
            await self._rpc.update_client_tools(tools)
        except Exception:
            pass

    def status_tool(self) -> Tool:
        """The dynamic ``local_subagent_status`` tool, dispatched from this
        session's tracking map. The harness registers it for dispatch only at
        connect; it is advertised to the provider only via
        ``updateClientTools``, never in the session-open ``tools`` list."""

        async def handler(input: dict[str, Any]) -> Content:
            self._require_rpc()
            return await self._handle_status(input)

        return Tool(
            LOCAL_SUBAGENT_STATUS_TOOL, STATUS_TOOL_DESCRIPTION, _status_input_schema(), handler
        )

    async def _handle_status(self, input: dict[str, Any]) -> Content:
        """Read the map, evict any terminal entry included in this response
        (evict-on-report), and — if the eviction empties the map — fire the
        ``updateClientTools`` removal transition."""
        raw_id = input.get("subagent_id")
        target = raw_id if isinstance(raw_id, str) else None

        async with self._lock:
            if target is not None:
                task = self._tasks.get(target)
                if task is None:
                    return _error_result("unknown subagent_id")
                entry = task.entry(target)
                was_nonempty = bool(self._tasks)
                if _is_terminal(task.status):
                    del self._tasks[target]
                emptied = was_nonempty and not self._tasks
                payload = _json_string({"subagents": [entry]})
            else:
                ordered = sorted(self._tasks.items(), key=lambda kv: kv[1].seq)
                entries = [t.entry(sid) for sid, t in ordered]
                was_nonempty = bool(self._tasks)
                for sid, t in ordered:
                    if _is_terminal(t.status):
                        del self._tasks[sid]
                emptied = was_nonempty and not self._tasks
                payload = _json_string({"subagents": entries})
            if emptied:
                await self._sync_client_tools(False)
            return payload

    async def cancel_subagent(self, subagent_id: str) -> None:
        """Cancel one local subagent in-process (idempotent). Cancels the
        watcher task (which kills the child, see :class:`ProcessSubagentRunner`),
        marks it ``cancelled`` with ``reason:"explicit"``, and mirrors the
        transition via telemetry. The entry stays tracked so the model can
        observe the cancellation through the status tool. A terminal/unknown id
        is a silent no-op.
        """
        async with self._lock:
            cancelled: tuple[str, str] | None = None
            task = self._tasks.get(subagent_id)
            if task is not None and task.status == "running":
                if task.watcher is not None:
                    task.watcher.cancel()
                    task.watcher = None
                task.status = "cancelled"
                task.reason = "explicit"
                cancelled = (task.harness, task.model)
            if cancelled is not None:
                harness, model = cancelled
                await self._report(
                    state="cancelled",
                    subagent_id=subagent_id,
                    harness=harness,
                    model=model,
                    reason="explicit",
                )

    async def close_all(self) -> None:
        """Session-close teardown: cancel every still-running local subagent
        (reaping its child), report each ``cancelled{reason:"session_close"}``,
        clear the whole map, and — if it was non-empty — fire the
        ``updateClientTools`` removal so the status tool disappears."""
        async with self._lock:
            cancelled: list[tuple[str, str, str]] = []
            was_nonempty = bool(self._tasks)
            for sid, task in self._tasks.items():
                if task.status == "running":
                    if task.watcher is not None:
                        task.watcher.cancel()
                        task.watcher = None
                    task.status = "cancelled"
                    task.reason = "session_close"
                    cancelled.append((sid, task.harness, task.model))
            self._tasks.clear()
            for sid, harness, model in cancelled:
                await self._report(
                    state="cancelled",
                    subagent_id=sid,
                    harness=harness,
                    model=model,
                    reason="session_close",
                )
            if was_nonempty:
                await self._sync_client_tools(False)


# ---------------------------------------------------------------------------
# Pinned tool schemas / descriptions
# ---------------------------------------------------------------------------

STATUS_TOOL_DESCRIPTION = (
    "Check the status of subagents launched with launch_subagent. "
    "Pass a subagent_id to query one subagent, or omit it to list all tracked subagents. "
    "A subagent that has finished is reported with its captured output exactly once."
)


def _status_input_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "properties": {
            "subagent_id": {
                "type": "string",
                "description": "The subagent to query. Omit to report every tracked subagent.",
            }
        },
        "required": [],
        "additionalProperties": False,
    }


def _status_tool_declaration() -> dict[str, Any]:
    return {
        "name": LOCAL_SUBAGENT_STATUS_TOOL,
        "description": STATUS_TOOL_DESCRIPTION,
        "input_schema": _status_input_schema(),
    }


def _launch_description(names: list[str]) -> str:
    return (
        f"Launch a CLI subagent ({', '.join(names)}) to work on a task in the background. "
        'This tool is ASYNCHRONOUS: it returns immediately with a subagent_id and status "started" '
        "— it never waits for or returns the subagent's output. The subagent keeps running in the "
        "background; call the subagent status tool later to check whether it has finished and to "
        "retrieve its output."
    )


def _launch_input_schema(names: list[str]) -> dict[str, Any]:
    return {
        "type": "object",
        "properties": {
            "harness": {
                "type": "string",
                "enum": names,
                "description": "Which configured CLI subagent to launch.",
            },
            "model": {
                "type": "string",
                "description": "The model name passed to the subagent CLI.",
            },
            "prompt": {
                "type": "string",
                "description": "The task prompt handed to the subagent.",
            },
        },
        "required": ["harness", "model", "prompt"],
        "additionalProperties": False,
    }


# ---------------------------------------------------------------------------
# Template validation (§8) — construction-time developer-bug errors
# ---------------------------------------------------------------------------


def _validate_template(definition: SubagentDef) -> None:
    """Validate a def's ``command_template`` placeholders against §8's rules.
    Raises :class:`ValueError` (developer bug at construction) on any violation."""
    params = parse_params(definition.command_template)
    for p in params:
        if p not in ("model", "prompt"):
            raise ValueError(
                f"subagent command_template placeholder `{{{p}}}` is not recognized "
                "(only `{model}` and `{prompt}` are allowed)"
            )
    has_prompt = "prompt" in params
    if definition.prompt_via == "arg" and not has_prompt:
        raise ValueError(
            'subagent command_template must contain `{prompt}` when prompt_via is "arg"'
        )
    if definition.prompt_via == "stdin" and has_prompt:
        raise ValueError(
            'subagent command_template must not contain `{prompt}` when prompt_via is "stdin" '
            "(the prompt is piped to stdin, never placed in argv)"
        )


# ---------------------------------------------------------------------------
# The launch tool
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class _SubTarget:
    """Which sandbox target a ``Local`` launch runs against (image resolved at
    construction; the same target for every configured harness)."""

    kind: str  # "host" | "container" | "remote"
    image: str | None = None


@dataclass(slots=True)
class _SpawnPlan:
    """How the watcher runs the subprocess."""

    kind: str  # "host" | "container" | "remote"
    command: str
    stdin: bytes | None = None
    image: str | None = None


def launch_subagent(
    session: SubagentSession,
    configs: list[SubagentDef],
    launch: SubagentLaunch,
) -> SubagentTool:
    """Declare the ``launch_subagent`` tool covering a **set** of configured CLI
    subagents (the model picks one by ``harness``). Returns a client-dispatched
    :meth:`SubagentTool.dispatched` for a :meth:`SubagentLaunch.local` launch and
    a declaration-only :meth:`SubagentTool.declared` for
    :meth:`SubagentLaunch.remote`.

    Every ``{model}`` substitution (and ``{prompt}`` under ``prompt_via="arg"``)
    is shell-escaped before interpolation — the injection boundary, shared with
    :mod:`bae_py.sandbox`. Under ``prompt_via="stdin"`` the prompt is piped to
    the child and never enters the constructed argv.

    Raises :class:`ValueError` on a developer bug at construction: empty
    ``configs``; duplicate ``harness`` names; a malformed template or one
    violating §8's placeholder rules; or a ``local(SandboxTarget.remote())``
    launch combined with any ``prompt_via="stdin"`` def (``execRemoteSandbox``
    carries no stdin).
    """
    if not configs:
        raise ValueError("launch_subagent requires at least one SubagentDef")

    names: list[str] = []
    resolved: dict[str, SubagentDef] = {}
    for definition in configs:
        if definition.harness in names:
            raise ValueError(f"duplicate subagent harness name `{definition.harness}`")
        names.append(definition.harness)
        _validate_template(definition)
        resolved[definition.harness] = definition

    description = _launch_description(names)
    input_schema = _launch_input_schema(names)

    # Remote launch → a declaration only (server-dispatched); no handler.
    if launch.kind == "remote":
        assert launch.image is not None
        subagents = [
            {
                "harness": d.harness,
                "command_template": d.command_template,
                "prompt_via": d.prompt_via,
                "timeout_secs": d.timeout_secs,
            }
            for d in configs
        ]
        return SubagentTool.declared(
            SubagentToolDef(
                name=LAUNCH_SUBAGENT_TOOL,
                description=description,
                input_schema=input_schema,
                image=launch.image,
                subagents=subagents,
            )
        )

    target = launch.target
    assert target is not None

    if target.kind == "none":
        sub_target = _SubTarget(kind="host")
    elif target.kind == "local":
        assert target.image is not None
        sub_target = _SubTarget(kind="container", image=target.image)
    else:  # "remote"
        # execRemoteSandbox has no stdin: a stdin def would silently drop its
        # prompt, so that combination is a construction error.
        for definition in configs:
            if definition.prompt_via != "arg":
                raise ValueError(
                    "a SubagentLaunch.local(SandboxTarget.remote()) launch requires "
                    'prompt_via="arg" (execRemoteSandbox carries no stdin), but harness '
                    f'`{definition.harness}` uses "stdin"'
                )
        sub_target = _SubTarget(kind="remote")

    session.mark_local()

    async def handler(input: dict[str, Any]) -> Content:
        return await _handle_launch(session, resolved, sub_target, input)

    return SubagentTool.dispatched(Tool(LAUNCH_SUBAGENT_TOOL, description, input_schema, handler))


def _parse_launch_input(input: dict[str, Any]) -> tuple[str, str, str] | None:
    """Extract the three required non-empty string fields, or ``None``."""

    def field(name: str) -> str | None:
        value = input.get(name)
        if not isinstance(value, str):
            return None
        return value if value.strip() else None

    harness = field("harness")
    model = field("model")
    prompt = field("prompt")
    if harness is None or model is None or prompt is None:
        return None
    return harness, model, prompt


async def _handle_launch(
    session: SubagentSession,
    resolved: dict[str, SubagentDef],
    sub_target: _SubTarget,
    input: dict[str, Any],
) -> Content:
    """The ``Local`` launch handler — validates, spawns the fire-and-forget
    watcher, and returns ``{"status":"started",…}`` immediately (§5.6 in-band
    errors on validation failure; never an aborted turn)."""
    session._require_rpc()
    # Field validation.
    triple = _parse_launch_input(input)
    if triple is None:
        return _error_result('launch_subagent requires string "harness", "model", and "prompt"')
    harness, model, prompt = triple

    # Harness lookup.
    definition = resolved.get(harness)
    if definition is None:
        return _error_result(f'unknown harness "{harness}"')

    # Build the (shell-escaped) command and the spawn plan.
    interp: dict[str, Any] = {"model": model}
    if definition.prompt_via == "arg":
        interp["prompt"] = prompt
    try:
        command = interpolate(definition.command_template, interp)
    except ValueError as exc:
        # Should not happen (fields validated), but surface in-band.
        return _error_result(f"failed to build subagent command: {exc}")

    stdin = prompt.encode() if definition.prompt_via == "stdin" else None
    if sub_target.kind == "host":
        plan = _SpawnPlan(kind="host", command=command, stdin=stdin)
    elif sub_target.kind == "container":
        plan = _SpawnPlan(kind="container", command=command, stdin=stdin, image=sub_target.image)
    else:
        plan = _SpawnPlan(kind="remote", command=command)

    # Cap check, reservation, lifecycle ordering, and dynamic-tool update are a
    # single serialized transition. The watcher may execute immediately but is
    # gated from publishing a terminal state until `running` is reported.
    async with session._lock:
        running = sum(1 for t in session._tasks.values() if t.status == "running")
        if running >= MAX_SUBAGENTS_PER_SESSION:
            return _error_result(
                f"subagent limit reached (max {MAX_SUBAGENTS_PER_SESSION} per session)"
            )
        subagent_id = _generate_subagent_id()
        was_empty = not session._tasks
        session._seq += 1
        seq = session._seq
        session._tasks[subagent_id] = _SubagentTask(seq=seq, harness=harness, model=model)
        await session._report(state="start", subagent_id=subagent_id, harness=harness, model=model)

        running_reported = asyncio.Event()
        watcher = asyncio.create_task(
            _watch(
                session,
                subagent_id,
                harness,
                model,
                definition.timeout_secs,
                plan,
                running_reported,
            )
        )
        task = session._tasks.get(subagent_id)
        if task is not None and task.status == "running":
            task.watcher = watcher

        try:
            await session._report(
                state="running", subagent_id=subagent_id, harness=harness, model=model
            )
            if was_empty:
                await session._sync_client_tools(True)
        finally:
            running_reported.set()

    return _json_string(
        {"subagent_id": subagent_id, "harness": harness, "model": model, "status": "started"}
    )


async def _watch(
    session: SubagentSession,
    subagent_id: str,
    harness: str,
    model: str,
    timeout_secs: int,
    plan: _SpawnPlan,
    running_reported: asyncio.Event,
) -> None:
    """The detached background task: run the plan under its timeout, truncate
    the output, set the terminal status (only if still ``running`` — a
    cancel/close may have won the race), and mirror the terminal state via
    telemetry.
    """

    async def work() -> RunnerOutput:
        if plan.kind == "host":
            return await session._runner.run("/bin/sh", ["-c", plan.command], plan.stdin)
        if plan.kind == "container":
            assert plan.image is not None
            handle = await session._sandbox.start_local(plan.image)
            program = session._sandbox.engine_program()
            args = ["exec", "-i", handle.id, "sh", "-c", plan.command]
            return await session._runner.run(program, args, plan.stdin)
        # "remote"
        result = await session._sandbox.exec_remote_sandbox(plan.command)
        return RunnerOutput(stdout=result.stdout, stderr=result.stderr, exit_code=result.exit_code)

    timed_out = False
    error: Exception | None = None
    output: RunnerOutput | None = None
    try:
        output = await asyncio.wait_for(work(), timeout=timeout_secs)
    except asyncio.TimeoutError:
        timed_out = True
    except asyncio.CancelledError:
        # An explicit cancel_subagent()/close_all() already set the terminal
        # state under the lock before requesting this cancellation — never
        # overwrite it, and never report it a second time.
        raise
    except Exception as exc:  # noqa: BLE001 - surfaced in-band as spawn_failed
        error = exc

    await running_reported.wait()

    # Compute the terminal state from the outcome.
    if timed_out:
        status, exit_code, stdout, stderr, truncated, reason, detail = (
            "timed_out",
            None,
            None,
            None,
            False,
            "timeout",
            None,
        )
    elif error is not None:
        status, exit_code, stdout, stderr, truncated, reason, detail = (
            "failed",
            None,
            None,
            None,
            False,
            "spawn_failed",
            str(error),
        )
    else:
        assert output is not None
        so, so_trunc = _truncate_output(output.stdout)
        se, se_trunc = _truncate_output(output.stderr)
        truncated = so_trunc or se_trunc
        if output.exit_code == 0:
            status, exit_code, stdout, stderr, reason, detail = ("completed", 0, so, se, None, None)
        else:
            status, exit_code, stdout, stderr, reason, detail = (
                "failed",
                output.exit_code,
                so,
                se,
                "nonzero_exit",
                None,
            )

    # Apply only if still running (a cancel/close may have set a terminal
    # state already — never overwrite it, and never re-report it).
    applied = False
    async with session._lock:
        task = session._tasks.get(subagent_id)
        if task is not None and task.status == "running":
            task.status = status
            task.exit_code = exit_code
            task.stdout = stdout
            task.stderr = stderr
            task.truncated = truncated
            task.reason = reason
            task.detail = detail
            task.watcher = None
            applied = True

    if applied:
        await session._report(
            state=_report_state(status),
            subagent_id=subagent_id,
            harness=harness,
            model=model,
            detail=detail,
            reason=reason,
            exit_code=exit_code,
        )


def _truncate_output(s: str) -> tuple[str, bool]:
    """Truncate to the first :data:`SUBAGENT_OUTPUT_CAP_BYTES` bytes on a UTF-8
    char boundary; the ``bool`` is whether anything was cut."""
    data = s.encode("utf-8")
    if len(data) <= SUBAGENT_OUTPUT_CAP_BYTES:
        return s, False
    end = SUBAGENT_OUTPUT_CAP_BYTES
    # Back off to a UTF-8 character boundary (continuation bytes are 0b10xxxxxx).
    while end > 0 and (data[end] & 0xC0) == 0x80:
        end -= 1
    return data[:end].decode("utf-8"), True


def _generate_subagent_id() -> str:
    """Generate a ``sba_`` + 32-hex-char id from 16 OS-random bytes — the
    identical format the server mints for remote subagents."""
    return f"sba_{random_hex(16)}"
