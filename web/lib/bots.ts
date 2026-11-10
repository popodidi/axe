import { type Run, shellQuote } from "./model";
// Read shapes of the axe bot API (crates/axe/src/bot.rs). Message text is never
// read: wakes come back redacted and `activity` is metadata only.
export type BotSummary = {
  name: string;
  project: string;
  agent: string;
  state: string;
  /** Paused by repeated failures: needs the owner. */
  attention: boolean;
  failures: number;
  live_run: number | null;
  next_wake: string | null;
  queued: number;
  last_run_end: string | null;
};
export type Bot = BotSummary & {
  id: number;
  identity: string;
  conduct: string;
  notes: string;
  nudge_after_seconds: number;
  session_agent: string | null;
  session_id: string | null;
  resume_count: number;
  created_at: string;
  updated_at: string;
};
export type Wake = {
  id: number;
  kind: string;
  body: string;
  due_at: string;
  created_at: string;
  actor: string;
  run_id: number | null;
  delivered_via: string | null;
  redelivered: boolean;
  message?: { id: number; kind: string; at: string } | null;
};
export type BotEvent = {
  id: number;
  at: string;
  actor: string;
  session: string | null;
  kind: string;
  body: string;
};
export type BotSchedule = {
  id: number;
  name: string;
  cron: string;
  timezone: string;
  objective: string;
  enabled: boolean;
  next_at: string;
  last_fired_at: string | null;
};
export type BotRun = {
  id: number;
  bot: string;
  agent: string;
  mode: string;
  session_id: string | null;
  claim: string;
  started_at: string;
  ended_at: string | null;
  exit_code: number | null;
  stop_reason: string | null;
  log_path: string;
  current_activity: string | null;
  last_activity_at: string | null;
  live: boolean;
};
export type MessageMeta = {
  id: number;
  direction: "in" | "out";
  kind: string;
  reply_to: number | null;
  edit_of: number | null;
  attachments: number;
  at: string;
};
export type TaskAction = {
  task_id: number;
  at: string;
  actor: string;
  kind: string;
};
export type Activity = { messages: MessageMeta[]; task_actions: TaskAction[] };
/** Slot use the dispatcher reports in `/api/admission` (axe `pools`). */
export type Slots = { used: number; total: number | null; configured?: number | null };
export type Admission = {
  admitting: boolean | null;
  bots_admitting?: boolean | null;
  bots_reason?: string | null;
  pools?: { tasks: Slots; bots: Slots };
};
/** `used/total`, with `?` when the dispatcher could not count active bots. */
export const slots = (s: Slots) => `${s.used}/${s.total ?? "?"}`;
/** Bot and task slot summary for the Bots view; null from an older dispatcher. */
export function poolSummary(a: Admission | null) {
  const p = a?.pools;
  if (!p) return null;
  const auto = p.bots.configured == null ? " (one per active bot)" : "";
  return `Bot slots ${slots(p.bots)}${auto} · Task slots ${slots(p.tasks)}`;
}
/** Why new bot runs are held (the resource floor), or null when they start. */
export function botHold(a: Admission | null) {
  return a?.bots_admitting === false ? a.bots_reason || "unknown" : null;
}
/** Every read is the latest this many records (the server cap). */
export const BOT_READ_LIMIT = 1000;
export const botTabs = ["profile", "wakes", "runs", "events", "stats"];
export function botStatus(b: Pick<BotSummary, "state" | "attention">) {
  return b.attention ? "Needs attention" : b.state === "paused" ? "Paused" : "Active";
}
/** The persistent bot worktree (crates/axe/src/dispatch/bots.rs `bot-<name>`). */
export const botWorktree = (b: Pick<BotSummary, "project" | "name">) =>
  `~/workspace/.worktrees/${shellQuote(b.project)}/bot-${b.name}`;
/** A bot run in the task-run shape, so `RunList` renders it unchanged. */
export function asRun(r: BotRun, b: Pick<BotSummary, "project" | "name">): Run {
  return {
    id: r.id,
    task_id: 0,
    project: b.project,
    agent: r.agent,
    mode: r.mode,
    started_at: r.started_at,
    ended_at: r.ended_at,
    exit_code: r.exit_code,
    current_activity:
      r.current_activity ?? (r.ended_at && r.stop_reason ? `Ended: ${r.stop_reason}` : null),
    last_activity_at: r.last_activity_at,
    log_path: r.log_path,
    session_id: r.session_id,
    cwd: botWorktree(b),
  };
}
/** Diff bodies of identity, conduct and notes edits, newest first. */
export function profileHistory(events: BotEvent[], field: string) {
  return events
    .filter((e) => e.kind === field)
    .slice()
    .reverse();
}
