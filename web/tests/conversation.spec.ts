import { test, expect } from "@playwright/test";
import { conversationFixture } from "./conversation-fixture";
for (const agent of ["claude", "codex"])
  for (const theme of ["dark", "light"])
    for (const width of [1440, 1024, 768, 390])
      test(`${agent} conversation ${theme} ${width}: safe Markdown, compact layout and disclosure`, async ({
        page,
      }) => {
        await conversationFixture(page, agent);
        await page.setViewportSize({ width, height: 900 });
        await page.goto(
          "/?server=http://axe.test&view=overview&sort=oldest&tab=runs&listY=214&run=11",
        );
        await page.evaluate(
          (theme) => (document.documentElement.dataset.theme = theme),
          theme,
        );
        const dialog = page.getByRole("dialog", { name: "Run log" });
        await expect(
          dialog.getByRole("heading", { name: "Review the change" }),
        ).toBeVisible();
        await expect(
          dialog.getByRole("heading", { name: "Result", exact: true }),
        ).toBeVisible();
        await expect(
          dialog.locator('article[data-kind="message"]'),
        ).toHaveCount(2);
        await expect(
          dialog.locator('article[data-kind="message"] li'),
        ).toHaveCount(4);
        await expect(
          dialog.locator('article[data-kind="message"] pre code'),
        ).toHaveCount(2);
        await expect(
          dialog.getByText("opaque-signature", { exact: false }),
        ).toHaveCount(0);
        await expect(
          dialog.getByText("routine diagnostic metadata", { exact: false }),
        ).toHaveCount(0);
        await expect(
          dialog.getByText("Example actionable error: credentials expired", {
            exact: false,
          }),
        ).toBeVisible();
        expect(await page.evaluate(() => "injected" in window)).toBe(false);
        expect(await dialog.locator('a[href^="javascript:"]').count()).toBe(0);
        await expect(
          dialog.getByRole("link", { name: "documentation" }),
        ).toHaveAttribute("rel", "noopener noreferrer");
        await expect(
          dialog.getByText("tool_progress", { exact: true }),
        ).toHaveCount(0);
        await expect(dialog.locator('article[data-error="true"]')).toHaveCount(
          1,
        );
        const tool = dialog.locator('article[data-kind="tool"]');
        await expect(tool).toHaveCount(1);
        // Measure the settled dialog, after its 4px entry motion.
        await dialog.evaluate((el) =>
          Promise.all(el.getAnimations({ subtree: true }).map((a) => a.finished)),
        );
        const measures = await dialog.evaluate((el) => {
          const header = el.querySelector("header")!,
            scroll = el.querySelector('[aria-label="Log output"]')!;
          const tool = el.querySelector('article[data-kind="tool"]')!,
            message = el.querySelector('article[data-kind="message"]')!;
          const s = (e: Element) => getComputedStyle(e);
          return {
            headerPad: parseFloat(s(header).paddingTop),
            scrollPad: parseFloat(s(scroll).paddingLeft),
            toolHeight: tool.getBoundingClientRect().height,
            toolPad: parseFloat(s(tool).paddingTop),
            messagePad: parseFloat(s(message).paddingLeft),
            textSize: parseFloat(s(message.querySelector("p")!).fontSize),
            overflow: el.scrollWidth > window.innerWidth,
            controls: [...header.querySelectorAll("button")].map((b) => ({
              height: b.getBoundingClientRect().height,
              left: b.getBoundingClientRect().left,
              right: b.getBoundingClientRect().right,
            })),
          };
        });
        expect(measures.headerPad).toBeLessThanOrEqual(8);
        expect(measures.scrollPad).toBeLessThanOrEqual(16);
        expect(measures.toolHeight).toBeLessThanOrEqual(58);
        expect(measures.toolPad).toBeLessThanOrEqual(8);
        expect(measures.messagePad).toBeLessThanOrEqual(12);
        expect(measures.textSize).toBeGreaterThanOrEqual(13);
        expect(measures.overflow).toBe(false);
        for (const control of measures.controls) {
          expect(control.height).toBeGreaterThanOrEqual(24);
          expect(control.height).toBeLessThanOrEqual(32);
          expect(control.left).toBeGreaterThanOrEqual(0);
          expect(control.right).toBeLessThanOrEqual(width);
        }
        const summary = tool.locator("summary");
        await summary.focus();
        await expect(summary).toBeFocused();
        expect(
          await summary.evaluate((el) => getComputedStyle(el).outlineStyle),
        ).not.toBe("none");
        await page.keyboard.press("Enter");
        await expect(tool.locator("details")).toHaveAttribute("open", "");
        const output = tool.locator("pre").last();
        await expect(output).toContainText("Tests passed");
        // The disclosure reveals its measured content over 200ms; long output
        // scrolls inside the record once revealed.
        await expect
          .poll(() => output.evaluate((el) => el.scrollWidth > el.clientWidth))
          .toBe(true);
        expect(
          await output.evaluate((el) => el.clientHeight),
        ).toBeLessThanOrEqual(360);
        expect(
          await dialog.evaluate((el) => el.scrollWidth <= window.innerWidth),
        ).toBe(true);
        await summary.press("Enter");
        await dialog.locator('article[data-kind="thinking"] summary').click();
        await expect(
          dialog.getByText(
            "Check the recorded test results before summarizing.",
          ),
        ).toBeVisible();
        await dialog.locator('article[data-kind="thinking"] summary').click();
        await dialog
          .locator('article[data-kind="event"]')
          .last()
          .locator("summary")
          .click();
        await expect(
          dialog.getByText("Unknown event preserved", { exact: false }),
        ).toBeVisible();
        await dialog
          .locator('article[data-kind="event"]')
          .last()
          .locator("summary")
          .click();
        await dialog
          .getByLabel("Log output")
          .evaluate((el) => (el.scrollTop = 0));
        await page.screenshot({
          path: test.info().outputPath(`${agent}-${theme}-${width}-after.png`),
        });
        await dialog
          .getByRole("button", { name: "Inspect raw events" })
          .click();
        await dialog.locator("details").first().locator("summary").click();
        await expect(
          dialog.getByText("routine diagnostic metadata", { exact: false }),
        ).toBeVisible();
        if (agent === "claude") {
          await dialog.locator("details").nth(2).locator("summary").click();
          await expect(
            dialog.locator("details").nth(2).locator("pre"),
          ).toContainText('"heartbeat":true');
        }
        await dialog.getByRole("button", { name: "Show conversation" }).click();
        await expect(
          dialog.getByRole("heading", { name: "Result", exact: true }),
        ).toHaveCount(1);
        await dialog.getByRole("button", { name: "Back to dashboard" }).click();
        await expect(page).not.toHaveURL(/run=11/);
        await expect(page).toHaveURL(/sort=oldest/);
      });
