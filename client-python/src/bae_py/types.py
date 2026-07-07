"""Wire types: the Anthropic-style content model, the session-event union, and
the sanitized profile view.

The event model is deliberately *closed*. ``EventType`` enumerates exactly the
twelve strings the server may emit (api-contract §8); parsing an unknown string
raises immediately, and :func:`describe_event` switches over every arm with an
``assert_never`` fall-through, so adding a new event type without handling it is
a loud failure rather than a silent pass-through.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, NoReturn, Union

# ---------------------------------------------------------------------------
# Content blocks (discriminated on ``type``)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class TextBlock:
    text: str
    type: str = "text"

    def to_wire(self) -> dict[str, Any]:
        return {"type": "text", "text": self.text}


@dataclass(slots=True)
class ToolUseBlock:
    id: str
    name: str
    input: dict[str, Any] = field(default_factory=dict)
    type: str = "tool_use"

    def to_wire(self) -> dict[str, Any]:
        return {"type": "tool_use", "id": self.id, "name": self.name, "input": self.input}


@dataclass(slots=True)
class ToolResultBlock:
    tool_use_id: str
    content: "Content"
    type: str = "tool_result"

    def to_wire(self) -> dict[str, Any]:
        return {
            "type": "tool_result",
            "tool_use_id": self.tool_use_id,
            "content": content_to_wire(self.content),
        }


ContentBlock = Union[TextBlock, ToolUseBlock, ToolResultBlock]

# ``content`` is either a plain string or a list of blocks (api-contract §6).
Content = Union[str, list[ContentBlock]]


def parse_block(raw: dict[str, Any]) -> ContentBlock:
    """Parse one content block, failing loudly on an unrecognized ``type``."""
    t = raw.get("type")
    match t:
        case "text":
            return TextBlock(text=raw.get("text", ""))
        case "tool_use":
            return ToolUseBlock(id=raw["id"], name=raw["name"], input=raw.get("input") or {})
        case "tool_result":
            return ToolResultBlock(
                tool_use_id=raw["tool_use_id"], content=parse_content(raw.get("content", []))
            )
        case _:
            raise ValueError(f"unknown content block type: {t!r}")


def parse_content(raw: Any) -> Content:
    """Normalize wire ``content`` (string, list of blocks, or ``None``)."""
    if raw is None:
        return []
    if isinstance(raw, str):
        return raw
    if isinstance(raw, list):
        return [parse_block(b) for b in raw]
    raise TypeError(f"content must be a string or list, got {type(raw).__name__}")


def content_to_wire(content: Content) -> Any:
    if isinstance(content, str):
        return content
    return [b.to_wire() for b in content]


# ---------------------------------------------------------------------------
# Messages
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class Message:
    """A conversation turn — the unit :meth:`Session.send` sends and returns."""

    role: str
    content: Content

    def to_wire(self) -> dict[str, Any]:
        return {"role": self.role, "content": content_to_wire(self.content)}

    @classmethod
    def from_wire(cls, raw: dict[str, Any]) -> "Message":
        return cls(role=raw.get("role", "assistant"), content=parse_content(raw.get("content")))

    def text(self) -> str:
        """Concatenate all text blocks (or return the string content verbatim)."""
        if isinstance(self.content, str):
            return self.content
        return "".join(b.text for b in self.content if isinstance(b, TextBlock))

    def tool_uses(self) -> list[ToolUseBlock]:
        """Extract ``tool_use`` blocks. An empty list means the loop is done."""
        if isinstance(self.content, str):
            return []
        return [b for b in self.content if isinstance(b, ToolUseBlock)]


def to_message(value: "str | Message") -> Message:
    """Normalize a bare string into a ``user`` :class:`Message`."""
    if isinstance(value, Message):
        return value
    return Message(role="user", content=value)


# ---------------------------------------------------------------------------
# Sanitized profile (returned at session open)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class Profile:
    id: str
    name: str
    allowed_tools: list[str]
    mcp_servers: list[Any]
    provider: dict[str, Any]

    @classmethod
    def from_wire(cls, raw: dict[str, Any]) -> "Profile":
        return cls(
            id=raw["id"],
            name=raw["name"],
            allowed_tools=list(raw.get("allowed_tools") or []),
            mcp_servers=list(raw.get("mcp_servers") or []),
            provider=dict(raw.get("provider") or {}),
        )


# ---------------------------------------------------------------------------
# Session events (the closed twelve-member set)
# ---------------------------------------------------------------------------


class EventType(str, Enum):
    """The complete, closed set of ``session_events.event_type`` values (§8)."""

    CLIENT_MESSAGE_SEND = "client.message.send"
    SERVER_MESSAGE_SEND = "server.message.send"
    PROVIDER_REQUEST = "provider.request"
    PROVIDER_RESPONSE = "provider.response"
    TOOL_CALL = "tool.call"
    TOOL_RESULT = "tool.result"
    MCP_REQUEST = "mcp.request"
    MCP_RESPONSE = "mcp.response"
    SESSION_OPEN = "session.open"
    SESSION_CLOSE = "session.close"
    SESSION_ERROR = "session.error"
    SESSION_COMPACTION = "session.compaction"


@dataclass(slots=True)
class SessionEvent:
    """One row from ``session_events`` (the ``EventView`` wire shape, §5.2)."""

    id: str
    session_id: str
    client_key_id: str | None
    event_type: EventType
    payload: dict[str, Any]
    created_at: str

    @classmethod
    def from_wire(cls, raw: dict[str, Any]) -> "SessionEvent":
        # ``EventType(...)`` raises ValueError on an unknown string — the server
        # introducing a type this SDK does not model fails loudly here.
        return cls(
            id=raw["id"],
            session_id=raw["session_id"],
            client_key_id=raw.get("client_key_id"),
            event_type=EventType(raw["event_type"]),
            payload=raw.get("payload") or {},
            created_at=raw["created_at"],
        )


def parse_events(raw: Any) -> list[SessionEvent]:
    return [SessionEvent.from_wire(e) for e in (raw or [])]


def assert_never(value: NoReturn) -> NoReturn:
    """Static-exhaustiveness marker: reaching this at runtime is a bug."""
    raise AssertionError(f"unhandled event type: {value!r}")


# ---------------------------------------------------------------------------
# MCP event payloads (the real, non-stub shapes emitted by the engine)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class McpRequestPayload:
    """Payload of an ``mcp.request`` event: the engine calling a configured
    server. Parse it from a :class:`SessionEvent` whose ``event_type`` is
    :attr:`EventType.MCP_REQUEST`.
    """

    method: str  # the MCP method invoked (currently always "tools/call")
    server_name: str | None  # the server the call routed to, or None if unroutable
    tool: str
    input: dict[str, Any]

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> "McpRequestPayload":
        return cls(
            method=payload.get("method", ""),
            server_name=payload.get("server_name"),
            tool=payload.get("tool", ""),
            input=payload.get("input") or {},
        )


@dataclass(slots=True)
class McpResponsePayload:
    """Payload of an ``mcp.response`` event. ``ok`` discriminates success
    (``result`` set) from failure (``error`` set). Parse it from a
    :class:`SessionEvent` whose ``event_type`` is :attr:`EventType.MCP_RESPONSE`.
    """

    server_name: str | None
    ok: bool
    result: dict[str, Any] | None = None
    error: str | None = None

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> "McpResponsePayload":
        return cls(
            server_name=payload.get("server_name"),
            ok=bool(payload.get("ok")),
            result=payload.get("result"),
            error=payload.get("error"),
        )


# ---------------------------------------------------------------------------
# JSON-RPC 2.0 envelopes for the session loop (`POST …/rpc`)
#
# The management routes (session open/close, events replay) stay plain REST;
# only the message loop is JSON-RPC. A request is POSTed to
# ``POST /api/v1/sessions/{id}/rpc`` and the reply is an ``application/x-ndjson``
# stream of these envelopes: a frame with no ``id`` is a notification (its
# ``params`` carry a ``session.event``); the frame carrying the request ``id``
# is the terminal response (``result`` on success, ``error`` on failure).
# ---------------------------------------------------------------------------

# The JSON-RPC methods the session loop understands.
RPC_METHODS = ("session.sendMessage", "session.subscribe", "session.unsubscribe")


@dataclass(slots=True)
class JsonRpcError:
    """A JSON-RPC 2.0 error object (terminal, or a mid-stream notice)."""

    code: int
    message: str
    data: Any = None


@dataclass(slots=True)
class JsonRpcRequest:
    """A JSON-RPC 2.0 request envelope."""

    id: int
    method: str
    params: dict[str, Any]

    def to_wire(self) -> dict[str, Any]:
        return {"jsonrpc": "2.0", "id": self.id, "method": self.method, "params": self.params}


@dataclass(slots=True)
class SendMessageResult:
    """The terminal ``result`` of a ``session.sendMessage`` call — the same
    ``{message, events}`` body the legacy synchronous message route returned.
    ``events`` is the full turn event list; the live ``session.event``
    notifications are an additive, filtered subset of it.
    """

    message: Message
    events: list[SessionEvent]

    @classmethod
    def from_wire(cls, raw: dict[str, Any]) -> "SendMessageResult":
        return cls(
            message=Message.from_wire(raw.get("message") or {}),
            events=parse_events(raw.get("events")),
        )


def describe_event(event: SessionEvent) -> str:
    """One-line human description of an event.

    The ``match`` is exhaustive over all twelve members; the ``_`` arm hands the
    value to :func:`assert_never`, so a type checker flags any newly added
    ``EventType`` member that is not given a ``case`` here.
    """
    et = event.event_type
    match et:
        case EventType.CLIENT_MESSAGE_SEND:
            return "client sent a user turn"
        case EventType.SERVER_MESSAGE_SEND:
            return "server sent an assistant turn"
        case EventType.PROVIDER_REQUEST:
            return "request dispatched to the provider"
        case EventType.PROVIDER_RESPONSE:
            ok = event.payload.get("ok")
            return f"provider response (ok={ok})"
        case EventType.TOOL_CALL:
            return f"tool call: {event.payload.get('name')} ({event.payload.get('dispatch')})"
        case EventType.TOOL_RESULT:
            return f"tool result ({event.payload.get('dispatch')})"
        case EventType.MCP_REQUEST:
            server = event.payload.get("server_name") or "<unrouted>"
            return f"MCP request: {event.payload.get('tool')} → {server}"
        case EventType.MCP_RESPONSE:
            server = event.payload.get("server_name") or "<unrouted>"
            return f"MCP response from {server} (ok={event.payload.get('ok')})"
        case EventType.SESSION_OPEN:
            return "session opened"
        case EventType.SESSION_CLOSE:
            return f"session closed ({event.payload.get('reason')})"
        case EventType.SESSION_ERROR:
            return f"session error ({event.payload.get('reason')})"
        case EventType.SESSION_COMPACTION:
            return "session history compacted"
        case _:
            assert_never(et)
