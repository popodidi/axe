import { test, expect } from "@playwright/test";
import { mockFixtureHistory, statsFixture } from "./stats-fixture";
import { choose, mockDesk, mockStatsHistory, tasks } from "./fixtures";
const base = "/?server=http://axe.test&view=stats";
test("loading, errors, partial samples and unknown completion remain explicit", async ({
  page,
}) => {
  await statsFixture(page);
  let release!: () => void;
  const gate = new Promise<void>((r) => {
    release = r;
  });
  await page.route("**/api/stats/history", async (route) => {
    await gate;
    await route.fulfill({ status: 503 });
  });
  await page.goto(base);
  await expect(
    page.getByRole("status", { name: "Loading history…" }),
  ).toBeVisible();
  release();
  await expect(
    page.getByRole("alert").filter({ hasText: "History unavailable" }),
  ).toBeVisible();
  await mockFixtureHistory(page, { missingRuns: true });
  await page.getByRole("button", { name: "Refresh history" }).click();
  const scope = page.getByRole("region", { name: "Statistics scope" });
  await expect(
    page.getByText("0 samples · 3 unknown", { exact: false }),
  ).toHaveCount(2);
  await expect(
    page
      .getByRole("region", { name: "All completed tasks", exact: true })
      .locator("tbody tr")
      .filter({ hasText: "Runs per task" }),
  ).toContainText("Unknown");
  // No done transition recorded for any task: completion stays unknown.
  await mockStatsHistory(page, [], () => []);
  await page.getByRole("button", { name: "Refresh history" }).click();
  await expect(scope).toContainText("4 tasks with unknown completion");
  await expect(
    page.getByText("No known completed tasks in this range match the filters."),
  ).toBeVisible();
});
test("a large completed cohort loads with one history read and no per-task reads", async ({
  page,
}) => {
  // Fixed "now" is 2026-10-29T12:00Z, so Last 7 days is 2026-10-23 .. 10-29 UTC.
  await page.clock.setFixedTime(new Date("2026-10-29T12:00:00Z"));
  const DAY = 86400000;
  const projects = ["atlas", "orbit", "tundra-desk"];
  const completion: Record<number, string> = {};
  for (let id = 1; id <= 75; id++)
    completion[id] =
      id === 1
        ? "2026-10-29T11:00:00.000Z" // low ID, most recent completion
        : id === 2
          ? "2026-10-23T00:00:00.000Z" // first instant of the range
          : new Date(
              Date.parse("2026-10-23T00:00:00Z") + (id % 7) * DAY + id * 60000,
            ).toISOString();
  // Newer IDs completed outside the range, just before it and long before.
  completion[200] = "2026-10-22T23:59:59.999Z";
  completion[201] = "2026-10-01T00:00:00.000Z";
  const list = Object.keys(completion).map((k) => {
    const id = Number(k);
    return {
      ...tasks[7],
      id,
      title: `Cohort task ${id}`,
      project: id <= 75 ? projects[id % 3] : "atlas",
      created_at: "2026-09-01T00:00:00Z",
    };
  });
  // Reopened after an in-range completion: open, never fetched or counted.
  list.push({ ...list[0], id: 202, state: "pending", title: "Reopened" });
  await mockDesk(page, list);
  let active = 0,
    reads = 0;
  await mockStatsHistory(
    page,
    list.map((t) => t.id),
    (id) => [
      {
        id: 1,
        kind: "state",
        from_value: "pending",
        to_value: "done",
        at: completion[id],
      },
    ],
    [],
    {
      onRead: async () => {
        reads++;
        active++;
        await new Promise((r) => setTimeout(r, 20));
        active--;
      },
    },
  );
  const perTask: string[] = [];
  page.on("request", (r) => {
    if (/\/api\/tasks\/\d+\//.test(r.url())) perTask.push(r.url());
  });
  const methods: string[] = [];
  page.on("request", (r) => {
    if (r.url().startsWith("http://axe.test")) methods.push(r.method());
  });
  const scope = page.getByRole("region", { name: "Statistics scope" });
  const daily = page.getByRole("figure", { name: "Completions per day" });
  const expectDaily = async (ids: number[], group: string) => {
    for (let d = 0; d < 7; d++) {
      const day = Date.parse("2026-10-23T00:00:00Z") + d * DAY;
      const n = ids.filter((id) => {
        const t = Date.parse(completion[id]);
        return t >= day && t < day + DAY;
      }).length;
      const label = new Date(day).toISOString().slice(5, 10);
      await expect(daily.locator("li", { hasText: `${label}:` })).toHaveText(
        `${label}: ${group}: ${n}`,
      );
    }
  };
  const cohort = Array.from({ length: 75 }, (_, i) => i + 1);
  const orbit = cohort.filter((id) => id % 3 === 1);

  // Changing project mid-load cancels the all-project read.
  await page.goto(base);
  await expect.poll(() => reads).toBeGreaterThanOrEqual(1);
  await choose(page.getByLabel("project", { exact: true }), "orbit");
  await expect(scope).toContainText(`${orbit.length} completed cohort tasks`);
  await expect(scope).toContainText("0 tasks with unknown completion");
  await expectDaily(orbit, "All tasks");

  // Back to all projects: a complete, consistent cohort larger than 50, from
  // one read; the reopened task 202 is not counted.
  await expect.poll(() => active).toBe(0);
  reads = 0;
  await choose(page.getByLabel("project", { exact: true }), "");
  await expect(scope).toContainText("75 completed cohort tasks");
  expect(reads).toBe(1);
  expect(perTask).toEqual([]);
  const kpis = page.getByRole("region", { name: "Key figures" });
  await expect(kpis).toContainText("Completed tasks75");
  await expectDaily(cohort, "All tasks");
  for (const m of [
    "Runs per task",
    "Escalations per task",
    "Wall-clock elapsed time",
    "Active agent runtime",
  ])
    await expect(
      page.getByRole("figure", { name: m, exact: true }),
    ).toContainText("75 samples · 0 unknown");
  await expect(
    page
      .getByRole("region", { name: "All completed tasks", exact: true })
      .locator("tbody tr")
      .first(),
  ).toContainText("All tasks75");
  const byProject = page.getByRole("region", { name: "By project" });
  for (const p of projects)
    await expect(
      byProject.locator("tbody tr", { hasText: p }).first(),
    ).toContainText(`${p}25`);

  // Group-by is a pure view change: no history reload.
  const before = reads;
  await choose(page.getByLabel("Split by", { exact: true }), "project");
  await expect(daily.locator("li", { hasText: "10-23:" })).toContainText(
    "atlas:",
  );
  expect(reads).toBe(before);

  // Narrow layout keeps the full cohort.
  await page.setViewportSize({ width: 390, height: 900 });
  await expect(kpis).toContainText("Completed tasks75");
  expect(reads).toBe(before);
  expect(methods.every((m) => m === "GET")).toBe(true);
});
