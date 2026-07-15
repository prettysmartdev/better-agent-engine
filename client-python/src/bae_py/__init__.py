"""bae-py — Python client library and customizable agent harness for the
Better Agent Engine (BAE).

The client is thin and stateless: all durable agent state lives on the server,
and this package provides an idiomatic way to drive it. The public surface
mirrors the Rust and TypeScript SDKs conceptually:

* :class:`Config` — where the server is and how to authenticate.
* :class:`Tool` — a name/description/schema plus a callable handler.
* :class:`Harness` — holds the config, tool registry, and hooks; ``connect()``
  opens a :class:`Session`.
* :class:`Session` — ``send(message)`` drives the full tool-call round-trip;
  ``close()`` ends the session.
* :class:`Hooks` — optional ``before_send`` / ``after_receive`` /
  ``before_tool_call`` / ``after_tool_call`` callbacks.

The lower-level harness machinery lives in :mod:`bae_py.harness`; everything a
typical agent author needs is re-exported here.
"""

from __future__ import annotations

from .config import Config
from .errors import (
    ApiError,
    BaeError,
    HookError,
    ProvidersFailedError,
    RpcError,
    ToolError,
    TransportError,
    UnknownToolError,
)
from .files import (
    FileToolConfig,
    explore_files_tool,
    read_file_tool,
    write_file_tool,
)
from .harness import Harness, HttpxTransport, Session, Transport, TransportResponse
from .hooks import Hook, Hooks
from .sandbox import (
    AppleContainerDriver,
    DockerDriver,
    ExecResult,
    RemoteMode,
    RemoteSandboxStarted,
    RemoteSandboxStopped,
    SandboxDriver,
    SandboxError,
    SandboxHandle,
    SandboxRpc,
    SandboxSession,
    SandboxTarget,
    SandboxTool,
    SandboxToolDef,
    run_shell_command,
    run_shell_named,
    shell_quote,
)
from .secure import constant_time_equal, random_hex
from .subagent import (
    DEFAULT_SUBAGENT_TIMEOUT_SECS,
    LAUNCH_SUBAGENT_TOOL,
    LOCAL_SUBAGENT_STATUS_TOOL,
    MAX_SUBAGENTS_PER_SESSION,
    SUBAGENT_OUTPUT_CAP_BYTES,
    ProcessSubagentRunner,
    RunnerOutput,
    SubagentDef,
    SubagentLaunch,
    SubagentRpc,
    SubagentRunner,
    SubagentSession,
    SubagentTool,
    SubagentToolDef,
    launch_subagent,
)
from .tool import Tool, ToolHandler, ToolRegistry
from .types import (
    Content,
    ContentBlock,
    EventType,
    JsonRpcError,
    JsonRpcRequest,
    McpRequestPayload,
    McpResponsePayload,
    Message,
    Profile,
    SendMessageResult,
    SessionEvent,
    SessionJoinPayload,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    assert_never,
    describe_event,
    to_message,
)

__version__ = "0.1.0"

__all__ = [
    "__version__",
    # config
    "Config",
    # tools
    "Tool",
    "ToolHandler",
    "ToolRegistry",
    # harness
    "Harness",
    "Session",
    "Transport",
    "TransportResponse",
    "HttpxTransport",
    # hooks
    "Hooks",
    "Hook",
    # sandbox tools
    "SandboxSession",
    "SandboxDriver",
    "DockerDriver",
    "AppleContainerDriver",
    "SandboxError",
    "SandboxHandle",
    "SandboxRpc",
    "SandboxTarget",
    "RemoteMode",
    "SandboxTool",
    "SandboxToolDef",
    "ExecResult",
    "RemoteSandboxStarted",
    "RemoteSandboxStopped",
    "run_shell_command",
    "run_shell_named",
    "shell_quote",
    # subagent tools
    "SubagentSession",
    "SubagentDef",
    "SubagentLaunch",
    "SubagentTool",
    "SubagentToolDef",
    "SubagentRunner",
    "ProcessSubagentRunner",
    "RunnerOutput",
    "SubagentRpc",
    "launch_subagent",
    "DEFAULT_SUBAGENT_TIMEOUT_SECS",
    "MAX_SUBAGENTS_PER_SESSION",
    "SUBAGENT_OUTPUT_CAP_BYTES",
    "LAUNCH_SUBAGENT_TOOL",
    "LOCAL_SUBAGENT_STATUS_TOOL",
    # file tools
    "FileToolConfig",
    "read_file_tool",
    "write_file_tool",
    "explore_files_tool",
    # content / event model
    "Message",
    "Content",
    "ContentBlock",
    "TextBlock",
    "ToolUseBlock",
    "ToolResultBlock",
    "Profile",
    "EventType",
    "SessionEvent",
    "describe_event",
    "assert_never",
    "to_message",
    # JSON-RPC + MCP wire types
    "JsonRpcRequest",
    "JsonRpcError",
    "SendMessageResult",
    "SessionJoinPayload",
    "McpRequestPayload",
    "McpResponsePayload",
    # security primitives
    "random_hex",
    "constant_time_equal",
    # errors
    "BaeError",
    "ApiError",
    "ProvidersFailedError",
    "RpcError",
    "UnknownToolError",
    "ToolError",
    "HookError",
    "TransportError",
]
