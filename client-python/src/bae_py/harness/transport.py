"""The HTTP seam. The harness talks to the server exclusively through a
:class:`Transport`; the default implementation wraps ``httpx.AsyncClient``, and
tests inject a mock through the same protocol so they run fully offline.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Protocol, runtime_checkable

from ..errors import TransportError


@dataclass(slots=True)
class TransportResponse:
    """A raw HTTP response: the status code and the parsed JSON body (or ``None``)."""

    status: int
    body: Any


@runtime_checkable
class Transport(Protocol):
    """Minimal async HTTP interface the harness depends on."""

    async def request(
        self,
        method: str,
        url: str,
        *,
        headers: Mapping[str, str],
        json: Any | None = None,
    ) -> TransportResponse: ...

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
        try:
            resp = await self._client.request(method, url, headers=dict(headers), json=json)
        except self._httpx.HTTPError as exc:
            raise TransportError(f"request to {url} failed: {exc}") from exc
        body: Any = None
        if resp.content:
            try:
                body = resp.json()
            except ValueError:
                body = None
        return TransportResponse(status=resp.status_code, body=body)

    async def aclose(self) -> None:
        await self._client.aclose()
