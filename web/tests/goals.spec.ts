import { test, expect } from "@playwright/test";
import { mockGoals } from "./goals-fixture";
import { choose } from "./fixtures";
const base = "/?server=http://axe.test";
for (const theme of ["dark", "light"]) for (const width of [1440, 390]) {
  test(`goals list ${theme} ${width}`, async ({ page }, info) => {
    await page.setViewportSize({ width, height: 1000 });
    await page.addInitScript((t) => localStorage.setItem("axe-theme", t), theme);
    const reads = await mockGoals(page);
    await page.goto(base);
    await page.getByRole("button", { name: "Goals", exact: true }).click();
    await expect(page).toHaveURL(/view=goals/);
    const list = page.getByRole("region", { name: "Goals", exact: true });
    await expect(list.getByRole("alert")).toHaveCount(0);
    const ci = list.locator("#goal-faster-ci");
    await expect(ci).toContainText("faster-ci");
    await expect(ci).toContainText("atlas");
    await expect(ci).toContainText("Active");
    await expect(ci).toContainText("$7.50/$20.00");
    await expect(list.getByRole("meter", { name: "faster-ci cost this week" })).toHaveAttribute(
      "aria-valuenow",
      "38",
    );
    // Over budget: the bar is full and marked.
    const digest = list.locator("#goal-weekly-digest");
    await expect(digest).toContainText("Proposed");
    await expect(digest).toContainText("$3.00/$2.00");
    await expect(digest.locator("[data-over=true]")).toHaveCount(1);
    for (const [name, state] of [
      ["lark-replies", "Paused"],
      ["registry-certs", "Done"],
      ["old-backtests", "Dropped"],
    ])
      await expect(list.locator(`#goal-${name}`)).toContainText(state);
    await expect(list.locator("#goal-lark-replies")).toContainText("$0.00 · no limit");
    if (width > 760) {
      await expect(ci).toContainText("ops-bot, watch-bot");
      await expect(ci).toContainText("1/4");
      await expect(ci).toContainText("2026-12-31");
      await expect(list.getByRole("meter", { name: "faster-ci attention this week" })).toHaveAttribute(
        "aria-valuenow",
        "25",
      );
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: info.outputPath(`goals-list-${theme}-${width}.png`), fullPage: true });
    // Search and project filter apply to goals.
    await page.getByLabel("Search goals").fill("digest");
    await expect(ci).toHaveCount(0);
    await expect(digest).toBeVisible();
    await page.getByLabel("Search goals").fill("");
    await choose(page.getByLabel("project", { exact: true }), "orbit");
    await expect(ci).toHaveCount(0);
    await expect(list.locator("#goal-old-backtests")).toBeVisible();
    expect(reads.every((r) => r.startsWith("GET "))).toBe(true);
  });
}
test("goal detail: intent, trajectory, linked tasks and next wake", async ({ page }, info) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  const reads = await mockGoals(page);
  await page.goto(base + "&view=goals");
  await page.getByRole("button", { name: "faster-ci", exact: true }).click();
  await expect(page).toHaveURL(/goal=faster-ci/);
  const detail = page.getByRole("region", { name: "Goal detail" });
  await expect(detail.getByRole("heading", { name: "faster-ci" })).toBeVisible();
  await expect(detail).toContainText("Active");
  await expect(detail).toContainText("ops-bot, watch-bot");
  await expect(detail).toContainText("$7.50/$20.00");
  await expect(detail).toContainText("1/4");
  await expect(detail).toContainText("2026-12-31");
  await expect(detail).toContainText("tasks $5.25 · bot ops-bot $2.25");
  await expect(detail.locator("dd", { hasText: "None scheduled" })).toHaveCount(0);
  await expect(detail.getByText("Next goal wake")).toBeVisible();
  // Intent renders as Markdown.
  const intent = detail.getByRole("region", { name: "Intent" });
  await expect(intent.locator("strong")).toHaveText("PR CI");
  // Every linked task: open ones first, then done ones newest first, each
  // with its state and title.
  const open = detail.getByRole("region", { name: "Linked tasks" });
  await expect(open).toContainText("2 open · 4 linked");
  await expect(open.locator("tbody tr")).toHaveCount(4);
  await expect(
    open.locator("tbody tr").evaluateAll((rows) => rows.map((r) => r.id)),
  ).resolves.toEqual(["goal-task-3", "goal-task-4", "goal-task-8", "goal-task-2"]);
  await expect(open.locator("#goal-task-3")).toContainText("Make dashboard Overview a desk page");
  await expect(open.locator("#goal-task-4")).toBeVisible();
  await expect(open.locator("#goal-task-8")).toContainText("done");
  await expect(open.locator("#goal-task-8")).toContainText("Close out shipped dashboard rebuild");
  await expect(open.locator("#goal-task-2")).toContainText("Cache the dependency build");
  // The full trajectory, oldest first, read from the events route.
  const traj = detail.getByRole("region", { name: "Trajectory" });
  await expect(traj.locator("li")).toHaveCount(4);
  await expect(traj.locator("li").evaluateAll((li) => li.map((e) => e.dataset.kind))).resolves.toEqual([
    "hypothesis",
    "test",
    "observation",
    "verdict",
  ]);
  await expect(traj.locator("li").first()).toContainText("Caching the dependency build");
  expect(reads).toContain("GET /api/goals/faster-ci/events?trajectory=1&limit=1000");
  await page.screenshot({ path: info.outputPath("goal-detail.png"), fullPage: true });
  // A linked task id opens that task in the right-hand pane, in place: the
  // goal stays on screen and the view stays Goals.
  await traj.getByRole("button", { name: "Open task #3" }).click();
  await expect(page).toHaveURL(/task=3/);
  await expect(page).toHaveURL(/view=goals/);
  await expect(page).toHaveURL(/goal=faster-ci/);
  await expect(page.locator("#task-detail")).toContainText(
    "Make dashboard Overview a desk page",
  );
  await expect(detail.getByRole("heading", { name: "faster-ci" })).toBeVisible();
  await expect(open.locator("#goal-task-3")).toHaveAttribute("aria-selected", "true");
  await page.screenshot({ path: info.outputPath("goal-task-pane.png"), fullPage: true });
  // Escape closes the pane first, keeping the goal; focus inside the pane
  // returns to the task's row.
  await page.locator("#task-detail").getByRole("button", { name: /^Back/ }).focus();
  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/task=/);
  await expect(page).toHaveURL(/goal=faster-ci/);
  await expect(page.locator("#task-detail")).toHaveCount(0);
  await expect(detail.getByRole("heading", { name: "faster-ci" })).toBeVisible();
  await expect(page.locator('[data-open-task="3"]').first()).toBeFocused();
  // An open task row also opens the task in place, and Back closes it.
  await detail.locator("#goal-task-4").click();
  await expect(page).toHaveURL(/task=4/);
  await expect(page).toHaveURL(/view=goals/);
  await expect(page.locator("#task-detail")).toBeVisible();
  await page.goBack();
  await expect(page).not.toHaveURL(/task=/);
  await expect(detail.getByRole("heading", { name: "faster-ci" })).toBeVisible();
  // A second Escape returns to the list with focus on its row.
  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/goal=/);
  await expect(page.locator("#goal-faster-ci button")).toBeFocused();
  expect(reads.every((r) => r.startsWith("GET "))).toBe(true);
});
test("goal without trajectory or wake, and a bare goal link", async ({ page }) => {
  await mockGoals(page);
  await page.goto(base + "&goal=lark-replies");
  await expect(page).toHaveURL(/view=goals/);
  const detail = page.getByRole("region", { name: "Goal detail" });
  await expect(detail).toContainText("Paused");
  await expect(detail).toContainText("None scheduled");
  await expect(detail).toContainText("No linked tasks");
  await expect(detail).toContainText("No trajectory yet");
});
test("goals empty state", async ({ page }) => {
  await mockGoals(page, []);
  await page.goto(base + "&view=goals");
  await expect(page.getByRole("region", { name: "Goals", exact: true })).toContainText("No goals yet.");
});
test("g o opens goals", async ({ page }) => {
  await mockGoals(page);
  await page.goto(base);
  await page.keyboard.press("g");
  await page.keyboard.press("o");
  await expect(page).toHaveURL(/view=goals/);
});
