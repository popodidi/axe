import { test, expect } from "@playwright/test";
import { compactFixture } from "./compact-fixture";
import { mkdirSync, writeFileSync } from "node:fs";
// Opt-in snapshots contain synthetic fixtures only. Normal CI runs the geometry checks.
for (const theme of ["dark", "light"])
  for (const width of [390, 768, 1024, 1440]) {
    test(`compact evidence ${theme} ${width}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 1000 });
      await page.addInitScript(
        (t) => localStorage.setItem("harvis-theme", t),
        theme,
      );
      await compactFixture(page);
      for (const tab of ["runs", "artifacts"]) {
        await page.goto(`/?taskq=http://taskq.test&task=3&tab=${tab}`);
        const detail = page.getByLabel("Task detail", { exact: true });
        await expect(detail).toContainText(
          tab === "runs" ? "npm run test:browser" : "layout-17.png",
        );
        await expect
          .poll(() =>
            page.evaluate(
              () => document.documentElement.scrollWidth - innerWidth,
            ),
          )
          .toBe(0);
        if (process.env.EVIDENCE_DIR) {
          mkdirSync(process.env.EVIDENCE_DIR, { recursive: true });
          await detail.screenshot({
            path: `${process.env.EVIDENCE_DIR}/${tab}-${theme}-${width}.png`,
          });
        }
        const rows =
          tab === "runs"
            ? detail.locator("article[data-live]")
            : detail.locator(
                process.env.BASELINE
                  ? "ul[aria-label=Artifacts] li"
                  : "tbody tr[data-artifact]",
              );
        const heights = await rows.evaluateAll((els) =>
          els.map((e) => e.getBoundingClientRect().height),
        );
        if (process.env.EVIDENCE_DIR)
          writeFileSync(
            `${process.env.EVIDENCE_DIR}/${tab}-${theme}-${width}.json`,
            JSON.stringify({ width, theme, tab, heights }, null, 2),
          );
        if (process.env.BASELINE) continue;
        for (const height of heights)
          expect(height).toBeGreaterThanOrEqual(tab === "runs" ? 64 : 36);
        for (const height of heights)
          expect(height).toBeLessThanOrEqual(tab === "runs" ? 72 : 40);
        if (tab === "runs")
          await detail.getByRole("button", { name: "Run #10 details" }).click();
        else
          await detail
            .getByRole("button", { name: /^Reference details:/ })
            .first()
            .click();
        await expect
          .poll(() =>
            page.evaluate(
              () => document.documentElement.scrollWidth - innerWidth,
            ),
          )
          .toBe(0);
        const bounds = await detail.evaluate((el) => {
          const pane = el.getBoundingClientRect();
          return Array.from(el.querySelectorAll("button, summary"))
            .filter((e) => e.getBoundingClientRect().width > 0)
            .every((e) => {
              const r = e.getBoundingClientRect();
              return r.left >= pane.left && r.right <= pane.right;
            });
        });
        expect(bounds).toBe(true);
        if (tab === "artifacts") {
          const unused = await detail.locator("table").evaluate((table) => {
            const cells = Array.from(
              table.querySelector("tr[data-artifact]")!.children,
            ).filter((e) => e.getBoundingClientRect().width > 0);
            return (
              table.getBoundingClientRect().right -
              cells.at(-1)!.getBoundingClientRect().right
            );
          });
          expect(unused).toBeLessThanOrEqual(1);
        }
        if (process.env.EVIDENCE_DIR)
          await detail.screenshot({
            path: `${process.env.EVIDENCE_DIR}/${tab}-expanded-${theme}-${width}.png`,
          });
      }
    });
  }
