import { test, expect, type Page, type Route } from "@playwright/test";
import { choose, mockDesk } from "./fixtures";

// Synthetic services on two nodes and one not reported yet.
const now = Date.now();
const at = (secs: number) => new Date(now + secs * 1000).toISOString();
const reported = { reported_at: at(-2) };
const services = [
  {
    bot: "trader", home: "trader", name: "feed", enabled: true, command: "python feed.py --live", restart: "always",
    env: { MODE: "live" }, created_at: at(-90000), updated_at: at(-90000),
    status: { state: "running", pid: 4242, started_at: at(-7500), restarts: 2, last_exit: "signal 15", log: "/logs/feed.log", dispatcher: "hlab-vm", ...reported },
  },
  {
    bot: "trader", home: "trader", name: "hedger", enabled: true, command: "./hedge", restart: "on-failure",
    created_at: at(-9000), updated_at: at(-9000),
    status: { state: "backoff", restarts: 6, last_exit: "exit 1", next_start_at: at(40), crash_loop_since: at(-300), dispatcher: "hlab-vm", ...reported },
  },
  {
    bot: "trader", home: "trader", name: "report", enabled: true, command: "./report.sh", restart: "never",
    schedule: { cron: "*/5 * * * *", timezone: "Asia/Taipei", timeout_seconds: 600, catch_up: true },
    created_at: at(-9000), updated_at: at(-9000),
    status: { state: "scheduled", restarts: 0, failures: 1, started_at: at(-185), finished_at: at(-180), last_exit: "exit 3", next_run_at: at(115), dispatcher: "hlab-vm", ...reported },
  },
  {
    bot: "cos", home: "cos", name: "digest", enabled: false, command: "sleep 1", restart: "never",
    created_at: at(-9000), updated_at: at(-9000),
    status: { state: "stopped", restarts: 0, last_exit: "exit 0", dispatcher: "laptop", reported_at: at(-900) },
  },
  {
    bot: "cos", home: "cos", name: "watch", enabled: true, command: "watch.sh", restart: "on-failure",
    created_at: at(-60), updated_at: at(-60), status: { state: "", restarts: 0 },
  },
];

async function mockServices(page: Page, read: boolean) {
  await mockDesk(page);
  const session = { read };
  await page.route("http://axe.test/api/session", (route: Route) => {
    if (route.request().method() === "POST") session.read = true;
    return route.fulfill({ json: { read: session.read, expires_at: session.read ? "2099-01-01T00:00:00Z" : null } });
  });
  await page.route("http://axe.test/api/services", (route) => route.fulfill({ json: services }));
  await page.route("http://axe.test/api/routines", (route) => route.fulfill({ json: [] }));
  await page.route("http://axe.test/api/bots/*/services/*/logs**", (route) =>
    session.read
      ? route.fulfill({ json: { text: "feed: connected\nfeed: tick 1\nfeed: tick 2", service: services[0] } })
      : route.fulfill({ status: 401, json: { error: "unauthorized" } }),
  );
}

for (const theme of ["dark", "light"]) for (const width of [1440, 390]) {
  test(`services per node with logs ${theme} ${width}`, async ({ page }, info) => {
    await page.setViewportSize({ width, height: 1000 });
    await page.addInitScript((theme) => localStorage.setItem("axe-theme", theme), theme);
    await mockServices(page, true);
    await page.goto("/?server=http://axe.test");
    await page.getByRole("button", { name: "Services", exact: true }).click();
    await expect(page).toHaveURL(/view=services/);
    const vm = page.getByRole("region", { name: "Node hlab-vm" });
    await expect(vm.getByRole("button", { name: "feed", exact: true })).toBeVisible();
    await expect(vm.getByText("running", { exact: true })).toBeVisible();
    await expect(vm.getByText("crash-looping", { exact: true })).toBeVisible();
    await expect(vm.getByText(/^up 2h 5m$/)).toBeVisible();
    await expect(vm.getByText(/^restart in \d+s$/)).toBeVisible();
    // A scheduled service: its next run, and its last run's exit.
    await expect(vm.getByText("scheduled", { exact: true })).toBeVisible();
    await expect(vm.getByText(/^next run in 1m$/)).toBeVisible();
    const laptop = page.getByRole("region", { name: "Node laptop" });
    await expect(laptop.getByText(/no report for 15m/)).toBeVisible();
    await expect(laptop.getByText("stopped", { exact: true })).toBeVisible();
    await expect(page.getByRole("region", { name: "Not reported by a node" }).getByText("pending", { exact: true })).toBeVisible();
    if (width > 760) {
      await expect(vm.getByText("long-running", { exact: true }).first()).toBeVisible();
      await expect(vm.getByText("signal 15", { exact: true })).toBeVisible();
      await expect(vm.getByText("*/5 * * * * Asia/Taipei", { exact: true })).toBeVisible();
      await expect(vm.getByText("exit 3, 3m ago", { exact: true })).toBeVisible();
    }
    // A row opens the service with its recent log.
    await vm.getByRole("button", { name: "feed", exact: true }).click();
    await expect(page).toHaveURL(/service=trader%2Ffeed/);
    const detail = page.getByRole("region", { name: "Service detail" });
    await expect(detail.getByText("python feed.py --live", { exact: true })).toBeVisible();
    await expect(detail.getByText("MODE", { exact: true })).toBeVisible();
    await expect(detail.getByText(/feed: tick 2/)).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({ path: info.outputPath(`services-${theme}-${width}.png`), fullPage: true });
    await page.keyboard.press("Escape");
    await expect(detail).toBeHidden();
    // A scheduled service's detail: its schedule, timeout and runs.
    await vm.getByRole("button", { name: "report", exact: true }).click();
    await expect(detail.getByText("*/5 * * * * Asia/Taipei", { exact: true })).toBeVisible();
    await expect(detail.getByText("10m", { exact: true })).toBeVisible();
    await expect(detail.getByText("next run in 1m", { exact: true })).toBeVisible();
    await expect(detail.getByText("exit 3, 3m ago", { exact: true })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(detail).toBeHidden();
    // The state filter.
    await choose(page.getByLabel("service state", { exact: true }), "crash-looping");
    await expect(page.getByRole("button", { name: "hedger", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "feed", exact: true })).toBeHidden();
  });
}

test("service logs need the read session; Routines and Services link", async ({ page }) => {
  await mockServices(page, false);
  await page.goto("/?server=http://axe.test&view=services&service=trader%2Ffeed");
  const detail = page.getByRole("region", { name: "Service detail" });
  await expect(detail.getByText(/private read/)).toBeVisible();
  await detail.getByLabel("axe token").fill("secret-token");
  await detail.getByRole("button", { name: "Unlock" }).click();
  await expect(detail.getByText(/feed: tick 2/)).toBeVisible();
  await page.getByRole("button", { name: "Routines", exact: true }).last().click();
  await expect(page).toHaveURL(/view=routines/);
  await page.getByRole("button", { name: "Services", exact: true }).last().click();
  await expect(page).toHaveURL(/view=services/);
});
