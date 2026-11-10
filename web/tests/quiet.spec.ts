import { test, expect, type Page } from "@playwright/test";
import { mockDesk } from "./fixtures";
// Interaction contract: pane focus
// return, inert exit, newest-action-wins, restored geometry without motion,
// copy feedback, and reduced motion.
const base = "/?server=http://axe.test&view=tasks&state=all";
const detail = "#task-detail";
const rowButton = (page: Page, id: number) =>
  page.locator(`#task-${id} button`);

test("keyboard opening focuses the detail heading; Esc returns to the row", async ({
  page,
}) => {
  await mockDesk(page);
  await page.goto(base);
  await rowButton(page, 3).focus();
  await page.keyboard.press("Enter");
  const heading = page.locator(`${detail} h2[tabindex="-1"]`);
  await expect(heading).toHaveText("Make dashboard Overview a desk page");
  await expect(heading).toBeFocused();
  // Programmatic focus target, not an extra tab stop.
  await expect(heading).toHaveAttribute("tabindex", "-1");
  await page.keyboard.press("Escape");
  await expect(rowButton(page, 3)).toBeFocused();
  // The exiting pane is inert while it leaves, then unmounts.
  await expect(page.locator(detail)).toHaveCount(0);
  expect(new URL(page.url()).searchParams.has("task")).toBe(false);
});

test("pointer opening keeps row focus and shows selection separately", async ({
  page,
}) => {
  await mockDesk(page);
  await page.goto(base);
  await rowButton(page, 4).click();
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeVisible();
  await expect(rowButton(page, 4)).toBeFocused();
  await expect(page.locator("#task-4")).toHaveAttribute("aria-selected", "true");
  // Selection: neutral fill and 600 title weight; focus: its own ring.
  await expect(rowButton(page, 4)).toHaveCSS("font-weight", "600");
  await expect(page.locator("#task-4")).toHaveCSS("outline-style", "solid");
  // Newest first: j moves focus to #3 while #4 stays selected.
  await page.keyboard.press("j");
  await expect(page.locator("#task-3")).toHaveAttribute("data-focused", "true");
  await expect(page.locator("#task-3")).toHaveAttribute("aria-selected", "false");
  await expect(page.locator("#task-4")).toHaveAttribute("aria-selected", "true");
  await expect(page.locator("#task-4")).not.toHaveCSS("outline-style", "solid");
});

test("closing content is inert and unreachable by Tab during its exit", async ({
  page,
}) => {
  await page.clock.install();
  await mockDesk(page);
  await page.goto(base);
  await rowButton(page, 3).click();
  const pane = page.locator(detail);
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeVisible();
  await expect(pane).not.toHaveAttribute("data-phase", /.+/);
  await page.clock.pauseAt(new Date());
  await page.getByRole("button", { name: /^Back/ }).click();
  // Freeze the exit timer so the inert assertion cannot miss its 275ms window.
  await expect(pane).toHaveAttribute("inert", "");
  await expect(rowButton(page, 3)).toBeFocused();
  await page.keyboard.press("Tab");
  expect(await pane.evaluate((el) => el.contains(document.activeElement))).toBe(false);
  await page.clock.runFor(300);
  await expect(pane).toHaveCount(0);
});

test("a queued opening frame cannot reopen a closing pane", async ({ page }) => {
  await mockDesk(page);
  await page.goto(base);
  await expect(page.locator("html")).toHaveAttribute("data-motion", "ready");
  // Hold the opening frame to reproduce it arriving after the close commit.
  await page.evaluate(() => {
    const raf = window.requestAnimationFrame;
    window.requestAnimationFrame = (callback) => raf((time) => {
      if (document.getElementById("task-detail")?.dataset.phase === "enter") {
        window.requestAnimationFrame = raf;
        Object.assign(window, { releaseOpeningFrame: () => callback(time) });
      } else callback(time);
    });
  });
  await rowButton(page, 3).click();
  await expect.poll(() => page.evaluate(() => "releaseOpeningFrame" in window)).toBe(true);
  await page.clock.install();
  await page.clock.pauseAt(new Date());
  await page.keyboard.press("Escape");
  const pane = page.locator(detail);
  await expect(pane).toHaveAttribute("data-phase", "closing");
  await page.evaluate(() => {
    (window as unknown as { releaseOpeningFrame: () => void }).releaseOpeningFrame();
  });
  await expect(pane).toHaveAttribute("inert", "");
  await page.clock.runFor(300);
  await expect(pane).toHaveCount(0);
});

test("rapid open, close and reselect: the newest action wins", async ({
  page,
}) => {
  await mockDesk(page);
  await page.goto(base);
  await rowButton(page, 3).click();
  await page.keyboard.press("Escape");
  await rowButton(page, 4).click();
  await page.keyboard.press("Escape");
  await rowButton(page, 5).click();
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toHaveText(
    "Ship tundra-desk weekly digest",
  );
  await expect(page.locator(detail)).toHaveCount(1);
  // The transition settles; no stale exit unmounts the new content.
  await expect(page.locator(detail)).not.toHaveAttribute("data-phase", /.+/);
  await expect(page.locator(detail)).not.toHaveAttribute("inert", "");
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toHaveText(
    "Ship tundra-desk weekly digest",
  );
  // Switching tasks replaces content without closing and reopening the pane.
  await rowButton(page, 7).click();
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toHaveText(
    "Triage summit-desk intake",
  );
  await expect(page.locator(detail)).not.toHaveAttribute("data-phase", /.+/);
});

test("restored geometry paints without animation; Back/Forward is immediate", async ({
  page,
}) => {
  await mockDesk(page);
  await page.addInitScript(() =>
    localStorage.setItem(
      "axe-layout",
      JSON.stringify({ nav: "collapsed", detail: 500 }),
    ),
  );
  await page.goto(base + "&task=3");
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeVisible();
  const restored = await page.evaluate(() => ({
    phase: document.getElementById("task-detail")!.dataset.phase ?? null,
    nav: document.getElementById("app-nav")!.getBoundingClientRect().width,
    running: document
      .getAnimations()
      .filter((a) => a.playState === "running").length,
  }));
  expect(restored).toEqual({ phase: null, nav: 48, running: 0 });
  await page.keyboard.press("Escape");
  await expect(page.locator(detail)).toHaveCount(0);
  await page.goBack();
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeVisible();
  expect(
    await page.locator(detail).evaluate((el) => el.dataset.phase ?? null),
  ).toBeNull();
  await page.goForward();
  await expect(page.locator(detail)).toHaveCount(0);
});

test("navigation collapse animates width only after hydration", async ({
  page,
}) => {
  await mockDesk(page);
  await page.goto(base);
  await expect(page.locator("html")).toHaveAttribute("data-motion", "ready");
  const nav = page.locator("#app-nav");
  // At rest (and on viewport changes) the rail has no width transition.
  await expect(nav).not.toHaveCSS("transition-property", "width");
  const toggle = page.getByRole("button", { name: "Collapse navigation" });
  await toggle.click();
  await expect(nav).toHaveCSS("transition-property", "width");
  await expect(
    page.getByRole("button", { name: "Expand navigation" }),
  ).toBeFocused();
  await expect
    .poll(() => nav.evaluate((el) => el.getBoundingClientRect().width))
    .toBe(48);
  // Rapid toggles reverse from the current width and settle on the last one.
  await page.getByRole("button", { name: "Expand navigation" }).click();
  await page.getByRole("button", { name: "Collapse navigation" }).click();
  await page.getByRole("button", { name: "Expand navigation" }).click();
  await expect
    .poll(() => nav.evaluate((el) => el.getBoundingClientRect().width))
    .toBe(168);
});

test("resizing by drag follows the pointer with no width transition", async ({
  page,
}) => {
  await mockDesk(page);
  await page.goto(base + "&task=3");
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeVisible();
  const pane = page.locator(detail);
  const handle = page.getByRole("separator", { name: "Resize detail pane" });
  const box = (await handle.boundingBox())!;
  const before = (await pane.boundingBox())!.width;
  await page.mouse.move(box.x + 2, box.y + 200);
  await page.mouse.down();
  await page.mouse.move(box.x - 98, box.y + 200);
  // Same frame: no easing or lag while dragging.
  expect((await pane.boundingBox())!.width).toBeCloseTo(before + 100, -1);
  expect(await pane.evaluate((el) => el.dataset.phase ?? null)).toBeNull();
  await page.mouse.up();
});

test("Copy confirms only after clipboard success and keeps its width", async ({
  page,
  context,
}) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await mockDesk(page);
  await page.goto(base + "&task=3");
  const copy = page.getByRole("button", { name: "Copy task log" });
  await expect(copy).toBeVisible();
  const width = (await copy.boundingBox())!.width;
  await copy.click();
  await expect(copy.getByText("Copied")).toHaveAttribute("data-hidden", "false");
  expect((await copy.boundingBox())!.width).toBe(width);
  await expect(page.locator('[aria-live="polite"]')).toHaveText(
    "Copied command",
  );
  expect(await page.evaluate(() => navigator.clipboard.readText())).toContain(
    "axe log 3",
  );
  // Label resets after 1.5s.
  await expect(copy.getByText("Copied")).toHaveAttribute("data-hidden", "true", {
    timeout: 3000,
  });
});

test("a failed copy stays actionable and says so", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(window, "isSecureContext", { value: false });
    document.execCommand = () => false;
  });
  await mockDesk(page);
  await page.goto(base + "&task=3");
  const copy = page.getByRole("button", { name: "Copy task log" });
  await copy.click();
  const toast = page.locator('[aria-live="polite"]');
  await expect(toast).toHaveText("Copy failed; select the command text");
  await expect(toast).toBeVisible();
  await expect(copy.getByText("Copied")).toHaveAttribute("data-hidden", "true");
});

test("reduced motion: every workflow completes with immediate geometry", async ({
  page,
}) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await mockDesk(page);
  await page.goto(base);
  expect(
    await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--duration-layout")
        .trim(),
    ),
  ).toBe("0ms");
  await rowButton(page, 3).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeFocused();
  await expect(page.locator(detail)).not.toHaveAttribute("data-phase", /.+/);
  await page.getByRole("tab", { name: "events" }).click();
  await expect(page.getByRole("tab", { name: "events" })).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(page.locator(detail)).toHaveCount(0);
  await expect(rowButton(page, 3)).toBeFocused();
  await page.getByRole("button", { name: "Collapse navigation" }).click();
  expect(
    await page
      .locator("#app-nav")
      .evaluate((el) => el.getBoundingClientRect().width),
  ).toBe(48);
});

test("phone: detail replaces the list and Back restores the invoking row", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await mockDesk(page);
  await page.goto(base);
  await rowButton(page, 4).focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(`${detail} h2[tabindex="-1"]`)).toBeFocused();
  await expect(page.locator('section[aria-label="Task workspace"]')).toBeHidden();
  await page.getByRole("button", { name: /^Back/ }).click();
  await expect(rowButton(page, 4)).toBeFocused();
});

test("empty filters explain and offer recovery", async ({ page }) => {
  await mockDesk(page);
  await page.goto(base + "&q=no-such-task");
  await expect(page.getByText("No tasks match these filters.")).toBeVisible();
  await page.getByRole("button", { name: "Clear filters" }).click();
  await expect(page.locator("tbody tr")).toHaveCount(8);
});
