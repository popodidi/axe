import { test, expect } from "@playwright/test";
import { mockBots } from "./bots-fixture";
import { choose } from "./fixtures";
const base = "/?taskq=http://taskq.test";
for (const theme of ["dark", "light"]) for (const width of [1440, 390]) {
  test(`bots list ${theme} ${width}`, async ({ page }, info) => {
    await page.setViewportSize({ width, height: 1000 });
    await page.addInitScript((t) => localStorage.setItem("harvis-theme", t), theme);
    const reads = await mockBots(page);
    await page.goto(base);
    await page.getByRole("button", { name: "Bots", exact: true }).click();
    await expect(page).toHaveURL(/view=bots/);
    const list = page.getByRole("region", { name: "Bots", exact: true });
    // Bot slots are counted apart from task slots, which are full.
    await expect(list).toContainText("Bot slots 1/2 · Task slots 5/5");
    await expect(list.getByRole("alert")).toHaveCount(0);
    const ops = list.locator("#bot-ops-bot");
    await expect(ops).toContainText("harvis · claude");
    await expect(ops).toContainText("Active");
    await expect(ops).toContainText("Run #31");
    await expect(ops).toContainText("gh pr checks 12");
    const watch = list.locator("#bot-watch-bot");
    await expect(watch).toContainText("Needs attention");
    await expect(watch).toContainText("Idle");
    await expect(watch.locator("td").last()).toHaveText("3");
    if (width > 760) {
      await expect(ops).toContainText("2 queued");
      await expect(ops).toContainText(/in .*\d/);
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: info.outputPath(`bots-list-${theme}-${width}.png`), fullPage: true });
    // Search and project filter apply to bots.
    await page.getByLabel("Search bots").fill("watch");
    await expect(ops).toHaveCount(0);
    await expect(watch).toBeVisible();
    await page.getByLabel("Search bots").fill("");
    await choose(page.getByLabel("project", { exact: true }), "rake");
    await expect(ops).toHaveCount(0);
    // Read-only: GETs only, and never the inbox.
    expect(reads.every((r) => r.startsWith("GET ") && !r.includes("/inbox"))).toBe(true);
  });
}
test("bot detail tabs, stats and the bot run log overlay", async ({ page }, info) => {
  // Final-state screenshots: disclosures and tabs do not animate.
  await page.emulateMedia({ reducedMotion: "reduce" });
  const reads = await mockBots(page);
  await page.goto(base + "&view=bots");
  await page.getByRole("button", { name: "ops-bot", exact: true }).click();
  await expect(page).toHaveURL(/bot=ops-bot/);
  const detail = page.getByRole("region", { name: "Bot detail" });
  await expect(detail.getByRole("heading", { name: "ops-bot" })).toBeVisible();
  await expect(detail).toContainText("claude · 4 resumes");
  // Schedules list with their next fire time.
  const schedules = page.getByRole("region", { name: "Schedules" });
  await expect(schedules.locator("#schedule-weekly-digest")).toContainText("Send the weekly digest");
  await expect(schedules.locator("#schedule-cleanup")).toContainText("Paused");
  // Profile: identity, conduct and notes with their edit history.
  await expect(detail.getByRole("tab", { name: "profile" })).toHaveAttribute("aria-selected", "true");
  await expect(detail).toContainText("Keeps the owner's queue moving.");
  const conduct = detail.getByRole("region", { name: "conduct" });
  await conduct.getByText("History (1 edit)").click();
  await expect(conduct.locator("[data-diff]")).toContainText("+- Be brief.");
  await expect(conduct.locator("[data-diff]")).toBeVisible();
  await expect(detail.getByRole("region", { name: "notes" })).toContainText("History (0 edits)");
  await page.screenshot({ path: info.outputPath("bot-profile.png"), fullPage: true });
  // Wakes: kind, due, delivery run and channel.
  await detail.getByRole("tab", { name: "wakes" }).click();
  await expect(page).toHaveURL(/tab=wakes/);
  const wakes = detail.getByRole("region", { name: "Wakes" });
  await expect(wakes.locator("#wake-4")).toContainText("button");
  await expect(wakes.locator("#wake-4")).toContainText("Run #30");
  await expect(wakes.locator("#wake-4")).toContainText("hook");
  await expect(wakes.locator("#wake-5")).toContainText("check PR #12 CI");
  await expect(wakes.locator("#wake-5")).toContainText("Queued");
  await expect(wakes.locator("#wake-3")).toContainText("message #12 · photo");
  await page.screenshot({ path: info.outputPath("bot-wakes.png"), fullPage: true });
  // Events, including the security drop.
  await detail.getByRole("tab", { name: "events" }).click();
  const drop = detail.locator('[data-kind="security_drop"]');
  await expect(drop).toContainText("Security");
  await expect(drop).toContainText("telegram");
  await page.screenshot({ path: info.outputPath("bot-events.png"), fullPage: true });
  // Stats over the default seven days.
  await detail.getByRole("tab", { name: "stats" }).click();
  const kpis = detail.getByRole("region", { name: "Key figures" });
  await expect(kpis.getByText("Runs", { exact: true }).locator("..")).toContainText("5");
  await expect(kpis).toContainText("4 resumed · 1 fresh · 1 live");
  await expect(kpis).toContainText("5m 30s ± 2m 57s");
  await expect(kpis).toContainText("3m 30s ± 2m 30s");
  await expect(kpis).toContainText("2 replies · 0 unanswered");
  await expect(kpis).toContainText("75%");
  await expect(kpis).toContainText("3 bot:ops-bot · 1 human");
  await expect(kpis).toContainText("1 stops");
  const table = detail.getByRole("region", { name: "Delivery and outcomes" });
  for (const [label, n] of [
    ["Mid-run deliveries (hook)", "1"],
    ["Follow-up runs", "1"],
    ["Failed runs", "1"],
    ["Owner stops", "1"],
    ["Security drops", "1"],
  ])
    await expect(table.locator("tr", { hasText: label }).locator("td").last()).toHaveText(n);
  await expect(detail.getByRole("figure", { name: "Runs per day" })).toBeVisible();
  await expect(detail.getByRole("figure", { name: "Runtime over time" })).toBeVisible();
  await expect(detail.getByRole("figure", { name: "Reply latency over time" })).toBeVisible();
  await expect(detail.getByRole("figure", { name: "Task actions per day" })).toContainText("bot:ops-bot");
  await page.screenshot({ path: info.outputPath("bot-stats.png"), fullPage: true });
  await choose(page.getByLabel("Period"), "30");
  await expect(page).toHaveURL(/days=30/);
  await expect(kpis).toContainText("4 resumed · 1 fresh · 1 live");
  // Runs reuse RunList; a finished run opens the bot run log overlay.
  await detail.getByRole("tab", { name: "runs" }).click();
  await expect(detail.getByRole("article", { name: "Run #31" })).toContainText("Running");
  await detail.getByRole("article", { name: "Run #30" }).getByRole("button", { name: "View log" }).click();
  await expect(page).toHaveURL(/botrun=30/);
  const log = page.getByRole("dialog", { name: "Run log" });
  await expect(log.getByRole("heading", { level: 2 })).toHaveText("Run #30 · Bot ops-bot · claude");
  await expect(log).toContainText("Bot run 30 transcript");
  await page.screenshot({ path: info.outputPath("bot-run-log.png") });
  expect(reads).toContain("GET /api/bot-runs/30/log");
  await log.getByRole("button", { name: "Back to dashboard" }).click();
  await expect(page).not.toHaveURL(/botrun=/);
  await expect(log).toBeHidden();
  // Escape returns to the list, focusing the bot row.
  await page.keyboard.press("Escape");
  await expect(page).not.toHaveURL(/bot=/);
  await expect(page.locator("#bot-ops-bot button")).toBeFocused();
  expect(reads.every((r) => r.startsWith("GET "))).toBe(true);
});
test("a bare bot link opens the Bots view and g b navigates there", async ({ page }) => {
  await mockBots(page);
  await page.goto(base + "&bot=watch-bot");
  await expect(page).toHaveURL(/view=bots/);
  const detail = page.getByRole("region", { name: "Bot detail" });
  await expect(detail).toContainText("Needs attention");
  await expect(detail).toContainText("Fresh on next run");
  await page.getByRole("button", { name: "Tasks", exact: true }).click();
  await expect(page).not.toHaveURL(/bot=/);
  await page.keyboard.press("g");
  await page.keyboard.press("b");
  await expect(page).toHaveURL(/view=bots/);
  await expect(page.getByRole("region", { name: "Bots", exact: true })).toBeVisible();
});
