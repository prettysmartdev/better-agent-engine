/**
 * Builtin **subagent tools** — hand a prompt to an external CLI agent in the
 * background and retrieve its result later through a temporary status tool.
 *
 * This mirrors the Rust client implementation. A local launch is a callable
 * client tool; a remote launch is declaration-only and is always sandboxed by
 * the server. The local status tool is registered for dispatch at connect time
 * but advertised only while the session tracks at least one subagent.
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

import type { Content } from "./types.js";
import {
  interpolate,
  parseParams,
  SandboxError,
  SandboxSession,
  type ExecResult,
  type SandboxTarget,
} from "./sandbox.js";
import type { ToolDefinition } from "./tool.js";
import { randomHex } from "./secure.js";

export const DEFAULT_SUBAGENT_TIMEOUT_SECS = 600;
export const MAX_SUBAGENTS_PER_SESSION = 8;
export const SUBAGENT_OUTPUT_CAP_BYTES = 65536;
export const LAUNCH_SUBAGENT_TOOL = "launch_subagent";
export const LOCAL_SUBAGENT_STATUS_TOOL = "local_subagent_status";

/** How the prompt reaches the CLI. Stdin is the default. */
export type PromptDelivery = "arg" | "stdin";

/** Convenient names for the two wire prompt-delivery values. */
export const PromptDelivery = {
  Arg: "arg" as PromptDelivery,
  Stdin: "stdin" as PromptDelivery,
};

export interface SubagentDef {
  harness: string;
  commandTemplate: string;
  promptVia?: PromptDelivery;
  timeoutSecs?: number;
}

/** The only three execution combinations supported by the feature. */
export type SubagentLaunch =
  { kind: "local"; target: SandboxTarget } | { kind: "remote"; image: string };

export const SubagentLaunch = {
  local(target: SandboxTarget): SubagentLaunch {
    return { kind: "local", target };
  },
  remote(image: string): SubagentLaunch {
    return { kind: "remote", image };
  },
};

export type SubagentStatus =
  "running" | "completed" | "failed" | "timed_out" | "cancelled";

export interface SubagentToolDef {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
  image: string;
  subagents: Array<{
    harness: string;
    command_template: string;
    prompt_via: PromptDelivery;
    timeout_secs: number;
  }>;
}

export type SubagentTool =
  | { kind: "tool"; tool: ToolDefinition }
  | { kind: "def"; def: SubagentToolDef };

export interface RunnerOutput {
  stdout: string;
  stderr: string;
  exit_code: number;
}

/** Injectable subprocess seam; fakes can run entirely offline. */
export interface SubagentRunner {
  run(
    program: string,
    args: string[],
    stdin: Uint8Array | null,
    signal: AbortSignal,
  ): Promise<RunnerOutput>;
}

/** Continuously drain a stream while retaining at most cap+1 bytes. */
class CappedCapture {
  private readonly chunks: Buffer[] = [];
  private retained = 0;

  push(chunk: Buffer | string): void {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    const remaining = SUBAGENT_OUTPUT_CAP_BYTES + 1 - this.retained;
    if (remaining <= 0) return;
    const kept = bytes.subarray(0, remaining);
    this.chunks.push(kept);
    this.retained += kept.length;
  }

  text(): string {
    return Buffer.concat(this.chunks, this.retained).toString("utf8");
  }
}

/** Production runner with kill-on-timeout/cancel process hygiene. */
export class ProcessRunner implements SubagentRunner {
  run(
    program: string,
    args: string[],
    stdin: Uint8Array | null,
    signal: AbortSignal = new AbortController().signal,
  ): Promise<RunnerOutput> {
    return new Promise((resolve, reject) => {
      let child: ChildProcessWithoutNullStreams;
      try {
        child = spawn(program, args, {
          stdio: [stdin === null ? "ignore" : "pipe", "pipe", "pipe"],
        }) as ChildProcessWithoutNullStreams;
      } catch (cause) {
        reject(cause);
        return;
      }

      const stdout = new CappedCapture();
      const stderr = new CappedCapture();
      let settled = false;
      const finish = (callback: () => void): void => {
        if (settled) return;
        settled = true;
        signal.removeEventListener("abort", onAbort);
        callback();
      };
      const onAbort = (): void => {
        if (!settled) child.kill("SIGKILL");
      };

      child.stdout.on("data", (chunk: Buffer | string) => stdout.push(chunk));
      child.stderr.on("data", (chunk: Buffer | string) => stderr.push(chunk));
      child.once("error", (cause) => finish(() => reject(cause)));
      child.once("close", (code) =>
        finish(() =>
          resolve({
            stdout: stdout.text(),
            stderr: stderr.text(),
            exit_code: code ?? -1,
          }),
        ),
      );
      signal.addEventListener("abort", onAbort, { once: true });
      if (signal.aborted) onAbort();
      if (stdin !== null && child.stdin !== null) {
        child.stdin.on("error", () => undefined);
        child.stdin.end(Buffer.from(stdin));
      }
    });
  }
}

export interface LocalSubagentReport {
  state: "start" | "running" | "completed" | "failed" | "cancelled";
  subagent_id: string;
  harness: string;
  model: string;
  detail?: string;
  reason?: string;
  exit_code?: number;
}

/** RPC methods needed by a local subagent session. */
export interface SubagentRpc {
  reportLocalSubagent(report: LocalSubagentReport): Promise<void>;
  updateClientTools(tools: Array<Record<string, unknown>>): Promise<void>;
  cancelSubagent(subagentId: string): Promise<unknown>;
}

interface ResolvedDef {
  commandTemplate: string;
  promptVia: PromptDelivery;
  timeoutSecs: number;
}

interface Task {
  seq: number;
  harness: string;
  model: string;
  status: SubagentStatus;
  exit_code: number | null;
  stdout: string | null;
  stderr: string | null;
  truncated: boolean;
  reason: string | null;
  detail: string | null;
  controller: AbortController | null;
}

interface StatusEntry {
  subagent_id: string;
  harness: string;
  model: string;
  status: SubagentStatus;
  exit_code: number | null;
  stdout: string | null;
  stderr: string | null;
  truncated: boolean;
  reason: string | null;
  detail: string | null;
}

const STATUS_DESCRIPTION =
  "Check the status of subagents launched with launch_subagent. Pass a subagent_id to query one subagent, or omit it to list all tracked subagents. A subagent that has finished is reported with its captured output exactly once.";

function statusSchema(): Record<string, unknown> {
  return {
    type: "object",
    properties: {
      subagent_id: {
        type: "string",
        description:
          "The subagent to query. Omit to report every tracked subagent.",
      },
    },
    required: [],
    additionalProperties: false,
  };
}

function statusDeclaration(): Record<string, unknown> {
  return {
    name: LOCAL_SUBAGENT_STATUS_TOOL,
    description: STATUS_DESCRIPTION,
    input_schema: statusSchema(),
  };
}

function launchDescription(names: string[]): string {
  return `Launch a CLI subagent (${names.join(", ")}) to work on a task in the background. This tool is ASYNCHRONOUS: it returns immediately with a subagent_id and status "started" — it never waits for or returns the subagent's output. The subagent keeps running in the background; call the subagent status tool later to check whether it has finished and to retrieve its output.`;
}

function launchSchema(names: string[]): Record<string, unknown> {
  return {
    type: "object",
    properties: {
      harness: {
        type: "string",
        enum: names,
        description: "Which configured CLI subagent to launch.",
      },
      model: {
        type: "string",
        description: "The model name passed to the subagent CLI.",
      },
      prompt: {
        type: "string",
        description: "The task prompt handed to the subagent.",
      },
    },
    required: ["harness", "model", "prompt"],
    additionalProperties: false,
  };
}

function jsonString(value: unknown): string {
  return JSON.stringify(value);
}

function errorResult(error: string): string {
  return jsonString({ error });
}

function terminal(status: SubagentStatus): boolean {
  return status !== "running";
}

function truncateOutput(output: string): [string, boolean] {
  const bytes = Buffer.from(output, "utf8");
  if (bytes.length <= SUBAGENT_OUTPUT_CAP_BYTES) return [output, false];
  let used = 0;
  let end = 0;
  for (const character of output) {
    const size = Buffer.byteLength(character, "utf8");
    if (used + size > SUBAGENT_OUTPUT_CAP_BYTES) break;
    used += size;
    end += character.length;
  }
  return [output.slice(0, end), true];
}

function subagentId(): string {
  return `sba_${randomHex(16)}`;
}

/** Late-bound local subagent state and transport handle. */
export class SubagentSession {
  private rpc: SubagentRpc | undefined;
  private runner: SubagentRunner = new ProcessRunner();
  private readonly tasks = new Map<string, Task>();
  private sequence = 0;
  private baseClientTools: Array<Record<string, unknown>> = [];
  private localLaunch = false;
  private transitionTail: Promise<void> = Promise.resolve();

  constructor(
    private readonly sandbox: SandboxSession = new SandboxSession(),
  ) {}

  bind(rpc: SubagentRpc): void {
    if (this.rpc === undefined) this.rpc = rpc;
  }

  setRunner(runner: SubagentRunner): void {
    this.runner = runner;
  }

  markLocal(): void {
    this.localLaunch = true;
  }

  hasLocal(): boolean {
    return this.localLaunch;
  }

  setBaseClientTools(tools: Array<Record<string, unknown>>): void {
    this.baseClientTools = tools.map((tool) => ({ ...tool }));
  }

  statusTool(): ToolDefinition {
    return {
      name: LOCAL_SUBAGENT_STATUS_TOOL,
      description: STATUS_DESCRIPTION,
      input_schema: statusSchema(),
      handler: (input) => {
        this.requireRpc();
        return this.handleStatus(input);
      },
    };
  }

  async cancelSubagent(id: string): Promise<void> {
    await this.transition(async () => {
      const task = this.tasks.get(id);
      if (task === undefined || task.status !== "running") return;
      task.controller?.abort();
      task.controller = null;
      task.status = "cancelled";
      task.reason = "explicit";
      await this.report({
        state: "cancelled",
        subagent_id: id,
        harness: task.harness,
        model: task.model,
        reason: "explicit",
      });
    });
  }

  async closeAll(): Promise<void> {
    await this.transition(async () => {
      const wasNonempty = this.tasks.size > 0;
      const cancelled: Array<[string, Task]> = [];
      for (const [id, task] of this.tasks) {
        if (task.status !== "running") continue;
        task.controller?.abort();
        task.controller = null;
        task.status = "cancelled";
        task.reason = "session_close";
        cancelled.push([id, task]);
      }
      this.tasks.clear();
      for (const [id, task] of cancelled) {
        await this.report({
          state: "cancelled",
          subagent_id: id,
          harness: task.harness,
          model: task.model,
          reason: "session_close",
        });
      }
      if (wasNonempty) await this.syncClientTools(false);
    });
  }

  /** Serialize task-set mutations with their full-replace tool-list RPC. */
  private async transition<T>(operation: () => Promise<T>): Promise<T> {
    const previous = this.transitionTail;
    let release!: () => void;
    this.transitionTail = new Promise<void>((resolve) => {
      release = resolve;
    });
    await previous;
    try {
      return await operation();
    } finally {
      release();
    }
  }

  private requireRpc(): SubagentRpc {
    if (this.rpc === undefined) {
      throw new SandboxError(
        "runtime",
        "subagent tool used before the session was connected; build subagent tools " +
          "from Harness.subagentSession() and register them, then connect()",
      );
    }
    return this.rpc;
  }

  private async report(report: LocalSubagentReport): Promise<void> {
    try {
      await this.requireRpc().reportLocalSubagent(report);
    } catch {
      /* Telemetry is best effort and never masks the tool result. */
    }
  }

  private async syncClientTools(includeStatus: boolean): Promise<void> {
    const tools = this.baseClientTools.map((tool) => ({ ...tool }));
    if (includeStatus) tools.push(statusDeclaration());
    try {
      await this.requireRpc().updateClientTools(tools);
    } catch {
      /* Retried at the next transition opportunity. */
    }
  }

  private async handleStatus(input: Record<string, unknown>): Promise<Content> {
    return this.transition(async () => {
      const target =
        typeof input.subagent_id === "string" ? input.subagent_id : undefined;
      let entries: StatusEntry[];
      let emptied = false;
      if (target !== undefined) {
        const task = this.tasks.get(target);
        if (task === undefined) return errorResult("unknown subagent_id");
        entries = [this.entry(target, task)];
        if (terminal(task.status)) {
          this.tasks.delete(target);
          emptied = this.tasks.size === 0;
        }
      } else {
        entries = [...this.tasks.entries()]
          .sort(([, a], [, b]) => a.seq - b.seq)
          .map(([id, task]) => this.entry(id, task));
        for (const [id, task] of this.tasks) {
          if (terminal(task.status)) this.tasks.delete(id);
        }
        emptied = entries.length > 0 && this.tasks.size === 0;
      }
      const result = jsonString({ subagents: entries });
      if (emptied) await this.syncClientTools(false);
      return result;
    });
  }

  private entry(id: string, task: Task): StatusEntry {
    return {
      subagent_id: id,
      harness: task.harness,
      model: task.model,
      status: task.status,
      exit_code: task.exit_code,
      stdout: task.stdout,
      stderr: task.stderr,
      truncated: task.truncated,
      reason: task.reason,
      detail: task.detail,
    };
  }

  async launch(
    resolved: Map<string, ResolvedDef>,
    target: SandboxTarget,
    input: Record<string, unknown>,
  ): Promise<Content> {
    this.requireRpc();
    const field = (name: string): string | undefined => {
      const value = input[name];
      return typeof value === "string" && value.trim().length > 0
        ? value
        : undefined;
    };
    const harness = field("harness");
    const model = field("model");
    const prompt = field("prompt");
    if (harness === undefined || model === undefined || prompt === undefined) {
      return errorResult(
        'launch_subagent requires string "harness", "model", and "prompt"',
      );
    }
    const def = resolved.get(harness);
    if (def === undefined) return errorResult(`unknown harness "${harness}"`);

    let command: string;
    try {
      command = interpolate(def.commandTemplate, { model, prompt });
    } catch (cause) {
      return errorResult(
        `failed to build subagent command: ${
          cause instanceof Error ? cause.message : String(cause)
        }`,
      );
    }
    const stdin = def.promptVia === "stdin" ? Buffer.from(prompt) : null;
    const plan: Plan =
      target.type === "none"
        ? { kind: "host", command, stdin }
        : target.type === "local"
          ? { kind: "container", image: target.image, command, stdin }
          : { kind: "remote", command };

    return this.transition(async () => {
      const running = [...this.tasks.values()].filter(
        (task) => !terminal(task.status),
      ).length;
      if (running >= MAX_SUBAGENTS_PER_SESSION) {
        return errorResult(
          `subagent limit reached (max ${MAX_SUBAGENTS_PER_SESSION} per session)`,
        );
      }
      const id = subagentId();
      const wasEmpty = this.tasks.size === 0;
      const task: Task = {
        seq: this.sequence++,
        harness,
        model,
        status: "running",
        exit_code: null,
        stdout: null,
        stderr: null,
        truncated: false,
        reason: null,
        detail: null,
        controller: new AbortController(),
      };
      this.tasks.set(id, task);
      await this.report({ state: "start", subagent_id: id, harness, model });

      let releaseRunning!: () => void;
      const runningReported = new Promise<void>((resolve) => {
        releaseRunning = resolve;
      });
      void this.watch(id, task, def.timeoutSecs, plan, runningReported);
      try {
        await this.report({
          state: "running",
          subagent_id: id,
          harness,
          model,
        });
        if (wasEmpty) await this.syncClientTools(true);
      } finally {
        releaseRunning();
      }
      return jsonString({ subagent_id: id, harness, model, status: "started" });
    });
  }

  private async watch(
    id: string,
    task: Task,
    timeoutSecs: number,
    plan: Plan,
    runningReported: Promise<void>,
  ): Promise<void> {
    if (task.status !== "running") return;
    const controller = task.controller!;
    const work = this.execute(plan, controller.signal);
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<WatchOutcome>((resolve) => {
      timer = setTimeout(
        () => {
          controller.abort();
          resolve({ kind: "timeout" });
        },
        Math.max(0, timeoutSecs) * 1000,
      );
    });
    const settled: Promise<WatchOutcome> = work
      .then((output) => ({ kind: "settled" as const, output }))
      .catch((error: unknown) => ({ kind: "error" as const, error }));
    const outcome: WatchOutcome = await Promise.race([settled, timeout]);
    if (timer !== undefined) clearTimeout(timer);
    await runningReported;

    let status: SubagentStatus;
    let exit_code: number | null = null;
    let stdout: string | null = null;
    let stderr: string | null = null;
    let truncated = false;
    let reason: string | null = null;
    let detail: string | null = null;
    if (outcome.kind === "timeout") {
      status = "timed_out";
      reason = "timeout";
    } else if (outcome.kind === "error") {
      status = "failed";
      reason = "spawn_failed";
      detail =
        outcome.error instanceof Error
          ? outcome.error.message
          : String(outcome.error);
    } else {
      const out = outcome.output;
      const [capturedStdout, stdoutTruncated] = truncateOutput(out.stdout);
      const [capturedStderr, stderrTruncated] = truncateOutput(out.stderr);
      stdout = capturedStdout;
      stderr = capturedStderr;
      truncated = stdoutTruncated || stderrTruncated;
      exit_code = out.exit_code;
      status = out.exit_code === 0 ? "completed" : "failed";
      if (status === "failed") reason = "nonzero_exit";
    }

    // Cancellation/close wins races with a child completing in the background.
    if (task.status !== "running") return;
    task.status = status;
    task.exit_code = exit_code;
    task.stdout = stdout;
    task.stderr = stderr;
    task.truncated = truncated;
    task.reason = reason;
    task.detail = detail;
    task.controller = null;
    await this.report({
      state: status === "timed_out" ? "failed" : status,
      subagent_id: id,
      harness: task.harness,
      model: task.model,
      ...(detail !== null ? { detail } : {}),
      ...(reason !== null ? { reason } : {}),
      ...(exit_code !== null ? { exit_code } : {}),
    });
  }

  private async execute(
    plan: Plan,
    signal: AbortSignal,
  ): Promise<RunnerOutput> {
    if (plan.kind === "remote") {
      const result = await this.sandbox.execRemoteSandbox(plan.command);
      return result;
    }
    if (plan.kind === "container") {
      const handle = await this.sandbox.startLocal(plan.image);
      return this.run(
        this.sandbox.engineProgram(),
        ["exec", "-i", handle.id, "sh", "-c", plan.command],
        plan.stdin,
        signal,
      );
    }
    return this.run("/bin/sh", ["-c", plan.command], plan.stdin, signal);
  }

  private run(
    program: string,
    args: string[],
    stdin: Uint8Array | null,
    signal: AbortSignal,
  ): Promise<RunnerOutput> {
    return this.runner.run(program, args, stdin, signal);
  }
}

type Plan =
  | { kind: "host"; command: string; stdin: Uint8Array | null }
  | {
      kind: "container";
      image: string;
      command: string;
      stdin: Uint8Array | null;
    }
  | { kind: "remote"; command: string };

type WatchOutcome =
  | { kind: "timeout" }
  | { kind: "settled"; output: RunnerOutput }
  | { kind: "error"; error: unknown };

function validateTemplate(def: SubagentDef, promptVia: PromptDelivery): void {
  const params = parseParams(def.commandTemplate);
  for (const param of params) {
    if (param !== "model" && param !== "prompt") {
      throw new Error(
        `subagent commandTemplate placeholder {${param}} is not recognized (only {model} and {prompt} are allowed)`,
      );
    }
  }
  const hasPrompt = params.includes("prompt");
  if (promptVia === "arg" && !hasPrompt) {
    throw new Error(
      "subagent commandTemplate must contain {prompt} when promptVia is arg",
    );
  }
  if (promptVia === "stdin" && hasPrompt) {
    throw new Error(
      "subagent commandTemplate must not contain {prompt} when promptVia is stdin (the prompt is piped to stdin, never placed in argv)",
    );
  }
}

/** Build a local callable tool or remote declaration for configured harnesses. */
export function launchSubagent(
  session: SubagentSession,
  configs: SubagentDef[],
  launch: SubagentLaunch,
): SubagentTool {
  if (configs.length === 0) {
    throw new Error("launch_subagent requires at least one SubagentDef");
  }
  const names: string[] = [];
  for (const config of configs) {
    if (names.includes(config.harness)) {
      throw new Error(`duplicate subagent harness name \`${config.harness}\``);
    }
    names.push(config.harness);
    const promptVia = config.promptVia ?? "stdin";
    validateTemplate(config, promptVia);
  }
  const description = launchDescription(names);
  const input_schema = launchSchema(names);
  if (launch.kind === "remote") {
    return {
      kind: "def",
      def: {
        name: LAUNCH_SUBAGENT_TOOL,
        description,
        input_schema,
        image: launch.image,
        subagents: configs.map((config) => ({
          harness: config.harness,
          command_template: config.commandTemplate,
          prompt_via: config.promptVia ?? "stdin",
          timeout_secs: config.timeoutSecs ?? DEFAULT_SUBAGENT_TIMEOUT_SECS,
        })),
      },
    };
  }

  if (launch.target.type === "remote") {
    for (const config of configs) {
      if ((config.promptVia ?? "stdin") !== "arg") {
        throw new Error(
          `a SandboxTarget.remote local launch requires promptVia: arg (execRemoteSandbox carries no stdin), but harness \`${config.harness}\` uses stdin`,
        );
      }
    }
  }
  session.markLocal();
  const resolved = new Map<string, ResolvedDef>();
  for (const config of configs) {
    resolved.set(config.harness, {
      commandTemplate: config.commandTemplate,
      promptVia: config.promptVia ?? "stdin",
      timeoutSecs: config.timeoutSecs ?? DEFAULT_SUBAGENT_TIMEOUT_SECS,
    });
  }
  return {
    kind: "tool",
    tool: {
      name: LAUNCH_SUBAGENT_TOOL,
      description,
      input_schema,
      handler: (input) => session.launch(resolved, launch.target, input),
    },
  };
}

export { STATUS_DESCRIPTION };
