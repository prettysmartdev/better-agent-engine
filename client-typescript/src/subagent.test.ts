import { describe, expect, it } from "vitest";

import { Config } from "./config.js";
import { Harness } from "./harness.js";
import { SandboxTarget } from "./sandbox.js";
import {
  LAUNCH_SUBAGENT_TOOL,
  LOCAL_SUBAGENT_STATUS_TOOL,
  ProcessRunner,
  SUBAGENT_OUTPUT_CAP_BYTES,
  SubagentLaunch,
  SubagentSession,
  launchSubagent,
  type LocalSubagentReport,
  type PromptDelivery,
  type RunnerOutput,
  type SubagentDef,
  type SubagentRpc,
  type SubagentRunner,
  type SubagentTool,
  type SubagentToolDef,
} from "./subagent.js";
import type { ToolDefinition } from "./tool.js";
import type {
  Transport,
  TransportRequest,
  TransportResponse,
} from "./transport.js";
import type { ContentBlock, JsonRpcFrame, JsonRpcRequest } from "./types.js";
import { messageText } from "./types.js";

// ===========================================================================
// Test doubles.
//
// This file ports `client-rust/src/subagent.rs`'s `#[cfg(test)] mod tests`
// (see `/awman/context/workflow/reference-rust-subagent.rs`) and the
// cross-SDK local-subagent parity section of `client-rust/src/harness.rs`.
// Everything below stays fully offline: no real `claude`/`codex` binary, no
// network — mirrors the Rust `FakeRunner`/`FakeSubagentRpc`/`Notify` pattern
// and this repo's own `sandbox.test.ts` `FakeDriver`/`RpcMock` convention.
// ===========================================================================

/**
 * A tiny `tokio::sync::Notify`-equivalent: `notify()` wakes the next waiter,
 * or — if nobody is waiting yet — stores a single permit so a subsequent
 * `notified()` resolves immediately. This is the deterministic,
 * promise-based synchronization primitive the fake runner/RPC doubles use to
 * let a test await the detached watcher's terminal report before asserting
 * on it, instead of a flaky `setTimeout`-based sleep.
 */
class Notify {
  private waiters: Array<() => void> = [];
  private permit = false;

  notify(): void {
    const waiter = this.waiters.shift();
    if (waiter !== undefined) {
      waiter();
    } else {
      this.permit = true;
    }
  }

  notified(): Promise<void> {
    if (this.permit) {
      this.permit = false;
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      this.waiters.push(resolve);
    });
  }
}

type FakeOutcome =
  | { kind: "ok"; output: RunnerOutput }
  | { kind: "err"; message: string }
  | { kind: "pending" };

/**
 * A fake {@link SubagentRunner} recording every `(program, args, stdin)` call,
 * optionally gated behind a {@link Notify} so a test controls exactly when
 * the fake subprocess "exits", and optionally invoking `onAbort` when its
 * signal fires — the TS analogue of the Rust `DropMarker` used to prove a
 * watcher was truly killed (via the runner's required `AbortSignal`) rather
 * than merely abandoned.
 */
class FakeRunner implements SubagentRunner {
  readonly calls: Array<{
    program: string;
    args: string[];
    stdin: Uint8Array | null;
  }> = [];

  constructor(
    private readonly outcome: FakeOutcome,
    private readonly gate?: Notify,
    private readonly onAbort?: () => void,
  ) {}

  async run(
    program: string,
    args: string[],
    stdin: Uint8Array | null,
    signal: AbortSignal,
  ): Promise<RunnerOutput> {
    this.calls.push({ program, args, stdin });
    if (this.onAbort !== undefined) {
      signal.addEventListener("abort", this.onAbort, { once: true });
    }
    if (this.gate !== undefined) {
      await this.gate.notified();
    }
    if (this.outcome.kind === "ok") return this.outcome.output;
    if (this.outcome.kind === "err") throw new Error(this.outcome.message);
    // "pending": never resolves on its own — only a killed/aborted watcher
    // stops waiting on it.
    return new Promise<RunnerOutput>(() => undefined);
  }
}

/**
 * A recording {@link SubagentRpc}: mirrors the Rust `FakeSubagentRpc`. Fires
 * `terminalNotify` on every terminal (`completed`/`failed`/`cancelled`)
 * report — the test's synchronization point for the detached watcher.
 */
class RecordingSubagentRpc implements SubagentRpc {
  readonly reports: LocalSubagentReport[] = [];
  readonly updateCalls: Array<Array<Record<string, unknown>>> = [];
  readonly cancelCalls: string[] = [];
  readonly terminalNotify = new Notify();

  async reportLocalSubagent(report: LocalSubagentReport): Promise<void> {
    this.reports.push(report);
    if (
      report.state === "completed" ||
      report.state === "failed" ||
      report.state === "cancelled"
    ) {
      this.terminalNotify.notify();
    }
  }

  async updateClientTools(
    tools: Array<Record<string, unknown>>,
  ): Promise<void> {
    this.updateCalls.push(tools);
  }

  async cancelSubagent(subagentId: string): Promise<unknown> {
    this.cancelCalls.push(subagentId);
    return { cancelled: true, subagent_id: subagentId, was_running: true };
  }
}

function sessionWithRpc(): {
  session: SubagentSession;
  rpc: RecordingSubagentRpc;
} {
  const session = new SubagentSession();
  const rpc = new RecordingSubagentRpc();
  session.bind(rpc);
  session.setBaseClientTools([
    { name: LAUNCH_SUBAGENT_TOOL, description: "launch", input_schema: {} },
  ]);
  return { session, rpc };
}

async function callTool(
  tool: ToolDefinition,
  input: Record<string, unknown>,
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
): Promise<any> {
  const raw = await tool.handler(input);
  return JSON.parse(raw as string);
}

function toolOf(t: SubagentTool): ToolDefinition {
  if (t.kind !== "tool") throw new Error("expected a client-dispatched tool");
  return t.tool;
}

function defOf(t: SubagentTool): SubagentToolDef {
  if (t.kind !== "def")
    throw new Error("expected a declaration-only subagent tool");
  return t.def;
}

function claudeDef(template: string, via: PromptDelivery): SubagentDef {
  return { harness: "claude", commandTemplate: template, promptVia: via };
}

// ===========================================================================
// 2. Shell-escaping for {model}/{prompt}, parametrized Arg vs. Stdin.
//
// IDENTICAL payload list to `sandbox.test.ts`'s `INJECTION_CASES` (do not
// invent new payloads — cross-SDK/cross-module parity depends on this list
// staying byte-for-byte the same everywhere it appears).
// ===========================================================================

const INJECTION_CASES: [string, string][] = [
  ["a'; rm -rf / #", "echo 'a'\\''; rm -rf / #'"],
  ["`whoami`", "echo '`whoami`'"],
  ["$(whoami)", "echo '$(whoami)'"],
  ["x && y", "echo 'x && y'"],
  ['he said "hi"', "echo 'he said \"hi\"'"],
];

describe("shell-escaping for {model}/{prompt} substitution", () => {
  // Ports Rust's `arg_mode_shell_escapes_every_injection_payload_into_argv`.
  it("arg mode shell-escapes every injection payload into argv", async () => {
    for (const [payload, expected] of INJECTION_CASES) {
      const { session, rpc } = sessionWithRpc();
      const runner = new FakeRunner({
        kind: "ok",
        output: { stdout: "", stderr: "", exit_code: 0 },
      });
      session.setRunner(runner);
      const tool = toolOf(
        launchSubagent(
          session,
          [claudeDef("echo {model} {prompt}", "arg")],
          SubagentLaunch.local(SandboxTarget.none()),
        ),
      );

      await callTool(tool, {
        harness: "claude",
        model: payload,
        prompt: payload,
      });
      // Wait for the detached watcher to actually invoke the runner and
      // report its terminal state before inspecting the call log.
      await rpc.terminalNotify.notified();

      expect(runner.calls).toHaveLength(1);
      const call = runner.calls[0]!;
      expect(call.program).toBe("/bin/sh");
      expect(call.args).toEqual([
        "-c",
        `${expected} ${expected.slice("echo ".length)}`,
      ]);
      expect(call.stdin).toBeNull();
    }
  });

  // Ports Rust's `stdin_mode_never_places_the_raw_prompt_in_argv`.
  it("stdin mode never places the raw prompt in argv", async () => {
    for (const [payload] of INJECTION_CASES) {
      const prompt = `prompt:\n${payload}`;
      const { session, rpc } = sessionWithRpc();
      const runner = new FakeRunner({
        kind: "ok",
        output: { stdout: "", stderr: "", exit_code: 0 },
      });
      session.setRunner(runner);
      // No `{prompt}` placeholder at all under Stdin (construction would
      // throw otherwise) — the command is fixed regardless of payload.
      const tool = toolOf(
        launchSubagent(
          session,
          [claudeDef("cat --model {model}", "stdin")],
          SubagentLaunch.local(SandboxTarget.none()),
        ),
      );

      await callTool(tool, {
        harness: "claude",
        model: payload,
        prompt,
      });
      await rpc.terminalNotify.notified();

      expect(runner.calls).toHaveLength(1);
      const call = runner.calls[0]!;
      expect(call.program).toBe("/bin/sh");
      const expected = INJECTION_CASES.find(([value]) => value === payload)![1];
      expect(call.args).toEqual([
        "-c",
        `cat --model ${expected.slice("echo ".length)}`,
      ]);
      expect(call.args.some((arg) => arg.includes(prompt))).toBe(false);
      // The raw (unescaped) prompt reaches the child only via stdin.
      expect(call.stdin).not.toBeNull();
      expect(Buffer.from(call.stdin!).toString("utf8")).toBe(prompt);
    }
  });
});

// ===========================================================================
// 3. Immediate-return contract: `launch_subagent`'s result is exactly the
//    pinned four-key `{"status":"started",...}` shape, never the subagent's
//    output — even when the fake subprocess has already produced output.
// ===========================================================================

describe("launch_subagent immediate-return contract", () => {
  // Ports Rust's `launch_result_is_exactly_the_started_shape_never_output`.
  it("returns exactly the started shape, never the subagent's output", async () => {
    const { session } = sessionWithRpc();
    const runner = new FakeRunner({
      kind: "ok",
      output: {
        stdout: "SECRET_SUBAGENT_OUTPUT",
        stderr: "SECRET_STDERR",
        exit_code: 0,
      },
    });
    session.setRunner(runner);
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );

    const raw = (await tool.handler({
      harness: "claude",
      model: "claude-sonnet-5",
      prompt: "hi",
    })) as string;
    const result = JSON.parse(raw) as Record<string, unknown>;

    // Key order pinned per the contract: subagent_id, harness, model, status.
    expect(Object.keys(result)).toEqual([
      "subagent_id",
      "harness",
      "model",
      "status",
    ]);
    expect(result.harness).toBe("claude");
    expect(result.model).toBe("claude-sonnet-5");
    expect(result.status).toBe("started");
    expect((result.subagent_id as string).startsWith("sba_")).toBe(true);
    expect(raw).not.toContain("SECRET_SUBAGENT_OUTPUT");
    expect(raw).not.toContain("SECRET_STDERR");
  });
});

// ===========================================================================
// 1. Status-tool visibility: `updateClientTools` fires exactly on the
//    empty->non-empty / non-empty->empty transitions, never redundantly.
// ===========================================================================

describe("local_subagent_status visibility (updateClientTools transitions)", () => {
  // Ports Rust's `update_client_tools_fires_exactly_on_transitions_never_redundantly`.
  it("fires once on the first launch and does not re-send on a second concurrent launch", async () => {
    const { session, rpc } = sessionWithRpc();
    // A runner that never settles on its own — we only care about the
    // launch-side transition here, not completion.
    const runner = new FakeRunner({ kind: "pending" });
    session.setRunner(runner);
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );

    expect(rpc.updateCalls).toHaveLength(0);

    // First launch: empty -> non-empty. Fires once, includes the status tool.
    await callTool(tool, { harness: "claude", model: "m", prompt: "first" });
    expect(rpc.updateCalls).toHaveLength(1);
    expect(
      rpc.updateCalls[0]!.some((t) => t.name === LOCAL_SUBAGENT_STATUS_TOOL),
    ).toBe(true);
    expect(
      rpc.updateCalls[0]!.some((t) => t.name === LAUNCH_SUBAGENT_TOOL),
    ).toBe(true);

    // Second concurrent launch: non-empty -> non-empty. No re-send.
    await callTool(tool, { harness: "claude", model: "m", prompt: "second" });
    expect(rpc.updateCalls).toHaveLength(1);
  });

  // Ports Rust's `terminal_entry_reported_once_then_evicted_and_unknown_id_errors`
  // (full launch -> poll -> completion cycle, including the resulting
  // non-empty->empty `updateClientTools` transition).
  it("reports a terminal entry exactly once then evicts it, firing the removal transition; unknown ids error", async () => {
    const { session, rpc } = sessionWithRpc();
    const statusTool = session.statusTool();

    // Unknown id before anything was ever launched.
    const errBefore = await callTool(statusTool, {
      subagent_id: "sba_doesnotexist",
    });
    expect(errBefore).toEqual({ error: "unknown subagent_id" });

    const gate = new Notify();
    const runner = new FakeRunner(
      { kind: "ok", output: { stdout: "done", stderr: "", exit_code: 0 } },
      gate,
    );
    session.setRunner(runner);
    const launchTool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );

    const started = await callTool(launchTool, {
      harness: "claude",
      model: "m",
      prompt: "hi",
    });
    const id = started.subagent_id as string;

    // While running: listed, but not evicted, no output yet.
    const running = await callTool(statusTool, {});
    expect(running.subagents).toHaveLength(1);
    expect(running.subagents[0].status).toBe("running");
    expect(running.subagents[0].stdout).toBeNull();

    // Let the fake subprocess "exit" and wait for the watcher's terminal report.
    gate.notify();
    await rpc.terminalNotify.notified();

    // First poll after completion: included exactly once, terminal.
    const first = await callTool(statusTool, {});
    expect(first.subagents).toHaveLength(1);
    expect(first.subagents[0].subagent_id).toBe(id);
    expect(first.subagents[0].status).toBe("completed");
    expect(first.subagents[0].stdout).toBe("done");

    // Second poll: the map has emptied — omitted entirely (evict-on-report).
    const second = await callTool(statusTool, {});
    expect(second.subagents).toEqual([]);

    // Querying that id now answers the unknown-id error.
    const byId = await callTool(statusTool, { subagent_id: id });
    expect(byId).toEqual({ error: "unknown subagent_id" });

    // The evicting read fired the non-empty->empty updateClientTools
    // removal: the last update no longer includes the status tool.
    expect(rpc.updateCalls).toHaveLength(2);
    expect(
      rpc.updateCalls[0]!.some((t) => t.name === LOCAL_SUBAGENT_STATUS_TOOL),
    ).toBe(true);
    expect(
      rpc.updateCalls[1]!.some((t) => t.name === LOCAL_SUBAGENT_STATUS_TOOL),
    ).toBe(false);
  });
});

// ===========================================================================
// 7. Truncation, and the bonus spawn-failure gap-closer.
// ===========================================================================

describe("captured output truncation", () => {
  // Ports Rust's `output_past_the_cap_is_truncated_and_flagged`.
  it("caps output at SUBAGENT_OUTPUT_CAP_BYTES and flags truncated:true", async () => {
    const { session, rpc } = sessionWithRpc();
    const huge = "a".repeat(SUBAGENT_OUTPUT_CAP_BYTES + 1000);
    const runner = new FakeRunner({
      kind: "ok",
      output: { stdout: huge, stderr: "", exit_code: 0 },
    });
    session.setRunner(runner);
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    await callTool(tool, { harness: "claude", model: "m", prompt: "hi" });
    await rpc.terminalNotify.notified();

    const status = session.statusTool();
    const result = await callTool(status, {});
    const entry = result.subagents[0];
    expect(entry.truncated).toBe(true);
    expect((entry.stdout as string).length).toBe(SUBAGENT_OUTPUT_CAP_BYTES);
    expect(huge.startsWith(entry.stdout as string)).toBe(true);
    expect((entry.stdout as string).length).toBeLessThan(huge.length);
  });

  // Ports Rust's `spawn_failure_reports_failed_with_spawn_failed_reason` — a
  // cheap gap-closer: a runner that rejects surfaces as
  // `failed{reason:"spawn_failed", exit_code:null}`, never a dropped turn.
  it("a spawn/io failure reports failed with reason spawn_failed and a null exit code", async () => {
    const { session, rpc } = sessionWithRpc();
    const runner = new FakeRunner({ kind: "err", message: "no such file" });
    session.setRunner(runner);
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    await callTool(tool, { harness: "claude", model: "m", prompt: "hi" });
    await rpc.terminalNotify.notified();

    const status = session.statusTool();
    const result = await callTool(status, {});
    const entry = result.subagents[0];
    expect(entry.status).toBe("failed");
    expect(entry.reason).toBe("spawn_failed");
    expect(entry.exit_code).toBeNull();
    expect((entry.detail as string).includes("no such file")).toBe(true);
  });
});

describe("timeout and explicit cancellation", () => {
  it("aborts timed-out work and reports timed_out through status", async () => {
    const { session, rpc } = sessionWithRpc();
    let aborted = false;
    session.setRunner(
      new FakeRunner({ kind: "pending" }, undefined, () => {
        aborted = true;
      }),
    );
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat", timeoutSecs: 0 }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    await callTool(tool, { harness: "claude", model: "m", prompt: "p" });
    await rpc.terminalNotify.notified();
    expect(aborted).toBe(true);
    const result = await callTool(session.statusTool(), {});
    expect(result.subagents[0].status).toBe("timed_out");
    expect(result.subagents[0].reason).toBe("timeout");
    expect(rpc.reports.map((report) => report.state)).toEqual([
      "start",
      "running",
      "failed",
    ]);
  });

  it("aborts explicitly cancelled work and keeps it visible until status", async () => {
    const { session, rpc } = sessionWithRpc();
    let aborted = false;
    session.setRunner(
      new FakeRunner({ kind: "pending" }, undefined, () => {
        aborted = true;
      }),
    );
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    const started = await callTool(tool, {
      harness: "claude",
      model: "m",
      prompt: "p",
    });
    await Promise.resolve();
    await session.cancelSubagent(started.subagent_id as string);
    expect(aborted).toBe(true);
    const result = await callTool(session.statusTool(), {
      subagent_id: started.subagent_id,
    });
    expect(result.subagents[0].status).toBe("cancelled");
    expect(result.subagents[0].reason).toBe("explicit");
  });
});

// ===========================================================================
// 5. Session close teardown: a still-running local subagent is killed (its
//    watcher's AbortSignal truly fires) and reported
//    `cancelled{reason:"session_close"}`.
// ===========================================================================

describe("session close teardown", () => {
  // Ports Rust's `close_all_kills_running_subagent_and_reports_session_close`,
  // adapted to TS's required AbortController/AbortSignal runner contract.
  it("kills a running local subagent and reports session_close on closeAll", async () => {
    const { session, rpc } = sessionWithRpc();
    let aborted = false;
    const runner: SubagentRunner = {
      run: (_program, _args, _stdin, signal) => {
        signal.addEventListener("abort", () => {
          aborted = true;
        });
        return new Promise<RunnerOutput>(() => undefined);
      },
    };
    session.setRunner(runner);
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    await callTool(tool, { harness: "claude", model: "m", prompt: "hi" });

    // Give the fire-and-forget watcher a couple of microtask ticks to have
    // actually invoked the runner (it does so synchronously off the launch
    // call in this implementation, but this stays robust either way).
    await Promise.resolve();
    await Promise.resolve();

    expect(aborted).toBe(false);

    await session.closeAll();

    expect(aborted).toBe(true);
    expect(
      rpc.reports.some(
        (r) => r.state === "cancelled" && r.reason === "session_close",
      ),
    ).toBe(true);
    // Launch fired the empty->non-empty transition; closeAll fires the
    // non-empty->empty removal.
    expect(rpc.updateCalls).toHaveLength(2);
    expect(
      rpc.updateCalls[1]!.some((t) => t.name === LOCAL_SUBAGENT_STATUS_TOOL),
    ).toBe(false);
  });
});

describe("adversarial launch and lifecycle interleavings", () => {
  it("fails launch and status tool calls before transport binding", async () => {
    const session = new SubagentSession();
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    await expect(
      tool.handler({ harness: "claude", model: "m", prompt: "p" }),
    ).rejects.toThrow("before the session was connected");
    expect(() => session.statusTool().handler({})).toThrow(
      "before the session was connected",
    );
  });

  it("preserves prompt and model boundary whitespace in arg and stdin modes", async () => {
    const prompt = "  keep this indentation\n";
    const model = " model-with-spaces ";
    for (const config of [
      {
        harness: "claude",
        commandTemplate: "cli --model {model}",
        promptVia: "stdin" as const,
      },
      {
        harness: "claude",
        commandTemplate: "cli --model {model} --prompt {prompt}",
        promptVia: "arg" as const,
      },
    ]) {
      const { session, rpc } = sessionWithRpc();
      const runner = new FakeRunner({
        kind: "ok",
        output: { stdout: "", stderr: "", exit_code: 0 },
      });
      session.setRunner(runner);
      const tool = toolOf(
        launchSubagent(
          session,
          [config],
          SubagentLaunch.local(SandboxTarget.none()),
        ),
      );
      const started = await callTool(tool, {
        harness: "claude",
        model,
        prompt,
      });
      expect(started.model).toBe(model);
      await rpc.terminalNotify.notified();
      const call = runner.calls[0]!;
      expect(call.args.at(-1)).toContain("' model-with-spaces '");
      if (config.promptVia === "stdin") {
        expect(Buffer.from(call.stdin!).toString()).toBe(prompt);
        expect(call.args.at(-1)).not.toContain(prompt);
      } else {
        expect(call.stdin).toBeNull();
        expect(call.args.at(-1)).toContain("'  keep this indentation\n'");
      }
      await session.closeAll();
    }
  });

  it("reserves only eight of nine genuinely concurrent launches", async () => {
    let releaseStart!: () => void;
    const startGate = new Promise<void>((resolve) => {
      releaseStart = resolve;
    });
    const firstStart = new Notify();
    class BlockingStartRpc extends RecordingSubagentRpc {
      released = false;

      override async reportLocalSubagent(
        report: LocalSubagentReport,
      ): Promise<void> {
        await super.reportLocalSubagent(report);
        if (report.state === "start" && !this.released) {
          firstStart.notify();
          await startGate;
        }
      }
    }
    const session = new SubagentSession();
    const rpc = new BlockingStartRpc();
    session.bind(rpc);
    session.setBaseClientTools([]);
    session.setRunner(new FakeRunner({ kind: "pending" }));
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    const launches = Array.from({ length: 9 }, (_, index) =>
      callTool(tool, { harness: "claude", model: "m", prompt: `p${index}` }),
    );
    await firstStart.notified();
    await Promise.resolve();
    await Promise.resolve();
    rpc.released = true;
    releaseStart();
    const results = await Promise.all(launches);
    expect(
      results.filter((result) => result.status === "started"),
    ).toHaveLength(8);
    expect(
      results.filter((result) =>
        String(result.error).includes("limit reached"),
      ),
    ).toHaveLength(1);
    expect(rpc.updateCalls).toHaveLength(1);
    await session.closeAll();
  });

  it("does not let terminal telemetry overtake a delayed running report", async () => {
    let releaseRunning!: () => void;
    const runningGate = new Promise<void>((resolve) => {
      releaseRunning = resolve;
    });
    const runningEntered = new Notify();
    class BlockingRunningRpc extends RecordingSubagentRpc {
      override async reportLocalSubagent(
        report: LocalSubagentReport,
      ): Promise<void> {
        await super.reportLocalSubagent(report);
        if (report.state === "running") {
          runningEntered.notify();
          await runningGate;
        }
      }
    }
    const session = new SubagentSession();
    const rpc = new BlockingRunningRpc();
    session.bind(rpc);
    session.setBaseClientTools([]);
    session.setRunner(
      new FakeRunner({
        kind: "ok",
        output: { stdout: "done", stderr: "", exit_code: 0 },
      }),
    );
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    const launch = tool.handler({ harness: "claude", model: "m", prompt: "p" });
    await runningEntered.notified();
    await Promise.resolve();
    expect(rpc.reports.map((report) => report.state)).toEqual([
      "start",
      "running",
    ]);
    releaseRunning();
    await launch;
    await rpc.terminalNotify.notified();
    expect(rpc.reports.map((report) => report.state)).toEqual([
      "start",
      "running",
      "completed",
    ]);
    await session.closeAll();
  });

  it("does not let an evicting removal commit after a new launch", async () => {
    let releaseRemove!: () => void;
    const removeGate = new Promise<void>((resolve) => {
      releaseRemove = resolve;
    });
    const removeStarted = new Notify();
    class BlockingRemoveRpc extends RecordingSubagentRpc {
      released = false;

      override async updateClientTools(
        tools: Array<Record<string, unknown>>,
      ): Promise<void> {
        this.updateCalls.push(tools);
        if (
          !tools.some((tool) => tool.name === LOCAL_SUBAGENT_STATUS_TOOL) &&
          !this.released
        ) {
          removeStarted.notify();
          await removeGate;
        }
      }
    }
    const session = new SubagentSession();
    const rpc = new BlockingRemoveRpc();
    session.bind(rpc);
    session.setBaseClientTools([]);
    session.setRunner(
      new FakeRunner({
        kind: "ok",
        output: { stdout: "done", stderr: "", exit_code: 0 },
      }),
    );
    const tool = toolOf(
      launchSubagent(
        session,
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.none()),
      ),
    );
    await callTool(tool, { harness: "claude", model: "m", prompt: "first" });
    await rpc.terminalNotify.notified();

    const eviction = callTool(session.statusTool(), {});
    await removeStarted.notified();
    let relaunched = false;
    const relaunch = callTool(tool, {
      harness: "claude",
      model: "m",
      prompt: "second",
    }).then((result) => {
      relaunched = true;
      return result;
    });
    await Promise.resolve();
    await Promise.resolve();
    expect(relaunched).toBe(false);
    expect(rpc.updateCalls).toHaveLength(2);

    rpc.released = true;
    releaseRemove();
    await eviction;
    await relaunch;
    expect(rpc.updateCalls).toHaveLength(3);
    expect(
      rpc.updateCalls[1]!.some(
        (toolDecl) => toolDecl.name === LOCAL_SUBAGENT_STATUS_TOOL,
      ),
    ).toBe(false);
    expect(
      rpc.updateCalls[2]!.some(
        (toolDecl) => toolDecl.name === LOCAL_SUBAGENT_STATUS_TOOL,
      ),
    ).toBe(true);
    await session.closeAll();
  });

  it("production capture retains only cap plus one truncation marker", async () => {
    const runner = new ProcessRunner();
    const output = await runner.run(
      process.execPath,
      [
        "-e",
        `process.stdout.write("x".repeat(${SUBAGENT_OUTPUT_CAP_BYTES + 50_000}))`,
      ],
      null,
      new AbortController().signal,
    );
    expect(Buffer.byteLength(output.stdout)).toBe(
      SUBAGENT_OUTPUT_CAP_BYTES + 1,
    );
  });
});

// ===========================================================================
// 6. Remote-shape safety: no remote-unsandboxed `SubagentLaunch` value is
//    constructible/expressible — `SubagentLaunch.remote(image)` always yields
//    a `{kind:"def"}` declaration-only shape, never a callable tool.
// ===========================================================================

describe("remote launch shape safety", () => {
  // Ports Rust's `remote_launch_is_always_sandboxed_and_never_a_client_tool`.
  it("SubagentLaunch.remote always yields a declaration-only def, never a client tool", () => {
    const session = new SubagentSession();
    const tool = launchSubagent(
      session,
      [
        {
          harness: "claude",
          commandTemplate: "claude --model {model} --print {prompt}",
          promptVia: "arg",
        },
      ],
      SubagentLaunch.remote("bae-subagents:latest"),
    );
    // The ONLY constructible remote shape carries an image; there is no
    // "remote unsandboxed" value the type permits (`SubagentLaunch`'s
    // "remote" arm has exactly one field: `image: string`).
    const def = defOf(tool);
    expect(def.image).toBe("bae-subagents:latest");
    expect(def.subagents[0]!.harness).toBe("claude");

    const session2 = new SubagentSession();
    const tool2 = launchSubagent(
      session2,
      [
        {
          harness: "claude",
          commandTemplate: "claude --print {prompt}",
          promptVia: "arg",
        },
      ],
      SubagentLaunch.remote("img"),
    );
    expect(tool2.kind).toBe("def");
    expect(() => toolOf(tool2)).toThrow();
  });

  it("rejects stdin delivery for a local launch targeting the remote sandbox", () => {
    expect(() =>
      launchSubagent(
        new SubagentSession(),
        [{ harness: "claude", commandTemplate: "cat" }],
        SubagentLaunch.local(SandboxTarget.remote()),
      ),
    ).toThrow("execRemoteSandbox carries no stdin");
  });
});

// ===========================================================================
// 8. Cross-SDK local-subagent parity (WI 0010)
//
// The three client SDKs must observe an IDENTICAL ordered live event
// sequence for the same scripted local launch -> poll(running) ->
// poll(completed) flow, driven entirely through client-dispatched tools (no
// server-side subagent dispatch is exercised here — that is the server
// suite's job). MUST stay byte-for-byte identical to:
//   - client-rust/src/harness.rs                   (LOCAL_SUBAGENT_PARITY_SEQUENCE)
//   - client-python/tests/test_subagent_parity.py   (same name)
// ===========================================================================

/** The full observed sequence across two `send()` calls. */
const LOCAL_SUBAGENT_PARITY_SEQUENCE = [
  "provider.request",
  "provider.response",
  "server.message.send", // assistant: tool_use launch_subagent
  "client.message.send", // tool_result: {"status":"started",...}
  "provider.request",
  "provider.response",
  "server.message.send", // assistant: tool_use local_subagent_status
  "client.message.send", // tool_result: {"subagents":[{"status":"running",...}]}
  "provider.request",
  "provider.response",
  "server.message.send", // assistant: final text; first send() ends
  "provider.request",
  "provider.response",
  "server.message.send", // assistant: tool_use local_subagent_status (2nd send())
  "client.message.send", // tool_result: {"subagents":[{"status":"completed",...}]}
  "provider.request",
  "provider.response",
  "server.message.send", // assistant: final text; second send() ends
];

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
      allowed_tools: [LAUNCH_SUBAGENT_TOOL, LOCAL_SUBAGENT_STATUS_TOOL],
      mcp_servers: [],
      provider: { provider: "anthropic", model: "claude-sonnet-4-6" },
    },
  },
};

/**
 * A transport that answers each JSON-RPC method by name (offline) and
 * records every outbound request — the same `RpcMock` convention
 * `sandbox.test.ts` uses, extended with the three subagent RPC methods.
 * `sendQueue` supplies one turn of NDJSON frames per `session.sendMessage`.
 */
class RpcMock implements Transport {
  readonly requests: TransportRequest[] = [];
  readonly updateClientToolsCalls: Array<Array<Record<string, unknown>>> = [];
  readonly reportLocalSubagentCalls: Record<string, unknown>[] = [];
  readonly terminalNotify = new Notify();
  sendQueue: JsonRpcFrame[][] = [];

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
      case "session.reportLocalSubagent":
        this.requests.push(structuredClone(req));
        this.reportLocalSubagentCalls.push(params);
        if (
          params.state === "completed" ||
          params.state === "failed" ||
          params.state === "cancelled"
        ) {
          this.terminalNotify.notify();
        }
        yield { jsonrpc: "2.0", id, result: { reported: true } };
        return;
      case "session.updateClientTools":
        this.requests.push(structuredClone(req));
        this.updateClientToolsCalls.push(
          params.tools as Array<Record<string, unknown>>,
        );
        yield { jsonrpc: "2.0", id, result: { updated: true } };
        return;
      case "session.cancelSubagent":
        this.requests.push(structuredClone(req));
        yield {
          jsonrpc: "2.0",
          id,
          result: {
            cancelled: true,
            subagent_id: params.subagent_id,
            was_running: true,
          },
        };
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

/** A {@link SubagentRunner} whose single subprocess blocks on a {@link Notify}
 * gate until the test releases it, so the launch -> poll(running) ->
 * poll(completed) ordering is deterministic. */
class GatedRunner implements SubagentRunner {
  constructor(private readonly gate: Notify) {}

  async run(): Promise<RunnerOutput> {
    await this.gate.notified();
    return { stdout: "subagent done", stderr: "", exit_code: 0 };
  }
}

describe("cross-SDK local-subagent parity", () => {
  // Ports Rust's `local_subagent_scenario_matches_canonical_sequence_across_two_sends`.
  it("observes the canonical sequence across two send() calls and matches structurally", async () => {
    const mock = new RpcMock();
    mock.sendQueue = [
      // Turn A1: assistant launches a subagent.
      [
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "tu_launch",
              name: "launch_subagent",
              input: {
                harness: "claude",
                model: "claude-sonnet-5",
                prompt: "do the task",
              },
            },
          ],
        }),
        terminal([
          {
            type: "tool_use",
            id: "tu_launch",
            name: "launch_subagent",
            input: {
              harness: "claude",
              model: "claude-sonnet-5",
              prompt: "do the task",
            },
          },
        ]),
      ],
      // Turn A2: assistant polls; the fake subprocess is still gated.
      [
        notif("client.message.send", {
          role: "user",
          content: [{ type: "tool_result", tool_use_id: "tu_launch" }],
        }),
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "tu_poll1",
              name: "local_subagent_status",
              input: {},
            },
          ],
        }),
        terminal([
          {
            type: "tool_use",
            id: "tu_poll1",
            name: "local_subagent_status",
            input: {},
          },
        ]),
      ],
      // Turn A3: assistant reports back and the first send() ends.
      [
        notif("client.message.send", {
          role: "user",
          content: [{ type: "tool_result", tool_use_id: "tu_poll1" }],
        }),
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [{ type: "text", text: "still running, I'll check back" }],
        }),
        terminal([{ type: "text", text: "still running, I'll check back" }]),
      ],
      // Turn B1: a fresh send() polls again; by now the subagent completed.
      [
        notif("provider.request", { attempt: 0 }),
        notif("provider.response", { ok: true, status: 200 }),
        notif("server.message.send", {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "tu_poll2",
              name: "local_subagent_status",
              input: {},
            },
          ],
        }),
        terminal([
          {
            type: "tool_use",
            id: "tu_poll2",
            name: "local_subagent_status",
            input: {},
          },
        ]),
      ],
      // Turn B2: assistant reports completion; second send() ends.
      [
        notif("client.message.send", {
          role: "user",
          content: [{ type: "tool_result", tool_use_id: "tu_poll2" }],
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

    const gate = new Notify();
    const harness = new Harness(config(), { transport: mock });
    harness.subagentSession().setRunner(new GatedRunner(gate));
    const launchTool = launchSubagent(
      harness.subagentSession(),
      [{ harness: "claude", commandTemplate: "cat" }],
      SubagentLaunch.local(SandboxTarget.none()),
    );
    harness.registerSubagentTool(launchTool);

    const observed: string[] = [];
    harness.setHooks({ on_event: (e) => void observed.push(e.event_type) });

    const session = await harness.connect();

    // First send(): launch, then poll while still running.
    const out1 = await session.send("please launch a subagent");
    expect(messageText(out1)).toBe("still running, I'll check back");

    // Let the fake subprocess "exit" and wait for the watcher's terminal report.
    gate.notify();
    await mock.terminalNotify.notified();

    // Second send(): poll again, now completed.
    const out2 = await session.send("check again");
    expect(messageText(out2)).toBe("done");

    expect(observed).toEqual(LOCAL_SUBAGENT_PARITY_SEQUENCE);

    // Structural parity of the actual tool_result content exchanged with the
    // server at each turn (not just the event-type skeleton) — per the
    // contract's "structural comparison, not raw bytes" note.
    const sends = mock.requests.filter(
      (r) => (r.body as JsonRpcRequest).method === "session.sendMessage",
    );
    expect(sends).toHaveLength(5);
    const toolResultContent = (turn: number): Record<string, unknown> => {
      const body = sends[turn]!.body as JsonRpcRequest<{
        message: { content: ContentBlock[] };
      }>;
      const block = body.params.message.content[0]!;
      if (block.type !== "tool_result") {
        throw new Error(
          `expected tool_result at turn ${turn}, got ${block.type}`,
        );
      }
      return JSON.parse(block.content as string) as Record<string, unknown>;
    };
    // sends[0] is the first user turn; [1] the launch tool_result; [2] the
    // running-poll tool_result; [3] the second send()'s user turn; [4] the
    // completed-poll tool_result.
    const started = toolResultContent(1);
    expect(started.status).toBe("started");
    expect(started.harness).toBe("claude");
    expect(started.model).toBe("claude-sonnet-5");
    const subagentId = started.subagent_id as string;

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const running = toolResultContent(2) as any;
    expect(running.subagents[0].status).toBe("running");
    expect(running.subagents[0].subagent_id).toBe(subagentId);

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const completed = toolResultContent(4) as any;
    expect(completed.subagents[0].status).toBe("completed");
    expect(completed.subagents[0].subagent_id).toBe(subagentId);
    expect(completed.subagents[0].stdout).toBe("subagent done");

    // updateClientTools fired exactly on the two transitions — never
    // redundantly (the eviction on the completed poll removes it again).
    expect(mock.updateClientToolsCalls).toHaveLength(2);
    expect(
      mock.updateClientToolsCalls[0]!.some(
        (t) => t.name === LOCAL_SUBAGENT_STATUS_TOOL,
      ),
    ).toBe(true);
    expect(
      mock.updateClientToolsCalls[1]!.some(
        (t) => t.name === LOCAL_SUBAGENT_STATUS_TOOL,
      ),
    ).toBe(false);
  });
});
