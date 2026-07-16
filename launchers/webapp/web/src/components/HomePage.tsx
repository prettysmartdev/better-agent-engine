import type { Agent } from "../api/types";
import AgentIcon from "./AgentIcon";

interface HomePageProps {
  agents: Agent[];
}

export default function HomePage({ agents }: HomePageProps) {
  if (agents.length === 0) {
    return (
      <section className="empty-state">
        <span className="empty-icon" aria-hidden="true">
          ◌
        </span>
        <h1>No agents are configured</h1>
        <p>
          An agent is a command your launcher can run with a prompt or other
          request input. Add one or more <code>[[agents]]</code> entries to
          <code>bae-app.toml</code>, then rebuild or redeploy this launcher.
        </p>
        <p className="muted">
          Each configured agent appears here with its own chat screen and any
          prompt buttons you define.
        </p>
      </section>
    );
  }

  return (
    <section>
      <div className="page-heading">
        <div>
          <p className="eyebrow">Harness launcher</p>
          <h1>Available agents</h1>
        </div>
        <span className="agent-count">
          {agents.length} {agents.length === 1 ? "agent" : "agents"}
        </span>
      </div>
      <div className="agent-grid">
        {agents.map((agent) => (
          <a
            className="agent-card"
            href={`/agents/${encodeURIComponent(agent.name)}`}
            key={agent.name}
          >
            <AgentIcon icon={agent.icon} />
            <span className="agent-card-copy">
              <strong>{agent.display_name ?? agent.name}</strong>
              <span>{agent.description ?? "Open this agent's chat."}</span>
            </span>
            <span className="card-arrow" aria-hidden="true">
              →
            </span>
          </a>
        ))}
      </div>
    </section>
  );
}
