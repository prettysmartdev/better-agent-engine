# bae-py (Python)

Python client library and customizable **agent harness** for the
[Better Agent Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this package gives
Python programs an idiomatic way to drive it. Feature-parity is maintained
with the [Rust](../client-rust/) and [TypeScript](../client-typescript/)
clients.

Requires Python ≥ 3.10. Managed with [uv](https://docs.astral.sh/uv/).

## Usage

The SDK is an **agent harness**, not a REST wrapper. Register client-side tools,
open a session, and let `send()` drive the whole round-trip — dispatching
server-returned tool calls to your handlers and posting results back until a
final assistant turn arrives. Handlers and hooks may be sync or async.

```python
import asyncio
from bae_py import Config, Harness, Hooks, Tool

def get_current_time(inp: dict) -> str:
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).isoformat()

async def main():
    config = Config(server_url="http://localhost:8080", client_key="bae_…")
    tool = Tool(
        name="get_current_time",
        description="Return the current time",
        input_schema={"type": "object", "properties": {}},
        handler=get_current_time,
    )
    # Optional customization points; raising from any hook aborts the loop.
    harness = Harness(config, tools=[tool], hooks=Hooks(
        before_send=lambda m: None,        # also: after_receive,
        before_tool_call=lambda t: None,   #        after_tool_call, on_event
    ))
    async with await harness.connect() as session:
        reply = await session.send("What time is it?")
        print(reply.text())

asyncio.run(main())
```

The public surface is re-exported from the top-level `bae_py` package; the
harness machinery lives in `bae_py.harness`.

## Surface

- **`Config`** — `server_url`, `client_key`, optional `client_version`.
- **`Tool`** — `name`, `description`, `input_schema`, `handler`; the handler
  receives the `tool_use` input and returns the result content.
- **`Harness`** — register tools and hooks, then `connect()` opens a new session
  or `join(session_id)` attaches to an existing one as a second driver; both
  return a `Session`.
- **`Session`** — `send(message)` drives the loop to a final assistant turn,
  `close()` ends the session, and `subscribe()` / `unsubscribe()` tap the live
  event stream out of band.
- **`Hooks`** — `before_send`, `after_receive`, `before_tool_call`,
  `after_tool_call`, and `on_event` (the live event stream); raising from any
  hook aborts the loop.
- **Built-in tools** (opt-in) — `read_file` / `write_file` / `explore_files`
  (scoped file access), `run_shell_command` / `run_shell_named` (local or
  server-side sandboxes), and `launch_subagent` (delegate to a CLI); attach them
  with `register_sandbox_tool()` / `register_subagent_tool()`.
- **Errors** — `ApiError` (RFC 7807 slug), `ProvidersFailedError` (a `502`,
  carrying the session `events`), `RpcError`, `UnknownToolError`, `ToolError`,
  `HookError`, `TransportError`.
- **Events** — `SessionEvent` is a discriminated union over all 27 event types;
  `describe_event()` demonstrates the exhaustive match.

## Example

A runnable `reference-assistant` agent lives in
[`examples/reference-assistant/`](examples/reference-assistant/): it registers
`get_current_time`, opens a session, drives the loop, prints the reply, and
exercises every hook point. Run it with:

```sh
uv run python examples/reference-assistant/main.py "What time is it?"
```

## Develop

From the repo root (in Docker): `make test-client-python`.

Directly in this directory:

```sh
make install   # uv sync
make test      # pytest
make lint      # ruff check + format check
make build     # sdist + wheel into dist/
```

## Publish

Released independently to PyPI as `bae-py` (see
[`aspec/devops/cicd.md`](../aspec/devops/cicd.md)). The
`Private :: Do Not Upload` classifier stays in `pyproject.toml` until the
first release is cut.
