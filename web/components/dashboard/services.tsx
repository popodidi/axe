"use client";
// Services: long-running (and, later, scheduled) processes bots declare and
// the axe node (dispatcher) runs, per node. Read-only; `axe service` manages
// them. Routines, server-side schedules that create tasks, have their own
// page; the two link to each other only.
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  Alert,
  Button,
  CodeBlock,
  EmptyState,
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
import { Facts, ItemPane, ListEmpty, PaneSection, StateBadge, type Fact } from "./side-pane";
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
  const tone = stateTone(state);
  return (
    <StateBadge tone={tone === "idle" ? "neutral" : tone}>
      {state}
      {!s.enabled && state !== "stopped" && " · off"}
    </StateBadge>
  );
}

// Last run for a long-running service: how its last process ended.
const lastExit = (s: Service) => s.status.last_exit || "—";

export function ServiceList({
  state: { rows, error },
  params,
  now,
  onOpen,
  onOpenRoutines,
  onClearFilters,
}: {
  state: ServicesState;
  params: URLSearchParams;
  now: number;
  onOpen: (key: string, keyboard: boolean) => void;
  onOpenRoutines: () => void;
  onClearFilters?: () => void;
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
            <ListEmpty
              total={rows.length}
              noun="services"
              empty="No services yet: a bot declares one with axe service add"
              onClear={onClearFilters}
            />
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
                        onClick={(e) => onOpen(serviceKey(s), e.detail === 0)}
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
                        </Td>
                        <Td data-label="State">
                          <StateLabel s={s} />
                        </Td>
                        <Td data-label="Restarts" numeric zero={!s.status.restarts} className={styles.optional}>
                          {s.status.restarts}
                        </Td>
                        <Td data-label="Uptime / next">{serviceTime(s, secs)}</Td>
                        <Td data-label="Last exit" mono className={styles.optional}>
                          {lastExit(s)}
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
    </div>
  );
}

type Logs = { text?: string; unavailable?: string };

// One service in the side pane: what it runs, its state and its recent log
// (a private read: the store's read session unlocks it). The facts stay
// generic (kind, timing, last result) so scheduled services fit too.
export function ServicePane({
  base,
  id,
  state: { rows },
  now,
  onClose,
}: {
  base: string;
  id: string;
  state: ServicesState;
  now: number;
  onClose: () => void;
}) {
  const s = rows?.find((x) => serviceKey(x) === id);
  const secs = Math.floor(now / 1000);
  return (
    <ItemPane
      id="service-detail"
      label="Service detail"
      kind="Service"
      badge={s && <StateLabel s={s} />}
      title={id}
      onClose={onClose}
    >
      {!rows ? (
        <Skeleton label="Loading service…" lines={4} />
      ) : !s ? (
        <Alert>Service {id} not found</Alert>
      ) : (
        <>
          <Facts items={serviceFacts(s, secs)} />
          <ServiceLogs base={base} s={s} />
        </>
      )}
    </ItemPane>
  );
}

function serviceFacts(s: Service, now: number): Fact[] {
  const stale = staleFor(s, now);
  const time = (at?: string) => (at ? new Date(at).toLocaleString() : "—");
  const facts: Fact[] = [
    { label: "Bot", value: s.bot },
    { label: "Node", value: s.status.dispatcher || "—", mono: true },
    { label: "Kind", value: serviceKind(s) },
    { label: "Restart policy", value: s.restart },
    { label: "Restarts", value: String(s.status.restarts) },
    { label: "Uptime / next", value: serviceTime(s, now) },
    { label: "Last exit", value: lastExit(s), mono: true },
    { label: "PID", value: s.status.pid ? String(s.status.pid) : "—" },
    { label: "Started", value: time(s.status.started_at) },
    {
      label: "Last report",
      value: s.status.reported_at
        ? `${time(s.status.reported_at)}${stale ? ` (${span(stale)} ago)` : ""}`
        : "—",
    },
    { label: "Command", value: s.command, wide: true, mono: true },
    { label: "Working directory", value: s.cwd || "the bot's run folder", wide: true },
  ];
  if (s.status.crash_loop_since)
    facts.push({ label: "Crash loop since", value: time(s.status.crash_loop_since) });
  if (s.status.error) facts.push({ label: "Error", value: s.status.error, wide: true });
  if (s.env && Object.keys(s.env).length)
    facts.push({ label: "Env", value: Object.keys(s.env).join(", "), wide: true, mono: true });
  return facts;
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
  // The log scrolls inside the pane and follows its tail while the reader
  // is at it; scrolling up holds the position across refreshes.
  const log = useRef<HTMLPreElement>(null);
  const atTail = useRef(true);
  useLayoutEffect(() => {
    const el = log.current;
    if (el && atTail.current) el.scrollTop = el.scrollHeight;
  }, [logs?.text]);
  return (
    <PaneSection
      title="Recent logs"
      note={<>Last 100 lines from the node · axe service logs {s.name} --bot {s.bot}</>}
    >
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
          <CodeBlock
            ref={log}
            className={styles.log}
            tabIndex={0}
            aria-label="Log output"
            onScroll={(e) => {
              const el = e.currentTarget;
              atTail.current = el.scrollHeight - el.scrollTop - el.clientHeight < 4;
            }}
          >
            {logs.text}
          </CodeBlock>
        ) : (
          <EmptyState>No output yet</EmptyState>
        )
      ) : (
        <Alert>Logs unavailable: {logs.unavailable || "no answer from the node"}</Alert>
      )}
    </PaneSection>
  );
}
