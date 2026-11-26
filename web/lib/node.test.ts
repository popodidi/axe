import { describe, expect, it } from "vitest";
import {
  age,
  gib,
  niceMax,
  nodePath,
  nodeRange,
  pct,
  runNames,
  segments,
  stackRuns,
  stale,
  timeLabel,
  timeTicks,
  type NodePoint,
  type NodeSeries,
} from "./node";

const point = (t: number, runs: Partial<Record<"task" | "bot" | "service" | "scheduled", number>> = {}): NodePoint => ({
  t,
  n: 1,
  cpu_avg: 10,
  cpu_max: 20,
  mem_used_avg: 1,
  mem_used_max: 1,
  mem_total: 2,
  load1_avg: 0.5,
  load1_max: 0.5,
  disk_free_min: 1,
  task_avg: runs.task ?? 0,
  task_max: runs.task ?? 0,
  bot_avg: runs.bot ?? 0,
  bot_max: runs.bot ?? 0,
  service_avg: runs.service ?? 0,
  service_max: runs.service ?? 0,
  scheduled_avg: runs.scheduled ?? 0,
  scheduled_max: runs.scheduled ?? 0,
});

describe("node view model", () => {
  it("reads the range from the URL, 6h by default", () => {
    expect(nodeRange(new URLSearchParams("range=30d"))).toBe("30d");
    expect(nodeRange(new URLSearchParams("range=2h"))).toBe("6h");
    expect(nodeRange(new URLSearchParams())).toBe("6h");
    expect(nodePath("1h")).toBe("/api/node/metrics?range=1h");
  });

  it("splits lines at outages, not at one empty bucket", () => {
    const pts = [0, 15, 30, 90, 105, 135].map((t) => ({ t }));
    expect(segments(pts, 15)).toEqual([[0, 1, 2], [3, 4, 5]]);
    expect(segments([], 15)).toEqual([]);
  });

  it("stacks run kinds bottom first: task, bot, service, scheduled", () => {
    expect(stackRuns([point(0, { task: 2, bot: 1, service: 3 })])).toEqual([[2, 3, 6, 6]]);
  });

  it("lists names largest first", () => {
    expect(runNames({ b: 1, a: 1, c: 3, z: 0 })).toEqual([
      ["c", 3],
      ["a", 1],
      ["b", 1],
    ]);
  });

  it("marks a sample older than three intervals stale", () => {
    const s = { now: 1000, sample_seconds: 15, current: { at: 950 } } as NodeSeries;
    expect(stale(s)).toBe(false);
    expect(stale({ ...s, current: { ...s.current!, at: 900 } })).toBe(true);
    expect(stale({ ...s, current: null })).toBe(true);
  });

  it("formats values and ages", () => {
    expect(gib(1.5 * 2 ** 30)).toBe("1.5 GiB");
    expect(gib(null)).toBe("–");
    expect(pct(4.25)).toBe("4.3%");
    expect(pct(42.4)).toBe("42%");
    expect(age(59)).toBe("59s");
    expect(age(3 * 3600 + 5)).toBe("3h");
  });

  it("picks clock ticks within a day and dates beyond", () => {
    const day = 1_800_000_000 - (1_800_000_000 % 86400);
    const hour = timeTicks(day, day + 3600);
    expect(hour.length).toBeGreaterThanOrEqual(2);
    expect(hour.length).toBeLessThanOrEqual(7);
    expect(timeLabel(day + 3600, 3600)).toBe("01:00");
    const month = timeTicks(day, day + 30 * 86400);
    expect(month.length).toBeLessThanOrEqual(7);
    expect(timeLabel(day, 30 * 86400)).toMatch(/^\d\d-\d\d$/);
  });

  it("rounds axis maxima up to 1, 2 or 5 times a power of ten", () => {
    expect(niceMax(0)).toBe(1);
    expect(niceMax(3)).toBe(5);
    expect(niceMax(12)).toBe(20);
    expect(niceMax(17 * 2 ** 30)).toBeGreaterThanOrEqual(17 * 2 ** 30);
  });
});
