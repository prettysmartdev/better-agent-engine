"""OpenTelemetry client-span + ``traceparent``-propagation tests (WI 0013 Parts
D/E; telemetry contract §1.2, §6, §7, and the canonical parity fixture §9).

Kept deliberately identical, scenario for scenario, to the Rust
(``client-rust/src/harness.rs`` ``telemetry_tests``) and TypeScript
(``client-typescript/src/telemetry*.test.ts``) suites.

The one Python-specific technique: instead of installing a **global** OTel SDK
provider (opentelemetry-python's global provider is set-once per process, which
would leak into the "no SDK installed" test), the with-SDK tests monkeypatch the
harness's module-level tracer (``bae_py.harness.core._tracer``) to a real
recording tracer from a *local* ``TracerProvider``. Span nesting and W3C
propagation both run through the shared ``contextvars`` context, not through the
provider, so this exercises the real span-creation and injection code paths
while leaving the process globals untouched — the "no SDK installed" test then
observes genuinely no-op tracing.
"""

from __future__ import annotations

import asyncio
import re
from typing import Any

import bae_py.harness.core as core
from bae_py import Config, Harness, Hooks, Tool
from bae_py.harness.transport import HttpxTransport
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from opentelemetry.trace import SpanKind

from mock_transport import (
    MockTransport,
    assistant_text,
    assistant_tool_call,
    assistant_tool_calls,
    connect_response,
)

_TRACEPARENT = re.compile(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$")


def _config() -> Config:
    return Config(server_url="http://test", client_key="bae_client", client_version="9.9.9")


def _sdk() -> tuple[TracerProvider, InMemorySpanExporter]:
    """A local recording provider with an in-memory exporter (never installed
    as the process global)."""
    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    return provider, exporter


def _use_sdk(monkeypatch: Any, provider: TracerProvider) -> None:
    """Point the harness's tracer at ``provider`` for the duration of one test —
    modelling an embedding app that installed an OTel SDK."""
    monkeypatch.setattr(core, "_tracer", provider.get_tracer("bae.client", core._TRACER_VERSION))


def _named(exporter: InMemorySpanExporter, name: str) -> list[Any]:
    return [s for s in exporter.get_finished_spans() if s.name == name]


# ---------------------------------------------------------------------------
# A canned httpx-shaped client so the FULL connect→send→close flow runs through
# the REAL HttpxTransport (whose request()/stream() are the two injection choke
# points), fully offline. This is the only way to observe the wire headers on
# every outbound request — the MockTransport used by the span tests bypasses the
# transport module entirely.
# ---------------------------------------------------------------------------
class _JsonResp:
    def __init__(self, status: int, body: dict[str, Any]) -> None:
        self.status_code = status
        self.content = b"{}"
        self._body = body

    def json(self) -> dict[str, Any]:
        return self._body

    async def aread(self) -> bytes:
        return self.content


class _LinesResp:
    def __init__(self, status: int, lines: list[str]) -> None:
        self.status_code = status
        self.content = b""
        self._lines = lines

    def json(self) -> dict[str, Any]:
        return {}

    async def aread(self) -> bytes:
        return self.content

    async def aiter_lines(self):
        for line in self._lines:
            yield line


class _StreamCtx:
    def __init__(self, resp: _LinesResp) -> None:
        self._resp = resp

    async def __aenter__(self) -> _LinesResp:
        return self._resp

    async def __aexit__(self, *exc: object) -> bool:
        return False


class _HarnessFakeClient:
    """Canned httpx client covering open (POST /sessions), registerDriver +
    sendMessage (POST /rpc), and close (DELETE /sessions), recording the headers
    seen on each request."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, str, dict[str, str]]] = []

    async def request(self, method: str, url: str, *, headers, json=None):
        self.calls.append((method, str(url), dict(headers)))
        if method == "POST" and str(url).endswith("/api/v1/sessions"):
            return _JsonResp(
                201,
                {
                    "session_id": "ses_test",
                    "session_key": "bae_ses_test",
                    "profile": {
                        "id": "pro",
                        "name": "main",
                        "allowed_tools": ["get_current_time"],
                        "mcp_servers": [],
                        "provider": {"provider": "anthropic", "model": "claude-sonnet-4-6"},
                    },
                },
            )
        return _JsonResp(200, {})

    def stream(self, method: str, url: str, *, headers, json=None):
        self.calls.append((method, str(url), dict(headers)))
        m = json.get("method") if isinstance(json, dict) else None
        if m == "session.registerDriver":
            lines = ['{"jsonrpc":"2.0","id":1,"result":{"registered":true}}']
        else:
            lines = [
                '{"jsonrpc":"2.0","id":1,"result":{"message":{"role":"assistant",'
                '"content":[{"type":"text","text":"hi"}]},"events":[]}}'
            ]
        return _StreamCtx(_LinesResp(200, lines))

    async def aclose(self) -> None:
        pass


def _harness_transport() -> tuple[HttpxTransport, _HarnessFakeClient]:
    transport = HttpxTransport()
    fake = _HarnessFakeClient()
    transport._client = fake  # type: ignore[assignment]
    return transport, fake


# -- 1. Disabled-by-default regression guard: no SDK => no traceparent. -------
async def test_no_sdk_installed_injects_no_traceparent_on_any_request() -> None:
    # No SDK installed: the harness's own spans are non-recording, so the no-op
    # propagator writes nothing — assert the header's absence on EVERY request
    # (session open, registerDriver, sendMessage turn, session close).
    transport, fake = _harness_transport()
    harness = Harness(_config(), transport=transport)
    session = await harness.connect()
    await session.send("hi")
    await session.close()

    assert len(fake.calls) >= 4  # open + registerDriver + sendMessage + close
    for method, url, headers in fake.calls:
        assert "traceparent" not in headers, f"{method} {url}"
        assert "tracestate" not in headers, f"{method} {url}"


# -- 1b. Wire allowlist: baggage the ambient propagator carries is dropped. ---
async def test_baggage_is_not_injected_onto_any_request(monkeypatch) -> None:
    # Model a host app whose global propagator ALSO carries baggage (a common
    # setup). BAE must still put only traceparent/tracestate on the wire — never
    # baggage, which could hold a token/tenant id/prompt fragment (contract §6).
    from opentelemetry import baggage, context as otel_context, propagate
    from opentelemetry.baggage.propagation import W3CBaggagePropagator
    from opentelemetry.propagators.composite import CompositePropagator
    from opentelemetry.trace.propagation.tracecontext import (
        TraceContextTextMapPropagator,
    )

    provider, _exporter = _sdk()
    _use_sdk(monkeypatch, provider)
    # Swap in a composite (trace context + baggage) propagator for this test.
    original = propagate.get_global_textmap()
    propagate.set_global_textmap(
        CompositePropagator([TraceContextTextMapPropagator(), W3CBaggagePropagator()])
    )
    try:
        transport, fake = _harness_transport()
        ctx = baggage.set_baggage("api_token", "fixture-secret")
        token = otel_context.attach(ctx)
        try:
            with provider.get_tracer("app").start_as_current_span("app-root"):
                harness = Harness(_config(), transport=transport)
                session = await harness.connect()
                await session.send("hi")
                await session.close()
        finally:
            otel_context.detach(token)
    finally:
        propagate.set_global_textmap(original)

    assert len(fake.calls) >= 4
    for method, url, headers in fake.calls:
        # trace context present (an app span was active)…
        assert "traceparent" in headers, f"{method} {url}"
        # …but baggage never reaches the wire.
        assert "baggage" not in headers, f"{method} {url}"
        assert "fixture-secret" not in "".join(headers.values()), f"{method} {url}"


# -- 2a. With an SDK installed: traceparent on EVERY outbound request. --------
async def test_traceparent_present_on_every_request_with_sdk(monkeypatch) -> None:
    provider, _exporter = _sdk()
    _use_sdk(monkeypatch, provider)

    transport, fake = _harness_transport()
    # An ambient app span is active around the whole lifecycle, so session
    # open/close (which get no BAE span of their own) still carry context.
    with provider.get_tracer("app").start_as_current_span("app-root"):
        harness = Harness(_config(), transport=transport)
        session = await harness.connect()
        await session.send("hi")
        await session.close()

    assert len(fake.calls) >= 4
    for method, url, headers in fake.calls:
        assert _TRACEPARENT.match(headers.get("traceparent", "")), f"{method} {url}"


# -- 2b + 4. Span hierarchy for the canonical mixed client+mcp turn, plus -----
#            the cross-SDK parity shape (names + attribute keys) per §9.       --
async def test_span_shape_matches_canonical_parity_fixture(monkeypatch) -> None:
    provider, exporter = _sdk()
    _use_sdk(monkeypatch, provider)

    # Turn 1: one dispatch:"client" tool_use + one dispatch:"mcp" tool_use.
    # Turn 2: final text. The client executes only its own tool.
    turn1 = assistant_tool_calls(
        [
            {
                "type": "tool_use",
                "id": "tu_client",
                "name": "get_current_time",
                "input": {},
                "dispatch": "client",
            },
            {
                "type": "tool_use",
                "id": "tu_mcp",
                "name": "remote_search",
                "input": {"q": "x"},
                "dispatch": "mcp",
            },
        ]
    )
    transport = MockTransport(script=[connect_response(), turn1, assistant_text("done")])
    tool = Tool(
        name="get_current_time",
        description="the time",
        input_schema={},
        handler=lambda _inp: "2026-07-06T00:00:00Z",
    )
    harness = Harness(_config(), tools=[tool], transport=transport)
    session = await harness.connect()
    reply = await session.send("go")
    assert reply.text() == "done"

    sends = _named(exporter, "bae.client.send")
    tools = _named(exporter, "bae.client.tool")
    assert len(sends) == 2, "one send span per round trip"
    assert len(tools) == 1, "a client span only for the dispatch:client block, none for mcp"

    # bae.client.send attribute KEYS + literal values (contract §1.2), plus
    # SpanKind and the instrumentation scope name+version — the cross-SDK parity
    # dimensions (contract §0.2). These must match the Rust/TypeScript assertions.
    for s in sends:
        assert set(s.attributes.keys()) == {
            "bae.session.id",
            "bae.rpc.method",
            "bae.client.iteration",
        }
        assert s.attributes["bae.session.id"] == "ses_test"
        assert s.attributes["bae.rpc.method"] == "session.sendMessage"
        assert s.kind == SpanKind.CLIENT
        assert s.instrumentation_scope.name == "bae.client"
        assert s.instrumentation_scope.version == core._TRACER_VERSION
    assert sorted(s.attributes["bae.client.iteration"] for s in sends) == [0, 1]

    # bae.client.tool attribute KEYS + literal values.
    tool_span = tools[0]
    assert set(tool_span.attributes.keys()) == {"bae.tool.name", "bae.tool.dispatch"}
    assert tool_span.attributes["bae.tool.name"] == "get_current_time"
    assert tool_span.attributes["bae.tool.dispatch"] == "client"
    assert tool_span.kind == SpanKind.INTERNAL
    assert tool_span.instrumentation_scope.name == "bae.client"
    assert tool_span.instrumentation_scope.version == core._TRACER_VERSION

    # Parentage: the tool span is a child of the iteration-0 send span.
    send0 = next(s for s in sends if s.attributes["bae.client.iteration"] == 0)
    assert tool_span.parent is not None
    assert tool_span.parent.span_id == send0.context.span_id


# -- 3. Ambient context survives the async boundary into hook + handler. ------
async def test_ambient_context_survives_into_hook_and_tool_handler(monkeypatch) -> None:
    provider, exporter = _sdk()
    _use_sdk(monkeypatch, provider)
    user_tracer = provider.get_tracer("user.code")

    async def handler(_inp: Any) -> str:
        # Open a user span AFTER an await — the case that only nests correctly
        # if the harness's bae.client.tool span survives the await (contract §7).
        await asyncio.sleep(0)
        with user_tracer.start_as_current_span("user.handler.span"):
            pass
        return "ok"

    async def before_tool(_tu: Any) -> None:
        await asyncio.sleep(0)
        with user_tracer.start_as_current_span("user.hook.span"):
            pass

    tool = Tool(name="probe", description="opens a user span", input_schema={}, handler=handler)
    hooks = Hooks(before_tool_call=before_tool)
    transport = MockTransport(
        script=[
            connect_response(allowed_tools=["probe"]),
            assistant_tool_call("tu_1", "probe", dispatch="client"),
            assistant_text("done"),
        ]
    )
    harness = Harness(_config(), tools=[tool], hooks=hooks, transport=transport)
    session = await harness.connect()
    await session.send("go")

    tool_id = _named(exporter, "bae.client.tool")[0].context.span_id
    hook_span = _named(exporter, "user.hook.span")[0]
    handler_span = _named(exporter, "user.handler.span")[0]
    assert hook_span.parent is not None and hook_span.parent.span_id == tool_id
    assert handler_span.parent is not None and handler_span.parent.span_id == tool_id
