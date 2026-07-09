import { describe, expect, it, vi } from "vitest";
import {
  AdminApiError,
  AdminClient,
  buildAdminRequest,
  type ProfileBody,
} from "./adminClient.js";

describe("buildAdminRequest", () => {
  it("builds a create-profile POST matching the documented body shape", () => {
    const body: ProfileBody = {
      name: "main",
      primary_provider: "anthropic-sonnet",
      fallback_providers: [],
      mcp_servers: ["filesystem"],
      allowed_tools: ["get_current_time"],
    };
    const req = buildAdminRequest("http://127.0.0.1:8081", "tok", {
      method: "POST",
      path: "/admin/v1/profiles",
      body,
    });
    expect(req.method).toBe("POST");
    expect(req.url).toBe("http://127.0.0.1:8081/admin/v1/profiles");
    expect(req.headers.authorization).toBe("Bearer tok");
    expect(req.headers["content-type"]).toBe("application/json");
    // Field-for-field the admin-api.md shape (order and keys).
    expect(JSON.parse(req.body!)).toEqual({
      name: "main",
      primary_provider: "anthropic-sonnet",
      fallback_providers: [],
      mcp_servers: ["filesystem"],
      allowed_tools: ["get_current_time"],
    });
  });

  it("builds a create-key POST with exactly name + profile_id", () => {
    const req = buildAdminRequest("http://h:1", "tok", {
      method: "POST",
      path: "/admin/v1/keys",
      body: { name: "my-agent", profile_id: "pro_123" },
    });
    expect(JSON.parse(req.body!)).toEqual({
      name: "my-agent",
      profile_id: "pro_123",
    });
  });

  it("builds a replace-profile PUT to the id path", () => {
    const req = buildAdminRequest("http://h:1", "tok", {
      method: "PUT",
      path: "/admin/v1/profiles/pro_9",
      body: {
        name: "n",
        primary_provider: "p",
        fallback_providers: [],
        mcp_servers: [],
        allowed_tools: [],
      },
    });
    expect(req.method).toBe("PUT");
    expect(req.url).toBe("http://h:1/admin/v1/profiles/pro_9");
  });

  it("appends query params, omitting undefined/empty ones", () => {
    const req = buildAdminRequest("http://h:1", "tok", {
      method: "GET",
      path: "/admin/v1/sessions",
      query: { limit: 20, cursor: undefined, state: "open" },
    });
    const url = new URL(req.url);
    expect(url.searchParams.get("limit")).toBe("20");
    expect(url.searchParams.get("state")).toBe("open");
    expect(url.searchParams.has("cursor")).toBe(false);
    expect(req.body).toBeUndefined();
    expect(req.headers["content-type"]).toBeUndefined();
  });

  it("normalizes a bare host:port base into an http URL prefix at the client", () => {
    const req = buildAdminRequest("http://127.0.0.1:8081", "t", {
      method: "GET",
      path: "/admin/v1/keys",
    });
    expect(req.url.startsWith("http://127.0.0.1:8081/admin/v1/keys")).toBe(
      true,
    );
  });
});

describe("AdminClient", () => {
  function mockFetch(status: number, body: unknown) {
    return vi.fn(
      async () =>
        new Response(body === undefined ? "" : JSON.stringify(body), {
          status,
          headers: { "content-type": "application/json" },
        }),
    );
  }

  it("prefixes a bare host:port with http:// and sends the bearer token", async () => {
    const fetchImpl = mockFetch(200, { items: [], next_cursor: null });
    const client = new AdminClient(
      "127.0.0.1:8081",
      "adm",
      fetchImpl as unknown as typeof fetch,
    );
    await client.listKeys({ limit: 5 });
    const [url, init] = fetchImpl.mock.calls[0]!;
    expect(String(url)).toBe("http://127.0.0.1:8081/admin/v1/keys?limit=5");
    expect((init as RequestInit).headers).toMatchObject({
      authorization: "Bearer adm",
    });
  });

  it("returns undefined on a 204", async () => {
    const fetchImpl = vi.fn(async () => new Response(null, { status: 204 }));
    const client = new AdminClient(
      "h:1",
      "t",
      fetchImpl as unknown as typeof fetch,
    );
    await expect(client.deleteKey("key_1")).resolves.toBeUndefined();
  });

  it("maps an error status to an AdminApiError carrying type + detail", async () => {
    const fetchImpl = mockFetch(409, {
      type: "profile_in_use",
      detail: "active keys reference this profile",
    });
    const client = new AdminClient(
      "h:1",
      "t",
      fetchImpl as unknown as typeof fetch,
    );
    await expect(client.deleteProfile("pro_1")).rejects.toMatchObject({
      status: 409,
      type: "profile_in_use",
      detail: "active keys reference this profile",
    });
  });

  it("wraps a transport failure as a 502 upstream_unreachable", async () => {
    const fetchImpl = vi.fn(async () => {
      throw new Error("ECONNREFUSED");
    });
    const client = new AdminClient(
      "h:1",
      "t",
      fetchImpl as unknown as typeof fetch,
    );
    await expect(client.listProviders()).rejects.toBeInstanceOf(AdminApiError);
    await expect(client.listProviders()).rejects.toMatchObject({ status: 502 });
  });

  it("pages getAllSessionEvents until next_cursor is null", async () => {
    const pages = [
      { items: [{ id: "e1" }], next_cursor: "1" },
      { items: [{ id: "e2" }], next_cursor: null },
    ];
    let call = 0;
    const fetchImpl = vi.fn(async () => {
      const page = pages[call++];
      return new Response(JSON.stringify(page), { status: 200 });
    });
    const client = new AdminClient(
      "h:1",
      "t",
      fetchImpl as unknown as typeof fetch,
    );
    const events = await client.getAllSessionEvents("ses_1");
    expect(events).toEqual([{ id: "e1" }, { id: "e2" }]);
    expect(fetchImpl).toHaveBeenCalledTimes(2);
  });
});
