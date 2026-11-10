"use client";
import { useEffect, useState } from "react";
import Markdown from "../../app/markdown";
import {
  Alert,
  Button,
  EmptyState,
  Kbd,
  Panel,
  PanelHead,
  Skeleton,
  StatusPill,
  Table,
  Tag,
  Td,
  Th,
} from "../ui";
import type { Task } from "../../lib/model";
import styles from "./routines.module.css";
type Routine = {
  id: number;
  name: string;
  project: string;
  cron: string;
  timezone: string;
  title: string;
  detail: string;
  enabled: boolean;
  archived: boolean;
  next_at: number;
  latest_task: number | null;
};
type Occurrence = {
  id: number;
  task_id: number;
  identity: string;
  created_at: string;
  title: string;
  state: string;
};
export type RoutinesState = { rows: Routine[] | null; error: string };
const status = (r: Routine) =>
  r.archived ? "Archived" : r.enabled ? "Enabled" : "Paused";
// Every status status() can return, as options for the list's Status filter.
export const routineStatuses = ["Enabled", "Paused", "Archived"].map((label) => ({
  value: label.toLowerCase(),
  label,
}));
const next = (r: Routine) =>
  r.enabled && !r.archived ? new Date(r.next_at * 1000).toLocaleString() : "—";
async function get<T>(url: string, signal: AbortSignal): Promise<T> {
  const r = await fetch(url, { signal });
  if (!r.ok) throw Error(`HTTP ${r.status}`);
  return r.json();
}
// Polls the routine list every five seconds with one request in flight and
// keeps stale data on errors. Shared by the list and the routine pane.
export function useRoutines(base: string, enabled: boolean): RoutinesState {
  const [rows, setRows] = useState<Routine[] | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!enabled) return;
    const c = new AbortController();
    let busy = false;
    const load = async () => {
      if (busy) return;
      busy = true;
      try {
        const rows = await get<Routine[]>(base + "/api/routines", c.signal);
        if (!c.signal.aborted) {
          setRows(rows);
          setError("");
        }
      } catch (e) {
        if (!c.signal.aborted) setError(String(e));
      } finally {
        busy = false;
      }
    };
    void load();
    const timer = setInterval(load, 5000);
    return () => {
      c.abort();
      clearInterval(timer);
    };
  }, [base, enabled]);
  return { rows, error };
}
// Paused and archived are labels, not faded rows.
function StatusLabel({ r }: { r: Routine }) {
  return (
    <span className={styles.status} data-status={status(r).toLowerCase()}>
      {status(r)}
    </span>
  );
}
// The routine table: like Tasks, a row opens the routine in a side pane.
export function RoutineList({
  state: { rows, error },
  params,
  onOpen,
  onOpenTask,
}: {
  state: RoutinesState;
  params: URLSearchParams;
  onOpen: (id: number) => void;
  onOpenTask: (id: number) => void;
}) {
  const selected = params.get("routine");
  const q = (params.get("q") || "").toLowerCase();
  const filtered = rows?.filter(
    (r) =>
      (!params.get("project") || r.project === params.get("project")) &&
      (!params.get("status") ||
        status(r).toLowerCase() === params.get("status")) &&
      `${r.name} ${r.title} ${r.detail}`.toLowerCase().includes(q),
  );
  return (
    <div className={styles.stack}>
      {error && (
        <Alert>Unable to load routines: {error}. Retrying automatically.</Alert>
      )}
      {!rows ? (
        <Skeleton label="Loading routines…" lines={4} />
      ) : (
        <Panel aria-label="Routines">
          <PanelHead
            label="Routines"
            meta="Read-only · manage with axe routine"
          />
          <Table className={styles.table}>
            <thead>
              <tr>
                <Th>Routine</Th>
                <Th className={styles.optional}>Schedule</Th>
                <Th>Status</Th>
                <Th className={styles.optional}>Next trigger</Th>
                <Th className={styles.optional}>Latest task</Th>
              </tr>
            </thead>
            <tbody>
              {filtered?.map((r) => (
                <tr
                  key={r.id}
                  id={"routine-" + r.id}
                  data-interactive=""
                  aria-selected={String(r.id) === selected}
                  onClick={() => onOpen(r.id)}
                >
                  <Td data-label="Routine">
                    {/* Keyboard target; clicks bubble to the row. */}
                    <button type="button" className={styles.rowButton}>
                      {r.name}
                    </button>
                    <div className={styles.sub}>{r.project}</div>
                  </Td>
                  <Td data-label="Schedule" mono className={styles.optional}>
                    <span>{r.cron}</span>
                    <div className={styles.sub}>{r.timezone}</div>
                  </Td>
                  <Td data-label="Status">
                    <StatusLabel r={r} />
                  </Td>
                  <Td data-label="Next trigger" className={styles.optional}>
                    {next(r)}
                  </Td>
                  <Td data-label="Latest task" className={styles.optional}>
                    {r.latest_task ? (
                      <Button
                        variant="link"
                        data-open-task={r.latest_task}
                        onClick={(e) => {
                          e.stopPropagation();
                          onOpenTask(r.latest_task!);
                        }}
                      >
                        #{r.latest_task}
                      </Button>
                    ) : (
                      "No tasks yet"
                    )}
                  </Td>
                </tr>
              ))}
            </tbody>
          </Table>
          {!filtered?.length && <EmptyState>No matching routines</EmptyState>}
        </Panel>
      )}
    </div>
  );
}
// Routine side pane: configuration, template and generated-task history. A
// history row opens its task in a pane stacked on the right.
export function RoutinePane({
  base,
  id,
  state: { rows },
  selectedTask,
  onClose,
  onOpenTask,
}: {
  base: string;
  id: string;
  state: RoutinesState;
  selectedTask: string | null;
  onClose: () => void;
  onOpenTask: (id: number) => void;
}) {
  const r = rows?.find((x) => String(x.id) === id);
  const [history, setHistory] = useState<Occurrence[] | null>(null);
  const [error, setError] = useState("");
  const [before, setBefore] = useState<number | null>(null);
  useEffect(() => {
    const c = new AbortController();
    let busy = false;
    const load = async () => {
      if (busy) return;
      busy = true;
      try {
        const rows = await get<Occurrence[]>(
          `${base}/api/routines/${id}/history${before ? `?before=${before}` : ""}`,
          c.signal,
        );
        if (!c.signal.aborted) {
          setHistory(rows);
          setError("");
        }
      } catch (e) {
        if (!c.signal.aborted) setError(String(e));
      } finally {
        busy = false;
      }
    };
    void load();
    const timer = setInterval(load, 5000);
    return () => {
      c.abort();
      clearInterval(timer);
    };
  }, [base, id, before]);
  return (
    <section
      id="routine-detail"
      className={styles.pane}
      aria-label="Routine detail"
    >
      <header className={styles.paneHeader}>
        <div className={styles.paneTop}>
          <span className={styles.paneId}>Routine #{id}</span>
          {r && <StatusLabel r={r} />}
          <Button variant="ghost" className={styles.close} onClick={onClose}>
            Close <Kbd>Esc</Kbd>
          </Button>
        </div>
        {r && (
          <h2 tabIndex={-1} className={styles.paneTitle}>
            {r.name}
          </h2>
        )}
      </header>
      {!rows ? (
        <Skeleton label="Loading routine…" lines={4} />
      ) : !r ? (
        <Alert>Routine not found</Alert>
      ) : (
        <>
          <dl className={styles.facts}>
            <div>
              <dt>Schedule</dt>
              <dd className={styles.mono}>{r.cron}</dd>
            </div>
            <div>
              <dt>Timezone</dt>
              <dd>{r.timezone}</dd>
            </div>
            <div>
              <dt>Next trigger</dt>
              <dd>{next(r)}</dd>
            </div>
            <div>
              <dt>Project</dt>
              <dd>{r.project}</dd>
            </div>
            <div className={styles.wide}>
              <dt>Task title</dt>
              <dd>{r.title}</dd>
            </div>
          </dl>
          <section className={styles.block} aria-label="Routine history">
            <h3 className={styles.blockTitle}>Routine history</h3>
            <p className={styles.note}>
              Template snapshots are retained on generated tasks
            </p>
            {error && (
              <Alert>
                Unable to load history: {error}. Retrying automatically.
              </Alert>
            )}
            {!history ? (
              <Skeleton label="Loading routine history…" />
            ) : !history.length ? (
              <EmptyState>No occurrences yet</EmptyState>
            ) : (
              <ul className={styles.history}>
                {history.map((h) => (
                  <li key={h.id}>
                    <button
                      type="button"
                      className={styles.historyRow}
                      aria-current={
                        String(h.task_id) === selectedTask ? "true" : undefined
                      }
                      data-open-task={h.task_id}
                      onClick={() => onOpenTask(h.task_id)}
                    >
                      <span className={styles.historyTitle}>
                        #{h.task_id} {h.title}
                      </span>
                      <StatusPill task={{ state: h.state as Task["state"] }} />
                      <span className={styles.historyMeta}>
                        {h.identity} · {new Date(h.created_at).toLocaleString()}
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
            <div className={styles.pager}>
              {before && (
                <Button onClick={() => setBefore(null)}>Latest history</Button>
              )}
              {history?.length === 100 && (
                <Button onClick={() => setBefore(history.at(-1)!.id)}>
                  Older history
                </Button>
              )}
            </div>
          </section>
          <section className={styles.block} aria-label="Template">
            <h3 className={styles.blockTitle}>Template</h3>
            <Markdown body={r.detail} compact />
          </section>
        </>
      )}
    </section>
  );
}
// Task detail fact: the routine that generated this task, if any.
export function RoutineBacklink({
  base,
  task,
  onOpen,
}: {
  base: string;
  task: number;
  onOpen: (id: number) => void;
}) {
  const [link, setLink] = useState<{
    routine_id: number;
    identity: string;
  } | null>(null);
  const [error, setError] = useState(false);
  useEffect(() => {
    const c = new AbortController();
    get<{ routine_id: number; identity: string } | null>(
      `${base}/api/tasks/${task}/routine`,
      c.signal,
    )
      .then((v) => {
        if (!c.signal.aborted) setLink(v);
      })
      .catch(() => {
        if (!c.signal.aborted) setError(true);
      });
    return () => c.abort();
  }, [base, task]);
  if (error)
    return (
      <div>
        <dt>Routine</dt>
        <dd>Unable to load routine link.</dd>
      </div>
    );
  return link?.routine_id ? (
    <div>
      <dt>Routine</dt>
      <dd>
        <Button variant="link" onClick={() => onOpen(link.routine_id)}>
          Routine #{link.routine_id}
        </Button>{" "}
        <Tag mono>{link.identity}</Tag>
      </dd>
    </div>
  ) : null;
}
