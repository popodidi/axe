import { describe, expect, it } from "vitest";
import {
  breakdownOf,
  breakdownTitle,
  bucketTokens,
  costBreakdowns,
  costFilters,
  costPath,
  costScope,
  orderRows,
  periodGap,
  reasonLabel,
  share,
  splitBars,
  tokenBars,
  tokenCount,
  tokenTick,
  totalSeries,
  usd,
  type CostBucket,
  type CostRow,
  type CostSeries,
} from "./cost";

const row = (key: string, cost: number): CostRow => ({
  key,
  cost,
  runs: 1,
  known_runs: 1,
  unknown_runs: 0,
  input: 0,
  output: 0,
  thinking: 0,
  cache_read: 0,
  cache_write: 0,
});

describe("cost view helpers", () => {
  it("asks for one aggregate with whole-second bounds", () => {
    const start = Date.UTC(2026, 10, 1);
    expect(costPath({ start, end: start + 86400000 })).toBe(
      "/api/stats/cost?since=2026-11-01T00%3A00%3A00Z&until=2026-11-02T00%3A00%3A00Z",
    );
  });
  it("orders by cost, time or context bucket", () => {
    const keys = (r: CostRow[]) => r.map((x) => x.key);
    expect(keys(orderRows([row("a", 1), row("b", 5)]))).toEqual(["b", "a"]);
    expect(
      keys(orderRows([row("2026-11-01", 9), row("2026-11-02", 1)], "time")),
    ).toEqual(["2026-11-02", "2026-11-01"]);
    expect(
      keys(
        orderRows(
          [row("unknown", 9), row("500k and over", 1), row("under 50k", 2)],
          "context",
        ),
      ),
    ).toEqual(["under 50k", "500k and over", "unknown"]);
  });
  it("formats money, tokens and shares", () => {
    expect(usd(0)).toBe("$0.00");
    expect(usd(0.004)).toBe("<$0.01");
    expect(usd(12.345)).toBe("$12.35");
    expect(tokenCount(950)).toBe("950");
    expect(tokenCount(12_300)).toBe("12.3k");
    expect(tokenCount(4_500_000)).toBe("4.5M");
    expect(tokenCount(2_000_000_000)).toBe("2.0B");
    expect(share(1, 4)).toBe("25%");
    expect(share(1, 0)).toBe("0%");
    expect(tokenTick(150_000_000)).toBe("150M");
    expect(tokenTick(1_500_000_000)).toBe("1.5B");
  });
  it("labels known and unforeseen unknown reasons", () => {
    expect(reasonLabel("log_missing")).toBe("log no longer exists");
    expect(reasonLabel("new_reason")).toBe("new_reason");
  });
  it("adds the bucket width only when asked", () => {
    const start = Date.UTC(2026, 10, 1);
    expect(costPath({ start, end: start + 86400000 }, 7)).toMatch(/&step_days=7$/);
  });
});

const DAY = 86400;
const bucket = (i: number, over: Partial<CostBucket>): CostBucket => ({
  start: i * DAY,
  end: (i + 1) * DAY,
  runs: 0,
  known_runs: 0,
  unknown_runs: 0,
  cost: 0,
  input: 0,
  output: 0,
  thinking: 0,
  cache_read: 0,
  cache_write: 0,
  splits: {},
  ...over,
});
const series: CostSeries = {
  step_days: 1,
  first_known: 1 * DAY + 3600,
  buckets: [
    bucket(0, {}),
    bucket(1, {
      runs: 3,
      known_runs: 2,
      unknown_runs: 1,
      cost: 6,
      input: 1,
      output: 2,
      cache_read: 30,
      cache_write: 4,
      splits: { project: { a: 4, b: 2 }, model: { m: 6 } },
    }),
    bucket(2, { runs: 2, unknown_runs: 2 }),
    bucket(3, {
      runs: 1,
      known_runs: 1,
      cost: 3,
      input: 10,
      output: 20,
      cache_read: 300,
      cache_write: 40,
      splits: { project: { a: 3 }, model: { m: 3 } },
    }),
  ],
};

describe("cost time series", () => {
  it("buckets sum to the view's totals", () => {
    const cost = totalSeries(series, (b) => b.cost);
    const tokens = totalSeries(series, bucketTokens);
    const sum = (p: typeof cost) => p.reduce((n, x) => n + (x.mean ?? 0), 0);
    expect(sum(cost)).toBe(9);
    expect(sum(tokens)).toBe(1 + 2 + 30 + 4 + 10 + 20 + 300 + 40);
  });
  it("never turns a period without known usage into zero", () => {
    const cost = totalSeries(series, (b) => b.cost);
    expect(cost[0]).toMatchObject({ n: 0, mean: null, high: null });
    expect(cost[2]).toMatchObject({ n: 0, unknown: 2, mean: null });
    expect(cost[1]).toMatchObject({ n: 2, unknown: 1, mean: 6 });
  });
  it("names why a period is incomplete", () => {
    const gap = (b: CostBucket, f = series.first_known) => periodGap(b, f);
    expect(gap(series.buckets[0])).toMatch(/before usage data/);
    expect(gap(series.buckets[1])).toMatch(/1 of 3 runs unknown/);
    expect(gap(series.buckets[2])).toMatch(/all 2 runs unknown/);
    expect(gap(series.buckets[3])).toBeNull();
    expect(gap(bucket(9, {}))).toBeNull();
    expect(gap(bucket(9, {}), null)).toMatch(/before usage data/);
  });
  it("splits sum to each period's total", () => {
    const bars = splitBars(series, "project");
    expect(bars.groups).toEqual(["a", "b"]);
    bars.buckets.forEach((b, i) =>
      expect(b.counts.reduce((n, c) => n + c, 0)).toBe(series.buckets[i].cost),
    );
    expect(bars.buckets[2].gap).toMatch(/all 2 runs/);
    const types = tokenBars(series);
    expect(types.groups).toEqual(["cache read", "cache write", "input", "output"]);
    types.buckets.forEach((b, i) =>
      expect(b.counts.reduce((n, c) => n + c, 0)).toBe(bucketTokens(series.buckets[i])),
    );
  });
  it("folds small groups into other without losing cost", () => {
    const project = Object.fromEntries(
      ["a", "b", "c", "d", "e", "f", "g"].map((k, i) => [k, 7 - i]),
    );
    const bars = splitBars(
      { ...series, buckets: [bucket(0, { cost: 28, known_runs: 1, runs: 1, splits: { project } })] },
      "project",
    );
    expect(bars.groups).toEqual(["a", "b", "c", "d", "e", "other"]);
    expect(bars.buckets[0].counts).toEqual([7, 6, 5, 4, 3, 3]);
  });
});

describe("cost filters and breakdown", () => {
  const start = Date.UTC(2026, 10, 1);
  const range = { start, end: start + 86400000 };
  it("round-trips every filter from the URL into the one request", () => {
    const url = new URLSearchParams(
      costFilters.map((f) => [f.param, `${f.name} value`]),
    );
    url.set("breakdown", "agent");
    url.set("view", "cost");
    url.set("days", "30");
    const sent = new URL(`http://x${costPath(range, 1, costScope(url))}`).searchParams;
    for (const f of costFilters) expect(sent.get(f.name)).toBe(`${f.name} value`);
    expect(sent.get("breakdown")).toBe("agent");
    expect(sent.get("options")).toBe("1");
    expect(sent.has("view")).toBe(false);
    expect(sent.has("days")).toBe(false);
    // The page's URL survives a serialize and parse unchanged.
    expect(new URLSearchParams(url.toString()).get("cost_bot")).toBe("bot value");
  });
  it("keeps bot and routine out of the keys other views read", () => {
    const params = costFilters.map((f) => f.param as string);
    expect(params).not.toContain("bot");
    expect(params).not.toContain("routine");
  });
  it("sends no filters or default breakdown when none are set", () => {
    expect(costScope(new URLSearchParams("view=cost")).toString()).toBe("options=1");
    expect(costPath(range, 1, costScope(new URLSearchParams()))).toMatch(
      /&step_days=1&options=1$/,
    );
    expect(costScope(new URLSearchParams("breakdown=project")).has("breakdown")).toBe(false);
  });
  it("falls back to the default for an unknown breakdown", () => {
    expect(breakdownOf(new URLSearchParams("breakdown=nope"))).toBe("project");
    expect(breakdownOf(new URLSearchParams("breakdown=wake"))).toBe("wake");
  });
  it("offers every breakdown table as a chart stack", () => {
    expect(costBreakdowns.map((g) => g.key)).toEqual(
      expect.arrayContaining(["project", "source", "bot", "routine", "workload",
        "wake", "agent", "model", "mode", "cache", "context", "outcome", "day", "week"]),
    );
    expect(breakdownTitle("project")).toBe("Cost by project");
    expect(breakdownTitle("wake")).toBe("Cost by bot wake reason");
  });
  it("caps series at five plus other and keeps the sum", () => {
    const outcome = { a: 6, b: 5, c: 4, d: 3, e: 2, f: 1, other: 9 };
    const cost = Object.values(outcome).reduce((n, v) => n + v, 0);
    const bars = splitBars(
      { ...series, buckets: [bucket(0, { cost, known_runs: 1, runs: 1, splits: { outcome } })] },
      "outcome",
    );
    expect(bars.groups).toEqual(["a", "b", "c", "d", "e", "other"]);
    expect(bars.buckets[0].counts).toEqual([6, 5, 4, 3, 2, 10]);
    expect(bars.buckets[0].counts.reduce((n, c) => n + c, 0)).toBe(cost);
  });
  it("leaves a real other group alone when nothing folds", () => {
    const outcome = { done: 2, other: 1 };
    const bars = splitBars(
      { ...series, buckets: [bucket(0, { cost: 3, known_runs: 1, runs: 1, splits: { outcome } })] },
      "outcome",
    );
    expect(bars.groups).toEqual(["done", "other"]);
    expect(bars.buckets[0].counts).toEqual([2, 1]);
  });
});
