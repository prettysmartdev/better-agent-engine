/**
 * reference-assistant — the canonical BAE example agent (TypeScript).
 *
 * It mirrors the Rust and Python examples: register a simple tool
 * (`get_current_time`), a builtin **local sandbox** shell tool, and the three
 * builtin **file tools** scoped to this example's own `workspace/` directory,
 * open a session, run a message loop, and print the assistant's replies.
 * Every harness customization point (all five hooks) is exercised at least
 * once, and the program fails with a clear message when the provider key env
 * var referenced by the profile is missing.
 *
 * Run (after `npm install`):
 *   BAE_CLIENT_KEY=bae_… npm run example -- "What time is it?"
 *
 * Env:
 *   BAE_SERVER_URL        default http://localhost:8080
 *   BAE_CLIENT_KEY        required — the client key from `POST /admin/v1/keys`
 *   BAE_PROVIDER_KEY_ENV  name of the provider key var (default ANTHROPIC_API_KEY)
 *
 * The `run_shell_command` tool is bound to a **local** sandbox: the model can
 * ask to run a shell command and this harness executes it via `docker exec`
 * (or `container exec` on macOS) against a throwaway `alpine:3.19` container.
 * That requires a local `docker`/`container` binary; the model only reaches
 * for this tool if the prompt calls for running a shell command, so a default
 * "What time is it?" run never touches it.
 */
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import {
  Config,
  exploreFilesTool,
  Harness,
  ProvidersFailedError,
  readFileTool,
  RemoteMode,
  runShellCommand,
  SandboxTarget,
  writeFileTool,
  describeEvent,
  messageText,
  randomHex,
  type Content,
  type FileToolConfig,
} from "../../src/index.js";

function requireEnv(name: string): string {
  const value = process.env[name];
  if (value === undefined || value.trim() === "") {
    console.error(`error: ${name} is not set. Export it and retry.`);
    process.exit(1);
  }
  return value;
}

/** The one client-side tool this agent exposes. */
function getCurrentTime(input: Record<string, unknown>): Content {
  const now = new Date();
  const iso = now.toISOString();
  // Honor an optional { unix: true } input to show handlers read their input.
  return input.unix === true ? String(Math.floor(now.getTime() / 1000)) : iso;
}

async function main(): Promise<void> {
  const serverUrl = process.env.BAE_SERVER_URL ?? "http://localhost:8080";
  const clientKey = requireEnv("BAE_CLIENT_KEY");

  // The provider key is a *server-side* concern (the server resolves the
  // profile's ${ENV_VAR} at call time), but the reference agent fails fast if
  // the operator forgot to export it, matching the other SDKs.
  const providerKeyEnv =
    process.env.BAE_PROVIDER_KEY_ENV ?? "ANTHROPIC_API_KEY";
  requireEnv(providerKeyEnv);

  const prompt = process.argv[2] ?? "What time is it?";
  const correlationId = randomHex(6); // crypto-random tag for log correlation

  const harness = new Harness(
    new Config({
      serverUrl,
      clientKey,
      clientVersion: "reference-assistant/0.1.0",
    }),
  );

  harness.registerTool({
    name: "get_current_time",
    description: "Return the current time as an ISO-8601 UTC string.",
    input_schema: {
      type: "object",
      properties: {
        unix: {
          type: "boolean",
          description: "return a Unix timestamp instead",
        },
      },
    },
    handler: getCurrentTime,
  });

  // Builtin file tools, scoped to this example's own workspace/ directory.
  // allowedDirs is required and an empty list permits nothing, so any path
  // outside workspace/ is rejected in-band. `.env` is denied even without an
  // allowedExtensions restriction, showing deniedExtensions always winning.
  const workspaceDir = path.join(
    path.dirname(fileURLToPath(import.meta.url)),
    "workspace",
  );
  fs.mkdirSync(workspaceDir, { recursive: true });
  const fileConfig: FileToolConfig = {
    allowedDirs: [workspaceDir],
    deniedExtensions: ["env"],
  };
  harness.registerTool(readFileTool(fileConfig));
  harness.registerTool(writeFileTool(fileConfig));
  harness.registerTool(exploreFilesTool(fileConfig));

  // Builtin sandbox tool: a local Docker/Apple Containers shell. Sandbox tools
  // need a live Session (for local lifecycle reporting), so unlike the file
  // tools above (which need no session) they are built from a handle obtained
  // *before* `connect()` but only actually usable *after* it returns.
  harness.registerSandboxTool(
    runShellCommand(
      harness.sandboxSession(),
      SandboxTarget.local("alpine:3.19"),
      // Ignored for a local target (only meaningful for remote).
      RemoteMode.auto(),
    ),
  );

  // Exercise every hook point; a shared counter proves each one fired.
  let hookCalls = 0;
  harness.setHooks({
    before_send: (message) => {
      hookCalls++;
      console.error(`[hook ${correlationId}] before_send role=${message.role}`);
    },
    after_receive: (message) => {
      hookCalls++;
      console.error(
        `[hook ${correlationId}] after_receive text=${JSON.stringify(messageText(message))}`,
      );
    },
    before_tool_call: (toolUse) => {
      hookCalls++;
      console.error(
        `[hook ${correlationId}] before_tool_call ${toolUse.name}(${JSON.stringify(toolUse.input)})`,
      );
    },
    after_tool_call: (toolResult) => {
      hookCalls++;
      console.error(
        `[hook ${correlationId}] after_tool_call ${toolResult.name} -> ${JSON.stringify(toolResult.content)}`,
      );
    },
    // on_event observes the live `session.event` stream carried by the `/rpc`
    // NDJSON notifications. describeEvent knows the real (non-stub)
    // mcp.request / mcp.response payload shapes.
    on_event: (event) => {
      hookCalls++;
      console.error(`[hook ${correlationId}] on_event ${describeEvent(event)}`);
    },
  });

  const session = await harness.connect();
  console.error(
    `[example] session ${session.id} on profile "${session.profile.name}"`,
  );

  try {
    const reply = await session.send(prompt);
    console.log(messageText(reply));

    // Optional: tap the live event feed via session.subscribe. Opt-in (set
    // BAE_SUBSCRIBE_DEMO) so the example stays a quick one-shot. A bogus
    // sinceEventId forces a replay from the start; we stop after the first
    // event so the demo terminates — a real observer would keep reading.
    if (process.env.BAE_SUBSCRIBE_DEMO) {
      console.error(
        `[example] subscribe demo (stopping after the first event)…`,
      );
      await session.subscribe(
        (event) => {
          console.error(`[subscribe] ${describeEvent(event)}`);
          return false;
        },
        { sinceEventId: "evt_replay_from_start" },
      );
    }
  } catch (err) {
    if (err instanceof ProvidersFailedError) {
      console.error(
        `error: the server could not reach any provider. Is ${providerKeyEnv} ` +
          `set in the server's environment and valid? Session events:`,
      );
      for (const event of err.events) {
        console.error(`  - ${event.event_type}`);
      }
      process.exit(1);
    }
    throw err;
  } finally {
    await session.close();
  }

  console.error(`[example] done — ${hookCalls} hook invocations`);
}

main().catch((err: unknown) => {
  console.error(`error: ${err instanceof Error ? err.message : String(err)}`);
  process.exit(1);
});
