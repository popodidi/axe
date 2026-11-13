// Read shapes of the axe goals API (crates/axe/src/goal.rs, `goal_route` in crates/axe/src/server.rs).
export type BudgetWindow = { since: string; until: string; days: number };
export type Budget = {
  cost_week: number | null;
  cost_spent: number;
  attention_week: number | null;
  attention_spent: number;
  unattributed_runs: number;
  unknown_runs: number;
  cost_tasks: number;
  cost_bots: Record<string, number>;
  window: BudgetWindow;
};
export type Goal = {
  id: number;
  name: string;
  project: string;
  state: string;
  intent: string;
  deadline: string | null;
  budget_cost_week: number | null;
  budget_attention_week: number | null;
  created_at: string;
  updated_at: string;
  bots: string[];
  budget?: Budget;
};
export type GoalEvent = {
  id: number;
  at: string;
  actor: string;
  session: string | null;
  kind: string;
  task_id: number | null;
  owner_ref: string | null;
  body: string;
};
export type GoalTask = { id: number; state: string; title: string };
export type GoalDetail = Goal & {
  tasks: GoalTask[];
  trajectory: GoalEvent[];
  next_wake: string | null;
};
export const goalStates = ["proposed", "active", "paused", "done", "dropped"];
/** Trajectory events the detail page reads (the server maximum). */
export const TRAJECTORY_LIMIT = 1000;
/** Task states that close a linked task (as the goal board counts them). */
const closedTaskStates = ["done"];
export const usd = (n: number) => `$${n.toFixed(2)}`;
/** Spent against a weekly limit; `null` limit is unlimited. */
export type Meter = {
  /** `$1.20/$5.00`, `3/4`, or the spend alone with no limit. */
  text: string;
  /** Filled share of the limit, 0..1; null without a limit. */
  share: number | null;
  over: boolean;
};
function meter(spent: number, limit: number | null, fmt: (n: number) => string): Meter {
  if (limit === null)
    return { text: `${fmt(spent)} · no limit`, share: null, over: false };
  return {
    text: `${fmt(spent)}/${fmt(limit)}`,
    share: limit > 0 ? Math.min(1, spent / limit) : 1,
    over: spent >= limit,
  };
}
export function costMeter(g: Goal): Meter {
  return meter(g.budget?.cost_spent ?? 0, g.budget_cost_week, usd);
}
export function attentionMeter(g: Goal): Meter {
  return meter(g.budget?.attention_spent ?? 0, g.budget_attention_week, String);
}
export function openTasks(tasks: GoalTask[]) {
  return tasks.filter((t) => !closedTaskStates.includes(t.state));
}
/** Goals matching the list's project filter and search (`q`). */
export function selectGoals(goals: Goal[], params: URLSearchParams) {
  const q = (params.get("q") || "").toLowerCase();
  const project = params.get("project");
  return goals.filter(
    (g) =>
      (!project || g.project === project) &&
      `${g.name} ${g.project} ${g.bots.join(" ")} ${g.intent}`
        .toLowerCase()
        .includes(q),
  );
}
/** A deadline is a date (`YYYY-MM-DD`) or an RFC3339 time. */
export function deadline(d: string | null) {
  if (!d) return "—";
  return /^\d{4}-\d{2}-\d{2}$/.test(d) ? d : new Date(d).toLocaleString();
}
