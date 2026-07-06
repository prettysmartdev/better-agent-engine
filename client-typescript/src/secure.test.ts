import { describe, expect, it } from "vitest";

import { constantTimeEqual, randomHex } from "./secure.js";

describe("randomHex", () => {
  it("returns lowercase hex of the requested byte length", () => {
    const hex = randomHex(24);
    expect(hex).toMatch(/^[0-9a-f]{48}$/);
  });

  it("is (overwhelmingly) unique per call", () => {
    expect(randomHex(16)).not.toBe(randomHex(16));
  });
});

describe("constantTimeEqual", () => {
  it("is true for equal strings", () => {
    expect(constantTimeEqual("bae_ses_abc", "bae_ses_abc")).toBe(true);
  });

  it("is false for differing content of equal length", () => {
    expect(constantTimeEqual("abcd", "abce")).toBe(false);
  });

  it("is false for differing lengths (no early return)", () => {
    expect(constantTimeEqual("abc", "abcd")).toBe(false);
    expect(constantTimeEqual("", "x")).toBe(false);
  });

  it("is true for two empty strings", () => {
    expect(constantTimeEqual("", "")).toBe(true);
  });
});
