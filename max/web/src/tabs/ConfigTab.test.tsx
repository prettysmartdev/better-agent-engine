import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import type { ConfigResponse } from "../api/types";

vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    getConfig: vi.fn(),
  };
});

import * as client from "../api/client";
import ConfigTab from "./ConfigTab";

const mocked = client as unknown as {
  getConfig: ReturnType<typeof vi.fn>;
};

const REDACTED = "••••••••";

function paragraphWithText(text: string) {
  return screen.getByText((_content, element) => {
    return element?.tagName === "P" && element.textContent === text;
  });
}

function configFixture(): ConfigResponse {
  return {
    mcp: {
      servers: [
        {
          name: "filesystem",
          transport: "stdio",
          command: "node",
          args: ["server.js", "--root", "/tmp"],
          url: null,
          headers: { Authorization: REDACTED },
        },
      ],
    },
    providers: {
      entries: [
        {
          name: "primary",
          provider: "openai",
          model: "gpt-5",
          base_url: "https://api.openai.com/v1",
          auth_token: REDACTED,
        },
      ],
    },
    telemetry: {
      enabled: true,
      otlp_endpoint: "https://otel.example.test/v1/traces",
      otlp_headers: { Authorization: REDACTED },
      sample_ratio: 0.35,
      service_name: "max-test",
      traces: { enabled: true },
      metrics: { enabled: true, disabled: ["http.server.duration"] },
    },
  };
}

describe("ConfigTab", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders MCP Servers, Providers, and Telemetry from getConfig", async () => {
    mocked.getConfig.mockResolvedValue(configFixture());

    render(<ConfigTab />);

    expect(
      await screen.findByRole("heading", { name: "Config" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("heading", { name: "MCP Servers" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("heading", { name: "Providers" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("heading", { name: "Telemetry" }),
    ).toBeInTheDocument();
    expect(mocked.getConfig).toHaveBeenCalledOnce();
  });

  it("renders every secret-bearing value as the response marker verbatim", async () => {
    mocked.getConfig.mockResolvedValue(configFixture());

    render(<ConfigTab />);

    expect(await screen.findAllByText(REDACTED)).toHaveLength(3);
    expect(
      within(screen.getByRole("region", { name: "MCP Servers" })).getByText(
        REDACTED,
      ),
    ).toBeInTheDocument();
    expect(
      within(screen.getByRole("region", { name: "Providers" })).getByText(
        REDACTED,
      ),
    ).toBeInTheDocument();
    expect(
      within(screen.getByRole("region", { name: "Telemetry" })).getByText(
        REDACTED,
      ),
    ).toBeInTheDocument();
  });

  it("renders each list section empty state", async () => {
    mocked.getConfig.mockResolvedValue({
      ...configFixture(),
      mcp: { servers: [] },
      providers: { entries: [] },
      telemetry: { ...configFixture().telemetry, otlp_headers: {} },
    });

    render(<ConfigTab />);

    expect(
      await screen.findByText((_content, element) => {
        return (
          element?.tagName === "P" &&
          element.textContent ===
            "No MCP servers are configured in bae-config.toml."
        );
      }),
    ).toBeInTheDocument();
    expect(
      paragraphWithText("No providers are configured in bae-config.toml."),
    ).toBeInTheDocument();
    expect(screen.getByText("No OTLP headers configured")).toBeInTheDocument();
  });

  it("renders enabled telemetry non-secret fields in full", async () => {
    mocked.getConfig.mockResolvedValue(configFixture());

    render(<ConfigTab />);

    expect(
      await screen.findByText("https://otel.example.test/v1/traces"),
    ).toBeInTheDocument();
    expect(screen.getByText("0.35")).toBeInTheDocument();
    expect(screen.getByText("max-test")).toBeInTheDocument();
    expect(screen.getByText("http.server.duration")).toBeInTheDocument();
  });

  it("keeps a disabled telemetry card present and says Enabled: no", async () => {
    mocked.getConfig.mockResolvedValue({
      ...configFixture(),
      telemetry: {
        ...configFixture().telemetry,
        enabled: false,
      },
    });

    render(<ConfigTab />);

    expect(
      await screen.findByRole("heading", { name: /Enabled:\s*no/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("OpenTelemetry export is disabled"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("heading", { name: "Telemetry" }),
    ).toBeInTheDocument();
  });
});
