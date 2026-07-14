"""A scripted, fully-offline :class:`~bae_py.harness.Transport` for the tests.

REST calls (session open/close) go through ``request`` and pop a
:class:`TransportResponse`; the JSON-RPC session loop goes through ``stream``
and pops a *list of frames* (the NDJSON reply) to yield. Both share the one
``script`` list so a single ordered queue drives the whole exchange, and every
call is recorded on ``.requests`` for assertions. No network, no keys.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Callable, Mapping, Union

from bae_py.errors import ApiError
from bae_py.harness.transport import TransportResponse

Responder = Callable[[str, str, Any], TransportResponse]
Scripted = Union[TransportResponse, Responder]
# A `/rpc` reply: the list of JSON-RPC frames the stream yields, or a non-2xx
# TransportResponse to simulate a pre-stream HTTP error (e.g. auth).
StreamScripted = Union[list[dict[str, Any]], TransportResponse]


@dataclass
class RecordedRequest:
    method: str
    url: str
    headers: dict[str, str]
    json: Any


@dataclass
class MockTransport:
    script: list[Any]
    requests: list[RecordedRequest] = field(default_factory=list)
    #: ``session.registerDriver`` calls, recorded separately and answered with a
    #: canned ``{registered: true}`` frame. connect()/join() each issue one
    #: during setup; keeping them out of ``script``/``requests`` leaves the
    #: ordered queue of the REST + sendMessage exchange untouched.
    register_driver_calls: list[RecordedRequest] = field(default_factory=list)
    closed: bool = False

    async def request(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> TransportResponse:
        self.requests.append(RecordedRequest(method, url, dict(headers), json))
        if not self.script:
            raise AssertionError(f"unexpected request: {method} {url}")
        item = self.script.pop(0)
        if callable(item):
            return item(method, url, json)
        return item

    async def stream(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> AsyncIterator[dict[str, Any]]:
        # Driver registration is auto-answered and tracked separately, so the
        # script queue stays aligned with the ordinary REST + sendMessage calls.
        if isinstance(json, dict) and json.get("method") == "session.registerDriver":
            self.register_driver_calls.append(RecordedRequest(method, url, dict(headers), json))
            yield {"jsonrpc": "2.0", "id": json.get("id", 1), "result": {"registered": True}}
            return
        self.requests.append(RecordedRequest(method, url, dict(headers), json))
        if not self.script:
            raise AssertionError(f"unexpected stream: {method} {url}")
        item = self.script.pop(0)
        # A TransportResponse in a stream slot models a pre-stream HTTP error.
        if isinstance(item, TransportResponse):
            if not (200 <= item.status < 300):
                raise ApiError.from_body(item.status, item.body)
            raise AssertionError("stream script expected frames, got a 2xx TransportResponse")
        for frame in item:
            yield frame

    async def aclose(self) -> None:
        self.closed = True


def ok(body: Any) -> TransportResponse:
    return TransportResponse(status=200, body=body)


def created(body: Any) -> TransportResponse:
    return TransportResponse(status=201, body=body)


def rpc_terminal(result: Any, id: int = 1) -> dict[str, Any]:
    """A terminal JSON-RPC frame carrying the request ``id`` and a ``result``."""
    return {"jsonrpc": "2.0", "id": id, "result": result}


def rpc_error_frame(code: int, message: str, id: int = 1) -> dict[str, Any]:
    """A terminal JSON-RPC frame carrying an ``error`` object."""
    return {"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}


def rpc_notification(event: dict[str, Any]) -> dict[str, Any]:
    """A `session.event` notification frame (no ``id``)."""
    return {"jsonrpc": "2.0", "method": "session.event", "params": event}


def connect_response(
    session_id: str = "ses_test",
    session_key: str = "bae_ses_test",
    allowed_tools: list[str] | None = None,
) -> TransportResponse:
    return created(
        {
            "session_id": session_id,
            "session_key": session_key,
            "profile": {
                "id": "pro_test",
                "name": "main",
                "allowed_tools": allowed_tools or ["get_current_time"],
                "mcp_servers": [],
                "provider": {"provider": "anthropic", "model": "claude-sonnet-4-6"},
            },
        }
    )


def assistant_text(text: str, events: list[Any] | None = None) -> list[dict[str, Any]]:
    """A one-frame `/rpc` reply: a terminal text turn plus its events."""
    return [
        rpc_terminal(
            {
                "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
                "events": events or [],
            }
        )
    ]


def assistant_tool_call(
    tool_use_id: str,
    name: str,
    tool_input: dict[str, Any] | None = None,
    dispatch: str | None = None,
) -> list[dict[str, Any]]:
    """A one-frame `/rpc` reply: a terminal turn requesting a tool call."""
    tool_use: dict[str, Any] = {
        "type": "tool_use",
        "id": tool_use_id,
        "name": name,
        "input": tool_input or {},
    }
    if dispatch is not None:
        tool_use["dispatch"] = dispatch
    return assistant_tool_calls([tool_use])


def assistant_tool_calls(tool_uses: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """A one-frame `/rpc` reply containing an assistant tool-use turn."""
    return [
        rpc_terminal(
            {
                "message": {
                    "role": "assistant",
                    "content": tool_uses,
                },
                "events": [],
            }
        )
    ]
