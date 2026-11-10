import { describe, it, expect } from "vitest";
import {
  botStats,
  followUpRuns,
  meanSd,
  replyLatencies,
  runOutcome,
  trendOf,
  type BotData,
} from "./bot-stats";
import {
  asRun,
  botHold,
  botStatus,
  poolSummary,
  profileHistory,
  type BotRun,
  type Wake,
} from "./bots";
import { runCommands } from "./model";
const DAY = 86400000;
const START = Date.UTC(2026, 10, 1);
const range = { start: START, end: START + 7 * DAY };
const t = (day: number, min = 0, sec = 0) =>
  new Date(START + day * DAY + min * 60000 + sec * 1000).toISOString();
const run = (
  id: number,
  start: string,
  end: string | null,
  extra: Partial<BotRun> = {},
): BotRun => ({
  id,
  bot: "b",
  agent: "claude",
  mode: "resume",
  session_id: "s",
  claim: `bot-run:${id}`,
  started_at: start,
  ended_at: end,
  exit_code: end ? 0 : null,
  stop_reason: end ? "exit" : null,
  log_path: `/logs/bot-${id}.log`,
  current_activity: null,
  last_activity_at: null,
  live: !end,
  ...extra,
});
const wake = (id: number, kind: string, created: string, extra: Partial<Wake> = {}): Wake => ({
  id,
  kind,
  body: "",
  due_at: created,
  created_at: created,
  actor: "owner",
  run_id: null,
  delivered_via: null,
  redelivered: false,
  ...extra,
});
const msg = (id: number, direction: "in" | "out", at: string, kind = "text") => ({
  id,
  direction,
  kind,
  reply_to: null,
  edit_of: null,
  attachments: 0,
  at,
});
const empty: BotData = {
  runs: [],
  wakes: [],
  events: [],
  activity: { messages: [], task_actions: [] },
};
describe("bot stats", () => {
  it("counts runs per UTC day and excludes runs outside the range", () => {
    const s = botStats(
      {
        ...empty,
        runs: [
          run(1, t(0, 10), t(0, 12)),
          run(2, t(0, 20), t(0, 30)),
          run(3, t(3, 0), t(3, 1)),
          run(4, t(-1, 0), t(-1, 1)),
          run(5, t(7, 0), t(7, 1)),
        ],
      },
      range,
      "bot:b",
    );
    expect(s.days).toBe(7);
    expect(s.runsPerDay.map((d) => d.count)).toEqual([2, 0, 0, 1, 0, 0, 0]);
    expect(s.runs).toEqual({ total: 3, live: 0 });
  });
  it("runtime mean and population SD exclude live runs; empty days are gaps", () => {
    const s = botStats(
      {
        ...empty,
        runs: [
          run(1, t(0, 0), t(0, 2)),
          run(2, t(0, 10), t(0, 14)),
          run(3, t(2, 0), null),
        ],
      },
      range,
      "bot:b",
    );
    expect(s.runs).toEqual({ total: 3, live: 1 });
    expect(s.runtime.n).toBe(2);
    expect(s.runtime.mean).toBe(3 * 60000);
    expect(s.runtime.sd).toBe(60000);
    expect(s.runtime.weekly).toBe(false);
    expect(s.runtime.trend[0]).toMatchObject({ n: 2, mean: 180000, sd: 60000, low: 120000, high: 240000 });
    expect(s.runtime.trend[1]).toMatchObject({ n: 0, mean: null, sd: null });
    expect(s.runtime.summary.median).toBe(180000);
  });
  it("groups wakes by kind and splits mid-run hook deliveries from prompt deliveries", () => {
    const s = botStats(
      {
        ...empty,
        wakes: [
          wake(1, "message", t(0, 1), { run_id: 1, delivered_via: "prompt" }),
          wake(2, "message", t(0, 2), { run_id: 1, delivered_via: "hook" }),
          wake(3, "button", t(0, 3), { run_id: 1, delivered_via: "hook" }),
          wake(4, "wake", t(1, 0), { actor: "bot:b" }),
          wake(5, "message", t(-2, 0), { run_id: 1, delivered_via: "hook" }),
        ],
      },
      range,
      "bot:b",
    );
    expect(s.wakesByKind).toEqual({ message: 2, button: 1, wake: 1 });
    expect(s.wakes).toBe(4);
    expect(s.deliveries).toMatchObject({ midRun: 2, prompt: 1, undelivered: 1 });
  });
  it("follow-up runs are runs delivering wakes created during the previous run", () => {
    const runs = [run(1, t(0, 0), t(0, 10)), run(2, t(0, 10), t(0, 12)), run(3, t(1, 0), t(1, 2))];
    const wakes = [
      wake(1, "message", t(0, 5), { run_id: 2, delivered_via: "prompt" }),
      wake(2, "message", t(0, 30), { run_id: 3, delivered_via: "prompt" }),
      wake(3, "message", t(0, 6), { run_id: 1, delivered_via: "hook" }),
    ];
    expect([...followUpRuns(runs, wakes)]).toEqual([2]);
    expect(botStats({ ...empty, runs, wakes }, range, "bot:b").deliveries.followUpRuns).toBe(1);
  });
  it("reply latency runs from the first inbound message of a turn to the first outbound", () => {
    const activity = {
      task_actions: [],
      messages: [
        msg(1, "in", t(0, 0, 0)),
        msg(2, "in", t(0, 0, 20)),
        msg(3, "out", t(0, 1, 0)),
        msg(4, "out", t(0, 2, 0)),
        msg(5, "in", t(1, 0, 0), "reaction"),
        msg(6, "in", t(1, 5, 0)),
        msg(7, "out", t(1, 8, 0)),
        msg(8, "in", t(2, 0, 0)),
      ],
    };
    const l = replyLatencies(activity, range);
    expect(l.samples.map((s) => s.value)).toEqual([60000, 180000]);
    expect(l.unanswered).toBe(1);
    const s = botStats({ ...empty, activity }, range, "bot:b");
    expect(s.latency.mean).toBe(120000);
    expect(s.latency.sd).toBe(60000);
    expect(s.latency.unanswered).toBe(1);
  });
  it("counts resumed vs fresh runs and run outcomes", () => {
    const runs = [
      run(1, t(0), t(0, 1), { mode: "fresh" }),
      run(2, t(0, 2), t(0, 3), { stop_reason: "exit", exit_code: 3 }),
      run(3, t(0, 4), t(0, 5), { stop_reason: "spawn_failed", exit_code: null }),
      run(4, t(0, 6), t(0, 7), { stop_reason: "stop", exit_code: null }),
      run(5, t(0, 8), t(0, 9), { stop_reason: "pause", exit_code: null }),
      run(6, t(0, 10), t(0, 11), { stop_reason: "dispatcher_stopped", exit_code: null }),
      run(7, t(0, 12), null),
    ];
    const events = [
      { id: 1, at: t(0, 9), actor: "human", session: null, kind: "pause", body: "" },
      { id: 2, at: t(0, 20), actor: "dispatcher", session: null, kind: "attention", body: "paused" },
      { id: 3, at: t(1), actor: "telegram", session: null, kind: "security_drop", body: "{}" },
      { id: 4, at: t(-1), actor: "human", session: null, kind: "pause", body: "" },
    ];
    const s = botStats({ ...empty, runs, events }, range, "bot:b");
    expect(s.sessions).toEqual({ resumed: 6, fresh: 1 });
    expect(s.outcomes).toEqual({
      ok: 1,
      failure: 2,
      stop: 1,
      pause: 1,
      interrupted: 1,
      live: 1,
      ownerPauses: 1,
      attention: 1,
      securityDrops: 1,
    });
    expect(runOutcome(runs[1])).toBe("failure");
  });
  it("task actions split bot:<name> from human (the Lv1 metric)", () => {
    const action = (actor: string, at: string) => ({ task_id: 1, actor, at, kind: "note" });
    const s = botStats(
      {
        ...empty,
        activity: {
          messages: [],
          task_actions: [
            action("bot:b", t(0)),
            action("bot:b", t(0, 1)),
            action("bot:b", t(2)),
            action("human", t(2, 1)),
            action("bot:other", t(2, 2)),
            action("human", t(-3)),
          ],
        },
      },
      range,
      "bot:b",
    );
    expect(s.taskActions).toMatchObject({ bot: 3, human: 1, share: 0.75 });
    expect(s.taskActions.perDay[0]).toMatchObject({ bot: 2, human: 0 });
    expect(s.taskActions.perDay[2]).toMatchObject({ bot: 1, human: 1 });
    expect(botStats(empty, range, "bot:b").taskActions.share).toBeNull();
  });
  it("flags reads at the cap as possibly truncated", () => {
    const runs = Array.from({ length: 1000 }, (_, i) => run(i + 1, t(0), t(0, 1)));
    expect(botStats({ ...empty, runs }, range, "bot:b").truncated).toEqual(["runs"]);
    expect(botStats(empty, range, "bot:b").truncated).toEqual([]);
  });
  it("trend helpers follow the Stats rules", () => {
    expect(meanSd([])).toEqual({ n: 0, mean: null, sd: null });
    expect(meanSd([5])).toEqual({ n: 1, mean: 5, sd: 0 });
    const [p] = trendOf([{ at: 1, value: 1 }, { at: 2, value: 5 }], [{ start: 0, end: 10 }]);
    expect(p).toMatchObject({ n: 2, mean: 3, sd: 2, low: 1, high: 5, unknown: 0 });
  });
});
describe("bot model", () => {
  it("labels attention before paused", () => {
    expect(botStatus({ state: "paused", attention: true })).toBe("Needs attention");
    expect(botStatus({ state: "paused", attention: false })).toBe("Paused");
    expect(botStatus({ state: "active", attention: false })).toBe("Active");
  });
  it("maps bot runs to RunList runs that resume in the bot worktree", () => {
    const r = asRun(run(9, t(0), t(0, 1), { session_id: "abc" }), { project: "tundra-desk", name: "chief" });
    expect(r.task_id).toBe(0);
    expect(runCommands(r).attach).toBe(
      "sh -c 'cd ~/workspace/.worktrees/tundra-desk/bot-chief && exec claude --resume abc'",
    );
    expect(runCommands(r).watch).toBe("tail -f /logs/bot-9.log");
    expect(asRun(run(2, t(0), t(0, 1), { stop_reason: "stop" }), { project: "p", name: "b" }).current_activity).toBe("Ended: stop");
  });
  it("profile history keeps one field, newest first", () => {
    const e = (id: number, kind: string) => ({ id, at: t(0, id), actor: "human", session: null, kind, body: "why: x" });
    expect(profileHistory([e(1, "conduct"), e(2, "notes"), e(3, "conduct")], "conduct").map((x) => x.id)).toEqual([3, 1]);
  });
});

describe("bot pool", () => {
  it("summarizes bot and task slots separately", () => {
    const pools = {
      tasks: { used: 5, total: 5 },
      bots: { used: 1, total: 2, configured: 2 },
    };
    expect(poolSummary({ admitting: true, pools })).toBe("Bot slots 1/2 · Task slots 5/5");
    expect(
      poolSummary({
        admitting: true,
        pools: { ...pools, bots: { used: 0, total: null, configured: null } },
      }),
    ).toBe("Bot slots 0/? (one per active bot) · Task slots 5/5");
    // An older dispatcher reports no pools.
    expect(poolSummary({ admitting: true })).toBeNull();
    expect(poolSummary(null)).toBeNull();
  });
  it("reports a hold only at the bot floor", () => {
    expect(botHold({ admitting: false, bots_admitting: true })).toBeNull();
    expect(botHold({ admitting: false })).toBeNull();
    expect(
      botHold({ admitting: false, bots_admitting: false, bots_reason: "memory 5.0% available" }),
    ).toBe("memory 5.0% available");
  });
});
