import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { makeAgents } from "./test/fixtures";

vi.mock("./api/client", () => ({
  listAgents: vi.fn(),
  getAgent: vi.fn(),
}));

import { getAgent, listAgents } from "./api/client";
import App from "./App";

const mockedListAgents = vi.mocked(listAgents);
const mockedGetAgent = vi.mocked(getAgent);

function setPath(path: string) {
  window.history.pushState({}, "", path);
}

describe("App routing", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    setPath("/");
  });

  it("renders the home grid with one card per agent at `/`", async () => {
    const agents = makeAgents();
    mockedListAgents.mockResolvedValue(agents);

    render(<App />);

    await screen.findByRole("heading", { name: "Available agents" });
    const cardLinks = screen
      .getAllByRole("link")
      .filter((link) => link.getAttribute("href")?.startsWith("/agents/"));
    expect(cardLinks).toHaveLength(agents.length);
    expect(mockedGetAgent).not.toHaveBeenCalled();
  });

  it("renders a chat detail page scoped to the requested agent at `/agents/{name}`", async () => {
    const agents = makeAgents();
    const target = agents[1];
    setPath(`/agents/${target.name}`);
    mockedGetAgent.mockResolvedValue(target);

    render(<App />);

    expect(
      await screen.findByRole("heading", {
        name: target.display_name ?? target.name,
      }),
    ).toBeInTheDocument();
    expect(mockedGetAgent).toHaveBeenCalledWith(target.name);
    expect(mockedListAgents).not.toHaveBeenCalled();
  });

  it("shows the explanatory empty state when zero agents are configured", async () => {
    mockedListAgents.mockResolvedValue([]);

    render(<App />);

    expect(
      await screen.findByRole("heading", { name: "No agents are configured" }),
    ).toBeInTheDocument();
  });
});
