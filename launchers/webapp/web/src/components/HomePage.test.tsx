import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import HomePage from "./HomePage";
import { makeAgent, makeAgents } from "../test/fixtures";

describe("HomePage", () => {
  it("renders one card per configured agent, linking to its detail page", () => {
    const agents = makeAgents();
    render(<HomePage agents={agents} />);

    const links = screen.getAllByRole("link");
    expect(links).toHaveLength(agents.length);

    for (const agent of agents) {
      const link = screen.getByRole("link", {
        name: new RegExp(agent.display_name ?? agent.name),
      });
      expect(link).toHaveAttribute(
        "href",
        `/agents/${encodeURIComponent(agent.name)}`,
      );
      expect(link).toHaveTextContent(agent.description ?? "");
    }
  });

  it("a single-agent config is the same fully-supported path (one card, no empty state)", () => {
    const only = makeAgent({ name: "solo", display_name: "Solo Agent" });
    render(<HomePage agents={[only]} />);

    const links = screen.getAllByRole("link");
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveAttribute("href", "/agents/solo");
    expect(
      screen.queryByRole("heading", { name: "No agents are configured" }),
    ).not.toBeInTheDocument();
  });

  it("shows an explanatory empty state, never a blank grid, with zero agents", () => {
    render(<HomePage agents={[]} />);

    expect(screen.queryAllByRole("link")).toHaveLength(0);
    expect(
      screen.getByRole("heading", { name: "No agents are configured" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/\[\[agents\]\]/)).toBeInTheDocument();
    expect(screen.getByText(/bae-app\.toml/)).toBeInTheDocument();
  });
});
