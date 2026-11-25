import { describe, expect, it } from "vitest";
import { byNode, lastRun, scheduleText, serviceKind, serviceState, serviceTime, span, staleFor, type Service } from "./services";

const NOW = 1_800_000_000;
const ts = (epoch: number) => new Date(epoch * 1000).toISOString();
const svc = (over: Partial<Service> = {}, status: Partial<Service["status"]> = {}): Service => ({
  bot: "b",
  home: "b",
  name: "feed",
  enabled: true,
  command: "run",
  restart: "on-failure",
  created_at: ts(NOW - 100),
  updated_at: ts(NOW - 100),
  ...over,
  status: { state: "", restarts: 0, failures: 0, ...status },
});

describe("services", () => {
  it("reads state like Service::display_state", () => {
    expect(serviceState(svc())).toBe("pending");
    expect(serviceState(svc({ enabled: false }))).toBe("stopped");
    expect(serviceState(svc({}, { state: "running" }))).toBe("running");
    expect(serviceState(svc({}, { state: "backoff", crash_loop_since: ts(NOW) }))).toBe("crash-looping");
    expect(serviceState(svc({ enabled: false }, { state: "stopping", crash_loop_since: ts(NOW) }))).toBe("stopping");
  });
  it("is scheduled with a schedule", () => {
    expect(serviceKind(svc())).toBe("long-running");
    expect(scheduleText(svc())).toBeNull();
    const sched = { cron: "*/5 * * * *", timezone: "UTC", timeout_seconds: 60, catch_up: true };
    expect(serviceKind(svc({ schedule: sched }))).toBe("scheduled");
    expect(scheduleText(svc({ schedule: sched }))).toBe("*/5 * * * * UTC");
  });
  it("shows a scheduled service's next run and last run like Service::summary", () => {
    const schedule = { cron: "*/5 * * * *", timezone: "UTC", timeout_seconds: 60, catch_up: true };
    const idle = svc(
      { schedule, restart: "never" },
      { state: "scheduled", started_at: ts(NOW - 180), last_exit: "exit 0", next_run_at: ts(NOW + 120) },
    );
    expect(serviceState(idle)).toBe("scheduled");
    expect(serviceTime(idle, NOW)).toBe("next run in 2m");
    expect(lastRun(idle, NOW)).toBe("exit 0, 3m ago");
    const running = svc({ schedule }, { state: "running", started_at: ts(NOW - 5), next_run_at: ts(NOW + 120) });
    expect(serviceTime(running, NOW)).toBe("running 5s");
    expect(serviceTime(svc({ schedule, enabled: false }, { state: "stopped", next_run_at: ts(NOW) }), NOW)).toBe("—");
    expect(lastRun(svc({ schedule }, { state: "scheduled", started_at: ts(NOW - 60), error: "cwd x" }), NOW)).toBe(
      "start failed, 1m ago",
    );
    expect(lastRun(svc({}, { last_exit: "exit 2" }), NOW)).toBe("exit 2");
  });
  it("shows uptime or the next restart", () => {
    expect(serviceTime(svc({}, { state: "running", started_at: ts(NOW - 7500) }), NOW)).toBe("up 2h 5m");
    expect(serviceTime(svc({}, { state: "backoff", next_start_at: ts(NOW + 30) }), NOW)).toBe("restart in 30s");
    expect(serviceTime(svc({}, { state: "exited" }), NOW)).toBe("—");
    expect(span(90000)).toBe("1d 1h");
  });
  it("flags stale reports", () => {
    expect(staleFor(svc({}, { reported_at: ts(NOW - 5) }), NOW)).toBeNull();
    expect(staleFor(svc({}, { reported_at: ts(NOW - 600) }), NOW)).toBe(600);
  });
  it("groups by node, unreported last", () => {
    const groups = byNode([
      svc({ name: "z" }, { dispatcher: "vm" }),
      svc({ name: "x" }),
      svc({ bot: "a", name: "y" }, { dispatcher: "vm" }),
      svc({ name: "w" }, { dispatcher: "laptop" }),
    ]);
    expect(groups.map((g) => [g.node, g.services.map((s) => `${s.bot}/${s.name}`)])).toEqual([
      ["laptop", ["b/w"]],
      ["vm", ["a/y", "b/z"]],
      [null, ["b/x"]],
    ]);
  });
});
