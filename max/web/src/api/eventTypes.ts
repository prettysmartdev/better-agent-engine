// Maps BAE's 14 EventType::ALL variants (server/src/events.rs) into a small set
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

/** All categories, in a stable order (used to render the graph legend). */
export const EVENT_CATEGORIES: EventCategory[] = [
  CLIENT_TURN,
  PROVIDER,
  TOOL,
  MCP,
  LIFECYCLE,
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
  "session.compaction": LIFECYCLE,
  "session.join": JOIN,
  "session.driver.register": JOIN,
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
