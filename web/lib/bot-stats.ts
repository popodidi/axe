import {
  BOT_READ_LIMIT,
  type Activity,
  type BotEvent,
  type BotRun,
  type Wake,
} from "./bots";
import {
  type DateRange,
  type TrendBucket,
  type TrendPoint,
  stamp,
  summary,
  trendBuckets,
} from "./stats";
// Per-bot statistics, computed client-side from the bot reads (each the latest
// BOT_READ_LIMIT records). Every figure is over records whose own time falls in
// [range.start, range.end): run start, wake creation, inbound message, task
// event or bot event. Live runs have no runtime yet and are counted apart.
const DAY = 86400000;
export type BotData = {
  runs: BotRun[];
  wakes: Wake[];
  events: BotEvent[];
  activity: Activity;
};
/** Mean and population SD, null without samples (same rule as Stats trends). */
export function meanSd(values: number[]) {
  const n = values.length;
  if (!n) return { n, mean: null, sd: null };
  const mean = values.reduce((a, b) => a + b, 0) / n;
  const sd = Math.sqrt(values.reduce((a, v) => a + (v - mean) ** 2, 0) / n);
  return { n, mean, sd };
}
/** One TrendPoint per bucket from timed samples; empty buckets are gaps. */
export function trendOf(
  samples: { at: number; value: number }[],
  buckets: TrendBucket[],
): TrendPoint[] {
  return buckets.map((b) => {
    const { n, mean, sd } = meanSd(
      samples.filter((s) => s.at >= b.start && s.at < b.end).map((s) => s.value),
    );
    return {
      ...b,
      n,
      unknown: 0,
      mean,
      sd,
      low: mean === null ? null : Math.max(0, mean - sd!),
      high: mean === null ? null : mean + sd!,
    };
  });
}
/** Run outcome from taskq's `stop_reason` (bot::finish). */
export function runOutcome(r: BotRun) {
  if (!r.ended_at) return "live";
  if (r.stop_reason === "exit" && r.exit_code === 0) return "ok";
  if (r.stop_reason === "stop") return "stop";
  if (r.stop_reason === "pause") return "pause";
  if (r.stop_reason === "dispatcher_stopped" || r.stop_reason === "reconciled")
    return "interrupted";
  return "failure";
}
/**
 * Reply latency: a turn opens at the first inbound message (not a reaction or
 * edit) after the previous outbound message, and closes at the next outbound
 * message. Turns opened in range count; open turns are unanswered.
 */
export function replyLatencies(activity: Activity, range: DateRange) {
  const messages = activity.messages
    .slice()
    .sort((a, b) => stamp(a.at) - stamp(b.at) || a.id - b.id);
  const samples: { at: number; value: number }[] = [];
  let open: number | null = null;
  for (const m of messages) {
    const at = stamp(m.at);
    if (m.direction === "in") {
      if (open === null && m.kind !== "reaction" && m.kind !== "edit") open = at;
    } else if (open !== null) {
      if (open >= range.start && open < range.end)
        samples.push({ at: open, value: at - open });
      open = null;
    }
  }
  const unanswered = open !== null && open >= range.start && open < range.end ? 1 : 0;
  return { samples, unanswered };
}
/**
 * Follow-up runs: a run whose prompt delivered a wake created while the
 * previous run of the bot was live (taskq's `during_previous`), i.e. a message
 * no hook took mid-run.
 */
export function followUpRuns(runs: BotRun[], wakes: Wake[]) {
  const ordered = runs.slice().sort((a, b) => a.id - b.id);
  const ids = new Set<number>();
  ordered.forEach((r, i) => {
    const prev = ordered[i - 1];
    if (!prev?.ended_at) return;
    const start = stamp(prev.started_at),
      end = stamp(prev.ended_at);
    if (
      wakes.some((w) => {
        const created = stamp(w.created_at);
        return (
          w.run_id === r.id &&
          w.delivered_via === "prompt" &&
          created >= start &&
          created <= end
        );
      })
    )
      ids.add(r.id);
  });
  return ids;
}
export function botStats(d: BotData, range: DateRange, botActor: string) {
  const inRange = (s: string | null) => {
    const t = stamp(s);
    return t >= range.start && t < range.end;
  };
  const days = Math.max(0, Math.round((range.end - range.start) / DAY));
  const runs = d.runs.filter((r) => inRange(r.started_at));
  const finished = runs.filter((r) => r.ended_at);
  const durations = finished.map((r) => ({
    at: stamp(r.started_at),
    value: stamp(r.ended_at) - stamp(r.started_at),
  }));
  const trend = trendBuckets(range.start, range.end);
  const perDay = Array.from({ length: days }, (_, i) => ({
    start: range.start + i * DAY,
    count: 0,
  }));
  for (const r of runs) {
    const i = Math.floor((stamp(r.started_at) - range.start) / DAY);
    if (perDay[i]) perDay[i].count++;
  }
  const wakes = d.wakes.filter((w) => inRange(w.created_at));
  const wakesByKind: Record<string, number> = { message: 0, button: 0, wake: 0 };
  for (const w of wakes) wakesByKind[w.kind] = (wakesByKind[w.kind] ?? 0) + 1;
  const latency = replyLatencies(d.activity, range);
  const outcomes = { ok: 0, failure: 0, stop: 0, pause: 0, interrupted: 0, live: 0 };
  for (const r of runs) outcomes[runOutcome(r)]++;
  const events = d.events.filter((e) => inRange(e.at));
  const actions = d.activity.task_actions.filter((a) => inRange(a.at));
  const byBot = actions.filter((a) => a.actor === botActor).length;
  const byHuman = actions.filter((a) => a.actor === "human").length;
  const actionDays = perDay.map((p) => ({ start: p.start, bot: 0, human: 0 }));
  for (const a of actions) {
    const i = Math.floor((stamp(a.at) - range.start) / DAY);
    if (!actionDays[i]) continue;
    if (a.actor === botActor) actionDays[i].bot++;
    else if (a.actor === "human") actionDays[i].human++;
  }
  const followUps = followUpRuns(d.runs, d.wakes);
  return {
    days,
    runs: { total: runs.length, live: runs.length - finished.length },
    runsPerDay: perDay,
    runtime: {
      ...meanSd(durations.map((s) => s.value)),
      summary: summary(durations.map((s) => s.value)),
      weekly: trend.weekly,
      trend: trendOf(durations, trend.buckets),
    },
    wakesByKind,
    wakes: wakes.length,
    latency: {
      ...meanSd(latency.samples.map((s) => s.value)),
      summary: summary(latency.samples.map((s) => s.value)),
      unanswered: latency.unanswered,
      trend: trendOf(latency.samples, trend.buckets),
    },
    deliveries: {
      midRun: wakes.filter((w) => w.delivered_via === "hook").length,
      prompt: wakes.filter((w) => w.delivered_via === "prompt").length,
      undelivered: wakes.filter((w) => w.run_id === null).length,
      followUpRuns: runs.filter((r) => followUps.has(r.id)).length,
    },
    sessions: {
      resumed: runs.filter((r) => r.mode === "resume").length,
      fresh: runs.filter((r) => r.mode === "fresh").length,
    },
    outcomes: {
      ...outcomes,
      ownerPauses: events.filter((e) => e.kind === "pause").length,
      attention: events.filter((e) => e.kind === "attention").length,
      securityDrops: events.filter((e) => e.kind === "security_drop").length,
    },
    taskActions: {
      bot: byBot,
      human: byHuman,
      share: byBot + byHuman ? byBot / (byBot + byHuman) : null,
      perDay: actionDays,
    },
    /** Reads that hit the cap: older records in range may be missing. */
    truncated: [
      d.runs.length >= BOT_READ_LIMIT && "runs",
      d.wakes.length >= BOT_READ_LIMIT && "wakes",
      d.events.length >= BOT_READ_LIMIT && "events",
      d.activity.messages.length >= BOT_READ_LIMIT && "messages",
      d.activity.task_actions.length >= BOT_READ_LIMIT && "task actions",
    ].filter((x): x is string => !!x),
  };
}
export type BotStats = ReturnType<typeof botStats>;
