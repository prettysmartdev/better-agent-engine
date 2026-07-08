"""The agent harness: holds config + tools + hooks, opens sessions, and drives
the tool-call round-trip loop (api-contract §6).
"""

from __future__ import annotations

import inspect
from typing import Any

from typing import Awaitable, Callable, Union

from ..config import Config
from ..errors import (
    ApiError,
    HookError,
    ProvidersFailedError,
    RpcError,
    ToolError,
    UnknownToolError,
)
from ..hooks import Hooks
from ..tool import Tool, ToolRegistry
from ..types import (
    EventType,
    Message,
    Profile,
    SendMessageResult,
    SessionEvent,
    ToolResultBlock,
    to_message,
)
from .transport import HttpxTransport, Transport, TransportResponse


async def _maybe_await(value: Any) -> Any:
    if inspect.isawaitable(value):
        return await value
    return value


class Harness:
    """An agent definition: connection config plus a registry of client-side
    tools and lifecycle hooks. Reusable — each :meth:`connect` opens a fresh
    session against the same profile.
    """

    def __init__(
        self,
        config: Config,
        *,
        tools: list[Tool] | None = None,
        hooks: Hooks | None = None,
        transport: Transport | None = None,
    ) -> None:
        self.config = config
        self.hooks = hooks or Hooks()
        self._registry = ToolRegistry()
        for tool in tools or []:
            self._registry.add(tool)
        # An injected transport is owned by the caller; a default one is created
        # on connect and closed when the session closes.
        self._transport = transport
        self._owns_transport = transport is None

    def register_tool(self, tool: Tool) -> "Harness":
        """Add a tool. Returns self for chaining."""
        self._registry.add(tool)
        return self

    def set_hooks(self, hooks: Hooks) -> "Harness":
        """Replace the hook set. Returns self for chaining."""
        self.hooks = hooks
        return self

    async def connect(self) -> "Session":
        """Open a new session, returning a :class:`Session`.

        POSTs ``/api/v1/sessions`` with the declared tools; on success the
        server returns a session id, a one-time session key, and the sanitized
        profile. Registers this connection as a driver
        (``session.registerDriver``) before returning, so the first
        :meth:`Session.send` is permitted.
        """
        return await self._open(self.config.url("/api/v1/sessions"))

    async def join(self, session_id: str) -> "Session":
        """Join an **existing** session as an additional driver, returning a
        :class:`Session` shaped identically to :meth:`connect`'s.

        POSTs to ``/api/v1/sessions/{session_id}/join`` with this harness's
        ``client_version`` and declared tools (a joining client declares its own,
        independent tool set, validated against the *same* profile's
        ``allowed_tools``). The joining client key must resolve to the same
        profile as the session, or the server rejects with
        ``403 profile_mismatch``. Like :meth:`connect`, registers this
        connection as a driver before returning.
        """
        return await self._open(self.config.url(f"/api/v1/sessions/{session_id}/join"))

    async def _open(self, url: str) -> "Session":
        """Shared body of :meth:`connect` and :meth:`join`: POST the declared
        tools to ``url`` with client-key auth, build the :class:`Session`, then
        register it as a driver before handing it back. Both endpoints return the
        identical ``{session_id, session_key, profile}`` shape.
        """
        transport = self._transport or HttpxTransport()
        try:
            resp = await transport.request(
                "POST",
                url,
                headers=self._client_auth(),
                json={
                    "client_version": self.config.client_version,
                    "tools": self._registry.declarations(),
                },
            )
            _raise_for_status(resp)
            body = resp.body or {}
            session = Session(
                config=self.config,
                transport=transport,
                registry=self._registry,
                hooks=self.hooks,
                session_id=body["session_id"],
                session_key=body["session_key"],
                profile=Profile.from_wire(body["profile"]),
                owns_transport=self._owns_transport,
            )
            # Register as a driver before any send: session.sendMessage requires
            # it (a -32001 error otherwise). Application code never calls this.
            await session._register_driver()
            return session
        except BaseException:
            if self._owns_transport:
                await transport.aclose()
            raise

    def _client_auth(self) -> dict[str, str]:
        return {
            "Authorization": f"Bearer {self.config.client_key}",
            "Content-Type": "application/json",
        }


class Session:
    """A live session handle. Drives :meth:`send` until the model stops calling
    tools, and :meth:`close` to end the session.
    """

    def __init__(
        self,
        *,
        config: Config,
        transport: Transport,
        registry: ToolRegistry,
        hooks: Hooks,
        session_id: str,
        session_key: str,
        profile: Profile,
        owns_transport: bool,
    ) -> None:
        self.config = config
        self.session_id = session_id
        self.session_key = session_key
        self.profile = profile
        #: Events appended by the most recent :meth:`send`, in order.
        self.last_events: list[SessionEvent] = []
        self._transport = transport
        self._registry = registry
        self._hooks = hooks
        self._owns_transport = owns_transport
        self._closed = False
        #: Monotonic JSON-RPC request id, unique per session.
        self._rpc_id = 0

    async def send(self, message: "str | Message") -> Message:
        """Send a user turn and drive the full round-trip (harness loop).

        Each turn is a ``session.sendMessage`` JSON-RPC call over ``…/rpc``:
        live ``session.event`` notifications are handed to the ``on_event``
        hook, and the terminal ``{message, events}`` result drives the loop.
        Dispatches any ``tool_use`` blocks the server returns to the registered
        handlers, sends the ``tool_result`` blocks back, and repeats until an
        assistant turn contains no tool calls — which is then returned.
        """
        current = to_message(message)
        while True:
            await self._run_hook("before_send", self._hooks.before_send, current)

            result, notifications = await self._send_message(current)
            for event in notifications:
                await self._run_hook("on_event", self._hooks.on_event, event)

            self.last_events = result.events
            assistant = result.message
            await self._run_hook("after_receive", self._hooks.after_receive, assistant)

            tool_uses = assistant.tool_uses()
            if not tool_uses:
                return assistant

            results: list[Any] = []
            for tu in tool_uses:
                await self._run_hook("before_tool_call", self._hooks.before_tool_call, tu)
                tool = self._registry.get(tu.name)
                if tool is None:
                    raise UnknownToolError(tu.name)
                try:
                    output = await _maybe_await(tool.handler(tu.input))
                except Exception as exc:
                    raise ToolError(tu.name, exc) from exc
                result_block = ToolResultBlock(tool_use_id=tu.id, content=output)
                await self._run_hook("after_tool_call", self._hooks.after_tool_call, result_block)
                results.append(result_block)

            current = Message(role="user", content=results)

    async def subscribe(
        self,
        handler: Callable[[SessionEvent], Union[None, bool, Awaitable[Union[None, bool]]]],
        *,
        since_event_id: str | None = None,
    ) -> None:
        """Subscribe to this session's live ``session.event`` feed via
        ``session.subscribe``, invoking ``handler`` for each event in order.

        With ``since_event_id`` the server first replays persisted events after
        that id, then streams live ones **indefinitely**. The stream is
        open-ended: return ``False`` from ``handler`` to stop reading (dropping
        the connection ends the subscription server-side), or call
        :meth:`unsubscribe` from elsewhere. Returns once the stream ends.
        """
        params: dict[str, Any] = (
            {} if since_event_id is None else {"since_event_id": since_event_id}
        )
        frames = self._transport.stream(
            "POST",
            self.config.url(f"/api/v1/sessions/{self.session_id}/rpc"),
            headers=self._session_auth(),
            json=self._rpc_request("session.subscribe", params),
        )
        async for frame in frames:
            _raise_for_rpc_error(frame)
            if _is_terminal(frame):
                break
            event = _event_from_frame(frame)
            if event is not None:
                if await _maybe_await(handler(event)) is False:
                    break

    async def unsubscribe(self) -> None:
        """End any active :meth:`subscribe` streams for this session
        (``session.unsubscribe``)."""
        frames = self._transport.stream(
            "POST",
            self.config.url(f"/api/v1/sessions/{self.session_id}/rpc"),
            headers=self._session_auth(),
            json=self._rpc_request("session.unsubscribe", {}),
        )
        async for frame in frames:
            _raise_for_rpc_error(frame)
            if _is_terminal(frame):
                break

    async def _register_driver(self) -> None:
        """Issue the one-time ``session.registerDriver`` JSON-RPC call over
        ``…/rpc``, consuming its single terminal ``{registered: true}`` frame.

        Called by :meth:`Harness.connect` / :meth:`Harness.join` during setup;
        application code never triggers it. A ``-32001`` from ``sendMessage``
        means this step was skipped.
        """
        frames = self._transport.stream(
            "POST",
            self.config.url(f"/api/v1/sessions/{self.session_id}/rpc"),
            headers=self._session_auth(),
            json=self._rpc_request("session.registerDriver", {}),
        )
        async for frame in frames:
            _raise_for_rpc_error(frame)
            if _is_terminal(frame):
                break

    async def _send_message(self, message: Message) -> tuple[SendMessageResult, list[SessionEvent]]:
        """Drive one ``session.sendMessage`` turn: stream the NDJSON reply,
        collecting ``session.event`` notifications and resolving on the terminal
        frame. An all-providers-failed turn raises :class:`ProvidersFailedError`;
        a JSON-RPC error object raises :class:`RpcError`.
        """
        frames = self._transport.stream(
            "POST",
            self.config.url(f"/api/v1/sessions/{self.session_id}/rpc"),
            headers=self._session_auth(),
            json=self._rpc_request("session.sendMessage", {"message": message.to_wire()}),
        )
        notifications: list[SessionEvent] = []
        async for frame in frames:
            if _is_terminal(frame):
                _raise_for_rpc_error(frame)
                result = SendMessageResult.from_wire(frame.get("result") or {})
                if _providers_failed(result.events):
                    raise ProvidersFailedError(result.message, result.events)
                return result, notifications
            _raise_for_rpc_error(frame)
            event = _event_from_frame(frame)
            if event is not None:
                notifications.append(event)
        raise RpcError(-32603, "stream ended without a terminal response")

    def _rpc_request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._rpc_id += 1
        return {"jsonrpc": "2.0", "id": self._rpc_id, "method": method, "params": params}

    async def close(self) -> None:
        """Close the session (DELETE) and release the owned transport, if any.

        Idempotent: a second call is a no-op. An already-closed session on the
        server (409) is swallowed.
        """
        if self._closed:
            return
        self._closed = True
        try:
            resp = await self._transport.request(
                "DELETE",
                self.config.url(f"/api/v1/sessions/{self.session_id}"),
                headers=self._session_auth(),
            )
            if resp.status != 409:
                _raise_for_status(resp)
        finally:
            if self._owns_transport:
                await self._transport.aclose()

    async def __aenter__(self) -> "Session":
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    def _session_auth(self) -> dict[str, str]:
        return {
            "Authorization": f"Bearer {self.session_key}",
            "Content-Type": "application/json",
        }

    async def _run_hook(self, name: str, hook: Any, event: Any) -> None:
        if hook is None:
            return
        try:
            await _maybe_await(hook(event))
        except Exception as exc:
            raise HookError(name, exc) from exc


def _raise_for_status(resp: TransportResponse) -> None:
    """Raise :class:`ApiError` for any non-2xx RFC 7807 response."""
    if 200 <= resp.status < 300:
        return
    raise ApiError.from_body(resp.status, resp.body)


def _is_terminal(frame: dict[str, Any]) -> bool:
    """A frame carrying an ``id`` is the terminal response; else a notification."""
    return frame.get("id") is not None


def _raise_for_rpc_error(frame: dict[str, Any]) -> None:
    """Raise :class:`RpcError` if a JSON-RPC frame carries an ``error`` object."""
    err = frame.get("error")
    if err is not None:
        raise RpcError(int(err.get("code", -32603)), str(err.get("message", "")))


def _event_from_frame(frame: dict[str, Any]) -> SessionEvent | None:
    """Decode a ``session.event`` notification's ``params`` into a
    :class:`SessionEvent`; return ``None`` for any other notification."""
    if frame.get("method") != "session.event":
        return None
    params = frame.get("params")
    if params is None:
        return None
    return SessionEvent.from_wire(params)


def _providers_failed(events: list[SessionEvent]) -> bool:
    """Does this turn's event list mark an all-providers-failed outcome? The
    server no longer returns a 502: the failure turn arrives as a normal
    terminal result, distinguished only by a ``session.error``/
    ``all_providers_failed`` event."""
    return any(
        e.event_type == EventType.SESSION_ERROR
        and e.payload.get("reason") == "all_providers_failed"
        for e in events
    )
