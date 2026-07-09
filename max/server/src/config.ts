const DEFAULT_ADDR = "0.0.0.0:3000";

/** Default loopback admin-port address inside the bae-max container. */
const DEFAULT_ADMIN_ADDR = "127.0.0.1:8081";
/** Default loopback client-port address inside the bae-max container. */
const DEFAULT_CLIENT_ADDR = "127.0.0.1:8080";
/** Bootstrap admin-key file written by baesrv (work item 0004). */
const DEFAULT_ADMIN_KEY_FILE = "/var/lib/bae/admin-key.pem";
/** Persisted per-profile observer client keys minted by MAX. */
const DEFAULT_OBSERVER_KEYS_FILE = "/var/lib/bae/max-observer-keys.json";
/** MAX's own login password (self-generated on first boot if unset). */
const DEFAULT_MAX_PASSWORD_FILE = "/var/lib/bae/max-password.pem";

export interface MaxConfig {
  /** Bind host for MAX's HTTP+WS surface (`BAE_MAX_ADDR`). */
  host: string;
  /** Bind port for MAX's HTTP+WS surface (`BAE_MAX_ADDR`). */
  port: number;
  /** Directory holding the built `web/dist` static assets. */
  webDist: string;
  /** `host:port` of baesrv's loopback admin port (`BAE_ADMIN_ADDR`). */
  adminAddr: string;
  /** `host:port` of baesrv's loopback client port for the observer bridge (`BAE_CLIENT_ADDR`). */
  clientAddr: string;
  /** Explicit admin bearer token, highest precedence (`BAE_ADMIN_TOKEN`). */
  adminToken: string | undefined;
  /** Plaintext admin-key file to read when no explicit token (`BAE_ADMIN_KEY_FILE`). */
  adminKeyFile: string;
  /** Where per-profile observer keys are persisted (`BAE_MAX_OBSERVER_KEYS_FILE`). */
  observerKeysFile: string;
  /** Explicit MAX login password, highest precedence (`BAE_MAX_PASSWORD`). */
  maxPassword: string | undefined;
  /** MAX password file (read, or self-generated on first boot) (`BAE_MAX_PASSWORD_FILE`). */
  maxPasswordFile: string;
}

/**
 * Parses MAX's environment into a {@link MaxConfig}.
 *
 * This is deliberately pure — it only reads environment strings and never
 * touches the filesystem. Reading/generating credentials (admin token, MAX
 * password) is a separate, side-effecting step ({@link ./credentials}) run at
 * startup so config parsing stays trivially unit-testable.
 */
export function loadConfig(
  env: NodeJS.ProcessEnv,
  defaultWebDist: string,
): MaxConfig {
  const addr = env.BAE_MAX_ADDR ?? DEFAULT_ADDR;
  const sep = addr.lastIndexOf(":");
  if (sep === -1) {
    throw new Error(
      `BAE_MAX_ADDR must be of the form host:port, got "${addr}"`,
    );
  }
  const host = addr.slice(0, sep);
  const port = Number(addr.slice(sep + 1));
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error(`BAE_MAX_ADDR has an invalid port, got "${addr}"`);
  }
  return {
    host,
    port,
    webDist: env.BAE_MAX_WEB_DIST ?? defaultWebDist,
    adminAddr: env.BAE_ADMIN_ADDR ?? DEFAULT_ADMIN_ADDR,
    clientAddr: env.BAE_CLIENT_ADDR ?? DEFAULT_CLIENT_ADDR,
    adminToken: env.BAE_ADMIN_TOKEN,
    adminKeyFile: env.BAE_ADMIN_KEY_FILE ?? DEFAULT_ADMIN_KEY_FILE,
    observerKeysFile:
      env.BAE_MAX_OBSERVER_KEYS_FILE ?? DEFAULT_OBSERVER_KEYS_FILE,
    maxPassword: env.BAE_MAX_PASSWORD,
    maxPasswordFile: env.BAE_MAX_PASSWORD_FILE ?? DEFAULT_MAX_PASSWORD_FILE,
  };
}
