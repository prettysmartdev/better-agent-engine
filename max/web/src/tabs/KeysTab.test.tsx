import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    listKeys: vi.fn(),
    listProfiles: vi.fn(),
    createKey: vi.fn(),
    deleteKey: vi.fn(),
  };
});

import * as client from "../api/client";
import KeysTab from "./KeysTab";

const mocked = client as unknown as {
  listKeys: ReturnType<typeof vi.fn>;
  listProfiles: ReturnType<typeof vi.fn>;
  createKey: ReturnType<typeof vi.fn>;
};

describe("KeysTab", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocked.listProfiles.mockResolvedValue({
      items: [
        {
          id: "pro_1",
          name: "Default",
          primary_provider: "openai",
          fallback_providers: [],
          mcp_servers: [],
          allowed_tools: [],
        },
      ],
      next_cursor: null,
    });
    mocked.listKeys.mockResolvedValue({
      items: [
        {
          id: "key_obs",
          name: "max-observer-pro_1",
          prefix: "bae_obs",
          profile_id: "pro_1",
        },
        {
          id: "key_usr",
          name: "my-key",
          prefix: "bae_usr",
          profile_id: "pro_1",
        },
      ],
      next_cursor: null,
    });
  });

  it("badges MAX's auto-provisioned observer keys distinctly", async () => {
    render(<KeysTab />);
    expect(await screen.findByText("max-observer-pro_1")).toBeInTheDocument();
    expect(screen.getByText("auto-created by MAX")).toBeInTheDocument();
    // A normal key gets no such badge.
    expect(screen.getAllByText("auto-created by MAX")).toHaveLength(1);
  });

  it("shows the plaintext key exactly once with a copy-now warning", async () => {
    mocked.createKey.mockResolvedValue({
      id: "key_new",
      name: "agent-key",
      key: "bae_secret_PLAINTEXT_123",
      prefix: "bae_secret",
      profile_id: "pro_1",
    });

    render(<KeysTab />);
    // Wait for the profile picker to populate.
    await screen.findByRole("option", { name: "Default" });

    fireEvent.change(screen.getByPlaceholderText("my-agent-key"), {
      target: { value: "agent-key" },
    });
    fireEvent.change(screen.getByRole("combobox"), {
      target: { value: "pro_1" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create key" }));

    const reveal = await screen.findByTestId("plaintext-key");
    expect(reveal).toHaveTextContent("bae_secret_PLAINTEXT_123");
    // The one-time warning must be present.
    expect(
      screen.getByText(/only time the plaintext key is shown/i),
    ).toBeInTheDocument();
    expect(mocked.createKey).toHaveBeenCalledWith("agent-key", "pro_1");

    // Dismissing removes the plaintext — it is never shown again.
    fireEvent.click(screen.getByRole("button", { name: /dismiss/i }));
    await waitFor(() =>
      expect(screen.queryByTestId("plaintext-key")).not.toBeInTheDocument(),
    );
  });
});
