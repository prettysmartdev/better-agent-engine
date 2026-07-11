"""Integration/regression tests for the Python `issue-triage` example.

These exercise the example's OWN control flow — the two-phase loop, the
list-phase JSON parsing, the environment validation, the `ExecMode` ->
`SandboxTarget` mapping, and (crucially) the unconditional `work_root` cleanup
— entirely offline: no live BAE server, no GitHub API, no LLM provider. The
harness is replaced with a scripted fake whose `send()` returns canned replies,
so the loop runs deterministically in-process.

This is the Python leg of WI 0008's cross-SDK example parity + cleanup
regression. The canonical scenario (a mock GitHub issue set with one PR entry
and one already-marked issue) is mirrored by the server-side integration test
(`server/tests/integration.rs`, which drives the real MCP tool-call round
trip). See `/awman/context/workflow/test-plan-examples.md` for the coverage map.

The example lives at `examples/issue-triage/main.py`; its directory is not an
importable package (the name has a hyphen), so it is loaded by file path.
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

from bae_py import ExecResult, SandboxSession, SandboxTarget

# --- Load the example module by file path -----------------------------------

_EXAMPLE_PATH = Path(__file__).resolve().parents[1] / "examples" / "issue-triage" / "main.py"


def _load_example() -> ModuleType:
    spec = importlib.util.spec_from_file_location("issue_triage_example", _EXAMPLE_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    # Register before executing so the `@dataclass` decorator can resolve the
    # module's namespace (it looks the module up in `sys.modules`). A name other
    # than "__main__" also ensures the `if __name__ == "__main__"` guard does
    # NOT invoke main() on import.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


example = _load_example()


# --- The canonical scripted scenario ----------------------------------------
#
# A mock GitHub issue set whose list-phase reply (as a correct agent following
# the prompt would produce it) EXCLUDES the PR entry (#103) and includes the
# already-marked issue (#102). The example receives only issue *numbers*; the
# PR-exclusion decision is the agent's, so at the example level the contract is:
# the number set drives exactly one per-issue send each, the PR number never
# appears, and the already-marked issue is still visited (the "skip" — no label
# or comment — is a server-side/agent behavior asserted in the Rust integration
# test, not here).

NORMAL_ISSUE_A = 101
MARKED_ISSUE = 102
PR_ENTRY = 103  # has a `pull_request` field -> excluded from the list reply
NORMAL_ISSUE_B = 104

# The list-phase reply an agent returns: a fenced JSON array, PR omitted.
LIST_REPLY = "Here are the open issues:\n```json\n[101, 102, 104]\n```\n"
EXPECTED_NUMBERS = [NORMAL_ISSUE_A, MARKED_ISSUE, NORMAL_ISSUE_B]


class FakeReply:
    """Duck-typed stand-in for the SDK `Message` the example reads via
    `.text()`."""

    def __init__(self, text: str) -> None:
        self._text = text

    def text(self) -> str:
        return self._text


class FakeProfile:
    name = "issue-triage-test-profile"


class FakeSession:
    """A scripted session: records every prompt sent and replies from a
    canned script keyed by the phase embedded in the prompt text."""

    def __init__(self) -> None:
        self.sent: list[str] = []
        self.closed = 0
        self.started_sandboxes: list[str] = []
        self.remote_commands: list[str] = []
        self.session_id = "ses_fake_triage"
        self.profile = FakeProfile()

    async def send(self, prompt: str) -> FakeReply:
        self.sent.append(prompt)
        if "TASK (list phase)" in prompt:
            return FakeReply(LIST_REPLY)
        # Per-issue phase: the marked issue is answered `already triaged`; any
        # other issue gets a one-line triage summary.
        if f"issue #{MARKED_ISSUE} " in prompt:
            return FakeReply("already triaged")
        return FakeReply("labeled bug/sev-medium; posted a triage plan comment")

    async def start_remote_sandbox(self, image: str) -> None:
        self.started_sandboxes.append(image)

    async def exec_remote_sandbox(self, command: str) -> ExecResult:
        self.remote_commands.append(command)
        return ExecResult(stdout="git version 2.test\n", stderr="", exit_code=0)

    async def close(self) -> None:
        self.closed += 1


class FakeHarness:
    """Replaces `Harness` so `run()` never opens a real transport. Tool
    registration is a no-op; `sandbox_session()` returns a real
    `SandboxSession` so the example's `run_shell_command(...)` construction runs
    unmodified."""

    last_instance: "FakeHarness | None" = None

    def __init__(self, config: Any) -> None:
        self.config = config
        self.session = FakeSession()
        self._sandbox = SandboxSession()
        FakeHarness.last_instance = self

    def sandbox_session(self) -> SandboxSession:
        return self._sandbox

    def register_tool(self, tool: Any) -> "FakeHarness":
        return self

    def register_sandbox_tool(self, tool: Any) -> "FakeHarness":
        return self

    async def connect(self) -> FakeSession:
        return self.session


# --- Environment helpers ----------------------------------------------------

_BASE_ENV = {
    "BAE_CLIENT_KEY": "bae_test_key",
    "ANTHROPIC_API_KEY": "sk-test",
    "GITHUB_TOKEN": "ghp_test",
    "TRIAGE_REPO": "octocat/Hello-World",
    "TRIAGE_EXEC_MODE": "none",
}


def _set_env(monkeypatch: pytest.MonkeyPatch, **overrides: str) -> None:
    # Clear every variable the example reads, then apply base + overrides so a
    # stray value in the ambient environment cannot leak into a test.
    for name in (
        "BAE_SERVER_URL",
        "BAE_CLIENT_KEY",
        "BAE_PROVIDER_KEY_ENV",
        "ANTHROPIC_API_KEY",
        "GITHUB_TOKEN",
        "TRIAGE_REPO",
        "TRIAGE_EXEC_MODE",
        "TRIAGE_SANDBOX_IMAGE",
        "TRIAGE_MAX_ISSUES",
    ):
        monkeypatch.delenv(name, raising=False)
    env = {**_BASE_ENV, **overrides}
    for name, value in env.items():
        monkeypatch.setenv(name, value)


# ---------------------------------------------------------------------------
# Pure-function unit coverage
# ---------------------------------------------------------------------------


def test_parse_issue_numbers_prefers_fenced_block_dedups_and_caps() -> None:
    # The PR (#103) is already absent from the agent's fenced reply; parsing
    # just extracts the integers, dedups preserving order, and caps at max.
    assert example.parse_issue_numbers(LIST_REPLY, 10) == EXPECTED_NUMBERS
    assert example.parse_issue_numbers("```\n[5, 5, 4, 5]\n```", 10) == [5, 4]
    assert example.parse_issue_numbers("```json\n[9, 8, 7]\n```", 2) == [9, 8]
    # Bracket-span fallback when the model omits the fence.
    assert example.parse_issue_numbers("issues: [3, 2, 1] done", 10) == [3, 2, 1]
    # An empty array is valid and means "nothing to triage".
    assert example.parse_issue_numbers("```json\n[]\n```", 10) == []


def test_parse_issue_numbers_rejects_non_integer_and_missing_array() -> None:
    with pytest.raises(ValueError):
        example.parse_issue_numbers("no array here at all", 10)
    with pytest.raises(ValueError):
        example.parse_issue_numbers('```json\n["a", "b"]\n```', 10)


def test_per_issue_prompt_carries_marker_and_issue_dir() -> None:
    settings = example.Settings(
        server_url="http://x",
        client_key="k",
        repo="octocat/Hello-World",
        mode=example.ExecMode.NONE,
        sandbox_image=None,
        max_issues=10,
    )
    prompt = example.per_issue_prompt(settings, "/work", 101)
    assert example.TRIAGE_MARKER in prompt
    assert "/work/issue-101" in prompt
    assert "issue #101" in prompt


def test_list_phase_prompt_excludes_pr_language_and_embeds_repo() -> None:
    settings = example.Settings(
        server_url="http://x",
        client_key="k",
        repo="octocat/Hello-World",
        mode=example.ExecMode.NONE,
        sandbox_image=None,
        max_issues=7,
    )
    prompt = example.list_phase_prompt(settings)
    assert "`pull_request`" in prompt
    assert "octocat/Hello-World" in prompt
    assert "at most 7 issues" in prompt


def test_sandbox_target_mapping_covers_all_three_modes() -> None:
    def target(mode: Any, image: str | None) -> SandboxTarget:
        settings = example.Settings(
            server_url="http://x",
            client_key="k",
            repo="a/b",
            mode=mode,
            sandbox_image=image,
            max_issues=10,
        )
        return example._sandbox_target(settings)

    assert target(example.ExecMode.NONE, None).kind == "none"
    assert target(example.ExecMode.LOCAL_SANDBOX, "python:3.12").kind == "local"
    assert target(example.ExecMode.REMOTE_SANDBOX, "python:3.12").kind == "remote"


def test_work_root_for_slugifies_owner_repo() -> None:
    assert (
        example._work_root_for("octocat/Hello-World")
        == Path("issue-triage-work") / "octocat-Hello-World"
    )


@pytest.mark.parametrize(
    ("missing", "fragment"),
    [
        ("BAE_CLIENT_KEY", "environment variable `BAE_CLIENT_KEY` is required"),
        ("ANTHROPIC_API_KEY", "provider key env var `ANTHROPIC_API_KEY` is not set"),
        ("GITHUB_TOKEN", "environment variable `GITHUB_TOKEN` is required"),
    ],
)
def test_settings_from_env_fails_fast_with_exact_messages(
    monkeypatch: pytest.MonkeyPatch, missing: str, fragment: str
) -> None:
    _set_env(monkeypatch)
    monkeypatch.delenv(missing, raising=False)
    with pytest.raises(ValueError) as excinfo:
        example.Settings.from_env()
    assert fragment in str(excinfo.value)


def test_settings_from_env_requires_sandbox_image_for_container_modes(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _set_env(monkeypatch, TRIAGE_EXEC_MODE="local-sandbox")
    with pytest.raises(ValueError) as excinfo:
        example.Settings.from_env()
    assert "TRIAGE_SANDBOX_IMAGE` is required" in str(excinfo.value)


# ---------------------------------------------------------------------------
# Two-phase loop + cleanup regression (end-to-end, offline)
# ---------------------------------------------------------------------------


async def test_two_phase_loop_visits_each_issue_excludes_pr_and_cleans_up(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _set_env(monkeypatch)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(example, "Harness", FakeHarness)

    await example.run()

    harness = FakeHarness.last_instance
    assert harness is not None
    session = harness.session

    # Phase 1: exactly one list-phase send, first.
    assert "TASK (list phase)" in session.sent[0]
    list_sends = [p for p in session.sent if "TASK (list phase)" in p]
    assert len(list_sends) == 1

    # Phase 2: exactly one per-issue send per parsed number, in order — and the
    # PR entry (#103) NEVER appears in a per-issue send.
    per_issue = [p for p in session.sent if "TASK (per-issue phase)" in p]
    assert len(per_issue) == len(EXPECTED_NUMBERS)
    for number, prompt in zip(EXPECTED_NUMBERS, per_issue):
        assert f"issue #{number} " in prompt
        assert example.TRIAGE_MARKER in prompt
    assert not any(f"issue #{PR_ENTRY} " in p for p in per_issue)

    # Regression — cleanup: `work_root` no longer exists on disk after the run
    # (required for `none` mode; the example removes it unconditionally).
    work_root = tmp_path / "issue-triage-work" / "octocat-Hello-World"
    assert not work_root.exists()

    # The session was closed (the teardown that stops any local sandbox and
    # triggers the server's remote-sandbox teardown), regardless of mode.
    assert session.closed == 1
    # `none` mode never starts a remote sandbox.
    assert session.started_sandboxes == []


async def test_empty_issue_list_finishes_without_per_issue_sends(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _set_env(monkeypatch)
    monkeypatch.chdir(tmp_path)

    class EmptyListSession(FakeSession):
        async def send(self, prompt: str) -> FakeReply:
            self.sent.append(prompt)
            assert "TASK (list phase)" in prompt, "no per-issue send for an empty list"
            return FakeReply("```json\n[]\n```")

    class EmptyHarness(FakeHarness):
        def __init__(self, config: Any) -> None:
            super().__init__(config)
            self.session = EmptyListSession()
            EmptyHarness.last_instance = self

    monkeypatch.setattr(example, "Harness", EmptyHarness)
    await example.run()

    session = EmptyHarness.last_instance.session
    assert len(session.sent) == 1  # only the list phase
    assert session.closed == 1
    assert not (tmp_path / "issue-triage-work" / "octocat-Hello-World").exists()


async def test_remote_sandbox_mode_starts_sandbox_then_cleans_up(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    # Regression — for a sandbox mode, the (mocked) sandbox is started up front
    # and the session is still closed (server-side teardown) after the run,
    # regardless of the example's own explicit work_root removal.
    _set_env(
        monkeypatch,
        TRIAGE_EXEC_MODE="remote-sandbox",
        TRIAGE_SANDBOX_IMAGE="python:3.12",
    )
    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr(example, "Harness", FakeHarness)

    await example.run()

    session = FakeHarness.last_instance.session
    assert session.started_sandboxes == ["python:3.12"]
    assert session.remote_commands == [example.GIT_BOOTSTRAP]
    assert session.closed == 1
    assert not (tmp_path / "issue-triage-work" / "octocat-Hello-World").exists()
