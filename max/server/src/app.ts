import path from "node:path";
import express, { type Express, type Router } from "express";

/** Options for {@link createApp}. */
export interface AppOptions {
  /** Directory holding the built `web/dist` static assets. */
  webDist: string;
  /** MAX's `/api` router (login + authenticated admin proxy). Optional so the
   *  health/static surface can be tested in isolation. */
  api?: Router;
}

/**
 * Builds the Express app: an unauthenticated health route, MAX's `/api` surface
 * (which self-gates every route but `/api/login` behind the session cookie), and
 * the built web/ frontend served as static files with an SPA fallback.
 *
 * The static assets are intentionally unauthenticated — they are just the login
 * page's shell; every capability lives behind `/api` (cookie-gated) and the WS
 * upgrade (cookie-gated in {@link ./index}).
 */
export function createApp(options: AppOptions): Express {
  const app = express();
  app.disable("x-powered-by");

  app.get("/healthz", (_req, res) => {
    res.json({ status: "ok" });
  });

  if (options.api) {
    app.use("/api", options.api);
  }

  app.use(express.static(options.webDist));
  app.use((req, res, next) => {
    if (req.method !== "GET") {
      next();
      return;
    }
    res.sendFile(path.join(options.webDist, "index.html"));
  });

  return app;
}
