"""Wire types: the Anthropic-style content model, the session-event union, and
the sanitized profile view.

The event model is deliberately *closed*. ``EventType`` enumerates exactly the
twenty-eight strings the server may emit (api-contract §8); parsing an unknown
string raises immediately, and :func:`describe_event` switches over every arm
with an ``assert_never`` fall-through, so adding a new event type without
handling it is a loud failure rather than a silent pass-through.
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
    # Server-selected owner for this invocation. This receive-only routing tag
    # is omitted by older servers and must not be echoed in a tool_result turn.
    dispatch: str | None = None
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
            return ToolUseBlock(
                id=raw["id"],
                name=raw["name"],
                input=raw.get("input") or {},
                dispatch=raw.get("dispatch"),
            )
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
# Session events (the closed twenty-eight-member set)
# ---------------------------------------------------------------------------


class EventType(str, Enum):
    """The complete, closed set of ``session_events.event_type`` values (§8).

    WI 0006 grew this from 14 to 22 with the sandbox lifecycle and dispatch
    events, and WI 0010 to 28 with the subagent lifecycle events; the ``SANDBOX_*`` members must be present or :meth:`SessionEvent.from_wire`
    would raise on a sandbox event the server now legitimately emits.
    """

    CLIENT_MESSAGE_SEND = "client.message.send"
    SERVER_MESSAGE_SEND = "server.message.send"
    PROVIDER_REQUEST = "provider.request"
    PROVIDER_RESPONSE = "provider.response"
    TOOL_CALL = "tool.call"
    TOOL_RESULT = "tool.result"
    MCP_REQUEST = "mcp.request"
    MCP_RESPONSE = "mcp.response"
    SESSION_OPEN = "session.open"
    SESSION_JOIN = "session.join"
    SESSION_DRIVER_REGISTER = "session.driver.register"
    SESSION_CLOSE = "session.close"
    SESSION_ERROR = "session.error"
    SESSION_COMPACTION_STARTED = "session.compaction.started"
    SESSION_COMPACTION_COMPLETED = "session.compaction.completed"
    # Sandbox lifecycle (WI 0006); ``dispatch`` in the payload is ``remote`` or
    # ``local`` for the running/stopped/error trio.
    SANDBOX_AVAILABLE = "session.sandbox.available"
    SANDBOX_START = "session.sandbox.start"
    SANDBOX_RUNNING = "session.sandbox.running"
    SANDBOX_STOP = "session.sandbox.stop"
    SANDBOX_STOPPED = "session.sandbox.stopped"
    SANDBOX_ERROR = "session.sandbox.error"
    # Sandbox Auto-dispatch round-trip (unprefixed, mirroring mcp.request/response).
    SANDBOX_REQUEST = "sandbox.request"
    SANDBOX_RESPONSE = "sandbox.response"
    # Subagent lifecycle (WI 0010); ``dispatch`` in the payload is ``local`` or
    # ``remote``. Appended after SANDBOX_RESPONSE per the interface contract.
    SUBAGENT_START = "session.subagent.start"
    SUBAGENT_RUNNING = "session.subagent.running"
    SUBAGENT_COMPLETED = "session.subagent.completed"
    SUBAGENT_FAILED = "session.subagent.failed"
    SUBAGENT_CANCELLED = "session.subagent.cancelled"


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
# Multi-driver event payloads (WI 0005)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class SessionJoinPayload:
    """Payload of a ``session.join`` event: a second (or further) client key
    minted a session key for an existing session via ``POST …/join``. Same shape
    as ``session.open``; the joining client is the event's ``client_key_id``.
    Parse it from a :class:`SessionEvent` whose ``event_type`` is
    :attr:`EventType.SESSION_JOIN`.
    """

    client_version: str | None
    tools: list[str]

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> "SessionJoinPayload":
        return cls(
            client_version=payload.get("client_version"),
            tools=list(payload.get("tools") or []),
        )


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
# Compaction (session-level config, typed compaction events, synthetic messages)
# ---------------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class AutoCompaction:
    """Compact automatically once the session reaches ``size`` tokens.
    Serializes as ``{"mode": "auto", "size": size}``."""

    size: int

    def to_wire(self) -> dict[str, Any]:
        return {"mode": "auto", "size": self.size}


@dataclass(frozen=True, slots=True)
class ClientCompaction:
    """Compact only when a driver calls :meth:`Session.compact`, with
    ``prompt`` (or the server default when ``None``) as the session's default
    summarization prompt. Serializes as ``{"mode": "client"}`` or
    ``{"mode": "client", "prompt": prompt}`` (never ``"prompt": null``)."""

    prompt: str | None = None

    def to_wire(self) -> dict[str, Any]:
        if self.prompt is None:
            return {"mode": "client"}
        return {"mode": "client", "prompt": self.prompt}


# Session-level compaction, fixed at creation (sent by ``Harness.connect`` only).
CompactionConfig = Union[AutoCompaction, ClientCompaction]

# ``payload.synthetic`` on the user-role preamble that precedes a compaction summary.
SYNTHETIC_COMPACTION_PREAMBLE = "compaction_preamble"
# ``payload.synthetic`` on the user-role tool results the server writes when it
# retires an expired paused turn.
SYNTHETIC_ABANDONED_TOOL_RESULTS = "abandoned_tool_results"


@dataclass(slots=True)
class ServerMessagePayload:
    """Payload of a ``server.message.send`` event. ``role`` is ``"assistant"``
    for model turns and ``"user"`` only on server-written synthetic messages,
    which carry ``synthetic`` naming why they exist (e.g.
    :data:`SYNTHETIC_COMPACTION_PREAMBLE`). ``content`` is the raw block list,
    verbatim. :class:`SessionEvent` keeps the whole payload as a dict, so
    ``synthetic`` is never dropped.
    """

    role: str
    content: Any
    synthetic: str | None = None

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> "ServerMessagePayload":
        return cls(
            role=payload.get("role", "assistant"),
            content=payload.get("content"),
            synthetic=payload.get("synthetic"),
        )

    @property
    def is_compaction_preamble(self) -> bool:
        return self.synthetic == SYNTHETIC_COMPACTION_PREAMBLE


@dataclass(slots=True)
class SessionCompactionStartedPayload:
    """Payload of a ``session.compaction.started`` event. ``trigger`` is
    ``"auto"`` or ``"client"``; ``reason`` is ``"token_threshold"``,
    ``"retry_after_failure"`` (auto only) or ``"manual"``."""

    trigger: str
    reason: str
    token_count: int | None = None
    threshold_tokens: int | None = None

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> "SessionCompactionStartedPayload":
        return cls(
            trigger=payload.get("trigger", ""),
            reason=payload.get("reason", ""),
            token_count=payload.get("token_count"),
            threshold_tokens=payload.get("threshold_tokens"),
        )


@dataclass(slots=True)
class SessionCompactionCompletedPayload:
    """Payload of a ``session.compaction.completed`` event.
    ``preamble_event_id`` is ``None`` on events written by servers that predate
    the compaction preamble."""

    summary_event_id: str
    compacted_message_count: int
    preamble_event_id: str | None = None
    input_tokens: int | None = None
    summary_tokens: int | None = None

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> "SessionCompactionCompletedPayload":
        return cls(
            # Required fields: a malformed record raises (KeyError/TypeError/
            # ValueError) instead of yielding a fake-success default.
            summary_event_id=str(payload["summary_event_id"]),
            compacted_message_count=int(payload["compacted_message_count"]),
            preamble_event_id=payload.get("preamble_event_id"),
            input_tokens=payload.get("input_tokens"),
            summary_tokens=payload.get("summary_tokens"),
        )


@dataclass(slots=True)
class SessionCompactionStarted:
    """A typed ``session.compaction.started`` event."""

    id: str
    session_id: str
    client_key_id: str | None
    created_at: str
    payload: SessionCompactionStartedPayload

    @classmethod
    def from_event(cls, event: SessionEvent) -> "SessionCompactionStarted":
        return cls(
            id=event.id,
            session_id=event.session_id,
            client_key_id=event.client_key_id,
            created_at=event.created_at,
            payload=SessionCompactionStartedPayload.from_payload(event.payload),
        )


@dataclass(slots=True)
class SessionCompactionCompleted:
    """A typed ``session.compaction.completed`` event — the terminal result of
    :meth:`Session.compact`."""

    id: str
    session_id: str
    client_key_id: str | None
    created_at: str
    payload: SessionCompactionCompletedPayload

    @classmethod
    def from_event(cls, event: SessionEvent) -> "SessionCompactionCompleted":
        return cls(
            id=event.id,
            session_id=event.session_id,
            client_key_id=event.client_key_id,
            created_at=event.created_at,
            payload=SessionCompactionCompletedPayload.from_payload(event.payload),
        )

    @classmethod
    def from_wire(cls, raw: dict[str, Any]) -> "SessionCompactionCompleted":
        return cls.from_event(SessionEvent.from_wire(raw))


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
RPC_METHODS = (
    "session.sendMessage",
    "session.subscribe",
    "session.unsubscribe",
    "session.compact",
)


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

    The ``match`` is exhaustive over all twenty-eight members; the ``_`` arm hands the
    value to :func:`assert_never`, so a type checker flags any newly added
    ``EventType`` member that is not given a ``case`` here.
    """
    et = event.event_type
    match et:
        case EventType.CLIENT_MESSAGE_SEND:
            return "client sent a user turn"
        case EventType.SERVER_MESSAGE_SEND:
            synthetic = event.payload.get("synthetic")
            if synthetic == SYNTHETIC_COMPACTION_PREAMBLE:
                return "server wrote the compaction preamble (synthetic)"
            if synthetic is not None:
                role = event.payload.get("role", "assistant")
                return f"server wrote a synthetic {role} message ({synthetic})"
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
        case EventType.SESSION_JOIN:
            return "driver joined the session"
        case EventType.SESSION_DRIVER_REGISTER:
            return "driver registered"
        case EventType.SESSION_CLOSE:
            return f"session closed ({event.payload.get('reason')})"
        case EventType.SESSION_ERROR:
            return f"session error ({event.payload.get('reason')})"
        case EventType.SESSION_COMPACTION_STARTED:
            return "session compaction started"
        case EventType.SESSION_COMPACTION_COMPLETED:
            return "session compaction completed"
        case EventType.SANDBOX_AVAILABLE:
            return "sandbox images available"
        case EventType.SANDBOX_START:
            return f"sandbox starting ({event.payload.get('dispatch')})"
        case EventType.SANDBOX_RUNNING:
            return f"sandbox running ({event.payload.get('dispatch')})"
        case EventType.SANDBOX_STOP:
            return f"sandbox stopping ({event.payload.get('dispatch')})"
        case EventType.SANDBOX_STOPPED:
            return f"sandbox stopped ({event.payload.get('dispatch')})"
        case EventType.SANDBOX_ERROR:
            return f"sandbox error ({event.payload.get('dispatch')})"
        case EventType.SANDBOX_REQUEST:
            return f"sandbox request: {event.payload.get('tool')}"
        case EventType.SANDBOX_RESPONSE:
            return f"sandbox response (ok={event.payload.get('ok')})"
        case EventType.SUBAGENT_START:
            return f"subagent starting: {event.payload.get('harness')} ({event.payload.get('dispatch')})"
        case EventType.SUBAGENT_RUNNING:
            return f"subagent running: {event.payload.get('harness')} ({event.payload.get('dispatch')})"
        case EventType.SUBAGENT_COMPLETED:
            return f"subagent completed (exit_code={event.payload.get('exit_code')})"
        case EventType.SUBAGENT_FAILED:
            return f"subagent failed ({event.payload.get('reason')})"
        case EventType.SUBAGENT_CANCELLED:
            return f"subagent cancelled ({event.payload.get('reason')})"
        case _:
            assert_never(et)
