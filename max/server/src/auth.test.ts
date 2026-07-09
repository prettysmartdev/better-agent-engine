import { describe, expect, it } from "vitest";
import {
  createAuthenticator,
  parseCookies,
  readSessionCookie,
  serializeSessionCookie,
  serializeClearCookie,
  SESSION_COOKIE,
} from "./auth.js";
import { deriveCookieSecret } from "./credentials.js";

function makeAuth(password = "hunter2") {
  return createAuthenticator(password, deriveCookieSecret(password));
}

describe("createAuthenticator", () => {
  it("accepts the correct password and rejects a wrong one", () => {
    const auth = makeAuth("s3cret");
    expect(auth.verifyPassword("s3cret")).toBe(true);
    expect(auth.verifyPassword("s3cre")).toBe(false);
    expect(auth.verifyPassword("s3cret ")).toBe(false);
    expect(auth.verifyPassword("")).toBe(false);
  });

  it("issues a cookie that then verifies before expiry", () => {
    const auth = makeAuth();
    const now = 1_000_000;
    const cookie = auth.issueCookie(now);
    expect(auth.verifyCookie(cookie, now + 1000)).toBe(true);
  });

  it("rejects an expired cookie", () => {
    const auth = makeAuth();
    const now = 1_000_000;
    const cookie = auth.issueCookie(now);
    expect(auth.verifyCookie(cookie, now + auth.ttlMs + 1)).toBe(false);
  });

  it("rejects a tampered signature", () => {
    const auth = makeAuth();
    const now = 1_000_000;
    const cookie = auth.issueCookie(now);
    const [expiry, mac] = cookie.split(".");
    // Flip the last hex char of the MAC.
    const flipped = mac!.slice(0, -1) + (mac!.slice(-1) === "0" ? "1" : "0");
    expect(auth.verifyCookie(`${expiry}.${flipped}`, now)).toBe(false);
  });

  it("rejects a cookie signed with a different password's secret", () => {
    const a = makeAuth("password-a");
    const b = makeAuth("password-b");
    const cookie = a.issueCookie(1000);
    expect(b.verifyCookie(cookie, 2000)).toBe(false);
  });

  it("rejects malformed cookies", () => {
    const auth = makeAuth();
    expect(auth.verifyCookie(undefined, 0)).toBe(false);
    expect(auth.verifyCookie("", 0)).toBe(false);
    expect(auth.verifyCookie("no-dot", 0)).toBe(false);
    expect(auth.verifyCookie("notanumber.deadbeef", 0)).toBe(false);
  });
});

describe("cookie helpers", () => {
  it("parses a Cookie header into a map", () => {
    expect(parseCookies("a=1; b=2; c=hello%20world")).toEqual({
      a: "1",
      b: "2",
      c: "hello world",
    });
    expect(parseCookies(undefined)).toEqual({});
  });

  it("reads the session cookie by name", () => {
    const header = `other=x; ${SESSION_COOKIE}=abc.def; y=z`;
    expect(readSessionCookie(header)).toBe("abc.def");
    expect(readSessionCookie("other=x")).toBeUndefined();
  });

  it("serializes a HttpOnly SameSite=Strict session cookie", () => {
    const s = serializeSessionCookie("v.mac", 60_000);
    expect(s).toContain(`${SESSION_COOKIE}=v.mac`);
    expect(s).toContain("HttpOnly");
    expect(s).toContain("SameSite=Strict");
    expect(s).toContain("Max-Age=60");
  });

  it("serializes a clearing cookie with Max-Age=0", () => {
    expect(serializeClearCookie()).toContain("Max-Age=0");
  });
});
