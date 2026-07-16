import type { Agent } from "./types";

export class ApiError extends Error {
  /** The HTTP status of the failed response, if one was received. */
  status?: number;

  constructor(message: string, status?: number) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/**
 * The operator-entered `BAE_LAUNCHER_API_TOKEN`, held in memory only: a page
 * refresh clears it, matching V1's no-persistence rule (like the chat
 * transcript itself). When set, trigger requests carry it as a Bearer token —
 * `/_launcher/*` introspection is never gated, so only triggers need it.
 */
let authToken: string | null = null;

export function setAuthToken(token: string | null): void {
  authToken = token && token.trim() ? token.trim() : null;
}

export function hasAuthToken(): boolean {
  return authToken !== null;
}

async function responseError(response: Response): Promise<ApiError> {
  const text = await response.text();
  try {
    const problem = JSON.parse(text) as { detail?: string; title?: string };
    return new ApiError(
      problem.detail ?? problem.title ?? `HTTP ${response.status}`,
      response.status,
    );
  } catch {
    return new ApiError(text || `HTTP ${response.status}`, response.status);
  }
}

async function get<T>(path: string): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, { credentials: "same-origin" });
  } catch {
    throw new ApiError("Could not reach the launcher API.");
  }
  if (!response.ok) throw await responseError(response);
  return (await response.json()) as T;
}

export function listAgents(): Promise<Agent[]> {
  return get<Agent[]>("/_launcher/agents");
}

export function getAgent(name: string): Promise<Agent> {
  return get<Agent>(`/_launcher/agents/${encodeURIComponent(name)}`);
}

/**
 * Send a trigger request and report each output line as it arrives. `baeapi`
 * sends agent output as streamed text plus a terminal NDJSON exit-code record;
 * the latter is transport metadata, not chat content, so it is omitted.
 */
export async function triggerAgent(
  name: string,
  inputField: string,
  text: string,
  onOutput: (chunk: string) => void,
): Promise<void> {
  let response: Response;
  try {
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
    };
    if (authToken) headers["Authorization"] = `Bearer ${authToken}`;
    response = await fetch(`/agents/${encodeURIComponent(name)}/trigger`, {
      method: "POST",
      credentials: "same-origin",
      headers,
      body: JSON.stringify({ [inputField]: text }),
    });
  } catch {
    throw new ApiError("Could not reach the launcher API.");
  }
  if (!response.ok) throw await responseError(response);
  if (!response.body)
    throw new ApiError("The launcher returned no response stream.");

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  let sawOutput = false;

  const consumeLine = (line: string) => {
    if (!isExitRecord(line)) {
      sawOutput = true;
      onOutput(`${line}\n`);
    }
  };

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffered += decoder.decode(value, { stream: true });
    const lines = buffered.split("\n");
    buffered = lines.pop() ?? "";
    lines.forEach(consumeLine);
  }
  buffered += decoder.decode();
  if (buffered) consumeLine(buffered);
  if (!sawOutput) onOutput("Agent completed without output.");
}

function isExitRecord(line: string): boolean {
  try {
    const value = JSON.parse(line) as unknown;
    return typeof value === "object" && value !== null && "exit_code" in value;
  } catch {
    return false;
  }
}
