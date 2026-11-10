import { test, expect, type Locator, type Page } from "@playwright/test";
import { choose, mockDesk } from "./fixtures";
const routine = { id: 9, name: "Acceptance routine", project: "harvis", cron: "0 9 * * 1-5", timezone: "Europe/London", title: "Record acceptance", detail: "## Outcome\nRecord acceptance only.", enabled: true, archived: false, next_at: 1793696400, latest_task: 3 };
async function inViewport(locator: Locator, page: Page, minWidth: number) {
  await expect(locator).toBeVisible();
  await expect.poll(async () => {
    const box = await locator.boundingBox();
    const viewport = page.viewportSize()!;
    return !!box && box.x >= 0 && box.y >= 0 && box.width >= minWidth
      && box.x + box.width <= viewport.width + 1
      && box.y + box.height <= viewport.height + 1;
  }).toBe(true);
}
for (const theme of ["dark", "light"]) for (const width of [1440, 761, 760, 390, 320]) {
  test.describe(() => {
    test.use({ hasTouch: true });
    test(`routines navigation and history ${theme} ${width}`, async ({ page }, info) => {
      await page.setViewportSize({ width, height: 1000 });
      await page.addInitScript(theme => localStorage.setItem("harvis-theme", theme), theme);
      if (width === 320) await page.emulateMedia({ reducedMotion: "reduce" });
      await mockDesk(page);
      await page.route("http://taskq.test/api/routines**", route => route.fulfill({ json: route.request().url().includes("history") ? Array.from({ length: 30 }, (_, i) => ({ id: i + 1, task_id: i === 0 ? 3 : 4, identity: `scheduled:${1793696400 - i}`, title: "Record acceptance", state: "in_progress", created_at: "2026-11-03T09:00:00Z" })) : [routine] }));
      await page.route("http://taskq.test/api/tasks/3/routine", route => route.fulfill({ json: { routine_id: 9, identity: "scheduled:1793696400" } }));
      await page.goto("/?taskq=http://taskq.test&project=harvis&q=Acceptance");
      await page.getByRole("button", { name: "Routines", exact: true }).click();
      await expect(page).toHaveURL(/view=routines/);
      // A row opens the routine in a side pane.
      await page.getByRole("button", { name: "Acceptance routine", exact: true }).click();
      const pane = page.getByRole("region", { name: "Routine detail", includeHidden: true });
      await expect(pane.getByText("0 9 * * 1-5", { exact: true })).toBeVisible();
      await expect(pane.getByText("Routine history", { exact: true })).toBeVisible();
      await expect(pane.getByText("Europe/London", { exact: true })).toBeVisible();
      await expect(pane.getByText("Enabled", { exact: true })).toBeVisible();
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      await page.screenshot({ path: info.outputPath(`routines-${theme}-${width}.png`), fullPage: true });
      // A generated task stacks its own pane; Back returns to the routine.
      const first = pane.locator('[data-open-task="3"]');
      await pane.evaluate(el => { el.scrollTop = 120; });
      const routineScroll = await pane.evaluate(el => el.scrollTop);
      expect(routineScroll).toBeGreaterThan(0);
      const point = (await first.boundingBox())!;
      await page.touchscreen.tap(point.x + point.width / 2, point.y + point.height / 2);
      await expect(page).toHaveURL(/routine=9/);
      await expect(page).toHaveURL(/task=3/);
      const task = page.getByLabel("Task detail", { exact: true });
      await expect(task.getByRole("button", { name: "Routine #9", exact: true })).toBeVisible();
      const assertPanels = async () => {
        // Above the phone breakpoint preserve the existing side-by-side layout;
        // full task viewport geometry is asserted at the supported desktop width.
        if (width !== 761) {
          await inViewport(task, page, width <= 760 ? width - 1 : 160);
          await inViewport(task.locator("header"), page, 140);
          await inViewport(task.getByRole("button", { name: /^Back to routine/ }), page, 80);
        }
        if (width <= 760) {
          await expect(pane).toBeHidden();
          // display:none must remove every history control from keyboard navigation.
          await task.getByRole("button", { name: /^Back to routine/ }).focus();
          await page.keyboard.press("Tab");
          expect(await pane.evaluate(el => el.contains(document.activeElement))).toBe(false);
        } else {
          await inViewport(pane, page, 300);
          await expect.poll(async () => {
            const routineBox = (await pane.boundingBox())!;
            const taskBox = (await task.boundingBox())!;
            return routineBox.x + routineBox.width - taskBox.x;
          }).toBeLessThanOrEqual(1);
        }
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      };
      await assertPanels();
      await page.screenshot({ path: info.outputPath(`routines-stacked-${theme}-${width}.png`) });
      await page.getByRole("button", { name: /^Back to routine/ }).click();
      await expect(pane).toBeVisible();
      await expect.poll(() => pane.evaluate(el => el.scrollTop)).toBe(routineScroll);
      await expect(page).toHaveURL(/project=harvis/);
      await expect(page).toHaveURL(/q=Acceptance/);
      await page.goBack();
      await assertPanels();
      await page.goForward();
      await expect(pane).toBeVisible();
      await pane.locator('[data-open-task="4"]').first().focus();
      await page.keyboard.press("Enter");
      await expect(page).toHaveURL(/task=4/);
      await assertPanels();
      // A reload exercises the same routine+task deep link without opener state.
      await page.reload();
      await assertPanels();
      await page.getByRole("button", { name: /^Back to routine/ }).click();
      await expect(page).not.toHaveURL(/task=/);
      await expect(pane.getByText("Routine history", { exact: true })).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(page).not.toHaveURL(/routine=9/);
      await expect(pane).toHaveCount(0);
    });
  });
}
test("routine loading empty and error states", async ({ page }) => {
  await mockDesk(page);
  let mode = "loading";
  await page.route("http://taskq.test/api/routines", async route => {
    await new Promise(r => setTimeout(r, 300));
    await route.fulfill(mode === "error" ? { status: 503, body: "unavailable" } : { json: [] });
  });
  await page.goto("/?taskq=http://taskq.test&view=routines");
  await expect(page.getByLabel("Loading routines…")).toBeVisible();
  await expect(page.getByText("No matching routines")).toBeVisible();
  mode = "error";
  await page.reload();
  await expect(page.getByText(/Unable to load routines/)).toBeVisible();
});
test("routine history loading empty and error", async ({ page }) => {
  await mockDesk(page);
  await page.route("http://taskq.test/api/routines", route => route.fulfill({ json: [routine] }));
  let broken = false;
  await page.route("http://taskq.test/api/routines/9/history", async route => {
    await new Promise(r => setTimeout(r, 400));
    await route.fulfill(broken ? { status: 503, body: "unavailable" } : { json: [] });
  });
  await page.goto("/?taskq=http://taskq.test&view=routines&routine=9");
  await expect(page.getByLabel("Loading routine history…")).toBeVisible();
  await expect(page.getByText("No occurrences yet")).toBeVisible();
  broken = true;
  await page.reload();
  await expect(page.getByText(/Unable to load history/)).toBeVisible();
});
test.describe("routine status filter", () => {
  const rows = [
    { ...routine, id: 1, name: "Nightly enabled", project: "harvis" },
    { ...routine, id: 2, name: "Nightly paused", project: "harvis", enabled: false },
    { ...routine, id: 3, name: "Nightly archived", project: "harvis", enabled: false, archived: true },
    { ...routine, id: 4, name: "Weekly archived", project: "rake", enabled: true, archived: true },
    { ...routine, id: 5, name: "Weekly enabled", project: "rake" },
  ];
  const shown = (page: Page) => page.locator("tbody tr td:first-child button");
  const open = async (page: Page, query = "") => {
    await mockDesk(page);
    await page.route("http://taskq.test/api/routines", route => route.fulfill({ json: rows }));
    await page.goto(`/?taskq=http://taskq.test&view=routines${query}`);
    await expect(shown(page).first()).toBeVisible();
    return page.getByLabel("status", { exact: true });
  };
  const all = rows.map(r => r.name);
  test("defaults to All and each status shows only its routines", async ({ page }) => {
    const status = await open(page);
    await expect(status).toHaveAttribute("data-value", "");
    await expect(shown(page)).toHaveText(all);
    await choose(status, "enabled");
    await expect(page).toHaveURL(/status=enabled/);
    await expect(shown(page)).toHaveText(["Nightly enabled", "Weekly enabled"]);
    await choose(status, "paused");
    await expect(shown(page)).toHaveText(["Nightly paused"]);
    await choose(status, "archived");
    await expect(shown(page)).toHaveText(["Nightly archived", "Weekly archived"]);
    await choose(status, "");
    await expect(page).not.toHaveURL(/status=/);
    await expect(shown(page)).toHaveText(all);
  });
  test("offers every status the routine model can have", async ({ page }) => {
    const status = await open(page);
    await status.click();
    await expect(page.locator('[role="listbox"] [role="option"]')).toHaveText(["All", "Enabled", "Paused", "Archived"]);
  });
  test("combines with Project and search and survives reload", async ({ page }) => {
    const status = await open(page, "&project=rake");
    await expect(shown(page)).toHaveText(["Weekly archived", "Weekly enabled"]);
    await choose(status, "archived");
    await expect(shown(page)).toHaveText(["Weekly archived"]);
    await expect(page).toHaveURL(/project=rake/);
    await page.reload();
    await expect(page.getByLabel("status", { exact: true })).toHaveAttribute("data-value", "archived");
    await expect(shown(page)).toHaveText(["Weekly archived"]);
    await page.goto("/?taskq=http://taskq.test&view=routines&status=archived&q=nightly");
    await expect(shown(page)).toHaveText(["Nightly archived"]);
    await page.goto("/?taskq=http://taskq.test&view=routines&status=paused&project=rake");
    await expect(page.getByText("No matching routines")).toBeVisible();
  });
  for (const width of [390, 320]) test(`fits and filters at phone width ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 });
    const status = await open(page);
    await inViewport(status, page, 60);
    await choose(status, "archived");
    await expect(shown(page)).toHaveText(["Nightly archived", "Weekly archived"]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });
  test("is only offered on Routines", async ({ page }) => {
    await mockDesk(page);
    await page.goto("/?taskq=http://taskq.test&status=archived");
    await expect(page.getByLabel("Search tasks")).toBeVisible();
    await expect(page.getByLabel("status", { exact: true })).toHaveCount(0);
  });
});
