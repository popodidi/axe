import type { Locator, Page } from "@playwright/test";
import type { StatsRun } from "../lib/stats";
// Read-only taskq fixture for the desk: an ops queue, live and ended runs,
// per-project counts, and a spec that exercises compact Markdown.
const spec = `## Outcome

The dashboard opens on one **Overview** desk page.

1. Ops queue, live runs, and counts together.
2. The full task list stays as a secondary page.

### Verify

- \`npm test\` passes.
- Browser tests cover the desk.

\`\`\`sh
cd infra/dashboard && npm ci && npm test
\`\`\`

| Area | Agent |
| --- | --- |
| dashboard | claude |
`;
const at = (minutes: number) =>
  new Date(Date.UTC(2026, 9, 28, 1, minutes)).toISOString();
const rows: [number, string, string, string][] = [
  [1, "Rotate hlab registry certificate", "harvis", "escalated"],
  [2, "Approve rake backtest rollout", "rake", "escalated"],
  [3, "Make dashboard Overview a desk page", "harvis", "in_progress"],
  [4, "Verify Lark reply ledger fix", "harvis", "pending"],
  [5, "Ship tn-desk weekly digest", "tn-desk", "pending"],
  [6, "Archive amux experiments", "amux", "done"],
  [7, "Triage semx-desk intake", "semx-desk", "escalated"],
  [8, "Close out shipped dashboard rebuild", "harvis", "done"],
];
export const tasks = rows.map(([id, title, project, state]) => ({
  id,
  title,
  project,
  state,
  detail: spec,
  created_at: at(id),
  updated_at: at(id + 10),
  runs: 1,
}));
export const runs = [
  {
    id: 11,
    task_id: 3,
    project: "harvis",
    agent: "claude",
    mode: "dispatch",
    started_at: at(20),
    ended_at: null,
    exit_code: null,
    current_activity: "npm run test:browser",
    last_activity_at: at(40),
    log_path: "/home/harvis/.local/state/taskq/logs/run 11.log",
    session_id: "48205d0b-549b-41f5-8e11-3ff79df32d56",
  },
  {
    id: 10,
    task_id: 4,
    project: "harvis",
    agent: "codex",
    mode: "dispatch",
    started_at: at(5),
    ended_at: at(15),
    exit_code: 0,
    current_activity: "taskq set 4 --state done",
    last_activity_at: at(15),
    log_path: "/home/harvis/.local/state/taskq/logs/run-10.log",
    session_id: "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
  },
];
// Block notes for the ops queue (`/api/overview` `ops_queue`), by task id.
const blockers: Record<number, [string, string]> = {
  1: ["dispatcher", "max_runs exceeded (20 >= 20)"],
  2: ["agent", "ESCALATE tier=high ceiling=medium: needs approval"],
  7: ["agent", "PR head moved since last review"],
  22: ["ops", "human must confirm scope"],
};
type FixtureTask = (typeof tasks)[number] & {
  status?: string;
  not_before?: string | null;
  waiting_on?: number[];
};
// Mirrors taskq: stored blocked is an actionable hold; a pending task with
// unfinished prerequisites is a dependency wait without block context.
const queueOf = (list: FixtureTask[]) =>
  list
    .filter((t) => ["blocked", "escalated"].includes(t.status || t.state))
    .map((t) =>
      ["blocked", "escalated"].includes(t.state)
        ? {
            ...t,
            kind: "hold",
            actionable: true,
            blocked_by: blockers[t.id]?.[0] ?? null,
            blocked_at: t.updated_at,
            blocker: blockers[t.id]
              ? `blocked: ${blockers[t.id][1]}\nresolver: ops: decide\nresume: taskq set ${t.id} --state pending`
              : null,
          }
        : {
            ...t,
            kind: "dependency",
            actionable: false,
            blocked_by: null,
            blocked_at: null,
            blocker: null,
          },
    );
/**
 * Fan-in: #22 (manual hold) and #23 wait on #21; #22 also needs done #20.
 * With `finished`, #21 is done: #23 becomes ready, #22 keeps its hold.
 */
export function depTasks(finished = false) {
  const base = (id: number, title: string, project: string) => ({
    ...tasks[0],
    id,
    title,
    project,
    created_at: at(id),
    updated_at: at(id + 10),
  });
  const s21 = finished ? "done" : "in_progress";
  const ref = (
    id: number,
    title: string,
    project: string,
    state: string,
    status = state,
  ) => ({ id, title, project, state, status });
  const r20 = ref(20, "Build dependency storage", "harvis", "done");
  const r21 = ref(21, "Build dependency dashboard", "rake", s21);
  const r22 = ref(22, "Integrate dependencies", "harvis", "blocked");
  const r23 = ref(
    23,
    "Document dependencies",
    "harvis",
    "pending",
    finished ? "pending" : "blocked",
  );
  const wait = finished ? [] : [21];
  return [
    {
      ...base(20, r20.title, "harvis"),
      state: "done",
      status: "done",
      waiting_on: [],
      deps: { blocked_by: [], blocks: [r22] },
    },
    {
      ...base(21, r21.title, "rake"),
      state: s21,
      status: s21,
      waiting_on: [],
      deps: { blocked_by: [], blocks: [r22, r23] },
    },
    {
      ...base(22, r22.title, "harvis"),
      state: "escalated",
      status: "escalated",
      waiting_on: wait,
      deps: { blocked_by: [r20, r21], blocks: [] },
    },
    {
      ...base(23, r23.title, "harvis"),
      state: "pending",
      status: r23.status,
      waiting_on: wait,
      deps: { blocked_by: [r21], blocks: [] },
    },
  ];
}
export const counts = tasks.map((t) => ({
  project: t.project,
  state: t.state,
  count: 1,
}));
// Enough tasks for a second list page.
export const manyTasks = [
  ...tasks,
  ...Array.from({ length: 30 }, (_, i) => ({
    ...tasks[2],
    id: 100 + i,
    title: `Backlog task ${100 + i}`,
    created_at: at(30 + i),
  })),
];
export async function mockDesk(page: Page, list: FixtureTask[] = tasks) {
  await page.route("http://taskq.test/**", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === "/api/stream") {
      await route.abort();
      return;
    }
    const match = url.pathname.match(/^\/api\/tasks\/(\d+)(\/events)?$/);
    const task = match && list.find((t) => t.id === Number(match[1]));
    const result =
      url.pathname === "/api/tasks"
        ? list
        : url.pathname === "/api/stats/history"
          ? { events: [], routines: [], runs: [] }
          : url.pathname === "/api/overview"
          ? { counts, ops_queue: queueOf(list), running: 1 }
          : url.pathname === "/api/runs"
            ? url.searchParams.get("task")
              ? runs.filter(
                  (r) => r.task_id === Number(url.searchParams.get("task")),
                )
              : runs
            : match?.[2]
              ? [
                  {
                    id: 1,
                    at: at(1),
                    actor: "human",
                    kind: "note",
                    body: "## Note\n\nHuman approved spec.\n\n- keep it small\n- ship behind PR",
                    from_value: null,
                    to_value: null,
                  },
                ]
              : task;
    await route.fulfill({
      json: result,
      headers: { "Access-Control-Allow-Origin": "*" },
    });
  });
}

export type StatsEvent = {
  id: number;
  kind: string;
  at: string;
  from_value?: string | null;
  to_value?: string | null;
};
/**
 * Serves `GET /api/stats/history` (the only per-task source Stats reads):
 * `eventsOf(id)` for each task in `ids`, and the routine occurrence IDs.
 * `runs` are its run rows; `onRead` sees each request.
 */
export async function mockStatsHistory(
  page: Page,
  ids: number[],
  eventsOf: (id: number) => StatsEvent[],
  routines: number[] = [],
  {
    onRead,
    runs: statsRuns = [],
  }: {
    onRead?: () => Promise<void> | void;
    runs?: StatsRun[];
  } = {},
) {
  await page.route("**/api/stats/history", async (route) => {
    await onRead?.();
    await route
      .fulfill({
        headers: { "Access-Control-Allow-Origin": "*" },
        json: {
          events: ids.flatMap((id) =>
            eventsOf(id).map((e) => ({
              from_value: null,
              to_value: null,
              ...e,
              task_id: id,
            })),
          ),
          routines,
          runs: statsRuns,
        },
      })
      .catch(() => {}); // the page aborted this read
  });
}

export const timedTasks = [
  { ...tasks[3], status: "blocked", waiting_on: [], not_before: "2099-11-09T09:00:00Z" },
  { ...tasks[4], status: "blocked", waiting_on: [4], not_before: "2099-11-09T09:00:00Z" },
];

// The styled Select (components/ui/select.tsx) is a combobox button with a
// listbox; it carries its committed value in data-value.
export async function choose(trigger: Locator, value: string) {
  await trigger.click();
  await trigger
    .page()
    .locator(`[role="listbox"] [role="option"][data-value="${value}"]`)
    .click();
}
