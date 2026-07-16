import { ApiError, TransportError } from "./errors.js";
import { injectAmbientContext } from "./telemetry.js";
import type { JsonRpcFrame, SessionEvent } from "./types.js";

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
 * The seam the harness talks through. `request` covers the REST management
 * routes (session open/close, events replay); `stream` drives the JSON-RPC
 * session loop over `…/rpc`, yielding one decoded {@link JsonRpcFrame} per
 * NDJSON line. Tests inject a mock so the whole loop runs offline.
 */
export interface Transport {
  request(req: TransportRequest): Promise<TransportResponse>;
  stream(req: TransportRequest): AsyncIterable<JsonRpcFrame>;
}

/** `fetch`-backed transport rooted at a server base URL. */
export class FetchTransport implements Transport {
  constructor(private readonly baseUrl: string) {}

  async request(req: TransportRequest): Promise<TransportResponse> {
    const headers: Record<string, string> = {
      authorization: `Bearer ${req.token}`,
    };
    if (req.body !== undefined) {
      headers["content-type"] = "application/json";
    }
    injectAmbientContext(headers);

    let res: Response;
    try {
      res = await fetch(this.baseUrl + req.path, {
        method: req.method,
        headers,
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

  /**
   * POST a JSON-RPC request and yield each NDJSON frame as it arrives. A
   * non-2xx status is a pre-stream RFC 7807 error ({@link ApiError}, e.g. auth)
   * raised before the first frame; the body itself is HTTP 200.
   */
  stream(req: TransportRequest): AsyncIterable<JsonRpcFrame> {
    const headers: Record<string, string> = {
      authorization: `Bearer ${req.token}`,
      "content-type": "application/json",
      accept: "application/x-ndjson",
    };
    injectAmbientContext(headers);

    return this.streamWithHeaders(req, headers);
  }

  private async *streamWithHeaders(
    req: TransportRequest,
    headers: Record<string, string>,
  ): AsyncIterable<JsonRpcFrame> {
    let res: Response;
    try {
      res = await fetch(this.baseUrl + req.path, {
        method: req.method,
        headers,
        body: req.body !== undefined ? JSON.stringify(req.body) : undefined,
      });
    } catch (cause) {
      throw new TransportError(
        `request to ${req.method} ${req.path} failed: ${
          cause instanceof Error ? cause.message : String(cause)
        }`,
      );
    }

    if (!res.ok) {
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
      throw ApiError.fromBody(res.status, body);
    }

    if (res.body === null) return;
    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.length > 0) yield parseFrame(line, req);
      }
    }
    const last = buf.trim();
    if (last.length > 0) yield parseFrame(last, req);
  }
}

function parseFrame(line: string, req: TransportRequest): JsonRpcFrame {
  try {
    return JSON.parse(line) as JsonRpcFrame;
  } catch {
    throw new TransportError(
      `malformed JSON-RPC frame from ${req.method} ${req.path}`,
    );
  }
}

/** A frame carrying an `id` is the terminal response; anything else is a notification. */
export function isTerminalFrame(frame: JsonRpcFrame): boolean {
  return frame.id !== undefined && frame.id !== null;
}

/** Decode a `session.event` notification's `params` into a {@link SessionEvent}, else null. */
export function eventFromFrame(frame: JsonRpcFrame): SessionEvent | null {
  if (frame.method !== "session.event" || frame.params === undefined) {
    return null;
  }
  return frame.params as SessionEvent;
}

/** Throw {@link ApiError} on any non-2xx status; otherwise return the body. */
export function expectOk(res: TransportResponse): unknown {
  if (res.status < 200 || res.status >= 300) {
    throw ApiError.fromBody(res.status, res.body);
  }
  return res.body;
}
