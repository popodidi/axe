import { test, expect, type Page } from "@playwright/test";
import { choose, mockDesk, runs } from "./fixtures";
// Compact layout contract (docs/design/dashboard.md "Compact defaults"). These assert rendered
// geometry, not token use: gutters, control sizes, filter wrapping, and the
// Watch entry staying inside the run card's activity row.
const base = "/?server=http://axe.test";
const listRows = 'section[aria-label="Task workspace"] tbody tr';
const widths = [
  [1440, 900],
  [1024, 768],
  [768, 1024],
  [390, 844],
] as const;
async function theme(page: Page, name: string) {
  await page.addInitScript(
    (t) => localStorage.setItem("axe-theme", t),
    name,
  );
}
type Box = { x: number; y: number; width: number; height: number };
const overlaps = (a: Box, b: Box) =>
  a.x < b.x + b.width - 1 &&
  b.x < a.x + a.width - 1 &&
  a.y < b.y + b.height - 1 &&
  b.y < a.y + a.height - 1;
// Every filter caption and selected value fits; no control overlaps another
// or leaves the list pane; the page never scrolls sideways.
async function expectReadableFilters(page: Page, count: number) {
  const m = await page.evaluate(() => {
    const pane = document.querySelector(
      'section[aria-label="Task workspace"]',
    )!;
    const box = (el: Element) => {
      const r = el.getBoundingClientRect();
      return { x: r.x, y: r.y, width: r.width, height: r.height };
    };
    return {
      pane: box(pane),
      overflow: document.documentElement.scrollWidth - window.innerWidth,
      search: pane.querySelector("input[type=search]")
        ? box(pane.querySelector("input[type=search]")!)
        : null,
      // Styled selects: caption above a combobox trigger showing the value.
      filters: Array.from(pane.querySelectorAll("[data-select]")).map(
        (field) => {
          const caption = field.querySelector("[data-caption]")!;
          const trigger = field.querySelector("[role=combobox]")!;
          const value = trigger.querySelector("span")!;
          return {
            box: box(field),
            caption: caption.textContent,
            captionClipped: caption.scrollWidth > caption.clientWidth,
            captionWidth: caption.getBoundingClientRect().width,
            select: box(trigger),
            valueClipped: value.scrollWidth > value.clientWidth + 1,
          };
        },
      ),
    };
  });
  expect(m.overflow).toBeLessThanOrEqual(0);
  expect(m.filters).toHaveLength(count);
  const boxes = [...(m.search ? [m.search] : []), ...m.filters.map((f) => f.box)];
  for (const [i, f] of m.filters.entries()) {
    expect(f.captionClipped, `${f.caption} caption`).toBe(false);
    expect(f.captionWidth).toBeGreaterThan(0);
    expect(f.valueClipped, `${f.caption} value`).toBe(false);
    expect(f.select.height).toBeGreaterThanOrEqual(24);
    expect(f.box.x).toBeGreaterThanOrEqual(m.pane.x);
    expect(f.box.x + f.box.width).toBeLessThanOrEqual(
      m.pane.x + m.pane.width,
    );
    // Every control placed before this one (search first, when present).
    for (const other of boxes.slice(0, i + (m.search ? 1 : 0)))
      expect(overlaps(f.box, other), `${f.caption} overlaps`).toBe(false);
  }
}
for (const name of ["dark", "light"])
  for (const [width, height] of widths)
    test(`${name} ${width}: filters stay labelled and readable`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height });
      await theme(page, name);
      await mockDesk(page);
      // Desktop widths keep the detail pane open beside the list.
      const detail = width > 760 ? "&task=3" : "";
      await page.goto(`${base}&view=tasks&project=atlas&state=all${detail}`);
      await expect(page.locator(listRows)).toHaveCount(4);
      if (detail)
        await expect(
          page.getByRole("heading", { name: "Outcome" }),
        ).toBeVisible();
      await expectReadableFilters(page, 3);
      await page.goto(`${base}&project=atlas&view=stats`);
      // The report has its own filters (project, period, task type, group
      // by), no search.
      await expect(page.getByLabel("Search tasks")).toHaveCount(0);
      await expect(page.getByLabel("Date range")).toBeVisible();
      await expectReadableFilters(page, 4);
    });
test("filters work from the keyboard at phone width and reset to All", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await mockDesk(page);
  await page.goto(`${base}&view=tasks&state=all`);
  await expect(page.locator(listRows)).toHaveCount(8);
  await page.getByLabel("Search tasks").focus();
  await page.keyboard.press("Tab");
  const project = page.getByLabel("project", { exact: true });
  await expect(project).toBeFocused();
  await expect(project).toHaveCSS("outline-style", "solid");
  // The visible caption labels the select.
  await expect(
    page.locator("[data-caption]", { hasText: "project" }),
  ).toBeVisible();
  for (const [label, value, rows] of [
    ["project", "atlas", 4],
    ["state", "blocked", 0],
    ["state", "escalated", 1],
  ] as const) {
    await choose(page.getByLabel(label, { exact: true }), value);
    await expect(page).toHaveURL(new RegExp(`${label}=${value}`));
    await expect(page.locator(listRows)).toHaveCount(rows);
    await expect(page.getByLabel(label, { exact: true })).toHaveAttribute("data-value", value);
  }
  await choose(page.getByLabel("Sort", { exact: true }), "oldest");
  await expect(page).toHaveURL(/sort=oldest/);
  await expectReadableFilters(page, 3);
  for (const label of ["project", "state"]) {
    await choose(page.getByLabel(label, { exact: true }), "");
    if (label === "project") await expect(page).not.toHaveURL(/project=/);
    else await expect(page).toHaveURL(/state=all/);
  }
  await expect(page.getByLabel("project", { exact: true })).toHaveAttribute("data-value", "");
  await expect(page.locator(listRows)).toHaveCount(8);
});
test("panes, panels, and inset surfaces keep compact gutters", async ({
  page,
}) => {
  await mockDesk(page);
  for (const [width, height] of widths.slice(0, 2)) {
    await page.setViewportSize({ width, height });
    await page.goto(`${base}&view=tasks&task=3`);
    await expect(page.getByRole("heading", { name: "Outcome" })).toBeVisible();
    const m = await page.evaluate(() => {
      const pad = (el: Element) => {
        const cs = getComputedStyle(el);
        return [cs.paddingTop, cs.paddingRight, cs.paddingLeft].map(
          parseFloat,
        );
      };
      const list = document.querySelector(
        'section[aria-label="Task workspace"]',
      )!;
      const panel = list.querySelector("table")!.closest("section")!;
      const detail = document.querySelector('[class*="detailPane"]')!;
      const spec = detail.querySelector('article[aria-label="Spec"]')!;
      const box = (el: Element) => {
        const cs = getComputedStyle(el);
        return [cs.borderTopWidth, cs.backgroundColor, cs.boxShadow];
      };
      return {
        list: pad(list),
        panel: pad(panel),
        detail: pad(detail),
        spec: pad(spec),
        // Quiet: borderless sections, no card around the spec or the list.
        panelBox: box(panel),
        specBox: box(spec),
        detailBorder: getComputedStyle(detail).borderLeftWidth,
        // Table fills its panel minus the compact padding and border.
        slack:
          panel.getBoundingClientRect().width -
          list.querySelector("table")!.getBoundingClientRect().width,
      };
    });
    expect(m.list).toEqual([12, 16, 16]);
    expect(m.detail).toEqual([12, 16, 16]);
    // Owner readability override: sections are bordered neutral cards (16px
    // padding, no shadow); the spec stays on the detail surface, no nested card.
    expect(m.panel).toEqual([16, 16, 16]);
    expect(m.spec).toEqual([0, 0, 0]);
    expect(m.panelBox[0]).toBe("1px");
    expect(m.panelBox[2]).toBe("none");
    expect(m.specBox).toEqual(["0px", "rgba(0, 0, 0, 0)", "none"]);
    expect(m.detailBorder).toBe("1px");
    // The table spans its card minus padding, border and focus allowance.
    expect(m.slack).toBeLessThanOrEqual(36);
  }
});
// The compact contract holds across the collapsible nav and resizable detail
// pane: at the narrowest list (detail at its maximum) filters still wrap
// readably and gutters stay compact.
for (const name of ["dark", "light"])
  for (const [width, height] of widths.slice(0, 2))
    test(`${name} ${width}: compact filters survive nav collapse and detail resize`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height });
      await theme(page, name);
      await mockDesk(page);
      await page.goto(`${base}&view=tasks&project=atlas&state=all&task=3`);
      await expect(page.getByRole("heading", { name: "Outcome" })).toBeVisible();
      const pad = () =>
        page
          .locator('[aria-label="Task workspace"]')
          .evaluate((el) => getComputedStyle(el).padding);
      const handle = page.getByRole("separator", {
        name: "Resize detail pane",
      });
      for (const nav of ["Collapse navigation", "Expand navigation"]) {
        await page.getByRole("button", { name: nav }).click();
        // Resizer bounds change throughout the navigation width transition.
        await expect.poll(() => page.locator("#app-nav").evaluate((el) =>
          el.getAnimations().filter((a) => a.playState === "running").length,
        )).toBe(0);
        for (const key of ["End", "Home"]) {
          await handle.focus();
          await page.keyboard.press(key);
          await expect(handle).toHaveAttribute(
            "aria-valuenow",
            (await handle.getAttribute(
              key === "End" ? "aria-valuemax" : "aria-valuemin",
            ))!,
          );
          expect(await pad()).toBe("12px 16px 16px");
          await expectReadableFilters(page, 3);
        }
      }
    });
async function logFixture(page: Page) {
  await mockDesk(page);
  const text = (i: number) =>
    JSON.stringify({
      type: "assistant",
      message: { content: [{ type: "text", text: `Message ${i}` }] },
    });
  const lines = Array.from({ length: 6 }, (_, i) => ({
    start: i * 100,
    end: i * 100 + 100,
    text: text(i),
    fragment: false,
  }));
  await page.route("http://axe.test/api/runs/*/log?*", (route) =>
    route.fulfill({
      json: {
        run: runs[0],
        page: { generation: "g", start: 0, end: 600, size: 600, lines },
      },
    }),
  );
}
for (const name of ["dark", "light"])
  for (const [width, height] of widths)
    test(`${name} ${width}: Watch stays compact on run cards and the viewer`, async ({
      page,
    }) => {
      await page.setViewportSize({ width, height });
      await theme(page, name);
      await logFixture(page);
      await page.goto(base + "&task=3&tab=runs");
      const card = page.locator("article[data-live]").first();
      await expect(card).toBeVisible();
      const m = await card.evaluate((c) => {
        const watch = Array.from(c.querySelectorAll("button")).find(
          (b) => b.textContent === "Watch",
        )!;
        const activity = c.querySelector("[class*=summary]")!;
        const r = watch.getBoundingClientRect();
        const a = activity.getBoundingClientRect();
        const cs = getComputedStyle(c);
        return {
          watch: [r.width, r.height],
          inActivityRow: activity.contains(watch) && r.top >= a.top - 1,
          rowHeight: a.height,
          padding: [cs.paddingTop, cs.paddingLeft].map(parseFloat),
          footButtons: Array.from(
            c.lastElementChild!.querySelectorAll("button"),
          ).map((b) => b.getAttribute("aria-label") || b.textContent),
        };
      });
      // Usable target (WCAG 2.5.8 24px) but no default 34px button row.
      expect(m.watch[1]).toBeGreaterThanOrEqual(24);
      expect(m.watch[1]).toBeLessThanOrEqual(28);
      expect(m.watch[0]).toBeGreaterThanOrEqual(24);
      expect(m.inActivityRow).toBe(true);
      expect(m.rowHeight).toBeLessThanOrEqual(48);
      // Runs are list items on the canvas: vertical rhythm, no card inset.
      expect(m.padding).toEqual([8, 0]);
      // Terminal commands stay available; only their Copy buttons remain.
      // (Copy/Copied share one stable-width cell, so check accessible names.)
      expect(m.footButtons.every((t) => t?.startsWith("Copy "))).toBe(true);
      await expect(card.getByText("watch", { exact: true })).toBeHidden();
      await card.getByRole("button", { name: "Run #11 details" }).click();
      await expect(card.getByText("watch", { exact: true })).toBeVisible();

      await card.getByRole("button", { name: "Watch", exact: true }).click();
      const dialog = page.getByRole("dialog", { name: "Run log" });
      await expect(dialog.getByText("Message 5")).toBeVisible();
      // Measure the settled dialog, after its 4px entry motion.
      await dialog.evaluate((el) =>
        Promise.all(el.getAnimations({ subtree: true }).map((a) => a.finished)),
      );
      const v = await dialog.evaluate((d) => {
        const header = d.querySelector("header")!;
        const records = Array.from(d.querySelectorAll("article"));
        const hs = getComputedStyle(header);
        const rs = getComputedStyle(records[1]);
        return {
          header: [hs.paddingTop, hs.paddingLeft].map(parseFloat),
          buttons: Array.from(d.querySelectorAll("button")).map(
            (b) => b.getBoundingClientRect().height,
          ),
          record: [rs.paddingTop, rs.paddingLeft, rs.marginTop].map(
            parseFloat,
          ),
          gap:
            records[1].getBoundingClientRect().top -
            records[0].getBoundingClientRect().bottom,
          overflow: document.documentElement.scrollWidth - window.innerWidth,
        };
      });
      expect(v.header).toEqual([8, 12]);
      for (const h of v.buttons) {
        expect(h).toBeGreaterThanOrEqual(24);
        expect(h).toBeLessThanOrEqual(28);
      }
      expect(v.record).toEqual([8, 12, 8]);
      expect(v.gap).toBeLessThanOrEqual(8);
      expect(v.overflow).toBeLessThanOrEqual(0);
    });
