// Integration/regression test for the TypeScript `issue-triage` example.
//
// The example is an executable script (`void main()` on import) with no
// exported functions, so it is exercised as a black box: spawned via `tsx`
// against a minimal in-process mock BAE server that speaks the exact wire
// protocol the SDK's `FetchTransport` uses (session open, `session.registerDriver`
// + `session.sendMessage` over `…/rpc` as NDJSON, and session close). No real
// server, no GitHub API, no LLM provider — fully offline.
//
// This is the TypeScript leg of WI 0008's cross-SDK example parity + cleanup
// regression: it asserts the same two-phase-loop contract the Python
// (`client-python/tests/test_issue_triage_example.py`) and Rust integration
// (`server/tests/integration.rs`) legs assert — one list-phase send, then one
// per-issue send per parsed number, the PR entry never visited, each per-issue
// prompt carrying the marker, and `work_root` removed on exit. See
// `/awman/context/workflow/test-plan-examples.md` for the coverage map.

import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, expect, it } from "vitest";

const dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(dirname, "..");
const examplePath = path.join(repoRoot, "examples", "issue-triage", "main.ts");
const tsxBin = path.join(repoRoot, "node_modules", ".bin", "tsx");

const MARKER = "<!-- issue-triage:v1 -->";
// The list-phase reply a correct agent returns: PR entry (#103) excluded,
// newest-first. The example parses these numbers and drives one send each.
const LIST_REPLY = "```json\n[104, 102, 101]\n```";
const MARKED_ISSUE = 102;
const PR_ENTRY = 103;
const EXPECTED_NUMBERS = [104, 102, 101];

/** Pull the text out of a `session.sendMessage` param message (string or
 * block-array content). */
function messageContentText(message: unknown): string {
  const content = (message as { content?: unknown })?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .filter((b) => (b as { type?: string }).type === "text")
      .map((b) => (b as { text?: string }).text ?? "")
      .join("\n");
  }
  return "";
}

/** The scripted assistant reply for a given prompt. */
function scriptedReply(prompt: string): string {
  if (prompt.includes("TASK (list phase)")) return LIST_REPLY;
  if (prompt.includes(`issue #${MARKED_ISSUE} `)) return "already triaged";
  return "labeled bug/sev-medium; posted a triage plan comment";
}

interface MockServer {
  url: string;
  prompts: string[];
  closed: boolean;
  stop: () => Promise<void>;
}

/** A minimal mock BAE server: the four endpoints the SDK drives, recording the
 * `session.sendMessage` prompts and replying from `scriptedReply`. */
async function startMockServer(): Promise<MockServer> {
  const state = { prompts: [] as string[], closed: false };

  const server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c as Buffer));
    req.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf8");
      const url = req.url ?? "";

      if (req.method === "POST" && url === "/api/v1/sessions") {
        res.writeHead(201, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            session_id: "ses_ts_triage",
            session_key: "sk_ts_triage",
            profile: { name: "issue-triage-test-profile" },
          }),
        );
        return;
      }

      if (req.method === "POST" && url.endsWith("/rpc")) {
        const body = raw.length > 0 ? JSON.parse(raw) : {};
        const id = body.id ?? 1;
        res.writeHead(200, { "content-type": "application/x-ndjson" });
        if (body.method === "session.registerDriver") {
          res.end(
            JSON.stringify({
              jsonrpc: "2.0",
              id,
              result: { registered: true },
            }) + "\n",
          );
          return;
        }
        if (body.method === "session.sendMessage") {
          const prompt = messageContentText(body.params?.message);
          state.prompts.push(prompt);
          const reply = scriptedReply(prompt);
          res.end(
            JSON.stringify({
              jsonrpc: "2.0",
              id,
              result: {
                message: {
                  role: "assistant",
                  content: [{ type: "text", text: reply }],
                },
                events: [],
              },
            }) + "\n",
          );
          return;
        }
        // Any other RPC: a bare terminal ok frame.
        res.end(JSON.stringify({ jsonrpc: "2.0", id, result: {} }) + "\n");
        return;
      }

      if (req.method === "DELETE" && url.startsWith("/api/v1/sessions/")) {
        state.closed = true;
        res.writeHead(200, { "content-type": "application/json" });
        res.end("{}");
        return;
      }

      res.writeHead(404);
      res.end();
    });
  });

  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const addr = server.address();
  if (addr === null || typeof addr === "string") {
    throw new Error("mock server did not bind a TCP port");
  }
  return {
    url: `http://127.0.0.1:${addr.port}`,
    get prompts() {
      return state.prompts;
    },
    get closed() {
      return state.closed;
    },
    stop: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

interface RunResult {
  code: number | null;
  stderr: string;
}

/** Run the example under `tsx` against `serverUrl`, in `cwd`, to completion. */
function runExample(serverUrl: string, cwd: string): Promise<RunResult> {
  const child = spawn(tsxBin, [examplePath], {
    cwd,
    env: {
      ...process.env,
      BAE_SERVER_URL: serverUrl,
      BAE_CLIENT_KEY: "bae_test_key",
      ANTHROPIC_API_KEY: "sk-test",
      GITHUB_TOKEN: "ghp_test",
      TRIAGE_REPO: "octocat/Hello-World",
      TRIAGE_EXEC_MODE: "none",
      TRIAGE_MAX_ISSUES: "10",
    },
  });
  let stderr = "";
  child.stderr.on("data", (c) => (stderr += String(c)));
  child.stdout.on("data", () => undefined);
  return new Promise<RunResult>((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, stderr }));
  });
}

let tmpDir: string;
let mock: MockServer;

beforeEach(async () => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "triage-ts-"));
  mock = await startMockServer();
});

afterEach(async () => {
  await mock.stop();
  fs.rmSync(tmpDir, { recursive: true, force: true });
});

it("runs the two-phase loop, excludes the PR, and cleans up work_root", async () => {
  const { code, stderr } = await runExample(mock.url, tmpDir);
  expect(code, `example exited non-zero:\n${stderr}`).toBe(0);

  // Phase 1: exactly one list-phase send, first.
  expect(mock.prompts[0]).toContain("TASK (list phase)");
  expect(
    mock.prompts.filter((p) => p.includes("TASK (list phase)")),
  ).toHaveLength(1);

  // Phase 2: one per-issue send per parsed number, in order; each carries the
  // marker + the per-issue work dir; the PR entry is never visited.
  const perIssue = mock.prompts.filter((p) =>
    p.includes("TASK (per-issue phase)"),
  );
  expect(perIssue).toHaveLength(EXPECTED_NUMBERS.length);
  EXPECTED_NUMBERS.forEach((n, i) => {
    expect(perIssue[i]).toContain(`issue #${n} `);
    expect(perIssue[i]).toContain(MARKER);
    expect(perIssue[i]).toContain(`issue-${n}`);
  });
  expect(perIssue.some((p) => p.includes(`issue #${PR_ENTRY} `))).toBe(false);

  // Regression — cleanup: work_root no longer exists on disk after the run.
  const workRoot = path.join(
    tmpDir,
    "issue-triage-work",
    "octocat-Hello-World",
  );
  expect(fs.existsSync(workRoot)).toBe(false);

  // The session was closed (the teardown the server relies on).
  expect(mock.closed).toBe(true);
}, 30_000);
