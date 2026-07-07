# bae Documentation

**bae** (Better Agent Engine) is a stateful HTTP server for hosting AI agent sessions. A client harness opens a session, sends messages in a loop, dispatches tool calls, and closes the session when done.

---

## Guides

Start here. Guides walk you through real tasks end to end.

- [Quickstart](guides/quickstart.md) — run the server, create a profile and key, open a session, and send your first message.
- [MCP Servers](guides/mcp-servers.md) — connect a real MCP server (filesystem, fetch) to a profile using `bae-config.toml`.
- [Event Streaming](guides/event-streaming.md) — consume live `session.event` notifications as a turn runs, subscribe as an observer, and resume after a disconnect.
- [Building a Client](guides/building-a-client.md) — short per-SDK walkthrough (Rust, TypeScript, Python) covering the harness API, the JSON-RPC transport, and the `on_event` hook.

---

## Reference

Precise specification of every API surface and configuration option.

- [Client API](reference/client-api.md) — REST session management plus `POST /api/v1/sessions/{id}/rpc` JSON-RPC methods.
- [Wire Protocol](reference/wire-protocol.md) — JSON-RPC 2.0 envelope conventions, NDJSON framing, and error codes.
- [Admin API](reference/admin-api.md) — profile and key management, `GET /admin/v1/mcp-servers`.
- [Message Types](reference/message-types.md) — all twelve `event_type` values and their payload shapes.
- [Configuration](reference/configuration.md) — every `BAE_*` env var, the `--config` flag, and the `bae-config.toml` schema.

---

## Examples

Short end-to-end walkthroughs with raw curl and SDK snippets.

- [Session Basics](examples/session-basics.md) — session open → send-message → close, with curl and one SDK.
- [MCP-Attached Profile](examples/mcp-profile.md) — create a profile that uses a configured MCP server, open a session, trigger the tool.
- [Live Events](examples/live-events.md) — read event notifications from `session.sendMessage` and subscribe as an independent observer.

---

## Also see

- [`profiles.md`](profiles.md) — provider config, fallbacks, tool allowlists, MCP server wiring.
- [`examples/bae-config/`](../examples/bae-config/) — ready-to-run `bae-config.toml` files for common MCP servers.
