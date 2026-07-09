// Browser-driven E2E for the MAX dashboard (work item 0007, Test
// Considerations → "E2E (browser-driven, e.g. Playwright)").
//
// Run explicitly with `npm run test:e2e` against a running bae-max stack — see
// playwright.config.ts for the prerequisites. Every test skips gracefully when
// the stack (or a fixture it needs) is absent, so this file never leaves a
// broken/failing test in the default (offline) `make test` suite, which does
// not run it at all.
//
// Coverage map (see docs test-plan for the offline component-test equivalents):
//   - auth gate + top-bar tabs .......... "authenticated dashboard" test
//   - responsive smoke (mobile/tablet) .. asserted in every test via the
//                                         desktop/tablet/mobile projects
//   - Keys CRUD + one-time plaintext .... "Keys tab round-trip"
//   - Profiles CRUD / empty-state ....... "Profiles tab round-trip"
//   - event graph: nodes, click-detail,
//     keyboard-activate detail .......... "session graph" (needs a session)

import { test, expect, type Page } from "@playwright/test";

const PASSWORD = process.env.MAX_E2E_PASSWORD;
// A session id whose event history should render in the graph. Point this at a
// session an out-of-band scripted driver has produced (see the test-plan for
// how to drive one); without it the graph test skips.
const SESSION_ID = process.env.MAX_E2E_SESSION_ID;

/** Skip the whole suite unless a MAX stack is reachable and a password is set. */
test.beforeEach(async ({ page, baseURL }) => {
  test.skip(
    !PASSWORD,
    "MAX_E2E_PASSWORD not set — requires a running bae-max stack",
  );
  let healthy = false;
  try {
    const res = await page.request.get(`${baseURL}/healthz`);
    healthy = res.ok();
  } catch {
    healthy = false;
  }
  test.skip(!healthy, `MAX stack not reachable at ${baseURL}`);
});

/** Log in through the UI and land on the dashboard. */
async function login(page: Page): Promise<void> {
  await page.goto("/");
  await page.fill('input[name="password"]', PASSWORD!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Keys" })).toBeVisible();
}

/** The page body must never scroll horizontally — the core mobile/tablet rule. */
async function assertNoHorizontalOverflow(page: Page): Promise<void> {
  const overflow = await page.evaluate(
    () =>
      document.documentElement.scrollWidth -
      document.documentElement.clientWidth,
  );
  expect(
    overflow,
    "no horizontal page overflow at this breakpoint",
  ).toBeLessThanOrEqual(1);
}

test("authenticated dashboard shows the top-bar tabs and fits the viewport", async ({
  page,
}) => {
  await login(page);
  await expect(page.getByText("max", { exact: true }).first()).toBeVisible();
  for (const tab of ["Keys", "Profiles", "Sessions"]) {
    await expect(page.getByRole("tab", { name: tab })).toBeVisible();
  }
  await assertNoHorizontalOverflow(page);
});

test("Keys tab round-trip: create shows the one-time plaintext, then revoke", async ({
  page,
}) => {
  await login(page);
  await page.getByRole("tab", { name: "Keys" }).click();
  await expect(page.getByRole("heading", { name: "Keys" })).toBeVisible();

  // Creating a key needs a profile to bind to; skip the create leg if the
  // instance has none (the empty-state path is covered by the Profiles test).
  const profileOptions = page.locator(
    'select[name="profile"] option, select option',
  );
  const hasProfile = (await profileOptions.count()) > 0;
  test.skip(!hasProfile, "no profiles configured to bind a key to");

  const name = `e2e-key-${Date.now()}`;
  await page.fill('input[placeholder="my-agent-key"]', name);
  await page.getByRole("button", { name: "Create key" }).click();

  // The plaintext is shown exactly once, with a copy-now warning.
  await expect(
    page.getByRole("alert").getByText("copy it now", { exact: false }),
  ).toBeVisible();
  await expect(page.getByTestId("plaintext-key")).toBeVisible();
  await page
    .getByRole("button", { name: /dismiss|done|close/i })
    .first()
    .click();

  // The new key appears in the table; revoke it and it disappears.
  const row = page.getByRole("row", { name: new RegExp(name) });
  await expect(row).toBeVisible();
  page.once("dialog", (d) => d.accept());
  await row.getByRole("button", { name: /revoke|delete/i }).click();
  await expect(row).toHaveCount(0);
  await assertNoHorizontalOverflow(page);
});

test("Profiles tab round-trip: create via pickers (or assert the empty-state)", async ({
  page,
}) => {
  await login(page);
  await page.getByRole("tab", { name: "Profiles" }).click();
  await expect(page.getByRole("heading", { name: "Profiles" })).toBeVisible();

  await page
    .getByRole("button", { name: /new profile|create/i })
    .first()
    .click();
  const primary = page.locator("select").first();
  const providerCount = await primary.locator("option").count();

  if (providerCount === 0) {
    // No providers configured → creation is disabled with an explanation.
    await expect(
      page.getByRole("button", { name: "Create profile" }),
    ).toBeDisabled();
    return;
  }

  const name = `e2e-profile-${Date.now()}`;
  await page.fill('input[name="name"]', name);
  await primary.selectOption({ index: 0 });
  await page.getByRole("button", { name: "Create profile" }).click();
  await expect(page.getByRole("row", { name: new RegExp(name) })).toBeVisible();

  // Clean up: delete the profile we created.
  page.once("dialog", (d) => d.accept());
  await page
    .getByRole("row", { name: new RegExp(name) })
    .getByRole("button", { name: /delete|remove/i })
    .click();
  await assertNoHorizontalOverflow(page);
});

test("session graph: nodes render and open a detail panel on click AND keyboard", async ({
  page,
}) => {
  test.skip(
    !SESSION_ID,
    "MAX_E2E_SESSION_ID not set — needs a driven session to inspect",
  );
  await login(page);
  await page.getByRole("tab", { name: "Sessions" }).click();

  // Navigate into the target session's graph view.
  const row = page.getByRole("row", { name: new RegExp(SESSION_ID!) });
  if (await row.count()) {
    await row.getByRole("button", { name: "Open" }).click();
  } else {
    // Fall back to showing closed/error sessions too, then open.
    await page
      .getByRole("button", { name: /closed|all|show/i })
      .first()
      .click();
    await page
      .getByRole("row", { name: new RegExp(SESSION_ID!) })
      .getByRole("button", { name: "Open" })
      .click();
  }

  // Event nodes are focusable buttons within the "Session events" list.
  const nodes = page.getByRole("button", { name: /event evt_/i });
  await expect(nodes.first()).toBeVisible();

  // Click opens the detail panel with the pretty-printed payload.
  await nodes.first().click();
  const panel = page.getByRole("complementary", { name: /Event .* detail/i });
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("event-payload")).toBeVisible();
  await panel.getByRole("button", { name: "Close detail panel" }).click();

  // Keyboard: focus a node and activate with Enter — same detail panel opens.
  await nodes.nth(0).focus();
  await page.keyboard.press("Enter");
  await expect(
    page.getByRole("complementary", { name: /Event .* detail/i }),
  ).toBeVisible();
  await assertNoHorizontalOverflow(page);
});
