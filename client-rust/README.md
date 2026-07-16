# bae-rs (Rust)

Rust client library and customizable **agent harness** for the
[Better Agent Engine](../README.md). Thin and stateless by design:
durable agent state lives on the [server](../server/), and this crate gives
Rust programs an idiomatic way to drive it. Feature-parity is maintained with
the [TypeScript](../client-typescript/) and [Python](../client-python/)
clients.

Requires a stable Rust toolchain (2021 edition).

## Usage

The SDK is an **agent harness**, not a REST wrapper. Register client-side tools,
open a session, and let `send()` drive the whole round-trip — dispatching
server-returned tool calls to your handlers and posting results back until a
final assistant turn arrives.

```rust
use bae_rs::{Config, Harness, Tool};
use serde_json::json;

# async fn run() -> Result<(), bae_rs::Error> {
let config = Config::new("http://localhost:8080", std::env::var("BAE_CLIENT_KEY")?);

let get_time = Tool::new(
    "get_current_time",
    "Return the current time as an ISO-8601 string",
    json!({ "type": "object", "properties": {} }),
    |_input| async move { Ok(json!("2026-07-06T12:00:00Z")) },
);

let mut session = Harness::new(config).with_tool(get_time).connect().await?;
let reply = session.send("What time is it?").await?;
println!("{}", reply.text());
session.close().await?;
# Ok(()) }
```

> **Alpha breaking change (work item 0006).** `ToolHandler` is now **async**:
> `Tool::new`'s handler closure returns a `Future` instead of a plain
> `Result`, and `Tool::call` is an `async fn`. Migrate an existing synchronous
> handler by wrapping its body in `async move { … }` — e.g.
> `|input| Ok(input)` becomes `|input| async move { Ok(input) }`. This lets a
> handler `.await` (an HTTP round-trip, a subprocess) without blocking the
> runtime, which the builtin sandbox tools require. Acceptable per the crate's
> alpha status, same posture as the WI 0003/0005 wire-protocol changes.

## Surface

- **`Config`** — server URL, client key, optional client version.
- **`Tool`** — name, description, JSON `input_schema`, and an **async** handler
  (see the breaking-change note above).
- **`Harness`** — register tools and hooks (builder-style `with_tool` /
  `with_hooks` or `register_tool`), then `connect()` opens a new session or
  `join(session_id)` attaches to an existing one as a second driver; both
  return a `Session`.
- **`Session`** — `send(message)` drives the loop until a final (no-`tool_use`)
  assistant turn, `close()` ends the session, and `subscribe()` /
  `unsubscribe()` tap the live event stream out of band.
- **`Hooks`** — `before_send`, `after_receive`, `before_tool_call`,
  `after_tool_call`, and `on_event` (the live event stream). Each gets `&mut`
  access to its value and may mutate or log it; returning `Err` aborts the loop.
- **Built-in tools** (opt-in) — `read_file` / `write_file` / `explore_files`
  (scoped file access), `run_shell_command` / `run_shell_named` (local or
  server-side sandboxes), and `launch_subagent` (delegate to a CLI); attach them
  with `register_sandbox_tool()` / `register_subagent_tool()`.
- **Errors** — one `Error` enum with variants `Api` (RFC 7807 slug),
  `ProvidersFailed` (a `502`, carrying the session `events`), plus RPC, tool,
  hook, and transport failures.
- **Events** — each live event arrives as an `EventView` whose `event_type` is
  one of the closed 27-value set; the `on_event` hook receives every one.

## Example

A runnable `reference-assistant` agent lives in
[`examples/reference-assistant/`](./examples/reference-assistant/): it registers
`get_current_time`, opens a session, drives the loop, prints the reply, and
exercises every hook point. Run it with:

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
