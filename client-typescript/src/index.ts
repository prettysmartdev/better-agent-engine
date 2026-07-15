/**
 * @prettysmartdev/bae-ts — TypeScript client library and agent harness for the
 * Better Agent Engine (BAE).
 *
 * The client is thin and stateless: durable agent state lives on the server.
 * This package is an **agent harness** — it opens a session, drives the
 * tool-call loop against the `/api/v1` surface, and dispatches server-returned
 * tool calls to locally-registered handlers.
 *
 * ```ts
 * const harness = new Harness(new Config({ serverUrl, clientKey }));
 * harness.registerTool({ name, description, input_schema, handler });
 * const session = await harness.connect();
 * const reply = await session.send("Hello");
 * console.log(messageText(reply));
 * await session.close();
 * ```
 */

/** Client library version. Keep in sync with package.json. */
export const VERSION = "0.1.0";

export { Config, type ConfigOptions } from "./config.js";
export { Harness, Session, type HarnessOptions } from "./harness.js";
export type { ToolDefinition, ToolHandler } from "./tool.js";
export {
  DockerDriver,
  AppleContainerDriver,
  SandboxError,
  SandboxSession,
  SandboxTarget,
  RemoteMode,
  shellQuote,
  runShellCommand,
  runShellNamed,
  type SandboxDriver,
  type SandboxHandle,
  type ExecResult,
  type RemoteSandboxStarted,
  type RemoteSandboxStopped,
  type SandboxErrorKind,
  type SandboxLifecycleState,
  type SandboxRpc,
  type SandboxToolDef,
  type SandboxTool,
} from "./sandbox.js";
export {
  DEFAULT_SUBAGENT_TIMEOUT_SECS,
  MAX_SUBAGENTS_PER_SESSION,
  SUBAGENT_OUTPUT_CAP_BYTES,
  LAUNCH_SUBAGENT_TOOL,
  LOCAL_SUBAGENT_STATUS_TOOL,
  ProcessRunner,
  PromptDelivery,
  SubagentLaunch,
  SubagentSession,
  launchSubagent,
  type SubagentDef,
  type SubagentRpc,
  type SubagentRunner,
  type RunnerOutput,
  type LocalSubagentReport,
  type SubagentStatus,
  type SubagentTool,
  type SubagentToolDef,
} from "./subagent.js";
export {
  readFileTool,
  writeFileTool,
  exploreFilesTool,
  type FileToolConfig,
} from "./files.js";
export type { Hooks, HookName } from "./hooks.js";
export {
  FetchTransport,
  isTerminalFrame,
  eventFromFrame,
  type Transport,
  type TransportRequest,
  type TransportResponse,
} from "./transport.js";
export {
  BaeError,
  ApiError,
  ProvidersFailedError,
  RpcError,
  UnknownToolError,
  ToolError,
  HookError,
  TransportError,
} from "./errors.js";
export { randomHex, constantTimeEqual } from "./secure.js";
export {
  toMessage,
  messageText,
  toolUses,
  describeEvent,
  assertNever,
  type Content,
  type ContentBlock,
  type Message,
  type ToolUse,
  type ToolResult,
  type Profile,
  type ProfileProvider,
  type EventType,
  type SessionEvent,
  type RpcMethod,
  type JsonRpcRequest,
  type JsonRpcErrorObject,
  type JsonRpcFrame,
  type SendMessageParams,
  type SubscribeParams,
  type SendMessageResult,
  type McpRequestPayload,
  type McpResponsePayload,
  type SubagentCommonPayload,
  type SubagentStartPayload,
  type SubagentRunningPayload,
  type SubagentCompletedPayload,
  type SubagentFailedPayload,
  type SubagentCancelledPayload,
  type SessionJoinPayload,
  type SessionDriverRegisterPayload,
} from "./types.js";
