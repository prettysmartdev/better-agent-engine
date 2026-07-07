# Building a Client

BAE ships three client SDKs — Rust, TypeScript, and Python — each with an
identical reference example in its `examples/reference-assistant/` directory.
This guide walks through the harness API, the JSON-RPC transport, and the
`on_event` hook.

---

## The harness API

All three SDKs expose the same conceptual surface:

1. **Configure** — server URL, client key, optional client version.
2. **Register tools** — client-side tools the agent can call.
3. **Set hooks** — optional callbacks at each stage of the loop.
4. **Connect** — open a session (`POST /api/v1/sessions`); returns a `Session`
   object.
5. **Send** — `session.send(message)` runs the full JSON-RPC turn loop until a
   final text response, dispatching tool calls as needed.
6. **Subscribe** — `session.subscribe(callback, since_event_id?)` opens an
   observer stream on the session.
7. **Close** — `session.close()` (or `DELETE /api/v1/sessions/{id}`).

---

## Rust (`client-rust/`)

Source: [`client-rust/examples/reference-assistant/main.rs`](../../client-rust/examples/reference-assistant/main.rs)

```rust
use bae_rs::{Config, HarnessBuilder, HookResult};

let config = Config::builder()
    .server_url("http://localhost:8080")
    .client_key("bae_…")
    .client_version("my-agent/1.0")
    .build()?;

let harness = HarnessBuilder::new(config)
    .register_tool(Tool {
        name: "get_current_time".into(),
        description: "Return the current UTC time.".into(),
        input_schema: serde_json::json!({"type":"object","properties":{}}),
        handler: Box::new(|_input| Ok("2026-07-07T00:00:00Z".into())),
    })
    .before_send(|msg| { eprintln!("→ {}", msg.role); HookResult::Continue })
    .after_receive(|msg| { eprintln!("← {} blocks", msg.content.len()); HookResult::Continue })
    .on_event(|event| {
        // Fires for each session.event notification on the /rpc stream.
        eprintln!("[event] {}", event.event_type);
        HookResult::Continue
    })
    .build();

let mut session = harness.connect().await?;

let reply = session.send("What time is it?").await?;
println!("{}", reply.text().unwrap_or_default());

session.close().await?;
```

### Tool dispatch loop

`session.send` handles the full loop internally:
1. POST `session.sendMessage` to `/rpc`.
2. Read NDJSON notifications, fire `on_event` for each.
3. On terminal result: if `message.content` has `tool_use` blocks, dispatch
   each to the registered handler, fire `before_tool_call`/`after_tool_call`,
   and POST `session.sendMessage` with the `tool_result` blocks.
4. Repeat until a final text response.

### `subscribe` / `unsubscribe`

```rust
// Observer connection — separate from the driving session.send call.
session.subscribe(Some("evt_last-seen"), |event| {
    eprintln!("[observe] {}", event.event_type);
    true // return false to stop
}).await?;

session.unsubscribe().await?;
```

---

## TypeScript (`client-typescript/`)

Source: [`client-typescript/examples/reference-assistant/main.ts`](../../client-typescript/examples/reference-assistant/main.ts)

```typescript
import { Config, Harness, describeEvent } from "@prettysmartdev/bae-ts";

const harness = new Harness(
  new Config({ serverUrl: "http://localhost:8080", clientKey: "bae_…" }),
);

harness.registerTool({
  name: "get_current_time",
  description: "Return the current UTC time.",
  input_schema: { type: "object", properties: {} },
  handler: (_input) => new Date().toISOString(),
});

harness.setHooks({
  before_send:      (msg) => console.error(`→ ${msg.role}`),
  after_receive:    (msg) => console.error(`← received`),
  before_tool_call: (tu)  => console.error(`tool: ${tu.name}`),
  after_tool_call:  (tr)  => console.error(`result: ${JSON.stringify(tr.content)}`),
  on_event:         (ev)  => console.error(`[event] ${describeEvent(ev)}`),
});

const session = await harness.connect();

const reply = await session.send("What time is it?");
console.log(reply.content[0]?.type === "text" ? reply.content[0].text : "");

// Optional observer subscription
await session.subscribe(
  (event) => {
    console.error(`[observe] ${describeEvent(event)}`);
    return true; // return false to stop
  },
  { sinceEventId: "evt_…" },
);

await session.close();
```

### Running the reference example

```sh
cd client-typescript
npm install
BAE_CLIENT_KEY=bae_… npm run example -- "What time is it?"
```

---

## Python (`client-python/`)

Source: [`client-python/examples/reference-assistant/main.py`](../../client-python/examples/reference-assistant/main.py)

```python
from bae_py import Config, Harness, Hooks, Tool, describe_event

config = Config(
    server_url="http://localhost:8080",
    client_key="bae_…",
    client_version="my-agent/1.0",
)

def get_current_time(inp: dict) -> str:
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).isoformat()

def on_event(event) -> None:
    print(f"[event] {describe_event(event)}", file=sys.stderr)

hooks = Hooks(on_event=on_event)
time_tool = Tool(
    name="get_current_time",
    description="Return the current UTC time.",
    input_schema={"type": "object", "properties": {}},
    handler=get_current_time,
)

harness = Harness(config, tools=[time_tool], hooks=hooks)
session = await harness.connect()

reply = await session.send("What time is it?")
print(reply.text())

# Optional observer subscription (async handler supported)
async def observe(event) -> bool:
    print(f"[observe] {describe_event(event)}", file=sys.stderr)
    return True  # return False to stop

await session.subscribe(observe, since_event_id="evt_…")

await session.close()
```

### Running the reference example

```sh
cd client-python
BAE_CLIENT_KEY=bae_… uv run python examples/reference-assistant/main.py "What time is it?"
```

---

## Hook points

All three SDKs expose the same five hook points:

| Hook | Fires | SDK signature |
|---|---|---|
| `before_send` | Before each turn is POSTed | `(message) -> void/HookResult` |
| `after_receive` | After the terminal response is received | `(message) -> void/HookResult` |
| `before_tool_call` | Before each client-side tool is dispatched | `(tool_use_block) -> void/HookResult` |
| `after_tool_call` | After each client-side tool result is built | `(tool_result_block) -> void/HookResult` |
| `on_event` | For each `session.event` notification on the `/rpc` stream | `(event) -> void/HookResult` |

`on_event` fires for all forwarded events (not the client-generated
`client.message.send` or client `tool.result` — those are filtered by the
server). In Rust, returning `HookResult::Abort` from any hook stops the loop;
in TS/Python, throwing an error does the same.

---

## Error types

| Scenario | SDK error type |
|---|---|
| Non-2xx HTTP response (auth, 404, etc.) | `ApiError` (all SDKs) |
| JSON-RPC error object in stream | `RpcError` (all SDKs) |
| Session not open (sent to closed session) | `RpcError(-32000)` — raised on `session.send` |
| All providers failed | `ProvidersFailedError` — detected from `session.error` event in `result.events` |
| Network / transport failure | `TransportError` (all SDKs) |

---

## JSON-RPC transport overview

`session.send` and `session.subscribe` both POST to
`/api/v1/sessions/{id}/rpc` with a JSON-RPC 2.0 request body. The response is
`application/x-ndjson` — one JSON object per line. Objects without `"id"` are
`session.event` notifications; the object carrying the request `"id"` is the
terminal response.

The SDK handles all NDJSON framing internally. You never interact with raw
JSON-RPC objects unless you are building a custom transport. See
[Wire Protocol](../reference/wire-protocol.md) for the full specification.
