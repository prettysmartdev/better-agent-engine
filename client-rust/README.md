# bae-rs (Rust)

Rust client library and customizable **agent harness** for the
[Better Agent Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this crate gives
Rust programs an idiomatic way to drive it. Feature-parity is maintained with
the [TypeScript](../client-typescript/) and [Python](../client-python/)
clients.

It is a harness, not a bare REST wrapper: you register tools and hooks, open a
session, and `send()` drives the full model ↔ tool round-trip for you.

## Surface

1. **`Config`** — server URL, client key, client version.
2. **`Tool`** — name, description, JSON input schema, and a callable handler.
3. **`Harness`** — holds the config + tool registry + hooks; async `connect()`
   opens a session and returns a `Session`.
4. **`Session`** — `send(message)` drives the round-trip until a final
   (no-`tool_use`) assistant turn arrives; `close()` ends the session.
5. **`Hooks`** — optional `before_send` / `after_receive` / `before_tool_call`
   / `after_tool_call` callbacks. Each gets `&mut` access to its event and may
   mutate or log it; returning `Err` aborts the loop.

```rust
use bae_rs::{Config, Harness, Tool};
use serde_json::json;

# async fn run() -> Result<(), bae_rs::Error> {
let config = Config::new("http://localhost:8080", std::env::var("BAE_CLIENT_KEY")?);

let get_time = Tool::new(
    "get_current_time",
    "Return the current time as an ISO-8601 string",
    json!({ "type": "object", "properties": {} }),
    |_input| Ok(json!("2026-07-06T12:00:00Z")),
);

let mut session = Harness::new(config).with_tool(get_time).connect().await?;
let reply = session.send("What time is it?").await?;
println!("{}", reply.text());
session.close().await?;
# Ok(()) }
```

## Example

`examples/reference-assistant/` implements the canonical `reference-assistant`
agent (see [`aspec/genai/agents.md`](../aspec/genai/agents.md)): it registers
`get_current_time`, opens a session, drives a message loop, prints the reply,
and exercises every hook point. It fails fast with a clear message if the
provider key env var (default `ANTHROPIC_API_KEY`) is unset.

```sh
export BAE_CLIENT_KEY=bae_…          # from POST /admin/v1/keys
export ANTHROPIC_API_KEY=sk-…        # the provider key the profile references
cargo run --example reference-assistant -- "What time is it?"
```

## Develop

From the repo root (in Docker): `make test-client-rust`.

Directly in this directory:

```sh
make build   # cargo build
make test    # cargo test (unit tests run fully offline — no API keys)
make lint    # clippy -D warnings + fmt --check
```

## Publish

Released independently to crates.io as `bae-rs` (see
[`aspec/devops/cicd.md`](../aspec/devops/cicd.md)). `Cargo.toml` has
`publish = false` until the first release is cut.
