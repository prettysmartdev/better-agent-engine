"""The agent harness: holds config + tools + hooks, opens sessions, and drives
the tool-call round-trip loop (api-contract §6).
"""

from __future__ import annotations

import inspect
from typing import Any

from ..config import Config
from ..errors import (
    ApiError,
    HookError,
    ProvidersFailedError,
    ToolError,
    UnknownToolError,
)
from ..hooks import Hooks
from ..tool import Tool, ToolRegistry
from ..types import (
    Message,
    Profile,
    SessionEvent,
    ToolResultBlock,
    parse_events,
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
        """Exchange the client key for a session, returning a :class:`Session`.

        POSTs ``/api/v1/sessions`` with the declared tools; on success the
        server returns a session id, a one-time session key, and the sanitized
        profile.
        """
        transport = self._transport or HttpxTransport()
        try:
            resp = await transport.request(
                "POST",
                self.config.url("/api/v1/sessions"),
                headers=self._client_auth(),
                json={
                    "client_version": self.config.client_version,
                    "tools": self._registry.declarations(),
                },
            )
            _raise_for_status(resp)
            body = resp.body or {}
            return Session(
                config=self.config,
                transport=transport,
                registry=self._registry,
                hooks=self.hooks,
                session_id=body["session_id"],
                session_key=body["session_key"],
                profile=Profile.from_wire(body["profile"]),
                owns_transport=self._owns_transport,
            )
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

    async def send(self, message: "str | Message") -> Message:
        """Send a user turn and drive the full round-trip (api-contract §6).

        Dispatches any ``tool_use`` blocks the server returns to the registered
        handlers, sends the ``tool_result`` blocks back, and repeats until an
        assistant turn contains no tool calls — which is then returned.
        """
        current = to_message(message)
        while True:
            await self._run_hook("before_send", self._hooks.before_send, current)
            resp = await self._transport.request(
                "POST",
                self.config.url(f"/api/v1/sessions/{self.session_id}/messages"),
                headers=self._session_auth(),
                json={"message": current.to_wire()},
            )
            body = resp.body or {}
            if resp.status == 502:
                raise ProvidersFailedError(
                    Message.from_wire(body.get("message", {})),
                    parse_events(body.get("events")),
                )
            _raise_for_status(resp)

            self.last_events = parse_events(body.get("events"))
            assistant = Message.from_wire(body["message"])
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
                result = ToolResultBlock(tool_use_id=tu.id, content=output)
                await self._run_hook("after_tool_call", self._hooks.after_tool_call, result)
                results.append(result)

            current = Message(role="user", content=results)

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
