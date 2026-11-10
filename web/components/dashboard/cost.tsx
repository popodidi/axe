"use client";
import { useEffect, useMemo, useState } from "react";
import {
  breakdownOf,
  breakdownTitle,
  bucketTokens,
  costBreakdowns,
  costFilters,
  costPath,
  costScope,
  costSections,
  DEFAULT_BREAKDOWN,
  orderRows,
  periodGap,
  reasonLabel,
  share,
  splitBars,
  tokenBars,
  tokenCount,
  tokenTick,
  totalSeries,
  usd,
  usdTick,
  type CostGroup,
  type CostSeries,
  type CostStats,
  type StackedBars,
} from "../../lib/cost";
import { dateRange, trendBuckets } from "../../lib/stats";
import {
  Alert,
  EmptyState,
  Filter,
  Panel,
  PanelHead,
  Select,
  Skeleton,
  Table,
  Td,
  Th,
} from "../ui";
import { BarChart, TrendChart } from "./stats-charts";
import styles from "./stats.module.css";
import own from "./cost.module.css";

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

function Breakdown({ group, stats }: { group: CostGroup; stats: CostStats }) {
  const rows = orderRows(stats.breakdowns[group.key] ?? [], group.order);
  return (
    <Panel aria-label={group.title}>
      <PanelHead label={group.title} />
      <Table>
        <thead>
          <tr>
            <Th>Group</Th>
            <Th numeric>Est. cost</Th>
            <Th numeric>Share</Th>
            <Th numeric>Runs</Th>
            <Th numeric>Unknown</Th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.key}>
              <Td primary>{r.key}</Td>
              <Td numeric>{usd(r.cost)}</Td>
              <Td numeric>{share(r.cost, stats.total.cost)}</Td>
              <Td numeric>{r.runs}</Td>
              <Td numeric>{r.unknown_runs}</Td>
            </tr>
          ))}
        </tbody>
      </Table>
    </Panel>
  );
}

function TokenTypes({ stats }: { stats: CostStats }) {
  const all = stats.token_types.reduce((n, t) => n + t.tokens, 0);
  return (
    <Panel aria-label="Token type">
      <PanelHead label="Token type" meta="Tokens only; cost is not split by type" />
      <Table>
        <thead>
          <tr>
            <Th>Type</Th>
            <Th numeric>Tokens</Th>
            <Th numeric>Share</Th>
          </tr>
        </thead>
        <tbody>
          {stats.token_types.map((t) => (
            <tr key={t.key}>
              <Td primary>{t.key}</Td>
              <Td numeric>{tokenCount(t.tokens)}</Td>
              <Td numeric>{share(t.tokens, all)}</Td>
            </tr>
          ))}
        </tbody>
      </Table>
    </Panel>
  );
}

function Stacked({
  title,
  subtitle,
  bars,
  format,
  tick,
}: {
  title: string;
  subtitle: string;
  bars: StackedBars;
  format: (n: number) => string;
  tick: (n: number) => string;
}) {
  return (
    <Panel>
      <BarChart
        title={title}
        subtitle={subtitle}
        groups={bars.groups}
        buckets={bars.buckets}
        labelEvery={bars.buckets.length > 10 ? 5 : 1}
        format={format}
        tick={tick}
        showTotals={false}
      />
    </Panel>
  );
}

function OverTime({
  series,
  breakdown,
  onBreakdown,
}: {
  series: CostSeries;
  breakdown: string;
  onBreakdown: (key: string) => void;
}) {
  const weekly = series.step_days > 1;
  const per = weekly ? "week" : "day";
  const gaps = series.buckets.map((b) => periodGap(b, series.first_known));
  const estimate = `Estimated at API prices, per UTC run-end ${per}`;
  const trend = {
    weekly,
    gaps,
    total: true,
    xLabel: "run end",
    groups: ["Total"],
    note: "n counts runs with known usage. Shaded periods are incomplete: unknown runs are left out, never counted as zero.",
  };
  return (
    <section aria-label="Over time" className={own.section}>
      <div className={own.sectionHead}>
        <h2 className={own.sectionTitle}>Over time</h2>
        <Filter
          label="Stack by"
          aria-label="Stack cost chart by"
          value={breakdown}
          onChange={onBreakdown}
          options={costBreakdowns.map((g) => ({
            value: g.key,
            label: g.title,
          }))}
        />
      </div>
      <div className={own.grid}>
        <Panel>
          <TrendChart
            {...trend}
            title="Estimated cost"
            subtitle={estimate}
            series={[totalSeries(series, (b) => b.cost)]}
            yLabel="Est. cost (USD)"
            format={usd}
            tick={usdTick}
          />
        </Panel>
        <Panel>
          <TrendChart
            {...trend}
            title="Total tokens"
            subtitle={`Input, output, cache read and write, per UTC run-end ${per}`}
            series={[totalSeries(series, bucketTokens)]}
            yLabel="Tokens"
            format={tokenCount}
            tick={tokenTick}
          />
        </Panel>
        {[breakdown, "source", "model"]
          .filter((key, i, all) => all.indexOf(key) === i)
          .map((key) => (
            <Stacked
              key={key}
              title={breakdownTitle(key)}
              subtitle={estimate}
              bars={splitBars(series, key)}
              format={usd}
              tick={usdTick}
            />
          ))}
        <Stacked
          title="Tokens by type"
          subtitle={`Output includes thinking, per UTC run-end ${per}`}
          bars={tokenBars(series)}
          format={tokenCount}
          tick={tokenTick}
        />
      </div>
    </section>
  );
}

export function Cost({
  base,
  params,
  onRange,
}: {
  base: string;
  params: URLSearchParams;
  onRange: (patch: Record<string, string | null>) => void;
}) {
  const days = params.get("days") || "7";
  const [now] = useState(() => Date.now());
  const range = useMemo(
    () => dateRange(new URLSearchParams({ days }), now),
    [days, now],
  );
  const breakdown = breakdownOf(params);
  const active = costFilters.some((f) => params.get(f.param));
  // One request whatever the range or filters: the server aggregates and buckets.
  const path = costPath(
    range,
    trendBuckets(range.start, range.end).weekly ? 7 : 1,
    costScope(params),
  );
  const [state, setState] = useState<{
    path: string;
    stats?: CostStats;
    error?: string;
    /** Survive a reload, so the filters stay usable while it runs. */
    options?: Record<string, string[]>;
  } | null>(null);
  useEffect(() => {
    const abort = new AbortController();
    fetch(base + path, { signal: abort.signal })
      .then(async (r) => {
        if (!r.ok) throw Error(`HTTP ${r.status}`);
        const stats = (await r.json()) as CostStats;
        setState({ path, stats, options: stats.filter_options });
      })
      .catch((e: Error) => {
        if (!abort.signal.aborted)
          setState((s) => ({ path, error: e.message, options: s?.options }));
      });
    return () => abort.abort();
  }, [base, path]);
  const current = state?.path === path ? state : null;
  const stats = current?.stats;
  const reasons = stats ? Object.entries(stats.coverage.unknown_reasons) : [];
  const tokens = stats
    ? stats.token_types.reduce((n, t) => n + t.tokens, 0)
    : 0;
  return (
    <div className={styles.desk}>
      <Panel aria-label="Cost scope" className={styles.header}>
        <div className={styles.titleRow}>
          <div>
            <h1 id="list-heading" tabIndex={-1} className={styles.title}>
              Cost
            </h1>
            <p className={styles.lede}>
              Token usage across task and bot runs. Cost is an API-price
              estimate, not a bill.
            </p>
          </div>
        </div>
        <div className={styles.filters}>
          <Select
            label="Period"
            ariaLabel="Date range"
            value={["7", "30", "90"].includes(days) ? days : "7"}
            onChange={(v) => onRange({ days: v, start: null, end: null })}
            options={[
              { value: "7", label: "Last 7 days" },
              { value: "30", label: "Last 30 days" },
              { value: "90", label: "Last 90 days" },
            ]}
          />
          {costFilters.map((f) => {
            const value = params.get(f.param) || "";
            // A value from a shared URL stays selectable if the period lacks it.
            const values = new Set(state?.options?.[f.name]);
            if (value) values.add(value);
            return (
              <Filter
                key={f.name}
                label={f.label}
                aria-label={`Filter by ${f.name}`}
                value={value}
                onChange={(v) => onRange({ [f.param]: v || null })}
                options={[
                  { value: "", label: "All" },
                  ...[...values].sort().map((v) => ({ value: v, label: v })),
                ]}
              />
            );
          })}
        </div>
      </Panel>
      {current?.error ? (
        <Alert>Could not load cost data: {current.error}</Alert>
      ) : !stats ? (
        <Skeleton label="Loading cost…" lines={4} />
      ) : stats.coverage.runs === 0 ? (
        <EmptyState>
          {active
            ? "No finished runs match these filters."
            : "No finished runs in this period."}
        </EmptyState>
      ) : (
        <>
          <section className={styles.kpis} aria-label="Key figures">
            <Kpi
              label="Estimated cost"
              value={usd(stats.total.cost)}
              detail="API-price estimate of known runs"
            />
            <Kpi
              label="Runs with usage"
              value={`${stats.coverage.known} / ${stats.coverage.runs}`}
              detail={`${stats.coverage.unknown} unknown, counted as no data`}
            />
            <Kpi
              label="Cost per finished task"
              value={
                stats.finished.cost_per_task === null
                  ? "Unknown"
                  : usd(stats.finished.cost_per_task)
              }
              detail={`${stats.finished.tasks} done tasks with known runs`}
            />
            <Kpi
              label="Tokens"
              value={tokenCount(tokens)}
              detail="Input, output, cache read and write"
            />
          </section>
          {reasons.length > 0 && (
            <Panel aria-label="Unknown runs">
              <PanelHead
                label="Why some runs are unknown"
                meta="Excluded from cost, never counted as zero"
              />
              <ul className={own.reasons}>
                {reasons.map(([reason, n]) => (
                  <li key={reason}>
                    {n} · {reasonLabel(reason)}
                  </li>
                ))}
              </ul>
            </Panel>
          )}
          {stats.series && (
            <OverTime
              series={stats.series}
              breakdown={breakdown}
              onBreakdown={(key) =>
                onRange({
                  breakdown: key === DEFAULT_BREAKDOWN ? null : key,
                })
              }
            />
          )}
          {costSections.map((section) => (
            <section
              key={section.title}
              aria-label={section.title}
              className={own.section}
            >
              <h2 className={own.sectionTitle}>{section.title}</h2>
              <div className={own.grid}>
                {section.groups.map((g) => (
                  <Breakdown key={g.key} group={g} stats={stats} />
                ))}
                {section.title === "Why" && <TokenTypes stats={stats} />}
              </div>
            </section>
          ))}
        </>
      )}
    </div>
  );
}
