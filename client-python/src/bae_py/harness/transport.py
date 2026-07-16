"""The HTTP seam. The harness talks to the server exclusively through a
:class:`Transport`; the default implementation wraps ``httpx.AsyncClient``, and
tests inject a mock through the same protocol so they run fully offline.
"""

from __future__ import annotations

import json as _json
from dataclasses import dataclass
from typing import Any, AsyncIterator, Mapping, Protocol, runtime_checkable

from opentelemetry import propagate

from ..errors import ApiError, TransportError

# The W3C Trace Context wire allowlist (telemetry contract §6): the only headers
# BAE ever injects. Anything else an ambient propagator emits — baggage most
# notably, or any header a custom/composite global propagator writes — is
# dropped, so no baggage value (token, tenant id, prompt fragment) leaks onto a
# BAE request. Matched case-insensitively.
_ALLOWED_PROPAGATION_HEADERS = frozenset({"traceparent", "tracestate"})


def _inject_trace_context(headers: dict[str, str]) -> None:
    """Inject the ambient W3C trace context into ``headers`` in place, restricted
    to the wire allowlist. A no-op propagator or an invalid (no-SDK) span context
    writes nothing, so this costs nothing when the embedding app installed no
    OTel SDK."""
    carrier: dict[str, str] = {}
    propagate.inject(carrier)
    for key, value in carrier.items():
        if key.lower() in _ALLOWED_PROPAGATION_HEADERS:
            headers[key] = value


@dataclass(slots=True)
class TransportResponse:
    """A raw HTTP response: the status code and the parsed JSON body (or ``None``)."""

    status: int
    body: Any


@runtime_checkable
class Transport(Protocol):
    """Minimal async HTTP interface the harness depends on.

    ``request`` covers the REST management routes (session open/close, events
    replay); ``stream`` drives the JSON-RPC session loop over ``…/rpc``,
    yielding one decoded JSON-RPC frame (a ``dict``) per NDJSON line.
    """

    async def request(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> TransportResponse: ...

    def stream(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> AsyncIterator[dict[str, Any]]: ...

    async def aclose(self) -> None: ...


class HttpxTransport:
    """Default transport, backed by a lazily-imported ``httpx.AsyncClient``."""

    def __init__(self, timeout: float = 30.0) -> None:
        try:
            import httpx
        except ImportError as exc:  # pragma: no cover - dependency is declared
            raise TransportError(
                "httpx is required for the default transport; install bae-py with its deps"
            ) from exc
        self._httpx = httpx
        self._client = httpx.AsyncClient(timeout=timeout)

    async def request(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> TransportResponse:
        # Inject W3C traceparent/tracestate from the ambient OTel context, if
        # any (restricted to the wire allowlist — never baggage).
        outgoing = dict(headers)
        _inject_trace_context(outgoing)
        try:
            resp = await self._client.request(method, url, headers=outgoing, json=json)
        except self._httpx.HTTPError as exc:
            raise TransportError(f"request to {url} failed: {exc}") from exc
        body: Any = None
        if resp.content:
            try:
                body = resp.json()
            except ValueError:
                body = None
        return TransportResponse(status=resp.status_code, body=body)

    async def stream(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> AsyncIterator[dict[str, Any]]:
        """POST a JSON-RPC request and yield each NDJSON frame as it arrives.

        A non-2xx status is a pre-stream RFC 7807 error (:class:`ApiError`, e.g.
        auth), raised before the first frame; the stream body itself is HTTP 200.
        """
        outgoing = dict(headers)
        _inject_trace_context(outgoing)
        try:
            async with self._client.stream(method, url, headers=outgoing, json=json) as resp:
                if not (200 <= resp.status_code < 300):
                    await resp.aread()
                    body: Any = None
                    if resp.content:
                        try:
                            body = resp.json()
                        except ValueError:
                            body = None
                    raise ApiError.from_body(resp.status_code, body)
                async for line in resp.aiter_lines():
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        yield _json.loads(line)
                    except ValueError as exc:
                        raise TransportError(
                            f"malformed JSON-RPC frame from {method} {url}"
                        ) from exc
        except self._httpx.HTTPError as exc:
            raise TransportError(f"request to {url} failed: {exc}") from exc

    async def aclose(self) -> None:
        await self._client.aclose()
