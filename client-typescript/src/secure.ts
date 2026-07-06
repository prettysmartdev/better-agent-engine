import { randomBytes } from "node:crypto";

/**
 * Security primitives sanctioned by work item 0002. Any randomness in this SDK
 * goes through `crypto.randomBytes` (never `Math.random`), and any secret
 * comparison uses the manual constant-time XOR loop below (never `===`, which
 * short-circuits and leaks length/prefix timing).
 */

/** Cryptographically-random lowercase-hex string of `bytes` bytes (2 chars each). */
export function randomHex(bytes: number): string {
  return randomBytes(bytes).toString("hex");
}

/**
 * Constant-time string equality via an explicit XOR accumulation loop. Runs in
 * time proportional to the longer input and never short-circuits, so it does
 * not leak where two secrets first differ. Length differences are folded into
 * the accumulator rather than returned early.
 */
export function constantTimeEqual(a: string, b: string): boolean {
  const bufA = Buffer.from(a, "utf8");
  const bufB = Buffer.from(b, "utf8");
  const len = Math.max(bufA.length, bufB.length);
  let diff = bufA.length ^ bufB.length;
  for (let i = 0; i < len; i++) {
    // `at`-style reads past the end yield 0; the length XOR above already
    // guarantees a mismatch when the lengths differ.
    const x = i < bufA.length ? bufA[i]! : 0;
    const y = i < bufB.length ? bufB[i]! : 0;
    diff |= x ^ y;
  }
  return diff === 0;
}
