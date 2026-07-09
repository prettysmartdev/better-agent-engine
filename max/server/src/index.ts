import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import http from "node:http";
import type { Duplex } from "node:stream";

import { WebSocketServer } from "ws";

import { createApp } from "./app.js";
import { loadConfig, type MaxConfig } from "./config.js";
import {
  resolveAdminToken,
  resolveMaxPassword,
  deriveCookieSecret,
} from "./credentials.js";
import { AdminClient } from "./adminClient.js";
import { ClientPortClient } from "./clientPortClient.js";
import { fileKeyStore } from "./observerKeys.js";
import { ObserverKeyProvisioner } from "./observerKeys.js";
import { createAuthenticator, readSessionCookie } from "./auth.js";
import { createApiRouter } from "./routes.js";
import { ObserverBridge } from "./bridge.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// From the compiled dist/index.js this resolves to max/web/dist; override with
// BAE_MAX_WEB_DIST if the deployed layout differs (e.g. inside the bae-max
// container image).
const DEFAULT_WEB_DIST = path.join(__dirname, "..", "..", "web", "dist");

const WS_SESSION_PATH = /^\/ws\/sessions\/([^/]+)$/;

/** A fully-wired MAX server, not yet listening. */
export interface MaxServerHandle {
  /** The HTTP server (with the WS upgrade handler already attached). */
  server: http.Server;
  /** The resolved configuration (bind host/port, credential file paths, …). */
  config: MaxConfig;
  /** The live observer bridge (exposed for tests/diagnostics). */
  bridge: ObserverBridge;
  /** The WebSocket server used for the `/ws/sessions/{id}` surface. */
  wss: WebSocketServer;
}

/**
 * Wire up MAX's full server — credential resolution, the admin/client-port
 * clients, the observer bridge, the Express `/api` surface, static assets, and
 * the cookie-gated `/ws/sessions/{id}` upgrade handler — and return it WITHOUT
 * calling `listen`. `main()` (below) listens on the configured address; tests
 * boot it against a real baesrv on an ephemeral port and drive it directly.
 *
 * `env` and `defaultWebDist` are injectable so a test can point MAX at a
 * temporary data volume and a throwaway static directory.
 */
export function buildMaxServer(
  env: NodeJS.ProcessEnv = process.env,
  defaultWebDist: string = DEFAULT_WEB_DIST,
): MaxServerHandle {
  const config = loadConfig(env, defaultWebDist);

  // Resolve MAX's two secrets at startup. The admin token authenticates every
  // request MAX makes to baesrv's admin port; the MAX password gates MAX's own
  // web surface. Neither is ever logged.
  const adminToken = resolveAdminToken(config);
  const maxPassword = resolveMaxPassword(config);
  if (maxPassword.generated) {
    console.log(
      `max/server: no BAE_MAX_PASSWORD/BAE_MAX_PASSWORD_FILE set — generated a ` +
        `random login password and wrote it (0600) to ${config.maxPasswordFile}. ` +
        `Read it there to log in; delete the file and restart to rotate.`,
    );
  }

  const admin = new AdminClient(config.adminAddr, adminToken);
  const auth = createAuthenticator(
    maxPassword.password,
    deriveCookieSecret(maxPassword.password),
  );
  const observerKeys = new ObserverKeyProvisioner(
    admin,
    fileKeyStore(config.observerKeysFile),
  );
  const clientPort = new ClientPortClient(config.clientAddr);
  const bridge = new ObserverBridge(admin, observerKeys, clientPort);

  const app = createApp({
    webDist: config.webDist,
    api: createApiRouter(admin, auth),
  });
  const server = http.createServer(app);

  // The observer bridge WebSocket surface: `/ws/sessions/{id}`, gated by the
  // same session cookie as every REST route. Everything else is rejected.
  const wss = new WebSocketServer({ noServer: true });

  server.on("upgrade", (req, socket: Duplex, head) => {
    let url: URL;
    try {
      url = new URL(req.url ?? "/", "http://localhost");
    } catch {
      rejectUpgrade(socket, 400, "Bad Request");
      return;
    }
    const match = WS_SESSION_PATH.exec(url.pathname);
    if (!match) {
      rejectUpgrade(socket, 404, "Not Found");
      return;
    }
    const cookie = readSessionCookie(req.headers.cookie);
    if (!auth.verifyCookie(cookie, Date.now())) {
      rejectUpgrade(socket, 401, "Unauthorized");
      return;
    }
    const sessionId = decodeURIComponent(match[1]!);
    const profileIdHint = url.searchParams.get("profile_id") ?? undefined;
    const stateHint = url.searchParams.get("state") ?? undefined;
    wss.handleUpgrade(req, socket, head, (ws) => {
      bridge.handleConnection(sessionId, ws, profileIdHint, stateHint);
    });
  });

  return { server, config, bridge, wss };
}

function rejectUpgrade(socket: Duplex, status: number, reason: string): void {
  socket.write(`HTTP/1.1 ${status} ${reason}\r\n\r\n`);
  socket.destroy();
}

function main(): void {
  const { server, config } = buildMaxServer();
  server.listen(config.port, config.host, () => {
    console.log(`max/server listening on ${config.host}:${config.port}`);
  });
}

// Boot only when executed directly (`node dist/index.js`, the container
// entrypoint) — never when imported by a test, so importing this module has no
// side effects beyond defining `buildMaxServer`.
if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  main();
}
