import type { Page } from "@playwright/test";
import { mockDesk } from "./fixtures";
import type { Budget, Goal, GoalDetail, GoalEvent } from "../lib/goals";
// Read-only goals API fixture: one goal per state, with budgets, bots, a
// deadline and a trajectory whose events link desk tasks (tests/fixtures.ts).
const DAY = 86400000;
const today = Math.floor(Date.now() / DAY) * DAY;
const ago = (day: number, h = 0) => new Date(today - day * DAY + h * 3600000).toISOString();
const budget = (b: Partial<Budget>): Budget => ({
  cost_week: null,
  cost_spent: 0,
  attention_week: null,
  attention_spent: 0,
  unattributed_runs: 0,
  unknown_runs: 0,
  cost_tasks: 0,
  cost_bots: {},
  window: { since: ago(7), until: ago(0), days: 7 },
  ...b,
});
const goal = (g: Partial<Goal> & { name: string; state: string }): Goal => ({
  id: 1,
  project: "atlas",
  intent: "",
  deadline: null,
  budget_cost_week: null,
  budget_attention_week: null,
  created_at: ago(6),
  updated_at: ago(1, 9),
  bots: [],
  ...g,
  budget: budget({
    cost_week: g.budget_cost_week ?? null,
    attention_week: g.budget_attention_week ?? null,
    ...g.budget,
  }),
});
export const goals: Goal[] = [
  goal({
    id: 1,
    name: "faster-ci",
    state: "active",
    intent: "## Faster CI\n\nCut the **PR CI** wall time below 10 minutes.",
    deadline: "2026-12-31",
    budget_cost_week: 20,
    budget_attention_week: 4,
    bots: ["ops-bot", "watch-bot"],
    budget: budget({ cost_spent: 7.5, attention_spent: 1, cost_tasks: 5.25, cost_bots: { "ops-bot": 2.25 } }),
  }),
  goal({
    id: 2,
    name: "weekly-digest",
    project: "orbit",
    state: "proposed",
    budget_cost_week: 2,
    bots: ["ops-bot"],
    budget: budget({ cost_spent: 3 }),
  }),
  goal({ id: 3, name: "lark-replies", state: "paused" }),
  goal({ id: 4, name: "registry-certs", state: "done", deadline: ago(2) }),
  goal({ id: 5, name: "old-backtests", project: "orbit", state: "dropped" }),
];
const event = (id: number, kind: string, day: number, body: string, task_id: number | null = null): GoalEvent => ({
  id,
  at: ago(day, 9),
  actor: "bot:ops-bot",
  session: null,
  kind,
  task_id,
  owner_ref: null,
  body,
});
export const trajectory: GoalEvent[] = [
  event(11, "hypothesis", 5, "Caching the dependency build halves CI time."),
  event(12, "test", 4, "Enable sccache on the runner.", 3),
  event(13, "observation", 3, "Rust job 19m -> 11m."),
  event(14, "verdict", 2, "Confirmed: keep the cache.", 8),
];
export const detail: GoalDetail = {
  ...goals[0],
  tasks: [
    { id: 3, state: "in_progress", title: "Make dashboard Overview a desk page" },
    { id: 4, state: "pending", title: "Verify Lark reply ledger fix" },
    { id: 8, state: "done", title: "Close out shipped dashboard rebuild" },
  ],
  // The detail route returns the latest 10; the page reads the full trajectory.
  trajectory: trajectory.slice(-1),
  next_wake: ago(-1, 9),
};
/** Every goals read the dashboard made, for the read-only check. */
export async function mockGoals(page: Page, list: Goal[] = goals) {
  const reads: string[] = [];
  await mockDesk(page);
  const json = (body: unknown) => ({
    json: body,
    headers: { "Access-Control-Allow-Origin": "*" },
  });
  await page.route(/^http:\/\/axe\.test\/api\/goals/, async (route) => {
    const url = new URL(route.request().url());
    reads.push(route.request().method() + " " + url.pathname + url.search);
    const path = url.pathname.split("/").slice(3).map(decodeURIComponent);
    if (!path.length) return route.fulfill(json(list));
    const g = list.find((x) => x.name === path[0]);
    if (!g) return route.fulfill({ status: 404, json: { error: `goal ${path[0]} not found` } });
    const own = g.name === detail.name;
    if (path[1] === "events") {
      const events = own ? trajectory : [];
      return route.fulfill(json(events));
    }
    return route.fulfill(
      json(own ? detail : { ...g, tasks: [], trajectory: [], next_wake: null }),
    );
  });
  return reads;
}
