#!/usr/bin/env python3
"""Minimal stdio MCP server fixture emulating the GitHub issues API surface the
`issue-triage` example (WI 0008) drives.

Like `mcp_echo_server.py`, it speaks the newline-delimited JSON-RPC 2.0 subset
the engine's MCP client (`server/src/engine/mcp.rs`) uses — ``initialize`` +
``notifications/initialized``, ``tools/list``, ``tools/call`` — as a local
fixture *process*: no network, no real GitHub API, no token. It exists so the
integration test can drive a genuine two-phase triage loop (list issues, then
per-issue label + comment) offline.

It advertises four tools mirroring the GitHub MCP server's shape closely enough
for the scripted mock provider to discover and call them from the prompt alone:

  - ``list_issues``  -> the canned open-issue set (INCLUDING one pull-request
    entry, which carries a ``pull_request`` field; the agent must exclude it).
  - ``get_issue``    -> one issue's full record, comments included. One issue's
    comments already contain the triage marker (the idempotency case).
  - ``add_labels``   -> records a label mutation, returns ``{"ok": true}``.
  - ``add_comment``  -> records a comment creation, returns ``{"ok": true}``.

The canned data below is the single source of truth for the test's assertions;
keep the two in sync. The server emits real ``mcp.request``/``mcp.response``
events for every ``tools/call``, so the test asserts the tool-call shape from
the event log (which PR was excluded, which issue was skipped, that each
labelled issue also got exactly one marker-bearing comment) — the fixture need
not itself record anything.
"""

import json
import sys

MARKER = "<!-- issue-triage:v1 -->"

# The canned open-"issue" set list_issues returns. #103 is a pull request (it
# carries a `pull_request` field) and must be excluded by the agent; #102 was
# already triaged (its comments contain the marker) and must be skipped without
# a second label/comment.
ISSUES = {
    101: {
        "number": 101,
        "title": "App crashes on startup with a null config",
        "body": "Launching with an empty config file panics immediately.",
        "labels": [],
        "comments": [],
    },
    102: {
        "number": 102,
        "title": "Typo in the README install section",
        "body": "The install command is missing a flag.",
        "labels": ["question"],
        "comments": [
            {"id": 5001, "body": MARKER + "\nPreviously triaged: docs typo, low effort."}
        ],
    },
    103: {
        "number": 103,
        "title": "Fix the crash (PR)",
        "body": "This pull request fixes #101.",
        "labels": [],
        "comments": [],
        # Presence of this field is what marks the entry as a pull request in
        # GitHub's issues API — the agent must EXCLUDE it from the per-issue phase.
        "pull_request": {"url": "https://api.github.com/repos/acme/widget/pulls/103"},
    },
    104: {
        "number": 104,
        "title": "Add a --json output mode",
        "body": "It would help scripting if the CLI could emit JSON.",
        "labels": [],
        "comments": [],
    },
}

TOOLS = [
    {
        "name": "list_issues",
        "description": "List the open issues (and pull requests) of the repository.",
        "inputSchema": {
            "type": "object",
            "properties": {"state": {"type": "string"}},
        },
    },
    {
        "name": "get_issue",
        "description": "Fetch one issue by number, with its labels and comments.",
        "inputSchema": {
            "type": "object",
            "properties": {"issue_number": {"type": "integer"}},
            "required": ["issue_number"],
        },
    },
    {
        "name": "add_labels",
        "description": "Add labels to an issue.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_number": {"type": "integer"},
                "labels": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["issue_number", "labels"],
        },
    },
    {
        "name": "add_comment",
        "description": "Post a comment on an issue.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "issue_number": {"type": "integer"},
                "body": {"type": "string"},
            },
            "required": ["issue_number", "body"],
        },
    },
]


def respond(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def _text_result(msg_id, payload):
    """A tools/call success whose single text block is the JSON payload."""
    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "result": {
            "content": [{"type": "text", "text": json.dumps(payload)}],
            "isError": False,
        },
    }


def _error(msg_id, code, message):
    return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}}


def handle_tools_call(msg_id, params):
    name = params.get("name")
    args = params.get("arguments") or {}

    if name == "list_issues":
        # Newest first, PR entry included — the agent is responsible for
        # excluding it.
        listed = [ISSUES[n] for n in sorted(ISSUES, reverse=True)]
        return _text_result(msg_id, listed)

    if name == "get_issue":
        number = args.get("issue_number")
        issue = ISSUES.get(number)
        if issue is None:
            return _error(msg_id, -32602, f"unknown issue: {number}")
        return _text_result(msg_id, issue)

    if name == "add_labels":
        return _text_result(
            msg_id,
            {"ok": True, "issue_number": args.get("issue_number"), "labels": args.get("labels")},
        )

    if name == "add_comment":
        return _text_result(
            msg_id, {"ok": True, "issue_number": args.get("issue_number"), "id": 9001}
        )

    return _error(msg_id, -32602, "unknown tool: " + str(name))


def handle(msg):
    msg_id = msg.get("id")
    method = msg.get("method")

    if msg_id is None:
        return None  # a notification (e.g. notifications/initialized): no reply.

    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "github-mock-fixture", "version": "0.1.0"},
            },
        }

    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS}}

    if method == "tools/call":
        return handle_tools_call(msg_id, msg.get("params") or {})

    return _error(msg_id, -32601, "method not found: " + str(method))


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        reply = handle(msg)
        if reply is not None:
            respond(reply)


if __name__ == "__main__":
    main()
