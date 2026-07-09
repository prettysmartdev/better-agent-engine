//! MAX's own authentication layer (work item 0007, section D).
//!
//! MAX's web port is the first thing in the project that exposes
//! admin-equivalent capability off-loopback, so it gates every route behind a
//! signed session cookie. The operator posts the single MAX password once
//! (`POST /api/login`); MAX issues an HMAC-signed cookie; every other REST
//! route and the WS upgrade require a valid cookie. There is deliberately no
//! disable flag.

import crypto from "node:crypto";
import type { NextFunction, Request, Response } from "express";

/** Cookie name carrying MAX's signed session token. */
export const SESSION_COOKIE = "bae_max_session";

/** Default cookie lifetime: 7 days. */
const DEFAULT_TTL_MS = 7 * 24 * 60 * 60 * 1000;

export interface Authenticator {
  /** Verify a login password attempt in constant time (no closeness leak). */
  verifyPassword(attempt: string): boolean;
  /** Mint a signed session cookie value valid until `now + ttl`. */
  issueCookie(nowMs: number): string;
  /** Validate a cookie value; true iff well-formed, signature valid, unexpired. */
  verifyCookie(value: string | undefined, nowMs: number): boolean;
  /** Cookie lifetime in ms (for `Max-Age`). */
  ttlMs: number;
}

/**
 * Build an {@link Authenticator} from the resolved password and its derived
 * HMAC secret.
 *
 * The cookie is `<expiryMs>.<hmacHex>` where the HMAC covers the expiry. This
 * is a stateless token: no server-side session table, and rotating the
 * password (which changes `secret`) invalidates every outstanding cookie.
 */
export function createAuthenticator(
  password: string,
  secret: Buffer,
  ttlMs: number = DEFAULT_TTL_MS,
): Authenticator {
  // Precompute a fixed-length digest of the password so comparison never leaks
  // length, and so we never hold the raw attempt alongside the real password in
  // a variable-time comparison.
  const passwordDigest = crypto.createHash("sha256").update(password).digest();

  function sign(expiryMs: number): string {
    const mac = crypto
      .createHmac("sha256", secret)
      .update(String(expiryMs))
      .digest("hex");
    return `${expiryMs}.${mac}`;
  }

  return {
    ttlMs,

    verifyPassword(attempt: string): boolean {
      const attemptDigest = crypto
        .createHash("sha256")
        .update(attempt)
        .digest();
      // Both digests are 32 bytes, so this is a constant-time compare that
      // reveals nothing about how close a wrong password was.
      return crypto.timingSafeEqual(attemptDigest, passwordDigest);
    },

    issueCookie(nowMs: number): string {
      return sign(nowMs + ttlMs);
    },

    verifyCookie(value: string | undefined, nowMs: number): boolean {
      if (!value) return false;
      const dot = value.indexOf(".");
      if (dot === -1) return false;
      const expiryStr = value.slice(0, dot);
      const mac = value.slice(dot + 1);
      const expiryMs = Number(expiryStr);
      if (!Number.isInteger(expiryMs)) return false;
      const expected = crypto
        .createHmac("sha256", secret)
        .update(expiryStr)
        .digest("hex");
      // Constant-time signature comparison; both are hex strings of equal length
      // (a length mismatch means a forged/garbage cookie → reject).
      if (mac.length !== expected.length) return false;
      const ok = crypto.timingSafeEqual(
        Buffer.from(mac),
        Buffer.from(expected),
      );
      if (!ok) return false;
      return expiryMs > nowMs;
    },
  };
}

/** Parse a `Cookie:` header into a name→value map (minimal, no dependency). */
export function parseCookies(
  header: string | undefined,
): Record<string, string> {
  const out: Record<string, string> = {};
  if (!header) return out;
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq === -1) continue;
    const name = part.slice(0, eq).trim();
    const value = part.slice(eq + 1).trim();
    if (name) out[name] = decodeURIComponent(value);
  }
  return out;
}

/** Read the MAX session cookie off a request. */
export function readSessionCookie(
  header: string | undefined,
): string | undefined {
  return parseCookies(header)[SESSION_COOKIE];
}

/**
 * Express middleware enforcing a valid session cookie on every route it guards.
 * Returns `401` (RFC-7807-ish JSON) with no body detail about why, matching the
 * admin API's opaque-rejection posture.
 */
export function requireAuth(auth: Authenticator) {
  return (req: Request, res: Response, next: NextFunction): void => {
    const cookie = readSessionCookie(req.headers.cookie);
    if (auth.verifyCookie(cookie, Date.now())) {
      next();
      return;
    }
    res.status(401).json({ error: "unauthorized" });
  };
}

/**
 * Serialize a `Set-Cookie` value for the session cookie. `HttpOnly` keeps it
 * out of JS; `SameSite=Strict` blocks CSRF; `Secure` is omitted because MAX may
 * be reached over plain HTTP on a LAN (the operator terminates TLS upstream if
 * they want it) — documented in the API surface notes.
 */
export function serializeSessionCookie(
  value: string,
  maxAgeMs: number,
): string {
  const maxAgeSec = Math.floor(maxAgeMs / 1000);
  return (
    `${SESSION_COOKIE}=${encodeURIComponent(value)}; ` +
    `Path=/; HttpOnly; SameSite=Strict; Max-Age=${maxAgeSec}`
  );
}

/** Serialize a `Set-Cookie` value that immediately clears the session cookie. */
export function serializeClearCookie(): string {
  return `${SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0`;
}
