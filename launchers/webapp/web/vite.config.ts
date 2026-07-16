/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["./src/setupTests.ts"],
    // Component/unit tests live under src/. The browser-driven Playwright E2E
    // suite (e2e/, run via `npm run test:e2e`) must never be collected by
    // vitest — it uses the @playwright/test runner, not vitest.
    include: ["src/**/*.{test,spec}.{ts,tsx}"],
    exclude: ["e2e/**", "node_modules/**", "dist/**"],
  },
});
