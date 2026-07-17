//! Cross-component integration tests (work item 0007, Test Considerations).
//!
//! Unlike the other `*.test.ts` in this package — which unit-test each module
//! against fakes — these boot the **real** stack end to end and are fully
//! offline (loopback only, a mock provider, no real keys):
//!
//!   raw HTTP driver ─────────────▶ real baesrv (child process)
//!                                     ▲ admin port      ▲ client port
//!                                     │                 │
//!   browser WebSocket ─▶ real max/server ──────────────┘
//!                        (buildMaxServer, in-process)   via a recording proxy
//!
//! The client port is reached through an in-process **recording proxy** so we
//! can inspect every request MAX made and prove (not merely by absence of
//! errors) that MAX never issued `session.registerDriver`/`session.sendMessage`.
//! The driver talks to baesrv's client port *directly*, so the proxy log
//! contains only MAX's traffic.
//!
//! Covers:
//!  1. Integration — full observe flow (WS receives every event in order; MAX
//!     never registers a driver or sends a message).
//!  2. Integration — fan-out (two browser WS on one session → one `session.join`).
//!  3. Regression — profile deletion still 409 `profile_in_use` after MAX
//!     observes; MAX's key distinguishable by its `max-observer-` prefix.
//!  4. Regression — MAX auth gate on every REST route and the WS upgrade.
//!
//! If the `baesrv` binary is not built, this whole file is skipped (a note is
//! logged) rather than failing — see `/awman/context/workflow/test-plan.md`.

import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import http from "node:http";
import net from "node:net";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawn, type ChildProcess } from "node:child_process";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";
import { WebSocket } from "ws";

import { buildMaxServer, type MaxServerHandle } from "./index.js";

// ---------------------------------------------------------------------------
// Locate the baesrv binary (skip the suite if it isn't built).
// ---------------------------------------------------------------------------

const here = path.dirname(fileURLToPath(import.meta.url)); // max/server/src
const workspace = path.resolve(here, "..", "..", "..");
const BAESRV_BIN = [
  path.join(workspace, "server", "target", "debug", "baesrv"),
  path.join(workspace, "server", "target", "release", "baesrv"),
].find((p) => fs.existsSync(p));

if (!BAESRV_BIN) {
  console.warn(
    "[crossComponent.integration] baesrv binary not found under " +
      "server/target/{debug,release} — skipping the cross-component suite. " +
      "Build it first (`make -C server build` or `cargo test`).",
  );
}

const MAX_PASSWORD = "test-max-password-shhh";

// ---------------------------------------------------------------------------
// Small async helpers
// ---------------------------------------------------------------------------

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function waitFor(
  cond: () => boolean | Promise<boolean>,
  { timeout = 15000, interval = 25, label = "condition" } = {},
): Promise<void> {
  const start = Date.now();
  for (;;) {
    if (await cond()) return;
    if (Date.now() - start > timeout) {
      throw new Error(`waitFor timed out waiting for ${label}`);
    }
    await sleep(interval);
  }
}

/** Reserve an OS-assigned free TCP port (for the external baesrv process). */
function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address() as net.AddressInfo;
      srv.close(() => resolve(addr.port));
    });
  });
}

// ---------------------------------------------------------------------------
// A recording HTTP proxy in front of baesrv's client port.
// ---------------------------------------------------------------------------

interface ProxiedRequest {
  method: string;
  path: string;
  /** The JSON-RPC `method` field of the body, if any (`session.subscribe`, …). */
  rpcMethod?: string;
}

interface RecordingProxy {
  server: http.Server;
  port: number;
  requests: ProxiedRequest[];
}

/**
 * Forwards every request to `127.0.0.1:${targetPort}`, streaming responses back
 * verbatim (so the long-lived NDJSON `session.subscribe` stream flows), and
 * records the method/path/JSON-RPC-method of each request for later assertions.
 */
async function startRecordingProxy(
  targetPort: number,
): Promise<RecordingProxy> {
  const requests: ProxiedRequest[] = [];
  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c as Buffer));
    req.on("end", () => {
      const body = Buffer.concat(chunks);
      let rpcMethod: string | undefined;
      try {
        const parsed = JSON.parse(body.toString("utf8")) as {
          method?: string;
        };
        if (typeof parsed.method === "string") rpcMethod = parsed.method;
      } catch {
        // Not a JSON body (or empty) — no rpcMethod to record.
      }
      requests.push({
        method: req.method ?? "",
        path: req.url ?? "",
        ...(rpcMethod ? { rpcMethod } : {}),
      });

      const upstream = http.request(
        {
          host: "127.0.0.1",
          port: targetPort,
          method: req.method,
          path: req.url,
          headers: req.headers,
        },
        (up) => {
          res.writeHead(up.statusCode ?? 502, up.headers);
          up.pipe(res);
        },
      );
      upstream.on("error", () => {
        if (!res.headersSent) res.writeHead(502);
        res.end();
      });
      // If the downstream (MAX aborting a subscribe) hangs up, kill the upstream.
      res.on("close", () => upstream.destroy());
      upstream.end(body);
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as net.AddressInfo).port;
  return { server, port, requests };
}

// ---------------------------------------------------------------------------
// The whole real stack, booted once for the suite.
// ---------------------------------------------------------------------------

interface Stack {
  tmp: string;
  baesrv: ChildProcess;
  clientPort: number;
  adminPort: number;
  adminToken: string;
  mock: http.Server;
  proxy: RecordingProxy;
  max: MaxServerHandle;
  maxPort: number;
}

let stack: Stack | undefined;

const clientBase = () => `http://127.0.0.1:${stack!.clientPort}`;
const adminBase = () => `http://127.0.0.1:${stack!.adminPort}`;
const maxBase = () => `http://127.0.0.1:${stack!.maxPort}`;

async function bootStack(): Promise<Stack> {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "max-e2e-"));

  // 1. Mock provider (Anthropic Messages-shaped, any path), no keys needed.
  const mock = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          role: "assistant",
          stop_reason: "end_turn",
          content: [{ type: "text", text: "Hello from the mock provider" }],
        }),
      );
    });
  });
  await new Promise<void>((r) => mock.listen(0, "127.0.0.1", r));
  const mockPort = (mock.address() as net.AddressInfo).port;

  // 2. bae-config.toml pointing a provider at the mock (literal token, no env).
  const configPath = path.join(tmp, "bae-config.toml");
  fs.writeFileSync(
    configPath,
    [
      "[providers]",
      "[[providers.entries]]",
      'name        = "mock"',
      'provider    = "anthropic"',
      `base_url    = "http://127.0.0.1:${mockPort}"`,
      'model       = "mock-model"',
      'auth_token  = "literal-token-no-env-needed"',
      "max_tokens  = 1024",
      "",
    ].join("\n"),
  );

  // 3. Boot the real baesrv on freshly reserved loopback ports.
  const clientPort = await freePort();
  const adminPort = await freePort();
  const dbPath = path.join(tmp, "bae.db");
  const adminKeyFile = path.join(tmp, "admin-key.pem");
  let stderr = "";
  const baesrv = spawn(
    BAESRV_BIN!,
    ["serve", "--config", configPath, "--admin-key-file", adminKeyFile],
    {
      env: {
        ...process.env,
        BAE_ADDR: `127.0.0.1:${clientPort}`,
        BAE_ADMIN_ADDR: `127.0.0.1:${adminPort}`,
        BAE_DB_PATH: dbPath,
        BAE_LOG: "error",
      },
      stdio: ["ignore", "ignore", "pipe"],
    },
  );
  baesrv.stderr?.on("data", (d) => (stderr += d.toString()));

  await waitFor(
    async () => {
      try {
        const r = await fetch(`http://127.0.0.1:${clientPort}/healthz`);
        return r.ok;
      } catch {
        return false;
      }
    },
    { timeout: 30000, label: `baesrv readiness (stderr so far: ${stderr})` },
  ).catch((e) => {
    throw new Error(`${e.message}\nbaesrv stderr:\n${stderr}`);
  });

  const adminToken = fs.readFileSync(adminKeyFile, "utf8").trim();

  // 4. Recording proxy in front of the client port (only MAX goes through it).
  const proxy = await startRecordingProxy(clientPort);

  // 5. Boot the real max/server in-process, pointed at admin + the proxy.
  const observerKeysFile = path.join(tmp, "max-observer-keys.json");
  const webDist = path.join(tmp, "web-dist");
  fs.mkdirSync(webDist, { recursive: true });
  const maxPort = await freePort();
  const max = buildMaxServer(
    {
      BAE_MAX_ADDR: `127.0.0.1:${maxPort}`,
      BAE_ADMIN_ADDR: `127.0.0.1:${adminPort}`,
      BAE_CLIENT_ADDR: `127.0.0.1:${proxy.port}`,
      BAE_ADMIN_TOKEN: adminToken,
      BAE_MAX_PASSWORD: MAX_PASSWORD,
      BAE_MAX_WEB_DIST: webDist,
      BAE_MAX_OBSERVER_KEYS_FILE: observerKeysFile,
    } as NodeJS.ProcessEnv,
    webDist,
  );
  await new Promise<void>((r) =>
    max.server.listen(max.config.port, max.config.host, r),
  );

  return {
    tmp,
    baesrv,
    clientPort,
    adminPort,
    adminToken,
    mock,
    proxy,
    max,
    maxPort,
  };
}

async function teardownStack(s: Stack): Promise<void> {
  // `http.Server.close()` only resolves once EVERY connection has ended, and
  // both the browser WebSockets and the keep-alive sockets pooled by undici
  // (fetch) and the bridge's upstream stay open well past the last test. Left
  // to idle-timeout on their own they routinely outlast the afterAll hook
  // budget, so force them shut: terminate the live WS clients, then
  // `closeAllConnections()` on each HTTP server before awaiting its close.
  for (const client of s.max.wss.clients) client.terminate();
  s.max.wss.close();
  s.max.server.closeAllConnections();
  await new Promise<void>((r) => s.max.server.close(() => r()));
  s.proxy.server.closeAllConnections();
  await new Promise<void>((r) => s.proxy.server.close(() => r()));
  s.mock.closeAllConnections();
  await new Promise<void>((r) => s.mock.close(() => r()));
  s.baesrv.kill("SIGKILL");
  await new Promise<void>((r) => {
    s.baesrv.once("exit", () => r());
    setTimeout(r, 2000);
  });
  fs.rmSync(s.tmp, { recursive: true, force: true });
}

// ---------------------------------------------------------------------------
// HTTP helpers: admin (as MAX would), the raw driver, and MAX's web surface.
// ---------------------------------------------------------------------------

interface Json {
  status: number;
  body: unknown;
}

async function readJson(r: Response): Promise<Json> {
  const text = await r.text();
  let body: unknown;
  try {
    body = text ? JSON.parse(text) : undefined;
  } catch {
    body = text;
  }
  return { status: r.status, body };
}

async function admin(
  method: string,
  path: string,
  body?: unknown,
): Promise<Json> {
  return readJson(
    await fetch(`${adminBase()}${path}`, {
      method,
      headers: {
        authorization: `Bearer ${stack!.adminToken}`,
        ...(body !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    }),
  );
}

async function createProfile(): Promise<string> {
  const { status, body } = await admin("POST", "/admin/v1/profiles", {
    name: `p-${randomUUID()}`,
    primary_provider: "mock",
    fallback_providers: [],
    allowed_tools: [],
  });
  expect(status, JSON.stringify(body)).toBe(201);
  return (body as { id: string }).id;
}

/** Create a client key, returning `{id, key}`. */
async function createKey(
  name: string,
  profileId: string,
): Promise<{ id: string; key: string }> {
  const { status, body } = await admin("POST", "/admin/v1/keys", {
    name,
    profile_id: profileId,
  });
  expect(status, JSON.stringify(body)).toBe(201);
  const b = body as { id: string; key: string };
  return { id: b.id, key: b.key };
}

/** Full server-side event history for a session (admin port), in log order. */
async function serverEvents(
  sessionId: string,
): Promise<Array<{ id: string; event_type: string }>> {
  const { body } = await admin(
    "GET",
    `/admin/v1/sessions/${sessionId}/events?limit=500`,
  );
  return (body as { items: Array<{ id: string; event_type: string }> }).items;
}

// --- The raw driver: talks to the client port DIRECTLY (never through MAX) ---

async function driverCreateSession(
  clientKey: string,
): Promise<{ sessionId: string; sessionKey: string }> {
  const { status, body } = await readJson(
    await fetch(`${clientBase()}/api/v1/sessions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${clientKey}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ client_version: "1.0.0", tools: [] }),
    }),
  );
  expect(status, JSON.stringify(body)).toBe(201);
  const b = body as { session_id: string; session_key: string };
  return { sessionId: b.session_id, sessionKey: b.session_key };
}

/** Drive a JSON-RPC method over `/rpc`, draining the NDJSON stream fully. */
async function driverRpc(
  sessionId: string,
  sessionKey: string,
  method: string,
  params: unknown = {},
): Promise<void> {
  const r = await fetch(`${clientBase()}/api/v1/sessions/${sessionId}/rpc`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${sessionKey}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  await r.text(); // drain so the turn completes and its events are persisted
}

/** Create a session, register as driver, and (optionally) send `n` messages. */
async function driveSession(
  clientKey: string,
  messages: number,
): Promise<{ sessionId: string; sessionKey: string }> {
  const s = await driverCreateSession(clientKey);
  await driverRpc(s.sessionId, s.sessionKey, "session.registerDriver", {});
  for (let i = 0; i < messages; i++) {
    await driverRpc(s.sessionId, s.sessionKey, "session.sendMessage", {
      message: { role: "user", content: `message ${i}` },
    });
  }
  return s;
}

// --- MAX's own web surface (login + cookie-gated REST) ---

async function login(): Promise<string> {
  const r = await fetch(`${maxBase()}/api/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ password: MAX_PASSWORD }),
  });
  expect(r.status).toBe(200);
  const setCookie = r.headers.get("set-cookie");
  expect(setCookie).toBeTruthy();
  return setCookie!.split(";")[0]!; // "bae_max_session=<token>"
}

async function maxReq(
  method: string,
  path: string,
  cookie?: string,
  body?: unknown,
): Promise<Json> {
  return readJson(
    await fetch(`${maxBase()}${path}`, {
      method,
      headers: {
        ...(cookie ? { cookie } : {}),
        ...(body !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    }),
  );
}

// --- Browser-facing WebSocket observer ---

interface Observer {
  ws: WebSocket;
  frames: Array<Record<string, unknown>>;
  /** Flattened event ids as received: history batch first, then live. */
  eventIds: string[];
  ended: boolean;
  opened: Promise<void>;
  gotHistory: Promise<void>;
  close(): void;
}

function observe(
  sessionId: string,
  profileId: string,
  cookie: string,
): Observer {
  const url =
    `ws://127.0.0.1:${stack!.maxPort}/ws/sessions/${sessionId}` +
    `?profile_id=${encodeURIComponent(profileId)}`;
  const ws = new WebSocket(url, { headers: { cookie } });
  const obs: Observer = {
    ws,
    frames: [],
    eventIds: [],
    ended: false,
    opened: new Promise<void>((resolve, reject) => {
      ws.once("open", () => resolve());
      ws.once("unexpected-response", (_req, res) =>
        reject(new Error(`ws upgrade refused: ${res.statusCode}`)),
      );
      ws.once("error", reject);
    }),
    gotHistory: undefined as unknown as Promise<void>,
    close: () => ws.close(),
  };
  let resolveHistory: () => void;
  obs.gotHistory = new Promise<void>((r) => (resolveHistory = r));
  ws.on("message", (data) => {
    const msg = JSON.parse(data.toString()) as Record<string, unknown>;
    obs.frames.push(msg);
    if (msg.type === "history") {
      for (const e of msg.events as Array<{ id: string }>)
        obs.eventIds.push(e.id);
      resolveHistory();
    } else if (msg.type === "event") {
      obs.eventIds.push((msg.event as { id: string }).id);
    } else if (msg.type === "session_ended") {
      obs.ended = true;
    }
  });
  // Swallow post-open socket errors (e.g. normal-close races) so they don't
  // surface as unhandled rejections after `opened` already settled.
  ws.on("error", () => {});
  return obs;
}

/** Attempt a WS upgrade; resolve whether it opened and the HTTP status if not. */
function wsAttempt(
  sessionId: string,
  cookie?: string,
): Promise<{ opened: boolean; status?: number }> {
  return new Promise((resolve) => {
    let settled = false;
    const done = (v: { opened: boolean; status?: number }) => {
      if (settled) return;
      settled = true;
      resolve(v);
    };
    const ws = new WebSocket(
      `ws://127.0.0.1:${stack!.maxPort}/ws/sessions/${sessionId}`,
      cookie ? { headers: { cookie } } : {},
    );
    ws.once("open", () => {
      done({ opened: true });
      ws.close();
    });
    ws.once("unexpected-response", (_req, res) => {
      done({ opened: false, status: res.statusCode });
      ws.terminate();
    });
    ws.once("error", () => done({ opened: false }));
  });
}

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

const suite = BAESRV_BIN ? describe : describe.skip;

suite("MAX ⇆ baesrv cross-component integration", () => {
  const openObservers: Observer[] = [];

  beforeAll(async () => {
    stack = await bootStack();
  }, 60000);

  afterAll(async () => {
    if (stack) await teardownStack(stack);
    stack = undefined;
  });

  afterEach(async () => {
    for (const o of openObservers.splice(0)) o.close();
    // Give the bridge a tick to tear down upstream streams between tests.
    await sleep(50);
  });

  it("full observe flow: WS gets every event in order; MAX never drives", async () => {
    const profileId = await createProfile();
    const driver = await createKey("driver", profileId);

    // Driver opens a session and sends one message (history), all directly.
    const { sessionId, sessionKey } = await driveSession(driver.key, 1);

    const cookie = await login();
    const obs = observe(sessionId, profileId, cookie);
    openObservers.push(obs);
    await obs.opened;
    await obs.gotHistory;

    // Now drive MORE messages so the observer sees them arrive live, in order.
    await driverRpc(sessionId, sessionKey, "session.sendMessage", {
      message: { role: "user", content: "live one" },
    });
    await driverRpc(sessionId, sessionKey, "session.sendMessage", {
      message: { role: "user", content: "live two" },
    });

    // The observer's flattened id stream must exactly equal the server log.
    await waitFor(
      async () => {
        const server = await serverEvents(sessionId);
        return obs.eventIds.length === server.length;
      },
      { label: "observer to catch up to the server event log", timeout: 8000 },
    ).catch(async (e) => {
      const server = await serverEvents(sessionId);
      const got = new Set(obs.eventIds);
      const missing = server.filter((s) => !got.has(s.id));
      const extra = obs.eventIds.filter(
        (id) => !server.some((s) => s.id === id),
      );
      console.error(
        "DIAG missing from observer:",
        missing.map((m) => `${m.id}:${m.event_type}`),
        "\nDIAG extra in observer:",
        extra,
        "\nserver order:",
        server.map((s) => s.event_type),
      );
      throw e;
    });
    const serverIds = (await serverEvents(sessionId)).map((e) => e.id);
    expect(obs.eventIds).toEqual(serverIds);
    // Sanity: this really was a rich, multi-event session, in order.
    expect(serverIds.length).toBeGreaterThanOrEqual(12);

    // Request-log inspection: MAX only ever joined + subscribed as an observer.
    const mine = stack!.proxy.requests.filter((r) =>
      r.path.includes(sessionId),
    );
    expect(mine.some((r) => r.rpcMethod === "session.registerDriver")).toBe(
      false,
    );
    expect(mine.some((r) => r.rpcMethod === "session.sendMessage")).toBe(false);
    expect(mine.some((r) => r.path.endsWith("/join"))).toBe(true);
    expect(mine.some((r) => r.rpcMethod === "session.subscribe")).toBe(true);
  }, 30000);

  it("fan-out: two browser sockets on one session share a single upstream join", async () => {
    const profileId = await createProfile();
    const driver = await createKey("driver", profileId);
    const { sessionId, sessionKey } = await driveSession(driver.key, 1);

    const cookie = await login();
    const a = observe(sessionId, profileId, cookie);
    openObservers.push(a);
    await a.opened;
    await a.gotHistory;

    const b = observe(sessionId, profileId, cookie);
    openObservers.push(b);
    await b.opened;
    await b.gotHistory;

    // Drive a live message; both sockets must receive the full log.
    await driverRpc(sessionId, sessionKey, "session.sendMessage", {
      message: { role: "user", content: "seen by both" },
    });

    await waitFor(
      async () => {
        const n = (await serverEvents(sessionId)).length;
        return a.eventIds.length === n && b.eventIds.length === n;
      },
      { label: "both observers to catch up" },
    );
    const serverIds = (await serverEvents(sessionId)).map((e) => e.id);
    expect(a.eventIds).toEqual(serverIds);
    expect(b.eventIds).toEqual(serverIds);

    // Exactly ONE upstream join → exactly one `session.join` event logged.
    const joins = (await serverEvents(sessionId)).filter(
      (e) => e.event_type === "session.join",
    );
    expect(joins).toHaveLength(1);
    // And MAX opened exactly one upstream subscribe for the shared session.
    const subs = stack!.proxy.requests.filter(
      (r) => r.path.includes(sessionId) && r.rpcMethod === "session.subscribe",
    );
    expect(subs).toHaveLength(1);
  }, 30000);

  it("profile deletion stays 409 profile_in_use after MAX observes; key is badged", async () => {
    const profileId = await createProfile();
    const driver = await createKey("driver", profileId);
    const { sessionId } = await driveSession(driver.key, 1);

    const cookie = await login();
    const obs = observe(sessionId, profileId, cookie);
    openObservers.push(obs);
    await obs.opened;
    await obs.gotHistory;

    // MAX provisions its per-profile observer key lazily on first observe.
    await waitFor(
      async () => {
        const { body } = await maxReq("GET", "/api/keys", cookie);
        const items = (body as { items: Array<{ name: string }> }).items;
        return items.some((k) => k.name === `max-observer-${profileId}`);
      },
      { label: "MAX's observer key to be provisioned" },
    );

    // Remove the *driver* key: now only MAX's observer key references the
    // profile — proving MAX's key alone keeps it undeletable.
    const del = await maxReq("DELETE", `/api/keys/${driver.id}`, cookie);
    expect(del.status).toBe(204);

    const attempt = await maxReq(
      "DELETE",
      `/api/profiles/${profileId}`,
      cookie,
    );
    expect(attempt.status).toBe(409);
    expect((attempt.body as { type: string }).type).toBe("profile_in_use");

    // The blocker is visibly MAX's own key, distinguishable by its prefix.
    const { body } = await maxReq("GET", "/api/keys", cookie);
    const items = (
      body as { items: Array<{ name: string; profile_id: string }> }
    ).items;
    const maxKeys = items.filter(
      (k) => k.profile_id === profileId && k.name.startsWith("max-observer-"),
    );
    expect(maxKeys).toHaveLength(1);
    expect(maxKeys[0]!.name).toBe(`max-observer-${profileId}`);
  }, 30000);

  describe("auth gate (REST + WS upgrade)", () => {
    it("rejects a wrong password without leaking closeness", async () => {
      const r = await maxReq("POST", "/api/login", undefined, {
        password: "test-max-password-shh", // one char short
      });
      expect(r.status).toBe(401);
      expect(r.body).toEqual({ error: "invalid_password" });
    });

    it("rejects every REST route without a valid cookie", async () => {
      for (const [method, path] of [
        ["GET", "/api/session"],
        ["GET", "/api/keys"],
        ["GET", "/api/profiles"],
        ["GET", "/api/providers"],
        ["GET", "/api/mcp-servers"],
        ["GET", "/api/sessions"],
        ["POST", "/api/keys"],
      ] as const) {
        const none = await maxReq(method, path);
        expect(none.status, `${method} ${path} with no cookie`).toBe(401);
        const bad = await maxReq(method, path, "bae_max_session=forged.abc");
        expect(bad.status, `${method} ${path} with a forged cookie`).toBe(401);
      }
    });

    it("leaves the login route and health shell unauthenticated", async () => {
      // /healthz is the static/login shell surface — reachable with no cookie.
      const health = await maxReq("GET", "/healthz");
      expect(health.status).toBe(200);
    });

    it("a correct password grants a cookie that authorizes REST", async () => {
      const cookie = await login();
      const ok = await maxReq("GET", "/api/session", cookie);
      expect(ok.status).toBe(200);
      expect(ok.body).toEqual({ authenticated: true });
    });

    it("refuses the WS upgrade without/with a bad cookie, allows a valid one", async () => {
      const profileId = await createProfile();
      const driver = await createKey("driver", profileId);
      const { sessionId } = await driveSession(driver.key, 0);

      const noCookie = await wsAttempt(sessionId);
      expect(noCookie.opened).toBe(false);
      expect(noCookie.status).toBe(401);

      const badCookie = await wsAttempt(
        sessionId,
        "bae_max_session=forged.deadbeef",
      );
      expect(badCookie.opened).toBe(false);
      expect(badCookie.status).toBe(401);

      const cookie = await login();
      const good = await wsAttempt(sessionId, cookie);
      expect(good.opened).toBe(true);
    }, 30000);
  });
});
