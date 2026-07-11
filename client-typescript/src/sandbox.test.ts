import { describe, expect, it, vi } from "vitest";

const execFileMock = vi.hoisted(() => vi.fn());
vi.mock("node:child_process", () => ({ execFile: execFileMock }));

import { Config } from "./config.js";
import { RpcError } from "./errors.js";
import { Harness } from "./harness.js";
import {
  RemoteMode,
  SandboxSession,
  SandboxTarget,
  runShellCommand,
  runShellNamed,
  shellQuote,
  type ExecResult,
  type SandboxDriver,
  type SandboxHandle,
  type SandboxTool,
} from "./sandbox.js";
import type {
  Transport,
  TransportRequest,
  TransportResponse,
} from "./transport.js";
import type { ContentBlock, JsonRpcFrame, JsonRpcRequest } from "./types.js";

// ===========================================================================
// Test doubles — a fake local driver and an RPC-recording transport, both
// fully offline (no `docker`/`container` binary, no network). Mirrors the
// server tests' recording driver and the Rust SDK's FakeDriver/FakeRpc.
// ===========================================================================

/** A fake local {@link SandboxDriver} recording each call into a shared timeline. */
class FakeDriver implements SandboxDriver {
  constructor(
    private readonly timeline: string[],
    private readonly execStdout = "hi",
  ) {}
  async ensureImage(): Promise<void> {
    this.timeline.push("ensure");
  }
  async start(image: string): Promise<SandboxHandle> {
    this.timeline.push("start");
    return { id: "cid-1", image };
  }
  async exec(_handle: SandboxHandle, command: string): Promise<ExecResult> {
    this.timeline.push(`exec:${command}`);
    return { stdout: this.execStdout, stderr: "", exit_code: 0 };
  }
  async stop(): Promise<void> {
    this.timeline.push("stop");
  }
}

const config = () =>
  new Config({
    serverUrl: "http://test",
    clientKey: "bae_test",
    clientVersion: "9.9.9",
  });

const openOk: TransportResponse = {
  status: 201,
  body: {
    session_id: "ses_1",
    session_key: "bae_ses_1",
    profile: {
      id: "pro_1",
      name: "main",
      allowed_tools: ["run_shell_command"],
      mcp_servers: [],
      provider: { provider: "anthropic", model: "claude-sonnet-4-6" },
    },
  },
};

/**
 * A transport that answers each JSON-RPC method by name (offline) and records
 * every outbound request. `sendQueue` supplies one turn of NDJSON frames per
 * `session.sendMessage`; the sandbox utility RPCs get canned results.
 */
class RpcMock implements Transport {
  readonly requests: TransportRequest[] = [];
  readonly reportCalls: Record<string, unknown>[] = [];
  execResult: ExecResult = { stdout: "remote-out", stderr: "", exit_code: 0 };
  startResult = {
    sandbox_id: "sbx-1",
    image: "python:3.12",
    started_at: "t0",
  };
  stopResult = { stopped: true, image: "python:3.12", sandbox_id: "sbx-1" };
  startError: { code: number; message: string } | undefined;
  sendQueue: JsonRpcFrame[][] = [];

  constructor(private readonly timeline: string[] = []) {}

  async request(req: TransportRequest): Promise<TransportResponse> {
    this.requests.push(structuredClone(req));
    if (req.method === "POST" && req.path === "/api/v1/sessions") return openOk;
    if (req.method === "DELETE") {
      return { status: 200, body: { session_id: "ses_1", state: "closed" } };
    }
    return { status: 200, body: {} };
  }

  async *stream(req: TransportRequest): AsyncIterable<JsonRpcFrame> {
    const body = req.body as JsonRpcRequest;
    const id = body.id;
    const params = (body.params ?? {}) as Record<string, unknown>;
    switch (body.method) {
      case "session.registerDriver":
        yield { jsonrpc: "2.0", id, result: { registered: true } };
        return;
      case "session.reportLocalSandbox":
        this.requests.push(structuredClone(req));
        this.reportCalls.push(params);
        this.timeline.push(`report:${String(params.state)}`);
        yield { jsonrpc: "2.0", id, result: { reported: true } };
        return;
      case "session.execRemoteSandbox":
        this.requests.push(structuredClone(req));
        yield { jsonrpc: "2.0", id, result: this.execResult };
        return;
      case "session.startRemoteSandbox":
        this.requests.push(structuredClone(req));
        if (this.startError) {
          yield { jsonrpc: "2.0", id, error: this.startError };
          return;
        }
        yield { jsonrpc: "2.0", id, result: this.startResult };
        return;
      case "session.stopRemoteSandbox":
        this.requests.push(structuredClone(req));
        yield { jsonrpc: "2.0", id, result: this.stopResult };
        return;
      case "session.sendMessage": {
        this.requests.push(structuredClone(req));
        const frames = this.sendQueue.shift() ?? [];
        for (const f of frames) yield f;
        return;
      }
      default:
        this.requests.push(structuredClone(req));
        return;
    }
  }
}

function toolOf(t: SandboxTool) {
  if (t.kind !== "tool") throw new Error("expected a client-dispatched tool");
  return t.tool;
}

function bindRecordingRpc(
  reports: Array<{
    state: string;
    image: string | null;
    containerId: string | null;
  }>,
) {
  return {
    execRemoteSandbox: async () => ({
      stdout: "remote",
      stderr: "",
      exit_code: 0,
    }),
    reportLocalSandbox: async (
      state: "running" | "stopped" | "error",
      image: string | null,
      containerId: string | null,
    ) => {
      reports.push({ state, image, containerId });
    },
  };
}

function mockHostShell(stdout = "none-out") {
  execFileMock.mockImplementation(
    (
      program: string,
      args: string[],
      _options: unknown,
      callback: (error: Error | null, stdout: string, stderr: string) => void,
    ) => {
      callback(null, stdout, "");
      return undefined;
    },
  );
}

describe("SandboxTarget.none dispatch", () => {
  it("runs run_shell_command through the host shell and never the container driver", async () => {
    const timeline: string[] = [];
    const reports: Array<{
      state: string;
      image: string | null;
      containerId: string | null;
    }> = [];
    const sbx = new SandboxSession();
    sbx.bind(bindRecordingRpc(reports));
    sbx.setLocalDriver(new FakeDriver(timeline));
    execFileMock.mockReset();
    mockHostShell();

    const tool = toolOf(
      runShellCommand(sbx, SandboxTarget.none(), RemoteMode.auto()),
    );
    const result = await tool.handler({ command: "printf none-out" });

    expect(JSON.parse(result as string)).toMatchObject({
      stdout: "none-out",
      stderr: "",
      exit_code: 0,
    });
    expect(execFileMock).toHaveBeenCalledWith(
      "/bin/sh",
      ["-c", "printf none-out"],
      expect.anything(),
      expect.any(Function),
    );
    expect(timeline).toEqual([]);
    expect(reports).toEqual([
      { state: "running", image: null, containerId: null },
      { state: "stopped", image: null, containerId: null },
    ]);
  });

  it("runs run_shell_named through the host shell and never the container driver", async () => {
    const timeline: string[] = [];
    const reports: Array<{
      state: string;
      image: string | null;
      containerId: string | null;
    }> = [];
    const sbx = new SandboxSession();
    sbx.bind(bindRecordingRpc(reports));
    sbx.setLocalDriver(new FakeDriver(timeline));
    execFileMock.mockReset();
    mockHostShell();

    const tool = toolOf(
      runShellNamed(
        sbx,
        "echo_it",
        "echo the name",
        "echo {name}",
        SandboxTarget.none(),
        RemoteMode.auto(),
      ),
    );
    await tool.handler({ name: "hello" });

    expect(execFileMock).toHaveBeenCalledWith(
      "/bin/sh",
      ["-c", "echo 'hello'"],
      expect.anything(),
      expect.any(Function),
    );
    expect(timeline).toEqual([]);
  });
});

// ===========================================================================
// Command-injection resistance — the single most important test.
//
// `echo {name}` interpolated with each classic shell-metacharacter payload must
// yield a final command string in which the WHOLE argument is one literal
// string argument to `echo`. The payload list is IDENTICAL across the three
// SDKs; the expected escaped strings use the hand-rolled `'\''` quoting shared
// by Rust and TS (Python's shlex.quote uses `'"'"'` — same semantics, asserted
// in client-python/tests/test_sandbox_injection.py).
// ===========================================================================

/** `[payload, expected final command]` — identical payloads across all SDKs. */
const INJECTION_CASES: [string, string][] = [
  ["a'; rm -rf / #", "echo 'a'\\''; rm -rf / #'"],
  ["`whoami`", "echo '`whoami`'"],
  ["$(whoami)", "echo '$(whoami)'"],
  ["x && y", "echo 'x && y'"],
  ['he said "hi"', "echo 'he said \"hi\"'"],
];

describe("run_shell_named command-injection resistance", () => {
  it("shell-escapes every payload into one literal argument to echo for local and none targets", async () => {
    for (const [payload, expected] of INJECTION_CASES) {
      for (const target of [
        SandboxTarget.local("alpine"),
        SandboxTarget.none(),
      ]) {
        const timeline: string[] = [];
        const hostCalls: string[] = [];
        const sbx = new SandboxSession();
        sbx.bind(bindRecordingRpc([]));
        sbx.setLocalDriver(new FakeDriver(timeline));
        execFileMock.mockReset();
        execFileMock.mockImplementation(
          (
            _program: string,
            args: string[],
            _options: unknown,
            callback: (
              error: Error | null,
              stdout: string,
              stderr: string,
            ) => void,
          ) => {
            hostCalls.push(args[1]!);
            callback(null, "", "");
            return undefined;
          },
        );
        const tool = toolOf(
          runShellNamed(
            sbx,
            "echo_it",
            "echo the name",
            "echo {name}",
            target,
            RemoteMode.auto(),
          ),
        );
        await tool.handler({ name: payload });

        if (target.type === "local") {
          const exec = timeline.find((e) => e.startsWith("exec:"));
          expect(exec).toBe(`exec:${expected}`);
          expect(hostCalls).toEqual([]);
        } else {
          expect(hostCalls).toEqual([expected]);
          expect(timeline).toEqual([]);
        }
      }
    }
  });

  it("shellQuote wraps the whole value as one literal argument", () => {
    expect(shellQuote("a'; rm -rf / #")).toBe("'a'\\''; rm -rf / #'");
    expect(shellQuote("$(whoami)")).toBe("'$(whoami)'");
    expect(shellQuote("plain")).toBe("'plain'");
  });
});

// ===========================================================================
// Local sandbox lifecycle reporting — verified via the mock transport's
// recorded outbound RPC calls (running before the first exec; stopped at close).
// ===========================================================================

describe("local sandbox lifecycle reporting", () => {
  it("reports running before the first exec and stopped on close", async () => {
    const timeline: string[] = [];
    const mock = new RpcMock(timeline);
    const harness = new Harness(config(), { transport: mock });
    const sbx = harness.sandboxSession();
    sbx.setLocalDriver(new FakeDriver(timeline));
    const tool = runShellCommand(
      sbx,
      SandboxTarget.local("alpine"),
      RemoteMode.auto(),
    );
    harness.registerSandboxTool(tool);

    const session = await harness.connect();
    await toolOf(tool).handler({ command: "echo hi" });
    await session.close();

    const running = timeline.indexOf("report:running");
    const exec = timeline.findIndex((e) => e.startsWith("exec:"));
    const stopped = timeline.indexOf("report:stopped");
    expect(running).toBeGreaterThanOrEqual(0);
    expect(running).toBeLessThan(exec);
    expect(exec).toBeLessThan(stopped);

    // The recorded outbound reportLocalSandbox RPCs: running (with the real
    // container id) then stopped.
    expect(mock.reportCalls[0]).toMatchObject({
      state: "running",
      image: "alpine",
      unsandboxed: false,
      container_id: "cid-1",
    });
    expect(mock.reportCalls.at(-1)).toMatchObject({ state: "stopped" });
  });

  it("sends unsandboxed host lifecycle reports over the real harness transport", async () => {
    const mock = new RpcMock();
    const harness = new Harness(config(), { transport: mock });
    const tool = runShellCommand(
      harness.sandboxSession(),
      SandboxTarget.none(),
      RemoteMode.auto(),
    );
    harness.registerSandboxTool(tool);
    execFileMock.mockReset();
    mockHostShell();

    await harness.connect();
    await toolOf(tool).handler({ command: "printf host" });

    expect(mock.reportCalls).toEqual([
      expect.objectContaining({
        state: "running",
        image: null,
        unsandboxed: true,
      }),
      expect.objectContaining({
        state: "stopped",
        image: null,
        unsandboxed: true,
      }),
    ]);
  });
});

// ===========================================================================
// Cross-SDK sandbox dispatch parity (WI 0006)
//
// The canonical sequences below MUST stay byte-for-byte identical to the arrays
// in the Rust and Python SDK sandbox parity tests:
//   - client-rust/src/harness.rs           (SANDBOX_AUTO_PARITY_SEQUENCE /
//                                            SANDBOX_MANUAL_PARITY_SEQUENCE)
//   - client-python/tests/test_sandbox_parity.py (same two names)
// ===========================================================================

/** Auto dispatch: one server-dispatched turn (mirrors the MCP round-trip). */
const SANDBOX_AUTO_PARITY_SEQUENCE = [
  "provider.request",
  "provider.response",
  "tool.call",
  "sandbox.request",
  "sandbox.response",
  "tool.result",
  "provider.request",
  "provider.response",
  "server.message.send",
];

/** Manual dispatch: assistant pauses with a tool_use, client runs it (issuing
 * execRemoteSandbox out of band — no lifecycle event on success), then sends the
 * tool_result back for a second turn. */
const SANDBOX_MANUAL_PARITY_SEQUENCE = [
  "provider.request",
  "provider.response",
  "server.message.send",
  "client.message.send",
  "provider.request",
  "provider.response",
  "server.message.send",
];

function notif(
  event_type: string,
  payload: Record<string, unknown>,
): JsonRpcFrame {
  return {
    jsonrpc: "2.0",
    method: "session.event",
    params: {
      id: `evt_${event_type}`,
      session_id: "ses_1",
      client_key_id: null,
      event_type,
      payload,
      created_at: "t",
    },
  } as JsonRpcFrame;
}

function terminal(content: ContentBlock[]): JsonRpcFrame {
  return {
    jsonrpc: "2.0",
    id: 1,
    result: { message: { role: "assistant", content }, events: [] },
  };
}

describe("sandbox dispatch cross-SDK parity", () => {
  it("auto dispatch observes the canonical sequence in a single server-dispatched turn", async () => {
    const mock = new RpcMock();
    mock.sendQueue = [
      [
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("tool.call", {
          dispatch: "sandbox",
          name: "run_shell_command",
          input: { command: "echo hi" },
        }),
        notif("sandbox.request", {
          tool: "run_shell_command",
          input: { command: "echo hi" },
          command: "echo hi",
        }),
        notif("sandbox.response", {
          sandbox_id: "cid-1",
          ok: true,
          result: { stdout: "hi\n", stderr: "", exit_code: 0 },
        }),
        notif("tool.result", {
          tool_use_id: "tu_sbx",
          dispatch: "sandbox",
          is_error: false,
          content: [{ type: "text", text: "hi\n" }],
        }),
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [{ type: "text", text: "ran it" }],
        }),
        terminal([{ type: "text", text: "ran it" }]),
      ],
    ];
    const observed: string[] = [];
    const harness = new Harness(config(), { transport: mock }).setHooks({
      on_event: (e) => void observed.push(e.event_type),
    });
    const session = await harness.connect();
    const reply = await session.send("run it");
    expect((reply.content as ContentBlock[])[0]).toMatchObject({
      text: "ran it",
    });
    expect(observed).toEqual(SANDBOX_AUTO_PARITY_SEQUENCE);
    // Server-dispatched: exactly one sendMessage turn.
    const sends = mock.requests.filter(
      (r) => (r.body as JsonRpcRequest).method === "session.sendMessage",
    );
    expect(sends).toHaveLength(1);
  });

  it("manual dispatch observes the canonical sequence and dispatches client-side", async () => {
    const mock = new RpcMock();
    mock.execResult = { stdout: "manual-out", stderr: "", exit_code: 0 };
    mock.sendQueue = [
      [
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "tu_manual",
              name: "run_shell_command",
              input: { command: "ls -la" },
            },
          ],
        }),
        {
          jsonrpc: "2.0",
          id: 1,
          result: {
            message: {
              role: "assistant",
              content: [
                {
                  type: "tool_use",
                  id: "tu_manual",
                  name: "run_shell_command",
                  input: { command: "ls -la" },
                },
              ],
            },
            events: [],
          },
        },
      ],
      [
        notif("client.message.send", {
          role: "user",
          content: [{ type: "tool_result", tool_use_id: "tu_manual" }],
        }),
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [{ type: "text", text: "done" }],
        }),
        terminal([{ type: "text", text: "done" }]),
      ],
    ];
    const harness = new Harness(config(), { transport: mock });
    const sbx = harness.sandboxSession();
    const tool = runShellCommand(
      sbx,
      SandboxTarget.remote(),
      RemoteMode.manual((r) => JSON.stringify(r)),
    );
    harness.registerSandboxTool(tool);
    const observed: string[] = [];
    harness.setHooks({ on_event: (e) => void observed.push(e.event_type) });

    const session = await harness.connect();
    const reply = await session.send("list files");
    expect((reply.content as ContentBlock[])[0]).toMatchObject({
      text: "done",
    });
    expect(observed).toEqual(SANDBOX_MANUAL_PARITY_SEQUENCE);

    // The client harness actually dispatched the tool, issuing the fully
    // interpolated command over session.execRemoteSandbox.
    const exec = mock.requests.find(
      (r) => (r.body as JsonRpcRequest).method === "session.execRemoteSandbox",
    );
    expect(
      ((exec!.body as JsonRpcRequest).params as { command: string }).command,
    ).toBe("ls -la");
    // Manual dispatch pauses: two sendMessage turns.
    const sends = mock.requests.filter(
      (r) => (r.body as JsonRpcRequest).method === "session.sendMessage",
    );
    expect(sends).toHaveLength(2);
  });
});

// ===========================================================================
// Remote start/stop wrappers (D-gap-1) — thin RPC wrappers, verified via the
// recorded outbound calls and error-code surfacing.
// ===========================================================================

describe("start/stop remote sandbox wrappers", () => {
  it("startRemoteSandbox issues the RPC with the image and returns the result", async () => {
    const mock = new RpcMock();
    const session = await new Harness(config(), { transport: mock }).connect();
    const started = await session.startRemoteSandbox("python:3.12");
    expect(started).toEqual(mock.startResult);
    const req = mock.requests.find(
      (r) => (r.body as JsonRpcRequest).method === "session.startRemoteSandbox",
    );
    expect((req!.body as JsonRpcRequest).params).toEqual({
      image: "python:3.12",
    });
  });

  it("surfaces sandbox_image_not_allowed (-32011) as RpcError", async () => {
    const mock = new RpcMock();
    mock.startError = { code: -32011, message: "sandbox_image_not_allowed" };
    const session = await new Harness(config(), { transport: mock }).connect();
    await expect(
      session.startRemoteSandbox("evil:latest"),
    ).rejects.toMatchObject({
      constructor: RpcError,
      code: -32011,
    });
  });

  it("stopRemoteSandbox issues the RPC and returns the result", async () => {
    const mock = new RpcMock();
    const session = await new Harness(config(), { transport: mock }).connect();
    const stopped = await session.stopRemoteSandbox();
    expect(stopped).toEqual(mock.stopResult);
    const req = mock.requests.find(
      (r) => (r.body as JsonRpcRequest).method === "session.stopRemoteSandbox",
    );
    expect((req!.body as JsonRpcRequest).params).toEqual({});
  });
});
