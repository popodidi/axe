import { test, expect, type Page } from "@playwright/test";
import { choose } from "./fixtures";
import { statsTrendsFixture, trendIds, completedAt } from "./stats-trends-fixture";
const base = "/?taskq=http://taskq.test&view=stats";
const titles = [
  "Runs per task over time",
  "Escalations per task over time",
  "Elapsed time over time",
  "Agent runtime over time",
];
const trend = (page: Page, name: string) =>
  page.getByRole("figure", { name, exact: true });
const inRange = (days: number, keep = (_id: number) => true) => {
  const start = Date.parse("2026-10-29T00:00:00Z") - (days - 1) * 86400000;
  return trendIds.filter((id) => completedAt(id) >= start && keep(id)).length;
};
test("trend charts follow range, scope and split, with readable groups", async ({
  page,
}) => {
  await statsTrendsFixture(page);
  await page.goto(base);
  const section = page.getByRole("region", { name: "Trends" });
  await expect(section.getByRole("figure")).toHaveCount(4);
  // Last 7 days: daily buckets, labelled axes, units and sample counts.
  for (const name of titles) {
    const f = trend(page, name);
    await expect(f).toContainText("per day");
    await expect(f).toContainText("UTC completion date");
    await expect(f).toContainText(`${inRange(7)} known samples · 0 unknown`);
    await expect(f.locator("ul li")).toHaveCount(7);
  }
  await expect(trend(page, titles[0])).toContainText("Runs (mean)");
  await expect(trend(page, titles[2])).toContainText("Elapsed (log scale)");
  await expect(trend(page, titles[2])).toContainText("log scale from 1 min");
  // Duration ticks use the compact form of the existing charts.
  await expect(trend(page, titles[3]).locator("svg text", { hasText: /^10m$/ })).toHaveCount(1);
  await section.screenshot({ path: test.info().outputPath("trends-default.png") });
  // Readout by keyboard: focusing lands on the latest bucket with samples.
  const runsChart = trend(page, titles[0]);
  const plot = runsChart.getByRole("group");
  await plot.focus();
  const readout = runsChart.locator("p[aria-live]");
  await expect(readout).toContainText("All tasks · 10-29 · n 1 · mean 1 · SD 0");
  await page.keyboard.press("ArrowLeft");
  await expect(readout).toContainText("All tasks · 10-28 · n");
  await page.keyboard.press("Home");
  await expect(readout).toContainText("10-23");
  // Hover readout on the elapsed chart, formatted like other durations.
  await trend(page, titles[2]).locator("circle").last().hover();
  await expect(trend(page, titles[2]).locator("p[aria-live]")).toContainText(
    /mean \d+h \d+m · SD/,
  );

  // 30 days stays daily; 90 days switches to 13 weekly buckets.
  await choose(page.getByLabel("Date range"), "30");
  await expect(runsChart.locator("ul li")).toHaveCount(30);
  await expect(runsChart).toContainText(`${inRange(30)} known samples`);
  await choose(page.getByLabel("Date range"), "90");
  await expect(runsChart).toContainText("per week");
  await expect(runsChart).toContainText("UTC completion week");
  await expect(runsChart.locator("ul li")).toHaveCount(13);
  await expect(runsChart).toContainText(`${inRange(90)} known samples`);
  await section.screenshot({ path: test.info().outputPath("trends-weekly.png") });

  // Custom range: 32 inclusive days is weekly, 31 daily.
  await choose(page.getByLabel("Date range"), "custom");
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2026-09-28");
  await page.getByLabel("End date (UTC, inclusive)").fill("2026-10-29");
  await expect(runsChart.locator("ul li")).toHaveCount(5);
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2026-09-29");
  await expect(runsChart.locator("ul li")).toHaveCount(31);
  await expect(runsChart).toContainText("per day");

  // Type scope filters the samples.
  await choose(page.getByLabel("Date range"), "90");
  const routine = inRange(90, (id) => (id - 3000) % 4 === 0);
  await choose(page.getByLabel("Task type", { exact: true }), "routine");
  await expect(runsChart).toContainText(`${routine} known samples`);
  await choose(page.getByLabel("Task type", { exact: true }), "normal");
  await expect(runsChart).toContainText(`${inRange(90) - routine} known samples`);
  await choose(page.getByLabel("Task type", { exact: true }), "all");

  // Split by project: one mean line per group, only the highlighted band.
  await choose(page.getByLabel("Split by", { exact: true }), "project");
  const legend = runsChart.getByRole("list", { name: "Legend: highlight a group" });
  await expect(legend.getByRole("button")).toHaveText(["harvis", "rake", "tn-desk"]);
  await expect(legend.getByRole("button", { name: "harvis" })).toHaveAttribute("aria-pressed", "true");
  await expect(runsChart.locator("path[class*=band], rect[class*=band]").first()).toBeVisible();
  const bands = async () =>
    runsChart.locator("g:has(> [class*=band])").count();
  expect(await bands()).toBe(1);
  await legend.getByRole("button", { name: "rake" }).click();
  await expect(legend.getByRole("button", { name: "rake" })).toHaveAttribute("aria-pressed", "true");
  expect(await bands()).toBe(1);
  await plot.focus();
  await page.keyboard.press("End");
  await expect(runsChart.locator("p[aria-live]")).toContainText(/^rake · /);
  await page.keyboard.press("ArrowDown");
  await expect(runsChart.locator("p[aria-live]")).toContainText(/^tn-desk · /);
  await expect(legend.getByRole("button", { name: "tn-desk" })).toHaveAttribute("aria-pressed", "true");
  await expect(runsChart.locator("ul li").filter({ hasText: /^harvis · / })).toHaveCount(13);
  await page.getByLabel("Split by", { exact: true }).focus();
  await section.screenshot({ path: test.info().outputPath("trends-project.png") });

  await choose(page.getByLabel("Split by", { exact: true }), "agent");
  await expect(legend.getByRole("button")).toHaveText(["claude", "codex"]);
  await section.screenshot({ path: test.info().outputPath("trends-agent.png") });
  await choose(page.getByLabel("Split by", { exact: true }), "none");
  await expect(legend).toHaveCount(0);
});
test("partial history and empty ranges stay explicit in trends", async ({ page }) => {
  await statsTrendsFixture(page, { missingRuns: true });
  await page.goto(base);
  const n = inRange(7);
  await expect(trend(page, titles[0])).toContainText(`0 known samples · ${n} unknown`);
  await expect(trend(page, titles[0])).toContainText("No known samples in this range");
  await expect(trend(page, titles[3])).toContainText(`0 known samples · ${n} unknown`);
  await expect(trend(page, titles[1])).toContainText(`${n} known samples · 0 unknown`);
  await expect(trend(page, titles[2])).toContainText(`${n} known samples · 0 unknown`);
  await choose(page.getByLabel("Date range"), "custom");
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2025-01-01");
  await page.getByLabel("End date (UTC, inclusive)").fill("2025-01-02");
  await expect(
    page.getByText("No known completed tasks in this range match the filters."),
  ).toBeVisible();
  await expect(page.getByRole("region", { name: "Trends" })).toHaveCount(0);
});
test("loading and error states cover trends", async ({ page }) => {
  await statsTrendsFixture(page);
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  await page.route("**/api/stats/history", async (route) => {
    await gate;
    await route.fulfill({ status: 503 });
  });
  await page.goto(base);
  await expect(page.getByRole("status", { name: "Loading history…" })).toBeVisible();
  await expect(page.getByRole("region", { name: "Trends" })).toHaveCount(0);
  release();
  await expect(page.getByRole("alert").filter({ hasText: "History unavailable" })).toBeVisible();
  await expect(page.getByRole("region", { name: "Trends" })).toHaveCount(0);
});
for (const width of [1440, 390])
  for (const theme of ["dark", "light"])
    test(`trends layout ${width} ${theme}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 1400 });
      await page.addInitScript((t) => localStorage.setItem("harvis-theme", t), theme);
      await statsTrendsFixture(page);
      await page.goto(base + "&days=30");
      const section = page.getByRole("region", { name: "Trends" });
      await expect(section.getByRole("figure")).toHaveCount(4);
      await choose(page.getByLabel("Split by", { exact: true }), "project");
      await section.scrollIntoViewIfNeeded();
      for (const f of await section.getByRole("figure").all()) {
        const box = (await f.boundingBox())!;
        expect(box.x).toBeGreaterThanOrEqual(0);
        expect(box.x + box.width).toBeLessThanOrEqual(width);
        // The plot stays legible: at least 300px wide on a phone.
        expect((await f.locator("svg[role=group]").boundingBox())!.width).toBeGreaterThan(300);
      }
      expect(
        await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),
      ).toBe(true);
      // Keyboard: Tab reaches the legend buttons and the plot.
      const first = section.getByRole("figure").first();
      await first.getByRole("button", { name: "harvis" }).focus();
      await page.keyboard.press("Tab");
      await page.keyboard.press("Tab");
      await page.keyboard.press("Tab");
      await expect(first.getByRole("group")).toBeFocused();
      await page.keyboard.press("ArrowLeft");
      await expect(first.locator("p[aria-live]")).toContainText(/^harvis · /);
      await section.screenshot({
        path: test.info().outputPath(`trends-${width}-${theme}.png`),
      });
    });
