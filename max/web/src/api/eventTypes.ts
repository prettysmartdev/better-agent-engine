// Maps BAE's 28 EventType::ALL variants (server/src/events.rs) into a small set
// of visual categories used to color- and shape-code event-graph nodes.

export type ShapeName =
  "circle" | "square" | "diamond" | "triangle" | "hexagon" | "pill";

export interface EventCategory {
  key: string;
  label: string;
  shape: ShapeName;
  /** Marker fill; chosen for legibility in light and dark themes. */
  color: string;
}

const CLIENT_TURN: EventCategory = {
  key: "client-turn",
  label: "Client turn",
  shape: "pill",
  color: "#2563eb",
};
const PROVIDER: EventCategory = {
  key: "provider",
  label: "Provider",
  shape: "circle",
  color: "#0d9488",
};
const TOOL: EventCategory = {
  key: "tool",
  label: "Tool call",
  shape: "square",
  color: "#d97706",
};
const MCP: EventCategory = {
  key: "mcp",
  label: "MCP exchange",
  shape: "hexagon",
  color: "#7c3aed",
};
const LIFECYCLE: EventCategory = {
  key: "lifecycle",
  label: "Session lifecycle",
  shape: "diamond",
  color: "#dc2626",
};
const JOIN: EventCategory = {
  key: "join",
  label: "Join / driver",
  shape: "triangle",
  color: "#6b7280",
};
const COMPACTION: EventCategory = {
  key: "compaction",
  label: "Compaction",
  shape: "diamond",
  color: "#db2777",
};
const SANDBOX: EventCategory = {
  key: "sandbox",
  label: "Sandbox",
  shape: "square",
  color: "#0284c7",
};
const SUBAGENT: EventCategory = {
  key: "subagent",
  label: "Subagent",
  shape: "hexagon",
  color: "#65a30d",
};

/** All categories, in a stable order (used to render the graph legend). */
export const EVENT_CATEGORIES: EventCategory[] = [
  CLIENT_TURN,
  PROVIDER,
  TOOL,
  MCP,
  LIFECYCLE,
  COMPACTION,
  SANDBOX,
  SUBAGENT,
  JOIN,
];

const BY_EVENT_TYPE: Record<string, EventCategory> = {
  "client.message.send": CLIENT_TURN,
  "server.message.send": CLIENT_TURN,
  "provider.request": PROVIDER,
  "provider.response": PROVIDER,
  "tool.call": TOOL,
  "tool.result": TOOL,
  "mcp.request": MCP,
  "mcp.response": MCP,
  "session.open": LIFECYCLE,
  "session.close": LIFECYCLE,
  "session.error": LIFECYCLE,
  "session.compaction.started": COMPACTION,
  "session.compaction.completed": COMPACTION,
  "session.join": JOIN,
  "session.driver.register": JOIN,
  "session.sandbox.available": SANDBOX,
  "session.sandbox.start": SANDBOX,
  "session.sandbox.running": SANDBOX,
  "session.sandbox.stop": SANDBOX,
  "session.sandbox.stopped": SANDBOX,
  "session.sandbox.error": SANDBOX,
  "sandbox.request": SANDBOX,
  "sandbox.response": SANDBOX,
  "session.subagent.start": SUBAGENT,
  "session.subagent.running": SUBAGENT,
  "session.subagent.completed": SUBAGENT,
  "session.subagent.failed": SUBAGENT,
  "session.subagent.cancelled": SUBAGENT,
};

const UNKNOWN: EventCategory = {
  key: "unknown",
  label: "Other",
  shape: "circle",
  color: "#6b7280",
};

/** Resolve the visual category for a given `event_type` string. */
export function categoryFor(eventType: string): EventCategory {
  return BY_EVENT_TYPE[eventType] ?? UNKNOWN;
}

/** `payload.synthetic` on the compaction preamble `server.message.send`. */
export const SYNTHETIC_COMPACTION_PREAMBLE = "compaction_preamble";

/**
 * The `payload.synthetic` value of a server-written synthetic message (e.g.
 * `"compaction_preamble"`, `"abandoned_tool_results"`), or null for every other
 * event. Such messages may carry `role: "user"` but are system-generated, never
 * text a user typed.
 */
export function syntheticKind(event: {
  event_type: string;
  payload: unknown;
}): string | null {
  if (event.event_type !== "server.message.send") return null;
  const p = event.payload;
  if (p === null || typeof p !== "object") return null;
  const synthetic = (p as Record<string, unknown>).synthetic;
  return typeof synthetic === "string" ? synthetic : null;
}

/**
 * A human label for a synthetic server message, shown instead of treating it
 * as a user/assistant turn; null for ordinary events.
 */
export function syntheticLabel(event: {
  event_type: string;
  payload: unknown;
}): string | null {
  const kind = syntheticKind(event);
  if (kind === null) return null;
  if (kind === SYNTHETIC_COMPACTION_PREAMBLE) {
    return "Compaction marker (system-generated)";
  }
  if (kind === "abandoned_tool_results") {
    return "Abandoned tool results (system-generated)";
  }
  return `System-generated message (${kind})`;
}

/**
 * The visual category for a concrete event: a synthetic compaction preamble
 * is drawn as a compaction marker, not a client turn; everything else resolves
 * by `event_type` alone.
 */
export function categoryForEvent(event: {
  event_type: string;
  payload: unknown;
}): EventCategory {
  if (syntheticKind(event) === SYNTHETIC_COMPACTION_PREAMBLE) return COMPACTION;
  return categoryFor(event.event_type);
}
