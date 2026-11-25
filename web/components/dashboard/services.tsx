"use client";
// Services: long-running (and, later, scheduled) processes bots declare and
// the axe node (dispatcher) runs, per node. Read-only; `axe service` manages
// them. Routines, server-side schedules that create tasks, have their own
// page; the two link to each other only.
import { useEffect, useRef, useState } from "react";
import {
  Alert,
  Button,
  CodeBlock,
  EmptyState,
  Kbd,
  Panel,
  PanelHead,
  Skeleton,
  Table,
  Td,
  Th,
} from "../ui";
import { getJson, useStoreSession } from "../../lib/store";
import {
  byNode,
  lastRun,
  scheduleText,
  serviceKey,
  serviceKind,
  serviceState,
  serviceTime,
  span,
  staleFor,
  stateTone,
  type Service,
} from "../../lib/services";
import { StoreUnlock } from "./store-viewer";
import styles from "./services.module.css";

export type ServicesState = { rows: Service[] | null; error: string };

// Polls every service every five seconds with one request in flight and
// keeps stale data on errors.
export function useServices(base: string, enabled: boolean): ServicesState {
  const [rows, setRows] = useState<Service[] | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!enabled) return;
    const c = new AbortController();
    let busy = false;
    const load = async () => {
      if (busy) return;
      busy = true;
      try {
        const rows = await getJson<Service[]>(base + "/api/services", { signal: c.signal });
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

function StateLabel({ s }: { s: Service }) {
  const state = serviceState(s);
  return (
    <span className={styles.state} data-tone={stateTone(state)}>
      {state}
      {!s.enabled && state !== "stopped" && " · off"}
    </span>
  );
}


export function ServiceList({
  base,
  state: { rows, error },
  params,
  now,
  onOpen,
  onOpenRoutines,
}: {
  base: string;
  state: ServicesState;
  params: URLSearchParams;
  now: number;
  onOpen: (key: string | null) => void;
  onOpenRoutines: () => void;
}) {
  const selected = params.get("service");
  const q = (params.get("q") || "").toLowerCase();
  const wanted = params.get("sstate");
  const secs = Math.floor(now / 1000);
  const filtered = rows?.filter(
    (s) =>
      (!wanted || serviceState(s) === wanted) &&
      `${s.bot} ${s.name} ${s.command} ${s.status.dispatcher || ""}`
        .toLowerCase()
        .includes(q),
  );
  const open = rows?.find((s) => serviceKey(s) === selected);
  return (
    <div className={styles.stack}>
      {error && <Alert>Unable to load services: {error}. Retrying automatically.</Alert>}
      {!rows ? (
        <Skeleton label="Loading services…" lines={4} />
      ) : (
        <Panel aria-label="Services">
          <PanelHead
            label="Services"
            meta={
              <>
                Read-only · manage with axe service · task schedules are{" "}
                <Button variant="link" onClick={onOpenRoutines}>
                  Routines
                </Button>
              </>
            }
          />
          {!filtered?.length && (
            <EmptyState>
              {rows.length
                ? "No matching services"
                : "No services yet: a bot declares one with axe service add"}
            </EmptyState>
          )}
          {byNode(filtered || []).map(({ node, services }) => {
            const stale = Math.max(...services.map((s) => staleFor(s, secs) ?? 0));
            return (
              <section
                key={node ?? ""}
                className={styles.node}
                aria-label={node ? `Node ${node}` : "Not reported by a node"}
              >
                <h4 className={styles.nodeHead}>
                  {node ? (
                    <>
                      Node <span className={styles.mono}>{node}</span>
                    </>
                  ) : (
                    "Not reported by a node yet"
                  )}
                  {stale > 0 && (
                    <span className={styles.stale}>no report for {span(stale)}</span>
                  )}
                </h4>
                <Table className={styles.table}>
                  <thead>
                    <tr>
                      <Th>Service</Th>
                      <Th className={styles.optional}>Kind</Th>
                      <Th>State</Th>
                      <Th numeric className={styles.optional}>
                        Restarts
                      </Th>
                      <Th>Uptime / next</Th>
                      <Th className={styles.optional}>Last exit</Th>
                    </tr>
                  </thead>
                  <tbody>
                    {services.map((s) => (
                      <tr
                        key={serviceKey(s)}
                        id={"service-" + serviceKey(s)}
                        data-interactive=""
                        aria-selected={serviceKey(s) === selected}
                        onClick={() => onOpen(serviceKey(s))}
                      >
                        <Td data-label="Service">
                          {/* Keyboard target; clicks bubble to the row. */}
                          <button type="button" className={styles.rowButton}>
                            {s.name}
                          </button>
                          <div className={styles.sub}>bot {s.bot}</div>
                        </Td>
                        <Td data-label="Kind" className={styles.optional}>
                          {serviceKind(s)}
                          {s.schedule && <div className={styles.sub}>{scheduleText(s)}</div>}
                        </Td>
                        <Td data-label="State">
                          <StateLabel s={s} />
                        </Td>
                        <Td data-label="Restarts" numeric zero={!s.status.restarts} className={styles.optional}>
                          {s.schedule ? "—" : s.status.restarts}
                        </Td>
                        <Td data-label="Uptime / next">{serviceTime(s, secs)}</Td>
                        <Td data-label="Last exit" mono className={styles.optional}>
                          {lastRun(s, secs)}
                        </Td>
                      </tr>
                    ))}
                  </tbody>
                </Table>
              </section>
            );
          })}
        </Panel>
      )}
      {selected && rows && (
        open ? (
          <ServiceDetail key={selected} base={base} s={open} now={secs} onClose={() => onOpen(null)} />
        ) : (
          <Alert>Service {selected} not found</Alert>
        )
      )}
    </div>
  );
}

type Logs = { text?: string; unavailable?: string };

// One service: what it runs, its state and its recent log (a private read:
// the store's read session unlocks it).
function ServiceDetail({
  base,
  s,
  now,
  onClose,
}: {
  base: string;
  s: Service;
  now: number;
  onClose: () => void;
}) {
  const ref = useRef<HTMLElement>(null);
  useEffect(() => {
    ref.current?.scrollIntoView?.({ block: "nearest" });
  }, []);
  const stale = staleFor(s, now);
  const facts: [string, string, boolean?][] = [
    ["Bot", s.bot],
    ["Node", s.status.dispatcher || "—"],
    ["Kind", serviceKind(s)],
    ...(s.schedule
      ? ([
          ["Schedule", scheduleText(s) || "", true],
          ["Timeout", span(s.schedule.timeout_seconds)],
          ["Catch-up", s.schedule.catch_up ? "one run after node downtime" : "off"],
          ["Next run", serviceTime(s, now)],
        ] as [string, string, boolean?][])
      : ([
          ["Restart policy", s.restart],
          ["Restarts", String(s.status.restarts)],
          ["Uptime / next", serviceTime(s, now)],
        ] as [string, string, boolean?][])),
    ["Last exit", lastRun(s, now)],
    ["PID", s.status.pid ? String(s.status.pid) : "—"],
    [s.schedule ? "Last run" : "Started", s.status.started_at ? new Date(s.status.started_at).toLocaleString() : "—"],
    ...(s.schedule && s.status.finished_at
      ? ([["Ended", new Date(s.status.finished_at).toLocaleString()]] as [string, string][])
      : []),
    ["Last report", s.status.reported_at
      ? `${new Date(s.status.reported_at).toLocaleString()}${stale ? ` (${span(stale)} ago)` : ""}`
      : "—"],
    ["Command", s.command, true],
    ["Working directory", s.cwd || "the bot's run folder", true],
  ];
  if (s.status.crash_loop_since)
    facts.push(["Crash loop since", new Date(s.status.crash_loop_since).toLocaleString()]);
  if (s.status.error) facts.push(["Error", s.status.error, true]);
  if (s.env && Object.keys(s.env).length) facts.push(["Env", Object.keys(s.env).join(", "), true]);
  return (
    <section ref={ref} id="service-detail" className={styles.detail} aria-label="Service detail">
      <header className={styles.detailTop}>
        <h3 className={styles.detailTitle}>
          {s.bot}/{s.name}
        </h3>
        <StateLabel s={s} />
        <Button variant="ghost" className={styles.close} onClick={onClose}>
          Close <Kbd>Esc</Kbd>
        </Button>
      </header>
      <dl className={styles.facts}>
        {facts.map(([k, v, wide]) => (
          <div key={k} className={wide ? styles.wide : undefined}>
            <dt>{k}</dt>
            <dd className={k === "Command" || k === "Last exit" ? styles.mono : undefined}>{v}</dd>
          </div>
        ))}
      </dl>
      <ServiceLogs base={base} s={s} />
    </section>
  );
}

function ServiceLogs({ base, s }: { base: string; s: Service }) {
  const session = useStoreSession(base);
  const [logs, setLogs] = useState<Logs | null>(null);
  const [error, setError] = useState("");
  const url = `${base}/api/bots/${encodeURIComponent(s.bot)}/services/${encodeURIComponent(s.name)}/logs?lines=100`;
  useEffect(() => {
    if (!session.read) return;
    const c = new AbortController();
    let busy = false;
    const load = async () => {
      if (busy) return;
      busy = true;
      try {
        const v = await getJson<Logs>(url, { signal: c.signal });
        if (!c.signal.aborted) {
          setLogs(v);
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
  }, [url, session.read]);
  return (
    <section className={styles.block} aria-label="Recent logs">
      <h3 className={styles.blockTitle}>Recent logs</h3>
      <p className={styles.note}>
        Last 100 lines from the node · axe service logs {s.name} --bot {s.bot}
      </p>
      {session.read === null ? (
        <Skeleton label="Checking the read session…" lines={2} />
      ) : !session.read ? (
        <StoreUnlock
          base={base}
          reason="A service's log is its program's own output, a private read. Unlock with the server's token to read it."
        />
      ) : error ? (
        <Alert>Unable to load logs: {error}. Retrying automatically.</Alert>
      ) : !logs ? (
        <Skeleton label="Loading logs…" lines={4} />
      ) : logs.text !== undefined ? (
        logs.text.trim() ? (
          <CodeBlock className={styles.log}>{logs.text}</CodeBlock>
        ) : (
          <EmptyState>No output yet</EmptyState>
        )
      ) : (
        <Alert>Logs unavailable: {logs.unavailable || "no answer from the node"}</Alert>
      )}
    </section>
  );
}
