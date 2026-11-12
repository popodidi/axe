"use client";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { useSearchParams } from "next/navigation";
import Markdown from "./markdown";
import styles from "./dashboard.module.css";
import {
  Alert,
  Button,
  Command,
  Commands,
  DiffBlock,
  EmptyState,
  Filter,
  Kbd,
  Panel,
  Search,
  Skeleton,
  StatusPill,
  Table,
  Tabs,
  Tag,
  Td,
  Th,
  choices,
  cx,
} from "../components/ui";
import { ArtifactTable } from "../components/dashboard/artifact-table";
import { RunList } from "../components/dashboard/run-card";
import { RunLog } from "../components/dashboard/run-log";
import {
  RoutineBacklink,
  RoutineList,
  RoutinePane,
  routineStatuses,
  useRoutines,
} from "../components/dashboard/routines";
import { Cost } from "../components/dashboard/cost";
import { Stats } from "../components/dashboard/stats";
import { BotDetail, BotList, useBots } from "../components/dashboard/bots";
import {
  GoalDetailPage,
  GoalList,
  useGoals,
} from "../components/dashboard/goals";
import { Dependencies } from "../components/dashboard/dependencies";
import {
  DetailResizer,
  PaneResizer,
} from "../components/dashboard/detail-resizer";
import { usePaneScroll, snapshotPaneScroll } from "../lib/use-pane-scroll";
import { useTheme } from "../lib/theme";
import { useNavCollapsed } from "../lib/layout";
import { motionMs, useMotionReady } from "../lib/motion";
import {
  type Task,
  type Event,
  type Run,
  states,
  selectTasks,
  taskSort,
  taskSortColumns,
  selectArtifacts,
  pageNumber,
  isDiff,
  taskCommands,
} from "../lib/model";
const when = (s: string) =>
  new Date(s).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
const views = ["tasks", "routines", "bots", "goals", "stats", "cost"];
const labels = ["Tasks", "Routines", "Bots", "Goals", "Stats", "Cost"];
const keys = ["t", "r", "b", "o", "s", "c"];
type Snapshot = {
  tasks: Task[];
};
// Desktop detail pane lifecycle: "enter" mounts
// at zero width, "opening"/"closing" run the 275ms boundary transition, null
// is at rest (no width transition, so resizing follows the pointer exactly).
type Phase = "enter" | "opening" | "closing" | null;
// True while a URL change made by this app (update) is being rendered; cleared
// after each URL render. A change without it (Back/Forward) is a restore. Kept
// outside React state: the router renders a popstate URL before any listener
// of ours could record it.
let userNavigation = false;
export default function Dashboard() {
  const search = useSearchParams();
  const query = search.toString();
  const p = new URLSearchParams(query);
  // A bare `?bot=<name>` or `?goal=<name>` link opens the Bots or Goals view.
  const view = views.includes(p.get("view") || "")
    ? p.get("view")!
    : p.has("bot")
      ? "bots"
      : p.has("goal")
        ? "goals"
        : "tasks";
  if (!p.has("state")) p.set("state", "active");
  const selected = p.get("task");
  const watched = p.get("run");
  const botRun = p.get("botrun");
  const overlay = !!(watched || botRun);
  const tab = p.get("tab") || "spec";
  // The embedded UI talks to the server that serves it; `?server=` points a
  // standalone build at another axe server (which must allow it in CORS).
  const base = (
    p.get("server") || (typeof location === "undefined" ? "" : location.origin)
  ).replace(/\/+$/, "");
  const [data, setData] = useState<Snapshot | null>(null);
  const [detail, setDetail] = useState<{
    task: Task;
    events: Event[];
    runs: Run[];
  } | null>(null);
  const [error, setError] = useState("");
  const [detailError, setDetailError] = useState("");
  const [connection, setConnection] = useState("Connecting");
  const [now, setClock] = useState(() => Date.now());
  // Bumped on every server change (SSE, or polling while disconnected) so views
  // with their own reads (Bots) reload with the task list.
  const [revision, setRevision] = useState(0);
  const searchRef = useRef<HTMLInputElement>(null);
  const prefix = useRef(0);
  const reload = useRef<() => void>(() => {});
  // Back/Forward restores geometry without animating the detail pane.
  useEffect(() => {
    userNavigation = false;
  }, [query]);
  const update = useCallback(
    (patch: Record<string, string | null>, replace = false) => {
      userNavigation = true;
      const u = new URL(location.href);
      snapshotPaneScroll(u);
      if (u.href !== location.href) history.replaceState(history.state, "", u);
      if ("task" in patch || "tab" in patch) u.searchParams.delete("detailY");
      if (
        ["view", "project", "state", "status", "q", "sort", "page", "bot", "goal"].some(
          (k) => k in patch,
        )
      )
        u.searchParams.delete("listY");
      Object.entries(patch).forEach(([k, v]) =>
        v ? u.searchParams.set(k, v) : u.searchParams.delete(k),
      );
      window.history[replace ? "replaceState" : "pushState"](null, "", u);
    },
    [],
  );
  useEffect(() => {
    const u = new URL(location.href);
    if (!views.includes(u.searchParams.get("view") || "")) {
      u.searchParams.set(
        "view",
        u.searchParams.has("bot")
          ? "bots"
          : u.searchParams.has("goal")
            ? "goals"
            : "tasks",
      );
      if (!u.searchParams.has("state")) u.searchParams.set("state", "active");
      window.history.replaceState(null, "", u);
    }
  }, [query]);
  useEffect(() => {
    let disposed = false;
    let busy = false;
    let again = false;
    const controller = new AbortController();
    async function get<T>(path: string): Promise<T> {
      const r = await fetch(base + path, { signal: controller.signal });
      if (!r.ok) throw Error(`${path}: HTTP ${r.status}`);
      return r.json();
    }
    async function load() {
      if (busy) {
        again = true;
        return;
      }
      busy = true;
      try {
        const tasks = await get<Task[]>("/api/tasks?all=1");
        if (!disposed) {
          setData({
            tasks,
          });
          setError("");
        }
        if (selected) {
          try {
            const [task, events, runs] = await Promise.all([
              get<Task>("/api/tasks/" + selected),
              get<Event[]>("/api/tasks/" + selected + "/events"),
              get<Run[]>("/api/runs?task=" + selected + "&limit=1000"),
            ]);
            if (!disposed) {
              setDetail({ task, events, runs });
              setDetailError("");
            }
          } catch (e) {
            if (!disposed) setDetailError(String(e));
          }
        }
      } catch (e) {
        if (!disposed) setError(String(e));
      } finally {
        busy = false;
        if (again && !disposed) {
          again = false;
          void load();
        }
      }
    }
    void load();
    const changed = () => {
      setRevision((n) => n + 1);
      void load();
    };
    reload.current = changed;
    const stream = new EventSource(base + "/api/stream");
    let live = false;
    stream.onopen = () => {
      live = true;
      setConnection("Live · SSE");
    };
    stream.addEventListener("change", changed);
    stream.onerror = () => {
      live = false;
      setConnection("Reconnecting · polling 5s");
    };
    const poll = setInterval(() => {
      if (!live) changed();
    }, 5000);
    const tick = setInterval(() => setClock(Date.now()), 1000);
    return () => {
      disposed = true;
      controller.abort();
      stream.close();
      clearInterval(poll);
      clearInterval(tick);
    };
  }, [base, selected]);
  const sorting = taskSort(p.get("sort"));
  const filtered = selectTasks(data?.tasks || [], p);
  const page = pageNumber(p);
  const pages = Math.max(1, Math.ceil(filtered.length / 25));
  const rows = filtered.slice((page - 1) * 25, page * 25);
  const focus = Number(p.get("focus"));
  // The newest open/close/reselect wins; a closing pane keeps the last task
  // mounted (inert) until its transition ends, then unmounts.
  const [last, setLast] = useState(selected);
  const [exiting, setExiting] = useState<string | null>(null);
  const [phase, setPhase] = useState<Phase>(null);
  if (selected !== last) {
    setLast(selected);
    if (!userNavigation) {
      setExiting(null);
      setPhase(null);
    } else if (selected) {
      setExiting(null);
      setPhase(!last && !exiting ? "enter" : exiting ? "opening" : phase);
    } else if (last) {
      setExiting(last);
      setPhase("closing");
    }
  }
  const shown = selected || exiting;
  useEffect(() => {
    if (phase !== "enter") return;
    // Paint the zero width once, then let the boundary transition run.
    const frame = requestAnimationFrame(() =>
      // A queued frame can run after a newer close has committed, before
      // effect cleanup cancels it. Never reopen that closing pane.
      setPhase((current) => (current === "enter" ? "opening" : current)),
    );
    return () => cancelAnimationFrame(frame);
  }, [phase]);
  useEffect(() => {
    if (phase !== "opening" && phase !== "closing") return;
    // Timer cleanup, never transitionend, so zero-duration motion still ends.
    const done = setTimeout(() => {
      setPhase(null);
      if (phase === "closing") setExiting(null);
    }, motionMs("--duration-layout"));
    return () => clearTimeout(done);
  }, [phase]);
  useMotionReady();
  const listScroll = usePaneScroll(
    query,
    "listY",
    !!data,
    focus ? "task-" + focus : undefined,
  );
  const detailScroll = usePaneScroll(
    query,
    "detailY",
    !!selected && detail?.task.id === Number(selected),
  );
  // Focus returns to the invoking row before the pane becomes inert; when the
  // row is hidden (phone) or gone, it moves after the list reappears.
  const returnFocus = useRef<string | null>(null);
  const focusReturnTarget = (id: string) => {
    // The invoking control: a task row, or a task link in the routine pane.
    const row = document.querySelector<HTMLElement>(
      `#task-${id} button, [data-open-task="${id}"]`,
    );
    const target =
      row && row.getClientRects().length
        ? row
        : document.getElementById("list-search");
    target?.focus({ preventScroll: !!row });
    return !!target && document.activeElement === target;
  };
  // Closing drops the tab too, so the next task opens on its spec.
  const closeTask = useCallback(() => {
    const id = new URLSearchParams(location.search).get("task");
    if (!id) return;
    if (
      document.getElementById("task-detail")?.contains(document.activeElement)
    ) {
      returnFocus.current = id;
      // Hidden rows cannot take focus; retried after the list is shown.
      if (focusReturnTarget(id)) returnFocus.current = null;
    }
    update({ task: null, tab: null });
  }, [update]);
  useLayoutEffect(() => {
    if (selected || !returnFocus.current) return;
    focusReturnTarget(returnFocus.current);
    returnFocus.current = null;
  }, [selected]);
  // Routines: the routine pane, with a task pane stacked on its right.
  const routineId = view === "routines" ? p.get("routine") : null;
  const routines = useRoutines(base, view === "routines");
  const closeRoutine = useCallback(() => {
    const id = new URLSearchParams(location.search).get("routine");
    if (!id) return;
    if (
      document.getElementById("routine-detail")?.contains(document.activeElement)
    )
      document.querySelector<HTMLElement>(`#routine-${id} button`)?.focus();
    update({ routine: null, task: null, tab: null });
  }, [update]);
  // Bots: the list, or one bot's detail page in its place.
  const botName = view === "bots" ? p.get("bot") : null;
  const bots = useBots(base, view === "bots" && !botName, revision);
  // Closing returns focus to the bot's row once the list has loaded.
  const botFocus = useRef<string | null>(null);
  const closeBot = useCallback(() => {
    const name = new URLSearchParams(location.search).get("bot");
    if (!name) return;
    botFocus.current = name;
    update({ bot: null, tab: null, days: null });
  }, [update]);
  useEffect(() => {
    if (botName || !botFocus.current) return;
    const row = document.querySelector<HTMLElement>(
      `#bot-${CSS.escape(botFocus.current)} button`,
    );
    if (!row) return;
    row.focus();
    botFocus.current = null;
  }, [botName, bots.bots]);
  // Goals: the list, or one goal's detail page in its place.
  const goalName = view === "goals" ? p.get("goal") : null;
  const goals = useGoals(base, view === "goals" && !goalName, revision);
  const goalFocus = useRef<string | null>(null);
  const closeGoal = useCallback(() => {
    const name = new URLSearchParams(location.search).get("goal");
    if (!name) return;
    goalFocus.current = name;
    update({ goal: null });
  }, [update]);
  useEffect(() => {
    if (goalName || !goalFocus.current) return;
    const row = document.querySelector<HTMLElement>(
      `#goal-${CSS.escape(goalFocus.current)} button`,
    );
    if (!row) return;
    row.focus();
    goalFocus.current = null;
  }, [goalName, goals.goals]);
  // Keyboard opening moves focus to the detail heading once it is mounted.
  const focusDetail = useRef<number | null>(null);
  useEffect(() => {
    const fn = (e: KeyboardEvent) => {
      if (
        e.ctrlKey ||
        e.altKey ||
        e.metaKey ||
        (e.target instanceof HTMLElement &&
          (e.target.matches("input,textarea,select,[role=combobox]") ||
            e.target.isContentEditable))
      )
        return;
      const overlaid = new URLSearchParams(location.search);
      if (overlaid.has("run") || overlaid.has("botrun")) return;
      if (e.key === "g") {
        prefix.current = Date.now();
        return;
      }
      if (Date.now() - prefix.current < 1000) {
        const v: Record<string, string> = {
          t: "tasks",
          v: "tasks",
          s: "stats",
          c: "cost",
          r: "routines",
          b: "bots",
          o: "goals",
        };
        if (v[e.key]) {
          update({ view: v[e.key], task: null, focus: null, page: null, bot: null, goal: null });
          prefix.current = 0;
          return;
        }
      }
      if (e.key === "/") {
        e.preventDefault();
        searchRef.current?.focus();
      }
      // Escape closes the topmost pane: the task, then its routine.
      if (e.key === "Escape") {
        const q = new URLSearchParams(location.search);
        if (q.has("task")) closeTask();
        else if (q.get("view") === "routines" && q.has("routine"))
          closeRoutine();
        else if (q.has("bot")) closeBot();
        else if (q.has("goal")) closeGoal();
      }
      if (view === "tasks") {
        if (e.key === "j" || e.key === "k") {
          e.preventDefault();
          const currentFocus = Number(
            new URLSearchParams(location.search).get("focus"),
          );
          const i = rows.findIndex((t) => t.id === currentFocus);
          const n = Math.max(
            0,
            Math.min(rows.length - 1, i + (e.key === "j" ? 1 : -1)),
          );
          if (rows[n]) {
            update({ focus: String(rows[n].id) }, true);
          }
        }
        if (e.key === "Enter") {
          const id = new URLSearchParams(location.search).get("focus");
          if (id) {
            e.preventDefault();
            focusDetail.current = Number(id);
            update({ task: id });
          }
        }
      }
    };
    window.addEventListener("keydown", fn);
    return () => window.removeEventListener("keydown", fn);
  }, [rows, focus, view, update, closeTask, closeRoutine, closeBot, closeGoal]);
  // Polite announcement; failures are also shown, and the button stays "Copy".
  const [copied, setCopied] = useState({ text: "", failed: false });
  const announce = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(announce.current), []);
  async function copy(text: string) {
    clearTimeout(announce.current);
    try {
      if (navigator.clipboard && window.isSecureContext)
        await navigator.clipboard.writeText(text);
      else {
        const el = document.createElement("textarea");
        el.value = text;
        document.body.appendChild(el);
        el.select();
        const ok = document.execCommand("copy");
        el.remove();
        if (!ok) throw Error("Copy unavailable");
      }
      setCopied({ text: "Copied command", failed: false });
      announce.current = setTimeout(
        () => setCopied({ text: "", failed: false }),
        1500,
      );
      return true;
    } catch {
      setCopied({ text: "Copy failed; select the command text", failed: true });
      return false;
    }
  }
  const activeDetail = detail?.task.id === Number(shown) ? detail : null;
  const detailHeading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    if (!activeDetail || focusDetail.current !== activeDetail.task.id) return;
    focusDetail.current = null;
    detailHeading.current?.focus();
  }, [activeDetail]);
  const artifacts = selectArtifacts(activeDetail?.events || []);
  const changeFilter = (k: string, v: string) =>
    update({ [k]: v, page: null, focus: null });
  const openTask = (id: number) => update({ task: String(id) });
  const live = connection.startsWith("Live");
  const [light, toggleTheme] = useTheme();
  const [collapsed, toggleNav] = useNavCollapsed();
  const titles = new Map((data?.tasks || []).map((t) => [t.id, t.title]));
  return (
    <div className={styles.app}>
      {watched && (
        <RunLog
          key={base + watched}
          id={watched}
          base={base}
          onClose={() => update({ run: null })}
        />
      )}
      {botRun && !watched && (
        <RunLog
          key={base + "bot" + botRun}
          id={botRun}
          base={base}
          kind="bot"
          onClose={() => update({ botrun: null })}
        />
      )}
      <aside id="app-nav" className={styles.nav} inert={overlay}>
        <div className={styles.brandRow}>
          <div className={styles.brand}>
            <span aria-hidden className={styles.brandMark}>
              A
            </span>
            <div className={styles.brandText}>
              <div className={styles.brandName}>axe</div>
              <div className={styles.brandMeta}>Task queue</div>
            </div>
          </div>
          {/* Wide screens only; phones use the top bar. */}
          <Button
            variant="ghost"
            size="sm"
            icon
            className={styles.collapse}
            aria-controls="app-nav"
            aria-expanded={!collapsed}
            aria-label={collapsed ? "Expand navigation" : "Collapse navigation"}
            title={collapsed ? "Expand navigation" : "Collapse navigation"}
            onClick={toggleNav}
          >
            <svg aria-hidden="true" viewBox="0 0 16 16" className={styles.collapseIcon}>
              <path d={collapsed ? "M6 4l4 4-4 4" : "M10 4L6 8l4 4"} />
            </svg>
          </Button>
        </div>
        <nav className={styles.navList} aria-label="Views">
          {views.map((v, i) => (
            <Button
              key={v}
              variant="nav"
              title={`${labels[i]} (g ${keys[i]})`}
              aria-current={view === v ? "page" : undefined}
              onClick={() =>
                update({ view: v, task: null, page: null, focus: null, bot: null, goal: null })
              }
            >
              <span className={styles.navLabel}>{labels[i]}</span>
              {/* Collapsed rail initial; the accessible name stays the label. */}
              <span aria-hidden className={styles.navGlyph}>
                {labels[i][0]}
              </span>
              {/* Hint only; the accessible name stays the view label. */}
              <span aria-hidden className={styles.hint}>
                <Kbd>g {keys[i]}</Kbd>
              </span>
            </Button>
          ))}
        </nav>
        <footer className={styles.navFoot}>
          <div className={styles.status}>
            <span
              className={styles.connection}
              data-live={live}
              role="status"
              title={connection}
            >
              <span className={styles.navLabel}>{connection}</span>
            </span>
            <Button
              variant="ghost"
              icon
              aria-label="Refresh now"
              title="Refresh now"
              onClick={() => reload.current()}
            >
              ↻
            </Button>
          </div>
          <span className={styles.hideNarrow}>
            server <span className={styles.mono}>{base}</span>
          </span>
          <span className={styles.hideNarrow}>Read-only</span>
          <Button
            variant="ghost"
            className={styles.theme}
            aria-pressed={light}
            title="Switch between dark and light"
            onClick={toggleTheme}
          >
            <span className={styles.navLabel}>Light theme</span>
            <span aria-hidden className={styles.navGlyph}>
              ◐
            </span>
          </Button>
        </footer>
      </aside>
      {!collapsed && (
        <PaneResizer
          controls="app-nav"
          label="Resize navigation"
          pane="nav"
          side="prev"
        />
      )}
      <main
        className={cx(
          styles.main,
          (selected || routineId) && styles.hasDetail,
          routineId && styles.hasRoutine,
          routineId && shown && styles.stacked,
        )}
        inert={overlay}
      >
        <div className={styles.workspace}>
          <section
            {...listScroll}
            className={styles.listPane}
            aria-label="Task workspace"
          >
            {view !== "stats" && view !== "cost" && !botName && !goalName && (
            <div className={styles.filters}>
              <Search
                compact
                id="list-search"
                ref={searchRef}
                className={styles.search}
                aria-label={`Search ${["routines", "bots", "goals"].includes(view) ? view : "tasks"}`}
                placeholder={`Search ${["routines", "bots", "goals"].includes(view) ? view : "tasks"}…  /`}
                value={p.get("q") || ""}
                onChange={(e) => changeFilter("q", e.target.value)}
              />
              {(view === "tasks" ? ["project", "state"] : ["project"]).map(
                (k) => (
                  <Filter
                    key={k}
                    label={k}
                    aria-label={k}
                    value={p.get(k) || ""}
                    onChange={(v) =>
                      changeFilter(k, k === "state" && !v ? "all" : v)
                    }
                    options={[
                      {
                        value: "",
                        label: k === "state" ? "All (incl. done)" : "All",
                      },
                      ...choices(
                        k === "state"
                          ? ["active", "all", ...states]
                          : Array.from(
                              new Set(
                                view === "bots"
                                  ? (bots.bots || []).map((b) => b.project)
                                  : view === "goals"
                                    ? (goals.goals || []).map((g) => g.project)
                                    : (data?.tasks || []).map((t) => t.project),
                              ),
                            ).sort(),
                      ),
                    ]}
                  />
                ),
              )}
              {view === "routines" && (
                <Filter
                  label="status"
                  aria-label="status"
                  value={p.get("status") || ""}
                  onChange={(v) => changeFilter("status", v)}
                  options={[{ value: "", label: "All" }, ...routineStatuses]}
                />
              )}
              {view === "tasks" && (
                <Filter
                  label="Sort"
                  aria-label="Sort"
                  value={p.get("sort") || "newest"}
                  onChange={(v) => changeFilter("sort", v)}
                  options={[
                    { value: "newest", label: "Newest first" },
                    { value: "oldest", label: "Oldest first" },
                    ...taskSortColumns.flatMap(({ key, label }) => [
                      { value: `${key}:asc`, label: `${label}: ascending` },
                      { value: `${key}:desc`, label: `${label}: descending` },
                    ]),
                  ]}
                />
              )}
            </div>
            )}
            {error && (
              <div className={styles.alert}>
                <Alert>
                  Unable to refresh: {error}. Check the server address and CORS.
                  Retrying automatically.
                </Alert>
              </div>
            )}
            {botName ? (
              <BotDetail
                key={base + botName}
                base={base}
                name={botName}
                params={p}
                revision={revision}
                now={now}
                onBack={closeBot}
                onChange={(patch) => update(patch)}
                onWatch={(id) => update({ botrun: String(id) })}
                onCopy={copy}
              />
            ) : goalName ? (
              <GoalDetailPage
                key={base + goalName}
                base={base}
                name={goalName}
                revision={revision}
                onBack={closeGoal}
                onOpenTask={(id) =>
                  update({ view: "tasks", goal: null, task: String(id), tab: null })
                }
                onCopy={copy}
              />
            ) : view === "goals" ? (
              <GoalList
                state={goals}
                params={p}
                onOpen={(name) => update({ goal: name })}
              />
            ) : view === "bots" ? (
              <BotList
                state={bots}
                params={p}
                onOpen={(name) => update({ bot: name, tab: null })}
              />
            ) : view === "routines" ? (
              <RoutineList
                state={routines}
                params={p}
                onOpen={(id) =>
                  update({ routine: String(id), task: null, tab: null })
                }
                onOpenTask={(id) => update({ task: String(id), tab: null })}
              />
            ) : view === "cost" ? (
              <Cost base={base} params={p} onRange={(patch) => update(patch)} />
            ) : !data ? (
              <Skeleton label="Loading tasks…" lines={6} />
            ) : view === "stats" ? (
              <Stats
                tasks={data.tasks}
                base={base}
                params={p}
                onRange={(patch) => update(patch)}
                onOpenTask={openTask}
              />
            ) : (
              <Panel aria-label="Tasks">
                <Table>
                  <thead>
                    <tr>
                      {taskSortColumns.map(({ key, label }) => {
                        const direction = sorting?.key === key ? sorting.direction : null;
                        return (
                          <Th key={key} scope="col"
                            className={key === "project" || key === "created_at" ? styles.optional : undefined}
                            aria-sort={direction === "asc" ? "ascending" : direction === "desc" ? "descending" : "none"}>
                            <Button variant="ghost"
                              aria-label={`Sort by ${label}`}
                              onClick={() => changeFilter("sort", `${key}:${direction === "asc" ? "desc" : "asc"}`)}>
                              {label} <span aria-hidden="true">{direction === "asc" ? "↑" : direction === "desc" ? "↓" : "↕"}</span>
                            </Button>
                          </Th>
                        );
                      })}
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((t) => (
                      <tr
                        key={t.id}
                        id={"task-" + t.id}
                        data-interactive=""
                        data-focused={focus === t.id}
                        aria-selected={Number(selected) === t.id}
                        onClick={(e) => {
                          // Keyboard activation of the title button (Enter or
                          // Space) focuses the detail heading; pointers keep focus.
                          if (e.detail === 0) focusDetail.current = t.id;
                          update({ task: String(t.id), focus: String(t.id) });
                        }}
                      >
                        <Td mono className={styles.idCell}>
                          #{t.id}
                        </Td>
                        <Td truncate>
                          {/* Keyboard target; clicks bubble to the row. */}
                          <button
                            type="button"
                            title={t.title}
                            className={styles.taskButton}
                            onFocus={() =>
                              update({ focus: String(t.id) }, true)
                            }
                          >
                            {t.title}
                          </button>
                        </Td>
                        <Td className={styles.optional}>
                          <Tag>{t.project}</Tag>
                        </Td>
                        <Td>
                          <StatusPill task={t} />
                        </Td>
                        <Td mono className={styles.optional}>
                          {new Date(t.created_at).toLocaleDateString()}
                        </Td>
                      </tr>
                    ))}
                  </tbody>
                </Table>
                {!rows.length && (
                  <EmptyState>
                    {data.tasks.length ? (
                      <>
                        No tasks match these filters.{" "}
                        <Button
                          variant="link"
                          onClick={() =>
                            update({
                              q: null,
                              project: null,
                              state: "all",
                              page: null,
                              focus: null,
                            })
                          }
                        >
                          Clear filters
                        </Button>
                      </>
                    ) : (
                      "No tasks yet."
                    )}
                  </EmptyState>
                )}
                <div className={styles.pagination}>
                  <span>
                    {filtered.length} results · Page {page} / {pages}
                  </span>
                  <Button
                    disabled={page <= 1}
                    onClick={() =>
                      update({ page: String(page - 1), focus: null })
                    }
                  >
                    Previous
                  </Button>
                  <Button
                    disabled={page >= pages}
                    onClick={() =>
                      update({ page: String(page + 1), focus: null })
                    }
                  >
                    Next
                  </Button>
                </div>
              </Panel>
            )}
          </section>
          {routineId && (
            <PaneResizer
              controls="routine-detail"
              label="Resize routine pane"
              pane="routine"
            />
          )}
          {routineId && (
            <RoutinePane
              key={base + routineId}
              base={base}
              id={routineId}
              state={routines}
              selectedTask={selected}
              onClose={closeRoutine}
              onOpenTask={(id) => update({ task: String(id), tab: null })}
            />
          )}
          {shown && <DetailResizer controls="task-detail" />}
          {shown && (
            <section
              {...detailScroll}
              id="task-detail"
              className={styles.detailPane}
              data-phase={phase || undefined}
              inert={phase === "closing" || overlay}
              aria-label="Task detail"
            >
              <div className={styles.detailInner}>
              <header className={styles.detailHeader}>
                <div className={styles.detailTop}>
                  <span className={styles.detailId}>Task #{shown}</span>
                  {activeDetail && <StatusPill task={activeDetail.task} />}
                  <Button
                    variant="ghost"
                    className={styles.back}
                    onClick={closeTask}
                  >
                    {routineId ? "Back to routine" : "Back"} <Kbd>Esc</Kbd>
                  </Button>
                </div>
                {activeDetail && (
                  <h2
                    ref={detailHeading}
                    tabIndex={-1}
                    className={styles.detailTitle}
                  >
                    {activeDetail.task.title}
                  </h2>
                )}
              </header>
              {detailError ? (
                <Alert>{detailError}</Alert>
              ) : !activeDetail ? (
                <Skeleton label="Loading task…" lines={8} />
              ) : (
                <>
                  <dl className={styles.facts}>
                    <div>
                      <dt>Project</dt>
                      <dd>{activeDetail.task.project}</dd>
                    </div>
                    <div>
                      <dt>Runs</dt>
                      <dd>{activeDetail.runs.length}</dd>
                    </div>
                    <div>
                      <dt>Created</dt>
                      <dd>{when(activeDetail.task.created_at)}</dd>
                    </div>
                    <div>
                      <dt>Updated</dt>
                      <dd>{when(activeDetail.task.updated_at)}</dd>
                    </div>
                    <RoutineBacklink
                      key={base + activeDetail.task.id}
                      base={base}
                      task={activeDetail.task.id}
                      onOpen={(id) =>
                        update({ view: "routines", routine: String(id) })
                      }
                    />
                  </dl>
                  <Dependencies
                    tasks={data?.tasks || []}
                    task={activeDetail.task}
                    onOpen={openTask}
                  />
                  <section
                    className={styles.terminal}
                    aria-label="Terminal commands"
                  >
                    <Commands>
                      <Command
                        label="log"
                        command={taskCommands(activeDetail.task).log}
                        copyLabel="Copy task log"
                        onCopy={copy}
                      />
                      <Command
                        label="spec"
                        command={taskCommands(activeDetail.task).spec}
                        copyLabel="Copy task spec"
                        onCopy={copy}
                      />
                    </Commands>
                  </section>
                  <Tabs
                    className={styles.tabs}
                    tabs={["spec", "events", "runs", "artifacts"]}
                    value={tab}
                    onChange={(t) => update({ tab: t })}
                  />
                  {tab === "runs" ? (
                    <RunList
                      taskContext
                      titles={titles}
                      runs={activeDetail.runs}
                      now={now}
                      onOpen={openTask}
                      onCopy={copy}
                      onWatch={(id) => update({ run: String(id) })}
                    />
                  ) : tab === "events" ? (
                    activeDetail.events
                      .slice()
                      .reverse()
                      .map((e) => (
                        <article className={styles.event} key={e.id}>
                          <div className={styles.eventHead}>
                            <span className={styles.mono}>
                              {new Date(e.at).toLocaleString()}
                            </span>
                            <Tag>{e.actor}</Tag>
                            <Tag mono>{e.kind}</Tag>
                            {e.from_value || e.to_value ? (
                              <span className={styles.mono}>
                                {e.from_value} → {e.to_value}
                              </span>
                            ) : null}
                          </div>
                          {isDiff(e) ? (
                            <DiffBlock body={e.body || ""} />
                          ) : (
                            <Markdown compact body={e.body || ""} />
                          )}
                        </article>
                      ))
                  ) : tab === "artifacts" ? (
                    <ArtifactTable key={activeDetail.task.id} events={artifacts} onCopy={copy} />
                  ) : (
                    <article className={styles.spec} aria-label="Spec">
                      <Markdown compact body={activeDetail.task.detail} />
                    </article>
                  )}
                </>
              )}
              </div>
            </section>
          )}
        </div>
        <div
          aria-live="polite"
          className={styles.toast}
          data-failed={copied.failed}
        >
          {copied.text}
        </div>
      </main>
    </div>
  );
}
