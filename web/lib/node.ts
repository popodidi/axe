// The Node view's model: ranges, formatting and chart geometry over
// `GET /api/node/metrics` (crates/axe/src/node_metrics.rs).
import type { NodePoint, NodeRuns, NodeSample, NodeSeries } from "./api";

export type { NodePoint, NodeRuns, NodeSample, NodeSeries };

/** The server's ranges (`node_metrics::RANGES`); 1h and 6h are raw samples, longer ranges rollups. */
export const NODE_RANGES = ["1h", "6h", "24h", "7d", "30d"] as const;
export type NodeRange = (typeof NODE_RANGES)[number];
export const nodeRangeOptions: { value: NodeRange; label: string }[] = [
  { value: "1h", label: "Last hour" },
  { value: "6h", label: "Last 6 hours" },
  { value: "24h", label: "Last 24 hours" },
  { value: "7d", label: "Last 7 days" },
  { value: "30d", label: "Last 30 days" },
];
export const nodeRange = (p: URLSearchParams): NodeRange =>
  (NODE_RANGES as readonly string[]).includes(p.get("range") || "")
    ? (p.get("range") as NodeRange)
    : "6h";
export const nodePath = (range: NodeRange) => `/api/node/metrics?range=${range}`;

/**
 * Run kinds in stacking order, bottom first, with their label and series
 * slot (`stats-charts.module.css` s0-s3: teal, blue, purple, amber). The
 * order teal, purple, amber, blue keeps every adjacent pair apart under
 * color-vision deficiency in both themes (dataviz validate_palette.js).
 */
export const RUN_KINDS = [
  { key: "task", label: "Task runs", slot: 0 },
  { key: "bot", label: "Bot runs", slot: 2 },
  { key: "service", label: "Services", slot: 3 },
  { key: "scheduled", label: "Scheduled runs", slot: 1 },
] as const;
export type RunKind = (typeof RUN_KINDS)[number]["key"];

const GIB = 2 ** 30;
export const gib = (bytes: number | null | undefined) =>
  bytes === null || bytes === undefined ? "–" : `${(bytes / GIB).toFixed(1)} GiB`;
export const pct = (n: number | null | undefined) =>
  n === null || n === undefined ? "–" : `${n.toFixed(n < 10 ? 1 : 0)}%`;
export const load = (n: number | null | undefined) =>
  n === null || n === undefined ? "–" : n.toFixed(2);
export const count = (n: number) =>
  Number.isInteger(n) ? String(n) : n.toFixed(1);

export const runTotal = (by: Record<string, number> | undefined) =>
  Object.values(by ?? {}).reduce((a, b) => a + b, 0);

/** Names of one run kind, largest first: `harvis 2, tiktape 1`. */
export function runNames(by: Record<string, number> | undefined): [string, number][] {
  return Object.entries(by ?? {})
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
}

/** A sample older than three intervals (and a minute) is stale: the node may be down. */
export function stale(s: NodeSeries): boolean {
  if (!s.current) return true;
  return s.now - s.current.at > Math.max(60, 3 * s.sample_seconds);
}

/** Seconds as a short age: 12s, 4m, 3h, 2d. */
export function age(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/**
 * Index runs of points without an outage between them: a hole wider than
 * 2.5 steps (the node was down) splits lines and areas, never drawn as zero.
 * One empty bucket is not an outage: the node's interval drifts a little
 * past `sample_seconds`, so raw 15 s buckets now and then miss a sample.
 */
export function segments(points: { t: number }[], step: number): number[][] {
  const out: number[][] = [];
  points.forEach((p, i) => {
    const last = out.at(-1);
    if (last && p.t - points[last.at(-1)!].t <= step * 2.5) last.push(i);
    else out.push([i]);
  });
  return out;
}

/** Per point, the cumulative top of each run kind (avg), bottom kind first. */
export function stackRuns(points: NodePoint[]): number[][] {
  return points.map((p) => {
    let y = 0;
    return RUN_KINDS.map((k) => (y += p[`${k.key}_avg`]));
  });
}

/** Nice tick step (seconds) for a time axis of `span` seconds. */
export function timeTicks(from: number, to: number): number[] {
  const span = to - from;
  const steps = [600, 1800, 3600, 3 * 3600, 4 * 3600, 6 * 3600, 86400, 2 * 86400, 5 * 86400, 7 * 86400];
  const step = steps.find((s) => span / s <= 7) ?? steps.at(-1)!;
  const out: number[] = [];
  for (let t = Math.ceil(from / step) * step; t < to; t += step) out.push(t);
  return out;
}

/** UTC clock time within a day, the date beyond. */
export function timeLabel(t: number, span: number): string {
  const iso = new Date(t * 1000).toISOString();
  return span <= 86400 * 1.5 ? iso.slice(11, 16) : iso.slice(5, 10);
}

/** UTC date and time of a point, for readouts. */
export const stamp = (t: number) =>
  new Date(t * 1000).toISOString().slice(0, 16).replace("T", " ") + " UTC";

/** A y-axis maximum at or above `n`: 1, 2 or 5 times a power of ten. */
export function niceMax(n: number): number {
  if (!(n > 0)) return 1;
  const p = 10 ** Math.floor(Math.log10(n));
  return ([1, 2, 5, 10].find((m) => m * p >= n) ?? 10) * p;
}
