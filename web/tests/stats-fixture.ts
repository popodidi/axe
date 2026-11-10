import type { Page } from "@playwright/test";
import { mockDesk, mockStatsHistory, tasks, runs } from "./fixtures";
export const statsRuns = [
    {
      ...runs[1],
      task_id: 8,
      started_at: "2026-10-28T02:00:00Z",
      ended_at: "2026-10-28T02:10:00Z",
    },
    {
      ...runs[1],
      id: 12,
      task_id: 8,
      agent: "claude",
      started_at: "2026-10-28T02:20:00Z",
      ended_at: "2026-10-28T02:30:00Z",
    },
    {
      ...runs[1],
      id: 13,
      task_id: 10,
      started_at: "2026-10-28T02:00:00Z",
      ended_at: "2026-10-28T02:05:00Z",
    },
  ];
/** `missingRuns`: each task records a run start but no run row is returned. */
export async function mockFixtureHistory(
  page: Page,
  { missingRuns = false }: { missingRuns?: boolean } = {},
) {
  await mockStatsHistory(
    page,
    [...tasks.map((t) => t.id), 9, 10],
    (id) => [
      ...(missingRuns
        ? [{ id: 0, kind: "run_start", at: "2026-10-28T01:00:00Z" }]
        : []),
      ...(id === 8
        ? [
            {
              id: 1,
              kind: "state",
              from_value: "pending",
              to_value: "escalated",
              at: "2026-10-28T01:10:00Z",
            },
          ]
        : []),
      {
        id: 2,
        kind: "state",
        from_value: "pending",
        to_value: "done",
        at: id === 9 ? "2026-10-10T03:00:00Z" : "2026-10-28T03:00:00Z",
      },
    ],
    [],
    { runs: missingRuns ? [] : statsRuns },
  );
}
export async function statsFixture(page: Page) {
  await page.clock.setFixedTime(new Date("2026-10-29T12:00:00Z"));
  await mockDesk(page, [
    ...tasks,
    {
      ...tasks[7],
      id: 9,
      title: "Older completion",
      created_at: "2026-10-01T00:00:00Z",
    },
    {
      ...tasks[7],
      id: 10,
      title: "Old creation, recent completion",
      created_at: "2026-10-01T00:00:00Z",
      project: "rake",
    },
  ]);
  await mockFixtureHistory(page);
}
