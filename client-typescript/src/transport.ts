import { ApiError, ProvidersFailedError, TransportError } from "./errors.js";
import type { Message, SessionEvent } from "./types.js";

/** A single HTTP request against the client surface. */
export interface TransportRequest {
  method: "GET" | "POST" | "DELETE";
  /** Path beginning with `/`, e.g. `/api/v1/sessions`. */
  path: string;
  /** Bearer token (client key or session key). */
  token: string;
  /** JSON request body, if any. */
  body?: unknown;
}

/** A parsed HTTP response. `body` is the decoded JSON (or `undefined`). */
export interface TransportResponse {
  status: number;
  body: unknown;
}

/**
 * The seam the harness talks through. The default implementation uses `fetch`;
 * tests inject a mock so the whole loop runs offline.
 */
export interface Transport {
  request(req: TransportRequest): Promise<TransportResponse>;
}

/** `fetch`-backed transport rooted at a server base URL. */
export class FetchTransport implements Transport {
  constructor(private readonly baseUrl: string) {}

  async request(req: TransportRequest): Promise<TransportResponse> {
    let res: Response;
    try {
      res = await fetch(this.baseUrl + req.path, {
        method: req.method,
        headers: {
          authorization: `Bearer ${req.token}`,
          ...(req.body !== undefined
            ? { "content-type": "application/json" }
            : {}),
        },
        body: req.body !== undefined ? JSON.stringify(req.body) : undefined,
      });
    } catch (cause) {
      throw new TransportError(
        `request to ${req.method} ${req.path} failed: ${
          cause instanceof Error ? cause.message : String(cause)
        }`,
      );
    }

    const text = await res.text();
    let body: unknown;
    if (text.length > 0) {
      try {
        body = JSON.parse(text);
      } catch {
        throw new TransportError(
          `non-JSON ${res.status} response from ${req.method} ${req.path}`,
        );
      }
    }
    return { status: res.status, body };
  }
}

/** The `{message, events}` body returned by `POST …/messages`. */
export interface MessagesResponse {
  message: Message;
  events: SessionEvent[];
}

/**
 * Interpret a `POST …/messages` response: `200` → the body; `502` →
 * {@link ProvidersFailedError}; any other non-2xx → {@link ApiError}.
 */
export function parseMessagesResponse(
  res: TransportResponse,
): MessagesResponse {
  if (res.status === 200) {
    return res.body as MessagesResponse;
  }
  if (res.status === 502) {
    const body = (res.body ?? {}) as Partial<MessagesResponse>;
    throw new ProvidersFailedError(
      body.message ?? { role: "assistant", content: "" },
      body.events ?? [],
    );
  }
  throw ApiError.fromBody(res.status, res.body);
}

/** Throw {@link ApiError} on any non-2xx status; otherwise return the body. */
export function expectOk(res: TransportResponse): unknown {
  if (res.status < 200 || res.status >= 300) {
    throw ApiError.fromBody(res.status, res.body);
  }
  return res.body;
}
