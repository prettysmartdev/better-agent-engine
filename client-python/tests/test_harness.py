"""Offline harness tests: tool dispatch, hook ordering/mutation, and error
propagation, all against a scripted mock transport (no server, no API keys).
"""

from __future__ import annotations

import pytest

from bae_py import (
    ApiError,
    Config,
    HookError,
    Hooks,
    Harness,
    Message,
    ProvidersFailedError,
    TextBlock,
    Tool,
    ToolError,
    ToolResultBlock,
    ToolUseBlock,
    UnknownToolError,
)
from bae_py.harness.transport import TransportResponse
from mock_transport import (
    MockTransport,
    assistant_text,
    assistant_tool_call,
    connect_response,
    ok,
)


def _config() -> Config:
    return Config(server_url="http://test", client_key="bae_client", client_version="9.9.9")


def _time_tool(calls: list[dict] | None = None) -> Tool:
    def handler(inp: dict) -> str:
        if calls is not None:
            calls.append(inp)
        return "2026-07-06T00:00:00Z"

    return Tool(
        name="get_current_time",
        description="Return the current time",
        input_schema={"type": "object", "properties": {}},
        handler=handler,
    )


async def test_connect_declares_tools_and_returns_session() -> None:
    transport = MockTransport(script=[connect_response()])
    harness = Harness(_config(), tools=[_time_tool()], transport=transport)

    session = await harness.connect()

    assert session.session_id == "ses_test"
    assert session.session_key == "bae_ses_test"
    assert session.profile.provider["model"] == "claude-sonnet-4-6"
    # The client key authenticated the open call, and tools were declared.
    open_req = transport.requests[0]
    assert open_req.url == "http://test/api/v1/sessions"
    assert open_req.headers["Authorization"] == "Bearer bae_client"
    assert open_req.json["client_version"] == "9.9.9"
    assert open_req.json["tools"][0]["name"] == "get_current_time"


async def test_send_plain_text_turn_returns_assistant() -> None:
    transport = MockTransport(script=[connect_response(), assistant_text("hello there")])
    session = await Harness(_config(), transport=transport).connect()

    reply = await session.send("hi")

    assert isinstance(reply, Message)
    assert reply.role == "assistant"
    assert reply.text() == "hello there"
    # Session key (not client key) authenticated the message call.
    assert transport.requests[1].headers["Authorization"] == "Bearer bae_ses_test"


async def test_tool_call_round_trip_dispatches_and_echoes_id() -> None:
    calls: list[dict] = []
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_call("tu_1", "get_current_time", {"unix": True}),
            assistant_text("it is noon"),
        ]
    )
    session = await Harness(_config(), tools=[_time_tool(calls)], transport=transport).connect()

    reply = await session.send("what time is it?")

    assert reply.text() == "it is noon"
    # The handler ran with the model-supplied input.
    assert calls == [{"unix": True}]
    # The second message POST carried a tool_result echoing tool_use.id.
    followup = transport.requests[2].json["message"]
    assert followup["role"] == "user"
    block = followup["content"][0]
    assert block["type"] == "tool_result"
    assert block["tool_use_id"] == "tu_1"
    assert block["content"] == "2026-07-06T00:00:00Z"


async def test_hook_ordering_across_a_tool_round_trip() -> None:
    order: list[str] = []
    hooks = Hooks(
        before_send=lambda m: order.append("before_send"),
        after_receive=lambda m: order.append("after_receive"),
        before_tool_call=lambda t: order.append("before_tool_call"),
        after_tool_call=lambda r: order.append("after_tool_call"),
    )
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_call("tu_1", "get_current_time"),
            assistant_text("done"),
        ]
    )
    session = await Harness(
        _config(), tools=[_time_tool()], hooks=hooks, transport=transport
    ).connect()

    await session.send("go")

    assert order == [
        "before_send",
        "after_receive",
        "before_tool_call",
        "after_tool_call",
        "before_send",  # the tool_result turn
        "after_receive",
    ]


async def test_before_tool_call_and_after_tool_call_receive_typed_events() -> None:
    seen: dict[str, object] = {}

    def before(tu: ToolUseBlock) -> None:
        seen["use"] = (tu.id, tu.name)

    def after(res: ToolResultBlock) -> None:
        seen["result"] = res.tool_use_id
        # Mutating content changes what is sent back.
        res.content = "REWRITTEN"

    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_call("tu_9", "get_current_time"),
            assistant_text("ok"),
        ]
    )
    hooks = Hooks(before_tool_call=before, after_tool_call=after)
    session = await Harness(
        _config(), tools=[_time_tool()], hooks=hooks, transport=transport
    ).connect()

    await session.send("go")

    assert seen["use"] == ("tu_9", "get_current_time")
    assert seen["result"] == "tu_9"
    assert transport.requests[2].json["message"]["content"][0]["content"] == "REWRITTEN"


async def test_before_send_can_mutate_outgoing_message() -> None:
    def before(m: Message) -> None:
        if isinstance(m.content, str):
            m.content = m.content.upper()

    transport = MockTransport(script=[connect_response(), assistant_text("hi")])
    session = await Harness(
        _config(), hooks=Hooks(before_send=before), transport=transport
    ).connect()

    await session.send("whisper")

    assert transport.requests[1].json["message"]["content"] == "WHISPER"


async def test_async_hook_and_async_handler_are_awaited() -> None:
    order: list[str] = []

    async def before(m: Message) -> None:
        order.append("async_before_send")

    async def handler(inp: dict) -> str:
        order.append("async_handler")
        return "async result"

    tool = Tool("get_current_time", "", {"type": "object"}, handler)
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_call("tu_1", "get_current_time"),
            assistant_text("fin"),
        ]
    )
    session = await Harness(
        _config(), tools=[tool], hooks=Hooks(before_send=before), transport=transport
    ).connect()

    reply = await session.send("go")

    assert reply.text() == "fin"
    assert "async_handler" in order
    assert order.count("async_before_send") == 2


async def test_hook_error_aborts_the_loop() -> None:
    def boom(_m: Message) -> None:
        raise RuntimeError("nope")

    transport = MockTransport(script=[connect_response(), assistant_text("unused")])
    session = await Harness(_config(), hooks=Hooks(before_send=boom), transport=transport).connect()

    with pytest.raises(HookError) as exc:
        await session.send("go")
    assert exc.value.hook == "before_send"
    assert isinstance(exc.value.cause, RuntimeError)
    # The message POST never happened — only the connect request was made.
    assert len(transport.requests) == 1


async def test_unknown_tool_raises() -> None:
    transport = MockTransport(
        script=[connect_response(), assistant_tool_call("tu_1", "not_registered")]
    )
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(UnknownToolError) as exc:
        await session.send("go")
    assert exc.value.name == "not_registered"


async def test_tool_handler_error_propagates_as_tool_error() -> None:
    def handler(_inp: dict) -> str:
        raise ValueError("bad tool")

    tool = Tool("get_current_time", "", {"type": "object"}, handler)
    transport = MockTransport(
        script=[connect_response(), assistant_tool_call("tu_1", "get_current_time")]
    )
    session = await Harness(_config(), tools=[tool], transport=transport).connect()

    with pytest.raises(ToolError) as exc:
        await session.send("go")
    assert exc.value.name == "get_current_time"
    assert isinstance(exc.value.cause, ValueError)


async def test_non_2xx_becomes_api_error_with_slug() -> None:
    problem = TransportResponse(
        status=409,
        body={
            "type": "session_closed",
            "title": "Session is closed",
            "status": 409,
            "detail": "no more turns",
        },
    )
    transport = MockTransport(script=[connect_response(), problem])
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(ApiError) as exc:
        await session.send("go")
    assert exc.value.type == "session_closed"
    assert exc.value.status == 409


async def test_connect_error_maps_and_closes_owned_transport() -> None:
    unauth = TransportResponse(
        status=401, body={"type": "unauthorized", "title": "bad key", "status": 401}
    )
    transport = MockTransport(script=[unauth])
    # No transport injected → the harness owns (and must close) the default one.
    # Here we inject to observe close(); ownership is forced for the assertion.
    harness = Harness(_config(), transport=transport)
    harness._owns_transport = True

    with pytest.raises(ApiError) as exc:
        await harness.connect()
    assert exc.value.type == "unauthorized"
    assert transport.closed is True


async def test_providers_failed_502_raises_with_events() -> None:
    body = {
        "message": {"role": "assistant", "content": [{"type": "text", "text": "unavailable"}]},
        "events": [
            {
                "id": "evt_1",
                "session_id": "ses_test",
                "client_key_id": "key_1",
                "event_type": "provider.response",
                "payload": {"ok": False, "status": None, "error": "env var unset"},
                "created_at": "2026-07-06T00:00:00Z",
            }
        ],
    }
    transport = MockTransport(script=[connect_response(), TransportResponse(status=502, body=body)])
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(ProvidersFailedError) as exc:
        await session.send("go")
    assert exc.value.assistant_message.text() == "unavailable"
    assert exc.value.events[0].payload["error"] == "env var unset"


async def test_send_records_last_events() -> None:
    events = [
        {
            "id": "evt_1",
            "session_id": "ses_test",
            "client_key_id": None,
            "event_type": "server.message.send",
            "payload": {"role": "assistant", "content": []},
            "created_at": "2026-07-06T00:00:00Z",
        }
    ]
    transport = MockTransport(script=[connect_response(), assistant_text("hi", events=events)])
    session = await Harness(_config(), transport=transport).connect()

    await session.send("hi")

    assert len(session.last_events) == 1
    assert session.last_events[0].event_type.value == "server.message.send"


async def test_close_deletes_and_releases_transport() -> None:
    transport = MockTransport(
        script=[
            connect_response(),
            ok({"session_id": "ses_test", "state": "closed"}),
        ]
    )
    harness = Harness(_config(), transport=transport)
    harness._owns_transport = True
    session = await harness.connect()

    await session.close()
    await session.close()  # idempotent

    assert transport.requests[-1].method == "DELETE"
    assert transport.closed is True


async def test_session_as_async_context_manager_closes() -> None:
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_text("hi"),
            ok({"session_id": "ses_test", "state": "closed"}),
        ]
    )
    harness = Harness(_config(), transport=transport)
    harness._owns_transport = True

    async with await harness.connect() as session:
        await session.send("hi")

    assert transport.closed is True
    assert transport.requests[-1].method == "DELETE"


def test_message_helpers() -> None:
    msg = Message(
        role="assistant",
        content=[
            TextBlock(text="hello "),
            ToolUseBlock(id="tu_1", name="t", input={}),
            TextBlock(text="world"),
        ],
    )
    assert msg.text() == "hello world"
    assert [b.name for b in msg.tool_uses()] == ["t"]
    # A string-content message has no tool uses and returns its text verbatim.
    assert Message("user", "plain").tool_uses() == []
    assert Message("user", "plain").text() == "plain"
