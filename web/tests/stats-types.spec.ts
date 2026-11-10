import { test, expect, type Page } from "@playwright/test";
import { statsTypesFixture } from "./stats-types-fixture";
import { choose } from "./fixtures";
const base = "/?server=http://axe.test&view=stats";
const reads = (page: Page) => {
  const urls: string[] = [];
  page.on("request", (r) => {
    if (/\/api\/(stats\/history|tasks\/\d+\/(events|routine))$/.test(r.url()))
      urls.push(r.url());
  });
  return urls;
};
const split = (page: Page) =>
  page.getByRole("definition").evaluateAll((els) =>
    els.map((e) => [e.previousElementSibling?.textContent, e.textContent]),
  );
test("type filter scopes the whole report and reuses classification", async ({
  page,
}) => {
  await statsTypesFixture(page);
  const urls = reads(page);
  await page.goto(base);
  const scope = page.getByRole("region", { name: "Statistics scope" });
  const kpis = page.getByRole("region", { name: "Key figures" });
  const runsChart = page.getByRole("figure", {
    name: "Runs per task",
    exact: true,
  });
  // Default All: the split is visible and reconciles with the cohort.
  await expect(scope).toContainText("Showing all tasks (normal and routine)");
  await expect(scope).toContainText("69 completed cohort tasks");
  expect(await split(page)).toEqual([
    ["Normal", "19"],
    ["Routine", "50"],
    ["Unknown type", "0"],
    ["All completed", "69"],
  ]);
  await expect(kpis).toContainText("Completed tasks69");
  await expect(kpis).toContainText("19 normal · 50 routine · 3 still open");
  await expect(scope).not.toContainText("Type unknown");
  await expect(scope).toContainText("3 open tasks");
  await expect(page.getByLabel("Task type", { exact: true })).toHaveAttribute(
    "data-value",
    "all",
  );
  const before = urls.length;
  // One stats history read classifies every task: no per-task reads.
  expect(urls).toEqual(["http://axe.test/api/stats/history"]);

  await choose(page.getByLabel("Task type", { exact: true }), "normal");
  await expect(page).toHaveURL(/type=normal/);
  await expect(scope).toContainText("Showing normal tasks only");
  await expect(scope).toContainText("19 completed normal cohort tasks");
  await expect(scope).toContainText("1 open normal tasks");
  await expect(scope).not.toContainText("unknown type excluded");
  await expect(kpis).toContainText("Completed normal tasks19");
  await expect(runsChart).toContainText("0: Normal tasks: 19");
  await expect(
    page.getByRole("figure", { name: "Completions per day", exact: true }),
  ).toContainText("Normal tasks");
  await expect(
    page.getByRole("region", { name: "Completed normal tasks", exact: true }),
  ).toBeVisible();
  await expect(
    page
      .getByRole("figure", { name: "Runs and escalations", exact: true })
      .getByRole("button"),
  ).toHaveCount(19);
  // The split stays visible and unchanged in a typed scope.
  expect((await split(page))[1]).toEqual(["Routine", "50"]);

  await choose(page.getByLabel("Task type", { exact: true }), "routine");
  await expect(scope).toContainText("50 completed routine cohort tasks");
  await expect(scope).toContainText("2 open routine tasks");
  await expect(runsChart).toContainText("0: Routine tasks: 50");
  // Toggling type reuses the loaded history: no further reads.
  expect(urls.length).toBe(before);

  // Composes with project and reload.
  await choose(page.getByLabel("project", { exact: true }), "orbit");
  await expect(scope).toContainText("25 completed routine cohort tasks");
  await page.reload();
  await expect(page.getByLabel("Task type", { exact: true })).toHaveAttribute(
    "data-value",
    "routine",
  );
  await expect(scope).toContainText("25 completed routine cohort tasks");
  // Back/Forward restore the type and project scope.
  await page.goBack();
  await expect(scope).toContainText("50 completed routine cohort tasks");
  await page.goBack();
  await expect(scope).toContainText("19 completed normal cohort tasks");
  await page.goBack();
  await expect(scope).toContainText("Showing all tasks");
  await page.goForward();
  await expect(scope).toContainText("Showing normal tasks only");

  // Date composition: the window's first UTC day.
  await choose(page.getByLabel("Date range"), "custom");
  await page.getByLabel("Start date (UTC)", { exact: true }).fill("2026-10-22");
  await page.getByLabel("End date (UTC, inclusive)").fill("2026-10-22");
  await expect(
    page.getByText(
      "No known completed normal tasks in this range match the filters.",
    ),
  ).toBeVisible();
  // The split still shows the routine completed that day.
  expect((await split(page))[1]).toEqual(["Routine", "1"]);
});
test("group by task type and task detail keep the scope", async ({ page }) => {
  await statsTypesFixture(page);
  await page.goto(base);
  await choose(page.getByLabel("Split by", { exact: true }), "type");
  const chart = page.getByRole("figure", { name: "Runs per task", exact: true });
  await expect(chart).toContainText("Normal: 19");
  await expect(chart).toContainText("Routine: 50");
  await expect(chart).not.toContainText("Unknown type");
  await choose(page.getByLabel("Task type", { exact: true }), "routine");
  const dots = page
    .getByRole("figure", { name: "Runs and escalations", exact: true })
    .getByRole("button");
  await expect(dots).toHaveCount(50);
  // Dots overlap at (0, 0) in this fixture; open one from the keyboard.
  await dots.first().focus();
  await page.keyboard.press("Enter");
  await expect(page.getByLabel("Task detail", { exact: true })).toBeVisible();
  await expect(page).toHaveURL(/type=routine/);
  await page.keyboard.press("Escape");
  await expect(page.getByLabel("Task detail", { exact: true })).toHaveCount(0);
  await expect(page.getByLabel("Task type", { exact: true })).toHaveAttribute(
    "data-value",
    "routine",
  );
});
for (const width of [1440, 390])
  for (const theme of ["dark", "light"])
    test(`type control ${width} ${theme}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 1400 });
      await page.addInitScript(
        (t) => localStorage.setItem("axe-theme", t),
        theme,
      );
      await statsTypesFixture(page);
      await page.goto(base + "&type=normal");
      const control = page.getByLabel("Task type", { exact: true });
      await expect(control).toBeVisible();
      await expect(control).toHaveAttribute("role", "combobox");
      const box = (await control.boundingBox())!;
      expect(box.x + box.width).toBeLessThanOrEqual(width);
      expect(box.height).toBeLessThanOrEqual(width > 760 ? 34 : 46);
      // Keyboard: Date range -> Task type -> Split by.
      await page.getByLabel("Date range").focus();
      await page.keyboard.press("Tab");
      await expect(control).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(page.getByLabel("Split by", { exact: true })).toBeFocused();
      const dl = page.locator('dl[aria-label="Completed by type"]');
      await expect(dl).toBeVisible();
      const dlBox = (await dl.boundingBox())!;
      expect(dlBox.x + dlBox.width).toBeLessThanOrEqual(width);
      await expect(
        page.getByRole("region", { name: "Statistics scope" }),
      ).toContainText("Showing normal tasks only");
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      await page.getByLabel("Split by", { exact: true }).blur();
      await page.screenshot({
        path: test.info().outputPath(`types-${width}-${theme}.png`),
      });
    });
