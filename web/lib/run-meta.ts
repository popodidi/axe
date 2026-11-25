// The run page's metadata panel: the server's `/api/runs/:id/meta` (or
// `/api/bot-runs/:id/meta`) reply and the facts derived from it, plus the
// time and size formats the trace and the panel share.

/** A file recorded beside the run's log on the node. */
export type RunFile = {
  text?: string;
  truncated?: boolean;
  bytes?: number;
  /** `run`: this run's own copy; `current`: the bot's latest (older runs). */
  source?: "run" | "current";
  unavailable?: string;
};
export type ModelUsage = {
  model: string;
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  cost: number;
};
export type RunMeta = {
  run?: Record<string, unknown>;
  task?: {
    id: number;
    title: string;
    project: string;
    state: string;
    status?: string;
    owner?: string | null;
    labels?: string[];
    repos?: string[] | null;
  };
  bot?: {
    name: string;
    project: string;
    state: string;
    agent: string;
    repos?: string[] | null;
  };
  start?: Record<string, unknown> | null;
  end?: Record<string, unknown> | null;
  outcome?: { from?: string; to?: string; note?: string; at?: string } | null;
  task_state?: string | null;
  wakes?: Record<string, number>;
  usage?: {
    status: string;
    reason?: string | null;
    turns?: number | null;
    context_tokens?: number | null;
    models: ModelUsage[];
  } | null;
  files?: {
    prompt?: RunFile;
    system_prompt?: RunFile;
    first_line?: string | null;
    times?: boolean;
    unavailable?: string;
  };
};

type Obj = Record<string, unknown>;
const obj = (v: unknown): Obj =>
  v !== null && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : {};
const str = (v: unknown) => (typeof v === "string" ? v : "");

/** The value after `flag` in `cmd`, or in a `flag=value` argument. */
function arg(cmd: string[], ...flags: string[]) {
  for (let i = 0; i < cmd.length; i++) {
    for (const f of flags) {
      if (cmd[i] === f && i + 1 < cmd.length) return cmd[i + 1];
      if (cmd[i].startsWith(f + "=")) return cmd[i].slice(f.length + 1);
    }
  }
  return "";
}

export type Runtime = {
  runtime: string;
  harness: string;
  model: string;
  effort: string;
  version: string;
  permission: string;
  rule: string;
};

/**
 * Runtime facts from what the run recorded: the start event's runtime,
 * harness, rule and `use` (`harness[:model[:effort]]`), its command, and the
 * harness's init record (the log's first line). Empty when not recorded.
 */
export function runtimeOf(meta: RunMeta | undefined, agent = ""): Runtime {
  const start = obj(meta?.start);
  const cmd = Array.isArray(start.cmd) ? start.cmd.map(String) : [];
  const [useHarness = "", useModel = "", useEffort = ""] = str(start.use).split(
    ":",
  );
  let init: Obj = {};
  try {
    init = obj(JSON.parse(meta?.files?.first_line || ""));
  } catch {
    /* A plain-text first line (Codex) names nothing. */
  }
  const codexEffort = /^model_reasoning_effort=(.+)$/.exec(
    arg(cmd, "-c", "--config"),
  )?.[1];
  return {
    runtime: str(start.runtime) || str(start.agent) || agent,
    harness: str(start.harness) || useHarness,
    model: str(init.model) || useModel || arg(cmd, "--model", "-m"),
    effort:
      useEffort ||
      arg(cmd, "--effort") ||
      (codexEffort ?? "").replace(/"/g, ""),
    version: str(init.claude_code_version) || str(init.version),
    permission: str(init.permissionMode),
    rule: str(start.rule),
  };
}

const pad = (n: number) => String(n).padStart(2, "0");

/** Short local time (`14:03:22`), with the date when it is not `day`'s. */
export function clock(ms: number, day?: number) {
  const d = new Date(ms);
  const t = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  if (day === undefined || new Date(day).toDateString() === d.toDateString())
    return t;
  return `${d.toLocaleDateString(undefined, { month: "short", day: "numeric" })} ${t}`;
}

/** Full local timestamp, for a tooltip or a `dateTime`-adjacent label. */
export function fullTime(ms: number) {
  return new Date(ms).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    timeZoneName: "short",
  });
}

/** `850ms`, `12s`, `3m 05s`, `1h 02m`. */
export function duration(ms: number) {
  if (!(ms >= 0)) return "";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${pad(s % 60)}s`;
  return `${Math.floor(s / 3600)}h ${pad(Math.floor((s % 3600) / 60))}m`;
}

/** `950`, `34.5k`, `1.2M`. */
export function count(n: number) {
  if (n < 1000) return String(n);
  if (n < 1e6) return `${(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}k`;
  return `${(n / 1e6).toFixed(1)}M`;
}

/** `512 B`, `12 KB`, `1.4 MB`. */
export function size(n: number) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** Milliseconds of an RFC 3339 time; `undefined` when absent or invalid. */
export function epochMs(s: unknown) {
  const ms = typeof s === "string" ? Date.parse(s) : NaN;
  return Number.isFinite(ms) ? ms : undefined;
}

/** Usage totals across models; `undefined` when none was recorded. */
export function usageTotals(meta: RunMeta | undefined) {
  const models = meta?.usage?.models;
  if (!models?.length) return undefined;
  return models.reduce(
    (t, m) => ({
      input: t.input + m.input,
      output: t.output + m.output,
      cache_read: t.cache_read + m.cache_read,
      cache_write: t.cache_write + m.cache_write,
      cost: t.cost + m.cost,
    }),
    { input: 0, output: 0, cache_read: 0, cache_write: 0, cost: 0 },
  );
}
