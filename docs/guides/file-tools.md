# File Tools

Every client SDK ships three builtin tools — `read_file`, `write_file`, and
`explore_files` — that give an agent scoped filesystem access. Unlike the
[sandbox tools](sandboxes.md), file tools touch the local filesystem directly
and need no live `Session`: they are built once from a security-constraint
config and registered through the ordinary pre-`connect()` tool builder, the
same as any other client-side tool. This guide walks through the constraint
builder with a worked example, the validation order, and the in-band-error
convention every builtin tool in this SDK family follows for security
rejections.

---

## The security-constraint builder

A harness developer must **choose** the constraints for a set of file tools
— there is no default that permits "the whole filesystem." Each SDK exposes
the same shape:

**Rust** — `FileToolConfig`, built with `FileToolConfig::new(allowed_dirs)`
and builder setters:

```rust
use bae_rs::files::{read_file_tool, write_file_tool, explore_files_tool, FileToolConfig};

let config = FileToolConfig::new(["./workspace"])
    .denied_extensions(["env"]);

let harness = harness
    .with_tool(read_file_tool(config.clone()))
    .with_tool(write_file_tool(config.clone()))
    .with_tool(explore_files_tool(config));
```

**TypeScript** — `FileToolConfig` object literal:

```typescript
import { readFileTool, writeFileTool, exploreFilesTool, FileToolConfig } from "@prettysmartdev/bae-ts";

const config: FileToolConfig = {
  allowedDirs: ["./workspace"],
  deniedExtensions: ["env"],
};

harness.registerTool(readFileTool(config));
harness.registerTool(writeFileTool(config));
harness.registerTool(exploreFilesTool(config));
```

**Python** — `FileToolConfig` dataclass:

```python
from bae_py.files import read_file_tool, write_file_tool, explore_files_tool, FileToolConfig

config = FileToolConfig(allowed_dirs=["./workspace"], denied_extensions=["env"])

harness.register_tool(read_file_tool(config))
harness.register_tool(write_file_tool(config))
harness.register_tool(explore_files_tool(config))
```

### Worked example: restrict to `./workspace`, deny `.env`

The example above is deliberately the canonical one: an agent that should
only ever touch files under a project's `./workspace` directory, and must
never read or write dotfiles that hold secrets (`.env`, `.env.local`, …).

Given that config:

| Requested path | Result |
|---|---|
| `./workspace/notes.txt` | permitted — under `allowed_dirs`, extension not denied |
| `./workspace/.env` | **rejected** — `denied_extensions` matches `env` regardless of the directory being otherwise permitted |
| `./workspace/../secrets/keys.txt` | **rejected** — canonicalizes to `./secrets/keys.txt`, outside `allowed_dirs` |
| `/etc/passwd` | **rejected** — outside `allowed_dirs` |
| `./workspace/subdir/data.json` | permitted — nested under an allowed dir is fine, `explore_files` finds it too |

`allowed_dirs` is **required** — passing an empty list is not "allow
everything," it is the opposite: nothing is permitted. This makes "an agent
with no file access" an explicit, visible configuration rather than an
accident of an unset default.

### Full `FileToolConfig` field reference

| Field | Default | Effect |
|---|---|---|
| `allowed_dirs` | _(required)_ | Only paths that canonicalize to somewhere under one of these directories are permitted. Empty ⇒ nothing permitted. |
| `allowed_extensions` | unset (any extension) | If set, only these extensions (no leading dot, case-insensitive) are permitted. |
| `denied_extensions` | `[]` | These extensions are always rejected, **even if `allowed_extensions` would otherwise permit them** — e.g. deny `env` while allowing everything else. |
| `allow_regex` | unset (any filename) | If set, the filename must match. |
| `deny_regex` | unset | If set, a filename match is always rejected — wins over `allow_regex`. |
| `create_parents` | `false` | Whether `write_file` may create a missing parent directory. |

A leading-dot extension like `.env` is treated as the extension `env` for
matching purposes, so `denied_extensions: ["env"]` is exactly what blocks
`.env` files (there's no separate "dotfile" concept to configure).

---

## Validation ordering

All three tools funnel every requested path through **one shared, private**
`validate_path` function per SDK, so the three tools' notions of "permitted"
can never drift apart. The order is deliberate and security-critical:

1. **Canonicalize first** — resolve `..` and symlinks *via the OS* before any
   allow/deny check runs. Checking an allowlist against the raw,
   uncanonicalized string is the classic path-traversal bug
   (`../../etc/passwd`, or a symlink inside an allowed directory that points
   outside it) — canonicalizing first, not lexically collapsing `..`
   first, closes both holes. (For `write_file`'s not-yet-existing leaf, the
   longest existing prefix is canonicalized and the remaining tail appended
   literally.)
2. The canonical path must sit under one of the canonicalized `allowed_dirs`.
3. `denied_extensions` — reject (always wins, even over an otherwise-matching
   `allowed_extensions`).
4. `allowed_extensions` — if set, must match.
5. `deny_regex` — reject a filename match (wins over `allow_regex`).
6. `allow_regex` — if set, the filename must match.

`write_file` layers one more check on top: it refuses to write under a
**missing parent directory** unless `create_parents` is explicitly enabled —
there is no implicit `mkdir -p` that could stage a file somewhere unexpected,
even if that "somewhere" is technically still inside an allowed root.

`explore_files` (non-recursive by default; pass `recursive: true` to
descend) re-runs this exact same `validate_path` on every discovered entry
before including it in the listing — so a directory listing can never
surface a path that a subsequent `read_file` call would then reject. A
symlinked child that escapes its `allowed_dirs` root is simply skipped from
the listing, not included with an error.

---

## The in-band-error convention

**A validation failure is returned as a normal, successful tool result —
never a thrown exception, a returned `Err`, or anything that aborts the
turn.** Each SDK renders it as a JSON-string content block:

```json
{"error": "path not permitted: outside allowed_dirs"}
```

Read/write I/O failures (file not found, permission denied, …) use the same
shape (`{"error": "read failed: …"}`). This is a deliberate, spec-level
choice, not an oversight: a security-boundary rejection is an **expected,
foreseeable** model behavior — the LLM guessing at a path outside its
sandbox — not a program bug. The model should see the rejection as tool
output it can read and react to (try a different path, ask the user, give
up gracefully), exactly the same way the server turns a failed MCP or
sandbox call into an error-shaped `tool.result` rather than aborting the
turn (see [Sandboxes](sandboxes.md) and
[Message Types — `tool.result`](../reference/message-types.md#toolresult)).

This is the **one place** a builtin tool constructor deliberately
catches-and-wraps rather than propagates. If you are writing your own custom
tool, following this same pattern for genuinely expected/foreseeable
failures — as opposed to real bugs — is worth doing on purpose, not by
accident.

### Success shapes

| Tool | Input | Success result |
|---|---|---|
| `read_file` | `{"path"}` | `{"path", "content"}` (UTF-8 text) |
| `write_file` | `{"path", "content"}` | `{"path", "bytes_written"}` (refuses a missing parent dir unless `create_parents`) |
| `explore_files` | `{"path", "recursive"?}` | JSON array of `{"path", "is_dir", "size_bytes"}`, each `path` relative to the matched `allowed_dirs` root, sorted |

All three constructors are pure functions of a `FileToolConfig` — no
`Session`, no network, no state beyond the config itself — so they can be
built and registered on a `Harness` before `connect()`, just like any other
client-side tool, and are never auto-registered.
