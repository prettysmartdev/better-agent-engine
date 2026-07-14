# Multi-Client Sessions

BAE sessions support multiple **drivers** — independent client keys that each
send messages into the *same* session — plus any number of **observers**.
This guide walks through the full multiplayer flow end to end: open a session
as driver A, join it as driver B (a different client key, same profile), have
both send messages, and watch the server serialize their turns FIFO while
every participant sees every other participant's activity.

For the conceptual model see [Wire Protocol — FIFO turn ownership](../reference/wire-protocol.md#fifo-turn-ownership-and-driver-registration).
For the endpoint/method reference see [Client API](../reference/client-api.md).

---

## Prerequisites

- A running BAE server (see [Quickstart](quickstart.md)) configured with a
  `[providers]` entry — e.g.
  [`examples/bae-config/providers.toml`](../../examples/bae-config/providers.toml):

  ```sh
  BAE_CONFIG=examples/bae-config/providers.toml baesrv
  ```
- `curl` and `python3` for the examples below.

---

## Step 1 — Create a profile

```sh
PROFILE=$(curl -s -X POST http://127.0.0.1:8081/admin/v1/profiles \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "pair-session",
    "primary_provider": "anthropic-sonnet",
    "allowed_tools": []
  }')
PROFILE_ID=$(echo "$PROFILE" | python3 -c "import sys,json; print(json.load(sys.stdin)['id'])")
echo "profile: $PROFILE_ID"
```

`primary_provider` must name an entry in the server's `[providers]` registry
(here, `anthropic-sonnet` from `providers.toml`) — see
[Profiles — Provider config](../profiles.md#provider-config).

---

## Step 2 — Issue two client keys against the *same* profile

Multi-driver sessions require two independent client keys sharing one
profile — this is the "client → profile → session" mapping the feature is
built on: a client key using a *different* profile can never attach to
another client's session.

```sh
KEY_A=$(curl -s -X POST http://127.0.0.1:8081/admin/v1/keys \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"driver-a\",\"profile_id\":\"$PROFILE_ID\"}")
CLIENT_KEY_A=$(echo "$KEY_A" | python3 -c "import sys,json; print(json.load(sys.stdin)['key'])")

KEY_B=$(curl -s -X POST http://127.0.0.1:8081/admin/v1/keys \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"driver-b\",\"profile_id\":\"$PROFILE_ID\"}")
CLIENT_KEY_B=$(echo "$KEY_B" | python3 -c "import sys,json; print(json.load(sys.stdin)['key'])")
```

---

## Step 3 — Open the session as driver A

```sh
SESSION=$(curl -s -X POST http://localhost:8080/api/v1/sessions \
  -H "Authorization: Bearer $CLIENT_KEY_A" \
  -H 'Content-Type: application/json' \
  -d '{"client_version": "1.0.0", "tools": []}')

SESSION_ID=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_id'])")
SESSION_KEY_A=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_key'])")
echo "session: $SESSION_ID"
```

This inserts a `session.open` event attributed to A's client key.

---

## Step 4 — Register A as a driver

Registration is required before `session.sendMessage` — the server never
auto-registers a caller. (SDK harnesses call this automatically inside
`connect()`/`join()`; the raw wire call is shown here for clarity.)

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY_A" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.registerDriver","params":{}}'
# {"jsonrpc":"2.0","id":1,"result":{"registered":true}}
```

This inserts a `session.driver.register` event (attributed to A) and is
idempotent — a repeat call also returns `registered: true` without logging a
duplicate.

---

## Step 5 — Join as driver B

Using B's **own client key** (not A's session key), join the session A
already opened:

```sh
JOIN=$(curl -s -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/join" \
  -H "Authorization: Bearer $CLIENT_KEY_B" \
  -H 'Content-Type: application/json' \
  -d '{"client_version": "1.0.0", "tools": []}')

SESSION_KEY_B=$(echo "$JOIN" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_key'])")
echo "$JOIN" | python3 -m json.tool
```

Response shape is identical to `create` — a **new** session key bound to B's
client key, minted for the *same* `session_id`. This inserts a `session.join`
event attributed to B. Register B as a driver too:

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY_B" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.registerDriver","params":{}}'
```

> **A different profile can never join.** If B's client key belonged to a
> different profile, this call would fail with `403 profile_mismatch` before
> touching the session at all — no event logged, no session key minted. See
> [Client API — `join`](../reference/client-api.md#post-apiv1sessionsidjoin--join-an-existing-session).

Confirm both are registered:

```sh
curl -s "http://localhost:8080/api/v1/sessions/$SESSION_ID/participants" \
  -H "Authorization: Bearer $SESSION_KEY_A"
# {"drivers":["key_…A","key_…B"]}
```

---

## Step 6 — Subscribe as an observer (optional third connection)

Any connection with a valid session key can watch everything happening in
the session — driver or not — via `session.subscribe`. Run this in a
separate terminal to see cross-visibility live as steps 7–8 run:

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY_A" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"session.subscribe","params":{}}'
```

`session.subscribe` is itself the observer registration act — no separate
call is needed, and unlike `session.registerDriver`, nothing is logged for
it. This connection will stream every event from **both** A's and B's turns
below, in order.

---

## Step 7 — Both drivers send messages

Send A's message first, then — without waiting for it to finish — send B's:

```sh
# Terminal 1 (driver A) — starts running immediately
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY_A" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"session.sendMessage","params":{"message":{"role":"user","content":"Say hello from A."}}}' &

# Terminal 2 (driver B) — queues behind A
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY_B" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":3,"method":"session.sendMessage","params":{"message":{"role":"user","content":"Say hello from B."}}}'

wait
```

---

## Step 8 — Observe FIFO ordering

B's `curl` call above **blocks silently** — zero bytes on the wire — until
A's turn reaches a terminal state (`Completed`, `ProvidersFailed`, or an
abandoned-turn timeout releases the gate). There is no interleaving: A's full
event sequence (`client.message.send` → `provider.request` →
`provider.response` → `server.message.send`) always completes before B's
first event appears, regardless of which `curl` the OS scheduled first.

The observer connection from step 6 sees **all** of it, live, attributed to
the correct client key:

```
[event] client.message.send   (client_key_id: key_…A)
[event] provider.request
[event] provider.response
[event] server.message.send   (client_key_id: key_…A)
[event] client.message.send   (client_key_id: key_…B)
[event] provider.request
[event] provider.response
[event] server.message.send   (client_key_id: key_…B)
```

Fetch the durable history to confirm — same sequence, plus the `session.open`
/ `session.join` / `session.driver.register` events from earlier steps:

```sh
curl -s "http://localhost:8080/api/v1/sessions/$SESSION_ID/events" \
  -H "Authorization: Bearer $SESSION_KEY_A" | python3 -m json.tool
```

See [Message Types — Typical event sequences](../reference/message-types.md#typical-event-sequences)
for the annotated version of this exact sequence.

---

## Private tool sets

Each driver may declare its own client-side tools at `create`/`join` time —
these are **never** merged or shared. If A declared `only_a` and B declared
`only_b`, the `provider.request` event logged during A's turn advertises
`only_a` (plus any session-wide MCP tools) and never `only_b`, and vice versa
for B's turn — enforced by construction, since each turn only ever reads the
*acting* driver's own declared tool list. See
[Wire Protocol — Per-turn tool scoping](../reference/wire-protocol.md#per-turn-tool-scoping-and-event-attribution).

---

## What happens if a driver disconnects mid-turn

If A's message triggers a client-side tool call (`tool_use` in the terminal
result) and A never sends the `tool_result` continuation, the turn stays
parked under A's ownership until `BAE_TURN_TIMEOUT` (default 120s) elapses.
At that point the next arrival — B's queued message, or a retry from A —
triggers abandonment: a `session.error` (`reason: "driver_turn_abandoned"`)
is logged, the server merges parked server results with synthetic error
results for unanswered client calls, and the session stays `open` with a valid
provider transcript. See
[Wire Protocol — "Remaining connected"](../reference/wire-protocol.md#remaining-connected-is-a-return-before-timeout-guarantee-not-a-held-socket).

---

## Closing the session

Any participant's session key can close the session for everyone:

```sh
curl -s -X DELETE "http://localhost:8080/api/v1/sessions/$SESSION_ID" \
  -H "Authorization: Bearer $SESSION_KEY_A"
```

This tears down the shared FIFO gate, driver registry, and MCP connections
for the whole session — both A's and B's session keys stop authenticating.

Revoking a single participant's **client key**, by contrast, is scoped to
that participant: `DELETE /admin/v1/keys/{id}` only force-closes the session
once the revoked key was the *last* active session key on it — revoking B
while A is still active leaves the session (and A) untouched.

---

## Using the SDK

Per the work item, `Harness.connect()` and the new `Harness.join(sessionId)`
both call `session.registerDriver` internally as part of session setup —
application code never has to call it directly. See
[Building a Client — the harness API](building-a-client.md#the-harness-api)
for the full per-language surface.
