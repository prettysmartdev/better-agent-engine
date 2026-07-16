// Thin fetch wrapper around MAX's own web API (same-origin `/api/*`). The
// session cookie is HttpOnly, so JS never reads it: we rely on same-origin
// credentials and treat any 401 as "the session is gone, show login".

import type {
  ConfigResponse,
  KeyCreated,
  KeyListItem,
  Page,
  Profile,
  ProfileInput,
  RegistryEntry,
  SessionEvent,
  SessionListItem,
  SessionState,
} from "./types";

/** An error carrying the upstream status and the `{type, detail}` body. */
export class ApiError extends Error {
  status: number;
  type: string;
  detail: string;
  constructor(status: number, type: string, detail: string) {
    super(detail || type || `HTTP ${status}`);
    this.name = "ApiError";
    this.status = status;
    this.type = type;
    this.detail = detail;
  }
}

/** Thrown/reported when the session cookie is missing or invalid. */
export class UnauthorizedError extends ApiError {
  constructor() {
    super(401, "unauthorized", "unauthorized");
    this.name = "UnauthorizedError";
  }
}

let onUnauthorized: (() => void) | null = null;

/** Register a handler invoked whenever a request comes back 401. */
export function setUnauthorizedHandler(fn: (() => void) | null): void {
  onUnauthorized = fn;
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method,
      credentials: "same-origin",
      headers:
        body === undefined ? undefined : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, "network_error", "Could not reach the MAX server.");
  }

  if (res.status === 401) {
    onUnauthorized?.();
    throw new UnauthorizedError();
  }

  if (res.status === 204) {
    return undefined as T;
  }

  let data: unknown = null;
  const text = await res.text();
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = null;
    }
  }

  if (!res.ok) {
    const err = (data ?? {}) as {
      type?: string;
      detail?: string;
      error?: string;
    };
    throw new ApiError(
      res.status,
      err.type ?? err.error ?? "error",
      err.detail ?? "",
    );
  }

  return data as T;
}

function query(params: Record<string, string | number | undefined>): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "")
      parts.push(`${k}=${encodeURIComponent(String(v))}`);
  }
  return parts.length ? `?${parts.join("&")}` : "";
}

// --- Auth ---------------------------------------------------------------

export async function login(password: string): Promise<void> {
  await request<{ ok: true }>("POST", "/api/login", { password });
}

export async function logout(): Promise<void> {
  await request<{ ok: true }>("POST", "/api/logout");
}

/** Returns true if the current cookie is valid; false on 401. */
export async function checkSession(): Promise<boolean> {
  try {
    await request<{ authenticated: true }>("GET", "/api/session");
    return true;
  } catch (e) {
    if (e instanceof UnauthorizedError) return false;
    throw e;
  }
}

// --- Profiles -----------------------------------------------------------

export function listProfiles(
  cursor?: string,
  limit = 100,
): Promise<Page<Profile>> {
  return request("GET", `/api/profiles${query({ cursor, limit })}`);
}
export function createProfile(input: ProfileInput): Promise<Profile> {
  return request("POST", "/api/profiles", input);
}
export function updateProfile(
  id: string,
  input: ProfileInput,
): Promise<Profile> {
  return request("PUT", `/api/profiles/${encodeURIComponent(id)}`, input);
}
export function deleteProfile(id: string): Promise<void> {
  return request("DELETE", `/api/profiles/${encodeURIComponent(id)}`);
}

// --- Keys ---------------------------------------------------------------

export function listKeys(
  cursor?: string,
  limit = 100,
): Promise<Page<KeyListItem>> {
  return request("GET", `/api/keys${query({ cursor, limit })}`);
}
export function createKey(
  name: string,
  profile_id: string,
): Promise<KeyCreated> {
  return request("POST", "/api/keys", { name, profile_id });
}
export function deleteKey(id: string): Promise<void> {
  return request("DELETE", `/api/keys/${encodeURIComponent(id)}`);
}

// --- Registries ---------------------------------------------------------

export function listProviders(): Promise<Page<RegistryEntry>> {
  return request("GET", "/api/providers?limit=500");
}
export function listMcpServers(): Promise<Page<RegistryEntry>> {
  return request("GET", "/api/mcp-servers?limit=500");
}

// --- Config -------------------------------------------------------------

export function getConfig(): Promise<ConfigResponse> {
  return request("GET", "/api/config");
}

// --- Sessions -----------------------------------------------------------

export function listSessions(
  state: SessionState | undefined,
  cursor?: string,
  limit = 100,
): Promise<Page<SessionListItem>> {
  return request("GET", `/api/sessions${query({ state, cursor, limit })}`);
}
export function listSessionEvents(
  id: string,
  cursor?: string,
  limit = 500,
): Promise<Page<SessionEvent>> {
  return request(
    "GET",
    `/api/sessions/${encodeURIComponent(id)}/events${query({ cursor, limit })}`,
  );
}
