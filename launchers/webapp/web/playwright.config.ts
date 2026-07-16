// Playwright E2E config for the launcher webapp (work item 0014, Test
// Considerations → "E2E (webapp, browser-driven)").
//
// This suite is BROWSER-GATED and deliberately NOT part of `make test`
// (`make test-launchers/webapp` runs only the offline vitest component tests),
// exactly like max/web's Playwright suite. It is run explicitly via
// `npm run test:e2e` against an already-running `baeapi` serving the built
// webapp, and every spec skips gracefully when that stack (or a launchable
// browser) is absent — so it never contributes a broken or failing test to
// the default suite.
//
// Prerequisites to actually run it:
//   1. `npx playwright install --with-deps chromium`
//   2. Build the frontend (`make -C launchers/webapp/web build`) and start
//      `baeapi` against the committed 3-agent fixture:
//        BAE_LAUNCHER_API_CONFIG=e2e/fixtures/bae-app.e2e.toml \
//        BAE_LAUNCHER_WEBAPP_STATIC_DIR=dist \
//        BAE_LAUNCHER_API_ADDR=127.0.0.1:9091 \
//        cargo run --manifest-path ../../api/Cargo.toml --bin baeapi
//      (or any equivalent, e.g. the bae-launcher-webapp image with the same
//      fixture COPY'd in)
//   3. Export the reachable URL if not the default:
//        LAUNCHER_E2E_BASE_URL=http://127.0.0.1:9091
//   4. `npm run test:e2e`
//
// In this repo's CI sandbox chromium cannot launch (no system libraries, no
// root), so CI relies on the offline component tests plus the Rust
// integration suite; this suite is for manual browser verification.

import { defineConfig, devices } from "@playwright/test";

const baseURL = process.env.LAUNCHER_E2E_BASE_URL ?? "http://127.0.0.1:9091";

export default defineConfig({
  testDir: "./e2e",
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
  projects: [{ name: "desktop", use: { ...devices["Desktop Chrome"] } }],
});
