"""Builtin **sandbox tools** — give an agent real shell/execution ability with a
security boundary the harness developer controls.

Mirrors the server's ``engine/sandbox.rs`` on the client side, offering the same
two-driver :class:`SandboxDriver` shape (``ensure_image``/``start``/``exec``/
``stop``, Docker and Apple Containers), the two tool constructors
:func:`run_shell_command` (arbitrary shell) and :func:`run_shell_named` (a
``{param}`` command template), and a :class:`SandboxTarget`/:class:`RemoteMode`
builder describing *where* a command runs and, for remote commands, *who* builds
the ``tool_result``.

Sandbox tools require a live :class:`~bae_py.harness.Session`
-------------------------------------------------------------

Unlike every other builtin tool, sandbox tools need a session handle: local-target
tools report their ``running``/``stopped``/``error`` lifecycle to the server
(``session.reportLocalSandbox``), and remote-manual tools fetch raw output via
``session.execRemoteSandbox``. Obtain a :class:`SandboxSession` from
:meth:`~bae_py.harness.Harness.sandbox_session` **before** ``connect()``, build
tools against it, register them, then connect. The handle's transport is
late-bound (unset until connect fills it); a tool that somehow fires before
connect raises. Because a handler only runs after ``send()`` — hence after
connect — this is safe, and it is the one shape under which Auto-mode tools
(declared in the session-open ``sandbox_tools`` list, i.e. before connect)
register uniformly alongside local and remote-manual tools.
"""

from __future__ import annotations

import abc
import asyncio
import json
import re
import shlex
from dataclasses import dataclass
from typing import Any, Callable, Protocol, runtime_checkable

from .tool import Tool
from .types import Content

# ---------------------------------------------------------------------------
# Core data types (mirror the server's SandboxDriver surface)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class SandboxHandle:
    """A running sandboxed container, opaque beyond its id and image."""

    id: str
    image: str


@dataclass(slots=True)
class ExecResult:
    """The captured result of one command run inside a sandbox."""

    stdout: str
    stderr: str
    exit_code: int

    def to_content(self) -> dict[str, Any]:
        return {"stdout": self.stdout, "stderr": self.stderr, "exit_code": self.exit_code}


@dataclass(slots=True)
class RemoteSandboxStarted:
    """Terminal result of ``session.startRemoteSandbox`` — the server-hosted
    sandbox is up and its handle retained session-wide."""

    sandbox_id: str
    image: str
    #: When it started (the ``session.sandbox.running`` event's ``created_at``),
    #: or ``None`` if that log write failed.
    started_at: str | None = None


@dataclass(slots=True)
class RemoteSandboxStopped:
    """Terminal result of ``session.stopRemoteSandbox``."""

    stopped: bool
    image: str
    sandbox_id: str


class SandboxError(Exception):
    """A structured sandbox failure, mirroring the server's ``SandboxError``.

    ``kind`` is one of ``"unsupported"``, ``"image"``, ``"runtime"``.
    """

    def __init__(self, kind: str, message: str, image: str | None = None) -> None:
        super().__init__(message)
        self.kind = kind
        self.image = image


class SandboxDriver(abc.ABC):
    """The local container-engine abstraction — a full mirror of the server's
    ``SandboxDriver`` (not just ``exec``), so a :class:`SandboxSession` can track
    a real container identity to report and a test can inject a fake. Implemented
    by :class:`DockerDriver` and :class:`AppleContainerDriver`.
    """

    @abc.abstractmethod
    async def ensure_image(self, image: str) -> None:
        """Idempotent: inspect ``image`` locally; pull it if absent."""

    @abc.abstractmethod
    async def start(self, image: str) -> SandboxHandle:
        """Start a long-lived container (keep-alive ``sleep infinity``)."""

    @abc.abstractmethod
    async def exec(self, handle: SandboxHandle, command: str) -> ExecResult:
        """Run one shell command in an already-started container."""

    @abc.abstractmethod
    async def stop(self, handle: SandboxHandle) -> None:
        """Stop and remove the container. Idempotent on an already-gone id."""


# ---------------------------------------------------------------------------
# CLI drivers (Docker / Apple Containers)
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class _Cli:
    """Per-engine CLI verbs. ``run``/``exec``/``stop`` are identical across engines."""

    program: str
    inspect: tuple[str, ...]
    pull: tuple[str, ...]


_DOCKER_CLI = _Cli(program="docker", inspect=("image", "inspect"), pull=("pull",))
_APPLE_CLI = _Cli(program="container", inspect=("images", "inspect"), pull=("images", "pull"))


async def _run_cli(program: str, args: list[str]) -> tuple[str, str, int]:
    """Run a CLI command to completion, capturing ``(stdout, stderr, exit_code)``.

    A spawn failure (missing binary) is a :class:`SandboxError` of kind
    ``runtime``, never an unhandled exception — exactly how the engine treats a
    missing subprocess binary.
    """
    try:
        proc = await asyncio.create_subprocess_exec(
            program,
            *args,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except OSError as exc:
        raise SandboxError("runtime", f"failed to spawn `{program}`: {exc}") from exc
    out, err = await proc.communicate()
    return (
        out.decode("utf-8", "replace"),
        err.decode("utf-8", "replace"),
        proc.returncode if proc.returncode is not None else -1,
    )


def _truncate(s: str) -> str:
    """Bound CLI stderr carried into an error so a runaway line stays small."""
    t = s.strip()
    return t if len(t) <= 2000 else f"{t[:2000]}… (truncated)"


async def _cli_ensure_image(cli: _Cli, image: str) -> None:
    _, _, code = await _run_cli(cli.program, [*cli.inspect, image])
    if code == 0:
        return
    _, stderr, code = await _run_cli(cli.program, [*cli.pull, image])
    if code != 0:
        raise SandboxError("image", _truncate(stderr), image)


async def _cli_start(cli: _Cli, image: str) -> SandboxHandle:
    stdout, stderr, code = await _run_cli(
        cli.program, ["run", "-d", "--rm", image, "sleep", "infinity"]
    )
    if code != 0:
        raise SandboxError("runtime", _truncate(stderr))
    return SandboxHandle(id=stdout.strip(), image=image)


async def _cli_exec(cli: _Cli, handle: SandboxHandle, command: str) -> ExecResult:
    # The command is one argv element to the container's ``sh -c``; no host shell
    # is involved. A non-zero exit is the command's own result, not a driver error.
    stdout, stderr, code = await _run_cli(cli.program, ["exec", handle.id, "sh", "-c", command])
    return ExecResult(stdout=stdout, stderr=stderr, exit_code=code)


async def _cli_stop(cli: _Cli, handle: SandboxHandle) -> None:
    _, stderr, code = await _run_cli(cli.program, ["stop", handle.id])
    if code != 0:
        if "No such container" in stderr or "not found" in stderr:
            return  # already gone — a ``--rm`` container that already exited
        raise SandboxError("runtime", _truncate(stderr))


class DockerDriver(SandboxDriver):
    """The Docker driver: ``docker image inspect`` → ``docker pull`` on miss;
    ``docker run -d --rm <image> sleep infinity``; ``docker exec <id> sh -c <cmd>``;
    ``docker stop <id>``.
    """

    async def ensure_image(self, image: str) -> None:
        await _cli_ensure_image(_DOCKER_CLI, image)

    async def start(self, image: str) -> SandboxHandle:
        return await _cli_start(_DOCKER_CLI, image)

    async def exec(self, handle: SandboxHandle, command: str) -> ExecResult:
        return await _cli_exec(_DOCKER_CLI, handle, command)

    async def stop(self, handle: SandboxHandle) -> None:
        await _cli_stop(_DOCKER_CLI, handle)


class AppleContainerDriver(SandboxDriver):
    """The Apple Containers driver, shaped identically against the ``container``
    CLI. :meth:`create` fails fast on a non-macOS host so a misconfiguration
    surfaces as one clear error rather than a subprocess failure.
    """

    def __init__(self) -> None:
        # Prefer :meth:`create`, which performs the OS check.
        pass

    @classmethod
    def create(cls, platform: str | None = None) -> "AppleContainerDriver":
        """Construct after checking the host OS (``sys.platform``)."""
        import sys

        host = platform if platform is not None else sys.platform
        if host != "darwin":
            raise SandboxError(
                "unsupported",
                f"Apple Containers driver requires macOS; host platform is `{host}`",
            )
        return cls()

    async def ensure_image(self, image: str) -> None:
        await _cli_ensure_image(_APPLE_CLI, image)

    async def start(self, image: str) -> SandboxHandle:
        return await _cli_start(_APPLE_CLI, image)

    async def exec(self, handle: SandboxHandle, command: str) -> ExecResult:
        return await _cli_exec(_APPLE_CLI, handle, command)

    async def stop(self, handle: SandboxHandle) -> None:
        await _cli_stop(_APPLE_CLI, handle)


# ---------------------------------------------------------------------------
# Shell escaping (the command-injection boundary)
# ---------------------------------------------------------------------------


def shell_quote(arg: str) -> str:
    """Shell-escape ``arg`` with Python's standard primitive, :func:`shlex.quote`.

    This is the command-injection boundary for :func:`run_shell_named`: every
    model-supplied value passes through here before substitution into a command
    template, so the shell always treats the result as **one literal argument**.
    (``shlex.quote`` wraps in single quotes and rewrites ``'`` as ``'"'"'`` — a
    different-looking but POSIX-equivalent escaping to the Rust/TS SDKs' ``'\\''``.)
    """
    return shlex.quote(arg)


_PLACEHOLDER = re.compile(r"\{([^}]*)\}")


def _parse_params(template: str) -> list[str]:
    """Ordered, unique ``{param}`` names in a template; raise on a malformed one."""
    params: list[str] = []
    last = 0
    for m in _PLACEHOLDER.finditer(template):
        if m.group(1) == "":
            raise ValueError("empty `{}` placeholder in command template")
        if m.group(1) not in params:
            params.append(m.group(1))
        last = m.end()
    if "{" in template[last:]:
        raise ValueError("unterminated `{` in command template")
    return params


def _interpolate(template: str, input: dict[str, Any]) -> str:
    """Single pass: copy literal text and splice each ``{name}`` in as the
    shell-escaped input value, so a substituted value can never be re-interpreted
    as a placeholder.
    """

    def repl(m: re.Match[str]) -> str:
        name = m.group(1)
        value = input.get(name)
        if not isinstance(value, str):
            raise ValueError(f"missing required string parameter `{name}`")
        return shell_quote(value)

    return _PLACEHOLDER.sub(repl, template)


# ---------------------------------------------------------------------------
# Session RPC seam
# ---------------------------------------------------------------------------


@runtime_checkable
class SandboxRpc(Protocol):
    """The two new session RPC methods a :class:`SandboxSession` needs.
    :class:`~bae_py.harness.Session` implements this; a test can supply a recorder.
    """

    async def exec_remote_sandbox(self, command: str) -> ExecResult: ...

    async def report_local_sandbox(
        self,
        state: str,
        image: str,
        container_id: str | None,
        detail: str | None,
    ) -> None: ...


# ---------------------------------------------------------------------------
# SandboxSession — the late-bound handle sandbox tools capture
# ---------------------------------------------------------------------------


class SandboxSession:
    """A cheap handle to a live session's sandbox capability: the transport for
    the remote RPC methods, plus the local container-engine driver and the set of
    local containers this session started.

    Obtain one from :meth:`~bae_py.harness.Harness.sandbox_session` (before
    connect) or :meth:`~bae_py.harness.Session.sandbox_session` (after); its
    transport is late-bound. See the module docs for ordering.
    """

    def __init__(self) -> None:
        self._rpc: SandboxRpc | None = None
        self._driver: SandboxDriver = DockerDriver()
        self._started: dict[str, SandboxHandle] = {}
        self._lock = asyncio.Lock()

    def bind(self, rpc: SandboxRpc) -> None:
        """Bind the transport once connected. The first bind wins."""
        if self._rpc is None:
            self._rpc = rpc

    def set_local_driver(self, driver: SandboxDriver) -> None:
        """Replace the local driver (e.g. :class:`AppleContainerDriver`, or a fake)."""
        self._driver = driver

    def _require_rpc(self) -> SandboxRpc:
        if self._rpc is None:
            raise SandboxError(
                "runtime",
                "sandbox tool used before the session was connected; build sandbox tools "
                "from Harness.sandbox_session() and register them, then connect()",
            )
        return self._rpc

    async def exec_remote_sandbox(self, command: str) -> ExecResult:
        """Run ``command`` in the remote sandbox (``session.execRemoteSandbox``)."""
        return await self._require_rpc().exec_remote_sandbox(command)

    async def report_local_sandbox(
        self,
        state: str,
        image: str,
        container_id: str | None = None,
        detail: str | None = None,
    ) -> None:
        """Report a local lifecycle transition (``session.reportLocalSandbox``)."""
        await self._require_rpc().report_local_sandbox(state, image, container_id, detail)

    async def _safe_report(
        self, state: str, image: str, container_id: str | None, detail: str | None
    ) -> None:
        """Report, swallowing telemetry failures (never masks the tool result)."""
        try:
            await self.report_local_sandbox(state, image, container_id, detail)
        except Exception:
            pass

    async def start_local(self, image: str) -> SandboxHandle:
        """Start (or reuse) a local container for ``image``, reporting
        ``running`` on a fresh start and ``error`` on failure. Idempotent per image.
        """
        async with self._lock:
            existing = self._started.get(image)
            if existing is not None:
                return existing
            try:
                await self._driver.ensure_image(image)
                handle = await self._driver.start(image)
            except Exception as exc:
                await self._safe_report("error", image, None, str(exc))
                raise
            self._started[image] = handle
        await self._safe_report("running", image, handle.id, None)
        return handle

    async def exec_local(self, image: str, command: str) -> ExecResult:
        """Lazily start the local container, run ``command``, report ``error`` on failure."""
        handle = await self.start_local(image)
        try:
            return await self._driver.exec(handle, command)
        except Exception as exc:
            await self._safe_report("error", image, handle.id, str(exc))
            raise

    async def stop_all_local(self) -> None:
        """Stop every local container this session started, reporting ``stopped``/``error``."""
        async with self._lock:
            entries = list(self._started.items())
            self._started.clear()
        for image, handle in entries:
            try:
                await self._driver.stop(handle)
                await self._safe_report("stopped", image, handle.id, None)
            except Exception as exc:
                await self._safe_report("error", image, handle.id, str(exc))


# ---------------------------------------------------------------------------
# Builder types + tool constructors
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class SandboxTarget:
    """Where a shell tool's commands run. Build with :meth:`local`/:meth:`remote`."""

    kind: str  # "local" | "remote"
    image: str | None = None

    @classmethod
    def local(cls, image: str) -> "SandboxTarget":
        """The harness's own local container engine, running ``image``."""
        return cls("local", image)

    @classmethod
    def remote(cls) -> "SandboxTarget":
        """The remote sandbox the server started for this session."""
        return cls("remote")


@dataclass(frozen=True, slots=True)
class RemoteMode:
    """For a remote tool, how the result is handled. Ignored for local tools."""

    kind: str  # "auto" | "manual"
    transform: Callable[[ExecResult], Content] | None = None

    @classmethod
    def auto(cls) -> "RemoteMode":
        """Server-dispatched: declared in ``sandbox_tools`` and run server-side."""
        return cls("auto")

    @classmethod
    def manual(cls, transform: Callable[[ExecResult], Content]) -> "RemoteMode":
        """Client-dispatched: fetch raw output, ``transform`` into the tool_result."""
        return cls("manual", transform)


@dataclass(slots=True)
class SandboxToolDef:
    """An Auto-mode sandbox tool declaration for the session-open ``sandbox_tools``
    list. It carries no handler (the server dispatches it) and is a distinct type
    from :class:`~bae_py.tool.Tool` so it can never be registered as a callable tool.
    """

    name: str
    description: str
    input_schema: dict[str, Any]

    def declaration(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        }


@dataclass(slots=True)
class SandboxTool:
    """The result of a sandbox tool constructor: a client-dispatched
    :class:`~bae_py.tool.Tool` (local or remote-manual) **or** an Auto
    :class:`SandboxToolDef` (remote-auto). Exactly one of the two fields is set.
    Register it with :meth:`~bae_py.harness.Harness.register_sandbox_tool`.
    """

    tool: Tool | None = None
    definition: SandboxToolDef | None = None

    @classmethod
    def dispatched(cls, tool: Tool) -> "SandboxTool":
        return cls(tool=tool)

    @classmethod
    def auto(cls, definition: SandboxToolDef) -> "SandboxTool":
        return cls(definition=definition)


def run_shell_command(
    session: SandboxSession,
    target: SandboxTarget,
    remote_mode: RemoteMode,
) -> SandboxTool:
    """Declare a tool that runs an **arbitrary** shell command (fixed name
    ``run_shell_command``, one required ``command`` string) in ``target``.

    For a remote target in :meth:`RemoteMode.auto` this returns a definition;
    otherwise a client-dispatched tool. ``run_shell_command`` is unconstrained by
    design: the image (local) or the server's sandbox is the entire security
    boundary. Use :func:`run_shell_named` when the agent should only run one
    specific command.
    """
    input_schema = {
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command to run inside the sandbox.",
            }
        },
        "required": ["command"],
        "additionalProperties": False,
    }
    return _build_tool(
        session,
        "run_shell_command",
        "Run an arbitrary shell command inside the configured sandbox and return "
        "its stdout, stderr, and exit code.",
        input_schema,
        _FullShell(),
        target,
        remote_mode,
    )


def run_shell_named(
    session: SandboxSession,
    name: str,
    description: str,
    command_template: str,
    target: SandboxTarget,
    remote_mode: RemoteMode,
) -> SandboxTool:
    """Declare a **named** shell tool whose input schema is derived from the
    ``{param}`` placeholders in ``command_template``.

    Each placeholder becomes a required string input; at call time every
    model-supplied value is **shell-escaped** (:func:`shell_quote`) before
    substitution — the command-injection boundary. Returns a definition for
    remote-auto, else a client-dispatched tool. Raises :class:`ValueError` if
    ``command_template`` is malformed.
    """
    params = _parse_params(command_template)
    properties = {
        p: {
            "type": "string",
            "description": f"Value substituted for {{{p}}} (shell-escaped before use).",
        }
        for p in params
    }
    input_schema = {
        "type": "object",
        "properties": properties,
        "required": params,
        "additionalProperties": False,
    }
    return _build_tool(
        session,
        name,
        description,
        input_schema,
        _Template(command_template),
        target,
        remote_mode,
    )


@dataclass(slots=True)
class _FullShell:
    """``run_shell_command`` command source: the whole command is ``input.command``."""

    def build(self, input: dict[str, Any]) -> str:
        command = input.get("command")
        if not isinstance(command, str):
            raise ValueError("run_shell_command requires a string `command`")
        return command


@dataclass(slots=True)
class _Template:
    """``run_shell_named`` command source: substitute escaped values into a template."""

    template: str

    def build(self, input: dict[str, Any]) -> str:
        return _interpolate(self.template, input)


def _exec_result_content(result: ExecResult) -> Content:
    # A JSON **string** of {stdout, stderr, exit_code} — a plain-string content
    # value, consistent with the Rust/TS SDKs and safe for ``content_to_wire``
    # (which accepts a string or a list of blocks, never a bare dict).
    return json.dumps(result.to_content())


def _build_tool(
    session: SandboxSession,
    name: str,
    description: str,
    input_schema: dict[str, Any],
    source: _FullShell | _Template,
    target: SandboxTarget,
    remote_mode: RemoteMode,
) -> SandboxTool:
    # Auto-mode remote tools are declarations only — no client handler.
    if target.kind == "remote" and remote_mode.kind == "auto":
        return SandboxTool.auto(SandboxToolDef(name, description, input_schema))

    async def handler(input: dict[str, Any]) -> Content:
        command = source.build(input)
        if target.kind == "local":
            assert target.image is not None
            result = await session.exec_local(target.image, command)
            return _exec_result_content(result)
        result = await session.exec_remote_sandbox(command)
        # remote + manual (auto handled above; local+auto never reaches here).
        if remote_mode.kind == "manual" and remote_mode.transform is not None:
            return remote_mode.transform(result)
        return _exec_result_content(result)

    return SandboxTool.dispatched(Tool(name, description, input_schema, handler))
