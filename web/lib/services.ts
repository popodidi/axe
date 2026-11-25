// The Services page model: bot services (crates/axe/src/service.rs) as the
// dashboard shows them, per node. The labels match `Service::summary` (bot
// /status and `axe bot show`).
import type { Service } from "./api";

export type { Service };

// A dispatcher reports its services every few seconds; an older report
// reads as stale (`service::STALE_SECONDS`).
export const STALE_SECONDS = 60;

// As `Service::kind`: a service with a schedule is a scheduled (cron)
// service and reads its own fields (next run, last run, exit code).
export type ServiceKind = "long-running" | "scheduled";
export const serviceKind = (s: Service): ServiceKind => (s.schedule ? "scheduled" : "long-running");

// "*/5 * * * * UTC" for a scheduled service, else null.
export const scheduleText = (s: Service): string | null =>
  s.schedule ? `${s.schedule.cron} ${s.schedule.timezone}` : null;

// As `Service::display_state`.
export function serviceState(s: Service): string {
  if (s.status.crash_loop_since && s.enabled) return "crash-looping";
  if (!s.status.state) return s.enabled ? "pending" : "stopped";
  return s.status.state;
}

// Tone of a state for its label: ok, attention, problem or neutral.
export function stateTone(state: string): "ok" | "warn" | "bad" | "idle" {
  if (state === "running") return "ok";
  if (state === "crash-looping") return "bad";
  if (["starting", "backoff", "stopping", "pending"].includes(state)) return "warn";
  return "idle";
}

const epoch = (at?: string) => {
  const t = at ? Date.parse(at) : NaN;
  return Number.isFinite(t) ? Math.floor(t / 1000) : null;
};

// `45s`, `12m`, `3h 5m`, `2d 4h` (as `board::age`, one more unit).
export function span(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) {
    const m = Math.floor((s % 3600) / 60);
    return `${Math.floor(s / 3600)}h${m ? ` ${m}m` : ""}`;
  }
  const h = Math.floor((s % 86400) / 3600);
  return `${Math.floor(s / 86400)}d${h ? ` ${h}h` : ""}`;
}

// The time column: uptime while running, the next restart while waiting
// to restart, else a dash; a scheduled service's next run. `now` is epoch
// seconds.
export function serviceTime(s: Service, now: number): string {
  const state = serviceState(s);
  const started = epoch(s.status.started_at);
  if (s.schedule) {
    if (state === "running" && started !== null) return `running ${span(now - started)}`;
    const next = epoch(s.status.next_run_at);
    return s.enabled && next !== null ? `next run in ${span(next - now)}` : "—";
  }
  if (state === "running" && started !== null) return `up ${span(now - started)}`;
  const next = epoch(s.status.next_start_at);
  if ((state === "backoff" || state === "crash-looping") && next !== null)
    return `restart in ${span(next - now)}`;
  return "—";
}

// The last-exit column: how the last process ended; for a scheduled
// service also when its last run started ("exit 0, 3m ago").
export function lastRun(s: Service, now: number): string {
  const exit = s.status.last_exit || (s.status.error ? "start failed" : "");
  const started = epoch(s.status.started_at);
  if (!s.schedule || started === null || serviceState(s) === "running") return exit || "—";
  return `${exit || "no exit"}, ${span(now - started)} ago`;
}

// Seconds since the last report when it is stale, else null.
export function staleFor(s: Service, now: number): number | null {
  const at = epoch(s.status.reported_at);
  return at !== null && now - at > STALE_SECONDS ? now - at : null;
}

export const serviceKey = (s: Pick<Service, "bot" | "name">) => `${s.bot}/${s.name}`;

// Services per node (the dispatcher that last reported them), nodes by
// name, then the ones no node reported yet. Inside a node: by bot, name.
export function byNode(services: Service[]): { node: string | null; services: Service[] }[] {
  const groups = new Map<string | null, Service[]>();
  for (const s of services) {
    const node = s.status.dispatcher || null;
    groups.set(node, [...(groups.get(node) || []), s]);
  }
  return Array.from(groups, ([node, services]) => ({
    node,
    services: services.sort((a, b) => serviceKey(a).localeCompare(serviceKey(b))),
  })).sort((a, b) =>
    a.node === null ? 1 : b.node === null ? -1 : a.node.localeCompare(b.node),
  );
}

// Every state serviceState() can return, as the list's State filter.
export const serviceStates = [
  "running",
  "scheduled",
  "crash-looping",
  "backoff",
  "starting",
  "stopping",
  "pending",
  "exited",
  "stopped",
];
