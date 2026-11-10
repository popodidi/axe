import type { Page } from "@playwright/test";
import { mockDesk, mockStatsHistory, tasks, runs } from "./fixtures";
// Synthetic trend cohort, snapshot 2026-10-29 12:00 UTC: 150 completions spread
// over the last 90 days across three projects and two agents, every fourth a
// Routine. Task 3000 completes today; completions are 0.6 days apart.
const DAY = 86400000;
const now = Date.parse("2026-10-29T12:00:00Z");
const projects = ["atlas", "orbit", "tundra-desk"];
export const trendIds = Array.from({ length: 150 }, (_, i) => 3000 + i);
const k = (id: number) => id - 3000;
export const completedAt = (id: number) =>
  now - 3600000 - Math.round(k(id) * 0.6 * DAY);
const createdAt = (id: number) =>
  completedAt(id) - (2 + ((k(id) * 13) % 17) * 2) * 3600000;
const runCount = (id: number) => 1 + (((k(id) * 5) % 7) % 4);
export async function statsTrendsFixture(
  page: Page,
  { missingRuns = false }: { missingRuns?: boolean } = {},
) {
  await page.clock.setFixedTime(new Date(now));
  await mockDesk(page, [
    ...trendIds.map((id) => ({
      ...tasks[7],
      id,
      title: `Trend task ${id}`,
      project: projects[k(id) % 3],
      state: "done",
      created_at: new Date(createdAt(id)).toISOString(),
    })),
    { ...tasks[3], id: 3100, title: "Open trend task", project: "atlas" },
  ]);
  const at = (t: number) => new Date(t).toISOString();
  const list = trendIds.flatMap((id) =>
    Array.from({ length: runCount(id) }, (_, r) => ({
      ...runs[1],
      id: id * 10 + r,
      task_id: id,
      project: projects[k(id) % 3],
      agent: k(id) % 2 ? "claude" : "codex",
      started_at: new Date(createdAt(id) + r * 600000).toISOString(),
      ended_at: new Date(
        createdAt(id) + r * 600000 + (2 + ((k(id) * 11) % 19) * 2) * 60000,
      ).toISOString(),
    })),
  );
  await mockStatsHistory(
    page,
    trendIds,
    (id) => [
      ...Array.from({ length: k(id) % 4 === 1 ? 2 : 0 }, (_, e) => ({
        id: 1 + e * 2,
        kind: "state",
        from_value: "pending",
        to_value: "escalated",
        at: at(createdAt(id) + (e + 1) * 60000),
      })),
      ...Array.from({ length: runCount(id) }, (_, r) => ({
        id: 10 + r,
        kind: "run_start",
        at: at(createdAt(id) + r * 600000),
      })),
      {
        id: 20,
        kind: "state",
        from_value: "pending",
        to_value: "done",
        at: at(completedAt(id)),
      },
    ],
    trendIds.filter((id) => k(id) % 4 === 0),
    { runs: missingRuns ? [] : list },
  );
}
