import type { Page } from "@playwright/test";
import { mockDesk, mockStatsHistory, tasks } from "./fixtures";
// Routine-heavy synthetic cohort, snapshot 2026-10-29 12:00 UTC (7-day window
// 2026-10-23..29): 50 routine and 19 normal completions in range across
// harvis and rake, one routine completed just before the window, and open tasks of each type.
// Normal titles mimic routines and vice versa: titles never decide type.
export const typed = {
  routine: Array.from({ length: 50 }, (_, i) => 1000 + i),
  normal: Array.from({ length: 19 }, (_, i) => 1050 + i),
  before: 1070,
  open: { routine: [1080, 1081], normal: [1082] },
};
export async function statsTypesFixture(page: Page) {
  await page.clock.setFixedTime(new Date("2026-10-29T12:00:00Z"));
  const done = (id: number, routine: boolean) => ({
    ...tasks[7],
    id,
    title: routine ? `Ship feature ${id}` : `Routine: daily digest ${id}`,
    project: id % 2 ? "rake" : "harvis",
    state: "done",
    created_at: "2026-10-01T00:00:00Z",
  });
  const list = [
    ...typed.routine.map((id) => done(id, true)),
    ...typed.normal.map((id) => done(id, false)),
    done(typed.before, true),
    ...[...typed.open.routine, ...typed.open.normal].map((id) => ({
      ...tasks[3],
      id,
      title: `Open ${id}`,
      project: id % 2 ? "rake" : "harvis",
    })),
  ];
  await mockDesk(page, list);
  const DAY = 86400000;
  const completion = (id: number) =>
    id === typed.before
      ? "2026-10-22T23:59:59.999Z"
      : id === 1000
        ? "2026-10-23T00:00:00.000Z"
        : new Date(
            Date.parse("2026-10-23T00:00:00Z") + (id % 7) * DAY + 3600000,
          ).toISOString();
  await mockStatsHistory(
    page,
    list.map((t) => t.id),
    (id) => [
      {
        id: 1,
        kind: "state",
        from_value: "pending",
        to_value: "done",
        at: completion(id),
      },
    ],
    [...typed.routine, typed.before, ...typed.open.routine],
  );
}
