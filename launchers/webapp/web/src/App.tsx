import { useEffect, useState } from "react";
import { getAgent, listAgents } from "./api/client";
import type { Agent } from "./api/types";
import AgentPage from "./components/AgentPage";
import HomePage from "./components/HomePage";

type LoadState<T> =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; value: T };

function routeAgentName(): string | null {
  const match = window.location.pathname.match(/^\/agents\/([^/]+)\/?$/);
  if (!match) return null;
  try {
    return decodeURIComponent(match[1]);
  } catch {
    return null;
  }
}

function messageFor(error: unknown): string {
  return error instanceof Error
    ? error.message
    : "Could not load the launcher.";
}

export default function App() {
  const agentName = routeAgentName();
  const [state, setState] = useState<LoadState<Agent[] | Agent>>({
    status: "loading",
  });

  useEffect(() => {
    let cancelled = false;
    const load = agentName ? getAgent(agentName) : listAgents();
    load
      .then((value) => {
        if (!cancelled) setState({ status: "ready", value });
      })
      .catch((error: unknown) => {
        if (!cancelled)
          setState({ status: "error", message: messageFor(error) });
      });
    return () => {
      cancelled = true;
    };
  }, [agentName]);

  return (
    <main className="app-shell">
      <a className="wordmark" href="/" aria-label="BAE launcher home">
        <span>bae</span>
        <span>launcher</span>
      </a>
      {state.status === "loading" && <p className="loading">Loading agents…</p>}
      {state.status === "error" && (
        <section className="error-state">
          <h1>Unable to load the launcher</h1>
          <p>{state.message}</p>
          <a className="retry-link" href={window.location.pathname}>
            Try again
          </a>
        </section>
      )}
      {state.status === "ready" &&
        (agentName ? (
          <AgentPage agent={state.value as Agent} />
        ) : (
          <HomePage agents={state.value as Agent[]} />
        ))}
    </main>
  );
}
