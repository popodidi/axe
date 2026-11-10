import { test, expect } from "@playwright/test";
import { choose, depTasks, timedTasks, manyTasks, mockDesk } from "./fixtures";
const base = "/?taskq=http://taskq.test";
for (const legacy of ["", "overview", "ops", "live"])
  test(`default/legacy ${legacy} opens Active Tasks and preserves task links`, async ({
    page,
  }) => {
    await mockDesk(page);
    const calls: string[] = [];
    page.on("request", (r) => calls.push(r.url()));
    await page.goto(
      base + (legacy ? `&view=${legacy}` : "") + "&task=8&tab=events",
    );
    await expect(
      page.getByRole("button", { name: "Tasks", exact: true }),
    ).toHaveAttribute("aria-current", "page");
    await expect(
      page.getByRole("button", { name: "Overview", exact: true }),
    ).toHaveCount(0);
    await expect(page.getByLabel("state", { exact: true })).toHaveAttribute("data-value", 
      "active",
    );
    await expect(page.locator("tbody tr")).toHaveCount(6);
    await expect(page.getByLabel("Task detail")).toBeVisible();
    await expect(
      page.getByRole("tab", { name: "events", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
    expect(new URL(page.url()).searchParams.get("view")).toBe("tasks");
    expect(calls.some((u) => u.includes("/api/overview"))).toBe(false);
  });
test("explicit filters survive legacy redirect; keyboard, pagination and search work", async ({
  page,
}) => {
  await mockDesk(page, manyTasks);
  await page.goto(base + "&view=overview&state=all");
  await expect(
    page.getByText("38 results · Page 1 / 2", { exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Next", exact: true }).click();
  await expect(
    page.getByText("38 results · Page 2 / 2", { exact: true }),
  ).toBeVisible();
  await choose(page.getByLabel("project", { exact: true }), "rake");
  await choose(page.getByLabel("state", { exact: true }), "escalated");
  await choose(page.getByLabel("Sort", { exact: true }), "oldest");
  await page.getByLabel("Search tasks").fill("rake");
  await expect(page.locator("tbody tr")).toHaveCount(1);
  await expect(page.locator("#task-2")).toBeVisible();
  await page.getByLabel("Search tasks").blur();
  await page.keyboard.press("g");
  await page.keyboard.press("s");
  await expect(
    page.getByRole("button", { name: "Stats", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await page.keyboard.press("g");
  await page.keyboard.press("t");
  await expect(page.locator("#task-2")).toBeVisible();
});
test("theme toggle switches and persists across reload both ways", async ({
  page,
}) => {
  await mockDesk(page);
  await page.goto(base + "&task=3");
  const html = page.locator("html");
  const toggle = page.getByRole("button", { name: "Light theme" });
  await expect(html).not.toHaveAttribute("data-theme", "light");
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  const dark = await page.evaluate(
    () => getComputedStyle(document.body).backgroundColor,
  );
  await page.screenshot({
    path: test.info().outputPath("detail-dark-desktop.png"),
  });
  await toggle.click();
  await expect(html).toHaveAttribute("data-theme", "light");
  await expect(toggle).toHaveAttribute("aria-pressed", "true");
  expect(
    await page.evaluate(() => getComputedStyle(document.body).backgroundColor),
  ).not.toBe(dark);
  await page.reload();
  await expect(html).toHaveAttribute("data-theme", "light");
  await expect(toggle).toHaveAttribute("aria-pressed", "true");
  await expect(
    page.locator('[aria-label="Task detail"] h2').first(),
  ).toBeVisible();
  await page.screenshot({
    path: test.info().outputPath("detail-light-desktop.png"),
  });
  await page.goto(base);
  await expect(page.getByLabel("state", { exact: true })).toHaveAttribute("data-value", "active");
  await page.screenshot({
    path: test.info().outputPath("active-light-desktop.png"),
  });
  await toggle.click();
  await expect(html).not.toHaveAttribute("data-theme", "light");
  await page.reload();
  await expect(html).not.toHaveAttribute("data-theme", "light");
  await expect(toggle).toHaveAttribute("aria-pressed", "false");
  expect(
    await page.evaluate(() => getComputedStyle(document.body).backgroundColor),
  ).toBe(dark);
});
test("stored light theme applies before first paint", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("harvis-theme", "light"));
  await mockDesk(page);
  await page.goto(base, { waitUntil: "commit" });
  // The inline head script runs before body parsing and hydration.
  await page.waitForSelector("body", { state: "attached" });
  expect(
    await page.evaluate(() => document.documentElement.dataset.theme),
  ).toBe("light");
});
test("dependency waits and holds retain status and bidirectional links", async ({
  page,
}) => {
  await mockDesk(page, depTasks());
  await page.goto(base);
  // Detail: prerequisites and dependents with status, and why it cannot resume.
  await page.locator("#task-22").getByRole("button").click();
  const detail = page.locator('[aria-label="Task detail"]');
  const deps = detail.getByRole("region", { name: "Dependencies" });
  await expect(
    deps.getByText(
      "Escalated: ops/human intervention required; explicitly resume; waiting on prerequisites #21; both must clear",
    ),
  ).toBeVisible();
  const blockedBy = deps.getByRole("list", { name: "Blocked by" });
  await expect(blockedBy.getByRole("listitem")).toHaveCount(2);
  await expect(
    blockedBy.getByRole("listitem").nth(0).locator("[data-state]"),
  ).toHaveAttribute("data-state", "done");
  await expect(
    blockedBy.getByRole("listitem").nth(1).locator("[data-state]"),
  ).toHaveAttribute("data-state", "in_progress");
  await page.screenshot({
    path: test.info().outputPath("dependencies-detail-dark-desktop.png"),
  });
  await blockedBy.getByRole("button", { name: "#21" }).click();
  await expect(detail.locator("h2").first()).toHaveText(
    "Build dependency dashboard",
  );
  const blocks = detail.getByRole("list", { name: "Blocks" });
  await expect(blocks.getByRole("listitem")).toHaveCount(2);
  await expect(blocks.getByText("Document dependencies")).toBeVisible();
  await blocks.getByRole("button", { name: "#23" }).click();
  await expect(
    detail.getByText(
      "Waiting on prerequisites #21; becomes ready automatically",
    ),
  ).toBeVisible();
  await expect(detail.locator("[data-state]").first()).toHaveAttribute(
    "data-state",
    "blocked",
  );
});

test("timed waits have local labels, UTC titles and a distinct filter", async ({ page }) => {
  await mockDesk(page, timedTasks);
  await page.goto(base);
  await expect(page.locator("tbody tr")).toHaveCount(2);
  await page.goto(base + "&view=tasks&state=waiting");
  const pill = page.locator('[data-state="waiting"]').first();
  await expect(pill).toHaveText(/^waiting until /);
  await expect(pill).toHaveAttribute("title", "2099-11-09T09:00:00.000Z");
  await page.getByRole("button", { name: timedTasks[0].title, exact: true }).click();
  await expect(page.getByText(/Waiting until .*becomes ready automatically/)).toBeVisible();
  await page.goto(base + "&view=tasks&state=blocked");
  await expect(page.getByText("blocked (deps #4)", { exact: true })).toBeVisible();
  await expect(page.locator('[data-state="waiting"]')).toHaveCount(0);
});
