import { describe, expect, it } from "vitest";
import {
  resolveAdminToken,
  resolveMaxPassword,
  deriveCookieSecret,
} from "./credentials.js";
import type { MaxConfig } from "./config.js";

function config(overrides: Partial<MaxConfig> = {}): MaxConfig {
  return {
    host: "0.0.0.0",
    port: 3000,
    webDist: "/web",
    adminAddr: "127.0.0.1:8081",
    clientAddr: "127.0.0.1:8080",
    adminToken: undefined,
    adminKeyFile: "/var/lib/bae/admin-key.pem",
    observerKeysFile: "/var/lib/bae/max-observer-keys.json",
    maxPassword: undefined,
    maxPasswordFile: "/var/lib/bae/max-password.pem",
    ...overrides,
  };
}

describe("resolveAdminToken", () => {
  it("prefers an explicit BAE_ADMIN_TOKEN over the file", () => {
    const token = resolveAdminToken(config({ adminToken: "explicit" }), () => {
      throw new Error("file should not be read");
    });
    expect(token).toBe("explicit");
  });

  it("falls back to reading (and trimming) the key file", () => {
    const token = resolveAdminToken(config(), () => "bae_fromfile\n");
    expect(token).toBe("bae_fromfile");
  });

  it("throws when the key file is empty", () => {
    expect(() => resolveAdminToken(config(), () => "  \n")).toThrow();
  });
});

describe("resolveMaxPassword", () => {
  it("prefers an explicit BAE_MAX_PASSWORD", () => {
    const result = resolveMaxPassword(config({ maxPassword: "explicit-pw" }), {
      readFileIfExists: () => {
        throw new Error("should not read file");
      },
    });
    expect(result).toEqual({ password: "explicit-pw", generated: false });
  });

  it("reads (and trims) the password file when present", () => {
    const result = resolveMaxPassword(config(), {
      readFileIfExists: () => "file-pw\n",
    });
    expect(result).toEqual({ password: "file-pw", generated: false });
  });

  it("self-generates and persists 0600 on first boot when nothing is set", () => {
    const writes: Array<{ path: string; contents: string }> = [];
    const result = resolveMaxPassword(config(), {
      readFileIfExists: () => undefined,
      writeSecretFile: (path, contents) => writes.push({ path, contents }),
      generate: () => "generated-pw",
    });
    expect(result).toEqual({ password: "generated-pw", generated: true });
    expect(writes).toEqual([
      { path: "/var/lib/bae/max-password.pem", contents: "generated-pw\n" },
    ]);
  });
});

describe("deriveCookieSecret", () => {
  it("is deterministic per password and differs across passwords", () => {
    expect(deriveCookieSecret("a")).toEqual(deriveCookieSecret("a"));
    expect(deriveCookieSecret("a").equals(deriveCookieSecret("b"))).toBe(false);
    expect(deriveCookieSecret("a")).toHaveLength(32);
  });
});
