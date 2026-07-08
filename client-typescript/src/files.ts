/**
 * Builtin **file tools** — give an agent scoped filesystem access (`read_file`,
 * `write_file`, `explore_files`) behind security constraints the harness
 * developer chooses.
 *
 * Unlike the {@link "./sandbox.js" | sandbox tools}, file tools need **no**
 * {@link Session}: they touch the local filesystem directly, so they are
 * constructed once from a {@link FileToolConfig} and registered through the
 * ordinary pre-connect builder ({@link Harness.registerTool}). They are opt-in —
 * never auto-registered.
 *
 * ## Security model
 *
 * All three tools funnel every requested path through **one** shared
 * `validatePath` so their notions of "permitted" can never drift. The validation
 * order is deliberate and security-critical:
 *
 * 1. **Canonicalize first** — resolve `..` and symlinks *before* any allow/deny
 *    check. Checking an allowlist against the raw, uncanonicalized string is the
 *    classic path-traversal bug (`../../etc/passwd`, or a symlink escaping the
 *    allowed root). Crucially we never lexically collapse `..` before resolving
 *    symlinks (that would erase a symlink component and defeat the check); we let
 *    `fs.realpathSync` resolve symlinks *and* `..` via the OS.
 * 2. The canonical path must sit under one of the canonicalized
 *    {@link FileToolConfig.allowedDirs} (an **empty** list permits *nothing*).
 * 3. {@link FileToolConfig.deniedExtensions} rejects (wins even over an allow).
 * 4. {@link FileToolConfig.allowedExtensions}, if set, must match.
 * 5. {@link FileToolConfig.denyRegex} rejects the filename.
 * 6. {@link FileToolConfig.allowRegex}, if set, must match the filename.
 *
 * {@link writeFileTool} additionally refuses to write under a **missing parent
 * directory** unless {@link FileToolConfig.createParents} is set.
 *
 * ## Why validation failures are tool *results*, not errors
 *
 * A rejected path returns an **in-band, error-shaped tool result**
 * (`{"error": "path not permitted: …"}`), never a thrown error that aborts the
 * loop. A security-boundary rejection is an *expected, foreseeable* model
 * behaviour (the LLM guessing at a path outside its sandbox), not a program bug —
 * so the model should see it as tool output it can read and retry from, mirroring
 * how the server turns MCP/sandbox failures into error-shaped `tool.result`s
 * rather than aborting the turn. This is the one place a builtin tool
 * deliberately catches-and-wraps rather than propagates.
 */

import * as fs from "node:fs";
import * as path from "node:path";

import type { Content } from "./types.js";
import type { ToolDefinition } from "./tool.js";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/** The security constraints a harness developer chooses for a set of file tools. */
export interface FileToolConfig {
  /**
   * Only paths that canonicalize to somewhere under one of these directories are
   * permitted. **Required** — an empty list permits *nothing* (an explicit,
   * developer-visible "nothing is allowed" rather than an implicit
   * filesystem-wide default).
   */
  allowedDirs: string[];
  /** If set, only these extensions (without the leading dot, case-insensitive) are permitted. */
  allowedExtensions?: string[];
  /**
   * These extensions are always rejected, even if `allowedExtensions` would
   * otherwise permit them (e.g. block `.env` while allowing everything else).
   */
  deniedExtensions?: string[];
  /** If set, the path's filename must match this regex. */
  allowRegex?: RegExp;
  /** If set, a filename matching this regex is always rejected. */
  denyRegex?: RegExp;
  /**
   * Whether {@link writeFileTool} may create missing parent directories.
   * Defaults to `false`: a write under a non-existent parent is rejected.
   */
  createParents?: boolean;
}

// ---------------------------------------------------------------------------
// Shared path validation (the single source of truth for all three tools)
// ---------------------------------------------------------------------------

/** A permitted path: its canonical form plus the matched `allowedDirs` root. */
interface Resolved {
  canonical: string;
  root: string;
}

/** A rejection reason from `validatePath`. */
interface Rejected {
  error: string;
}

function isRejected(r: Resolved | Rejected): r is Rejected {
  return (r as Rejected).error !== undefined;
}

/**
 * Canonicalize `p`, resolving `..` and symlinks. Unlike a bare
 * `fs.realpathSync` this also works for a **not-yet-existing** leaf (needed by
 * `writeFile`): it realpaths the longest existing prefix — so every symlink is
 * resolved — and appends the not-yet-created tail literally. We deliberately do
 * *not* `path.resolve`/normalize the input first, since collapsing `..`
 * lexically would erase a symlink component before it is resolved.
 */
function resolveCanonical(p: string): string {
  // Make absolute without lexical normalization (preserve `..` for realpath).
  const absolute = path.isAbsolute(p) ? p : `${process.cwd()}${path.sep}${p}`;
  try {
    return fs.realpathSync(absolute);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== "ENOENT") {
      throw err;
    }
    const parent = path.dirname(absolute);
    const base = path.basename(absolute);
    if (parent === absolute || base === "" || base === "." || base === "..") {
      throw err;
    }
    return path.join(resolveCanonical(parent), base);
  }
}

/**
 * Extension of `p`'s file name, lowercased, or "" if none. Unlike
 * `path.extname` this treats a **dotfile**'s suffix as its extension
 * (`.env` → `env`), so a `deniedExtensions: ["env"]` constraint blocks `.env`
 * exactly as the file-tools guide's worked example promises.
 */
function extensionOf(p: string): string {
  const name = path.basename(p);
  const i = name.lastIndexOf(".");
  return i === -1 || i === name.length - 1
    ? ""
    : name.slice(i + 1).toLowerCase();
}

/** Normalise a developer-supplied extension (`".env"` or `"env"`) for matching. */
function normExt(ext: string): string {
  return ext.replace(/^\./, "").toLowerCase();
}

/** Whether `canonical` is at or under the canonical directory `root`. */
function underRoot(canonical: string, root: string): boolean {
  return canonical === root || canonical.startsWith(root + path.sep);
}

/**
 * Validate `requested` against `config`, returning its resolved canonical form
 * or a `{ error }` rejection. The single choke point every file tool shares (see
 * the module docs for the ordered rules).
 */
function validatePath(
  config: FileToolConfig,
  requested: string,
): Resolved | Rejected {
  let canonical: string;
  try {
    canonical = resolveCanonical(requested);
  } catch (err) {
    return {
      error: `cannot resolve \`${requested}\`: ${(err as Error).message}`,
    };
  }

  // (1) Prefix-match against the canonicalized allowedDirs (empty ⇒ nothing).
  let root: string | undefined;
  for (const dir of config.allowedDirs) {
    let cdir: string;
    try {
      cdir = fs.realpathSync(dir);
    } catch {
      continue;
    }
    if (underRoot(canonical, cdir)) {
      root = cdir;
      break;
    }
  }
  if (root === undefined) {
    return { error: `\`${requested}\` is not under any allowed directory` };
  }

  const fileName = path.basename(canonical);
  const ext = extensionOf(canonical);

  // (2) deniedExtensions — always wins.
  if (
    ext !== "" &&
    (config.deniedExtensions ?? []).some((d) => normExt(d) === ext)
  ) {
    return { error: `extension \`.${ext}\` is denied` };
  }

  // (3) allowedExtensions must match if set.
  if (config.allowedExtensions !== undefined) {
    const ok =
      ext !== "" && config.allowedExtensions.some((a) => normExt(a) === ext);
    if (!ok) {
      return { error: "extension is not in the allowed set" };
    }
  }

  // (4) denyRegex rejects the filename.
  if (config.denyRegex !== undefined && config.denyRegex.test(fileName)) {
    return { error: "filename matches the deny pattern" };
  }

  // (5) allowRegex must match the filename if set.
  if (config.allowRegex !== undefined && !config.allowRegex.test(fileName)) {
    return { error: "filename does not match the allow pattern" };
  }

  return { canonical, root };
}

// ---------------------------------------------------------------------------
// Result shaping (every tool result is a JSON *string* content block)
// ---------------------------------------------------------------------------

/**
 * An in-band, error-shaped result: the JSON string `{"error": <reason>}`. A path
 * rejection is prefixed `path not permitted:` so the model can tell a security
 * rejection from an I/O failure.
 */
function errorResult(reason: string): Content {
  return JSON.stringify({ error: reason });
}

/** A success result: `value` serialized to a JSON string (a plain-string block). */
function okResult(value: unknown): Content {
  return JSON.stringify(value);
}

// ---------------------------------------------------------------------------
// Tool constructors
// ---------------------------------------------------------------------------

function pathInputSchema(
  extra: Record<string, unknown>,
): Record<string, unknown> {
  return {
    path: {
      type: "string",
      description:
        "Filesystem path, validated against the tool's security constraints.",
    },
    ...extra,
  };
}

/**
 * A tool that reads a UTF-8 text file at `path`, if `config` permits it. On a
 * permitted, readable path the result is the JSON string `{"path", "content"}`;
 * a rejected path or read error is an error-shaped result (see the module docs).
 */
export function readFileTool(config: FileToolConfig): ToolDefinition {
  return {
    name: "read_file",
    description:
      "Read a UTF-8 text file. The path must satisfy the tool's configured " +
      "directory/extension/filename constraints; a disallowed path returns an error result.",
    input_schema: {
      type: "object",
      properties: pathInputSchema({}),
      required: ["path"],
      additionalProperties: false,
    },
    handler: (input): Content => {
      const requested = input.path;
      if (typeof requested !== "string") {
        return errorResult("read_file requires a string `path`");
      }
      const resolved = validatePath(config, requested);
      if (isRejected(resolved)) {
        return errorResult(`path not permitted: ${resolved.error}`);
      }
      try {
        const content = fs.readFileSync(resolved.canonical, "utf8");
        return okResult({ path: requested, content });
      } catch (err) {
        return errorResult(`read failed: ${(err as Error).message}`);
      }
    },
  };
}

/**
 * A tool that writes `content` to a file at `path`, if `config` permits it.
 * Refuses a missing parent directory unless {@link FileToolConfig.createParents}
 * is set. On success the result is the JSON string `{"path", "bytes_written"}`.
 */
export function writeFileTool(config: FileToolConfig): ToolDefinition {
  return {
    name: "write_file",
    description:
      "Write a UTF-8 text file. The path must satisfy the tool's configured constraints, and " +
      "(unless createParents is enabled) its parent directory must already exist.",
    input_schema: {
      type: "object",
      properties: pathInputSchema({
        content: { type: "string", description: "The UTF-8 text to write." },
      }),
      required: ["path", "content"],
      additionalProperties: false,
    },
    handler: (input): Content => {
      const requested = input.path;
      if (typeof requested !== "string") {
        return errorResult("write_file requires a string `path`");
      }
      const content = typeof input.content === "string" ? input.content : "";
      const resolved = validatePath(config, requested);
      if (isRejected(resolved)) {
        return errorResult(`path not permitted: ${resolved.error}`);
      }
      const parent = path.dirname(resolved.canonical);
      if (!fs.existsSync(parent)) {
        if (config.createParents === true) {
          try {
            fs.mkdirSync(parent, { recursive: true });
          } catch (err) {
            return errorResult(
              `could not create parents: ${(err as Error).message}`,
            );
          }
        } else {
          return errorResult(
            "path not permitted: parent directory does not exist (enable createParents to allow)",
          );
        }
      }
      try {
        fs.writeFileSync(resolved.canonical, content, "utf8");
        return okResult({
          path: requested,
          bytes_written: Buffer.byteLength(content, "utf8"),
        });
      } catch (err) {
        return errorResult(`write failed: ${(err as Error).message}`);
      }
    },
  };
}

interface FileEntry {
  path: string;
  is_dir: boolean;
  size_bytes: number;
}

/**
 * A tool that lists the files under a permitted directory, **non-recursive by
 * default** (`{"recursive": true}` descends). Every discovered entry is itself
 * filtered through `validatePath`, so the listing can never surface a path
 * {@link readFileTool} would reject. The result is a JSON string of an array of
 * `{"path", "is_dir", "size_bytes"}` entries, each `path` relative to the matched
 * `allowedDirs` root.
 */
export function exploreFilesTool(config: FileToolConfig): ToolDefinition {
  return {
    name: "explore_files",
    description:
      "List files under a permitted directory (non-recursive unless recursive=true). Only " +
      "entries that satisfy the same constraints as read_file are returned.",
    input_schema: {
      type: "object",
      properties: pathInputSchema({
        recursive: {
          type: "boolean",
          description: "Descend into subdirectories (default false).",
        },
      }),
      required: ["path"],
      additionalProperties: false,
    },
    handler: (input): Content => {
      const requested = input.path;
      if (typeof requested !== "string") {
        return errorResult("explore_files requires a string `path`");
      }
      const recursive = input.recursive === true;
      const resolved = validatePath(config, requested);
      if (isRejected(resolved)) {
        return errorResult(`path not permitted: ${resolved.error}`);
      }
      if (!fs.statSync(resolved.canonical).isDirectory()) {
        return errorResult(`\`${requested}\` is not a directory`);
      }
      const entries: FileEntry[] = [];
      walk(resolved.canonical, resolved.root, config, recursive, entries);
      // Sort by relative path so the listing is deterministic and byte-identical
      // across the Rust/TS/Python SDKs (each engine's directory-read order differs).
      entries.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
      return okResult(entries);
    },
  };
}

/**
 * Depth-first walk emitting one entry per discovered path that passes
 * `validatePath`. Descent is gated on the canonical child still living under
 * `root`, so a symlinked subdirectory pointing outside the allowed tree is
 * neither listed nor followed.
 */
function walk(
  dir: string,
  root: string,
  config: FileToolConfig,
  recursive: boolean,
  out: FileEntry[],
): void {
  let names: string[];
  try {
    names = fs.readdirSync(dir);
  } catch {
    return;
  }
  for (const name of names) {
    let canonical: string;
    try {
      // Canonicalize each child so a symlink escaping `root` is caught here and
      // never listed or descended into.
      canonical = fs.realpathSync(path.join(dir, name));
    } catch {
      continue;
    }
    if (!underRoot(canonical, root)) {
      continue;
    }
    const stat = fs.statSync(canonical);
    const isDir = stat.isDirectory();
    // Emit only entries a subsequent read_file would also accept.
    if (!isRejected(validatePath(config, canonical))) {
      out.push({
        path: path.relative(root, canonical),
        is_dir: isDir,
        size_bytes: isDir ? 0 : stat.size,
      });
    }
    if (isDir && recursive) {
      walk(canonical, root, config, recursive, out);
    }
  }
}
