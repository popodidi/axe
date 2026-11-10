import { test, expect, type Page } from "@playwright/test";
import { mockDesk, tasks } from "./fixtures";
// Task states are restrained tinted labels (owner override, accent-tokens.css):
// pending purple, running blue, held/dependency wait amber, escalated danger,
// done teal. Each keeps a decorative symbol plus readable text.
const base = "/?taskq=http://taskq.test";
const list = [
  ...tasks,
  { ...tasks[3], id: 30, title: "Stored blocked hold", state: "blocked" },
  {
    ...tasks[3],
    id: 31,
    title: "Dependency wait",
    state: "pending",
    status: "blocked",
    waiting_on: [3],
  },
];
const expected: Record<string, [string, string]> = {
  pending: ["--accent-purple", "--accent-purple-bg"],
  in_progress: ["--accent-blue", "--accent-blue-bg"],
  blocked: ["--accent-amber", "--accent-amber-bg"],
  escalated: ["--status-danger", "--status-danger-bg"],
  done: ["--accent-teal", "--accent-teal-bg"],
};
// Resolve tokens through the browser so they compare as computed rgb().
const resolve = (page: Page, fg: string, bg: string) =>
  page.evaluate(
    ([fg, bg]) => {
      const el = document.createElement("span");
      el.style.color = `var(${fg})`;
      el.style.background = `var(${bg}, transparent)`;
      document.body.append(el);
      const s = getComputedStyle(el);
      const out = [s.color, s.backgroundColor];
      el.remove();
      return out;
    },
    [fg, bg],
  );
for (const theme of ["dark", "light"])
  for (const [width, height] of [
    [1440, 900],
    [390, 844],
  ] as const)
    test(`${theme} ${width}: states are tinted labels with symbols, escalated danger`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height });
      await page.addInitScript(
        (t) => localStorage.setItem("harvis-theme", t),
        theme,
      );
      await mockDesk(page, list);
      await page.goto(`${base}&view=tasks&state=all`);
      const pill = (title: string) =>
        page
          .locator('section[aria-label="Task workspace"] tbody tr', {
            hasText: title,
          })
          .locator("[data-state]");
      for (const [title, state] of [
        ["Stored blocked hold", "blocked"],
        ["Dependency wait", "blocked"],
        ["Rotate hlab registry certificate", "escalated"],
        ["Verify Lark reply ledger fix", "pending"],
        ["Make dashboard Overview a desk page", "in_progress"],
        ["Archive amux experiments", "done"],
      ] as const) {
        await expect(pill(title)).toHaveAttribute("data-state", state);
        const [fg, bg] = await resolve(page, ...expected[state]);
        await expect(pill(title)).toHaveCSS("color", fg);
        await expect(pill(title)).toHaveCSS("background-color", bg);
        // Decorative symbol, hidden from assistive technology.
        await expect(pill(title).locator("svg")).toHaveAttribute(
          "aria-hidden",
          "true",
        );
      }
      const [red] = await resolve(page, ...expected.escalated);
      await expect(pill("Stored blocked hold")).not.toHaveCSS("color", red);
      await page.screenshot({
        path: test.info().outputPath(`status-${theme}-${width}.png`),
        fullPage: true,
      });
    });
