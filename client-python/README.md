# bae-py (Python)

Python client library and customizable agent harness for the
[Better Agent Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this package gives
Python programs an idiomatic way to drive it. Feature-parity is maintained
with the [Rust](../client-rust/) and [TypeScript](../client-typescript/)
clients.

Requires Python ≥ 3.10. Managed with [uv](https://docs.astral.sh/uv/).

## Usage

The SDK is an **agent harness**, not a REST wrapper. You register client-side
tools, open a session, and call `send()` — the harness drives the whole
tool-call round trip against the server and returns the final assistant turn.

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
    harness = Harness(config, tools=[tool], hooks=Hooks(
        before_send=lambda m: None,        # also: after_receive,
        before_tool_call=lambda t: None,   #        after_tool_call
    ))
    async with await harness.connect() as session:
        reply = await session.send("What time is it?")
        print(reply.text())

asyncio.run(main())
```

The public surface (`Config`, `Tool`, `Harness`, `Session`, `Hooks`, the
content/event model, and the security primitives) is re-exported from the
top-level `bae_py` package; the harness machinery lives in `bae_py.harness`.
Handlers and hooks may be sync or async. See
[`examples/reference-assistant/`](examples/reference-assistant/) for a complete,
runnable agent.

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
