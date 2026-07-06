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
    """A ``502`` from ``…/messages``: all provider configs failed (§5.2).

    The body is the normal ``{message, events}`` shape (not a problem doc); the
    session is now in the ``error`` state. Inspect ``events`` for the
    ``provider.response`` failures (e.g. an unset provider-key env var).
    """

    def __init__(self, message: "Message", events: "list[SessionEvent]") -> None:
        self.assistant_message = message
        self.events = events
        super().__init__("all provider configs failed (HTTP 502)")


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
