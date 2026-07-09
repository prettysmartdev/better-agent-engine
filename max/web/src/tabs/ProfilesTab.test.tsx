import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    listProfiles: vi.fn(),
    listProviders: vi.fn(),
    listMcpServers: vi.fn(),
    createProfile: vi.fn(),
    updateProfile: vi.fn(),
    deleteProfile: vi.fn(),
  };
});

import * as client from "../api/client";
import ProfilesTab from "./ProfilesTab";

const mocked = client as unknown as {
  listProfiles: ReturnType<typeof vi.fn>;
  listProviders: ReturnType<typeof vi.fn>;
  listMcpServers: ReturnType<typeof vi.fn>;
};

describe("ProfilesTab", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocked.listProfiles.mockResolvedValue({ items: [], next_cursor: null });
    mocked.listMcpServers.mockResolvedValue({
      items: [{ name: "fs" }],
      next_cursor: null,
    });
  });

  it("uses pickers (not free text) for provider and MCP fields", async () => {
    mocked.listProviders.mockResolvedValue({
      items: [{ name: "openai" }, { name: "anthropic" }],
      next_cursor: null,
    });
    render(<ProfilesTab />);
    fireEvent.click(await screen.findByRole("button", { name: "New profile" }));

    // Primary provider is a <select>, populated from the providers endpoint.
    const primary = await screen.findByLabelText("Primary provider");
    expect(primary.tagName).toBe("SELECT");
    expect(screen.getByRole("option", { name: "openai" })).toBeInTheDocument();

    // Fallback providers and MCP servers are checkbox pickers, never text inputs.
    expect(
      screen.getByRole("checkbox", { name: "anthropic" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "fs" })).toBeInTheDocument();

    // allowed_tools stays free text (a textarea).
    const tools = screen.getByPlaceholderText(/One tool name per line/);
    expect(tools.tagName).toBe("TEXTAREA");
  });

  it("disables profile creation when no providers are configured", async () => {
    mocked.listProviders.mockResolvedValue({ items: [], next_cursor: null });
    render(<ProfilesTab />);

    const newBtn = await screen.findByRole("button", { name: "New profile" });
    expect(newBtn).toBeDisabled();
    expect(
      screen.getByText(/No providers are configured/i),
    ).toBeInTheDocument();
  });
});
