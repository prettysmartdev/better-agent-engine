import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import AgentPage from "./AgentPage";
import { setAuthToken } from "../api/client";
import { makeAgent } from "../test/fixtures";

/** A streamed NDJSON `Response`, one chunk per entry, each optionally delayed
 * to prove the UI updates as chunks arrive rather than only after the full
 * body is buffered. */
function ndjsonResponse(lines: { text: string; delayMs?: number }[]): Response {
  const encoder = new TextEncoder();
  let i = 0;
  const stream = new ReadableStream<Uint8Array>({
    async pull(controller) {
      if (i >= lines.length) {
        controller.close();
        return;
      }
      const { text, delayMs = 0 } = lines[i];
      if (delayMs > 0)
        await new Promise((resolve) => setTimeout(resolve, delayMs));
      controller.enqueue(encoder.encode(text));
      i += 1;
    },
  });
  return new Response(stream, { status: 200 });
}

describe("AgentPage", () => {
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    // The bearer token is module-level in-memory state; never leak it between
    // tests.
    setAuthToken(null);
  });

  it("scopes the header and composer to the given agent", () => {
    const agent = makeAgent({ name: "summarize", display_name: "Summarizer" });
    render(<AgentPage agent={agent} />);

    expect(
      screen.getByRole("heading", { name: "Summarizer" }),
    ).toBeInTheDocument();
    expect(
      screen.getByPlaceholderText("Message Summarizer"),
    ).toBeInTheDocument();
  });

  it("sends a free-form message to the correct agent's trigger route and renders the streamed reply live", async () => {
    const agent = makeAgent({
      name: "summarize",
      display_name: "Summarizer",
      chat_input_field: "prompt",
    });
    fetchMock.mockImplementation(() =>
      Promise.resolve(
        ndjsonResponse([
          { text: "[summarize] first line\n" },
          { text: "[summarize] second line\n", delayMs: 300 },
          { text: '{"exit_code":0}\n' },
        ]),
      ),
    );

    render(<AgentPage agent={agent} />);
    fireEvent.change(screen.getByPlaceholderText("Message Summarizer"), {
      target: { value: "Please summarize this." },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() =>
      expect(fetchMock).toHaveBeenCalledWith(
        "/agents/summarize/trigger",
        expect.objectContaining({
          method: "POST",
          body: JSON.stringify({ prompt: "Please summarize this." }),
        }),
      ),
    );

    // The first chunk must be visible well before the (deliberately delayed)
    // second chunk arrives — proving the response renders incrementally, not
    // only once the whole body has buffered.
    await waitFor(
      () => expect(screen.getByText(/first line/)).toBeInTheDocument(),
      { timeout: 150 },
    );
    expect(screen.queryByText(/exit_code/)).not.toBeInTheDocument();

    await waitFor(() =>
      expect(screen.getByText(/second line/)).toBeInTheDocument(),
    );
    // The trailing NDJSON exit-code record is transport metadata, never chat
    // content.
    expect(screen.queryByText(/exit_code/)).not.toBeInTheDocument();
  });

  it("a pre-defined prompt button triggers the same agent with its configured prompt text", async () => {
    const agent = makeAgent({
      name: "translate",
      display_name: "Translator",
      chat_input_field: "text",
      prompts: [{ label: "To French", prompt: "Translate to French." }],
    });
    fetchMock.mockResolvedValue(
      ndjsonResponse([{ text: "[translate] bonjour\n" }]),
    );

    render(<AgentPage agent={agent} />);
    fireEvent.click(screen.getByRole("button", { name: "To French" }));

    await waitFor(() =>
      expect(fetchMock).toHaveBeenCalledWith(
        "/agents/translate/trigger",
        expect.objectContaining({
          method: "POST",
          body: JSON.stringify({ text: "Translate to French." }),
        }),
      ),
    );
    await waitFor(() =>
      expect(screen.getByText(/bonjour/)).toBeInTheDocument(),
    );
  });

  it("a 401 shows the token form, and the saved token is sent on the next trigger", async () => {
    const agent = makeAgent({
      name: "secured",
      display_name: "Secured",
      chat_input_field: "prompt",
    });
    const problem = () =>
      new Response(
        JSON.stringify({
          type: "unauthorized",
          title: "Unauthorized",
          status: 401,
          detail: "missing Authorization header",
        }),
        {
          status: 401,
          headers: { "Content-Type": "application/problem+json" },
        },
      );
    fetchMock.mockImplementation(
      (_url: string, init: { headers?: Record<string, string> }) => {
        const auth = init.headers?.["Authorization"];
        return Promise.resolve(
          auth === "Bearer operator-secret"
            ? ndjsonResponse([
                { text: "[secured] authorized output\n" },
                { text: '{"exit_code":0}\n' },
              ])
            : problem(),
        );
      },
    );

    render(<AgentPage agent={agent} />);
    fireEvent.change(screen.getByPlaceholderText("Message Secured"), {
      target: { value: "first try" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    // The 401 surfaces the token form instead of a dead-end error.
    const tokenInput = await screen.findByPlaceholderText("API token");
    expect(screen.getByText(/BAE_LAUNCHER_API_TOKEN/)).toBeInTheDocument();

    fireEvent.change(tokenInput, { target: { value: "operator-secret" } });
    fireEvent.click(screen.getByRole("button", { name: "Use token" }));

    // Resend: the trigger now carries the Bearer token and succeeds.
    fireEvent.change(screen.getByPlaceholderText("Message Secured"), {
      target: { value: "second try" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send" }));

    await waitFor(() =>
      expect(screen.getByText(/authorized output/)).toBeInTheDocument(),
    );
    expect(fetchMock).toHaveBeenLastCalledWith(
      "/agents/secured/trigger",
      expect.objectContaining({
        headers: expect.objectContaining({
          Authorization: "Bearer operator-secret",
        }),
      }),
    );
  });

  it("two agents' chat views in separate tabs do not cross-talk", async () => {
    const alpha = makeAgent({
      name: "alpha",
      display_name: "Alpha",
      chat_input_field: "prompt",
    });
    const beta = makeAgent({
      name: "beta",
      display_name: "Beta",
      chat_input_field: "prompt",
    });

    fetchMock.mockImplementation((url: string) => {
      if (url === "/agents/alpha/trigger") {
        return Promise.resolve(
          ndjsonResponse([{ text: "[alpha] alpha-only output\n" }]),
        );
      }
      if (url === "/agents/beta/trigger") {
        return Promise.resolve(
          ndjsonResponse([{ text: "[beta] beta-only output\n" }]),
        );
      }
      throw new Error(`unexpected trigger URL: ${url}`);
    });

    // Two independently-mounted AgentPage instances stand in for two browser
    // tabs, each scoped to a different agent.
    const tabA = render(<AgentPage agent={alpha} />);
    const tabB = render(<AgentPage agent={beta} />);

    fireEvent.change(
      within(tabA.container).getByPlaceholderText("Message Alpha"),
      { target: { value: "hi alpha" } },
    );
    fireEvent.click(
      within(tabA.container).getByRole("button", { name: "Send" }),
    );

    await waitFor(() =>
      expect(
        within(tabA.container).getByText(/alpha-only output/),
      ).toBeInTheDocument(),
    );
    expect(
      within(tabB.container).queryByText(/alpha-only output/),
    ).not.toBeInTheDocument();
    expect(
      within(tabB.container).getByText(/start a new conversation/),
    ).toBeInTheDocument();

    fireEvent.change(
      within(tabB.container).getByPlaceholderText("Message Beta"),
      { target: { value: "hi beta" } },
    );
    fireEvent.click(
      within(tabB.container).getByRole("button", { name: "Send" }),
    );

    await waitFor(() =>
      expect(
        within(tabB.container).getByText(/beta-only output/),
      ).toBeInTheDocument(),
    );
    expect(
      within(tabA.container).queryByText(/beta-only output/),
    ).not.toBeInTheDocument();
    // Alpha's own transcript is unaffected by Beta's later trigger.
    expect(
      within(tabA.container).getByText(/alpha-only output/),
    ).toBeInTheDocument();
  });
});
