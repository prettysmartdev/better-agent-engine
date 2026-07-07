"""Exception hierarchy. Everything the SDK raises derives from :class:`BaeError`."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from .types import Message, SessionEvent


class BaeError(Exception):
    """Base class for every error raised by this SDK."""


class ApiError(BaeError):
    """A non-2xx response carrying an RFC 7807 problem document (§2).

    ``type`` is the short stable slug (e.g. ``unauthorized``, ``not_found``,
    ``tool_not_allowed``) — match on it rather than the human ``title``.
    """

    def __init__(self, type: str, title: str, status: int, detail: str | None = None) -> None:
        self.type = type
        self.title = title
        self.status = status
        self.detail = detail
        msg = f"{status} {type}: {title}"
        if detail:
            msg += f" — {detail}"
        super().__init__(msg)

    @classmethod
    def from_body(cls, status: int, body: Any) -> "ApiError":
        if isinstance(body, dict):
            return cls(
                type=body.get("type", "unknown"),
                title=body.get("title", "request failed"),
                status=int(body.get("status", status)),
                detail=body.get("detail"),
            )
        return cls(type="unknown", title="request failed", status=status, detail=str(body))


class ProvidersFailedError(BaeError):
    """All provider configs failed server-side during a ``session.sendMessage``
    turn; the session is now in the ``error`` state.

    The ``/rpc`` loop delivers this as a normal terminal ``{message, events}``
    result (not a 502 or a JSON-RPC error); the harness recognises the
    ``session.error``/``all_providers_failed`` event in the turn and surfaces it
    here for continuity. Inspect ``events`` for the ``provider.response``
    failures (e.g. an unset provider-key env var).
    """

    def __init__(self, message: "Message", events: "list[SessionEvent]") -> None:
        self.assistant_message = message
        self.events = events
        super().__init__("all provider configs failed")


class RpcError(BaeError):
    """The ``/rpc`` stream carried a JSON-RPC 2.0 error object (HTTP was still
    ``200``).

    Reserved for ``-32700`` parse / ``-32600`` invalid-request / ``-32601``
    method-not-found / ``-32602`` invalid-params / ``-32603`` internal errors
    and ``-32000`` application errors (session-not-open,
    profile-unavailable-mid-session, ``lagged``). Distinct from
    :class:`ApiError`, which is a pre-stream HTTP/RFC-7807 failure (e.g. auth).
    """

    def __init__(self, code: int, message: str) -> None:
        self.code = code
        self.rpc_message = message
        super().__init__(f"JSON-RPC error {code}: {message}")


class UnknownToolError(BaeError):
    """The server called a tool with no registered handler."""

    def __init__(self, name: str) -> None:
        self.name = name
        super().__init__(f"no handler registered for tool {name!r}")


class ToolError(BaeError):
    """A tool handler raised while executing."""

    def __init__(self, name: str, cause: BaseException) -> None:
        self.name = name
        self.cause = cause
        super().__init__(f"tool {name!r} handler failed: {cause}")


class HookError(BaeError):
    """A hook raised, aborting the loop."""

    def __init__(self, hook: str, cause: BaseException) -> None:
        self.hook = hook
        self.cause = cause
        super().__init__(f"hook {hook!r} failed: {cause}")


class TransportError(BaeError):
    """A network or JSON-decoding failure below the API layer."""
