/**
 * Builtin **sandbox tools** — give an agent real shell/execution ability with a
 * security boundary the harness developer controls.
 *
 * Mirrors the server's `engine/sandbox.rs` on the client side, offering the same
 * two-driver {@link SandboxDriver} shape (`ensureImage`/`start`/`exec`/`stop`,
 * Docker and Apple Containers), the two tool constructors {@link runShellCommand}
 * (arbitrary shell) and {@link runShellNamed} (a `{param}` command template), and
 * a {@link SandboxTarget}/{@link RemoteMode} builder describing where a command
 * runs and, for remote commands, who builds the `tool_result`.
 *
 * ## Sandbox tools require a live {@link Session}
 *
 * Unlike every other builtin tool, sandbox tools need a session handle:
 * local-target tools report their `running`/`stopped`/`error` lifecycle to the
 * server (`session.reportLocalSandbox`), and remote-manual tools fetch raw output
 * via `session.execRemoteSandbox`. Obtain a {@link SandboxSession} from
 * {@link Harness.sandboxSession} **before** `connect()`, build tools against it,
 * register them, then connect. The handle's transport is late-bound (empty until
 * connect fills it); a tool that somehow fired before connect throws. Because a
 * handler only runs after `send()` — hence after connect — this is safe, and it
 * is the one shape under which Auto-mode tools (declared in the session-open
 * `sandbox_tools` list, i.e. before connect) register uniformly alongside local
 * and remote-manual tools.
 */

import { execFile } from "node:child_process";

import type { Content } from "./types.js";
import type { ToolDefinition } from "./tool.js";

// ---------------------------------------------------------------------------
// Core data types (mirror the server's SandboxDriver surface)
// ---------------------------------------------------------------------------

/** A running sandboxed container, opaque beyond its id and image. */
export interface SandboxHandle {
  /** Container id printed by `docker run -d` / `container run -d`. */
  id: string;
  /** The image the container was started from. */
  image: string;
}

/** The captured result of one command run inside a sandbox. */
export interface ExecResult {
  stdout: string;
  stderr: string;
  /** Process exit code (`-1` if the process was killed by a signal). */
  exit_code: number;
}

/**
 * Terminal result of `session.startRemoteSandbox` — the server-hosted sandbox is
 * up and its handle retained session-wide.
 */
export interface RemoteSandboxStarted {
  /** The started container's id. */
  sandbox_id: string;
  /** The image it was started from. */
  image: string;
  /**
   * When it started (the `session.sandbox.running` event's `created_at`), or
   * `null` if that log write failed.
   */
  started_at: string | null;
}

/** Terminal result of `session.stopRemoteSandbox`. */
export interface RemoteSandboxStopped {
  /** Always `true` on success. */
  stopped: boolean;
  /** The stopped sandbox's image. */
  image: string;
  /** The stopped sandbox's container id. */
  sandbox_id: string;
}

/** The failure kinds a driver can report, mirroring the server's `SandboxError`. */
export type SandboxErrorKind = "unsupported" | "image" | "runtime";

/** A structured sandbox failure. */
export class SandboxError extends Error {
  constructor(
    readonly kind: SandboxErrorKind,
    message: string,
    /** The image name, for `kind === "image"`. */
    readonly image?: string,
  ) {
    super(message);
    this.name = "SandboxError";
  }
}

/**
 * The local container-engine abstraction — a full mirror of the server's
 * `SandboxDriver` (not just `exec`), so a {@link SandboxSession} can track a
 * real container identity to report and a test can inject a fake. Implemented by
 * {@link DockerDriver} and {@link AppleContainerDriver}.
 */
export interface SandboxDriver {
  /** Idempotent: inspect `image` locally; pull it if absent. */
  ensureImage(image: string): Promise<void>;
  /** Start a long-lived container (keep-alive `sleep infinity`) and return it. */
  start(image: string): Promise<SandboxHandle>;
  /** Run one shell command in an already-started container. */
  exec(handle: SandboxHandle, command: string): Promise<ExecResult>;
  /** Stop and remove the container. Idempotent on an already-gone id. */
  stop(handle: SandboxHandle): Promise<void>;
}

// ---------------------------------------------------------------------------
// CLI drivers (Docker / Apple Containers)
// ---------------------------------------------------------------------------

/** Per-engine CLI verbs. `run`/`exec`/`stop` are identical across engines. */
interface Cli {
  program: string;
  inspect: string[];
  pull: string[];
}

const DOCKER_CLI: Cli = {
  program: "docker",
  inspect: ["image", "inspect"],
  pull: ["pull"],
};

const APPLE_CLI: Cli = {
  program: "container",
  inspect: ["images", "inspect"],
  pull: ["images", "pull"],
};

interface CliOutput {
  stdout: string;
  stderr: string;
  exitCode: number;
}

/**
 * Run a CLI command to completion, capturing stdout/stderr/exit code. A spawn
 * failure (missing binary) is a {@link SandboxError} of kind `runtime`, never an
 * unhandled throw — exactly how the engine treats a missing subprocess binary.
 */
function runCli(program: string, args: string[]): Promise<CliOutput> {
  return new Promise((resolve, reject) => {
    execFile(
      program,
      args,
      { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
      (error, stdout, stderr) => {
        // A non-zero exit sets `error` with a numeric `.code`; a spawn/OS
        // failure (e.g. ENOENT) sets a string `.code`.
        const code = (error as NodeJS.ErrnoException | null)?.code;
        if (error && typeof code === "string") {
          reject(
            new SandboxError(
              "runtime",
              `failed to spawn \`${program}\`: ${error.message}`,
            ),
          );
          return;
        }
        const exitCode =
          error && typeof code === "number" ? code : error ? 1 : 0;
        resolve({ stdout: String(stdout), stderr: String(stderr), exitCode });
      },
    );
  });
}

/** Truncate CLI stderr carried into an error so a runaway line stays bounded. */
function truncate(s: string): string {
  const MAX = 2000;
  const t = s.trim();
  return t.length <= MAX ? t : `${t.slice(0, MAX)}… (truncated)`;
}

async function cliEnsureImage(cli: Cli, image: string): Promise<void> {
  const inspect = await runCli(cli.program, [...cli.inspect, image]);
  if (inspect.exitCode === 0) return;
  const pull = await runCli(cli.program, [...cli.pull, image]);
  if (pull.exitCode !== 0) {
    throw new SandboxError("image", truncate(pull.stderr), image);
  }
}

async function cliStart(cli: Cli, image: string): Promise<SandboxHandle> {
  const out = await runCli(cli.program, [
    "run",
    "-d",
    "--rm",
    image,
    "sleep",
    "infinity",
  ]);
  if (out.exitCode !== 0) {
    throw new SandboxError("runtime", truncate(out.stderr));
  }
  return { id: out.stdout.trim(), image };
}

async function cliExec(
  cli: Cli,
  handle: SandboxHandle,
  command: string,
): Promise<ExecResult> {
  // The command is one argv element to the container's `sh -c`; no host shell is
  // involved. A non-zero exit is the command's own result, not a driver error.
  const out = await runCli(cli.program, [
    "exec",
    handle.id,
    "sh",
    "-c",
    command,
  ]);
  return { stdout: out.stdout, stderr: out.stderr, exit_code: out.exitCode };
}

async function cliStop(cli: Cli, handle: SandboxHandle): Promise<void> {
  const out = await runCli(cli.program, ["stop", handle.id]);
  if (out.exitCode !== 0) {
    if (
      out.stderr.includes("No such container") ||
      out.stderr.includes("not found")
    ) {
      return; // already gone — `--rm` container that already exited
    }
    throw new SandboxError("runtime", truncate(out.stderr));
  }
}

/**
 * The Docker driver: `docker image inspect` → `docker pull` on miss;
 * `docker run -d --rm <image> sleep infinity`; `docker exec <id> sh -c <cmd>`;
 * `docker stop <id>`.
 */
export class DockerDriver implements SandboxDriver {
  ensureImage(image: string): Promise<void> {
    return cliEnsureImage(DOCKER_CLI, image);
  }
  start(image: string): Promise<SandboxHandle> {
    return cliStart(DOCKER_CLI, image);
  }
  exec(handle: SandboxHandle, command: string): Promise<ExecResult> {
    return cliExec(DOCKER_CLI, handle, command);
  }
  stop(handle: SandboxHandle): Promise<void> {
    return cliStop(DOCKER_CLI, handle);
  }
}

/**
 * The Apple Containers driver, shaped identically against the `container` CLI.
 * {@link AppleContainerDriver.create} fails fast on a non-macOS host so a
 * misconfiguration surfaces as one clear error rather than a subprocess failure.
 */
export class AppleContainerDriver implements SandboxDriver {
  private constructor() {}

  /** Construct after checking the host OS (`process.platform === "darwin"`). */
  static create(
    platform: NodeJS.Platform = process.platform,
  ): AppleContainerDriver {
    if (platform !== "darwin") {
      throw new SandboxError(
        "unsupported",
        `Apple Containers driver requires macOS; host platform is \`${platform}\``,
      );
    }
    return new AppleContainerDriver();
  }

  ensureImage(image: string): Promise<void> {
    return cliEnsureImage(APPLE_CLI, image);
  }
  start(image: string): Promise<SandboxHandle> {
    return cliStart(APPLE_CLI, image);
  }
  exec(handle: SandboxHandle, command: string): Promise<ExecResult> {
    return cliExec(APPLE_CLI, handle, command);
  }
  stop(handle: SandboxHandle): Promise<void> {
    return cliStop(APPLE_CLI, handle);
  }
}

// ---------------------------------------------------------------------------
// Shell escaping (the command-injection boundary)
// ---------------------------------------------------------------------------

/**
 * POSIX single-quote escaping — the standard shell-quoting primitive. Wraps
 * `arg` in single quotes and rewrites every embedded `'` as `'\''`, so the shell
 * always treats the result as **one literal argument**. This is the
 * command-injection boundary for {@link runShellNamed}: every model-supplied
 * value passes through here before substitution into a command template.
 */
export function shellQuote(arg: string): string {
  return `'${arg.replaceAll("'", "'\\''")}'`;
}

/** Ordered, unique `{param}` names in a template; throws on a malformed one. */
function parseParams(template: string): string[] {
  const params: string[] = [];
  const re = /\{([^}]*)\}/g;
  let m: RegExpExecArray | null;
  let lastIndex = 0;
  while ((m = re.exec(template)) !== null) {
    if (m[1] === "") {
      throw new Error("empty `{}` placeholder in command template");
    }
    if (!params.includes(m[1]!)) params.push(m[1]!);
    lastIndex = re.lastIndex;
  }
  // Detect an unterminated `{` after the last full match.
  const tail = template.slice(lastIndex);
  if (tail.includes("{")) {
    throw new Error("unterminated `{` in command template");
  }
  return params;
}

/**
 * Single left-to-right pass: copy literal text and splice each `{name}` in as
 * the **shell-escaped** input value. A single pass guarantees a substituted
 * value can never itself be re-interpreted as a placeholder.
 */
function interpolate(template: string, input: Record<string, unknown>): string {
  return template.replace(/\{([^}]*)\}/g, (_full, name: string) => {
    const value = input[name];
    if (typeof value !== "string") {
      throw new Error(`missing required string parameter \`${name}\``);
    }
    return shellQuote(value);
  });
}

// ---------------------------------------------------------------------------
// Session RPC seam
// ---------------------------------------------------------------------------

/** A local sandbox lifecycle state reported to the server. */
export type SandboxLifecycleState = "running" | "stopped" | "error";

/**
 * The two new session RPC methods a {@link SandboxSession} needs. {@link Session}
 * implements this; a test can supply a recorder.
 */
export interface SandboxRpc {
  /** `session.execRemoteSandbox` — run a command in the remote sandbox. */
  execRemoteSandbox(command: string): Promise<ExecResult>;
  /** `session.reportLocalSandbox` — log a local sandbox lifecycle transition. */
  reportLocalSandbox(
    state: SandboxLifecycleState,
    image: string,
    containerId: string | null,
    detail: string | null,
  ): Promise<void>;
}

// ---------------------------------------------------------------------------
// SandboxSession — the late-bound handle sandbox tools capture
// ---------------------------------------------------------------------------

/**
 * A cheap handle to a live {@link Session}'s sandbox capability: the transport
 * for the remote RPC methods, plus the local container-engine driver and the set
 * of local containers this session started. Obtain one from
 * {@link Harness.sandboxSession} (before connect) or {@link Session.sandboxSession}
 * (after); its transport is late-bound. See the module docs for ordering.
 */
export class SandboxSession {
  private rpc: SandboxRpc | undefined;
  private driver: SandboxDriver = new DockerDriver();
  private readonly started = new Map<string, SandboxHandle>();

  /** Bind the transport once connected. The first bind wins. */
  bind(rpc: SandboxRpc): void {
    if (this.rpc === undefined) this.rpc = rpc;
  }

  /** Replace the local driver (e.g. {@link AppleContainerDriver}, or a fake). */
  setLocalDriver(driver: SandboxDriver): void {
    this.driver = driver;
  }

  private requireRpc(): SandboxRpc {
    if (this.rpc === undefined) {
      throw new SandboxError(
        "runtime",
        "sandbox tool used before the session was connected; build sandbox tools " +
          "from Harness.sandboxSession() and register them, then connect()",
      );
    }
    return this.rpc;
  }

  /** Run `command` in the session's remote sandbox (`session.execRemoteSandbox`). */
  execRemoteSandbox(command: string): Promise<ExecResult> {
    return this.requireRpc().execRemoteSandbox(command);
  }

  /** Report a local sandbox lifecycle transition (`session.reportLocalSandbox`). */
  reportLocalSandbox(
    state: SandboxLifecycleState,
    image: string,
    containerId: string | null,
    detail: string | null,
  ): Promise<void> {
    return this.requireRpc().reportLocalSandbox(
      state,
      image,
      containerId,
      detail,
    );
  }

  /** Start (or reuse) a local container for `image`, reporting `running`/`error`. */
  async startLocal(image: string): Promise<SandboxHandle> {
    const existing = this.started.get(image);
    if (existing !== undefined) return existing;
    try {
      await this.driver.ensureImage(image);
      const handle = await this.driver.start(image);
      this.started.set(image, handle);
      await this.safeReport("running", image, handle.id, null);
      return handle;
    } catch (cause) {
      await this.safeReport("error", image, null, errorDetail(cause));
      throw cause;
    }
  }

  /** Lazily start the local container, run `command`, report `error` on failure. */
  async execLocal(image: string, command: string): Promise<ExecResult> {
    const handle = await this.startLocal(image);
    try {
      return await this.driver.exec(handle, command);
    } catch (cause) {
      await this.safeReport("error", image, handle.id, errorDetail(cause));
      throw cause;
    }
  }

  /** Stop every local container this session started, reporting `stopped`/`error`. */
  async stopAllLocal(): Promise<void> {
    const entries = [...this.started.entries()];
    this.started.clear();
    for (const [image, handle] of entries) {
      try {
        await this.driver.stop(handle);
        await this.safeReport("stopped", image, handle.id, null);
      } catch (cause) {
        await this.safeReport("error", image, handle.id, errorDetail(cause));
      }
    }
  }

  /** Report, swallowing telemetry failures (the report never masks the tool result). */
  private async safeReport(
    state: SandboxLifecycleState,
    image: string,
    containerId: string | null,
    detail: string | null,
  ): Promise<void> {
    try {
      await this.reportLocalSandbox(state, image, containerId, detail);
    } catch {
      /* telemetry only */
    }
  }
}

function errorDetail(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

// ---------------------------------------------------------------------------
// Builder types + tool constructors
// ---------------------------------------------------------------------------

/** Where a shell tool's commands run. */
export type SandboxTarget =
  { type: "local"; image: string } | { type: "remote" };

/** Constructors for {@link SandboxTarget}. */
export const SandboxTarget = {
  /** The harness's own local container engine. */
  local(image: string): SandboxTarget {
    return { type: "local", image };
  },
  /** The remote sandbox the server started for this session. */
  remote(): SandboxTarget {
    return { type: "remote" };
  },
};

/**
 * For a remote tool, how the result is handled. Ignored for local tools (which
 * always dispatch client-side).
 */
export type RemoteMode =
  | { type: "auto" }
  | { type: "manual"; transform: (result: ExecResult) => Content };

/** Constructors for {@link RemoteMode}. */
export const RemoteMode = {
  /** Server-dispatched: declared in `sandbox_tools`, run server-side. */
  auto(): RemoteMode {
    return { type: "auto" };
  },
  /** Client-dispatched: fetch raw output, `transform` it into the tool_result. */
  manual(transform: (result: ExecResult) => Content): RemoteMode {
    return { type: "manual", transform };
  },
};

/**
 * An Auto-mode sandbox tool declaration for the session-open `sandbox_tools`
 * list. It carries no handler (the server dispatches it) and is a distinct type
 * from {@link ToolDefinition} so it can never be registered as a callable tool.
 */
export interface SandboxToolDef {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

/**
 * The result of a sandbox tool constructor: either a client-dispatched tool
 * (local, or remote-manual) or an Auto declaration (remote-auto). Register it
 * with {@link Harness.registerSandboxTool}, which routes each kind correctly.
 */
export type SandboxTool =
  { kind: "tool"; tool: ToolDefinition } | { kind: "def"; def: SandboxToolDef };

/** Turn an {@link ExecResult} into default tool-result content. */
function execResultContent(result: ExecResult): Content {
  return JSON.stringify(result);
}

/**
 * Declare a tool that runs an **arbitrary** shell command (fixed name
 * `run_shell_command`, one required `command` string) in `target`. For a remote
 * target in {@link RemoteMode.auto} this returns a `def`; otherwise a
 * client-dispatched `tool`.
 *
 * `run_shell_command` is unconstrained by design: the image (local) or the
 * server's sandbox is the entire security boundary. Use {@link runShellNamed}
 * when the agent should only run one specific command.
 */
export function runShellCommand(
  session: SandboxSession,
  target: SandboxTarget,
  remoteMode: RemoteMode,
): SandboxTool {
  const input_schema = {
    type: "object",
    properties: {
      command: {
        type: "string",
        description: "The shell command to run inside the sandbox.",
      },
    },
    required: ["command"],
    additionalProperties: false,
  };
  return buildTool(
    session,
    "run_shell_command",
    "Run an arbitrary shell command inside the configured sandbox and return its " +
      "stdout, stderr, and exit code.",
    input_schema,
    { type: "full" },
    target,
    remoteMode,
  );
}

/**
 * Declare a **named** shell tool whose input schema is derived from the
 * `{param}` placeholders in `commandTemplate`. Each placeholder becomes a
 * required string input; at call time every model-supplied value is
 * **shell-escaped** ({@link shellQuote}) before substitution — the
 * command-injection boundary. Returns a `def` for remote-auto, else a `tool`.
 * Throws if `commandTemplate` is malformed.
 */
export function runShellNamed(
  session: SandboxSession,
  name: string,
  description: string,
  commandTemplate: string,
  target: SandboxTarget,
  remoteMode: RemoteMode,
): SandboxTool {
  const params = parseParams(commandTemplate);
  const properties: Record<string, unknown> = {};
  for (const p of params) {
    properties[p] = {
      type: "string",
      description: `Value substituted for {${p}} (shell-escaped before use).`,
    };
  }
  const input_schema = {
    type: "object",
    properties,
    required: params,
    additionalProperties: false,
  };
  return buildTool(
    session,
    name,
    description,
    input_schema,
    { type: "template", template: commandTemplate },
    target,
    remoteMode,
  );
}

/** How a handler turns model input into the final command string. */
type CommandSource = { type: "full" } | { type: "template"; template: string };

function buildCommand(
  source: CommandSource,
  input: Record<string, unknown>,
): string {
  if (source.type === "full") {
    const command = input.command;
    if (typeof command !== "string") {
      throw new Error("run_shell_command requires a string `command`");
    }
    return command;
  }
  return interpolate(source.template, input);
}

function buildTool(
  session: SandboxSession,
  name: string,
  description: string,
  input_schema: Record<string, unknown>,
  source: CommandSource,
  target: SandboxTarget,
  remoteMode: RemoteMode,
): SandboxTool {
  // Auto-mode remote tools are declarations only — no client handler.
  if (target.type === "remote" && remoteMode.type === "auto") {
    return { kind: "def", def: { name, description, input_schema } };
  }

  const handler = async (input: Record<string, unknown>): Promise<Content> => {
    const command = buildCommand(source, input);
    if (target.type === "local") {
      const result = await session.execLocal(target.image, command);
      return execResultContent(result);
    }
    const result = await session.execRemoteSandbox(command);
    // remote + manual (auto handled above; local+auto never reaches here).
    return remoteMode.type === "manual"
      ? remoteMode.transform(result)
      : execResultContent(result);
  };

  return { kind: "tool", tool: { name, description, input_schema, handler } };
}
