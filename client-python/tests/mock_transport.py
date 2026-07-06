"""A scripted, fully-offline :class:`~bae_py.harness.Transport` for the tests.

Each request pops the next queued response (or invokes a callable responder);
every request is recorded on ``.requests`` for assertions. No network, no keys.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Callable, Mapping, Union

from bae_py.harness.transport import TransportResponse

Responder = Callable[[str, str, Any], TransportResponse]
Scripted = Union[TransportResponse, Responder]


@dataclass
class RecordedRequest:
    method: str
    url: str
    headers: dict[str, str]
    json: Any


@dataclass
class MockTransport:
    script: list[Scripted]
    requests: list[RecordedRequest] = field(default_factory=list)
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

    async def aclose(self) -> None:
        self.closed = True


def ok(body: Any) -> TransportResponse:
    return TransportResponse(status=200, body=body)


def created(body: Any) -> TransportResponse:
    return TransportResponse(status=201, body=body)


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


def assistant_text(text: str, events: list[Any] | None = None) -> TransportResponse:
    return ok(
        {
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "events": events or [],
        }
    )


def assistant_tool_call(
    tool_use_id: str, name: str, tool_input: dict[str, Any] | None = None
) -> TransportResponse:
    return ok(
        {
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": tool_use_id,
                        "name": name,
                        "input": tool_input or {},
                    }
                ],
            },
            "events": [],
        }
    )
