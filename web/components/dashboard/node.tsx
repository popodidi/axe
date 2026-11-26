"use client";
import { useEffect, useState } from "react";
import {
  age,
  count,
  gib,
  load,
  nodePath,
  nodeRange,
  nodeRangeOptions,
  niceMax,
  pct,
  RUN_KINDS,
  runNames,
  runTotal,
  stale,
  type NodeRange,
  type NodeSeries,
} from "../../lib/node";
import { getJson } from "../../lib/store";
import { Alert, EmptyState, Panel, PanelHead, Select, Skeleton, Table, Td, Th, cx } from "../ui";
import { TimeChart } from "./node-chart";
import stats from "./stats.module.css";
import styles from "./node.module.css";

const GIB = 2 ** 30;
/** How often the view refetches; the node samples every `sample_seconds`. */
const REFRESH_MS = 15000;

function Kpi({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <div className={stats.kpi}>
      <div className={stats.kpiLabel}>{label}</div>
      <div className={stats.kpiValue}>{value}</div>
      <div className={stats.kpiDetail}>{detail}</div>
    </div>
  );
}

function useNodeSeries(base: string, range: NodeRange) {
  const [state, setState] = useState<{ range: NodeRange; data?: NodeSeries; error?: string } | null>(
    null,
  );
  useEffect(() => {
    const c = new AbortController();
    const load = () =>
      getJson<NodeSeries>(base + nodePath(range), { signal: c.signal })
        .then((data) => setState({ range, data }))
        .catch((e) => {
          if (!c.signal.aborted) setState((s) => ({ range, data: s?.data, error: String(e) }));
        });
    void load();
    const timer = setInterval(load, REFRESH_MS);
    return () => {
      c.abort();
      clearInterval(timer);
    };
  }, [base, range]);
  return state;
}

/** The server, the node binary and the CLI on the node host (`node_metrics.rs`). */
function versions(s: NodeSeries): [string, string | null][] {
  return [
    ["axe server", s.server_version],
    ["node", s.current?.node_version ?? null],
    ["CLI", s.current?.cli_version ?? null],
  ];
}

/** What the points are, for chart subtitles. */
function source(s: NodeSeries): string {
  const step = s.step_seconds;
  const per = step < 60 ? `${step} s` : step < 3600 ? `${step / 60} min` : `${step / 3600} h`;
  return s.source === "raw"
    ? step <= s.sample_seconds
      ? `Raw ${s.sample_seconds} s samples`
      : `Raw samples, averaged per ${per}; band to the peak`
    : `5-minute rollups, averaged per ${per}; band to the peak`;
}

export function Node({
  base,
  params,
  onRange,
}: {
  base: string;
  params: URLSearchParams;
  onRange: (patch: Record<string, string | null>) => void;
}) {
  const range = nodeRange(params);
  const state = useNodeSeries(base, range);
  // While a new range loads, the previous charts stay at reduced opacity.
  const s = state?.data;
  const loading = state?.range !== range;
  const cur = s?.current ?? null;
  const memTotal = cur?.mem_total_bytes ?? null;
  return (
    <div className={stats.desk}>
      <Panel aria-label="Node scope" className={stats.header}>
        <div className={stats.titleRow}>
          <div>
            <h1 id="list-heading" tabIndex={-1} className={stats.title}>
              Node
            </h1>
            <p className={stats.lede}>
              Host load and running runs of the axe node. Times are UTC.
            </p>
            {s && (
              <p className={styles.versions} aria-label="Versions">
                {versions(s).map(([label, v]) => (
                  <span key={label}>
                    {label} <code>{v ?? "?"}</code>
                  </span>
                ))}
              </p>
            )}
          </div>
        </div>
        <div className={stats.filters}>
          <Select
            label="Range"
            ariaLabel="Time range"
            value={range}
            onChange={(v) => onRange({ range: v })}
            options={nodeRangeOptions}
          />
        </div>
      </Panel>
      {state?.error && !s ? (
        <Alert>Could not load node metrics: {state.error}</Alert>
      ) : !s ? (
        <Skeleton label="Loading node metrics…" lines={4} />
      ) : !cur ? (
        <EmptyState>
          No node samples yet. The node reports every [node_metrics] sample_seconds (15 s by default) once it runs this
          version.
        </EmptyState>
      ) : (
        <div className={cx(styles.body, loading && styles.loading)} aria-busy={loading}>
          {state?.error && <Alert>Could not refresh: {state.error}</Alert>}
          {stale(s) && (
            <Alert>
              The last sample is {age(s.now - cur.at)} old: the node may be down or unable to reach the server.
            </Alert>
          )}
          <section className={stats.kpis} aria-label="Current values">
            <Kpi
              label="CPU"
              value={pct(cur.cpu_percent)}
              detail={`load ${load(cur.load1)} · ${load(cur.load5)} · ${load(cur.load15)}`}
            />
            <Kpi
              label="Memory used"
              value={gib(cur.mem_used_bytes)}
              detail={
                memTotal
                  ? `of ${gib(memTotal)} · ${pct((100 * (cur.mem_used_bytes ?? 0)) / memTotal)}`
                  : "total unknown"
              }
            />
            <Kpi
              label="Disk free"
              value={gib(cur.disk_free_bytes)}
              detail={`of ${gib(cur.disk_total_bytes)} · ${cur.disk_path || "run volume"}`}
            />
            <Kpi
              label="Running runs"
              value={String(RUN_KINDS.reduce((n, k) => n + runTotal(cur.runs[k.key]), 0))}
              detail={RUN_KINDS.map((k) => `${runTotal(cur.runs[k.key])} ${k.label.toLowerCase()}`).join(" · ")}
            />
            <Kpi label="Last sample" value={`${age(s.now - cur.at)} ago`} detail={`every ${s.sample_seconds} s`} />
          </section>
          <div className={styles.charts}>
            <Panel>
              <TimeChart
                title="CPU"
                subtitle={source(s)}
                points={s.points}
                step={s.step_seconds}
                from={s.from}
                to={s.to}
                yMax={100}
                series={[
                  {
                    label: "CPU",
                    slot: 0,
                    values: s.points.map((p) => p.cpu_avg),
                    peaks: s.source === "raw" && s.step_seconds <= s.sample_seconds ? undefined : s.points.map((p) => p.cpu_max),
                  },
                ]}
                format={pct}
                tick={(n) => `${n}%`}
                yLabel="Busy %"
              />
            </Panel>
            <Panel>
              <TimeChart
                title="Memory used"
                subtitle={source(s)}
                points={s.points}
                step={s.step_seconds}
                from={s.from}
                to={s.to}
                reference={memTotal ? { value: memTotal, label: `total ${gib(memTotal)}` } : undefined}
                // Whole GiB ticks: the axis is sized in GiB, not bytes.
                yMax={memTotal ? niceMax((memTotal * 1.05) / GIB) * GIB : undefined}
                series={[
                  {
                    label: "Memory used",
                    slot: 0,
                    values: s.points.map((p) => p.mem_used_avg),
                    peaks: s.source === "raw" && s.step_seconds <= s.sample_seconds ? undefined : s.points.map((p) => p.mem_used_max),
                  },
                ]}
                format={gib}
                tick={(n) => `${Math.round(n / GIB)}`}
                yLabel="GiB"
              />
            </Panel>
            <Panel className={styles.wide}>
              <TimeChart
                title="Running runs by kind"
                subtitle={`${source(s).replace(/; band to the peak$/, "")}; stacked averages`}
                points={s.points}
                step={s.step_seconds}
                from={s.from}
                to={s.to}
                stacked
                series={RUN_KINDS.map((k) => ({
                  label: k.label,
                  slot: k.slot,
                  values: s.points.map((p) => p[`${k.key}_avg`]),
                }))}
                format={count}
                tick={(n) => String(Math.round(n * 10) / 10)}
                yLabel="Runs"
              />
            </Panel>
          </div>
          <Panel aria-label="Running now">
            <PanelHead label="Running now" meta={`sampled ${age(s.now - cur.at)} ago`} />
            <Table>
              <thead>
                <tr>
                  <Th>Kind</Th>
                  <Th numeric>Running</Th>
                  <Th>By name</Th>
                </tr>
              </thead>
              <tbody>
                {RUN_KINDS.map((k) => (
                  <tr key={k.key}>
                    <Td primary>{k.label}</Td>
                    <Td numeric>{runTotal(cur.runs[k.key])}</Td>
                    <Td>
                      {runNames(cur.runs[k.key])
                        .map(([name, n]) => (n > 1 ? `${name} ${n}` : name))
                        .join(", ") || "–"}
                    </Td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </Panel>
        </div>
      )}
    </div>
  );
}
