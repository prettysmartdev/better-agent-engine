// Browser-driven E2E for the launcher webapp (work item 0014, Test
// Considerations → "E2E (webapp, browser-driven)").
//
// Run explicitly with `npm run test:e2e` against a running `baeapi` serving
// the built webapp and the committed 3-agent fixture
// (e2e/fixtures/bae-app.e2e.toml) — see playwright.config.ts for the exact
// boot command. Every test skips gracefully when the stack is absent, so this
// file never leaves a failing test in the default (offline) `make test`
// suite, which does not run it at all.
//
// Coverage map (offline component-test equivalents live in src/**/*.test.tsx):
//   - home grid: one card per configured agent (3+) ... "home grid" test
//   - per-card detail page scoped to its agent ........ "detail pages" test
//   - free-form message → correct agent, live stream .. "free-form" test
//   - pre-defined prompt button → correct agent ....... "prompt button" test
//   - two tabs, two agents, no cross-talk .............. "two tabs" test

import { test, expect, type Page } from "@playwright/test";

type AgentView = { name: string; display_name: string | null };

/** Skip the whole suite unless a launcher stack with 3+ agents is reachable. */
test.beforeEach(async ({ page, baseURL }) => {
  let agents: AgentView[] | null = null;
  try {
    const res = await page.request.get(`${baseURL}/_launcher/agents`);
    if (res.ok()) agents = (await res.json()) as AgentView[];
  } catch {
    agents = null;
  }
  test.skip(!agents, `launcher stack not reachable at ${baseURL}`);
  test.skip(
    (agents ?? []).length < 3,
    "stack must serve the 3-agent fixture e2e/fixtures/bae-app.e2e.toml",
  );
});

async function openAgent(page: Page, displayName: string): Promise<void> {
  await page.goto("/");
  await page.getByRole("link", { name: new RegExp(displayName) }).click();
  await expect(page.getByRole("heading", { name: displayName })).toBeVisible();
}

test("home grid renders one card per configured agent", async ({ page }) => {
  await page.goto("/");
  for (const name of ["Summarizer", "Translator", "Triage"]) {
    await expect(
      page.getByRole("link", { name: new RegExp(name) }),
    ).toBeVisible();
  }
  const res = await page.request.get("/_launcher/agents");
  const agents = (await res.json()) as AgentView[];
  await expect(page.getByRole("link")).toHaveCount(agents.length + 1); // + wordmark
});

test("each card opens a detail page with its own chat view", async ({
  page,
}) => {
  for (const name of ["Summarizer", "Translator", "Triage"]) {
    await openAgent(page, name);
    await expect(page.getByPlaceholder(`Message ${name}`)).toBeVisible();
  }
});

test("a free-form message triggers the correct agent and renders the stream live", async ({
  page,
}) => {
  await openAgent(page, "Summarizer");
  await page.getByPlaceholder("Message Summarizer").fill("hello from e2e");
  await page.getByRole("button", { name: "Send" }).click();

  // The first echoed line appears while the harness is still sleeping before
  // its second line — the stream renders live, not after child exit.
  await expect(page.getByText("summarize:hello from e2e")).toBeVisible({
    timeout: 900,
  });
  await expect(page.getByText("summarize-done")).not.toBeVisible();
  await expect(page.getByText("summarize-done")).toBeVisible();
  // The trailing exit-code NDJSON record is transport metadata, never chat.
  await expect(page.getByText(/exit_code/)).not.toBeVisible();
});

test("a pre-defined prompt button triggers the correct agent with its configured text", async ({
  page,
}) => {
  await openAgent(page, "Summarizer");
  await page.getByRole("button", { name: "Summarize the day" }).click();
  await expect(
    page.getByText("summarize:Summarize today's activity."),
  ).toBeVisible();
});

test("two agents' chat views in two tabs do not cross-talk", async ({
  context,
}) => {
  const tabA = await context.newPage();
  const tabB = await context.newPage();
  await openAgent(tabA, "Summarizer");
  await openAgent(tabB, "Translator");

  await tabA.getByPlaceholder("Message Summarizer").fill("only for A");
  await tabA.getByRole("button", { name: "Send" }).click();
  await expect(tabA.getByText("summarize:only for A")).toBeVisible();
  await expect(tabB.getByText(/only for A/)).not.toBeVisible();

  await tabB.getByPlaceholder("Message Translator").fill("only for B");
  await tabB.getByRole("button", { name: "Send" }).click();
  await expect(tabB.getByText("translate:only for B")).toBeVisible();
  await expect(tabA.getByText(/only for B/)).not.toBeVisible();
  // A's earlier transcript is unaffected by B's later trigger.
  await expect(tabA.getByText("summarize:only for A")).toBeVisible();
});
