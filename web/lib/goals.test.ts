import { describe, it, expect } from "vitest";
import {
  attentionMeter,
  costMeter,
  deadline,
  linkedTasks,
  openTasks,
  selectGoals,
  type Goal,
} from "./goals";
const goal = (g: Partial<Goal> = {}): Goal => ({
  id: 1,
  name: "faster-ci",
  project: "atlas",
  state: "active",
  intent: "Cut CI time",
  deadline: null,
  budget_cost_week: null,
  budget_attention_week: null,
  created_at: "2026-11-01T00:00:00Z",
  updated_at: "2026-11-01T00:00:00Z",
  bots: ["ops-bot"],
  ...g,
});
const budget = (cost: number, attention: number) => ({
  cost_week: null,
  cost_spent: cost,
  attention_week: null,
  attention_spent: attention,
  unattributed_runs: 0,
  unknown_runs: 0,
  cost_tasks: cost,
  cost_bots: {},
  window: { since: "", until: "", days: 7 },
});
describe("goal meters", () => {
  it("shows spent against the weekly limit with a clamped share", () => {
    const g = goal({
      budget_cost_week: 5,
      budget_attention_week: 4,
      budget: budget(1.2, 3),
    });
    expect(costMeter(g)).toEqual({ text: "$1.20/$5.00", share: 0.24, over: false });
    expect(attentionMeter(g)).toEqual({ text: "3/4", share: 0.75, over: false });
    const over = goal({ budget_cost_week: 2, budget: budget(3, 0) });
    expect(costMeter(over)).toEqual({ text: "$3.00/$2.00", share: 1, over: true });
  });
  it("has no bar without a limit and treats a zero limit as spent", () => {
    expect(costMeter(goal({ budget: budget(0.5, 1) }))).toEqual({
      text: "$0.50 · no limit",
      share: null,
      over: false,
    });
    expect(attentionMeter(goal({ budget_attention_week: 0 }))).toEqual({
      text: "0/0",
      share: 1,
      over: true,
    });
  });
});
describe("goal lists", () => {
  it("filters by project and searches name, bots and intent", () => {
    const goals = [goal(), goal({ id: 2, name: "docs", project: "orbit", intent: "", bots: [] })];
    const p = (q: string) => new URLSearchParams(q);
    expect(selectGoals(goals, p("project=orbit")).map((g) => g.name)).toEqual(["docs"]);
    expect(selectGoals(goals, p("q=OPS-BOT")).map((g) => g.name)).toEqual(["faster-ci"]);
    expect(selectGoals(goals, p("q=cut ci")).map((g) => g.name)).toEqual(["faster-ci"]);
  });
  it("keeps open linked tasks", () => {
    expect(
      openTasks([
        { id: 1, state: "done", title: "a" },
        { id: 2, state: "escalated", title: "b" },
      ]).map((t) => t.id),
    ).toEqual([2]);
  });
  it("lists every linked task, open first then done newest first", () => {
    expect(
      linkedTasks([
        { id: 1, state: "done", title: "a" },
        { id: 2, state: "escalated", title: "b" },
        { id: 3, state: "done", title: "c" },
        { id: 4, state: "pending", title: "d" },
      ]).map((t) => t.id),
    ).toEqual([2, 4, 3, 1]);
  });
  it("shows a date deadline as is", () => {
    expect(deadline("2026-12-01")).toBe("2026-12-01");
    expect(deadline(null)).toBe("—");
  });
});
