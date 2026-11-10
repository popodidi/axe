"use client";
import { useEffect, useMemo, useState } from "react";
import Markdown from "../../app/markdown";
import {
  Alert,
  Button,
  Command,
  Commands,
  DiffBlock,
  EmptyState,
  Filter,
  Kbd,
  LiveDot,
  Panel,
  PanelHead,
  Skeleton,
  Table,
  Tabs,
  Tag,
  Td,
  Th,
} from "../ui";
import { RunList } from "./run-card";
import { BarChart, TrendChart } from "./stats-charts";
import { command, formatDuration } from "../../lib/model";
import { dateRange } from "../../lib/stats";
import { botStats } from "../../lib/bot-stats";
import {
  BOT_READ_LIMIT,
  type Activity,
  type Admission,
  type Bot,
  type BotEvent,
  type BotRun,
  type BotSchedule,
  type BotSummary,
  type MessageMeta,
  type Wake,
  asRun,
  botHold,
  botStatus,
  poolSummary,
  botTabs,
  profileHistory,
} from "../../lib/bots";
import styles from "./bots.module.css";
// Generic bot monitoring. Read-only
// and metadata-only: wakes arrive redacted, activity carries no message text.
const when = (s: string | null) => (s ? new Date(s).toLocaleString() : "—");
const date = (n: number) => new Date(n).toISOString().slice(0, 10);
const compact = (ms: number) =>
  ms < 3600000
    ? `${Math.round(ms / 60000)}m`
    : ms < 86400000
      ? `${Math.round(ms / 3600000)}h`
      : `${Math.round(ms / 86400000)}d`;
const pct = (n: number | null) => (n === null ? "—" : `${Math.round(n * 100)}%`);
async function get<T>(url: string, signal: AbortSignal): Promise<T> {
  const r = await fetch(url, { signal });
  if (!r.ok) throw Error(`HTTP ${r.status}`);
  return r.json();
}
/** The latest inbound and outbound message times. */
function lastMessages(messages: MessageMeta[]) {
  const last = (d: string) =>
    messages.filter((m) => m.direction === d).at(-1)?.at ?? null;
  return { in: last("in"), out: last("out") };
}
function Status({ bot }: { bot: Pick<BotSummary, "state" | "attention"> }) {
  const label = botStatus(bot);
  return (
    <span
      className={styles.status}
      data-status={bot.attention ? "attention" : bot.state}
    >
      {label}
    </span>
  );
}
export type BotListData = {
  bots: BotSummary[] | null;
  /** Dispatcher admission with bot and task slot use; null if unavailable. */
  admission: Admission | null;
  live: Map<number, BotRun>;
  last: Map<string, { in: string | null; out: string | null }>;
  error: string;
};
/**
 * The bot list and its per-row extras: live runs (one read) and the latest
 * message times (one metadata read per bot). Reloads on every server change.
 */
export function useBots(base: string, enabled: boolean, revision: number) {
  const [state, setState] = useState<BotListData>({
    bots: null,
    admission: null,
    live: new Map(),
    last: new Map(),
    error: "",
  });
  useEffect(() => {
    if (!enabled) return;
    const c = new AbortController();
    (async () => {
      try {
        const [bots, live, admission] = await Promise.all([
          get<BotSummary[]>(base + "/api/bots", c.signal),
          get<BotRun[]>(base + "/api/bot-runs?live=1&limit=1000", c.signal),
          // Slot counters are optional: an older server or a failed read
          // leaves the list itself usable.
          get<Admission>(base + "/api/admission", c.signal).catch(() => null),
        ]);
        const last = new Map(
          await Promise.all(
            bots.map(async (b) => {
              const a = await get<Activity>(
                `${base}/api/bots/${encodeURIComponent(b.name)}/activity?limit=200`,
                c.signal,
              ).catch(() => null);
              return [b.name, a ? lastMessages(a.messages) : { in: null, out: null }] as const;
            }),
          ),
        );
        if (!c.signal.aborted)
          setState({
            bots,
            admission,
            live: new Map(live.map((r) => [r.id, r])),
            last,
            error: "",
          });
      } catch (e) {
        if (!c.signal.aborted) setState((s) => ({ ...s, error: String(e) }));
      }
    })();
    return () => c.abort();
  }, [base, enabled, revision]);
  return state;
}
export function BotList({
  state: { bots, admission, live, last, error },
  params,
  onOpen,
}: {
  state: BotListData;
  params: URLSearchParams;
  onOpen: (name: string) => void;
}) {
  const q = (params.get("q") || "").toLowerCase();
  const filtered = bots?.filter(
    (b) =>
      (!params.get("project") || b.project === params.get("project")) &&
      `${b.name} ${b.project} ${b.agent}`.toLowerCase().includes(q),
  );
  return (
    <div className={styles.stack}>
      {error && (
        <Alert>Unable to load bots: {error}. Retrying automatically.</Alert>
      )}
      {!bots ? (
        <Skeleton label="Loading bots…" lines={4} />
      ) : (
        <Panel aria-label="Bots">
          <PanelHead
            label="Bots"
            meta={[poolSummary(admission), "Read-only · manage with axe bot"]
              .filter(Boolean)
              .join(" · ")}
          />
          {botHold(admission) && (
            <Alert>New bot runs held at the resource floor: {botHold(admission)}</Alert>
          )}
          <Table>
            <thead>
              <tr>
                <Th>Bot</Th>
                <Th>Status</Th>
                <Th>Live run</Th>
                <Th className={styles.optional}>Next wake</Th>
                <Th className={styles.optional}>Last message</Th>
                <Th numeric>Failures</Th>
              </tr>
            </thead>
            <tbody>
              {filtered?.map((b) => {
                const run = b.live_run ? live.get(b.live_run) : undefined;
                const msgs = last.get(b.name);
                return (
                  <tr
                    key={b.name}
                    id={"bot-" + b.name}
                    data-interactive=""
                    onClick={() => onOpen(b.name)}
                  >
                    <Td>
                      {/* Keyboard target; clicks bubble to the row. */}
                      <button type="button" className={styles.rowButton}>
                        {b.name}
                      </button>
                      <div className={styles.sub}>
                        {b.project} · {b.agent}
                      </div>
                    </Td>
                    <Td>
                      <Status bot={b} />
                    </Td>
                    <Td>
                      {b.live_run ? (
                        <>
                          <span className={styles.mono}>
                            <LiveDot live /> Run #{b.live_run}
                          </span>
                          <span
                            className={`${styles.sub} ${styles.activity}`}
                            title={run?.current_activity || undefined}
                          >
                            {run?.current_activity || "No activity recorded"}
                          </span>
                        </>
                      ) : (
                        <span className={styles.sub}>Idle</span>
                      )}
                    </Td>
                    <Td className={styles.optional}>
                      {when(b.next_wake)}
                      <div className={styles.sub}>{b.queued} queued</div>
                    </Td>
                    <Td className={styles.optional}>
                      <div className={styles.sub}>in {when(msgs?.in ?? null)}</div>
                      <div className={styles.sub}>out {when(msgs?.out ?? null)}</div>
                    </Td>
                    <Td numeric zero={!b.failures}>
                      {b.failures}
                    </Td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
          {!filtered?.length && (
            <EmptyState>
              {bots.length ? "No matching bots" : "No bots yet."}
            </EmptyState>
          )}
        </Panel>
      )}
    </div>
  );
}
type Detail = {
  bot: Bot;
  wakes: Wake[];
  events: BotEvent[];
  runs: BotRun[];
  activity: Activity;
  schedules: BotSchedule[];
};
export function BotDetail({
  base,
  name,
  params,
  revision,
  now,
  onBack,
  onChange,
  onWatch,
  onCopy,
}: {
  base: string;
  name: string;
  params: URLSearchParams;
  revision: number;
  now: number;
  onBack: () => void;
  onChange: (patch: Record<string, string | null>) => void;
  onWatch: (run: number) => void;
  onCopy: (command: string) => void;
}) {
  const [detail, setDetail] = useState<Detail | null>(null);
  const [error, setError] = useState("");
  const tab = botTabs.includes(params.get("tab") || "")
    ? params.get("tab")!
    : "profile";
  useEffect(() => {
    const c = new AbortController();
    const path = `${base}/api/bots/${encodeURIComponent(name)}`;
    const n = BOT_READ_LIMIT;
    Promise.all([
      get<Bot>(path, c.signal),
      get<Wake[]>(`${path}/wakes?limit=${n}`, c.signal),
      get<BotEvent[]>(`${path}/events?limit=${n}`, c.signal),
      get<BotRun[]>(
        `${base}/api/bot-runs?bot=${encodeURIComponent(name)}&limit=${n}`,
        c.signal,
      ),
      get<Activity>(`${path}/activity?limit=${n}`, c.signal),
      get<BotSchedule[]>(`${path}/schedules`, c.signal),
    ])
      .then(([bot, wakes, events, runs, activity, schedules]) => {
        if (c.signal.aborted) return;
        setDetail({ bot, wakes, events, runs, activity, schedules });
        setError("");
      })
      .catch((e) => {
        if (!c.signal.aborted) setError(String(e));
      });
    return () => c.abort();
  }, [base, name, revision]);
  const d = detail?.bot.name === name ? detail : null;
  return (
    <section className={styles.stack} id="bot-detail" aria-label="Bot detail">
      <header className={styles.header}>
        <div className={styles.top}>
          <span className={styles.id}>Bot</span>
          {d && <Status bot={d.bot} />}
          <Button variant="ghost" className={styles.back} onClick={onBack}>
            Back to bots <Kbd>Esc</Kbd>
          </Button>
        </div>
        <h2 className={styles.title} tabIndex={-1}>
          {name}
        </h2>
      </header>
      {error && (
        <Alert>Unable to load bot: {error}. Retrying on the next change.</Alert>
      )}
      {!d ? (
        !error && <Skeleton label="Loading bot…" lines={8} />
      ) : (
        <>
          <dl className={styles.facts}>
            <div>
              <dt>Project</dt>
              <dd>{d.bot.project}</dd>
            </div>
            <div>
              <dt>Agent</dt>
              <dd>{d.bot.agent}</dd>
            </div>
            <div>
              <dt>Live run</dt>
              <dd>{d.bot.live_run ? `#${d.bot.live_run}` : "Idle"}</dd>
            </div>
            <div>
              <dt>Next wake</dt>
              <dd>
                {when(d.bot.next_wake)} · {d.bot.queued} queued
              </dd>
            </div>
            <div>
              <dt>Session</dt>
              <dd>
                {d.bot.session_id
                  ? `${d.bot.session_agent} · ${d.bot.resume_count} resumes`
                  : "Fresh on next run"}
              </dd>
            </div>
            <div>
              <dt>Failures</dt>
              <dd>{d.bot.failures}</dd>
            </div>
            <div>
              <dt>Updated</dt>
              <dd>{when(d.bot.updated_at)}</dd>
            </div>
          </dl>
          <Commands>
            <Command
              label="show"
              command={command(d.bot.project, ["axe", "bot", "show", name])}
              copyLabel="Copy bot show"
              onCopy={onCopy}
            />
            <Command
              label="log"
              command={command(d.bot.project, ["axe", "bot", "log", name])}
              copyLabel="Copy bot log"
              onCopy={onCopy}
            />
          </Commands>
          <Tabs
            tabs={botTabs}
            value={tab}
            onChange={(t) => onChange({ tab: t })}
          />
          {tab === "wakes" ? (
            <Wakes wakes={d.wakes} />
          ) : tab === "runs" ? (
            <RunList
              taskContext
              titles={new Map()}
              runs={d.runs.map((r) => asRun(r, d.bot))}
              now={now}
              onOpen={() => {}}
              onCopy={onCopy}
              onWatch={onWatch}
            />
          ) : tab === "events" ? (
            <Events events={d.events} />
          ) : tab === "stats" ? (
            <BotStats detail={d} params={params} now={now} onChange={onChange} />
          ) : (
            <>
              <Schedules schedules={d.schedules} />
              <Profile bot={d.bot} events={d.events} />
            </>
          )}
        </>
      )}
    </section>
  );
}
function Schedules({ schedules }: { schedules: BotSchedule[] }) {
  if (!schedules.length) return null;
  return (
    <Panel aria-label="Schedules">
      <PanelHead label="Schedules" meta={`${schedules.length} · next fire time local`} />
      <Table>
        <thead>
          <tr>
            <Th>Name</Th>
            <Th>Next fire</Th>
            <Th className={styles.optional}>Cron</Th>
          </tr>
        </thead>
        <tbody>
          {schedules.map((s) => (
            <tr key={s.id} id={"schedule-" + s.name}>
              <Td>
                <Tag mono>{s.name}</Tag>
                <div className={`${styles.sub} ${styles.activity}`} title={s.objective}>
                  {s.objective}
                </div>
              </Td>
              <Td mono>{s.enabled ? when(s.next_at) : "Paused"}</Td>
              <Td mono className={styles.optional}>
                {s.cron} ({s.timezone})
              </Td>
            </tr>
          ))}
        </tbody>
      </Table>
    </Panel>
  );
}
function Profile({ bot, events }: { bot: Bot; events: BotEvent[] }) {
  return (
    <>
      {(["identity", "conduct", "notes"] as const).map((field) => {
        const history = profileHistory(events, field);
        return (
          <section key={field} className={styles.block} aria-label={field}>
            <h3 className={styles.blockTitle}>
              {field[0].toUpperCase() + field.slice(1)}
            </h3>
            {bot[field] ? (
              <Markdown compact body={bot[field]} />
            ) : (
              <p className={styles.note}>Empty</p>
            )}
            <details className={styles.history}>
              <summary>
                History ({history.length} edit{history.length === 1 ? "" : "s"})
              </summary>
              {history.map((e) => (
                <article key={e.id}>
                  <div className={styles.eventHead}>
                    <span className={styles.mono}>{when(e.at)}</span>
                    <Tag>{e.actor}</Tag>
                  </div>
                  <DiffBlock body={e.body} />
                </article>
              ))}
            </details>
          </section>
        );
      })}
    </>
  );
}
function Wakes({ wakes }: { wakes: Wake[] }) {
  if (!wakes.length) return <EmptyState>No wakes</EmptyState>;
  return (
    <Panel aria-label="Wakes">
      <PanelHead
        label="Wakes"
        meta={`Newest due first · latest ${BOT_READ_LIMIT} · message text is not shown`}
      />
      <Table>
        <thead>
          <tr>
            <Th>Kind</Th>
            <Th>Due</Th>
            <Th className={styles.optional}>From</Th>
            <Th>Delivered</Th>
            <Th>Via</Th>
          </tr>
        </thead>
        <tbody>
          {wakes
            .slice()
            .reverse()
            .map((w) => (
              <tr key={w.id} id={"wake-" + w.id}>
                <Td>
                  <Tag mono>{w.kind}</Tag>
                  {w.kind === "wake" && w.body && (
                    <div className={`${styles.sub} ${styles.activity}`} title={w.body}>
                      {w.body}
                    </div>
                  )}
                  {w.message && (
                    <div className={styles.sub}>
                      message #{w.message.id} · {w.message.kind}
                    </div>
                  )}
                </Td>
                <Td mono>{when(w.due_at)}</Td>
                <Td className={styles.optional}>{w.actor}</Td>
                <Td>
                  {w.run_id ? `Run #${w.run_id}` : "Queued"}
                  {w.redelivered && <div className={styles.sub}>redelivered</div>}
                </Td>
                <Td>{w.delivered_via ?? "—"}</Td>
              </tr>
            ))}
        </tbody>
      </Table>
    </Panel>
  );
}
function Events({ events }: { events: BotEvent[] }) {
  if (!events.length) return <EmptyState>No events</EmptyState>;
  return (
    <div>
      {events
        .slice()
        .reverse()
        .map((e) => (
          <article className={styles.event} key={e.id} data-kind={e.kind}>
            <div className={styles.eventHead}>
              <span className={styles.mono}>{when(e.at)}</span>
              <Tag>{e.actor}</Tag>
              <Tag mono>{e.kind}</Tag>
              {e.kind === "security_drop" && <Tag>Security</Tag>}
            </div>
            {e.body &&
              (/\n--- /.test(e.body) ? (
                <DiffBlock body={e.body} />
              ) : (
                <pre className={styles.body}>{e.body}</pre>
              ))}
          </article>
        ))}
    </div>
  );
}
function Kpi({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <div className={styles.kpi}>
      <div className={styles.kpiLabel}>{label}</div>
      <div className={styles.kpiValue}>{value}</div>
      <div className={styles.kpiDetail}>{detail}</div>
    </div>
  );
}
function BotStats({
  detail: d,
  params,
  now,
  onChange,
}: {
  detail: Detail;
  params: URLSearchParams;
  now: number;
  onChange: (patch: Record<string, string | null>) => void;
}) {
  const days = ["7", "30", "90"].includes(params.get("days") || "")
    ? params.get("days")!
    : "7";
  // Recompute on data or range, not on the one-second clock.
  const today = Math.floor(now / 86400000);
  const s = useMemo(() => {
    const range = dateRange(new URLSearchParams({ days }), today * 86400000);
    return botStats(d, range, `bot:${d.bot.name}`);
  }, [d, days, today]);
  const labelEvery = s.days > 31 ? 14 : s.days > 10 ? 5 : 1;
  const dayLabel = (n: number) => date(n).slice(5);
  const r = s.runtime,
    l = s.latency,
    o = s.outcomes;
  const dur = (n: number | null) => (n === null ? "—" : formatDuration(n));
  return (
    <section className={styles.stack} aria-label="Bot stats">
      <div className={styles.filters}>
        <Filter
          label="Period"
          aria-label="Period"
          value={days}
          onChange={(v) => onChange({ days: v })}
          options={[
            { value: "7", label: "Last 7 days" },
            { value: "30", label: "Last 30 days" },
            { value: "90", label: "Last 90 days" },
          ]}
        />
        <p className={styles.note}>
          UTC days through today · each read is the latest {BOT_READ_LIMIT}{" "}
          records
          {s.truncated.length > 0 &&
            ` · ${s.truncated.join(", ")} hit the cap; older records may be missing`}
        </p>
      </div>
      <section className={styles.kpis} aria-label="Key figures">
        <Kpi
          label="Runs"
          value={String(s.runs.total)}
          detail={`${s.sessions.resumed} resumed · ${s.sessions.fresh} fresh · ${s.runs.live} live`}
        />
        <Kpi
          label="Runtime"
          value={r.mean === null ? "—" : `${formatDuration(r.mean)} ± ${formatDuration(r.sd!)}`}
          detail={`Mean ± SD · ${r.n} finished runs`}
        />
        <Kpi
          label="Reply latency"
          value={l.mean === null ? "—" : `${formatDuration(l.mean)} ± ${formatDuration(l.sd!)}`}
          detail={`Median ${dur(l.summary.median)} · ${l.n} replies · ${l.unanswered} unanswered`}
        />
        <Kpi
          label="Bot task actions"
          value={pct(s.taskActions.share)}
          detail={`${s.taskActions.bot} bot:${d.bot.name} · ${s.taskActions.human} human`}
        />
        <Kpi
          label="Failures"
          value={String(o.failure)}
          detail={`${o.stop} stops · ${o.ownerPauses + o.attention} pauses (${o.attention} automatic)`}
        />
      </section>
      <div className={styles.charts}>
        <Panel>
          <BarChart
            title="Runs per day"
            subtitle="Bot runs by UTC start date"
            groups={["Runs"]}
            labelEvery={labelEvery}
            buckets={s.runsPerDay.map((b) => ({
              label: dayLabel(b.start),
              counts: [b.count],
            }))}
          />
        </Panel>
        <Panel>
          <BarChart
            title="Task actions per day"
            subtitle={`Task events in ${d.bot.project} by the bot and by the owner`}
            groups={[`bot:${d.bot.name}`, "human"]}
            labelEvery={labelEvery}
            buckets={s.taskActions.perDay.map((b) => ({
              label: dayLabel(b.start),
              counts: [b.bot, b.human],
            }))}
          />
        </Panel>
        <Panel>
          <TrendChart
            title="Runtime over time"
            subtitle={`Mean ±1 SD per ${r.weekly ? "week" : "day"} · log scale from 1 min`}
            groups={["Runtime"]}
            series={[r.trend]}
            weekly={r.weekly}
            yLabel="Runtime (log scale)"
            xLabel="run start"
            log
            format={formatDuration}
            tick={compact}
            note={`${r.n} finished runs · ${s.runs.live} live excluded`}
          />
        </Panel>
        <Panel>
          <TrendChart
            title="Reply latency over time"
            subtitle={`Inbound message to first reply · mean ±1 SD per ${r.weekly ? "week" : "day"} · log scale`}
            groups={["Reply latency"]}
            series={[l.trend]}
            weekly={r.weekly}
            yLabel="Latency (log scale)"
            xLabel="message"
            log
            format={formatDuration}
            tick={compact}
            note={`${l.n} replies · ${l.unanswered} unanswered`}
          />
        </Panel>
        <Panel>
          <BarChart
            title="Wakes by kind"
            subtitle="Wakes created in the period"
            groups={["Wakes"]}
            buckets={Object.entries(s.wakesByKind).map(([kind, n]) => ({
              label: kind,
              counts: [n],
            }))}
          />
        </Panel>
      </div>
      <Panel aria-label="Delivery and outcomes">
        <PanelHead label="Delivery and outcomes" meta={`${s.days}-day window`} />
        <Table>
          <tbody>
            {(
              [
                ["Mid-run deliveries (hook)", s.deliveries.midRun],
                ["Prompt deliveries", s.deliveries.prompt],
                ["Follow-up runs", s.deliveries.followUpRuns],
                ["Undelivered wakes", s.deliveries.undelivered],
                ["Resumed runs", s.sessions.resumed],
                ["Fresh runs", s.sessions.fresh],
                ["Completed runs", o.ok],
                ["Failed runs", o.failure],
                ["Owner stops", o.stop],
                ["Runs ended by pause", o.pause],
                ["Dispatcher interruptions", o.interrupted],
                ["Owner pauses", o.ownerPauses],
                ["Automatic pauses (attention)", o.attention],
                ["Security drops", o.securityDrops],
              ] as const
            ).map(([label, n]) => (
              <tr key={label}>
                <Td>{label}</Td>
                <Td numeric zero={!n}>
                  {n}
                </Td>
              </tr>
            ))}
          </tbody>
        </Table>
      </Panel>
    </section>
  );
}
