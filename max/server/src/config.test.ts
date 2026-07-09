import { describe, expect, it } from "vitest";
import { loadConfig } from "./config.js";

describe("loadConfig", () => {
  it("defaults to 0.0.0.0:3000", () => {
    const config = loadConfig({}, "/default/web/dist");
    expect(config.host).toBe("0.0.0.0");
    expect(config.port).toBe(3000);
    expect(config.webDist).toBe("/default/web/dist");
  });

  it("parses BAE_MAX_ADDR and BAE_MAX_WEB_DIST overrides", () => {
    const config = loadConfig(
      { BAE_MAX_ADDR: "127.0.0.1:4000", BAE_MAX_WEB_DIST: "/custom/dist" },
      "/default/web/dist",
    );
    expect(config.host).toBe("127.0.0.1");
    expect(config.port).toBe(4000);
    expect(config.webDist).toBe("/custom/dist");
  });

  it("rejects a malformed address", () => {
    expect(() =>
      loadConfig({ BAE_MAX_ADDR: "not-an-addr" }, "/default"),
    ).toThrow();
  });
});
