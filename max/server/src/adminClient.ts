//! A minimal `fetch`-based wrapper over baesrv's admin port (`/admin/v1/*`).
//!
//! Request bodies mirror the shapes documented in `docs/reference/admin-api.md`
//! field-for-field. The request-*building* seam ({@link buildAdminRequest}) is
//! pure and unit-testable in isolation from the network; {@link AdminClient}
//! executes those requests with the admin bearer token attached. The browser
//! never sees this token — MAX proxies a thin REST surface in front of it.

/** A fully-described HTTP request to the admin port, before execution. */
export interface AdminRequest {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string | undefined;
}

/** A declarative spec for one admin call — the input to {@link buildAdminRequest}. */
export interface AdminRequestSpec {
  method: "GET" | "POST" | "PUT" | "DELETE";
  /** Path beginning with `/admin/v1/...` (no host). */
  path: string;
  /** Query params; `undefined`/empty values are omitted. */
  query?: Record<string, string | number | undefined>;
  /** JSON body, serialized verbatim. */
  body?: unknown;
}

/** `POST`/`PUT /admin/v1/profiles` body — field-for-field the documented shape. */
export interface ProfileBody {
  name: string;
  primary_provider: string;
  fallback_providers: string[];
  mcp_servers: string[];
  allowed_tools: string[];
}

/** `POST /admin/v1/keys` body. */
export interface KeyBody {
  name: string;
  profile_id: string;
}

/**
 * Build the concrete request (method, absolute URL, headers, serialized body)
 * for an admin call. Pure: no I/O, deterministic, so the request shapes can be
 * asserted field-for-field in unit tests.
 */
export function buildAdminRequest(
  base: string,
  token: string,
  spec: AdminRequestSpec,
): AdminRequest {
  const trimmedBase = base.replace(/\/+$/, "");
  const url = new URL(`${trimmedBase}${spec.path}`);
  if (spec.query) {
    for (const [key, value] of Object.entries(spec.query)) {
      if (value === undefined || value === "") continue;
      url.searchParams.set(key, String(value));
    }
  }
  const headers: Record<string, string> = {
    authorization: `Bearer ${token}`,
  };
  let body: string | undefined;
  if (spec.body !== undefined) {
    headers["content-type"] = "application/json";
    body = JSON.stringify(spec.body);
  }
  return { method: spec.method, url: url.toString(), headers, body };
}

/** An admin-API error surfaced with the upstream status and RFC 7807 body. */
export class AdminApiError extends Error {
  constructor(
    public status: number,
    public type: string,
    public detail: string,
  ) {
    super(`admin API ${status} ${type}: ${detail}`);
    this.name = "AdminApiError";
  }
}

/** One page of a cursor-paginated admin list endpoint. */
export interface AdminPage {
  items: unknown[];
  next_cursor: string | null;
}

export class AdminClient {
  private readonly base: string;

  constructor(
    adminAddr: string,
    private readonly token: string,
    private readonly fetchImpl: typeof fetch = fetch,
  ) {
    this.base = adminAddr.includes("://") ? adminAddr : `http://${adminAddr}`;
  }

  /** Execute `spec`, returning the parsed JSON body (or `undefined` on 204). */
  async request(spec: AdminRequestSpec): Promise<unknown> {
    const req = buildAdminRequest(this.base, this.token, spec);
    let resp: globalThis.Response;
    try {
      resp = await this.fetchImpl(req.url, {
        method: req.method,
        headers: req.headers,
        body: req.body,
      });
    } catch (err) {
      throw new AdminApiError(
        502,
        "upstream_unreachable",
        `could not reach the admin port at ${this.base}: ${(err as Error).message}`,
      );
    }
    if (resp.status === 204) return undefined;
    const text = await resp.text();
    const parsed = text ? safeParse(text) : undefined;
    if (!resp.ok) {
      const problem = (parsed ?? {}) as Record<string, unknown>;
      throw new AdminApiError(
        resp.status,
        typeof problem.type === "string" ? problem.type : "error",
        typeof problem.detail === "string" ? problem.detail : text,
      );
    }
    return parsed;
  }

  // --- Profiles ---------------------------------------------------------

  listProfiles(query: PageParams): Promise<AdminPage> {
    return this.request({
      method: "GET",
      path: "/admin/v1/profiles",
      query,
    }) as Promise<AdminPage>;
  }

  getProfile(id: string): Promise<unknown> {
    return this.request({ method: "GET", path: `/admin/v1/profiles/${id}` });
  }

  createProfile(body: ProfileBody): Promise<unknown> {
    return this.request({ method: "POST", path: "/admin/v1/profiles", body });
  }

  replaceProfile(id: string, body: ProfileBody): Promise<unknown> {
    return this.request({
      method: "PUT",
      path: `/admin/v1/profiles/${id}`,
      body,
    });
  }

  deleteProfile(id: string): Promise<unknown> {
    return this.request({ method: "DELETE", path: `/admin/v1/profiles/${id}` });
  }

  // --- Keys -------------------------------------------------------------

  listKeys(query: PageParams): Promise<AdminPage> {
    return this.request({
      method: "GET",
      path: "/admin/v1/keys",
      query,
    }) as Promise<AdminPage>;
  }

  createKey(body: KeyBody): Promise<unknown> {
    return this.request({ method: "POST", path: "/admin/v1/keys", body });
  }

  deleteKey(id: string): Promise<unknown> {
    return this.request({ method: "DELETE", path: `/admin/v1/keys/${id}` });
  }

  // --- Registries -------------------------------------------------------

  listProviders(): Promise<unknown> {
    return this.request({ method: "GET", path: "/admin/v1/providers" });
  }

  listMcpServers(): Promise<unknown> {
    return this.request({ method: "GET", path: "/admin/v1/mcp-servers" });
  }

  // --- Sessions (read-only admin routes from section B) -----------------

  listSessions(query: PageParams & { state?: string }): Promise<AdminPage> {
    return this.request({
      method: "GET",
      path: "/admin/v1/sessions",
      query,
    }) as Promise<AdminPage>;
  }

  getSessionEvents(id: string, query: PageParams): Promise<AdminPage> {
    return this.request({
      method: "GET",
      path: `/admin/v1/sessions/${id}/events`,
      query,
    }) as Promise<AdminPage>;
  }

  /** Fetch the full event history for a session by following `next_cursor`. */
  async getAllSessionEvents(id: string): Promise<unknown[]> {
    const events: unknown[] = [];
    let cursor: string | undefined;
    do {
      const page = await this.getSessionEvents(id, { cursor, limit: 200 });
      events.push(...page.items);
      cursor = page.next_cursor ?? undefined;
    } while (cursor);
    return events;
  }
}

export interface PageParams {
  cursor?: string;
  limit?: number;
  // Index signature so a PageParams (and its `& { state? }` refinements) is
  // directly usable as an AdminRequestSpec `query` without a cast.
  [key: string]: string | number | undefined;
}

function safeParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}
