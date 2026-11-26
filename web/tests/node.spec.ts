import { test, expect } from "@playwright/test";
import { choose } from "./fixtures";
import { mockNode } from "./node-fixture";

for (const theme of ["dark", "light"]) for (const width of [1440, 390]) {
  test(`node view ${theme} ${width}`, async ({ page }, info) => {
    await page.setViewportSize({ width, height: 1100 });
    await page.addInitScript((theme) => localStorage.setItem("axe-theme", theme), theme);
    const ranges = await mockNode(page);
    await page.goto("/?server=http://axe.test");
    await page.getByRole("button", { name: "Node", exact: true }).click();
    await expect(page).toHaveURL(/view=node/);
    await expect(page.getByRole("heading", { name: "Node", level: 1 })).toBeVisible();
    const kpis = page.getByRole("region", { name: "Current values" });
    await expect(kpis).toContainText("23%");
    await expect(kpis).toContainText("6.2 GiB");
    await expect(kpis).toContainText("of 16.0 GiB · 39%");
    await expect(kpis).toContainText("120.4 GiB");
    await expect(kpis).toContainText("3 task runs · 1 bot runs · 2 services · 0 scheduled runs");
    for (const name of ["CPU", "Memory used", "Running runs by kind"])
      await expect(page.getByRole("figure", { name })).toBeVisible();
    // Stacked chart: a legend names every kind.
    const runs = page.getByRole("figure", { name: "Running runs by kind" });
    for (const k of ["Task runs", "Bot runs", "Services", "Scheduled runs"])
      await expect(runs.getByRole("list", { name: "Legend" })).toContainText(k);
    await expect(page.getByRole("region", { name: "Running now" })).toContainText("harvis 2, tiktape");
    // The crosshair readout lists every series at the hovered time.
    await runs.locator("svg[role=group]").scrollIntoViewIfNeeded();
    const box = (await runs.locator("svg[role=group]").boundingBox())!;
    await page.mouse.move(box.x + box.width * 0.3, box.y + box.height / 2);
    await expect(runs.locator("[aria-live=polite]")).toContainText(/UTC · Task runs \d/);
    await expect(runs.locator("[aria-live=polite]")).toContainText("Services 2");
    // No horizontal overflow at any width.
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    expect(overflow).toBeLessThanOrEqual(0);
    await page.screenshot({ path: info.outputPath(`node-${theme}-${width}.png`), fullPage: true });
    expect(ranges.at(-1)).toBe("6h");
    await choose(page.getByRole("combobox", { name: "Time range" }), "7d");
    await expect(page).toHaveURL(/range=7d/);
    await expect.poll(() => ranges.at(-1)).toBe("7d");
    await expect(page.getByRole("figure", { name: "CPU" })).toContainText("5-minute rollups, averaged per 30 min");
  });
}

test("node view before any sample", async ({ page }) => {
  await mockNode(page, true);
  await page.goto("/?server=http://axe.test&view=node");
  await expect(page.getByText("No node samples yet.")).toBeVisible();
});
