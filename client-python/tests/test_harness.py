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
    RpcError,
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
    assistant_tool_calls,
    connect_response,
    ok,
    rpc_error_frame,
    rpc_terminal,
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
    # The second /rpc call carried a session.sendMessage whose params.message is
    # a tool_result echoing tool_use.id.
    envelope = transport.requests[2].json
    assert envelope["method"] == "session.sendMessage"
    followup = envelope["params"]["message"]
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
    assert transport.requests[2].json["params"]["message"]["content"][0]["content"] == "REWRITTEN"


async def test_before_send_can_mutate_outgoing_message() -> None:
    def before(m: Message) -> None:
        if isinstance(m.content, str):
            m.content = m.content.upper()

    transport = MockTransport(script=[connect_response(), assistant_text("hi")])
    session = await Harness(
        _config(), hooks=Hooks(before_send=before), transport=transport
    ).connect()

    await session.send("whisper")

    assert transport.requests[1].json["params"]["message"]["content"] == "WHISPER"


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


async def test_client_dispatch_without_handler_raises_unknown_tool() -> None:
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_call("tu_1", "not_registered", dispatch="client"),
        ]
    )
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(UnknownToolError) as exc:
        await session.send("go")
    assert exc.value.name == "not_registered"


async def test_no_dispatch_falls_back_to_registered_tool_membership() -> None:
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_call("tu_1", "get_current_time"),
            assistant_text("the time is noon"),
        ]
    )
    session = await Harness(_config(), tools=[_time_tool()], transport=transport).connect()

    reply = await session.send("what time is it")

    assert reply.text() == "the time is noon"
    followup = transport.requests[2].json["params"]["message"]
    assert followup["content"] == [
        {
            "type": "tool_result",
            "tool_use_id": "tu_1",
            "content": "2026-07-06T00:00:00Z",
        }
    ]


async def test_null_dispatch_falls_back_to_registered_tool_membership() -> None:
    calls: list[dict] = []
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_calls(
                [
                    {
                        "type": "tool_use",
                        "id": "tu_null",
                        "name": "get_current_time",
                        "input": {},
                        "dispatch": None,
                    }
                ]
            ),
            assistant_text("done"),
        ]
    )
    session = await Harness(_config(), tools=[_time_tool(calls)], transport=transport).connect()

    await session.send("go")

    assert calls == [{}]
    followup = transport.requests[2].json["params"]["message"]
    assert followup["content"] == [
        {
            "type": "tool_result",
            "tool_use_id": "tu_null",
            "content": "2026-07-06T00:00:00Z",
        }
    ]


async def test_dispatch_wins_over_registry_membership_for_same_name_collision() -> None:
    calls: list[dict] = []
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_calls(
                [
                    {
                        "type": "tool_use",
                        "id": "tu_server",
                        "name": "get_current_time",
                        "input": {"owner": "server"},
                        "dispatch": "mcp",
                    },
                    {
                        "type": "tool_use",
                        "id": "tu_client",
                        "name": "get_current_time",
                        "input": {"owner": "client"},
                        "dispatch": "client",
                    },
                ]
            ),
            assistant_text("done"),
        ]
    )
    session = await Harness(_config(), tools=[_time_tool(calls)], transport=transport).connect()

    await session.send("go")

    assert calls == [{"owner": "client"}]
    followup = transport.requests[2].json["params"]["message"]
    assert [block["tool_use_id"] for block in followup["content"]] == ["tu_client"]


async def test_mixed_dispatch_executes_only_client_result_and_surfaces_server_tool() -> None:
    # `issue_read` has no client handler. Its MCP tag must make it
    # informational rather than an UnknownToolError.
    transport = MockTransport(
        script=[
            connect_response(),
            assistant_tool_calls(
                [
                    {
                        "type": "tool_use",
                        "id": "tu_mcp",
                        "name": "issue_read",
                        "input": {"id": 9},
                        "dispatch": "mcp",
                    },
                    {
                        "type": "tool_use",
                        "id": "tu_client",
                        "name": "get_current_time",
                        "input": {},
                        "dispatch": "client",
                    },
                ]
            ),
            assistant_text("done"),
        ]
    )
    informational: list[ToolUseBlock] = []

    def after_receive(message: Message) -> None:
        informational.extend(tu for tu in message.tool_uses() if tu.dispatch in {"mcp", "sandbox"})

    session = await Harness(
        _config(),
        tools=[_time_tool()],
        hooks=Hooks(after_receive=after_receive),
        transport=transport,
    ).connect()

    reply = await session.send("go")

    assert reply.text() == "done"
    followup = transport.requests[2].json["params"]["message"]
    assert followup["content"] == [
        {
            "type": "tool_result",
            "tool_use_id": "tu_client",
            "content": "2026-07-06T00:00:00Z",
        }
    ]
    assert [(tu.id, tu.name, tu.dispatch) for tu in informational] == [
        ("tu_mcp", "issue_read", "mcp")
    ]


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


async def test_rpc_error_object_in_stream_becomes_rpc_error() -> None:
    # A closed/not-open session is a -32000 JSON-RPC error object in the stream
    # (HTTP is still 200), not an HTTP 409 problem doc.
    transport = MockTransport(
        script=[connect_response(), [rpc_error_frame(-32000, "session is not open")]]
    )
    session = await Harness(_config(), transport=transport).connect()

    with pytest.raises(RpcError) as exc:
        await session.send("go")
    assert exc.value.code == -32000
    assert "not open" in exc.value.rpc_message


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


async def test_all_providers_failed_turn_raises_with_events() -> None:
    # The server no longer returns 502: the failure turn arrives as a normal
    # terminal result whose events include session.error/all_providers_failed.
    result = {
        "message": {"role": "assistant", "content": [{"type": "text", "text": "unavailable"}]},
        "events": [
            {
                "id": "evt_1",
                "session_id": "ses_test",
                "client_key_id": "key_1",
                "event_type": "provider.response",
                "payload": {"ok": False, "status": None, "error": "env var unset"},
                "created_at": "2026-07-06T00:00:00Z",
            },
            {
                "id": "evt_2",
                "session_id": "ses_test",
                "client_key_id": "key_1",
                "event_type": "session.error",
                "payload": {"reason": "all_providers_failed"},
                "created_at": "2026-07-06T00:00:01Z",
            },
        ],
    }
    transport = MockTransport(script=[connect_response(), [rpc_terminal(result)]])
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
