# reference-assistant (TypeScript)

The canonical BAE example agent, implemented once per client SDK with identical
behavior across Rust, TypeScript, and Python. It doubles as the parity check
between the three harnesses (see `aspec/genai/agents.md`).

It registers a single client-side tool, `get_current_time`, opens a session,
drives the tool-call loop, prints the assistant's final text to **stdout**, and
logs every hook invocation to **stderr**.

## Prerequisites

1. A running BAE server (see `docs/quickstart.md`).
2. A profile whose `allowed_tools` includes `get_current_time`, and whose
   provider config references your provider key (e.g. `${ANTHROPIC_API_KEY}`).
3. A client key for that profile (`POST /admin/v1/keys`).

## Run

```sh
cd client-typescript
npm install

export BAE_CLIENT_KEY=bae_...        # required
export ANTHROPIC_API_KEY=sk-ant-...  # the provider key your profile references
# optional:
export BAE_SERVER_URL=http://localhost:8080     # default
export BAE_PROVIDER_KEY_ENV=ANTHROPIC_API_KEY   # default

npm run example -- "What time is it?"
```

## Behavior / failure modes

- Exits `1` with a clear message if `BAE_CLIENT_KEY` or the provider key env var
  (named by `BAE_PROVIDER_KEY_ENV`) is unset.
- If the server cannot reach any provider it returns `502`; the example catches
  `ProvidersFailedError`, prints the session events, and exits `1` — the usual
  cause is a missing/invalid provider key in the _server's_ environment.

Every customization point is exercised: `before_send`, `after_receive`,
`before_tool_call`, and `after_tool_call` all fire and log a `[hook …]` line.
