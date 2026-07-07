#!/usr/bin/env python3
"""Minimal stdio MCP server fixture for the BAE integration tests.

Speaks the newline-delimited JSON-RPC 2.0 subset the engine's MCP client
(`server/src/engine/mcp.rs`) actually uses: ``initialize`` +
``notifications/initialized``, ``tools/list``, and ``tools/call``. It is a local
fixture *process* — no network, no real MCP SaaS, no API keys — so the whole
suite stays offline per `make test-server` convention.

It advertises exactly one tool, ``remote_search`` (the same name the tests' mock
provider asks for), and answers a ``tools/call`` with a real, non-stub result so
the tests can assert genuine ``mcp.request`` / ``mcp.response`` / ``tool.result``
payloads flow end to end.

Usage: ``mcp_echo_server.py [PID_FILE]``. When ``PID_FILE`` is given, the server
writes its own PID there on startup, so a test can confirm (via ``/proc``) that
the subprocess is terminated after the session is closed.
"""

import json
import os
import sys

TOOLS = [
    {
        "name": "remote_search",
        "description": "Echo a search query back (test fixture).",
        "inputSchema": {
            "type": "object",
            "properties": {"q": {"type": "string"}},
        },
    }
]


def respond(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def handle(msg):
    """Return a JSON-RPC response object for a request, or None for a
    notification (no ``id``) that needs no reply."""
    msg_id = msg.get("id")
    method = msg.get("method")

    if msg_id is None:
        # A notification (e.g. notifications/initialized): no reply.
        return None

    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mcp-echo-fixture", "version": "0.1.0"},
            },
        }

    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS}}

    if method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        arguments = params.get("arguments") or {}
        if name == "remote_search":
            q = arguments.get("q", "")
            return {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "content": [{"type": "text", "text": "echo: " + str(q)}],
                    "isError": False,
                },
            }
        # Unknown tool: a JSON-RPC error the client maps to a protocol error.
        return {
            "jsonrpc": "2.0",
            "id": msg_id,
            "error": {"code": -32602, "message": "unknown tool: " + str(name)},
        }

    return {
        "jsonrpc": "2.0",
        "id": msg_id,
        "error": {"code": -32601, "message": "method not found: " + str(method)},
    }


def main():
    if len(sys.argv) > 1:
        # Record our PID so a test can assert the subprocess is reaped on close.
        with open(sys.argv[1], "w") as f:
            f.write(str(os.getpid()))
            f.flush()

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
