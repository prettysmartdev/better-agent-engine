import { describe, expect, it } from "vitest";

import { VERSION } from "./index.js";

describe("VERSION", () => {
  it("is a semver string", () => {
    expect(VERSION).toMatch(/^\d+\.\d+\.\d+$/);
  });
});
