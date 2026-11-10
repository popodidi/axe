import { describe, it, expect } from "vitest";
import {
  selectTasks,
  selectArtifacts,
  pageNumber,
  isDiff,
  safeLink,
  formatDuration,
  shellQuote,
  taskCommands,
  runCommands,
  taskStatus,
  taskStatusLabel,
  taskStatusTitle,
  waitReason,
  type Task,
  type Event,
  type Run,
} from "./model";
const task = (id: number, state = "pending"): Task => ({
  id,
  title: "Task " + id,
  detail: "A spec",
  project: "harvis",
  state,
  created_at: `2026-10-${id + 10}`,
  updated_at: "",
  runs: 0,
});
describe("URL task selection", () => {
  it("defaults to newest created including done", () =>
    expect(
      selectTasks(
        [task(1), task(3, "done"), task(2)],
        new URLSearchParams(),
      ).map((t) => t.id),
    ).toEqual([3, 2, 1]));
  it("combines filters and search and preserves explicit done", () =>
    expect(
      selectTasks(
        [task(1), task(2, "done")],
        new URLSearchParams("state=done&project=harvis&q=Task+2"),
      ).map((t) => t.id),
    ).toEqual([2]));
  it("legacy view=ops no longer narrows the task list", () =>
    expect(
      selectTasks(
        [task(1), task(2)],
        new URLSearchParams("view=ops"),
      ).map((t) => t.id),
    ).toEqual([2, 1]));
  it("supports oldest and bounded page values", () => {
    expect(
      selectTasks([task(2), task(1)], new URLSearchParams("sort=oldest"))[0].id,
    ).toBe(1);
    expect(pageNumber(new URLSearchParams("page=-2"))).toBe(1);
  });
  it("recognizes only spec edit diffs", () => {
    expect(
      isDiff({
        kind: "edit",
        body: "--- detail\n+++ detail\n@@ -1 +1 @@",
      } as Event),
    ).toBe(true);
    expect(isDiff({ kind: "note", body: "# Heading" } as Event)).toBe(false);
  });
  it("rejects executable artifact URLs", () => {
    expect(safeLink("javascript:alert(1)")).toBeUndefined();
    expect(safeLink("https://example.com/pr/1")).toBe(
      "https://example.com/pr/1",
    );
  });
});
describe("formatDuration", () => {
  it("uses the previous dashboard's compact elapsed format", () => {
    expect(formatDuration(-5)).toBe("0s");
    expect(formatDuration(59_999)).toBe("59s");
    expect(formatDuration(61_000)).toBe("1m 1s");
    expect(formatDuration(3_660_000)).toBe("1h 1m");
    expect(formatDuration(49 * 3_600_000)).toBe("2d 1h");
  });
});
describe("hv commands", () => {
  const run = (agent: string, ended = true): Run => ({
    id: 1,
    task_id: 38,
    project: "harvis",
    agent,
    mode: "dispatch",
    started_at: "2026-10-28T00:00:00Z",
    ended_at: ended ? "2026-10-28T01:00:00Z" : null,
    exit_code: 0,
    current_activity: null,
    last_activity_at: null,
    log_path: "/home/harvis/.local/state/taskq/logs/run 1.log",
    session_id: "48205d0b-549b-41f5-8e11",
  });
  it("task log and spec", () => {
    expect(taskCommands({ id: 38, project: "harvis" })).toEqual({
      log: "hv harvis taskq log 38",
      spec: "hv harvis taskq show 38",
    });
  });
  it("attach resumes in the task worktree, only after the run ended", () => {
    expect(runCommands(run("claude")).attach).toBe(
      "hv harvis sh -c 'cd ~/workspace/.worktrees/harvis/task-38--work && exec claude --resume 48205d0b-549b-41f5-8e11'",
    );
    expect(runCommands(run("codex")).attach).toBe(
      "hv harvis sh -c 'cd ~/workspace/.worktrees/harvis/task-38--work && exec codex resume 48205d0b-549b-41f5-8e11'",
    );
    expect(runCommands(run("claude", false)).attach).toBeNull();
    expect(runCommands(run("other")).attach).toBeNull();
  });
  it("tail quotes a log path with spaces", () => {
    expect(runCommands(run("claude")).watch).toBe(
      "hv harvis tail -f '/home/harvis/.local/state/taskq/logs/run 1.log'",
    );
  });
  it("quotes like hv: safe words bare, others literal", () => {
    expect(shellQuote("tn-desk")).toBe("tn-desk");
    expect(shellQuote("it's $HOME")).toBe("'it'\\''s $HOME'");
    expect(shellQuote("")).toBe("''");
  });
});
describe("dependencies", () => {
  const waiting = (state: string, waiting_on: number[]): Task => ({
    ...task(9, state),
    status: state === "pending" && waiting_on.length ? "blocked" : state,
    waiting_on,
  });
  it("shows dependency waits as blocked and filters by status", () => {
    const tasks = [task(1), waiting("pending", [1]), task(3, "blocked")];
    expect(tasks.map(taskStatus)).toEqual(["pending", "blocked", "blocked"]);
    // Older servers without status fall back to the stored state.
    expect(taskStatus({ state: "done" })).toBe("done");
    const ids = (state: string) =>
      selectTasks(tasks, new URLSearchParams({ state, sort: "oldest" })).map(
        (t) => t.id,
      );
    expect(ids("blocked")).toEqual([3, 9]);
    expect(ids("pending")).toEqual([1]);
  });
  it("explains why work cannot resume without reading notes", () => {
    expect(waitReason(waiting("pending", [1, 2]))).toBe(
      "Waiting on prerequisites #1, #2; becomes ready automatically",
    );
    expect(waitReason(waiting("blocked", [1]))).toBe(
      "Hold (resolve it explicitly) and waiting on prerequisites #1; both must clear",
    );
    expect(waitReason(waiting("blocked", []))).toBe(
      "Hold: resolve it explicitly, then set pending",
    );
    expect(waitReason(waiting("in_progress", [1]))).toBe(
      "Waiting on prerequisites #1 (prerequisite reopened; not cancelled)",
    );
    expect(waitReason(waiting("pending", []))).toBeNull();
    expect(waitReason(waiting("done", []))).toBeNull();
  });

});

it("filters escalations independently of dependency and external waits", () => {
  const escalated = { ...task(1, "escalated"), waiting_on: [7] };
  const tasks = [escalated, task(2, "blocked"), { ...task(3), status: "blocked", waiting_on: [7] }];
  expect(selectTasks(tasks, new URLSearchParams("state=escalated")).map(t => t.id)).toEqual([1]);
  expect(taskStatus(escalated)).toBe("escalated");
  expect(waitReason(escalated)).toContain("both must clear");
  expect(waitReason({ ...escalated, state: "pending" })).toContain("becomes ready automatically");
});

describe("selectArtifacts", () => {
  it("keeps first-seen exact displayed values without mutating audit events", () => {
    const values: [string | null, string | null][] = [
      ["https://example.com", "ignored"], ["evidence", null],
      ["https://example.com", null], [null, "evidence"],
      ["", "fallback"], ["fallback", null], [null, null], ["", ""],
      ["https://example.com/", null], [" evidence ", null],
    ];
    const events = Object.freeze([
      Object.freeze({ id: 0, kind: "note", body: "evidence", to_value: null, from_value: null, at: "", actor: "agent" }),
      ...values.map(([body, to_value], i) => Object.freeze({
        id: i + 1, kind: "artifact", body, to_value, from_value: null, at: "", actor: "agent",
      })),
    ]);
    const before = JSON.stringify(events);
    const artifacts = selectArtifacts(events);
    expect(artifacts.map(e => e.id)).toEqual([1, 2, 5, 7, 9, 10]);
    expect(artifacts[0]).toBe(events[1]);
    expect(JSON.stringify(events)).toBe(before);
    expect(events.filter(e => e.kind === "artifact")).toHaveLength(10);
    expect(selectArtifacts([])).toEqual([]);
  });
});

describe("timed waits", () => {
  const sleeping = { ...task(9), status: "blocked", waiting_on: [], not_before: "2099-11-09T09:00:00Z" };
  it("labels a future wake time locally and exposes UTC", () => {
    expect(taskStatus(sleeping)).toBe("waiting");
    expect(taskStatusLabel(sleeping)).toMatch(/^waiting until /);
    expect(taskStatusTitle(sleeping)).toBe("2099-11-09T09:00:00.000Z");
    expect(waitReason(sleeping)).toBe(taskStatusLabel(sleeping).replace(/^waiting/, "Waiting") + "; becomes ready automatically");
    expect(selectTasks([sleeping], new URLSearchParams("state=waiting"))).toEqual([sleeping]);
    expect(selectTasks([sleeping], new URLSearchParams("state=blocked"))).toEqual([]);
  });
  it("shows expired wake times as pending even before the API refreshes", () => {
    const expired = { ...sleeping, not_before: "2020-01-01T00:00:00Z" };
    expect(taskStatusLabel(expired)).toBe("pending");
    expect(waitReason(expired)).toBeNull();
  });
  it("gives dependencies priority and preserves stored holds and escalation", () => {
    expect(taskStatusLabel({ ...sleeping, waiting_on: [1, 2] })).toBe("blocked (deps #1, #2)");
    expect(taskStatusLabel({ ...sleeping, not_before: null, waiting_on: [1] })).toBe("blocked (deps #1)");
    expect(taskStatusLabel({ ...sleeping, state: "blocked" })).toBe("blocked");
    expect(taskStatusLabel({ ...sleeping, state: "escalated", status: "escalated" })).toBe("escalated");
  });
});

describe("column sorting", () => {
  const rows = [
    { ...task(10), title: "alpha", project: "z", state: "pending", waiting_on: [1], created_at: "2026-01-01T01:00:00+02:00" },
    { ...task(2), title: "Beta", project: "a", state: "done", created_at: "2026-01-01T00:00:00Z" },
    { ...task(3), title: "Alpha", project: "z", state: "pending", created_at: "2026-01-01T00:00:00Z" },
  ];
  const ids = (sort: string, list = rows) => selectTasks(list, new URLSearchParams({ sort })).map(t => t.id);
  it.each([
    ["id", [2, 3, 10], [10, 3, 2]],
    ["title", [3, 10, 2], [2, 3, 10]],
    ["project", [2, 3, 10], [3, 10, 2]],
    ["state", [10, 2, 3], [3, 2, 10]],
    ["created_at", [10, 2, 3], [2, 3, 10]],
  ])("sorts %s both ways with stable ID ties", (key, ascending, descending) => {
    expect(ids(`${key}:asc`)).toEqual(ascending);
    expect(ids(`${key}:desc`, [...rows].reverse())).toEqual(descending);
  });
  it("keeps empty text and invalid dates last without mutating input", () => {
    const list = [{ ...task(1), title: "  ", project: "", state: "", created_at: "invalid" }, ...rows];
    for (const key of ["title", "project", "state", "created_at"])
      for (const direction of ["asc", "desc"])
        expect(ids(`${key}:${direction}`, list).at(-1)).toBe(1);
    expect(list.map(t => t.id)).toEqual([1, 10, 2, 3]);
    expect(ids("unknown:asc")).toEqual(ids(""));
  });
});
