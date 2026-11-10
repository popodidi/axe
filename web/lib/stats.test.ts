import { describe, it, expect, vi } from "vitest";
import {
  calculateStats,
  dateRange,
  loadHistory,
  dailyCompletions,
  DAILY_TREND_MAX_DAYS,
  collapseGroups,
  trendBuckets,
  trends,
  histogram,
  summary,
  metrics,
  type History,
} from "./stats";
import { type Task, type Run, type Event } from "./model";
const at = Date.parse("2026-11-02T12:00:00Z");
const task = (id = 1, state = "done"): Task => ({
  id,
  state,
  title: "Task",
  project: "harvis",
  detail: "",
  runs: 0,
  created_at: "2026-11-01T00:00:00Z",
  updated_at: "2026-11-02T11:00:00Z",
});
const event = (
  id: number,
  to_value: string,
  from_value = "pending",
  time = "2026-11-01T12:00:00Z",
): Event => ({
  id,
  kind: "state",
  at: time,
  actor: "human",
  from_value,
  to_value,
  body: null,
});
const run = (id = 1, agent = "codex"): Run => ({
  id,
  task_id: 1,
  agent,
  project: "harvis",
  mode: "dispatch",
  started_at: "2026-11-01T01:00:00Z",
  ended_at: "2026-11-01T02:00:00Z",
  exit_code: 0,
  current_activity: "",
  last_activity_at: null,
  log_path: "",
  session_id: null,
});
const calc = (
  tasks = [task()],
  runs = [run()],
  events: Record<number, Event[]> = { 1: [event(1, "done")] },
  query = "",
  routines: Record<number, boolean> = {},
) =>
  calculateStats(
    tasks,
    { at, runs, events, routines },
    new URLSearchParams(query),
  );
describe("completed task distributions", () => {
  it("counts records, escalation transitions and runtime separately from elapsed", () => {
    const s = calc([task()], [run(), run(2)], {
      1: [
        event(1, "escalated"),
        event(2, "escalated", "escalated"),
        { ...event(3, "escalated"), kind: "note" },
        event(4, "pending"),
        event(5, "escalated"),
        event(6, "done"),
      ],
    });
    expect(s.rows[0]).toMatchObject({
      elapsed: 43200000,
      runtime: 7200000,
      runCount: 2,
      escalations: 2,
      agents: "codex",
    });
  });
  it("uses latest completion of currently done tasks, excludes reopened open tasks", () => {
    const history = {
      1: [
        event(1, "done"),
        event(2, "pending", "done"),
        event(3, "done", "pending", "2026-11-02T00:00:00Z"),
      ],
    };
    expect(calc([task()], [], history).rows[0].elapsed).toBe(86400000);
    expect(
      calc([task()], [], history, "start=2026-11-01&end=2026-11-01").rows,
    ).toEqual([]);
    const s = calc([task(1, "pending")], [], history);
    expect(s.rows).toEqual([]);
    expect(s.open).toBe(1);
    expect(
      calc([task()], [], {
        1: [event(1, "done"), event(2, "done", "done", "2026-11-02T00:00:00Z")],
      }).rows[0].elapsed,
    ).toBe(43200000);
  });
  it("includes completion at start, excludes next midnight, includes tasks created before the range", () => {
    const times = [
      "2026-10-31T23:59:59.999Z",
      "2026-11-01T00:00:00Z",
      "2026-11-01T23:59:59.999Z",
      "2026-11-02T00:00:00Z",
    ];
    const ts = times.map((_, id) => ({
      ...task(id),
      created_at: "2026-10-01T00:00:00Z",
    }));
    const events = Object.fromEntries(
      times.map((t, id) => [id, [event(id, "done", "pending", t)]]),
    );
    expect(
      calc(ts, [], events, "start=2026-11-01&end=2026-11-01").rows.map(
        (r) => r.task.id,
      ),
    ).toEqual([1, 2]);
    expect(calc(ts, [], events, "project=rake").rows).toEqual([]);
    expect(calc(ts, [], events, "q=missing").rows).toEqual([]);
  });
  it("reports unclassifiable completion separately, never assumes updated_at or zero", () => {
    for (const events of [
      {},
      { 1: [] },
      { 1: [event(1, "pending")] },
      ...["bad", "2026-10-31", "2027-01-01"].map((t) => ({
        1: [event(1, "done", "pending", t)],
      })),
    ]) {
      const s = calc([task()], [], events as Record<number, Event[]>);
      expect(s.rows).toEqual([]);
      expect(s.unknownCompletion).toBe(1);
    }
  });
  it("keeps missing, capped, invalid and unfinished run history unknown per metric", () => {
    const s = calc([task()], [], {
      1: [{ ...event(1, ""), kind: "run_start" }, event(2, "done")],
    });
    expect(s.rows[0]).toMatchObject({
      runCount: null,
      runtime: null,
      escalations: 0,
      agents: "Unknown / partial",
    });
    expect(s.total[0].metrics.runtime).toEqual({
      n: 0,
      unknown: 1,
      median: null,
      p90: null,
      max: null,
    });
    expect(s.total[0].histograms.runtime.every((b) => b.count === 0)).toBe(
      true,
    );
    for (const ended_at of [null, "bad", "2026-10-31", "2027-01-01"])
      expect(
        calc([task()], [{ ...run(), ended_at }]).rows[0].runtime,
      ).toBeNull();
    expect(calc([task()], []).rows[0]).toMatchObject({
      runCount: 0,
      runtime: 0,
      escalations: 0,
    });
  });
  it("computes runtime from every run when runs exceed the old 1000-run read cap", () => {
    const n = 1201;
    const minute = 60000;
    const start = Date.parse("2026-11-01T00:00:00Z");
    const tasks = Array.from({ length: n }, (_, i) => task(i + 1));
    const runs = tasks.map((t, i) => ({
      ...run(i + 1),
      task_id: t.id,
      started_at: new Date(start).toISOString(),
      ended_at: new Date(start + (i + 1) * minute).toISOString(),
    }));
    const events = Object.fromEntries(
      tasks.map((t) => [t.id, [event(t.id, "done")]]),
    );
    const s = calc(tasks, runs, events);
    expect(s.total[0].metrics.runtime).toMatchObject({
      n,
      unknown: 0,
      median: 601 * minute,
      max: n * minute,
    });
    expect(s.total[0].metrics.runCount).toMatchObject({ n, unknown: 0, max: 1 });
    expect(s.agents.map((g) => g.label)).toEqual(["codex"]);
  });
  it("attributes mixed-agent tasks once and group bucket counts equal known samples", () => {
    const s = calc(
      [task(), { ...task(2), project: "rake" }],
      [run(), run(2, "claude")],
      { 1: [event(1, "done")], 2: [event(2, "done")] },
    );
    expect(s.rows[0].agents).toBe("Mixed: claude + codex");
    expect(s.rows[1].agents).toBe("No recorded agent");
    for (const groups of [s.total, s.projects, s.agents]) {
      expect(groups.reduce((n, g) => n + g.tasks, 0)).toBe(2);
      for (const g of groups)
        for (const m of metrics)
          expect(g.histograms[m.key].reduce((n, b) => n + b.count, 0)).toBe(
            g.metrics[m.key].n,
          );
    }
    expect(s.total[0].metrics.runCount).toEqual({
      n: 2,
      unknown: 0,
      median: 1,
      p90: 2,
      max: 2,
    });
    expect(
      s.projects.find((g) => g.label === "harvis")!.metrics.runtime.median,
    ).toBe(7200000);
    expect(calc([task()], [run(1, "legacy")]).rows[0].agents).toBe("legacy");
  });
  it("pins every histogram edge including zero and overflow", () => {
    for (const metric of ["runCount", "escalations"] as const)
      expect(
        histogram([0, 1, 2, 3, 4, 5, 9, 10, 999, null], metric).map(
          (b) => b.count,
        ),
      ).toEqual([1, 1, 1, 1, 1, 2, 2]);
    const edges = [
      0, 1, 60000, 300000, 900000, 3600000, 14400000, 86400000, 604800000,
    ];
    for (const metric of ["elapsed", "runtime"] as const) {
      expect(histogram(edges, metric).map((b) => b.count)).toEqual(
        edges.map(() => 1),
      );
      expect(
        histogram(
          edges.slice(1).map((n) => n - 1),
          metric,
        ).map((b) => b.count),
      ).toEqual([1, 1, 1, 1, 1, 1, 1, 1, 0]);
      expect(histogram([1e12, null], metric).at(-1)!.count).toBe(1);
    }
  });
  it("defines median, nearest-rank p90, max and empty summaries", () => {
    expect(summary([4, 1, null, 3, 2])).toEqual({
      n: 4,
      unknown: 1,
      median: 2.5,
      p90: 4,
      max: 4,
    });
    expect(summary([1, 2, 3])).toMatchObject({ median: 2 });
    expect(summary(Array.from({ length: 10 }, (_, i) => i + 1))).toMatchObject({
      p90: 9,
      max: 10,
    });
    expect(summary([])).toEqual({
      n: 0,
      unknown: 0,
      median: null,
      p90: null,
      max: null,
    });
    expect(calc([], [], {})).toMatchObject({
      rows: [],
      total: [],
      projects: [],
      agents: [],
      unknownCompletion: 0,
      open: 0,
    });
  });
  it("validates dates, reversed ranges, future ends and preset UTC boundaries", () => {
    for (const q of [
      "days=custom",
      "start=bad&end=2026-11-02",
      "start=2026-02-30&end=2026-11-02",
      "start=2026-11-02&end=2026-11-01",
      "start=2026-11-01&end=2027-01-01",
    ])
      expect(dateRange(new URLSearchParams(q), at).error).toBeTruthy();
    for (const days of [7, 30, 90]) {
      const r = dateRange(new URLSearchParams(`days=${days}`), at);
      expect(r.end - r.start).toBe(days * 86400000);
      expect(new Date(r.end).toISOString()).toBe("2026-11-03T00:00:00.000Z");
    }
  });
});
describe("large completed cohort", () => {
  // 75 qualifying tasks across three projects with full event history, plus
  // newer tasks completed outside the range and a reopened task.
  const DAY = 86400000;
  const projects = ["harvis", "tn-desk", "rake"];
  const inRange = Array.from({ length: 75 }, (_, i) => i + 1);
  const ts: Task[] = [];
  const events: Record<number, Event[]> = {};
  for (const id of inRange) {
    ts.push({
      ...task(id),
      project: projects[id % 3],
      created_at: "2026-09-01T00:00:00Z",
    });
    // Spread completions over the 7 UTC days 2026-10-27 .. 2026-11-02. Low-ID
    // task 1 completes most recently, just before the noon snapshot; task 2
    // completes at the range's first instant.
    const time =
      id === 1
        ? "2026-11-02T11:59:59.999Z"
        : id === 2
          ? "2026-10-27T00:00:00.000Z"
          : new Date(
              Date.parse("2026-10-27T00:00:00Z") + (id % 7) * DAY + id * 1000,
            ).toISOString();
    events[id] = [event(id * 10, "done", "pending", time)];
  }
  // Newer IDs completed outside the 7-day range on both UTC sides.
  for (const [id, time] of [
    [200, "2026-10-26T23:59:59.999Z"],
    [201, "2026-10-01T00:00:00Z"],
  ] as const) {
    ts.push({ ...task(id), created_at: "2026-09-01T00:00:00Z" });
    events[id] = [event(id * 10, "done", "pending", time)];
  }
  // Reopened after an in-range completion and still open: not in the cohort.
  ts.push({ ...task(202, "pending"), created_at: "2026-09-01T00:00:00Z" });
  events[202] = [
    event(2020, "done", "pending", "2026-10-30T00:00:00Z"),
    event(2021, "pending", "done", "2026-10-31T00:00:00Z"),
  ];
  // Completed in range, reopened, then completed again outside: latest wins.
  ts.push({ ...task(203), created_at: "2026-09-01T00:00:00Z" });
  events[203] = [
    event(2030, "done", "pending", "2026-10-28T00:00:00Z"),
    event(2031, "pending", "done", "2026-10-29T00:00:00Z"),
    event(2032, "done", "pending", "2026-11-02T12:00:01Z"),
  ];
  const runs = inRange.map((id) => ({ ...run(id), task_id: id }));

  it("includes every qualifying task with consistent totals", () => {
    const s = calc(ts, runs, events, "days=7");
    expect(s.rows.map((r) => r.task.id).sort((a, b) => a - b)).toEqual(inRange);
    // 203's latest completion is after the snapshot, so it is unknown.
    expect(s.unknownCompletion).toBe(1);
    expect(s.open).toBe(1);
    const groupOf = (r: (typeof s.rows)[number]) =>
      s.projects.findIndex((g) => g.label === r.task.project);
    const days = dailyCompletions(
      s.rows,
      s.start,
      7,
      s.projects.length,
      groupOf,
    );
    expect(days[0].counts.reduce((a, b) => a + b)).toBeGreaterThan(0);
    expect(days.flatMap((d) => d.counts).reduce((a, b) => a + b)).toBe(75);
    for (const [g, group] of s.projects.entries())
      expect(days.reduce((n, d) => n + d.counts[g], 0)).toBe(group.tasks);
    expect(s.projects.map((g) => g.tasks)).toEqual([25, 25, 25]);
    for (const groups of [s.total, s.projects, s.agents]) {
      expect(groups.reduce((n, g) => n + g.tasks, 0)).toBe(75);
      for (const g of groups)
        for (const m of metrics) {
          expect(g.metrics[m.key].n + g.metrics[m.key].unknown).toBe(g.tasks);
          expect(g.histograms[m.key].reduce((n, b) => n + b.count, 0)).toBe(
            g.metrics[m.key].n,
          );
        }
    }
    expect(s.total[0].metrics.runCount).toMatchObject({ n: 75, unknown: 0 });
  });
  it("filters by project and UTC range without dropping older or low-ID tasks", () => {
    const rake = calc(ts, runs, events, "days=7&project=rake");
    expect(rake.rows.map((r) => r.task.id).sort((a, b) => a - b)).toEqual(
      inRange.filter((id) => id % 3 === 2),
    );
    const lastDay = calc(ts, runs, events, "start=2026-11-02&end=2026-11-02");
    expect(lastDay.rows.map((r) => r.task.id)).toContain(1);
    const firstDay = calc(ts, runs, events, "start=2026-10-27&end=2026-10-27");
    expect(firstDay.rows.map((r) => r.task.id)).toContain(2);
    expect(firstDay.rows.map((r) => r.task.id)).not.toContain(200);
    expect(
      calc(ts, runs, events, "start=2026-10-26&end=2026-10-26").rows.map(
        (r) => r.task.id,
      ),
    ).toEqual([200]);
  });
});
describe("history loading", () => {
  const stub = (
    history: unknown = { events: [], routines: [], runs: [] },
    { status = 200, delay = 1 } = {},
  ) => {
    const state = { calls: [] as string[], methods: [] as string[] };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        state.calls.push(url);
        state.methods.push(init?.method ?? "GET");
        init?.signal?.throwIfAborted();
        await new Promise((r) => setTimeout(r, delay));
        init?.signal?.throwIfAborted();
        return { ok: status === 200, status, json: async () => history };
      }),
    );
    return state;
  };
  it("makes one GET whatever the task or run count, never a read per task", async () => {
    const ts = Array.from({ length: 500 }, (_, id) => task(id + 1));
    const open = Array.from({ length: 50 }, (_, i) => task(1000 + i, "pending"));
    const done = (task_id: number, id: number, time: string) => ({
      ...event(id, "done", "pending", time),
      task_id,
    });
    const state = stub({
      events: [
        done(1, 1, "2026-11-01T12:00:00Z"),
        { ...event(2, "escalated"), task_id: 2 },
        { id: 3, task_id: 2, kind: "run_start", at: "2026-11-01T01:00:00Z" },
        done(2, 4, "2026-11-02T01:00:00Z"),
      ],
      routines: [2, 1000],
      runs: [],
    });
    try {
      const h = await loadHistory(ts, "http://test", new AbortController().signal, {
        open,
      });
      expect(state.calls).toEqual(["http://test/api/stats/history"]);
      expect(state.methods.every((m) => m === "GET")).toBe(true);
      expect(state.calls.some((u) => /\/api\/tasks\/\d+\//.test(u))).toBe(false);
      // Every candidate has events (possibly none) and every task a type.
      expect(Object.keys(h.events)).toHaveLength(500);
      expect(h.events[3]).toEqual([]);
      expect(h.events[2].map((e) => e.id)).toEqual([2, 3, 4]);
      expect(Object.keys(h.routines)).toHaveLength(550);
      expect(h.routines[2]).toBe(true);
      expect(h.routines[1000]).toBe(true);
      expect(h.routines[1]).toBe(false);
      expect(h.routines[1001]).toBe(false);
      // Same numbers as the per-task history it replaces.
      const perTask: History = {
        ...h,
        events: {
          ...Object.fromEntries(ts.map((t) => [t.id, [] as Event[]])),
          1: [event(1, "done")],
          2: [
            event(2, "escalated"),
            { ...event(3, ""), kind: "run_start", at: "2026-11-01T01:00:00Z", from_value: null, to_value: null },
            event(4, "done", "pending", "2026-11-02T01:00:00Z"),
          ],
        },
      };
      const p = new URLSearchParams("days=7");
      const strip = (x: unknown) => JSON.parse(JSON.stringify(x));
      const s = calculateStats([...ts, ...open], { ...h, at }, p);
      expect(strip(s)).toEqual(
        strip(calculateStats([...ts, ...open], { ...perTask, at }, p)),
      );
      expect(s.rows).toHaveLength(2);
      expect(s.unknownCompletion).toBe(498);
      expect(s.types).toEqual({ normal: 1, routine: 1, unknown: 0 });
      expect(s.rows.find((r) => r.task.id === 2)?.escalations).toBe(1);
    } finally {
      vi.unstubAllGlobals();
    }
  });
  it("rejects when the stats history read fails", async () => {
    stub(undefined, { status: 503 });
    try {
      await expect(
        loadHistory([task()], "http://test", new AbortController().signal),
      ).rejects.toThrow("/api/stats/history: HTTP 503");
    } finally {
      vi.unstubAllGlobals();
    }
  });
  it("rejects once aborted", async () => {
    stub(undefined, { delay: 20 });
    try {
      const controller = new AbortController();
      const pending = loadHistory([task()], "http://test", controller.signal);
      controller.abort();
      await expect(pending).rejects.toThrow();
    } finally {
      vi.unstubAllGlobals();
    }
  });
});

describe("normal and routine task types", () => {
  // Routine-heavy cohort over the 7 UTC days 2026-10-27..2026-11-02: 60
  // routine and 15 normal completions across three projects, 2 completions of
  // unknown type, plus open, unknown-completion and out-of-range tasks.
  const DAY = 86400000;
  const projects = ["harvis", "tn-desk", "rake"];
  const ts: Task[] = [];
  const events: Record<number, Event[]> = {};
  const routines: Record<number, boolean> = {};
  const done = (id: number, time: string, routine: boolean | null) => {
    ts.push({
      ...task(id),
      project: projects[id % 3],
      // Titles never decide type: normal tasks look routine-like and back.
      title: routine ? `Ship feature ${id}` : `Routine: daily report ${id}`,
      created_at: "2026-09-01T00:00:00Z",
    });
    events[id] = [event(id * 10, "done", "pending", time)];
    if (routine !== null) routines[id] = routine;
  };
  for (let id = 1; id <= 77; id++)
    done(
      id,
      id === 1
        ? "2026-10-27T00:00:00.000Z"
        : id === 2
          ? "2026-11-02T11:59:59.999Z"
          : new Date(
              Date.parse("2026-10-27T00:00:00Z") + (id % 7) * DAY + id * 1000,
            ).toISOString(),
      id <= 60 ? true : id <= 75 ? false : null,
    );
  // Completed just outside both UTC edges.
  done(100, "2026-10-26T23:59:59.999Z", true);
  done(101, "2026-11-03T00:00:00.000Z", false);
  // Open: 3 routine, 2 normal, 1 unknown.
  for (const [id, routine] of [
    [110, true],
    [111, true],
    [112, true],
    [113, false],
    [114, false],
    [115, null],
  ] as const) {
    ts.push({ ...task(id, "pending"), project: projects[id % 3] });
    if (routine !== null) routines[id] = routine;
  }
  // Done with failed event history: unknown completion, one of each type.
  for (const [id, routine] of [
    [120, true],
    [121, false],
    [122, null],
  ] as const) {
    ts.push({ ...task(id), project: projects[id % 3] });
    if (routine !== null) routines[id] = routine;
  }
  const runs = ts.map((t) => ({ ...run(t.id), task_id: t.id }));
  const stats = (query: string) => calc(ts, runs, events, query, routines);
  const ids = (s: ReturnType<typeof stats>) =>
    s.rows.map((r) => r.task.id).sort((a, b) => a - b);
  const range = (a: number, b: number) =>
    Array.from({ length: b - a + 1 }, (_, i) => a + i);

  it("splits the complete All cohort by type with no double counting", () => {
    const all = stats("days=7");
    expect(all.scope).toBe("all");
    expect(ids(all)).toEqual(range(1, 77));
    expect(all.types).toEqual({ normal: 15, routine: 60, unknown: 2 });
    expect(all.types.normal + all.types.routine + all.types.unknown).toBe(
      all.rows.length,
    );
    expect(all.open).toBe(6);
    expect(all.openTypes).toEqual({ normal: 2, routine: 3, unknown: 1 });
    // 101 completes after the snapshot: unknown completion, like 120..122.
    expect(all.unknownCompletion).toBe(4);
    expect(all.excludedUnknownType).toBe(0);
    expect(all.taskTypes.map((g) => [g.label, g.tasks])).toEqual([
      ["Normal", 15],
      ["Routine", 60],
      ["Unknown type", 2],
    ]);
    expect(all.total[0].label).toBe("All tasks");
  });
  it("applies Normal and Routine scopes across rows, open and unknown counts", () => {
    const normal = stats("days=7&type=normal");
    expect(ids(normal)).toEqual(range(61, 75));
    expect(normal.rows.every((r) => r.type === "normal")).toBe(true);
    expect(normal.types).toEqual({ normal: 15, routine: 60, unknown: 2 });
    expect(normal.open).toBe(2);
    expect(normal.unknownCompletion).toBe(2); // 101 and 121
    expect(normal.excludedUnknownType).toBe(4); // 76, 77, 115, 122
    expect(normal.total[0]).toMatchObject({ label: "Normal tasks", tasks: 15 });
    for (const groups of [normal.total, normal.projects, normal.agents])
      expect(groups.reduce((n, g) => n + g.tasks, 0)).toBe(15);
    expect(normal.total[0].metrics.runCount).toMatchObject({
      n: 15,
      unknown: 0,
    });
    const routine = stats("days=7&type=routine");
    expect(ids(routine)).toEqual(range(1, 60));
    expect(routine.open).toBe(3);
    expect(routine.unknownCompletion).toBe(1); // 120
    expect(routine.total[0]).toMatchObject({ label: "Routine tasks", tasks: 60 });
    // Unknown is excluded from both typed scopes, never counted as Normal.
    expect(normal.rows.length + routine.rows.length + 2).toBe(77);
    // Unrecognized type values fall back to the All default.
    expect(stats("days=7&type=bogus").rows).toHaveLength(77);
  });
  it("composes type with project and UTC date boundaries", () => {
    const harvis = stats("days=7&project=harvis&type=routine");
    expect(ids(harvis)).toEqual(range(1, 60).filter((id) => id % 3 === 0));
    expect(harvis.open).toBe(
      [110, 111, 112].filter((id) => projects[id % 3] === "harvis").length,
    );
    const first = stats("start=2026-10-27&end=2026-10-27&type=routine");
    expect(ids(first)).toContain(1);
    expect(ids(first)).not.toContain(100);
    expect(
      ids(stats("start=2026-10-26&end=2026-10-26&type=routine")),
    ).toEqual([100]);
    expect(stats("start=2026-10-26&end=2026-10-26&type=normal").rows).toEqual(
      [],
    );
    const last = stats("start=2026-11-02&end=2026-11-02&type=routine");
    expect(ids(last)).toContain(2);
  });
  it("keeps an empty type empty and reports unknown-only cohorts honestly", () => {
    const onlyRoutine = calc(
      [task(1)],
      [run()],
      { 1: [event(1, "done")] },
      "type=normal",
      { 1: true },
    );
    expect(onlyRoutine.rows).toEqual([]);
    expect(onlyRoutine.types).toEqual({ normal: 0, routine: 1, unknown: 0 });
    const unclassified = calc([task(1)], [run()], { 1: [event(1, "done")] });
    expect(unclassified.types).toEqual({ normal: 0, routine: 0, unknown: 1 });
    expect(unclassified.rows[0].type).toBe("unknown");
    expect(
      calc([task(1)], [run()], { 1: [event(1, "done")] }, "type=normal").rows,
    ).toEqual([]);
  });
});
describe("metric trends", () => {
  const DAY = 86400000;
  const start = Date.parse("2026-11-01T00:00:00Z");
  const row = (
    completedAt: number,
    values: Partial<Record<"runCount" | "escalations" | "elapsed" | "runtime", number | null>> = {},
    project = "harvis",
  ) =>
    ({
      task: { ...task(), project },
      type: "normal",
      agents: "codex",
      completedAt,
      runCount: 1,
      escalations: 0,
      elapsed: 1000,
      runtime: 500,
      ...values,
    }) as Parameters<typeof trends>[0][number];
  it("switches from daily to weekly buckets after the documented threshold", () => {
    expect(DAILY_TREND_MAX_DAYS).toBe(31);
    const daily = trendBuckets(start, start + 31 * DAY);
    expect(daily.weekly).toBe(false);
    expect(daily.buckets).toHaveLength(31);
    expect(daily.buckets[0]).toEqual({ start, end: start + DAY });
    const weekly = trendBuckets(start, start + 32 * DAY);
    expect(weekly.weekly).toBe(true);
    expect(weekly.buckets.map((b) => (b.end - b.start) / DAY)).toEqual([
      7, 7, 7, 7, 4,
    ]);
    const ninety = trendBuckets(start, start + 90 * DAY);
    expect(ninety.buckets).toHaveLength(13);
    expect(ninety.buckets.at(-1)!.end).toBe(start + 90 * DAY);
    expect(trendBuckets(start, start).buckets).toEqual([]);
  });
  it("assigns rows by UTC completion date, start inclusive and end exclusive", () => {
    const { buckets } = trendBuckets(start, start + 2 * DAY);
    const t = trends(
      [
        row(start - 1),
        row(start),
        row(start + DAY - 1),
        row(start + DAY),
        row(start + 2 * DAY),
      ],
      buckets,
      1,
      () => 0,
    );
    expect(t.runCount[0].map((p) => p.n)).toEqual([2, 1]);
  });
  it("computes mean and population SD, SD 0 for one sample and a gap for none", () => {
    const { buckets } = trendBuckets(start, start + 3 * DAY);
    const t = trends(
      [
        row(start, { runCount: 1 }),
        row(start + 1, { runCount: 3 }),
        row(start + 2, { runCount: 5 }),
        row(start + 2 * DAY, { runCount: 4 }),
      ],
      buckets,
      1,
      () => 0,
    );
    const [a, gap, one] = t.runCount[0];
    expect(a.n).toBe(3);
    expect(a.mean).toBe(3);
    expect(a.sd).toBeCloseTo(Math.sqrt(8 / 3));
    expect(a.high).toBeCloseTo(3 + Math.sqrt(8 / 3));
    expect(gap).toMatchObject({ n: 0, unknown: 0, mean: null, sd: null, low: null, high: null });
    expect(one).toMatchObject({ n: 1, mean: 4, sd: 0, low: 4, high: 4 });
  });
  it("clips the band's lower bound at zero", () => {
    const { buckets } = trendBuckets(start, start + DAY);
    const t = trends(
      [row(start, { escalations: 0 }), row(start, { escalations: 0 }), row(start, { escalations: 6 })],
      buckets,
      1,
      () => 0,
    );
    const p = t.escalations[0][0];
    expect(p.mean).toBe(2);
    expect(p.mean! - p.sd!).toBeLessThan(0);
    expect(p.low).toBe(0);
  });
  it("excludes unknown metrics from the bucket and counts them as unknown", () => {
    const { buckets } = trendBuckets(start, start + DAY);
    const t = trends(
      [row(start, { runtime: null, runCount: null }), row(start, { runtime: 60000 })],
      buckets,
      1,
      () => 0,
    );
    expect(t.runtime[0][0]).toMatchObject({ n: 1, unknown: 1, mean: 60000 });
    expect(t.runCount[0][0]).toMatchObject({ n: 1, unknown: 1, mean: 1 });
    expect(t.elapsed[0][0]).toMatchObject({ n: 2, unknown: 0 });
    const none = trends([row(start, { runtime: null })], buckets, 1, () => 0);
    expect(none.runtime[0][0]).toMatchObject({ n: 0, unknown: 1, mean: null });
  });
  it("returns gaps for an empty cohort", () => {
    const { buckets } = trendBuckets(start, start + 7 * DAY);
    const t = trends([], buckets, 1, () => 0);
    for (const m of metrics) {
      expect(t[m.key][0]).toHaveLength(7);
      expect(t[m.key][0].every((p) => p.n === 0 && p.mean === null)).toBe(true);
    }
  });
  it("per-group series split the total cohort exactly", () => {
    const tasks = Array.from({ length: 12 }, (_, i) => ({
      ...task(i + 1),
      project: ["harvis", "rake", "amux"][i % 3],
      created_at: "2026-10-20T00:00:00Z",
    }));
    const events = Object.fromEntries(
      tasks.map((t, i) => [
        t.id,
        [event(t.id, "done", "pending", new Date(Date.parse("2026-10-27T00:00:00Z") + i * 13 * 3600000).toISOString())],
      ]),
    );
    const s = calc(tasks, [], events);
    expect(s.rows).toHaveLength(12);
    const { buckets } = trendBuckets(s.start, s.end);
    const total = trends(s.rows, buckets, 1, () => 0);
    const groupOf = (r: (typeof s.rows)[number]) =>
      s.projects.findIndex((g) => g.label === r.task.project);
    const split = trends(s.rows, buckets, s.projects.length, groupOf);
    for (const m of metrics)
      buckets.forEach((_, i) => {
        const parts = split[m.key].map((g) => g[i]);
        const n = parts.reduce((a, p) => a + p.n, 0);
        expect(n).toBe(total[m.key][0][i].n);
        expect(parts.reduce((a, p) => a + p.unknown, 0)).toBe(total[m.key][0][i].unknown);
        if (n)
          expect(parts.reduce((a, p) => a + (p.mean ?? 0) * p.n, 0) / n).toBeCloseTo(
            total[m.key][0][i].mean!,
          );
      });
    expect(total.elapsed[0].reduce((a, p) => a + p.n, 0)).toBe(12);
  });
  it("follows the type scope", () => {
    const tasks = [task(1), task(2), task(3)];
    const events = {
      1: [event(1, "done")],
      2: [event(2, "done")],
      3: [event(3, "done")],
    };
    const routines = { 1: true, 2: false };
    const count = (q: string) => {
      const s = calc(tasks, [], events, q, routines);
      const { buckets } = trendBuckets(s.start, s.end);
      return trends(s.rows, buckets, 1, () => 0).elapsed[0].reduce((a, p) => a + p.n, 0);
    };
    expect(count("")).toBe(3);
    expect(count("type=routine")).toBe(1);
    expect(count("type=normal")).toBe(1);
  });
  it("collapses long group lists into the largest five plus Other", () => {
    const short = [{ label: "a", tasks: 1 }, { label: "b", tasks: 5 }];
    expect(collapseGroups(short)).toEqual({ labels: ["a", "b"], index: [0, 1] });
    const long = ["a", "b", "c", "d", "e", "f", "g"].map((label, i) => ({
      label,
      tasks: [1, 9, 3, 8, 2, 7, 6][i],
    }));
    const c = collapseGroups(long);
    expect(c.labels).toEqual(["b", "d", "f", "g", "c", "Other (2 groups)"]);
    expect(c.index).toEqual([5, 0, 4, 1, 5, 2, 3]);
  });
});
