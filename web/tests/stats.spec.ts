import { test, expect } from "@playwright/test";
import { statsFixture } from "./stats-fixture";
import { choose } from "./fixtures";
const base = "/?taskq=http://taskq.test&view=stats";
test("completion ranges, custom dates and filters survive navigation", async ({
  page,
}) => {
  await statsFixture(page);
  await page.goto(base);
  const scope = page.getByRole("region", { name: "Statistics scope" });
  await expect(scope).toContainText("3 completed cohort tasks");
  await expect(scope).toContainText("6 open tasks (not sampled)");
  const summary = page.getByRole("region", {
    name: "All completed tasks",
    exact: true,
  });
  const row = summary.locator("tbody tr").filter({ hasText: "Runs per task" });
  await expect(row.locator("td")).toHaveText([
    "All tasks",
    "3",
    "Runs per task",
    "3 / 0",
    "1",
    "2",
    "2",
  ]);
  await choose(page.getByLabel("Date range"), "30");
  await expect(scope).toContainText("4 completed cohort tasks");
  await choose(page.getByLabel("Date range"), "90");
  await expect(scope).toContainText("4 completed cohort tasks");
  await choose(page.getByLabel("Date range"), "custom");
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2026-10-28");
  await page.getByLabel("End date (UTC, inclusive)").fill("2026-10-28");
  await expect(scope).toContainText("3 completed cohort tasks");
  await choose(page.getByLabel("project", { exact: true }), "harvis");
  await expect(scope).toContainText("1 completed cohort tasks");
  await page.getByRole("button", { name: "Tasks", exact: true }).click();
  await page.getByRole("button", { name: "Stats", exact: true }).click();
  await page.reload();
  await expect(
    page.getByLabel("Start date (UTC)", { exact: true }),
  ).toHaveValue("2026-10-28");
  await expect(scope).toContainText("1 completed cohort tasks");
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2026-10-29");
  await expect(
    page.getByRole("alert").filter({ hasText: "Choose valid" }),
  ).toBeVisible();
  await expect(page.getByRole("figure")).toHaveCount(0);
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2026-10-01");
  await page.getByLabel("End date (UTC, inclusive)").fill("2026-10-01");
  await expect(
    page.getByText("No known completed tasks in this range match the filters."),
  ).toBeVisible();
  // Back restores the previous range (one history entry per date change).
  await page.goBack();
  await expect(
    page.getByLabel("End date (UTC, inclusive)"),
  ).toHaveValue("2026-10-28");
  await expect(scope).toContainText("completed cohort tasks");
});
for (const width of [1440, 390])
  for (const theme of ["dark", "light"])
    test(`distributions ${width} ${theme}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 1600 });
      await page.addInitScript(
        (t) => localStorage.setItem("harvis-theme", t),
        theme,
      );
      await statsFixture(page);
      await page.goto("/?taskq=http://taskq.test");
      await expect(page.getByLabel("state", { exact: true })).toHaveAttribute("data-value", 
        "active",
      );
      await page.getByRole("button", { name: "Stats", exact: true }).click();
      // One-page report: completions, four trends, two scatter plots, four
      // histograms.
      await expect(page.getByRole("figure")).toHaveCount(11);
      await expect(page.getByLabel("Search tasks")).toHaveCount(0);
      const kpis = page.getByRole("region", { name: "Key figures" });
      await expect(kpis).toContainText("Completed tasks");
      await expect(
        page.getByRole("figure", { name: "Escalations per task", exact: true }),
      ).toContainText("0: All tasks: 2");
      // Scatter: one dot per task with known runs; a dot opens its task.
      const dots = page
        .getByRole("figure", { name: "Runs and escalations", exact: true })
        .getByRole("button");
      await expect(dots).toHaveCount(3);

      await expect(
        page.getByRole("region", { name: "Execution timeline" }),
      ).toHaveCount(0);
      await expect(
        page.getByRole("region", { name: "Task metrics", exact: true }),
      ).toHaveCount(0);
      for (const title of [
        "Runs per task",
        "Escalations per task",
        "Wall-clock elapsed time",
        "Active agent runtime",
      ])
        await expect(
          page.getByRole("figure", { name: title, exact: true }),
        ).toContainText("Tasks by");
      await expect(
        page.getByText("3 samples · 0 unknown", { exact: false }),
      ).toHaveCount(4);
      const picker = page.getByLabel("Date range");
      expect((await picker.boundingBox())!.width).toBeLessThan(200);
      expect((await picker.boundingBox())!.height).toBeLessThanOrEqual(
        width > 760 ? 34 : 46,
      );
      await picker.focus();
      await page.keyboard.press("Tab");
      await expect(page.getByLabel("Task type", { exact: true })).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(page.getByLabel("Split by", { exact: true })).toBeFocused();
      await page.screenshot({
        path: test.info().outputPath(`default-${width}-${theme}.png`),
      });
      const chart = page.getByRole("figure", {
        name: "Runs per task",
        exact: true,
      });
      await page.getByLabel("Split by", { exact: true }).focus();
      await page.keyboard.press("ArrowDown");
      await page.keyboard.press("Enter");
      await page.keyboard.press("Escape");
      await expect(page.getByLabel("Split by", { exact: true })).toHaveAttribute("data-value", 
        "project",
      );
      await expect(chart).toContainText("harvis: 1");
      await expect(chart).toContainText("rake: 1");
      await page.screenshot({
        path: test.info().outputPath(`project-${width}-${theme}.png`),
      });
      await choose(page.getByLabel("Split by", { exact: true }), "agent");
      await expect(chart).toContainText("Mixed: claude + codex: 1");
      await page.getByLabel("Split by", { exact: true }).blur();
      await page.screenshot({
        path: test.info().outputPath(`agent-${width}-${theme}.png`),
      });
      for (const figure of await page.getByRole("figure").all()) {
        await figure.scrollIntoViewIfNeeded();
        const box = (await figure.boundingBox())!;
        expect(box.x).toBeGreaterThanOrEqual(0);
        expect(box.x + box.width).toBeLessThanOrEqual(width);
      }
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      // A dot opens its task in the detail pane; Escape closes it.
      await dots.first().click();
      await expect(page.getByLabel("Task detail", { exact: true })).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(page.getByLabel("Task detail", { exact: true })).toHaveCount(0);
    });
