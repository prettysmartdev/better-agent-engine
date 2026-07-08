import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  exploreFilesTool,
  readFileTool,
  writeFileTool,
  type FileToolConfig,
} from "./files.js";
import type { ToolDefinition } from "./tool.js";

// The file tools are the one builtin that needs no Session — they touch the
// local filesystem directly and are validated entirely offline against real
// temp dirs and (for the symlink-escape case) a real symlink.

let root: string;

beforeEach(() => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "bae-files-"));
});

afterEach(() => {
  fs.rmSync(root, { recursive: true, force: true });
});

/** Drive a file tool's (synchronous) handler and parse its JSON-string result. */
function call(tool: ToolDefinition, input: Record<string, unknown>): unknown {
  const raw = tool.handler(input) as string;
  return JSON.parse(raw);
}

/** Was the path rejected with an in-band `path not permitted:` error? */
function isNotPermitted(result: unknown): boolean {
  const err = (result as { error?: string }).error;
  return typeof err === "string" && err.startsWith("path not permitted:");
}

function write(p: string, contents: string): void {
  fs.mkdirSync(path.dirname(p), { recursive: true });
  fs.writeFileSync(p, contents, "utf8");
}

describe("validatePath (via read_file)", () => {
  it("permits an allowed path with an allowed extension", () => {
    const file = path.join(root, "notes.txt");
    write(file, "hello");
    const config: FileToolConfig = {
      allowedDirs: [root],
      allowedExtensions: ["txt"],
    };
    const result = call(readFileTool(config), { path: file }) as {
      content: string;
      error?: string;
    };
    expect(result.content).toBe("hello");
    expect(result.error).toBeUndefined();
  });

  it("rejects a `../` traversal escaping the allowed dir", () => {
    const allowed = path.join(root, "allowed");
    fs.mkdirSync(allowed);
    write(path.join(root, "secret.txt"), "top secret");
    const result = call(readFileTool({ allowedDirs: [allowed] }), {
      path: path.join(allowed, "../secret.txt"),
    });
    expect(isNotPermitted(result)).toBe(true);
  });

  it("rejects a real symlink inside the allowed dir that points outside it", () => {
    const allowed = path.join(root, "allowed");
    const outside = path.join(root, "outside");
    fs.mkdirSync(allowed);
    fs.mkdirSync(outside);
    write(path.join(outside, "secret.txt"), "top secret");
    const link = path.join(allowed, "link.txt");
    fs.symlinkSync(path.join(outside, "secret.txt"), link);
    // Canonicalize-before-check resolves the symlink to `outside/…`, which is
    // not under the allowed root (asserted on behaviour, not the string form).
    const result = call(readFileTool({ allowedDirs: [allowed] }), {
      path: link,
    });
    expect(isNotPermitted(result)).toBe(true);
  });

  it("lets denied_extensions override allowed_extensions", () => {
    write(path.join(root, "app.env"), "SECRET=1");
    write(path.join(root, "app.txt"), "ok");
    const tool = readFileTool({
      allowedDirs: [root],
      allowedExtensions: ["env", "txt"],
      deniedExtensions: ["env"],
    });
    expect(
      isNotPermitted(call(tool, { path: path.join(root, "app.env") })),
    ).toBe(true);
    const ok = call(tool, { path: path.join(root, "app.txt") }) as {
      content: string;
    };
    expect(ok.content).toBe("ok");
  });

  it("lets deny_regex override allow_regex", () => {
    write(path.join(root, "public.txt"), "ok");
    write(path.join(root, "secret.txt"), "no");
    const tool = readFileTool({
      allowedDirs: [root],
      allowRegex: /\.txt$/,
      denyRegex: /secret/,
    });
    expect(
      isNotPermitted(call(tool, { path: path.join(root, "secret.txt") })),
    ).toBe(true);
    const ok = call(tool, { path: path.join(root, "public.txt") }) as {
      content: string;
    };
    expect(ok.content).toBe("ok");
  });

  it("rejects everything when allowed_dirs is empty", () => {
    const file = path.join(root, "f.txt");
    write(file, "hello");
    const result = call(readFileTool({ allowedDirs: [] }), { path: file });
    expect(isNotPermitted(result)).toBe(true);
  });
});

describe("write_file parent-directory guard", () => {
  it("rejects a missing parent by default and succeeds with createParents", () => {
    const target = path.join(root, "newdir", "out.txt");
    const strict = call(writeFileTool({ allowedDirs: [root] }), {
      path: target,
      content: "x",
    });
    expect(isNotPermitted(strict)).toBe(true);
    expect(fs.existsSync(target)).toBe(false);

    const ok = call(
      writeFileTool({ allowedDirs: [root], createParents: true }),
      { path: target, content: "hello" },
    ) as { bytes_written: number };
    expect(ok.bytes_written).toBe(5);
    expect(fs.readFileSync(target, "utf8")).toBe("hello");
  });
});

describe("explore_files / read_file consistency", () => {
  it("only returns paths read_file would also accept (real files, symlink escape)", () => {
    const allowed = path.join(root, "allowed");
    const outside = path.join(root, "outside");
    fs.mkdirSync(allowed);
    fs.mkdirSync(outside);
    write(path.join(allowed, "a.txt"), "a");
    write(path.join(allowed, "sub", "b.txt"), "b");
    write(path.join(outside, "c.txt"), "c");
    fs.symlinkSync(
      path.join(outside, "c.txt"),
      path.join(allowed, "escape.txt"),
    );

    const config: FileToolConfig = { allowedDirs: [allowed] };
    const entries = call(exploreFilesTool(config), {
      path: allowed,
      recursive: true,
    }) as Array<{ path: string; is_dir: boolean }>;
    const rels = entries.map((e) => e.path);

    // The escaping symlink and the outside file never appear.
    expect(rels).toContain("a.txt");
    expect(rels.some((p) => p.endsWith("b.txt"))).toBe(true);
    expect(rels.some((p) => p.includes("escape"))).toBe(false);
    expect(rels.some((p) => p.includes("c.txt"))).toBe(false);

    // Property: every returned path passes read_file's validation under the
    // identical config (a directory yields a read error, never a rejection).
    const reader = readFileTool(config);
    const canonicalRoot = fs.realpathSync(allowed);
    for (const rel of rels) {
      const result = call(reader, { path: path.join(canonicalRoot, rel) });
      expect(isNotPermitted(result)).toBe(false);
    }
  });
});
