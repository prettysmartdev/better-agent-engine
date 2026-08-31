//! Self-contained stdio MCP fixture used by the server integration tests.
//!
//! This deliberately has no networking or third-party-runtime dependency. The
//! tests exercise the real stdio transport, but must also run on a clean macOS
//! checkout where `python3` is not installed.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

const TRIAGE_MARKER: &str = "<!-- issue-triage:v1 -->";

fn main() {
    let mut args = std::env::args().skip(1);
    let fixture = args.next().unwrap_or_else(|| "echo".to_owned());
    if let Some(pidfile) = args.next() {
        if let Err(error) = std::fs::write(&pidfile, std::process::id().to_string()) {
            eprintln!("failed to write MCP fixture PID file {pidfile:?}: {error}");
            std::process::exit(1);
        }
    }

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(response) = respond(&fixture, &request) else {
            continue;
        };
        // A broken pipe simply means the BAE session has closed its end.
        if writeln!(stdout, "{response}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

fn respond(fixture: &str, request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "bae-mcp-test-fixture", "version": "0.1.0" },
        }),
        "tools/list" => json!({ "tools": tools(fixture) }),
        "tools/call" => match call_tool(fixture, request.get("params").unwrap_or(&Value::Null)) {
            Ok(result) => return Some(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
            Err(message) => {
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32602, "message": message },
                }));
            }
        },
        _ => {
            return Some(json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }));
        }
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tools(fixture: &str) -> Value {
    match fixture {
        "echo" => json!([{
            "name": "remote_search",
            "description": "Echo a search query back (test fixture).",
            "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } },
        }]),
        "github" => json!([
            {
                "name": "list_issues",
                "description": "List the open issues (and pull requests) of the repository.",
                "inputSchema": { "type": "object", "properties": { "state": { "type": "string" } } },
            },
            {
                "name": "get_issue",
                "description": "Fetch one issue by number, with its labels and comments.",
                "inputSchema": { "type": "object", "properties": { "issue_number": { "type": "integer" } }, "required": ["issue_number"] },
            },
            {
                "name": "add_labels",
                "description": "Add labels to an issue.",
                "inputSchema": { "type": "object", "properties": { "issue_number": { "type": "integer" }, "labels": { "type": "array", "items": { "type": "string" } } }, "required": ["issue_number", "labels"] },
            },
            {
                "name": "add_comment",
                "description": "Post a comment on an issue.",
                "inputSchema": { "type": "object", "properties": { "issue_number": { "type": "integer" }, "body": { "type": "string" } }, "required": ["issue_number", "body"] },
            },
        ]),
        _ => json!([]),
    }
}

fn call_tool(fixture: &str, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let payload = match (fixture, name) {
        ("echo", "remote_search") => json!({
            "content": [{ "type": "text", "text": format!("echo: {}", arguments["q"].as_str().unwrap_or_default()) }],
            "isError": false,
        }),
        ("github", "list_issues") => text_result(json!([
            issue(
                104,
                "Add a --json output mode",
                "It would help scripting if the CLI could emit JSON.",
                json!([]),
                json!([])
            ),
            issue(
                103,
                "Fix the crash (PR)",
                "This pull request fixes #101.",
                json!([]),
                json!([])
            )
            .as_object()
            .cloned()
            .map(|mut issue| {
                issue.insert(
                    "pull_request".into(),
                    json!({ "url": "https://api.github.com/repos/acme/widget/pulls/103" }),
                );
                Value::Object(issue)
            })
            .unwrap(),
            issue(
                102,
                "Typo in the README install section",
                "The install command is missing a flag.",
                json!(["question"]),
                json!([{ "id": 5001, "body": format!("{TRIAGE_MARKER}\nPreviously triaged: docs typo, low effort.") }])
            ),
            issue(
                101,
                "App crashes on startup with a null config",
                "Launching with an empty config file panics immediately.",
                json!([]),
                json!([])
            ),
        ])),
        ("github", "get_issue") => {
            let number = arguments["issue_number"]
                .as_u64()
                .ok_or_else(|| "unknown issue".to_owned())?;
            let issue = match number {
                101 => issue(
                    101,
                    "App crashes on startup with a null config",
                    "Launching with an empty config file panics immediately.",
                    json!([]),
                    json!([]),
                ),
                102 => issue(
                    102,
                    "Typo in the README install section",
                    "The install command is missing a flag.",
                    json!(["question"]),
                    json!([{ "id": 5001, "body": format!("{TRIAGE_MARKER}\nPreviously triaged: docs typo, low effort.") }]),
                ),
                103 => issue(
                    103,
                    "Fix the crash (PR)",
                    "This pull request fixes #101.",
                    json!([]),
                    json!([]),
                ),
                104 => issue(
                    104,
                    "Add a --json output mode",
                    "It would help scripting if the CLI could emit JSON.",
                    json!([]),
                    json!([]),
                ),
                _ => return Err(format!("unknown issue: {number}")),
            };
            text_result(issue)
        }
        ("github", "add_labels") => text_result(
            json!({ "ok": true, "issue_number": arguments["issue_number"], "labels": arguments["labels"] }),
        ),
        ("github", "add_comment") => text_result(
            json!({ "ok": true, "issue_number": arguments["issue_number"], "id": 9001 }),
        ),
        _ => return Err(format!("unknown tool: {name}")),
    };
    Ok(payload)
}

fn text_result(payload: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": payload.to_string() }], "isError": false })
}

fn issue(number: u64, title: &str, body: &str, labels: Value, comments: Value) -> Value {
    json!({ "number": number, "title": title, "body": body, "labels": labels, "comments": comments })
}
