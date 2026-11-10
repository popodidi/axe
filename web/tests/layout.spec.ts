import { test, expect, type Page } from "@playwright/test";
import { mockDesk } from "./fixtures";
const base = "/?taskq=http://taskq.test";
const detail = '[aria-label="Task detail"]';
const list = '[aria-label="Task workspace"]';
const width = (page: Page, selector: string) =>
  page
    .locator(selector)
    .evaluate((el) => Math.round(el.getBoundingClientRect().width));
async function noPageOverflow(page: Page) {
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth - window.innerWidth,
    ),
  ).toBeLessThanOrEqual(0);
}
async function openDetail(page: Page, url = base + "&view=tasks&task=3") {
  await page.goto(url);
  await expect(page.locator(`${detail} h2`).first()).toBeVisible();
}
test("navigation collapses, stays usable, and persists across reload", async ({
  page,
}) => {
  await mockDesk(page);
  await openDetail(page);
  const nav = page.locator("#app-nav");
  const toggle = page.getByRole("button", { name: "Collapse navigation" });
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  const expanded = await width(page, "#app-nav");
  const listBefore = await width(page, list);
  await toggle.click();
  const expand = page.getByRole("button", { name: "Expand navigation" });
  await expect(expand).toHaveAttribute("aria-expanded", "false");
  await expect.poll(() => width(page, "#app-nav")).toBeLessThan(expanded);
  // The reclaimed width goes to the task list.
  expect(await width(page, list)).toBeGreaterThan(listBefore);
  // Views, refresh, and theme stay reachable by name in the rail.
  const tasks = nav.getByRole("button", { name: "Tasks", exact: true });
  await expect(tasks).toBeVisible();
  await expect(nav.getByRole("button", { name: "Refresh now" })).toBeVisible();
  await expect(nav.getByRole("button", { name: "Light theme" })).toBeVisible();
  await page.screenshot({
    path: test.info().outputPath("nav-collapsed-dark-1440.png"),
  });
  await page.reload();
  await expect(expand).toHaveAttribute("aria-expanded", "false");
  await page.getByRole("button", { name: "Stats", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Stats", exact: true }),
  ).toHaveAttribute("aria-current", "page");
  await expand.click();
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await expect.poll(() => width(page, "#app-nav")).toBe(expanded);
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
});
test("detail pane resizes by drag and keyboard within bounds and persists", async ({
  page,
}) => {
  await mockDesk(page);
  await openDetail(page);
  const handle = page.getByRole("separator", { name: "Resize detail pane" });
  await expect(handle).toHaveAttribute("aria-controls", "task-detail");
  const start = await width(page, detail);
  await expect(handle).toHaveAttribute("aria-valuenow", String(start));
  const min = Number(await handle.getAttribute("aria-valuemin"));
  const max = Number(await handle.getAttribute("aria-valuemax"));
  expect(min).toBe(360);
  expect(max).toBeGreaterThan(start);

  // Drag left by 200px widens detail by 200px.
  const box = (await handle.boundingBox())!;
  const y = box.y + box.height / 2;
  await page.mouse.move(box.x + box.width / 2, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 - 100, y, { steps: 5 });
  await page.mouse.move(box.x + box.width / 2 - 200, y, { steps: 5 });
  await page.mouse.up();
  await expect.poll(() => width(page, detail)).toBe(start + 200);
  await expect(handle).toHaveAttribute("aria-valuenow", String(start + 200));

  // Dragging far past either edge stops at the bounds.
  const now = (await handle.boundingBox())!;
  await page.mouse.move(now.x + now.width / 2, y);
  await page.mouse.down();
  await page.mouse.move(5, y, { steps: 5 });
  await page.mouse.up();
  await expect.poll(() => width(page, detail)).toBe(max);
  const listMin = await width(page, list);
  expect(listMin).toBeGreaterThanOrEqual(224);
  const atMax = (await handle.boundingBox())!;
  await page.mouse.move(atMax.x + atMax.width / 2, y);
  await page.mouse.down();
  await page.mouse.move(1435, y, { steps: 5 });
  await page.mouse.up();
  await expect.poll(() => width(page, detail)).toBe(min);

  // Keyboard: arrows step 16px (Shift 64px); Home/End jump to the bounds.
  await handle.focus();
  await page.keyboard.press("ArrowLeft");
  await expect.poll(() => width(page, detail)).toBe(min + 16);
  await page.keyboard.press("Shift+ArrowLeft");
  await expect.poll(() => width(page, detail)).toBe(min + 80);
  await page.keyboard.press("ArrowRight");
  await expect.poll(() => width(page, detail)).toBe(min + 64);
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Shift+ArrowRight");
  await expect.poll(() => width(page, detail)).toBe(min);
  await page.keyboard.press("End");
  await expect.poll(() => width(page, detail)).toBe(max);
  await page.keyboard.press("Home");
  await expect.poll(() => width(page, detail)).toBe(min);
  await page.keyboard.press("Shift+ArrowLeft");
  await expect(handle).toHaveAttribute("aria-valuenow", String(min + 64));
  // Resizing with the keyboard leaves Esc to close the task.
  await noPageOverflow(page);

  // Saved width applies on reload and to the next task.
  await page.reload();
  await expect.poll(() => width(page, detail)).toBe(min + 64);
  await page.locator(`${list} #task-2 button`).click();
  await expect(page.locator(`${detail} h2`).first()).toHaveText(
    "Approve rake backtest rollout",
  );
  expect(await width(page, detail)).toBe(min + 64);
  await page.keyboard.press("Escape");
  await expect(page.locator(detail)).toHaveCount(0);

  // Double-click restores the default 48% share.
  await page.locator(`${list} #task-2 button`).click();
  await handle.dblclick();
  const workspace = await width(page, `${list} >> xpath=..`);
  await expect
    .poll(() => width(page, detail))
    .toBe(Math.round(workspace * 0.48));
  await page.reload();
  await expect
    .poll(() => width(page, detail))
    .toBe(Math.round(workspace * 0.48));
});
test("a saved width stays within bounds when the viewport changes", async ({
  page,
}) => {
  await mockDesk(page);
  await openDetail(page);
  const handle = page.getByRole("separator", { name: "Resize detail pane" });
  await handle.focus();
  await page.keyboard.press("End");
  const wide = await width(page, detail);
  for (const [w, h] of [
    [1024, 768],
    [768, 1024],
    [761, 900],
  ]) {
    await page.setViewportSize({ width: w, height: h });
    // The rail (216px default, yielding to 22vw) and a 224px list minimum
    // leave the rest to detail.
    // Poll: the rail settles its width after the viewport change.
    await expect
      .poll(async () =>
        Number(await handle.getAttribute("aria-valuemax")) ===
        w - (await width(page, "#app-nav")) - 224,
      )
      .toBe(true);
    const max = w - (await width(page, "#app-nav")) - 224;
    await expect.poll(() => width(page, detail)).toBe(max);
    expect(await width(page, list)).toBeGreaterThanOrEqual(224);
    await noPageOverflow(page);
    await expect(page.getByRole("button", { name: /Back/ })).toBeInViewport();
  }
  // Phone: detail takes the full width with Back to the list; no resizer.
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(handle).toBeHidden();
  await expect.poll(() => width(page, detail)).toBe(390);
  await expect(page.locator(list)).toBeHidden();
  await noPageOverflow(page);
  await page.getByRole("button", { name: /Back/ }).click();
  await expect(page.locator(list)).toBeVisible();
  await noPageOverflow(page);
  // Back to desktop: the saved (clamped) width returns.
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.locator(`${list} #task-3 button`).click();
  await expect.poll(() => width(page, detail)).toBe(wide);
});
test("collapsed navigation is ignored on phones and returns on wide screens", async ({
  page,
}) => {
  await page.addInitScript(() =>
    localStorage.setItem("harvis-layout", JSON.stringify({ nav: "collapsed" })),
  );
  await mockDesk(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await openDetail(page);
  // Phone top bar: view links visible by label, no collapse control.
  await expect(
    page.getByRole("button", { name: "Tasks", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "Expand navigation" }),
  ).toBeHidden();
  await expect(
    page.locator("#app-nav").getByText("Tasks", { exact: true }),
  ).toBeVisible();
  await noPageOverflow(page);
  await page.setViewportSize({ width: 1024, height: 768 });
  await expect(
    page.getByRole("button", { name: "Expand navigation" }),
  ).toBeVisible();
  await expect.poll(() => width(page, "#app-nav")).toBe(48);
  await noPageOverflow(page);
});
for (const [w, h] of [
  [1440, 900],
  [1024, 768],
  [768, 1024],
  [390, 844],
] as const)
  for (const theme of ["dark", "light"] as const)
    test(`layout at ${w}x${h} (${theme}) has no page overflow`, async ({
      page,
    }) => {
      if (theme === "light")
        await page.addInitScript(() =>
          localStorage.setItem("harvis-theme", "light"),
        );
      await page.setViewportSize({ width: w, height: h });
      await mockDesk(page);
      await page.goto(base + "&view=tasks");
      await expect(page.locator("#task-3")).toBeVisible();
      await noPageOverflow(page);
      await page.screenshot({
        path: test.info().outputPath(`tasks-${theme}-${w}.png`),
      });
      await page.locator("#task-3 button").click();
      await expect(page.locator(`${detail} h2`).first()).toBeVisible();
      await noPageOverflow(page);
      // Detail scrolls inside its own pane.
      await expect(page.locator(detail)).toHaveCSS("overflow-y", "auto");
      await page.screenshot({
        path: test.info().outputPath(`detail-${theme}-${w}.png`),
      });
      if (w > 760) {
        await page.getByRole("button", { name: "Collapse navigation" }).click();
        await noPageOverflow(page);
        await page.screenshot({
          path: test.info().outputPath(`detail-collapsed-${theme}-${w}.png`),
        });
      }
      await page.keyboard.press("Escape");
      await expect(page.locator(detail)).toHaveCount(0);
      await expect(page.locator("#task-3")).toBeVisible();
    });
