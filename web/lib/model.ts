export type Task = {
  id: number;
  project: string;
  title: string;
  detail: string;
  state: string;
  created_at: string;
  updated_at: string;
  runs: number;
  /** API-derived: `blocked` also includes pending timers and dependency waits. */
  status?: string;
  not_before?: string | null;
  /** Derived: prerequisites that are not done yet. */
  waiting_on?: number[];
  deps?: { blocked_by: DepRef[]; blocks: DepRef[] };
};
/** The other end of a dependency edge, with its own status. */
export type DepRef = {
  id: number;
  project: string;
  title: string;
  state: string;
  status: string;
};
export type Event = {
  id: number;
  at: string;
  actor: string;
  kind: string;
  body: string | null;
  from_value: string | null;
  to_value: string | null;
};
export type Run = {
  id: number;
  task_id: number;
  project: string;
  agent: string;
  mode: string;
  started_at: string;
  ended_at: string | null;
  exit_code: number | null;
  current_activity: string | null;
  last_activity_at: string | null;
  log_path: string;
  session_id: string | null;
  /** Session directory when it is not the task worktree (bot runs). */
  cwd?: string;
};
export const states = ["pending", "waiting", "in_progress", "blocked", "escalated", "done"];
type TaskStatus = Pick<Task, "state" | "status" | "waiting_on" | "not_before">;
/** Stable display category for filters, counts and styling; deps take priority. */
export function taskStatus(t: TaskStatus) {
  if (t.state !== "pending") return t.status || t.state;
  if (t.waiting_on?.length) return "blocked";
  if (t.not_before && Date.parse(t.not_before) > Date.now()) return "waiting";
  // Expired wake times must not retain the API's stale derived blocked status.
  if (t.not_before) return "pending";
  return t.status || t.state;
}
export function taskStatusTitle(t: TaskStatus) {
  return taskStatus(t) === "waiting" ? new Date(t.not_before!).toISOString() : undefined;
}
export function taskStatusLabel(t: TaskStatus) {
  const status = taskStatus(t);
  if (status === "waiting")
    return "waiting until " + new Date(t.not_before!).toLocaleString(undefined, {
      month: "short", day: "numeric", hour: "numeric", minute: "2-digit",
    });
  if (status === "blocked" && t.waiting_on?.length)
    return `blocked (${t.state === "blocked" ? "hold+deps" : "deps"} ${idList(t.waiting_on)})`;
  return status.replace("_", " ");
}
const idList = (ids: number[]) => ids.map((i) => "#" + i).join(", ");
/** Why the task cannot be claimed, or null when it is ready, running, or done. */
export function waitReason(t: Task) {
  const w = t.waiting_on || [];
  const deps = w.length ? "waiting on prerequisites " + idList(w) : "";
  if (t.state === "escalated")
    return "Escalated: ops/human intervention required; explicitly resume" + (deps ? "; " + deps + "; both must clear" : "");
  if (t.state === "blocked")
    return deps
      ? `Hold (resolve it explicitly) and ${deps}; both must clear`
      : "Hold: resolve it explicitly, then set pending";
  if (taskStatus(t) === "waiting")
    return `${taskStatusLabel(t).replace(/^waiting/, "Waiting")}; becomes ready automatically`;
  if (!deps) return null;
  return t.state === "pending"
    ? `${deps[0].toUpperCase() + deps.slice(1)}; becomes ready automatically`
    : `${deps[0].toUpperCase() + deps.slice(1)} (prerequisite reopened; not cancelled)`;
}
/** Shared desk/history filters; state is only a task-table filter. */
export function matchesTask(t: Task, p: URLSearchParams) {
  return (
    (!p.get("project") || t.project === p.get("project")) &&
    `${t.id} ${t.title} ${t.detail}`
      .toLowerCase()
      .includes((p.get("q") || "").toLowerCase())
  );
}
export const taskSortColumns = [
  { key: "id", label: "ID" },
  { key: "title", label: "Task" },
  { key: "project", label: "Project" },
  { key: "state", label: "State" },
  { key: "created_at", label: "Created" },
] as const;
export function taskSort(value: string | null) {
  if (value === "oldest" || value === "newest")
    return { key: "created_at", direction: value === "oldest" ? "asc" : "desc" };
  const [key, direction] = (value || "").split(":");
  return taskSortColumns.some((column) => column.key === key) &&
    (direction === "asc" || direction === "desc")
    ? { key, direction }
    : null;
}
function compareTasks(a: Task, b: Task, value: string | null) {
  const sort = taskSort(value);
  // Preserve the original default order and legacy URL aliases exactly.
  if (!sort || value === "oldest" || value === "newest")
    return (value === "oldest" ? 1 : -1) *
      (a.created_at.localeCompare(b.created_at) || a.id - b.id);
  const read = (task: Task): string | number | null => {
    if (sort.key === "id") return task.id;
    if (sort.key === "created_at") {
      const time = Date.parse(task.created_at);
      return Number.isFinite(time) ? time : null;
    }
    return (sort.key === "state" ? taskStatus(task).replace("_", " ") :
      task[sort.key as "title" | "project"]).trim() || null;
  };
  const left = read(a), right = read(b);
  // Empty values always trail populated values; ties always use ascending ID.
  if (left === null || right === null)
    return left === right ? a.id - b.id : left === null ? 1 : -1;
  const order = typeof left === "number" && typeof right === "number"
    ? left - right
    : String(left).localeCompare(String(right), "en", { sensitivity: "base" });
  return order * (sort.direction === "asc" ? 1 : -1) || a.id - b.id;
}
export function selectTasks(tasks: Task[], p: URLSearchParams) {
  return tasks
    .filter(
      (t) =>
        (!p.get("project") || t.project === p.get("project")) &&
        (p.get("state")
          ? p.get("state") === "all" ||
            (p.get("state") === "active"
              ? t.state !== "done"
              : taskStatus(t) === p.get("state"))
          : true) &&
        `${t.id} ${t.title} ${t.detail}`
          .toLowerCase()
          .includes((p.get("q") || "").toLowerCase()),
    )
    .sort((a, b) => compareTasks(a, b, p.get("sort")));
}
export function pageNumber(p: URLSearchParams) {
  return Math.max(1, Math.floor(Number(p.get("page")) || 1));
}
export function isDiff(e: Event) {
  return (
    e.kind === "edit" && /^--- detail\r?\n\+\+\+ detail/.test(e.body || "")
  );
}
// Same rule as hv's quote(): safe words stay bare, anything else becomes
// one literal single-quoted argument for the laptop shell.
export function shellQuote(s: string) {
  return /^[\w@%+=:,./-]+$/.test(s)
    ? s
    : "'" + s.replaceAll("'", "'\\''") + "'";
}
// `hv <project> <cmd...>` runs cmd in ~/workspace/<project> on hlab over ssh.
export function hv(project: string, ...args: string[]) {
  return ["hv", project, ...args].map(shellQuote).join(" ");
}
export const taskCommands = (t: Pick<Task, "id" | "project">) => ({
  log: hv(t.project, "taskq", "log", String(t.id)),
  spec: hv(t.project, "taskq", "show", String(t.id)),
});
// Dispatcher sessions run in the task worktree (taskq/src/worktree.rs), and
// Claude Code looks sessions up by cwd, so resume from that directory.
export function taskWorktree(r: Pick<Run, "project" | "task_id" | "cwd">) {
  if (r.cwd) return r.cwd;
  return `~/workspace/.worktrees/${shellQuote(r.project)}/task-${r.task_id}--work`;
}
export const runCommands = (r: Run) => ({
  watch: hv(r.project, "tail", "-f", r.log_path),
  attach:
    r.ended_at && r.session_id && ["codex", "claude"].includes(r.agent)
      ? hv(
          r.project,
          "sh",
          "-c",
          `cd ${taskWorktree(r)} && exec ${
            r.agent === "claude"
              ? "claude --resume"
              : "codex resume"
          } ${shellQuote(r.session_id)}`,
        )
      : null,
});
export function safeLink(s: string) {
  try {
    const u = new URL(s);
    return ["http:", "https:"].includes(u.protocol) ? u.href : undefined;
  } catch {
    return undefined;
  }
}
export function formatDuration(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return s + "s";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** Artifacts are unique by displayed value; the audit events stay untouched. */
export function selectArtifacts(events: readonly Event[]) {
  const seen = new Set<string>();
  return events.filter((event) => {
    if (event.kind !== "artifact") return false;
    const value = event.body || event.to_value || "";
    if (seen.has(value)) return false;
    seen.add(value);
    return true;
  });
}
