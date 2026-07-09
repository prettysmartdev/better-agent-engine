//! Credential resolution for MAX's two secrets: the baesrv admin bearer token
//! (reused verbatim from work item 0004) and MAX's own login password.
//!
//! Both follow the exact `explicit value > file override > default file`
//! precedence baectl establishes for its admin-token resolution. Neither
//! secret is ever logged.

import fs from "node:fs";
import crypto from "node:crypto";

import type { MaxConfig } from "./config.js";

/**
 * The admin key file (and MAX password file) are written by baesrv/MAX with a
 * trailing newline for `cat`-friendliness; readers MUST trim before use — the
 * same contract `admin_auth.rs` and `baectl` document.
 */
function trimToken(raw: string): string {
  return raw.trim();
}

/**
 * Resolve the admin bearer token by precedence:
 *   `BAE_ADMIN_TOKEN` (explicit) > `BAE_ADMIN_KEY_FILE` (read) > default file.
 *
 * This reuses baesrv's bootstrap-admin-key-file mechanism verbatim (work item
 * 0004) rather than inventing a second bootstrap path. The token authenticates
 * every `/admin/v1/*` request MAX makes; the browser never sees it.
 */
export function resolveAdminToken(
  config: MaxConfig,
  readFile: (path: string) => string = defaultReadFile,
): string {
  if (config.adminToken !== undefined && config.adminToken.trim() !== "") {
    return trimToken(config.adminToken);
  }
  const raw = readFile(config.adminKeyFile);
  const token = trimToken(raw);
  if (token === "") {
    throw new Error(
      `admin key file ${config.adminKeyFile} is empty — is baesrv running and ` +
        `has it written its bootstrap admin key?`,
    );
  }
  return token;
}

/** Result of resolving MAX's login password. */
export interface MaxPassword {
  /** The plaintext password MAX compares login attempts against. */
  password: string;
  /** True when this boot self-generated the password (for logging, never the value). */
  generated: boolean;
}

/**
 * Resolve MAX's login password by precedence:
 *   `BAE_MAX_PASSWORD` (explicit) > `BAE_MAX_PASSWORD_FILE` (read) > default file.
 *
 * If none is set on first boot, a random password is generated, written to the
 * default file path with `0600` permissions, and `generated: true` is returned
 * so the caller can log *that* it happened (never the value). There is
 * deliberately no disable flag — MAX's web port is off-loopback and this gate
 * has no escape hatch.
 */
export function resolveMaxPassword(
  config: MaxConfig,
  deps: {
    readFileIfExists?: (path: string) => string | undefined;
    writeSecretFile?: (path: string, contents: string) => void;
    generate?: () => string;
  } = {},
): MaxPassword {
  const readFileIfExists = deps.readFileIfExists ?? defaultReadFileIfExists;
  const writeSecretFile = deps.writeSecretFile ?? defaultWriteSecretFile;
  const generate = deps.generate ?? defaultGeneratePassword;

  if (config.maxPassword !== undefined && config.maxPassword.trim() !== "") {
    return { password: config.maxPassword, generated: false };
  }
  const fromFile = readFileIfExists(config.maxPasswordFile);
  if (fromFile !== undefined && fromFile.trim() !== "") {
    return { password: trimToken(fromFile), generated: false };
  }
  // First boot with nothing configured: self-generate and persist 0600.
  const password = generate();
  writeSecretFile(config.maxPasswordFile, `${password}\n`);
  return { password, generated: true };
}

/**
 * Derive the HMAC cookie-signing secret from the login password. Binding the
 * secret to the password means rotating the password (delete the file +
 * restart, which self-generates a fresh one) invalidates every outstanding
 * session cookie — exactly the documented rotation behavior — while a plain
 * restart with an unchanged password keeps existing logins valid.
 */
export function deriveCookieSecret(password: string): Buffer {
  return crypto
    .createHash("sha256")
    .update("bae-max-cookie-secret-v1")
    .update(password)
    .digest();
}

function defaultReadFile(path: string): string {
  return fs.readFileSync(path, "utf8");
}

function defaultReadFileIfExists(path: string): string | undefined {
  try {
    return fs.readFileSync(path, "utf8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return undefined;
    }
    throw err;
  }
}

function defaultWriteSecretFile(path: string, contents: string): void {
  // 0600 on create; also clamp explicitly in case a stale file exists, mirroring
  // admin_auth.rs's write_key_file posture.
  fs.writeFileSync(path, contents, { mode: 0o600 });
  fs.chmodSync(path, 0o600);
}

function defaultGeneratePassword(): string {
  // 24 bytes → 32-char base64url, ~192 bits, same entropy budget as a client key.
  return crypto.randomBytes(24).toString("base64url");
}
