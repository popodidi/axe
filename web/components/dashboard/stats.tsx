"use client";
import { useEffect, useState } from "react";
import { type Task, formatDuration, matchesTask } from "../../lib/model";
import {
  DAILY_TREND_MAX_DAYS,
  calculateStats,
  collapseGroups,
  completionCandidates,
  dailyCompletions,
  dateRange,
  loadHistory,
  metrics,
  scopeLabels,
  trendBuckets,
  trends,
  typeLabels,
  typeScope,
  type History,
  type Breakdown,
  type TaskMetric,
} from "../../lib/stats";
import {
  Alert,
  Button,
  EmptyState,
  Filter,
  Panel,
  PanelHead,
  Select,
  Skeleton,
  Table,
  Td,
  Th,
  choices,
} from "../ui";
import { BarChart, Scatter, TrendChart } from "./stats-charts";
import styles from "./stats.module.css";
const DAY = 86400000;
const date = (n: number) => new Date(n).toISOString().slice(0, 10);
const utc = (n: number) =>
  new Date(n).toISOString().slice(0, 16).replace("T", " ");
const value = (n: number | null, unit: string) =>
  n === null ? "Unknown" : unit === "duration" ? formatDuration(n) : String(n);
const compact = (ms: number) =>
  ms < 3600000
    ? `${Math.round(ms / 60000)}m`
    : ms < 86400000
      ? `${Math.round(ms / 3600000)}h`
      : `${Math.round(ms / 86400000)}d`;
// Point labels keep full precision; axes use the compact form.
const mean = (n: number) => String(Math.round(n * 100) / 100);
const trendCopy: Record<string, { title: string; y: string }> = {
  runCount: { title: "Runs per task over time", y: "Runs (mean)" },
  escalations: { title: "Escalations per task over time", y: "Escalations (mean)" },
  elapsed: { title: "Elapsed time over time", y: "Elapsed (log scale)" },
  runtime: { title: "Agent runtime over time", y: "Runtime (log scale)" },
};
function Comparison({ title, groups }: { title: string; groups: Breakdown[] }) {
  return (
    <Panel aria-label={title}>
      <PanelHead label={title} meta="Known samples · p90 uses nearest rank" />
      <Table>
        <thead>
          <tr>
            <Th>Group</Th>
            <Th numeric>Tasks</Th>
            <Th>Metric</Th>
            <Th numeric>Samples / unknown</Th>
            <Th numeric>Median</Th>
            <Th numeric>P90</Th>
            <Th numeric>Max</Th>
          </tr>
        </thead>
        <tbody>
          {groups.flatMap((g) =>
            metrics.map((m) => {
              const s = g.metrics[m.key];
              return (
                <tr key={g.label + m.key}>
                  <Td>{g.label}</Td>
                  <Td numeric>{g.tasks}</Td>
                  <Td>{m.label}</Td>
                  <Td numeric>
                    {s.n} / {s.unknown}
                  </Td>
                  <Td numeric>{value(s.median, m.unit)}</Td>
                  <Td numeric>{value(s.p90, m.unit)}</Td>
                  <Td numeric>{value(s.max, m.unit)}</Td>
                </tr>
              );
            }),
          )}
        </tbody>
      </Table>
    </Panel>
  );
}
function Kpi({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <div className={styles.kpi}>
      <div className={styles.kpiLabel}>{label}</div>
      <div className={styles.kpiValue}>{value}</div>
      <div className={styles.kpiDetail}>{detail}</div>
    </div>
  );
}
const subtitles: Record<string, string> = {
  runCount: "Tasks by number of agent runs",
  escalations: "Tasks by number of escalations to the owner",
  elapsed: "Tasks by creation-to-completion time, including waits",
  runtime: "Tasks by summed agent run time",
};
// One-page report: the header filters control everything below it.
export function Stats({
  tasks,
  base,
  params: raw,
  onRange,
  onOpenTask,
}: {
  tasks: Task[];
  base: string;
  params: URLSearchParams;
  onRange: (patch: Record<string, string | null>) => void;
  onOpenTask: (id: number) => void;
}) {
  // The report has no search box, so a stale task search never filters it.
  const params = new URLSearchParams(raw);
  params.delete("q");
  const [split, setSplit] = useState("none");
  const [snapshot, setSnapshot] = useState(() => ({ tasks, at: Date.now() }));
  const [result, setResult] = useState<{
    key: string;
    history: History;
  } | null>(null);
  const [error, setError] = useState<{ key: string; message: string } | null>(
    null,
  );
  const range = dateRange(params, snapshot.at);
  const key = [params.get("project"), range.start, range.end, snapshot.at].join(
    "|",
  );
  const scope = typeScope(params);
  const query = new URLSearchParams(
    Object.fromEntries(
      ["project", "days", "start", "end"]
        .filter((k) => params.has(k))
        .map((k) => [k, params.get(k)!]),
    ),
  ).toString();
  useEffect(() => {
    const p = new URLSearchParams(query);
    const range = dateRange(p, snapshot.at);
    if (range.error) return;
    const controller = new AbortController();
    // Type is classified for the whole cohort once, so switching
    // All / Normal / Routine reuses this history instead of reloading it.
    loadHistory(
      completionCandidates(snapshot.tasks, p, range),
      base,
      controller.signal,
      {
        open: snapshot.tasks.filter(
          (t) => matchesTask(t, p) && t.state !== "done",
        ),
      },
    )
      .then((history) => {
        if (!controller.signal.aborted) setResult({ key, history });
      })
      .catch((e) => {
        if (!controller.signal.aborted) setError({ key, message: String(e) });
      });
    return () => controller.abort();
  }, [snapshot, base, key, query]);
  const history = result?.key === key ? result.history : null;
  const s = history && calculateStats(snapshot.tasks, history, params);
  const custom =
    params.has("start") || params.has("end") || params.get("days") === "custom";
  const projects = Array.from(new Set(tasks.map((t) => t.project))).sort();
  const groups: Breakdown[] = !s
    ? []
    : split === "project"
      ? s.projects
      : split === "agent"
        ? s.agents
        : split === "type"
          ? s.taskTypes
          : s.total;
  const groupOf = (r: TaskMetric) =>
    Math.max(
      0,
      groups.findIndex(
        (g) =>
          g.label ===
          (split === "project"
            ? r.task.project
            : split === "agent"
              ? r.agents
              : split === "type"
                ? typeLabels[r.type]
                : scopeLabels[scope]),
      ),
    );
  const noun = scope === "all" ? "" : `${scope} `;
  const total = s?.total[0];
  const pct = (a: number, b: number) =>
    b ? `${Math.round((a / b) * 100)}%` : "—";
  const days = s ? Math.round((s.end - s.start) / DAY) : 0;
  // Trends keep at most six series (top five groups plus Other).
  const trendGroups = collapseGroups(groups);
  const trendRange = s ? trendBuckets(s.start, s.end) : null;
  const trend =
    s &&
    trendRange &&
    trends(
      s.rows,
      trendRange.buckets,
      trendGroups.labels.length,
      (r) => trendGroups.index[groupOf(r)],
    );
  return (
    <div className={styles.desk}>
      <Panel aria-label="Statistics scope" className={styles.header}>
        <div className={styles.titleRow}>
          <div>
            <h1 id="list-heading" tabIndex={-1} className={styles.title}>
              Delivery report
            </h1>
            <p className={styles.lede}>
              Completed tasks in the selected UTC dates: how many runs,
              escalations and how much time they took.
            </p>
          </div>
          <Button
            onClick={() => {
              setError(null);
              setSnapshot({ tasks, at: Date.now() });
            }}
          >
            Refresh history
          </Button>
        </div>
        <div className={styles.filters}>
          <Filter
            label="Project"
            aria-label="project"
            value={params.get("project") || ""}
            onChange={(v) => onRange({ project: v || null })}
            options={[{ value: "", label: "All projects" }, ...choices(projects)]}
          />
          <Select
            label="Period"
            ariaLabel="Date range"
            value={custom ? "custom" : params.get("days") || "7"}
            onChange={(v) =>
              onRange(
                v === "custom"
                  ? {
                      days: "custom",
                      start: date(range.start),
                      end: date(range.end - 1),
                    }
                  : { days: v, start: null, end: null },
              )
            }
            options={[
              { value: "7", label: "Last 7 days" },
              { value: "30", label: "Last 30 days" },
              { value: "90", label: "Last 90 days" },
              { value: "custom", label: "Custom dates" },
            ]}
          />
          {custom && (
            <>
              <label className={styles.dateField}>
                Start (UTC)
                <input
                  type="date"
                  aria-label="Start date (UTC)"
                  value={params.get("start") || ""}
                  onChange={(e) => onRange({ start: e.target.value })}
                />
              </label>
              <label className={styles.dateField}>
                End (UTC, inclusive)
                <input
                  type="date"
                  aria-label="End date (UTC, inclusive)"
                  max={date(snapshot.at)}
                  value={params.get("end") || ""}
                  onChange={(e) => onRange({ end: e.target.value })}
                />
              </label>
            </>
          )}
          <Select
            label="Task type"
            ariaLabel="Task type"
            value={scope}
            onChange={(v) => onRange({ type: v === "all" ? null : v })}
            options={[
              { value: "all", label: "All tasks" },
              { value: "normal", label: "Normal" },
              { value: "routine", label: "Routine" },
            ]}
          />
          <Select
            label="Group by"
            ariaLabel="Split by"
            value={split}
            onChange={setSplit}
            options={[
              { value: "none", label: "None" },
              { value: "project", label: "Project" },
              { value: "agent", label: "Coding agent" },
              { value: "type", label: "Task type" },
            ]}
          />
        </div>
        {range.error && <Alert>{range.error}</Alert>}
        {s && (
          <dl className={styles.types} aria-label="Completed by type">
            {(["normal", "routine", "unknown"] as const).map((t) => (
              <div
                key={t}
                className={styles.type}
                data-active={scope === t || undefined}
              >
                <dt>{typeLabels[t]}</dt>
                <dd>{s.types[t]}</dd>
              </div>
            ))}
            <div className={styles.type} data-active={scope === "all" || undefined}>
              <dt>All completed</dt>
              <dd>{s.types.normal + s.types.routine + s.types.unknown}</dd>
            </div>
          </dl>
        )}
        {s && (
          <p className={styles.scope}>
            <strong className={styles.active}>
              Showing{" "}
              {scope === "all"
                ? "all tasks (normal and routine)"
                : `${scope} tasks only`}
            </strong>{" "}
            · Snapshot {utc(history!.at)} UTC · {s.rows.length} completed{" "}
            {noun}cohort tasks · {s.open} open {noun}tasks (not sampled) ·{" "}
            {s.unknownCompletion} {noun}tasks with unknown completion.
            {s.unknownCompletion > 0 &&
              " Excluded from samples; cohort may be incomplete."}
            {s.types.unknown + s.openTypes.unknown + s.unknownCompletionTypes.unknown > 0 &&
              (scope === "all"
                ? ` Type unknown for ${s.types.unknown + s.openTypes.unknown + s.unknownCompletionTypes.unknown} tasks (routine link unreadable); counted only in All.`
                : ` ${s.excludedUnknownType} tasks of unknown type excluded (routine link unreadable).`)}
          </p>
        )}
        <details className={styles.details}>
          <summary>History coverage and definitions</summary>
          <p>
            Every matching completed-task history and every recorded run. Missing completion excludes a task from
            the cohort and is reported separately. Missing run history is
            Unknown, never zero. Elapsed includes waits; runtime sums actual run
            spans. Mixed agents form exclusive groups; each task contributes
            once. Median averages the middle pair; p90 uses nearest rank.
            Routine tasks were created by a native Routine (scheduled or
            manually triggered, including paused or archived routines), read
            from each task&apos;s routine link; Normal tasks have none. An
            unreadable link is Unknown type, never Normal.
            Trends bucket by UTC completion date: daily up to{" "}
            {DAILY_TREND_MAX_DAYS} days, otherwise seven-day weeks from the
            range start. Each point is the mean of known samples with a
            population SD band (one sample: SD 0) clipped at zero; buckets
            without samples are gaps, never zero.
          </p>
        </details>
      </Panel>
      {!range.error &&
        (error?.key === key ? (
          <Alert>
            History unavailable: {error.message}. Use Refresh history to retry.
          </Alert>
        ) : !s || !history ? (
          <Skeleton label="Loading history…" lines={4} />
        ) : !s.rows.length || !total ? (
          <EmptyState>
            No known completed {noun}tasks in this range match the filters.
          </EmptyState>
        ) : (
          <>
            <section className={styles.kpis} aria-label="Key figures">
              <Kpi
                label={scope === "all" ? "Completed tasks" : `Completed ${scope} tasks`}
                value={String(s.rows.length)}
                detail={
                  scope === "all"
                    ? `${s.types.normal} normal · ${s.types.routine} routine${s.types.unknown ? ` · ${s.types.unknown} unknown` : ""} · ${s.open} still open`
                    : `${days}-day window · ${s.open} still open`
                }
              />
              <Kpi
                label="Median elapsed"
                value={value(total.metrics.elapsed.median, "duration")}
                detail={`P90 ${value(total.metrics.elapsed.p90, "duration")}`}
              />
              <Kpi
                label="Median agent runtime"
                value={value(total.metrics.runtime.median, "duration")}
                detail={`${total.metrics.runtime.n} known · ${total.metrics.runtime.unknown} unknown`}
              />
              <Kpi
                label="Runs per task"
                value={value(total.metrics.runCount.median, "runs")}
                detail={`Median · P90 ${value(total.metrics.runCount.p90, "runs")}`}
              />
              <Kpi
                label="Escalated"
                value={pct(
                  s.rows.filter((r) => (r.escalations ?? 0) > 0).length,
                  s.rows.length,
                )}
                detail={`${s.rows.filter((r) => (r.escalations ?? 0) > 0).length} of ${s.rows.length} tasks needed the owner`}
              />
            </section>
            <Panel className={styles.wide}>
              <BarChart
                title="Completions per day"
                subtitle="Completed tasks by UTC completion date"
                groups={groups.map((g) => g.label)}
                labelEvery={days > 31 ? 14 : days > 10 ? 5 : 1}
                buckets={dailyCompletions(
                  s.rows,
                  s.start,
                  days,
                  groups.length,
                  groupOf,
                ).map((b) => ({
                  label: date(b.start).slice(5),
                  counts: b.counts,
                }))}
              />
            </Panel>
            <section className={styles.trends} aria-labelledby="trends-heading">
              <div>
                <h2 id="trends-heading" className={styles.sectionTitle}>
                  Trends
                </h2>
                <p className={styles.sectionLede}>
                  Mean per UTC completion {trendRange!.weekly ? "week" : "day"}{" "}
                  with a shaded ±1 SD band and known samples (n) per{" "}
                  {trendRange!.weekly ? "week" : "day"}.
                  {groups.length > 1 &&
                    " Every group draws its mean; choose, hover or focus a group to show its band."}
                </p>
              </div>
              <div className={styles.trendGrid}>
                {metrics.map((m) => {
                  const duration = m.unit === "duration";
                  const points = trend![m.key].flat();
                  return (
                    <Panel key={m.key}>
                      <TrendChart
                        key={split}
                        title={trendCopy[m.key].title}
                        subtitle={`Mean ±1 SD per ${trendRange!.weekly ? "week" : "day"}${duration ? " · log scale from 1 min" : ""}`}
                        groups={trendGroups.labels}
                        series={trend![m.key]}
                        weekly={trendRange!.weekly}
                        yLabel={trendCopy[m.key].y}
                        log={duration}
                        format={duration ? formatDuration : mean}
                        tick={duration ? compact : mean}
                        note={`${points.reduce((n, p) => n + p.n, 0)} known samples · ${points.reduce((n, p) => n + p.unknown, 0)} unknown`}
                      />
                    </Panel>
                  );
                })}
              </div>
            </section>
            <div className={styles.charts}>
              <Panel>
                <Scatter
                  title="Runs and escalations"
                  subtitle="One dot per task. Up and right: tasks that needed more attempts and owner help."
                  groups={groups.map((g) => g.label)}
                  xLabel="Runs"
                  yLabel="Escalations"
                  jitter
                  onOpen={onOpenTask}
                  points={s.rows
                    .filter((r) => r.runCount !== null)
                    .map((r) => ({
                      id: r.task.id,
                      label: r.task.title,
                      group: groupOf(r),
                      x: r.runCount!,
                      y: r.escalations ?? 0,
                    }))}
                  note={`${s.rows.filter((r) => r.runCount !== null).length} plotted · ${s.rows.filter((r) => r.runCount === null).length} unknown runs`}
                />
              </Panel>
              <Panel>
                <Scatter
                  title="Elapsed and agent runtime"
                  subtitle="One dot per task, log scale. Far right of the line: most of the time was spent waiting, not working."
                  groups={groups.map((g) => g.label)}
                  xLabel="Elapsed"
                  yLabel="Runtime"
                  log
                  format={compact}
                  diagonal="runtime = elapsed"
                  onOpen={onOpenTask}
                  points={s.rows
                    .filter((r) => r.runtime !== null && r.elapsed !== null)
                    .map((r) => ({
                      id: r.task.id,
                      label: r.task.title,
                      group: groupOf(r),
                      x: r.elapsed!,
                      y: r.runtime!,
                    }))}
                  note={`${s.rows.filter((r) => r.runtime !== null).length} plotted · ${s.rows.filter((r) => r.runtime === null).length} unknown runtime`}
                />
              </Panel>
              {metrics.map((m) => (
                <Panel key={m.key}>
                  <BarChart
                    title={m.label}
                    subtitle={subtitles[m.key]}
                    groups={groups.map((g) => g.label)}
                    buckets={total.histograms[m.key].map((bucket, i) => ({
                      label: bucket.label,
                      counts: groups.map((g) => g.histograms[m.key][i].count),
                    }))}
                    note={`${total.metrics[m.key].n} samples · ${total.metrics[m.key].unknown} unknown`}
                  />
                </Panel>
              ))}
            </div>
            <Comparison
              title={scope === "all" ? "All completed tasks" : `Completed ${scope} tasks`}
              groups={s.total}
            />
            <Comparison title="By task type" groups={s.taskTypes} />
            <Comparison title="By project" groups={s.projects} />
            <Comparison title="By coding agent" groups={s.agents} />
          </>
        ))}
    </div>
  );
}
