// The Services page model: bot services (crates/axe/src/service.rs) as the
// dashboard shows them, per node. The labels match `Service::summary` (bot
// /status and `axe bot show`).
import type { Service } from "./api";

export type { Service };

// A dispatcher reports its services every few seconds; an older report
// reads as stale (`service::STALE_SECONDS`).
export const STALE_SECONDS = 60;

// "long-running"; scheduled (cron) services are the other kind once they
// exist, and read their own fields (next run, last run, exit code).
export type ServiceKind = "long-running" | "scheduled";
export const serviceKind = (s: Service): ServiceKind =>
  "schedule" in s && (s as { schedule?: unknown }).schedule ? "scheduled" : "long-running";

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
// to restart, else a dash. `now` is epoch seconds.
export function serviceTime(s: Service, now: number): string {
  const state = serviceState(s);
  const started = epoch(s.status.started_at);
  if (state === "running" && started !== null) return `up ${span(now - started)}`;
  const next = epoch(s.status.next_start_at);
  if ((state === "backoff" || state === "crash-looping") && next !== null)
    return `restart in ${span(next - now)}`;
  return "—";
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
  "crash-looping",
  "backoff",
  "starting",
  "stopping",
  "pending",
  "exited",
  "stopped",
];
