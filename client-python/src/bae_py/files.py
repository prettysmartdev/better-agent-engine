"""Builtin **file tools** — give an agent scoped filesystem access (``read_file``,
``write_file``, ``explore_files``) behind security constraints the harness
developer chooses.

Unlike the :mod:`sandbox tools <bae_py.sandbox>`, file tools need **no**
:class:`~bae_py.harness.Session`: they touch the local filesystem directly, so
they are constructed once from a :class:`FileToolConfig` and registered through
the ordinary pre-connect builder (:meth:`~bae_py.harness.Harness.register_tool`).
They are opt-in — never auto-registered.

Security model
--------------

All three tools funnel every requested path through **one** shared
``_validate_path`` so their notions of "permitted" can never drift. The
validation order is deliberate and security-critical:

1. **Canonicalize first** — resolve ``..`` and symlinks *before* any allow/deny
   check. Checking an allowlist against the raw, uncanonicalized string is the
   classic path-traversal bug (``../../etc/passwd``, or a symlink escaping the
   allowed root). We resolve the longest existing prefix with
   :func:`os.path.realpath` (``strict=True``) so symlinks *and* ``..`` are
   resolved by the OS, never collapsed lexically first.
2. The canonical path must sit under one of the canonicalized
   :attr:`FileToolConfig.allowed_dirs` (an **empty** list permits *nothing*).
3. :attr:`FileToolConfig.denied_extensions` rejects (wins even over an allow).
4. :attr:`FileToolConfig.allowed_extensions`, if set, must match.
5. :attr:`FileToolConfig.deny_regex` rejects the filename.
6. :attr:`FileToolConfig.allow_regex`, if set, must match the filename.

:func:`write_file_tool` additionally refuses to write under a **missing parent
directory** unless :attr:`FileToolConfig.create_parents` is set.

Why validation failures are tool *results*, not errors
------------------------------------------------------

A rejected path returns an **in-band, error-shaped tool result**
(``{"error": "path not permitted: …"}``), never a raised exception that aborts
the loop. A security-boundary rejection is an *expected, foreseeable* model
behaviour (the LLM guessing at a path outside its sandbox), not a program bug —
so the model should see it as tool output it can read and retry from, mirroring
how the server turns MCP/sandbox failures into error-shaped ``tool.result`` s
rather than aborting the turn. This is the one place a builtin tool deliberately
catches-and-wraps rather than propagates.
"""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from typing import Any, Optional

from .tool import Tool
from .types import Content

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class FileToolConfig:
    """The security constraints a harness developer chooses for a set of file tools.

    The same config can be reused for all three constructors.
    """

    #: Only paths that canonicalize to somewhere under one of these directories
    #: are permitted. **Required** — an empty list permits *nothing* (an explicit,
    #: developer-visible "nothing is allowed" rather than an implicit
    #: filesystem-wide default).
    allowed_dirs: list[str]
    #: If set, only these extensions (without the leading dot, case-insensitive)
    #: are permitted.
    allowed_extensions: Optional[list[str]] = None
    #: These extensions are always rejected, even if ``allowed_extensions`` would
    #: otherwise permit them (e.g. block ``.env`` while allowing everything else).
    denied_extensions: list[str] = field(default_factory=list)
    #: If set, the path's filename must match this compiled regex.
    allow_regex: Optional[re.Pattern[str]] = None
    #: If set, a filename matching this compiled regex is always rejected.
    deny_regex: Optional[re.Pattern[str]] = None
    #: Whether :func:`write_file_tool` may create missing parent directories.
    #: Defaults to ``False``: a write under a non-existent parent is rejected.
    create_parents: bool = False


# ---------------------------------------------------------------------------
# Shared path validation (the single source of truth for all three tools)
# ---------------------------------------------------------------------------


@dataclass(slots=True)
class _Resolved:
    """A permitted path: its canonical form plus the matched ``allowed_dirs`` root."""

    canonical: str
    root: str


def _resolve_canonical(p: str) -> str:
    """Canonicalize ``p``, resolving ``..`` and symlinks.

    Unlike a bare :func:`os.path.realpath` this works for a **not-yet-existing**
    leaf (needed by ``write_file``): it realpaths the longest existing prefix —
    so every symlink is resolved — and appends the not-yet-created tail literally.
    We never lexically collapse ``..`` before resolving symlinks (that would erase
    a symlink component and defeat the check).
    """
    absolute = p if os.path.isabs(p) else os.getcwd() + os.sep + p
    try:
        return os.path.realpath(absolute, strict=True)
    except FileNotFoundError:
        parent = os.path.dirname(absolute)
        base = os.path.basename(absolute)
        if parent == absolute or base in ("", ".", ".."):
            raise
        return os.path.join(_resolve_canonical(parent), base)


def _extension_of(p: str) -> str:
    """Extension of ``p``'s file name, lowercased, or ``""`` if none.

    Unlike :func:`os.path.splitext` this treats a **dotfile**'s suffix as its
    extension (``.env`` → ``env``), so a ``denied_extensions=["env"]`` constraint
    blocks ``.env`` exactly as the file-tools guide's worked example promises.
    """
    name = os.path.basename(p)
    idx = name.rfind(".")
    return "" if idx == -1 or idx == len(name) - 1 else name[idx + 1 :].lower()


def _norm_ext(ext: str) -> str:
    """Normalise a developer-supplied extension (``".env"`` or ``"env"``)."""
    return ext.lstrip(".").lower()


def _under_root(canonical: str, root: str) -> bool:
    """Whether ``canonical`` is at or under the canonical directory ``root``."""
    return canonical == root or canonical.startswith(root + os.sep)


def _validate_path(config: FileToolConfig, requested: str) -> _Resolved | str:
    """Validate ``requested`` against ``config``.

    Returns a :class:`_Resolved` on success, or a ``str`` rejection reason. The
    single choke point every file tool shares (see the module docstring for the
    ordered rules).
    """
    try:
        canonical = _resolve_canonical(requested)
    except OSError as err:
        return f"cannot resolve `{requested}`: {err}"

    # (1) Prefix-match against the canonicalized allowed_dirs (empty ⇒ nothing).
    root: str | None = None
    for directory in config.allowed_dirs:
        try:
            cdir = os.path.realpath(directory, strict=True)
        except OSError:
            continue
        if _under_root(canonical, cdir):
            root = cdir
            break
    if root is None:
        return f"`{requested}` is not under any allowed directory"

    file_name = os.path.basename(canonical)
    ext = _extension_of(canonical)

    # (2) denied_extensions — always wins.
    if ext and any(_norm_ext(d) == ext for d in config.denied_extensions):
        return f"extension `.{ext}` is denied"

    # (3) allowed_extensions must match if set.
    if config.allowed_extensions is not None:
        ok = bool(ext) and any(_norm_ext(a) == ext for a in config.allowed_extensions)
        if not ok:
            return "extension is not in the allowed set"

    # (4) deny_regex rejects the filename.
    if config.deny_regex is not None and config.deny_regex.search(file_name):
        return "filename matches the deny pattern"

    # (5) allow_regex must match the filename if set.
    if config.allow_regex is not None and not config.allow_regex.search(file_name):
        return "filename does not match the allow pattern"

    return _Resolved(canonical=canonical, root=root)


# ---------------------------------------------------------------------------
# Result shaping (every tool result is a JSON *string* content block)
# ---------------------------------------------------------------------------


def _error_result(reason: str) -> Content:
    """An in-band, error-shaped result: the JSON string ``{"error": <reason>}``.

    A path rejection is prefixed ``path not permitted:`` so the model can tell a
    security rejection from an I/O failure.
    """
    return json.dumps({"error": reason})


def _ok_result(value: Any) -> Content:
    """A success result: ``value`` serialized to a JSON string (a plain-string block)."""
    return json.dumps(value)


# ---------------------------------------------------------------------------
# Tool constructors
# ---------------------------------------------------------------------------


def _path_input_schema(extra: dict[str, Any]) -> dict[str, Any]:
    return {
        "path": {
            "type": "string",
            "description": "Filesystem path, validated against the tool's security constraints.",
        },
        **extra,
    }


def read_file_tool(config: FileToolConfig) -> Tool:
    """A tool that reads a UTF-8 text file at ``path``, if ``config`` permits it.

    On a permitted, readable path the result is the JSON string
    ``{"path", "content"}``; a rejected path or read error is an error-shaped
    result (see the module docstring).
    """

    def handler(inp: dict[str, Any]) -> Content:
        requested = inp.get("path")
        if not isinstance(requested, str):
            return _error_result("read_file requires a string `path`")
        resolved = _validate_path(config, requested)
        if isinstance(resolved, str):
            return _error_result(f"path not permitted: {resolved}")
        try:
            with open(resolved.canonical, encoding="utf-8") as fh:
                content = fh.read()
        except OSError as err:
            return _error_result(f"read failed: {err}")
        return _ok_result({"path": requested, "content": content})

    return Tool(
        name="read_file",
        description=(
            "Read a UTF-8 text file. The path must satisfy the tool's configured "
            "directory/extension/filename constraints; a disallowed path returns an error result."
        ),
        input_schema={
            "type": "object",
            "properties": _path_input_schema({}),
            "required": ["path"],
            "additionalProperties": False,
        },
        handler=handler,
    )


def write_file_tool(config: FileToolConfig) -> Tool:
    """A tool that writes ``content`` to a file at ``path``, if ``config`` permits it.

    Refuses a missing parent directory unless :attr:`FileToolConfig.create_parents`
    is set. On success the result is the JSON string ``{"path", "bytes_written"}``.
    """

    def handler(inp: dict[str, Any]) -> Content:
        requested = inp.get("path")
        if not isinstance(requested, str):
            return _error_result("write_file requires a string `path`")
        content = inp.get("content")
        if not isinstance(content, str):
            content = ""
        resolved = _validate_path(config, requested)
        if isinstance(resolved, str):
            return _error_result(f"path not permitted: {resolved}")
        parent = os.path.dirname(resolved.canonical)
        if parent and not os.path.exists(parent):
            if config.create_parents:
                try:
                    os.makedirs(parent, exist_ok=True)
                except OSError as err:
                    return _error_result(f"could not create parents: {err}")
            else:
                return _error_result(
                    "path not permitted: parent directory does not exist "
                    "(enable create_parents to allow)"
                )
        try:
            data = content.encode("utf-8")
            with open(resolved.canonical, "wb") as fh:
                fh.write(data)
        except OSError as err:
            return _error_result(f"write failed: {err}")
        return _ok_result({"path": requested, "bytes_written": len(data)})

    return Tool(
        name="write_file",
        description=(
            "Write a UTF-8 text file. The path must satisfy the tool's configured constraints, "
            "and (unless create_parents is enabled) its parent directory must already exist."
        ),
        input_schema={
            "type": "object",
            "properties": _path_input_schema(
                {"content": {"type": "string", "description": "The UTF-8 text to write."}}
            ),
            "required": ["path", "content"],
            "additionalProperties": False,
        },
        handler=handler,
    )


def explore_files_tool(config: FileToolConfig) -> Tool:
    """A tool that lists the files under a permitted directory.

    **Non-recursive by default** (``{"recursive": true}`` descends). Every
    discovered entry is itself filtered through ``_validate_path``, so the listing
    can never surface a path :func:`read_file_tool` would reject. The result is a
    JSON string of an array of ``{"path", "is_dir", "size_bytes"}`` entries, each
    ``path`` relative to the matched ``allowed_dirs`` root.
    """

    def handler(inp: dict[str, Any]) -> Content:
        requested = inp.get("path")
        if not isinstance(requested, str):
            return _error_result("explore_files requires a string `path`")
        recursive = inp.get("recursive") is True
        resolved = _validate_path(config, requested)
        if isinstance(resolved, str):
            return _error_result(f"path not permitted: {resolved}")
        if not os.path.isdir(resolved.canonical):
            return _error_result(f"`{requested}` is not a directory")
        entries: list[dict[str, Any]] = []
        _walk(resolved.canonical, resolved.root, config, recursive, entries)
        # Sort by relative path so the listing is deterministic and byte-identical
        # across the Rust/TS/Python SDKs (each engine's directory-read order differs).
        entries.sort(key=lambda e: e["path"])
        return _ok_result(entries)

    return Tool(
        name="explore_files",
        description=(
            "List files under a permitted directory (non-recursive unless recursive=true). "
            "Only entries that satisfy the same constraints as read_file are returned."
        ),
        input_schema={
            "type": "object",
            "properties": _path_input_schema(
                {
                    "recursive": {
                        "type": "boolean",
                        "description": "Descend into subdirectories (default false).",
                    }
                }
            ),
            "required": ["path"],
            "additionalProperties": False,
        },
        handler=handler,
    )


def _walk(
    directory: str,
    root: str,
    config: FileToolConfig,
    recursive: bool,
    out: list[dict[str, Any]],
) -> None:
    """Depth-first walk emitting one entry per discovered path that passes
    ``_validate_path``.

    Descent is gated on the canonical child still living under ``root``, so a
    symlinked subdirectory pointing outside the allowed tree is neither listed
    nor followed.
    """
    try:
        names = os.listdir(directory)
    except OSError:
        return
    for name in names:
        try:
            # Canonicalize each child so a symlink escaping `root` is caught here
            # and never listed or descended into.
            canonical = os.path.realpath(os.path.join(directory, name), strict=True)
        except OSError:
            continue
        if not _under_root(canonical, root):
            continue
        is_dir = os.path.isdir(canonical)
        # Emit only entries a subsequent read_file would also accept.
        if not isinstance(_validate_path(config, canonical), str):
            try:
                size_bytes = 0 if is_dir else os.path.getsize(canonical)
            except OSError:
                size_bytes = 0
            out.append(
                {
                    "path": os.path.relpath(canonical, root),
                    "is_dir": is_dir,
                    "size_bytes": size_bytes,
                }
            )
        if is_dir and recursive:
            _walk(canonical, root, config, recursive, out)
