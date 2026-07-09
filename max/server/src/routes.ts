//! MAX's browser-facing REST surface (`/api/*`).
//!
//! Every route here is a thin proxy in front of baesrv's admin port: the browser
//! holds only a MAX session cookie, never the admin key. `POST /api/login` is the
//! sole unauthenticated route; everything after `router.use(requireAuth)` demands
//! a valid cookie. Upstream `AdminApiError`s are surfaced with their original
//! status and RFC-7807-ish `{type, detail}` body so the UI can render them.

import express, { type Request, type Response, type Router } from "express";

import {
  AdminApiError,
  type AdminClient,
  type PageParams,
} from "./adminClient.js";
import {
  requireAuth,
  serializeSessionCookie,
  serializeClearCookie,
  type Authenticator,
} from "./auth.js";

/** Build the `/api` router. Mount it at `/api` in {@link ./app}. */
export function createApiRouter(
  admin: AdminClient,
  auth: Authenticator,
): Router {
  const router = express.Router();
  router.use(express.json());

  // --- Unauthenticated: login ------------------------------------------
  router.post("/login", (req: Request, res: Response) => {
    const password = (req.body as { password?: unknown } | undefined)?.password;
    if (typeof password !== "string" || !auth.verifyPassword(password)) {
      // Opaque rejection — never reveals whether the password was close.
      res.status(401).json({ error: "invalid_password" });
      return;
    }
    const cookie = auth.issueCookie(Date.now());
    res.setHeader("set-cookie", serializeSessionCookie(cookie, auth.ttlMs));
    res.json({ ok: true });
  });

  // --- Everything below requires a valid session cookie ----------------
  router.use(requireAuth(auth));

  // A cheap authed probe the SPA hits on load to decide login vs dashboard.
  router.get("/session", (_req, res) => {
    res.json({ authenticated: true });
  });

  router.post("/logout", (_req, res) => {
    res.setHeader("set-cookie", serializeClearCookie());
    res.json({ ok: true });
  });

  // --- Profiles ---------------------------------------------------------
  router.get(
    "/profiles",
    proxy((req) => admin.listProfiles(pageParams(req))),
  );
  router.get(
    "/profiles/:id",
    proxy((req) => admin.getProfile(pathParam(req, "id"))),
  );
  router.post(
    "/profiles",
    proxy((req) => admin.createProfile(req.body), 201),
  );
  router.put(
    "/profiles/:id",
    proxy((req) => admin.replaceProfile(pathParam(req, "id"), req.body)),
  );
  router.delete(
    "/profiles/:id",
    proxy((req) => admin.deleteProfile(pathParam(req, "id"))),
  );

  // --- Keys -------------------------------------------------------------
  router.get(
    "/keys",
    proxy((req) => admin.listKeys(pageParams(req))),
  );
  router.post(
    "/keys",
    proxy((req) => admin.createKey(req.body), 201),
  );
  router.delete(
    "/keys/:id",
    proxy((req) => admin.deleteKey(pathParam(req, "id"))),
  );

  // --- Registries -------------------------------------------------------
  router.get(
    "/providers",
    proxy(() => admin.listProviders()),
  );
  router.get(
    "/mcp-servers",
    proxy(() => admin.listMcpServers()),
  );

  // --- Sessions (read-only) --------------------------------------------
  router.get(
    "/sessions",
    proxy((req) => {
      const state = firstQuery(req.query.state);
      return admin.listSessions({
        ...pageParams(req),
        ...(state ? { state } : {}),
      });
    }),
  );
  router.get(
    "/sessions/:id/events",
    proxy((req) =>
      admin.getSessionEvents(pathParam(req, "id"), pageParams(req)),
    ),
  );

  return router;
}

/** Read a required path parameter as a string (Express types it loosely). */
function pathParam(req: Request, name: string): string {
  const value = (req.params as Record<string, string | string[]>)[name];
  return Array.isArray(value) ? (value[0] ?? "") : (value ?? "");
}

/**
 * Wrap an admin call as an Express handler: await it, translate
 * `AdminApiError`s to the upstream status, and send JSON (or `204` when the
 * upstream returned nothing). `successStatus` overrides the `200` default (e.g.
 * `201` for creates).
 */
function proxy(
  fn: (req: Request) => Promise<unknown>,
  successStatus = 200,
): (req: Request, res: Response) => void {
  return (req, res) => {
    fn(req)
      .then((body) => {
        if (body === undefined) {
          res.status(204).end();
          return;
        }
        res.status(successStatus).json(body);
      })
      .catch((err) => {
        if (err instanceof AdminApiError) {
          res.status(err.status).json({ type: err.type, detail: err.detail });
          return;
        }
        res
          .status(500)
          .json({ type: "internal", detail: (err as Error).message });
      });
  };
}

/** Extract `?cursor=&limit=` as {@link PageParams}. */
function pageParams(req: Request): PageParams {
  const params: PageParams = {};
  const cursor = firstQuery(req.query.cursor);
  if (cursor) params.cursor = cursor;
  const limit = firstQuery(req.query.limit);
  if (limit && Number.isFinite(Number(limit))) params.limit = Number(limit);
  return params;
}

/** Collapse a possibly-array query value to its first string. */
function firstQuery(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  if (Array.isArray(value) && typeof value[0] === "string") return value[0];
  return undefined;
}
