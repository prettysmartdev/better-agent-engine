// Playwright E2E config for the MAX dashboard (work item 0007, Test
// Considerations → "E2E (browser-driven, e.g. Playwright)").
//
// This suite is BROWSER-GATED and deliberately NOT part of `make test`
// (`make test-max` runs only the offline vitest component/unit + server
// integration suites). It is run explicitly via `npm run test:e2e` against a
// already-running bae-max stack, and every spec skips gracefully when that
// stack (or a launchable browser) is absent — so it never contributes a broken
// or failing test to the default suite.
//
// Prerequisites to actually run it (see docs/test-plan for the full walkthrough):
//   1. `npx playwright install --with-deps chromium`
//   2. Start a bae-max stack (baesrv + max/server serving web/dist), e.g. via
//      `make image-max` + `docker run`, or locally boot baesrv and
//      `node max/server/dist/index.js`.
//   3. Export the reachable URL + MAX password:
//        MAX_E2E_BASE_URL=http://127.0.0.1:3000
//        MAX_E2E_PASSWORD=<the value from /var/lib/bae/max-password.pem>
//        BAE_CLIENT_ADDR=127.0.0.1:8080   # for the scripted driver in the graph spec
//   4. `npm run test:e2e`
//
// In this repo's CI sandbox chromium cannot launch (no system libraries, no
// root), so the suite is documented with manual verification steps instead.

import { defineConfig, devices } from "@playwright/test";

const baseURL = process.env.MAX_E2E_BASE_URL ?? "http://127.0.0.1:3000";

export default defineConfig({
  testDir: "./e2e",
  // Fail fast rather than hang if the stack is up but misbehaving.
  timeout: 30_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL,
    trace: "on-first-retry",
  },
  // Desktop, tablet, and mobile breakpoints — the responsive smoke-check
  // required by the work item's "mobile and tablet friendly" mandate.
  projects: [
    { name: "desktop", use: { ...devices["Desktop Chrome"] } },
    { name: "tablet", use: { ...devices["iPad (gen 7)"] } },
    { name: "mobile", use: { ...devices["Pixel 5"] } },
  ],
});
