import { type Event, type Run, type Task, matchesTask } from "./model";
/** The run fields Stats reads; `/api/stats/history` returns only these. */
export type StatsRun = Pick<
  Run,
  "id" | "task_id" | "agent" | "started_at" | "ended_at"
>;
/**
 * `routines[id]` is true for a native Routine occurrence (scheduled or manual
 * trigger, whatever the parent routine's current state), false for any other
 * task. A missing entry means the type is unknown.
 */
export type History = {
  runs: StatsRun[];
  events: Record<number, Event[]>;
  routines: Record<number, boolean>;
  at: number;
};
export type TaskType = "normal" | "routine" | "unknown";
export const taskTypes = ["all", "normal", "routine"] as const;
export type TypeScope = (typeof taskTypes)[number];
export const typeLabels: Record<TaskType, string> = {
  normal: "Normal",
  routine: "Routine",
  unknown: "Unknown type",
};
export const scopeLabels: Record<TypeScope, string> = {
  all: "All tasks",
  normal: "Normal tasks",
  routine: "Routine tasks",
};
export function typeScope(p: URLSearchParams): TypeScope {
  const t = p.get("type");
  return t === "normal" || t === "routine" ? t : "all";
}
export const typeOf = (h: History, id: number): TaskType =>
  id in h.routines ? (h.routines[id] ? "routine" : "normal") : "unknown";
export const stamp = (s: string | null) => (s ? Date.parse(s) : NaN);
const DAY = 86400000;
export type DateRange = { start: number; end: number; error?: string };
/** UTC calendar dates, start inclusive and end exclusive. Custom end date is inclusive. */
export function dateRange(p: URLSearchParams, now: number): DateRange {
  const today = Math.floor(now / DAY) * DAY;
  if (p.has("start") || p.has("end") || p.get("days") === "custom") {
    const parse = (key: string) => {
      const value = p.get(key) || "";
      const n = /^\d{4}-\d{2}-\d{2}$/.test(value)
        ? stamp(value + "T00:00:00Z")
        : NaN;
      return Number.isFinite(n) &&
        new Date(n).toISOString().slice(0, 10) === value
        ? n
        : NaN;
    };
    const start = parse("start"),
      last = parse("end");
    if (
      !Number.isFinite(start) ||
      !Number.isFinite(last) ||
      start > last ||
      last > today
    )
      return {
        start: NaN,
        end: NaN,
        error:
          "Choose valid UTC dates, start on or before end, and end no later than today.",
      };
    return { start, end: last + DAY };
  }
  const days = ["7", "30", "90"].includes(p.get("days") || "")
    ? Number(p.get("days"))
    : 7;
  return { start: today - (days - 1) * DAY, end: today + DAY };
}
/** Candidates include old tasks: creation date is not completion date. */
export function completionCandidates(
  tasks: Task[],
  p: URLSearchParams,
  range: DateRange,
) {
  return tasks.filter(
    (t) =>
      matchesTask(t, p) &&
      t.state === "done" &&
      stamp(t.created_at) < range.end,
  );
}
/** Latest actual done transition, NaN when missing or invalid. */
export function completion(task: Task, events: Event[] | undefined, at: number) {
  const states = events
    ?.slice()
    .sort((a, b) => a.id - b.id)
    .filter((e) => e.kind === "state" && e.from_value !== e.to_value);
  const last = states?.at(-1);
  const end = last?.to_value === "done" ? stamp(last.at) : NaN;
  const start = stamp(task.created_at);
  return Number.isFinite(start) && end >= start && end <= at ? end : NaN;
}
/** `GET /api/stats/history`: every task's actual state transitions, run starts and runs. */
type StatsHistory = {
  events: (Pick<Event, "id" | "at" | "kind" | "from_value" | "to_value"> & {
    task_id: number;
  })[];
  routines: number[];
  runs: StatsRun[];
};
/**
 * One request whatever the task or run count (`GET /api/stats/history`), never
 * a read per task. Every `tasks` and `open` entry gets its events and its type
 * from that response.
 */
export async function loadHistory(
  tasks: Task[],
  base: string,
  signal: AbortSignal,
  { open = [] }: { open?: Task[] } = {},
): Promise<History> {
  async function get<T>(path: string): Promise<T> {
    const r = await fetch(base + path, { signal });
    if (!r.ok) throw Error(`${path}: HTTP ${r.status}`);
    return r.json();
  }
  const stats = await get<StatsHistory>("/api/stats/history");
  const byTask: Record<number, Event[]> = {};
  for (const { task_id, ...e } of stats.events)
    (byTask[task_id] ??= []).push({ ...e, actor: "", body: null });
  const routine = new Set(stats.routines);
  const events: Record<number, Event[]> = {};
  const routines: Record<number, boolean> = {};
  for (const t of tasks) events[t.id] = byTask[t.id] ?? [];
  for (const t of [...tasks, ...open]) routines[t.id] = routine.has(t.id);
  return { runs: stats.runs, events, routines, at: Date.now() };
}
export type Metric = "runCount" | "escalations" | "elapsed" | "runtime";
export const metrics: { key: Metric; label: string; unit: string }[] = [
  { key: "runCount", label: "Runs per task", unit: "runs" },
  { key: "escalations", label: "Escalations per task", unit: "escalations" },
  { key: "elapsed", label: "Wall-clock elapsed time", unit: "duration" },
  { key: "runtime", label: "Active agent runtime", unit: "duration" },
];
export type TaskMetric = {
  task: Task;
  type: TaskType;
  agents: string;
  completedAt: number;
} & Record<Metric, number | null>;
export function summary(values: (number | null)[]) {
  const known = values
    .filter((n): n is number => n !== null)
    .sort((a, b) => a - b);
  const n = known.length;
  return {
    n,
    unknown: values.length - n,
    median: n
      ? (known[Math.floor((n - 1) / 2)] + known[Math.floor(n / 2)]) / 2
      : null,
    p90: n ? known[Math.ceil(n * 0.9) - 1] : null,
    max: n ? known[n - 1] : null,
  };
}
export function histogram(values: (number | null)[], metric: Metric) {
  const count = metric === "runCount" || metric === "escalations";
  const edges = count
    ? [0, 1, 2, 3, 4, 5, 10]
    : [0, 1, 60000, 300000, 900000, 3600000, 14400000, 86400000, 604800000];
  const labels = count
    ? ["0", "1", "2", "3", "4", "5–9", "10+"]
    : [
        "0",
        "<1 min",
        "1–<5 min",
        "5–<15 min",
        "15–<60 min",
        "1–<4 h",
        "4–<24 h",
        "1–<7 d",
        "7 d+",
      ];
  return edges.map((edge, i) => ({
    label: labels[i],
    count: values.filter(
      (v) => v !== null && v >= edge && v < (edges[i + 1] ?? Infinity),
    ).length,
  }));
}
export function breakdown(rows: TaskMetric[], key: (r: TaskMetric) => string) {
  const labels = [...new Set(rows.map(key))].sort();
  return labels.map((label) => {
    const members = rows.filter((r) => key(r) === label);
    return {
      label,
      tasks: members.length,
      metrics: Object.fromEntries(
        metrics.map((m) => [m.key, summary(members.map((r) => r[m.key]))]),
      ) as Record<Metric, ReturnType<typeof summary>>,
      histograms: Object.fromEntries(
        metrics.map((m) => [
          m.key,
          histogram(
            members.map((r) => r[m.key]),
            m.key,
          ),
        ]),
      ) as Record<Metric, ReturnType<typeof histogram>>,
    };
  });
}
export type Breakdown = ReturnType<typeof breakdown>[number];
/** Per UTC day completion counts for each group index, from `start` for `days`. */
export function dailyCompletions(
  rows: TaskMetric[],
  start: number,
  days: number,
  groups: number,
  groupOf: (r: TaskMetric) => number,
) {
  const buckets = Array.from({ length: days }, (_, i) => ({
    start: start + i * DAY,
    counts: Array<number>(groups).fill(0),
  }));
  for (const r of rows) {
    const day = Math.floor((r.completedAt - start) / DAY);
    if (day >= 0 && day < days) buckets[day].counts[groupOf(r)]++;
  }
  return buckets;
}
/** Ranges up to this many UTC days trend daily; longer ranges trend weekly. */
export const DAILY_TREND_MAX_DAYS = 31;
export type TrendBucket = { start: number; end: number };
/**
 * Trend buckets over [start, end): one per UTC day up to
 * DAILY_TREND_MAX_DAYS days, otherwise seven-day weeks counted from the range
 * start (the last week ends at the range end and may be shorter).
 */
export function trendBuckets(start: number, end: number) {
  const days = Math.max(0, Math.round((end - start) / DAY));
  const size = days > DAILY_TREND_MAX_DAYS ? 7 * DAY : DAY;
  const buckets: TrendBucket[] = [];
  for (let b = start; b < end; b += size)
    buckets.push({ start: b, end: Math.min(b + size, end) });
  return { weekly: size > DAY, buckets };
}
export type TrendPoint = TrendBucket & {
  /** Known samples; unknown metrics are excluded and counted separately. */
  n: number;
  unknown: number;
  /** Null when n is 0: a gap, never zero. */
  mean: number | null;
  /** Population SD (divides by n), so a single sample has SD 0. */
  sd: number | null;
  /** Mean - SD clipped at 0, and mean + SD. */
  low: number | null;
  high: number | null;
};
/**
 * Mean and population SD of each metric per completion bucket and group:
 * `[metric][group][bucket]`. Rows completed outside the buckets are ignored.
 */
export function trends(
  rows: TaskMetric[],
  buckets: TrendBucket[],
  groups: number,
  groupOf: (r: TaskMetric) => number,
) {
  const at = (t: number) =>
    buckets.findIndex((b) => t >= b.start && t < b.end);
  const members = Array.from({ length: groups }, () =>
    buckets.map(() => [] as TaskMetric[]),
  );
  for (const r of rows) {
    const i = at(r.completedAt);
    if (i >= 0) members[groupOf(r)][i].push(r);
  }
  return Object.fromEntries(
    metrics.map((m) => [
      m.key,
      members.map((series) =>
        series.map((cell, i): TrendPoint => {
          const known = cell
            .map((r) => r[m.key])
            .filter((v): v is number => v !== null);
          const n = known.length;
          const mean = n ? known.reduce((a, b) => a + b, 0) / n : null;
          const sd =
            mean === null
              ? null
              : Math.sqrt(
                  known.reduce((a, v) => a + (v - mean) ** 2, 0) / n,
                );
          return {
            ...buckets[i],
            n,
            unknown: cell.length - n,
            mean,
            sd,
            low: mean === null ? null : Math.max(0, mean - sd!),
            high: mean === null ? null : mean + sd!,
          };
        }),
      ),
    ]),
  ) as Record<Metric, TrendPoint[][]>;
}
/**
 * Keeps at most `max` series: the largest `max - 1` groups by task count
 * (ties by original order) plus one "Other" for the rest. Returns the kept
 * labels and each original group's series index.
 */
export function collapseGroups(
  groups: { label: string; tasks: number }[],
  max = 6,
) {
  const order = groups
    .map((g, i) => ({ ...g, i }))
    .sort((a, b) => b.tasks - a.tasks || a.i - b.i);
  if (groups.length <= max)
    return { labels: groups.map((g) => g.label), index: groups.map((_, i) => i) };
  const kept = order.slice(0, max - 1);
  const index = groups.map(() => max - 1);
  kept.forEach((g, k) => (index[g.i] = k));
  return {
    labels: [
      ...kept.map((g) => g.label),
      `Other (${groups.length - kept.length} groups)`,
    ],
    index,
  };
}
export function calculateStats(
  tasks: Task[],
  history: History,
  p: URLSearchParams,
) {
  const range = dateRange(p, history.at);
  const scope = typeScope(p);
  const counts = () => ({ normal: 0, routine: 0, unknown: 0 });
  const unknownCompletionTypes = counts();
  const allRows: TaskMetric[] = [];
  for (const task of completionCandidates(tasks, p, range)) {
    const ordered = history.events[task.id]
      ?.slice()
      .sort((a, b) => a.id - b.id);
    // Ignore repeated done snapshots; completion must be an actual transition.
    const end = completion(task, ordered, history.at);
    const start = stamp(task.created_at);
    const type = typeOf(history, task.id);
    if (!Number.isFinite(end)) {
      unknownCompletionTypes[type]++;
      continue;
    }
    if (end < range.start || end >= range.end) continue;
    const runs = history.runs.filter((r) => r.task_id === task.id);
    const complete =
      (ordered?.filter((e) => e.kind === "run_start").length ?? 0) <=
        runs.length;
    const valid = runs.every(
      (r) =>
        Number.isFinite(stamp(r.started_at)) &&
        stamp(r.started_at) >= start &&
        r.ended_at !== null &&
        stamp(r.ended_at) >= stamp(r.started_at) &&
        stamp(r.ended_at) <= history.at,
    );
    const agents = [
      ...new Set(runs.map((r) => r.agent || "Unknown agent")),
    ].sort();
    allRows.push({
      task,
      type,
      completedAt: end,
      elapsed: end - start,
      runCount: complete ? runs.length : null,
      runtime:
        complete && valid
          ? runs.reduce(
              (n, r) => n + stamp(r.ended_at) - stamp(r.started_at),
              0,
            )
          : null,
      escalations: ordered!.filter(
        (e) =>
          e.kind === "state" &&
          e.to_value === "escalated" &&
          e.from_value !== "escalated",
      ).length,
      agents: !complete
        ? "Unknown / partial"
        : agents.length > 1
          ? `Mixed: ${agents.join(" + ")}`
          : agents[0] || "No recorded agent",
    });
  }
  const types = counts();
  for (const r of allRows) types[r.type]++;
  const openTypes = counts();
  for (const t of tasks)
    if (matchesTask(t, p) && t.state !== "done")
      openTypes[typeOf(history, t.id)]++;
  const total = (c: Record<TaskType, number>) =>
    c.normal + c.routine + c.unknown;
  const pick = (c: Record<TaskType, number>) =>
    scope === "all" ? total(c) : c[scope];
  // Normal and Routine scopes exclude tasks whose type could not be read.
  const rows =
    scope === "all" ? allRows : allRows.filter((r) => r.type === scope);
  return {
    ...range,
    scope,
    rows,
    types,
    openTypes,
    unknownCompletionTypes,
    unknownCompletion: pick(unknownCompletionTypes),
    open: pick(openTypes),
    excludedUnknownType:
      scope === "all"
        ? 0
        : types.unknown + openTypes.unknown + unknownCompletionTypes.unknown,
    taskTypes: breakdown(rows, (r) => typeLabels[r.type]),
    projects: breakdown(rows, (r) => r.task.project),
    agents: breakdown(rows, (r) => r.agents),
    total: breakdown(rows, () => scopeLabels[scope]),
  };
}
