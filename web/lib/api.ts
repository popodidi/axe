// Generated from the Rust API types by crates/axe/tests/ts_bindings.rs.
// Do not edit; regenerate with
// AXE_BLESS=1 cargo test -p axe-os --features ts --test ts_bindings

export type Task = { id: number, project: string, title: string, detail: string, 
/**
 * Stored state.
 */
state: string, agent: string | null, claimed_by: string | null, runs: number, created_at: string, updated_at: string, 
/**
 * Wake time: a pending task is not ready before it (UTC, `WAKE_FORMAT`).
 */
not_before: string | null, 
/**
 * Derived state users see: `blocked` for a stored hold and for a pending task
 * waiting on prerequisites; otherwise the stored state.
 */
status: string, 
/**
 * Derived: prerequisites that are not done yet.
 */
waiting_on: Array<number>, deps: Deps, };

export type Deps = { 
/**
 * Prerequisites: tasks that must be done before this one can be claimed.
 */
blocked_by: Array<DepRef>, 
/**
 * Dependents: tasks waiting on this one.
 */
blocks: Array<DepRef>, };

export type DepRef = { id: number, project: string, title: string, state: string, status: string, };

export type Event = { id: number, task_id: number, at: string, actor: string, session: string | null, kind: string, from_value: string | null, to_value: string | null, body: string | null, };

export type Run = { id: number, task_id: number, project: string, agent: string, mode: string, session_id: string | null, claim: string, started_at: string, ended_at: string | null, exit_code: number | null, log_path: string, current_activity: string | null, last_activity_at: string | null, live: boolean, };

export type Bot = { id: number, name: string, project: string, agent: string, identity: string, conduct: string, notes: string, 
/**
 * Retired and unused. Still served because older CLIs and dispatchers
 * require the field when they parse a bot.
 */
heartbeat_seconds: number, nudge_after_seconds: number, state: string, session_agent: string | null, session_id: string | null, resume_count: number, failures: number, created_at: string, updated_at: string, 
/**
 * Derived: the live run, if any.
 */
live_run: number | null, 
/**
 * Derived: earliest undelivered wake.
 */
next_wake: string | null, 
/**
 * Derived: undelivered wakes.
 */
queued: number, 
/**
 * Derived: end of the latest finished run.
 */
last_run_end: string | null, 
/**
 * Derived: paused by repeated failures (needs the owner).
 */
attention: boolean, };

export type Message = { id: number, kind: string, text: string, reply_to: number | null, edit_of: number | null, attachments: JsonValue, flags: JsonValue, at: string, };

export type Wake = { id: number, kind: string, body: string, due_at: string, created_at: string, actor: string, run_id: number | null, delivered_via: string | null, redelivered: boolean, message: Message | null, };

export type BotRun = { id: number, bot: string, agent: string, mode: string, session_id: string | null, claim: string, started_at: string, ended_at: string | null, exit_code: number | null, stop_reason: string | null, log_path: string, current_activity: string | null, last_activity_at: string | null, live: boolean, dispatcher: string, };

export type BotEvent = { id: number, at: string, actor: string, session: string | null, kind: string, body: string, };

export type MessageMeta = { id: number, direction: string, kind: string, reply_to: number | null, edit_of: number | null, attachments: number, at: string, };

export type TaskAction = { task_id: number, at: string, actor: string, kind: string, };

export type Activity = { messages: Array<MessageMeta>, task_actions: Array<TaskAction>, };

export type BotSchedule = { id: number, name: string, cron: string, timezone: string, objective: string, enabled: boolean, next_at: string, last_fired_at: string | null, created_at: string, updated_at: string, 
/**
 * The goal each firing is labeled with (`[goal NAME]`).
 */
goal: string | null, };

export type Goal = { id: number, name: string, project: string, state: string, intent: string, deadline: string | null, budget_cost_week: number | null, budget_attention_week: number | null, created_at: string, updated_at: string, 
/**
 * Attached bot names.
 */
bots: Array<string>, 
/**
 * Spend over the last 7 days against the weekly budgets; set by `list`
 * and `detail` only.
 */
budget?: Budget, };

export type Budget = { cost_week: number | null, 
/**
 * Estimated USD of the runs attributed to the goal.
 */
cost_spent: number, attention_week: number | null, 
/**
 * Escalation episodes and approvals on linked tasks: one unit each.
 * Goal setup (propose, edit, activate) is not counted.
 */
attention_spent: number, 
/**
 * Bot runs whose wakes mix this goal with other work; not in `cost_spent`.
 */
unattributed_runs: number, 
/**
 * Attributed runs with unknown usage; not in `cost_spent`.
 */
unknown_runs: number, 
/**
 * Cost of linked task runs.
 */
cost_tasks: number, 
/**
 * Cost of goal-tagged bot runs, per bot.
 */
cost_bots: { [key in string]: number }, window: BudgetWindow, };

export type BudgetWindow = { since: string, until: string, days: number, };

export type GoalEvent = { id: number, at: string, actor: string, session: string | null, kind: string, task_id: number | null, owner_ref: string | null, body: string, };

export type GoalTask = { id: number, state: string, title: string, };

export type GoalDetail = { tasks: Array<GoalTask>, trajectory: Array<GoalEvent>, 
/**
 * The next goal-tagged wake ([`next_wake`]).
 */
next_wake: string | null, id: number, name: string, project: string, state: string, intent: string, deadline: string | null, budget_cost_week: number | null, budget_attention_week: number | null, created_at: string, updated_at: string, 
/**
 * Attached bot names.
 */
bots: Array<string>, 
/**
 * Spend over the last 7 days against the weekly budgets; set by `list`
 * and `detail` only.
 */
budget?: Budget, };

export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;
