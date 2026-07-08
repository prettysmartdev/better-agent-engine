"""Builtin file-tool security tests — the ``validate_path`` suite, the
``write_file`` parent-directory guard, and ``explore``/``read`` consistency.

The file tools are the one builtin that needs no Session; everything here runs
offline against real temp dirs and a real symlink (via pytest's ``tmp_path``).
The scenarios mirror the Rust (``files.rs``) and TypeScript (``files.test.ts``)
suites.
"""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
from typing import Any

from bae_py import (
    FileToolConfig,
    explore_files_tool,
    read_file_tool,
    write_file_tool,
)
from bae_py.tool import Tool


def _call(tool: Tool, inp: dict[str, Any]) -> Any:
    """Drive a file tool's (synchronous) handler and parse its JSON-string result."""
    raw = tool.handler(inp)
    assert isinstance(raw, str)
    return json.loads(raw)


def _not_permitted(result: Any) -> bool:
    """Was the path rejected with an in-band ``path not permitted:`` error?"""
    err = result.get("error") if isinstance(result, dict) else None
    return isinstance(err, str) and err.startswith("path not permitted:")


def _write(p: Path, contents: str) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(contents, encoding="utf-8")


def test_allowed_dir_and_allowed_extension_is_permitted(tmp_path: Path) -> None:
    file = tmp_path / "notes.txt"
    _write(file, "hello")
    config = FileToolConfig(allowed_dirs=[str(tmp_path)], allowed_extensions=["txt"])
    result = _call(read_file_tool(config), {"path": str(file)})
    assert result["content"] == "hello"
    assert "error" not in result


def test_parent_traversal_escape_is_rejected(tmp_path: Path) -> None:
    allowed = tmp_path / "allowed"
    allowed.mkdir()
    _write(tmp_path / "secret.txt", "top secret")
    config = FileToolConfig(allowed_dirs=[str(allowed)])
    escape = str(allowed / ".." / "secret.txt")
    assert _not_permitted(_call(read_file_tool(config), {"path": escape}))


def test_symlink_escaping_allowed_dir_is_rejected(tmp_path: Path) -> None:
    allowed = tmp_path / "allowed"
    outside = tmp_path / "outside"
    allowed.mkdir()
    outside.mkdir()
    _write(outside / "secret.txt", "top secret")
    link = allowed / "link.txt"
    os.symlink(outside / "secret.txt", link)
    # Canonicalize-before-check resolves the symlink to ``outside/…``, which is
    # not under the allowed root (asserted on behaviour, not the string form).
    config = FileToolConfig(allowed_dirs=[str(allowed)])
    assert _not_permitted(_call(read_file_tool(config), {"path": str(link)}))


def test_denied_extension_overrides_allowed_extension(tmp_path: Path) -> None:
    _write(tmp_path / "app.env", "SECRET=1")
    _write(tmp_path / "app.txt", "ok")
    tool = read_file_tool(
        FileToolConfig(
            allowed_dirs=[str(tmp_path)],
            allowed_extensions=["env", "txt"],
            denied_extensions=["env"],
        )
    )
    assert _not_permitted(_call(tool, {"path": str(tmp_path / "app.env")}))
    assert _call(tool, {"path": str(tmp_path / "app.txt")})["content"] == "ok"


def test_deny_regex_overrides_allow_regex(tmp_path: Path) -> None:
    _write(tmp_path / "public.txt", "ok")
    _write(tmp_path / "secret.txt", "no")
    tool = read_file_tool(
        FileToolConfig(
            allowed_dirs=[str(tmp_path)],
            allow_regex=re.compile(r"\.txt$"),
            deny_regex=re.compile(r"secret"),
        )
    )
    assert _not_permitted(_call(tool, {"path": str(tmp_path / "secret.txt")}))
    assert _call(tool, {"path": str(tmp_path / "public.txt")})["content"] == "ok"


def test_empty_allowed_dirs_rejects_everything(tmp_path: Path) -> None:
    file = tmp_path / "f.txt"
    _write(file, "hello")
    config = FileToolConfig(allowed_dirs=[])
    assert _not_permitted(_call(read_file_tool(config), {"path": str(file)}))


def test_write_file_parent_directory_guard(tmp_path: Path) -> None:
    target = tmp_path / "newdir" / "out.txt"
    # create_parents defaults False → a write under a missing parent is rejected.
    strict = _call(
        write_file_tool(FileToolConfig(allowed_dirs=[str(tmp_path)])),
        {"path": str(target), "content": "x"},
    )
    assert _not_permitted(strict)
    assert not target.exists()

    # With create_parents enabled the same write succeeds.
    ok = _call(
        write_file_tool(FileToolConfig(allowed_dirs=[str(tmp_path)], create_parents=True)),
        {"path": str(target), "content": "hello"},
    )
    assert ok["bytes_written"] == 5
    assert target.read_text() == "hello"


def test_explore_only_returns_paths_read_file_would_accept(tmp_path: Path) -> None:
    # Real files inside and outside the allowed tree, plus a symlink inside the
    # tree escaping it: explore must return only entries read_file also permits.
    allowed = tmp_path / "allowed"
    outside = tmp_path / "outside"
    allowed.mkdir()
    outside.mkdir()
    _write(allowed / "a.txt", "a")
    _write(allowed / "sub" / "b.txt", "b")
    _write(outside / "c.txt", "c")
    os.symlink(outside / "c.txt", allowed / "escape.txt")

    config = FileToolConfig(allowed_dirs=[str(allowed)])
    entries = _call(explore_files_tool(config), {"path": str(allowed), "recursive": True})
    rels = [e["path"] for e in entries]

    # The escaping symlink and the outside file never appear.
    assert "a.txt" in rels
    assert any(p.endswith("b.txt") for p in rels)
    assert not any("escape" in p for p in rels)
    assert not any("c.txt" in p for p in rels)

    # Property: every returned path passes read_file's validation under the
    # identical config (a directory yields a read error, never a rejection).
    reader = read_file_tool(config)
    canonical_root = os.path.realpath(str(allowed))
    for rel in rels:
        result = _call(reader, {"path": os.path.join(canonical_root, rel)})
        assert not _not_permitted(result), f"explore surfaced {rel!r} read_file rejected"
