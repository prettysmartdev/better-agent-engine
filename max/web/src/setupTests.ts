import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

// Vitest globals are disabled, so RTL's automatic per-test cleanup does not
// register itself — do it explicitly, or DOM leaks across tests in a file.
afterEach(() => cleanup());
