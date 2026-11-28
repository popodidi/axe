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
  ThemeToggle,
  Wordmark,
  cx,
} from "../components/ui";
import { ArtifactTable } from "../components/dashboard/artifact-table";
import {
  Store,
  StoreItem,
  storeItemKey,
  useStoreData,
} from "../components/dashboard/store";
import { RunList } from "../components/dashboard/run-card";
import { RunLog } from "../components/dashboard/run-log";
import {
  RoutineBacklink,
  RoutineList,
  RoutinePane,
  routineStatuses,
  useRoutines,
} from "../components/dashboard/routines";
import { Node } from "../components/dashboard/node";
import { ServiceList, ServicePane, useServices } from "../components/dashboard/services";
import { serviceStates } from "../lib/services";
import { Cost } from "../components/dashboard/cost";
import { Stats } from "../components/dashboard/stats";
import { ClusterList } from "../components/dashboard/cluster-list";
import { clusterIds } from "../lib/scatter-cluster";
import { BotList, BotPane, useBots } from "../components/dashboard/bots";
import { GoalList, GoalPane, useGoals } from "../components/dashboard/goals";
import { Facts, PaneHeader } from "../components/dashboard/side-pane";
import { goalStateFilters } from "../lib/goals";
import { Dependencies } from "../components/dashboard/dependencies";
import {
  DetailResizer,
  PaneResizer,
} from "../components/dashboard/detail-resizer";
import { usePaneScroll, snapshotPaneScroll } from "../lib/use-pane-scroll";
import { useNavCollapsed } from "../lib/layout";
import { buildLine } from "../lib/version";
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
// The server's host for the footer ("axe.hlab"); the full URL is the tooltip.
const host = (base: string) => {
  try {
    return new URL(base).host || base;
  } catch {
    return base;
  }
};
const views = ["tasks", "routines", "services", "bots", "goals", "store", "stats", "cost", "node"];
const labels = ["Tasks", "Routines", "Services", "Bots", "Goals", "Store", "Stats", "Cost", "Node"];
const keys = ["t", "r", "n", "b", "o", "e", "s", "c", "d"];
// The list views whose selected item opens in the item pane, by the URL
// parameter that names it; the list row id is `<param>-<key>`.
const itemParams: Record<string, string> = {
  routines: "routine",
  services: "service",
  bots: "bot",
  goals: "goal",
};
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
  // The server's build for the footer. Read again whenever the stream
  // (re)connects, so a restarted server shows its new build.
  const [build, setBuild] = useState<ReturnType<typeof buildLine>>(null);
  const live = connection.startsWith("Live");
  useEffect(() => {
    const controller = new AbortController();
    fetch(base + "/api/version", { signal: controller.signal })
      .then((r) => (r.ok ? r.json() : null))
      .then((v) => {
        const line = buildLine(v);
        if (line) setBuild(line);
      })
      .catch(() => {}); // aborted, or an older server without the endpoint
    return () => controller.abort();
  }, [base, live]);
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
      if (["task", "tab", "blob", "doc", "cluster"].some((k) => k in patch))
        u.searchParams.delete("detailY");
      if (
        ["view", "project", "state", "goalstate", "status", "sstate", "q", "sort", "page"].some(
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
  // The detail pane shows a task (`task:<id>`) or, on the Store view, a store
  // item (`blob:<sha256>`, `doc:<path>`), or on Stats the tasks of a scatter
  // dot or cluster (`cluster:<ids>`); a task opened over an item wins.
  const storeItem = view === "store" ? storeItemKey(p) : null;
  const cluster = view === "stats" ? p.get("cluster") : null;
  const paneItem = selected
    ? `task:${selected}`
    : storeItem || (cluster ? `cluster:${cluster}` : null);
  // The newest open/close/reselect wins; a closing pane keeps the last item
  // mounted (inert) until its transition ends, then unmounts.
  const [last, setLast] = useState(paneItem);
  const [exiting, setExiting] = useState<string | null>(null);
  const [phase, setPhase] = useState<Phase>(null);
  if (paneItem !== last) {
    setLast(paneItem);
    if (!userNavigation) {
      setExiting(null);
      setPhase(null);
    } else if (paneItem) {
      setExiting(null);
      setPhase(!last && !exiting ? "enter" : exiting ? "opening" : phase);
    } else if (last) {
      setExiting(last);
      setPhase("closing");
    }
  }
  const shownItem = paneItem || exiting;
  const shown = shownItem?.startsWith("task:") ? shownItem.slice(5) : null;
  const shownCluster = shownItem?.startsWith("cluster:")
    ? clusterIds(shownItem.slice(8))
    : null;
  const shownStore = shownItem && !shown && !shownCluster ? shownItem : null;
  // A task opened from a cluster list keeps the list one step back.
  const clusterCount = clusterIds(cluster).length;
  const store = useStoreData(base, view === "store");
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
    // A timer can fire after a newer phase has committed, before effect
    // cleanup clears it (closing just as opening ends). End only its own phase.
    const done = setTimeout(() => {
      setPhase((current) => (current === phase ? null : current));
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
    selected ? detail?.task.id === Number(selected) : !!storeItem || !!cluster,
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
  // Closing a store item returns focus to its row.
  const closeStoreItem = useCallback(() => {
    const item = storeItemKey(new URLSearchParams(location.search));
    if (!item) return;
    if (document.getElementById("store-detail")?.contains(document.activeElement))
      document
        .querySelector<HTMLElement>(`[data-store-item="${CSS.escape(item)}"]`)
        ?.focus();
    update({ blob: null, doc: null });
  }, [update]);
  // Closing a cluster list returns focus to its dot or cluster.
  const closeCluster = useCallback(() => {
    const ids = new URLSearchParams(location.search).get("cluster");
    if (!ids) return;
    if (document.getElementById("cluster-detail")?.contains(document.activeElement))
      document
        .querySelector<HTMLElement>(`[data-cluster="${CSS.escape(ids)}"]`)
        ?.focus();
    update({ cluster: null, clabel: null });
  }, [update]);
  // Routines, Services, Bots and Goals: the list stays, and the selected
  // item opens in the item pane beside it; a task may stack on its right.
  const itemParam = itemParams[view] || null;
  const itemKey = itemParam ? p.get(itemParam) : null;
  const routines = useRoutines(base, view === "routines");
  const services = useServices(base, view === "services");
  const bots = useBots(base, view === "bots", revision);
  const goals = useGoals(base, view === "goals", revision);
  // Closing returns focus to the item's row when focus was in the pane (or
  // nowhere), before the pane unmounts.
  const closeItem = useCallback(() => {
    if (!itemParam) return;
    const key = new URLSearchParams(location.search).get(itemParam);
    if (!key) return;
    const active = document.activeElement;
    if (
      !active ||
      active === document.body ||
      document.querySelector("[data-item-pane]")?.contains(active)
    )
      document
        .querySelector<HTMLElement>(`#${itemParam}-${CSS.escape(key)} button`)
        ?.focus();
    update({ [itemParam]: null, task: null, tab: null, days: null });
  }, [itemParam, update]);
  // Keyboard opening (Enter on a row) focuses the pane's heading on mount.
  const focusItem = useRef<string | null>(null);
  const openItem = (key: string, keyboard: boolean) => {
    if (!itemParam) return;
    if (keyboard) focusItem.current = key;
    update({ [itemParam]: key, task: null, tab: null, days: null });
  };
  useEffect(() => {
    if (!itemKey || focusItem.current !== itemKey) return;
    focusItem.current = null;
    document.querySelector<HTMLElement>("[data-item-pane] h2")?.focus();
  }, [itemKey]);
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
          d: "node",
          r: "routines",
          n: "services",
          b: "bots",
          o: "goals",
          e: "store",
        };
        if (v[e.key]) {
          update({ view: v[e.key], task: null, focus: null, page: null, routine: null, service: null, bot: null, goal: null, cluster: null, clabel: null });
          prefix.current = 0;
          return;
        }
      }
      if (e.key === "/") {
        e.preventDefault();
        searchRef.current?.focus();
      }
      // Escape closes the topmost pane: the task, then its routine or store
      // item.
      if (e.key === "Escape") {
        const q = new URLSearchParams(location.search);
        if (q.has("task")) closeTask();
        else if (q.get("view") === "store" && storeItemKey(q)) closeStoreItem();
        else if (q.get("view") === "stats" && q.has("cluster")) closeCluster();
        else closeItem();
      }
      // j/k move between rows of an item list; Enter on a row opens it.
      if (itemParam && (e.key === "j" || e.key === "k")) {
        e.preventDefault();
        const rows = Array.from(
          document.querySelectorAll<HTMLElement>(
            "#list-pane tbody tr[data-interactive]",
          ),
        );
        const i = rows.findIndex((r) => r.contains(document.activeElement));
        const next =
          rows[
            i < 0
              ? 0
              : Math.max(0, Math.min(rows.length - 1, i + (e.key === "j" ? 1 : -1)))
          ];
        next?.querySelector<HTMLElement>("button")?.focus();
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
  }, [rows, focus, view, itemParam, update, closeTask, closeItem, closeStoreItem, closeCluster]);
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
  // An item list's "Clear filters": its search and filters, not the view.
  const clearFilters = () =>
    update({ q: null, project: null, status: null, sstate: null, goalstate: null });
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
          onOpen={update}
        />
      )}
      {botRun && !watched && (
        <RunLog
          key={base + "bot" + botRun}
          id={botRun}
          base={base}
          kind="bot"
          onClose={() => update({ botrun: null })}
          onOpen={update}
        />
      )}
      <aside id="app-nav" className={styles.nav} inert={overlay}>
        <div className={styles.brandRow}>
          <div className={styles.brand}>
            <div className={styles.brandText}>
              <div className={styles.brandName}>
                <Wordmark />
              </div>
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
                update({ view: v, task: null, page: null, focus: null, routine: null, service: null, bot: null, goal: null, cluster: null, clabel: null })
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
          {/* One status row (connection, then refresh and the theme icon
              toggle), and one muted line for the server. */}
          <div className={styles.status}>
            <span
              className={styles.connection}
              data-live={live}
              role="status"
              title={connection}
            >
              <span className={styles.navLabel}>{connection}</span>
            </span>
            <span className={styles.footActions}>
              <Button
                variant="ghost"
                icon
                aria-label="Refresh now"
                title="Refresh now"
                onClick={() => reload.current()}
              >
                <svg aria-hidden viewBox="0 0 16 16" className={styles.footIcon}>
                  <path d="M13 8a5 5 0 1 1-1.46-3.54" />
                  <path d="M13 2.5V5h-2.5" />
                </svg>
              </Button>
              <ThemeToggle />
            </span>
          </div>
          <span className={cx(styles.serverLine, styles.hideNarrow)} title={`server ${base}`}>
            <span className={styles.mono}>{host(base)}</span> · Read-only
          </span>
          {build && (
            <span
              className={cx(styles.serverLine, styles.hideNarrow)}
              data-testid="server-version"
            >
              axe <span className={styles.mono}>{build.release}</span>
              {build.short && (
                <>
                  {" ("}
                  {build.href ? (
                    <a
                      className={cx(styles.mono, styles.versionLink)}
                      href={build.href}
                      target="_blank"
                      rel="noopener noreferrer"
                      title={`Commit ${build.short} on GitHub`}
                    >
                      {build.short}
                    </a>
                  ) : (
                    <span className={styles.mono}>{build.short}</span>
                  )}
                  )
                </>
              )}
            </span>
          )}
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
          (paneItem || itemKey) && styles.hasDetail,
          itemKey && styles.hasItem,
          itemKey && shown && styles.stacked,
          view === "store" && styles.storeView,
        )}
        inert={overlay}
      >
        <div className={styles.workspace}>
          <section
            {...listScroll}
            id="list-pane"
            className={styles.listPane}
            aria-label="Task workspace"
          >
            {view !== "stats" && view !== "cost" && view !== "store" && view !== "node" && (
            <div className={styles.filters}>
              <Search
                compact
                id="list-search"
                ref={searchRef}
                className={styles.search}
                aria-label={`Search ${["routines", "services", "bots", "goals"].includes(view) ? view : "tasks"}`}
                placeholder={`Search ${["routines", "services", "bots", "goals"].includes(view) ? view : "tasks"}…  /`}
                value={p.get("q") || ""}
                onChange={(e) => changeFilter("q", e.target.value)}
              />
              {(view === "tasks" ? ["project", "state"] : view === "services" ? [] : ["project"]).map(
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
              {view === "goals" && (
                <Filter
                  label="state"
                  aria-label="goal state"
                  value={p.get("goalstate") || ""}
                  onChange={(v) => changeFilter("goalstate", v)}
                  options={goalStateFilters}
                />
              )}
              {view === "services" && (
                <Filter
                  label="state"
                  aria-label="service state"
                  value={p.get("sstate") || ""}
                  onChange={(v) => changeFilter("sstate", v)}
                  options={[{ value: "", label: "All" }, ...serviceStates.map((v) => ({ value: v, label: v }))]}
                />
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
            {view === "goals" ? (
              <GoalList
                state={goals}
                params={p}
                onOpen={openItem}
                onClearFilters={clearFilters}
              />
            ) : view === "bots" ? (
              <BotList
                state={bots}
                params={p}
                onOpen={openItem}
                onClearFilters={clearFilters}
              />
            ) : view === "routines" ? (
              <RoutineList
                state={routines}
                params={p}
                onOpen={(id, keyboard) => openItem(String(id), keyboard)}
                onOpenTask={(id) => update({ task: String(id), tab: null })}
                onOpenServices={() => update({ view: "services", routine: null, task: null, tab: null })}
                onClearFilters={clearFilters}
              />
            ) : view === "services" ? (
              <ServiceList
                state={services}
                params={p}
                now={now}
                onOpen={openItem}
                onOpenRoutines={() => update({ view: "routines", service: null })}
                onClearFilters={clearFilters}
              />
            ) : view === "store" ? (
              <Store
                base={base}
                params={p}
                state={store}
                // Selecting a store item replaces a task opened over it.
                onChange={(patch) =>
                  update(patch.blob || patch.doc ? { ...patch, task: null, tab: null } : patch)
                }
                onOpenTask={(id) => update({ task: String(id), tab: "artifacts" })}
              />
            ) : view === "cost" ? (
              <Cost base={base} params={p} onRange={(patch) => update(patch)} />
            ) : view === "node" ? (
              <Node base={base} params={p} onRange={(patch) => update(patch)} />
            ) : !data ? (
              <Skeleton label="Loading tasks…" lines={6} />
            ) : view === "stats" ? (
              <Stats
                tasks={data.tasks}
                base={base}
                params={p}
                onRange={(patch) => update(patch)}
                onOpenCluster={({ ids, label }) =>
                  update({
                    cluster: ids.join(","),
                    clabel: label,
                    task: null,
                    tab: null,
                  })
                }
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
          {itemKey && (
            <PaneResizer
              controls={`${itemParam}-detail`}
              label={`Resize ${itemParam} pane`}
              pane="item"
            />
          )}
          {itemKey && view === "routines" ? (
            <RoutinePane
              key={base + itemKey}
              base={base}
              id={itemKey}
              state={routines}
              selectedTask={selected}
              onClose={closeItem}
              onOpenTask={(id) => update({ task: String(id), tab: null })}
            />
          ) : itemKey && view === "services" ? (
            <ServicePane
              key={base + itemKey}
              base={base}
              id={itemKey}
              state={services}
              now={now}
              onClose={closeItem}
            />
          ) : itemKey && view === "bots" ? (
            <BotPane
              key={base + itemKey}
              base={base}
              name={itemKey}
              params={p}
              revision={revision}
              now={now}
              onClose={closeItem}
              onChange={(patch) => update(patch)}
              onWatch={(id) => update({ botrun: String(id) })}
              onCopy={copy}
            />
          ) : itemKey && view === "goals" ? (
            <GoalPane
              key={base + itemKey}
              base={base}
              name={itemKey}
              revision={revision}
              selectedTask={selected}
              onClose={closeItem}
              onOpenTask={(id) => update({ task: String(id), tab: null })}
              onCopy={copy}
            />
          ) : null}
          {shownItem && (
            <DetailResizer
              controls={
                shown ? "task-detail" : shownCluster ? "cluster-detail" : "store-detail"
              }
            />
          )}
          {shownCluster && (
            <section
              {...detailScroll}
              id="cluster-detail"
              className={styles.detailPane}
              data-phase={phase || undefined}
              inert={phase === "closing" || overlay}
              aria-label="Cluster tasks"
            >
              <div className={styles.detailInner}>
                <PaneHeader
                  kind={`Cluster · ${shownCluster.length} ${shownCluster.length === 1 ? "task" : "tasks"}`}
                  title={p.get("clabel") || "Selected tasks"}
                  onClose={closeCluster}
                />
                <ClusterList
                  ids={shownCluster}
                  tasks={data?.tasks || []}
                  onOpen={(id, keyboard) => {
                    if (keyboard) focusDetail.current = id;
                    update({ task: String(id), tab: null });
                  }}
                />
              </div>
            </section>
          )}
          {shownStore && (
            <section
              {...detailScroll}
              id="store-detail"
              className={styles.detailPane}
              data-phase={phase || undefined}
              inert={phase === "closing" || overlay}
              aria-label="Store item"
            >
              <div className={`${styles.detailInner} ${styles.storeInner}`}>
                <PaneHeader
                  kind={shownStore.startsWith("doc:") ? "Doc" : "Blob"}
                  onClose={closeStoreItem}
                />
                <StoreItem
                  base={base}
                  item={shownStore}
                  state={store}
                  onOpenTask={(id) => update({ task: String(id), tab: null })}
                />
              </div>
            </section>
          )}
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
              <PaneHeader
                kind={
                  cluster && !itemKey ? (
                    <nav aria-label="Breadcrumb" className={styles.crumbs}>
                      <Button variant="link" className={styles.detailId} onClick={closeTask}>
                        Cluster ({clusterCount})
                      </Button>
                      <span aria-hidden="true" className={styles.detailId}>
                        ›
                      </span>
                      <span aria-current="page" className={styles.detailId}>
                        #{shown}
                      </span>
                    </nav>
                  ) : (
                    `Task #${shown}`
                  )
                }
                badge={activeDetail && <StatusPill task={activeDetail.task} />}
                title={activeDetail ? activeDetail.task.title : undefined}
                headingRef={detailHeading}
                closeLabel={
                  itemKey ? `Back to ${itemParam}` : cluster ? "Back to cluster" : "Close"
                }
                onClose={closeTask}
              />
              {detailError ? (
                <Alert>{detailError}</Alert>
              ) : !activeDetail ? (
                <Skeleton label="Loading task…" lines={8} />
              ) : (
                <>
                  <Facts
                    items={[
                      { label: "Project", value: activeDetail.task.project },
                      { label: "Runs", value: activeDetail.runs.length },
                      { label: "Created", value: when(activeDetail.task.created_at) },
                      { label: "Updated", value: when(activeDetail.task.updated_at) },
                    ]}
                  >
                    <RoutineBacklink
                      key={base + activeDetail.task.id}
                      base={base}
                      task={activeDetail.task.id}
                      onOpen={(id) =>
                        update({ view: "routines", routine: String(id) })
                      }
                    />
                  </Facts>
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
                    <ArtifactTable key={activeDetail.task.id} base={base} events={artifacts} onCopy={copy} />
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
