import { describe, expect, it } from "vitest";
import { NdjsonBuffer } from "./ndjson.js";

describe("NdjsonBuffer", () => {
  it("splits complete lines and retains a partial trailer", () => {
    const buf = new NdjsonBuffer();
    expect(buf.push('{"a":1}\n{"b":2}\n')).toEqual(['{"a":1}', '{"b":2}']);
    expect(buf.push('{"c":')).toEqual([]);
    expect(buf.push("3}\n")).toEqual(['{"c":3}']);
  });

  it("reassembles a line split across chunk boundaries", () => {
    const buf = new NdjsonBuffer();
    expect(buf.push('{"hel')).toEqual([]);
    expect(buf.push('lo":')).toEqual([]);
    expect(buf.push("true}\n")).toEqual(['{"hello":true}']);
  });

  it("drops blank lines", () => {
    const buf = new NdjsonBuffer();
    expect(buf.push("\n\n{}\n\n")).toEqual(["{}"]);
  });

  it("flush returns a non-newline-terminated remainder once", () => {
    const buf = new NdjsonBuffer();
    buf.push("{}\n{tail}");
    expect(buf.flush()).toBe("{tail}");
    expect(buf.flush()).toBeUndefined();
  });
});
