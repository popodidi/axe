import { test, expect, type Page } from "@playwright/test";
const longSpec = Array.from(
  { length: 80 },
  (_, i) =>
    `## Section ${i}\n\nParagraph ${i} with enough content to scroll the task detail.\n`,
).join("\n");
async function mockTaskq(page: Page) {
  const tasks = Array.from({ length: 30 }, (_, i) => ({
    id: 30 - i,
    title: `Dashboard task ${30 - i}`,
    project: "other",
    state: "pending",
    detail: longSpec,
    created_at: new Date(Date.UTC(2026, 9, 27, 0, 30 - i)).toISOString(),
    updated_at: "2026-10-27T00:00:00Z",
    runs: 0,
  }));
  await page.route("http://taskq.test/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path === "/api/stream") {
      await route.abort();
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
    const match = path.match(/^\/api\/tasks\/(\d+)$/);
    const result =
      path === "/api/tasks"
        ? tasks
        : path === "/api/overview"
          ? { counts: [], running: 0 }
          : match
            ? tasks.find((t) => t.id === Number(match[1]))
            : [];
    await route.fulfill({
      json: result,
      headers: { "Access-Control-Allow-Origin": "*" },
    });
  });
}
const detail = '[aria-label="Task detail"]';
const list = '[aria-label="Task workspace"]';
async function scrollTo(page: Page, selector: string, y: number) {
  await page.locator(selector).evaluate((el, y) => {
    el.scrollTop = y;
  }, y);
  await expect
    .poll(() => page.locator(selector).evaluate((el) => el.scrollTop))
    .toBe(y);
}
test.beforeEach(async ({ page }) => {
  await mockTaskq(page);
});
test("closing and history navigation restore task/tab detail scroll", async ({
  page,
}) => {
  await page.goto("/?taskq=http://taskq.test&view=tasks&task=29");
  await expect(page.locator(`${detail} h2`).first()).toContainText(
    "Dashboard task 29",
  );
  await scrollTo(page, detail, 700);
  await page.keyboard.press("Escape");
  await expect(page.locator(detail)).toHaveCount(0);
  await page.goBack();
  await expect(page.locator(`${detail} h2`).first()).toContainText(
    "Dashboard task 29",
  );
  await page.getByRole("tab", { name: "events", exact: true }).click();
  await page.keyboard.press("Escape");
  await expect(page.locator(detail)).toHaveCount(0);
  expect(new URL(page.url()).searchParams.get("tab")).toBeNull();
  await page.goBack();
  await page.goBack();
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(700);
  await page.reload();
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(700);
  // Any point in the row opens the task.
  await page.locator("#task-28 [data-state]").click();
  await expect(page.locator(`${detail} h2`).first()).toContainText(
    "Dashboard task 28",
  );
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(0);
  await page.goBack();
  await expect(page.locator(`${detail} h2`).first()).toContainText(
    "Dashboard task 29",
  );
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(700);
  await page.screenshot({
    path: test.info().outputPath("detail-restored-desktop.png"),
  });
  await page.getByRole("tab", { name: "events", exact: true }).click();
  await expect(
    page.getByRole("tab", { name: "events", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await page.goBack();
  await expect(
    page.getByRole("tab", { name: "spec", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(700);
  await page.goForward();
  await expect(
    page.getByRole("tab", { name: "events", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(0);
});
test("shared focus is visible after delayed data, reload and history navigation", async ({
  page,
}) => {
  // Short enough that the focused row starts below the fold of the dense list.
  await page.setViewportSize({ width: 1440, height: 700 });
  const url =
    "/?taskq=http://taskq.test&view=tasks&focus=6&task=30&tab=events&project=other&q=Dashboard";
  await page.goto(url);
  const row = page.locator("#task-6");
  await expect(row).toBeAttached();
  async function expectVisibleFocus() {
    await expect
      .poll(() =>
        row.evaluate((el) => {
          const pane = el.closest('[aria-label="Task workspace"]')!.getBoundingClientRect();
          const r = el.getBoundingClientRect();
          return r.top >= pane.top && r.bottom <= pane.bottom;
        }),
      )
      .toBe(true);
  }
  await expectVisibleFocus();
  const y = await page.locator(list).evaluate((el) => el.scrollTop);
  expect(y).toBeGreaterThan(0);
  await page.reload();
  await expectVisibleFocus();
  await expect
    .poll(() => page.locator(list).evaluate((el) => el.scrollTop))
    .toBe(y);
  await page
    .getByRole("button", { name: "Stats", exact: true })
    .click();
  await page.goBack();
  await expectVisibleFocus();
  await expect
    .poll(() => page.locator(list).evaluate((el) => el.scrollTop))
    .toBe(y);
  await page.screenshot({
    path: test.info().outputPath("focus-restored-desktop.png"),
  });
  await scrollTo(page, list, 0);
  await expect
    .poll(() => new URL(page.url()).searchParams.get("listY"))
    .toBeNull();
  await page.waitForResponse("http://taskq.test/api/tasks?all=1");
  await expect
    .poll(() => page.locator(list).evaluate((el) => el.scrollTop))
    .toBe(0);
});
test("phone detail restores after close, Back and reload", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/?taskq=http://taskq.test&view=tasks&task=29");
  await expect(page.locator(`${detail} h2`).first()).toContainText(
    "Dashboard task 29",
  );
  await scrollTo(page, detail, 700);
  await page.keyboard.press("Escape");
  await expect(page.locator(detail)).toHaveCount(0);
  await page.goBack();
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(700);
  await page.reload();
  await expect
    .poll(() => page.locator(detail).evaluate((el) => el.scrollTop))
    .toBe(700);
  await expect(page.locator(list)).toBeHidden();
  await page.screenshot({
    path: test.info().outputPath("detail-restored-phone.png"),
  });
});
