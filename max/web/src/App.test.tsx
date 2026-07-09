import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";

vi.mock("./api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api/client")>();
  return {
    ...actual,
    checkSession: vi.fn(),
    logout: vi.fn().mockResolvedValue(undefined),
    listKeys: vi.fn().mockResolvedValue({ items: [], next_cursor: null }),
    listProfiles: vi.fn().mockResolvedValue({ items: [], next_cursor: null }),
  };
});

import * as client from "./api/client";
import App from "./App";

const mocked = client as unknown as { checkSession: ReturnType<typeof vi.fn> };

describe("App auth gate", () => {
  beforeEach(() => vi.clearAllMocks());

  it("shows the login page when the session is not authenticated", async () => {
    mocked.checkSession.mockResolvedValue(false);
    render(<App />);
    expect(
      await screen.findByRole("button", { name: "Sign in" }),
    ).toBeInTheDocument();
  });

  it("shows the dashboard tabs when authenticated", async () => {
    mocked.checkSession.mockResolvedValue(true);
    render(<App />);
    expect(
      await screen.findByRole("tab", { name: "Keys" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Sessions" })).toBeInTheDocument();
  });
});
