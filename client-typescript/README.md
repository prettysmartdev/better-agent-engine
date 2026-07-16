# @prettysmartdev/bae-ts (TypeScript)

TypeScript client library and customizable **agent harness** for the
[Better Agent Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this package gives
Node.js/TypeScript programs an idiomatic way to drive it. Feature-parity is
maintained with the [Rust](../client-rust/) and [Python](../client-python/)
clients.

Requires Node.js ≥ 20.

## Usage

The SDK is an **agent harness**, not a REST wrapper. Register client-side tools,
open a session, and let `send()` drive the whole round-trip — dispatching
server-returned tool calls to your handlers and posting results back until a
final assistant turn arrives.

```ts
import { Config, Harness, messageText } from "@prettysmartdev/bae-ts";

const harness = new Harness(
  new Config({
    serverUrl: "http://localhost:8080",
    clientKey: process.env.BAE_CLIENT_KEY!,
  }),
);

harness.registerTool({
  name: "get_current_time",
  description: "Return the current time as an ISO-8601 UTC string.",
  input_schema: { type: "object", properties: {} },
  handler: () => new Date().toISOString(),
});

// Optional customization points; throwing from any hook aborts the loop.
harness.setHooks({
  before_send: (m) => console.error("→", m.role),
  after_receive: (m) => console.error("←", messageText(m)),
  before_tool_call: (tu) => console.error("tool", tu.name),
  after_tool_call: (tr) => console.error("result", tr.name),
});

const session = await harness.connect();
const reply = await session.send("What time is it?");
console.log(messageText(reply));
await session.close();
```

## Surface

- **`Config`** — `serverUrl`, `clientKey`, optional `clientVersion`.
- **`ToolDefinition`** — `{ name, description, input_schema, handler }`; the
  handler receives the `tool_use.input` and returns the result content.
- **`Harness`** — register tools and hooks, then `connect()` opens a new session
  or `join(sessionId)` attaches to an existing one as a second driver; both
  return a `Session`.
- **`Session`** — `send(message)` drives the loop to a final assistant turn,
  `close()` ends the session, and `subscribe()` / `unsubscribe()` tap the live
  event stream out of band.
- **`Hooks`** — `before_send`, `after_receive`, `before_tool_call`,
  `after_tool_call`, and `on_event` (the live event stream); throwing from any
  hook aborts the loop.
- **Built-in tools** (opt-in) — `read_file` / `write_file` / `explore_files`
  (scoped file access), `run_shell_command` / `run_shell_named` (local or
  server-side sandboxes), and `launch_subagent` (delegate to a CLI); attach them
  with `registerSandboxTool()` / `registerSubagentTool()`.
- **Errors** — `ApiError` (RFC 7807 slug in `.type`), `ProvidersFailedError`
  (a `502`, carrying the session `events`), `RpcError`, `UnknownToolError`,
  `ToolError`, `HookError`, `TransportError`.
- **Events** — `SessionEvent` is a discriminated union over all 27 event types;
  `describeEvent()` demonstrates the exhaustive match.

## Example

A runnable `reference-assistant` agent lives in
[`examples/reference-assistant/`](./examples/reference-assistant/): it registers
`get_current_time`, opens a session, drives the loop, prints the reply, and
exercises every hook point. Run it with:

```sh
npm run example -- "What time is it?"
```

## Develop

From the repo root (in Docker): `make test-client-typescript`.

Directly in this directory:

```sh
make build   # npm install + tsc
make test    # vitest
make lint    # tsc --noEmit + prettier --check
```

## Publish

Released independently to npm as `@prettysmartdev/bae-ts` (see
[`aspec/devops/cicd.md`](../aspec/devops/cicd.md)). `package.json` is marked
`"private": true` until the first release is cut.
