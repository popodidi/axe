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
waiting_on: Array<number>, deps: Deps, 
/**
 * Who follows the task up: a bot (`bot:<name>`) or a person. The creator is
 * the actor of the `created` event.
 */
owner: string | null, 
/**
 * Repos the task works in, each a worktree in its run workspace. `None`
 * (tasks from before repos existed) is the same as none.
 */
repos: Array<string> | null, 
/**
 * Free-form labels. Filters also match the project as a label.
 */
labels: Array<string>, };

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
 * Repos the bot works in: each run gets a run folder with one worktree
 * per repo (`run_workspace.rs`). `None` (also an empty list): a run
 * folder with no repos.
 */
repos: Array<string> | null, 
/**
 * The bot's retro card settings (`bots.retro`): the stored ones, else
 * the defaults. `None` from servers that predate per-bot retros.
 */
retro?: RetroConfig, 
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
attention: boolean, 
/**
 * Derived: the name the bot's folders on the node host are keyed
 * by (run folder, logs, sealed form values): its first
 * name, so a rename keeps them and the agent session that lives in them.
 * `None` from servers that predate renames: the name.
 */
home: string | null, };

export type RetroConfig = { 
/**
 * Whether the card goes out.
 */
on: boolean, 
/**
 * The interval, in whole days (`7d`, 1d-28d). The card holds the asks
 * of the last interval.
 */
every: string, 
/**
 * The first slot: a UTC weekday and time (`mon 01:00`); slots repeat
 * every `every` from it.
 */
at: string, 
/**
 * The most asks on the card (1-1000).
 */
size: number, };

export type Message = { id: number, kind: string, text: string, reply_to: number | null, 
/**
 * Text of the stored message `reply_to` points at, when there is one.
 */
reply_text: string | null, edit_of: number | null, attachments: JsonValue, flags: JsonValue, at: string, };

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
budget?: Budget, 
/**
 * Set when the goal is idle ([`idle_since`]): since when it has had no
 * next action (RFC3339); set by `list` and `detail` only.
 */
idle_since?: string, };

export type Budget = { cost_week: number | null, 
/**
 * Estimated USD of the runs attributed to the goal.
 */
cost_spent: number, attention_week: number | null, 
/**
 * Escalation episodes and approvals on linked tasks: one unit each,
 * except episodes the owner rated wasted ([`attention`]). Goal setup
 * (propose, edit, activate) is not counted.
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
budget?: Budget, 
/**
 * Set when the goal is idle ([`idle_since`]): since when it has had no
 * next action (RFC3339); set by `list` and `detail` only.
 */
idle_since?: string, };

export type Service = { bot: string, 
/**
 * The bot's folder key (`Bot::home`): where its run folder and logs are.
 */
home: string, name: string, enabled: boolean, 
/**
 * Empty (`state` "") until a node reported it.
 */
status: ServiceStatus, created_at: string, updated_at: string, 
/**
 * A shell command (`sh -c`).
 */
command: string, 
/**
 * Where it runs: absolute, or relative to the bot's run folder (the
 * default).
 */
cwd?: string, env?: { [key in string]: string }, 
/**
 * `never` for a scheduled service.
 */
restart: string, 
/**
 * Set: a scheduled service, run at these times instead of kept running.
 */
schedule?: ServiceSchedule, };

export type ServiceStatus = { 
/**
 * `starting`, `running`, `backoff` (waiting to restart), `stopping`,
 * `stopped` or `exited` (done, not restarted by its policy); a scheduled
 * service is `scheduled` between runs.
 */
state: string, pid?: number, 
/**
 * When the current (or last) process started: a scheduled service's
 * last run.
 */
started_at?: string, 
/**
 * Restarts since the node started the service.
 */
restarts: number, 
/**
 * Failures since the node started the service: unsuccessful exits
 * it did not ask for, and starts that failed.
 */
failures: number, 
/**
 * How the last process ended: `exit N` or `signal N`, or for a
 * scheduled run past its timeout `timeout after Ns (signal N)`.
 */
last_exit?: string, 
/**
 * When the next restart is due, in `backoff`.
 */
next_start_at?: string, 
/**
 * Since when it has been crash looping, if it is.
 */
crash_loop_since?: string, 
/**
 * Why it could not start, if it could not.
 */
error?: string, 
/**
 * A scheduled service: when its last run ended.
 */
finished_at?: string, 
/**
 * A scheduled service: when it runs next.
 */
next_run_at?: string, 
/**
 * The log file on the node's machine.
 */
log?: string, 
/**
 * The node that reported it, and when.
 */
dispatcher?: string, reported_at?: string, };

export type ServiceSchedule = { cron: string, timezone: string, 
/**
 * A run still going this long is stopped (SIGTERM, then SIGKILL after
 * the node's stop grace) and counts as failed.
 */
timeout_seconds: number, 
/**
 * One run at node start when a run time passed while it was down.
 */
catch_up: boolean, };

export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;
