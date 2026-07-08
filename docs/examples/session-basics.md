# Session Basics

Session open → send a message → close. Shown in raw curl, then in the
TypeScript SDK.

---

## Prerequisites

- BAE server running on `http://localhost:8080`.
- A client key (from `POST /admin/v1/keys`). Set it:
  ```sh
  CLIENT_KEY="bae_…"
  ```

---

## curl

### Open a session

```sh
SESSION=$(curl -s -X POST http://localhost:8080/api/v1/sessions \
  -H "Authorization: Bearer $CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "client_version": "1.0.0",
    "tools": []
  }')

SESSION_ID=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_id'])")
SESSION_KEY=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['session_key'])")
echo "session: $SESSION_ID"
```

### Register as a driver

Required once per session key, before its first `session.sendMessage` (SDKs
do this automatically inside `connect()`):

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"session.registerDriver","params":{}}'
# {"jsonrpc":"2.0","id":1,"result":{"registered":true}}
```

### Send a message (`POST /rpc` with JSON-RPC)

```sh
curl -s -N -X POST "http://localhost:8080/api/v1/sessions/$SESSION_ID/rpc" \
  -H "Authorization: Bearer $SESSION_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":2,"method":"session.sendMessage","params":{"message":{"role":"user","content":"Say hello."}}}' \
| while IFS= read -r line; do
    echo "$line"
  done
```

Each line is a JSON object. Objects without `"id"` are live event
notifications; the last line (carrying `"id":2`) is the terminal result.

Extract the assistant text from the terminal result:

```sh
... | tail -1 | python3 -c "
import sys, json
r = json.load(sys.stdin)
for block in r['result']['message']['content']:
    if block.get('type') == 'text':
        print(block['text'])
"
```

### Close the session

```sh
curl -s -X DELETE "http://localhost:8080/api/v1/sessions/$SESSION_ID" \
  -H "Authorization: Bearer $SESSION_KEY"
# {"session_id":"ses_…","state":"closed"}
```

---

## TypeScript SDK

```typescript
import { Config, Harness } from "@prettysmartdev/bae-ts";

const harness = new Harness(
  new Config({
    serverUrl: process.env.BAE_SERVER_URL ?? "http://localhost:8080",
    clientKey: process.env.BAE_CLIENT_KEY!,
  }),
);

const session = await harness.connect();
console.error(`session ${session.id}`);

const reply = await session.send("Say hello.");
const text = reply.content
  .filter((b) => b.type === "text")
  .map((b) => b.text)
  .join("");
console.log(text);

await session.close();
```

Run:
```sh
BAE_CLIENT_KEY=bae_… npm run example
```

---

## What you see

`session.send` streams notifications internally. Once it resolves, `reply` is
the final `{role, content}` object. The SDK fires `on_event` for each
notification if you set that hook — see
[Building a Client](../guides/building-a-client.md) for the hook API.
