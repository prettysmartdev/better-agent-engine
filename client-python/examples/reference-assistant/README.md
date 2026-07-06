# reference-assistant (Python)

The canonical BAE example agent, implemented with `bae-py`. It mirrors the
Rust and TypeScript `reference-assistant` examples exactly — the three are the
parity check between the harnesses.

What it does:

1. Registers one client-side tool, `get_current_time`.
2. Opens a session against a profile whose `allowed_tools` includes
   `get_current_time`.
3. Sends a user turn and drives the harness loop: when the model calls the
   tool, the handler runs and the result is sent back, repeating until a plain
   text answer arrives.
4. Prints the final assistant text to **stdout**; hook and event logs go to
   **stderr**.
5. Exercises all four hook points (`before_send`, `after_receive`,
   `before_tool_call`, `after_tool_call`) — a counter confirms each fired.

## Prerequisites

A running BAE server, a profile that allows `get_current_time`, and a client
key for that profile. See [`docs/quickstart.md`](../../../docs/quickstart.md)
for the admin-side setup (create a profile, create a key).

## Configuration (environment)

| Variable               | Default                  | Meaning                              |
|------------------------|--------------------------|--------------------------------------|
| `BAE_SERVER_URL`       | `http://localhost:8080`  | Server base URL (client port).       |
| `BAE_CLIENT_KEY`       | *(required)*             | The `bae_…` client key.              |
| `BAE_PROVIDER_KEY_ENV` | `ANTHROPIC_API_KEY`      | Name of the provider-key env var the profile references. |

The provider key itself is used **server-side** and never sent by the SDK, but
the example fails fast with a clear message if it is unset locally, and reports
the server's `502 providers-failed` outcome (likely an unset/invalid key on the
server) if it happens at runtime.

## Run

```sh
export BAE_CLIENT_KEY=bae_...          # from POST /admin/v1/keys
export ANTHROPIC_API_KEY=sk-...        # the profile's provider key
uv run python examples/reference-assistant/main.py "What time is it?"
```

The prompt is optional (defaults to `"What time is it?"`).
