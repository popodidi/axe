import type { DateRange, TrendPoint } from "./stats";

/** One row of a breakdown from `GET /api/stats/cost`. */
export type CostRow = {
  key: string;
  runs: number;
  known_runs: number;
  unknown_runs: number;
  cost: number;
  input: number;
  output: number;
  thinking: number;
  cache_read: number;
  cache_write: number;
};

/** One period of the time series; times are epoch seconds. */
export type CostBucket = Omit<CostRow, "key"> & {
  start: number;
  end: number;
  /** Estimated cost per key; unknown runs add nothing here. */
  splits: Record<string, Record<string, number>>;
};
export type CostSeries = {
  step_days: number;
  /** When the earliest run with usage ended; older periods have no data. */
  first_known: number | null;
  buckets: CostBucket[];
};

export type CostStats = {
  since: number;
  until: number;
  total: Omit<CostRow, "key">;
  token_types: { key: string; tokens: number }[];
  breakdowns: Record<string, CostRow[]>;
  coverage: {
    runs: number;
    known: number;
    unknown: number;
    unknown_reasons: Record<string, number>;
    by_agent: CostRow[];
  };
  finished: { tasks: number; cost: number; cost_per_task: number | null };
  /** Present when requested with `step_days` on a server that supports it. */
  series?: CostSeries;
  /** Present when requested with `options`: each filter's values in the period. */
  filter_options?: Record<string, string[]>;
};

const CONTEXT_ORDER = [
  "under 50k",
  "50k-100k",
  "100k-200k",
  "200k-500k",
  "500k and over",
  "unknown",
];

export type CostGroup = { key: string; title: string; order?: "time" | "context" };
export const costSections: { title: string; groups: CostGroup[] }[] = [
  {
    title: "Who",
    groups: [
      { key: "project", title: "Project" },
      { key: "source", title: "Bot, routine or one-off task" },
      { key: "bot", title: "Bot" },
      { key: "routine", title: "Routine" },
    ],
  },
  {
    title: "What",
    groups: [
      { key: "workload", title: "Workload" },
      { key: "wake", title: "Bot wake reason" },
    ],
  },
  {
    title: "How",
    groups: [
      { key: "agent", title: "Agent" },
      { key: "model", title: "Model" },
      { key: "mode", title: "Fresh or resumed" },
    ],
  },
  {
    title: "Why",
    groups: [
      { key: "cache", title: "Cache at run start" },
      { key: "context", title: "Context size at run end", order: "context" },
    ],
  },
  { title: "Outcome", groups: [{ key: "outcome", title: "Task outcome" }] },
  {
    title: "Time",
    groups: [
      { key: "day", title: "Day (UTC)", order: "time" },
      { key: "week", title: "Week starting Monday (UTC)", order: "time" },
    ],
  },
];

/** Filters with their API name and URL key. `bot` and `routine` already open
 * details in other views, so the Cost page keeps its own keys for those. */
export const costFilters = [
  { name: "project", param: "project", label: "Project" },
  { name: "source", param: "source", label: "Source" },
  { name: "bot", param: "cost_bot", label: "Bot" },
  { name: "routine", param: "cost_routine", label: "Routine" },
  { name: "agent", param: "agent", label: "Agent" },
  { name: "model", param: "model", label: "Model" },
  { name: "workload", param: "workload", label: "Workload" },
] as const;

export const DEFAULT_BREAKDOWN = "project";

/** Every table's dimension, each selectable as the over-time chart's stack. */
export const costBreakdowns = costSections.flatMap((s) => s.groups);

export const breakdownOf = (params: URLSearchParams) => {
  const chosen = params.get("breakdown");
  return costBreakdowns.some((g) => g.key === chosen) && chosen
    ? chosen
    : DEFAULT_BREAKDOWN;
};

export const breakdownTitle = (key: string) => {
  const title = costBreakdowns.find((g) => g.key === key)?.title ?? key;
  return `Cost by ${title[0].toLowerCase()}${title.slice(1)}`;
};

/** The URL's filters and breakdown, as the query the one aggregate request adds. */
export function costScope(params: URLSearchParams) {
  const out = new URLSearchParams();
  for (const f of costFilters) {
    const value = params.get(f.param);
    if (value) out.set(f.name, value);
  }
  const breakdown = breakdownOf(params);
  if (breakdown !== DEFAULT_BREAKDOWN) out.set("breakdown", breakdown);
  out.set("options", "1");
  return out;
}

const seconds = (ms: number) =>
  new Date(ms).toISOString().slice(0, 19) + "Z";

/** The Cost view's only request: one aggregate, whatever the run count. */
export const costPath = (
  range: DateRange,
  stepDays?: number,
  scope?: URLSearchParams,
) =>
  `/api/stats/cost?since=${encodeURIComponent(seconds(range.start))}` +
  `&until=${encodeURIComponent(seconds(range.end))}` +
  (stepDays ? `&step_days=${stepDays}` : "") +
  (scope ? `&${scope}` : "");

export function orderRows(rows: CostRow[], order?: CostGroup["order"]) {
  const out = [...rows];
  if (order === "time") return out.sort((a, b) => b.key.localeCompare(a.key));
  if (order === "context")
    return out.sort(
      (a, b) => CONTEXT_ORDER.indexOf(a.key) - CONTEXT_ORDER.indexOf(b.key),
    );
  return out.sort((a, b) => b.cost - a.cost || a.key.localeCompare(b.key));
}

export const usd = (n: number) =>
  n > 0 && n < 0.01 ? "<$0.01" : `$${n.toFixed(2)}`;

export function tokenCount(n: number) {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
  return String(n);
}

export const share = (part: number, whole: number) =>
  whole > 0 ? `${Math.round((part / whole) * 100)}%` : "0%";

const reasonLabels: Record<string, string> = {
  no_result: "no result event (killed, crashed, or an agent that reports none)",
  no_model_usage: "result without per-model usage",
  log_missing: "log no longer exists",
  log_reused: "log overwritten by a later run",
  unreadable: "log unreadable",
  not_recorded: "not recorded yet",
  counter_reset: "session counter went backwards",
};
export const reasonLabel = (reason: string) => reasonLabels[reason] ?? reason;

/** Why a period's figure is incomplete, or null when it is complete. */
export function periodGap(b: CostBucket, firstKnown: number | null) {
  if (b.unknown_runs > 0)
    return b.known_runs > 0
      ? `${b.unknown_runs} of ${b.runs} runs unknown, figure is a lower bound`
      : `all ${b.runs} runs unknown, no figure`;
  if (b.known_runs === 0 && (firstKnown === null || b.end <= firstKnown))
    return "before usage data begins, no figure";
  return null;
}

export const bucketTokens = (b: CostRow | CostBucket) =>
  b.input + b.output + b.cache_read + b.cache_write;

/** A plotted total: the period's known sum, or a gap when nothing is known. */
export function totalSeries(
  series: CostSeries,
  value: (b: CostBucket) => number,
): TrendPoint[] {
  return series.buckets.map((b) => {
    const v = b.known_runs > 0 ? value(b) : null;
    return {
      start: b.start * 1000,
      end: b.end * 1000,
      n: b.known_runs,
      unknown: b.unknown_runs,
      mean: v,
      sd: v === null ? null : 0,
      low: v,
      high: v,
    };
  });
}

const barLabel = (b: CostBucket) =>
  new Date(b.start * 1000).toISOString().slice(5, 10);

/** Keeps every series color distinct; the remainder folds into "other". */
const MAX_SPLIT_GROUPS = 5;

export type StackedBars = {
  groups: string[];
  buckets: { label: string; counts: number[]; gap: string | null }[];
};

/** Stacked cost per period by one dimension, largest groups first. */
export function splitBars(series: CostSeries, split: string): StackedBars {
  const sum = new Map<string, number>();
  for (const b of series.buckets)
    for (const [k, v] of Object.entries(b.splits[split] ?? {}))
      sum.set(k, (sum.get(k) ?? 0) + v);
  const keys = [...sum.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([k]) => k);
  const fold = keys.length > MAX_SPLIT_GROUPS;
  // A real group called "other" (outcome has one) joins the fold, not a twin series.
  const named = fold
    ? keys.filter((k) => k !== "other").slice(0, MAX_SPLIT_GROUPS)
    : keys;
  return {
    groups: fold ? [...named, "other"] : named,
    buckets: series.buckets.map((b) => {
      const row = b.splits[split] ?? {};
      const counts = named.map((k) => row[k] ?? 0);
      if (fold)
        counts.push(
          Object.entries(row)
            .filter(([k]) => !named.includes(k))
            .reduce((n, [, v]) => n + v, 0),
        );
      return { label: barLabel(b), counts, gap: periodGap(b, series.first_known) };
    }),
  };
}

const TOKEN_FIELDS = [
  ["cache read", "cache_read"],
  ["cache write", "cache_write"],
  ["input", "input"],
  ["output", "output"],
] as const;

/** Stacked tokens per period by type; output includes thinking. */
export function tokenBars(series: CostSeries): StackedBars {
  return {
    groups: TOKEN_FIELDS.map(([label]) => label),
    buckets: series.buckets.map((b) => ({
      label: barLabel(b),
      counts: TOKEN_FIELDS.map(([, f]) => b[f]),
      gap: periodGap(b, series.first_known),
    })),
  };
}

/** Whole-unit axis labels for narrow chart gutters. */
export const usdTick = (n: number) => `$${n < 10 && n % 1 ? n.toFixed(1) : Math.round(n)}`;

/** Token axis label without a trailing ".0", so it clears the axis title. */
export const tokenTick = (n: number) => tokenCount(n).replace(/\.0(?=[kMB]$)/, "");
