#!/usr/bin/env python3
"""reference-assistant — the canonical example agent (per aspec/genai/agents.md),
implemented identically across the Rust, TypeScript, and Python SDKs.

It registers a single client-side tool (``get_current_time``), opens a session,
sends one user turn, drives the harness loop (dispatching the tool call and
sending the result back), and prints the final assistant text. Every hook point
is exercised at least once — a counter proves it on exit.

Configuration (all via environment):
  BAE_SERVER_URL       server base URL          (default http://localhost:8080)
  BAE_CLIENT_KEY       the bae_… client key     (required)
  BAE_PROVIDER_KEY_ENV name of the provider-key env var the profile references
                                                (default ANTHROPIC_API_KEY)

The provider key is a *server-side* concern: the SDK never sends it. But we
fail fast with a clear message if it is unset locally, and we also catch the
server's 502 "all providers failed" outcome (surfaced as ProvidersFailedError)
and explain the likely cause.

Run:  uv run python examples/reference-assistant/main.py "What time is it?"
"""

from __future__ import annotations

import asyncio
import os
import sys
from datetime import datetime, timezone

from bae_py import (
    ApiError,
    Config,
    Harness,
    Hooks,
    Message,
    ProvidersFailedError,
    Tool,
    ToolResultBlock,
    ToolUseBlock,
    TransportError,
    describe_event,
    random_hex,
)

DEFAULT_PROMPT = "What time is it?"


def get_current_time(inp: dict) -> str:
    """Return the current UTC time. Honors an optional ``{"unix": true}`` input
    so the example shows a handler reading its arguments."""
    now = datetime.now(timezone.utc)
    if inp.get("unix"):
        return str(int(now.timestamp()))
    return now.isoformat()


def build_hooks() -> tuple[Hooks, dict[str, int]]:
    """Wire up all four hook points; each just logs to stderr and bumps a
    counter so we can assert on exit that every point fired."""
    counts = {"before_send": 0, "after_receive": 0, "before_tool_call": 0, "after_tool_call": 0}

    def before_send(message: Message) -> None:
        counts["before_send"] += 1
        print(f"[hook before_send] sending {message.role} turn", file=sys.stderr)

    def after_receive(message: Message) -> None:
        counts["after_receive"] += 1
        text = message.text() or "(tool call)"
        print(f"[hook after_receive] got: {text[:60]}", file=sys.stderr)

    def before_tool_call(tool_use: ToolUseBlock) -> None:
        counts["before_tool_call"] += 1
        print(f"[hook before_tool_call] {tool_use.name}({tool_use.input})", file=sys.stderr)

    def after_tool_call(result: ToolResultBlock) -> None:
        counts["after_tool_call"] += 1
        print(f"[hook after_tool_call] {result.tool_use_id} -> {result.content}", file=sys.stderr)

    return (
        Hooks(
            before_send=before_send,
            after_receive=after_receive,
            before_tool_call=before_tool_call,
            after_tool_call=after_tool_call,
        ),
        counts,
    )


async def run() -> int:
    server_url = os.environ.get("BAE_SERVER_URL", "http://localhost:8080")
    client_key = os.environ.get("BAE_CLIENT_KEY")
    provider_key_env = os.environ.get("BAE_PROVIDER_KEY_ENV", "ANTHROPIC_API_KEY")
    prompt = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_PROMPT

    if not client_key:
        print(
            "error: BAE_CLIENT_KEY is not set — create a client key on the admin "
            "port (POST /admin/v1/keys) and export it.",
            file=sys.stderr,
        )
        return 1

    # The provider key is used server-side, but fail fast with a clear message
    # if the developer's environment is missing it (per the agent spec).
    if not os.environ.get(provider_key_env):
        print(
            f"error: {provider_key_env} is not set. The profile's provider config "
            f"references it and the server resolves it at call time; set it before "
            f"running, e.g. export {provider_key_env}=sk-…",
            file=sys.stderr,
        )
        return 1

    config = Config(
        server_url=server_url, client_key=client_key, client_version="ref-assistant/0.1"
    )
    hooks, counts = build_hooks()
    time_tool = Tool(
        name="get_current_time",
        description="Return the current time as an ISO-8601 UTC string.",
        input_schema={
            "type": "object",
            "properties": {"unix": {"type": "boolean", "description": "return a unix timestamp"}},
        },
        handler=get_current_time,
    )

    harness = Harness(config, tools=[time_tool], hooks=hooks)

    # A correlation tag using the secrets-backed RNG (never `random`).
    print(f"[run {random_hex(4)}] connecting to {server_url}", file=sys.stderr)

    try:
        session = await harness.connect()
    except ApiError as exc:
        print(f"error: could not open session ({exc.type}): {exc}", file=sys.stderr)
        return 1
    except TransportError as exc:
        print(f"error: could not reach the server at {server_url}: {exc}", file=sys.stderr)
        return 1

    print(f"[session {session.session_id}] profile '{session.profile.name}'", file=sys.stderr)

    try:
        async with session:
            reply = await session.send(prompt)
            # The final assistant text goes to stdout; everything else is stderr.
            print(reply.text())
            for event in session.last_events:
                print(f"[event] {describe_event(event)}", file=sys.stderr)
    except ProvidersFailedError as exc:
        print(
            "error: the server could not reach any provider (HTTP 502). The most "
            f"likely cause is that {provider_key_env} is unset or invalid on the "
            "server. Provider events:",
            file=sys.stderr,
        )
        for event in exc.events:
            print(f"  [event] {describe_event(event)}", file=sys.stderr)
        return 1
    except ApiError as exc:
        print(f"error: request failed ({exc.type}): {exc}", file=sys.stderr)
        return 1

    fired = [name for name, n in counts.items() if n > 0]
    print(f"[hooks fired] {', '.join(fired)} (counts: {counts})", file=sys.stderr)
    return 0


def main() -> None:
    raise SystemExit(asyncio.run(run()))


if __name__ == "__main__":
    main()
