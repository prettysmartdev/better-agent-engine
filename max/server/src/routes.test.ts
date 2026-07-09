import { describe, expect, it, vi } from "vitest";
import request from "supertest";
import { createApp } from "./app.js";
import { createApiRouter } from "./routes.js";
import { AdminApiError, type AdminClient } from "./adminClient.js";
import { createAuthenticator } from "./auth.js";
import { deriveCookieSecret } from "./credentials.js";

const PASSWORD = "correct-horse";

function makeApp(admin: Partial<AdminClient>) {
  const auth = createAuthenticator(PASSWORD, deriveCookieSecret(PASSWORD));
  const api = createApiRouter(admin as AdminClient, auth);
  return createApp({ webDist: "/nonexistent", api });
}

/** Log in and return the session Cookie header value. */
async function login(app: ReturnType<typeof makeApp>): Promise<string> {
  const res = await request(app)
    .post("/api/login")
    .send({ password: PASSWORD });
  expect(res.status).toBe(200);
  const cookie = res.headers["set-cookie"]?.[0];
  expect(cookie).toBeTruthy();
  return cookie!;
}

describe("auth gate", () => {
  it("rejects an unauthenticated API request with 401", async () => {
    const app = makeApp({ listKeys: vi.fn() });
    const res = await request(app).get("/api/keys");
    expect(res.status).toBe(401);
  });

  it("rejects a wrong password without revealing closeness", async () => {
    const app = makeApp({});
    const res = await request(app)
      .post("/api/login")
      .send({ password: "correct-hors" });
    expect(res.status).toBe(401);
    expect(res.body).toEqual({ error: "invalid_password" });
  });

  it("grants a cookie that authorizes subsequent requests", async () => {
    const listKeys = vi.fn(async () => ({ items: [], next_cursor: null }));
    const app = makeApp({ listKeys });
    const cookie = await login(app);
    const res = await request(app).get("/api/keys").set("Cookie", cookie);
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ items: [], next_cursor: null });
    expect(listKeys).toHaveBeenCalledOnce();
  });

  it("does not require auth for the login route or static shell", async () => {
    const app = makeApp({});
    // /api/login reachable without a cookie (returns 401 only on bad password).
    const bad = await request(app).post("/api/login").send({ password: "x" });
    expect(bad.status).toBe(401);
    // /healthz is unauthenticated.
    expect((await request(app).get("/healthz")).status).toBe(200);
  });
});

describe("proxy mapping", () => {
  it("forwards create-profile as 201 with the returned body", async () => {
    const createProfile = vi.fn(async () => ({ id: "pro_1", name: "main" }));
    const app = makeApp({ createProfile });
    const cookie = await login(app);
    const body = {
      name: "main",
      primary_provider: "p",
      fallback_providers: [],
      mcp_servers: [],
      allowed_tools: [],
    };
    const res = await request(app)
      .post("/api/profiles")
      .set("Cookie", cookie)
      .send(body);
    expect(res.status).toBe(201);
    expect(res.body).toEqual({ id: "pro_1", name: "main" });
    expect(createProfile).toHaveBeenCalledWith(body);
  });

  it("returns 204 with no body when the admin call resolves undefined", async () => {
    const deleteKey = vi.fn(async () => undefined);
    const app = makeApp({ deleteKey });
    const cookie = await login(app);
    const res = await request(app)
      .delete("/api/keys/key_1")
      .set("Cookie", cookie);
    expect(res.status).toBe(204);
    expect(deleteKey).toHaveBeenCalledWith("key_1");
  });

  it("surfaces an AdminApiError with its upstream status and body", async () => {
    const deleteProfile = vi.fn(async () => {
      throw new AdminApiError(
        409,
        "profile_in_use",
        "active keys reference it",
      );
    });
    const app = makeApp({ deleteProfile });
    const cookie = await login(app);
    const res = await request(app)
      .delete("/api/profiles/pro_1")
      .set("Cookie", cookie);
    expect(res.status).toBe(409);
    expect(res.body).toEqual({
      type: "profile_in_use",
      detail: "active keys reference it",
    });
  });

  it("passes the ?state= filter and pagination through to listSessions", async () => {
    const listSessions = vi.fn(async () => ({ items: [], next_cursor: null }));
    const app = makeApp({ listSessions });
    const cookie = await login(app);
    await request(app)
      .get("/api/sessions?state=open&limit=10&cursor=5")
      .set("Cookie", cookie);
    expect(listSessions).toHaveBeenCalledWith({
      cursor: "5",
      limit: 10,
      state: "open",
    });
  });
});
