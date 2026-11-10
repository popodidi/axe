import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import type { TrendPoint } from "../../lib/stats";
import { cx } from "../ui";
import styles from "./stats-charts.module.css";
// Report charts. Exact values are always available as text (bar totals, a
// screen-reader data list, dot labels); color only separates groups, and a
// legend names them. Group colors cycle through six restrained series.
const series = (i: number) => styles[`s${i % 6}`];
export function Legend({ groups }: { groups: string[] }) {
  if (groups.length < 2) return null;
  return (
    <ul className={styles.legend} aria-label="Legend">
      {groups.map((g, i) => (
        <li key={g}>
          <svg aria-hidden="true" viewBox="0 0 10 10" className={styles.swatch}>
            <rect width="10" height="10" rx="2" className={series(i)} />
          </svg>
          {g}
        </li>
      ))}
    </ul>
  );
}
const niceMax = (n: number) => {
  if (n <= 5) return Math.max(1, Math.ceil(n));
  const step = Math.pow(10, Math.floor(Math.log10(n)));
  return Math.ceil(n / step) * step;
};
/**
 * Vertical bar chart: x buckets, y values stacked by group. A bucket's `gap`
 * says why its figure is incomplete and shades its column.
 */
export function BarChart({
  title,
  subtitle,
  groups,
  buckets,
  note,
  labelEvery = 1,
  format = String,
  tick = format,
  showTotals = true,
}: {
  title: string;
  subtitle: string;
  groups: string[];
  buckets: { label: string; counts: number[]; gap?: string | null }[];
  note?: string;
  labelEvery?: number;
  format?: (n: number) => string;
  tick?: (n: number) => string;
  /** Off when formatted totals would not fit above narrow bars. */
  showTotals?: boolean;
}) {
  const totals = buckets.map((b) => b.counts.reduce((s, n) => s + n, 0));
  const max = niceMax(Math.max(0, ...totals));
  const n = buckets.length;
  const H = 100;
  const gap = n > 30 ? 0.15 : 0.25;
  return (
    <figure className={styles.chart} aria-label={title}>
      <figcaption className={styles.head}>
        <h3 className={styles.title}>{title}</h3>
        <p className={styles.subtitle}>{subtitle}</p>
      </figcaption>
      <Legend groups={groups} />
      <div className={styles.plot}>
        <div className={styles.yAxis} aria-hidden="true">
          <span>{tick(max)}</span>
          <span>{tick(max / 2)}</span>
          <span>{tick(0)}</span>
        </div>
        <div className={styles.area}>
          <div className={styles.totals} aria-hidden="true">
            {totals.map((t, i) => (
              <span key={i}>{showTotals && t ? format(t) : "\u00a0"}</span>
            ))}
          </div>
          <svg
            className={styles.bars}
            viewBox={`0 0 ${n} ${H}`}
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            {[0.5, 1].map((f) => (
              <line
                key={f}
                x1="0"
                x2={n}
                y1={H - H * f}
                y2={H - H * f}
                className={styles.grid}
                vectorEffect="non-scaling-stroke"
              />
            ))}
            {buckets.map((b, i) =>
              b.gap ? (
                <rect
                  key={`gap-${i}`}
                  data-gap={b.label}
                  x={i}
                  y={0}
                  width={1}
                  height={H}
                  className={styles.gap}
                >
                  <title>{`${b.label}: ${b.gap}`}</title>
                </rect>
              ) : null,
            )}
            {buckets.map((b, i) => {
              let y = H;
              return b.counts.map((c, g) => {
                const h = (c / max) * H;
                y -= h;
                return c ? (
                  <rect
                    key={`${i}-${g}`}
                    x={i + gap / 2}
                    y={y}
                    width={1 - gap}
                    height={h}
                    className={series(g)}
                  >
                    <title>{`${b.label} · ${groups[g]}: ${format(c)}`}</title>
                  </rect>
                ) : null;
              });
            })}
          </svg>
          <div className={styles.xLabels} aria-hidden="true">
            {buckets.map((b, i) => (
              <span key={b.label}>{i % labelEvery === 0 ? b.label : ""}</span>
            ))}
          </div>
        </div>
      </div>
      {note && <p className={styles.note}>{note}</p>}
      <ul className={styles.data}>
        {buckets.map((b) => (
          <li key={b.label}>
            {b.label}:{" "}
            {groups.map((g, i) => `${g}: ${format(b.counts[i])}`).join(" · ")}
            {b.gap ? ` · ${b.gap}` : ""}
          </li>
        ))}
      </ul>
    </figure>
  );
}
// Log duration axes start at one minute and use readable duration ticks.
const MIN = 60000;
const durationTicks = [
  MIN, 10 * MIN, 60 * MIN, 360 * MIN, 1440 * MIN, 10080 * MIN, 43200 * MIN,
  129600 * MIN,
];
export type Point = {
  id: number;
  label: string;
  group: number;
  x: number;
  y: number;
};
/** Scatter plot: one dot per task; a dot opens its task. */
export function Scatter({
  title,
  subtitle,
  groups,
  points,
  xLabel,
  yLabel,
  log = false,
  format = String,
  jitter = false,
  diagonal,
  note,
  onOpen,
}: {
  title: string;
  subtitle: string;
  groups: string[];
  points: Point[];
  xLabel: string;
  yLabel: string;
  log?: boolean;
  format?: (n: number) => string;
  jitter?: boolean;
  diagonal?: string;
  note?: string;
  onOpen: (id: number) => void;
}) {
  const W = 420,
    H = 260,
    L = 52,
    B = 34,
    T = 10,
    R = 12;
  // Log axes (durations) start at one minute and use readable duration ticks;
  // count axes are padded half a step so integer dots never touch the frame.
  const t = (v: number) => (log ? Math.log10(Math.max(v, MIN)) : v);
  const maxX = Math.max(1, ...points.map((p) => p.x));
  const maxY = Math.max(1, ...points.map((p) => p.y));
  const top = (m: number) =>
    durationTicks.find((d) => d >= m * 1.2) ?? durationTicks.at(-1)!;
  const lo = log ? t(MIN) : -0.5;
  const hiX = log ? t(top(Math.max(maxX, maxY))) : Math.ceil(maxX) + 0.5;
  const hiY = log ? hiX : Math.ceil(maxY) + 0.5;
  const sx = (v: number) => L + ((v - lo) / (hiX - lo)) * (W - L - R);
  const sy = (v: number) => H - B - ((v - lo) / (hiY - lo)) * (H - B - T);
  const ticks = (hi: number) =>
    log
      ? durationTicks.filter((d) => t(d) <= hi + 1e-9).map(t)
      : Array.from({ length: Math.floor(hi) + 1 }, (_, i) => i).filter(
          (i, _, all) => all.length <= 8 || i % Math.ceil(all.length / 6) === 0,
        );
  // Tasks sharing one integer cell spread on a small spiral (in pixels).
  const seen = new Map<string, number>();
  const placed = points.map((p) => {
    const cell = jitter ? `${p.x}:${p.y}` : "";
    const k = jitter ? (seen.get(cell) ?? 0) : 0;
    if (jitter) seen.set(cell, k + 1);
    const r = k ? 5 * Math.sqrt(k) : 0;
    return {
      p,
      cx: sx(t(p.x)) + r * Math.cos(k * 2.4),
      cy: sy(t(p.y)) + r * Math.sin(k * 2.4),
    };
  });
  const key = (e: KeyboardEvent, id: number) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onOpen(id);
    }
  };
  const mid = (T + H - B) / 2;
  return (
    <figure className={styles.chart} aria-label={title}>
      <figcaption className={styles.head}>
        <h3 className={styles.title}>{title}</h3>
        <p className={styles.subtitle}>{subtitle}</p>
      </figcaption>
      <Legend groups={groups} />
      <svg
        viewBox={`0 0 ${W} ${H}`}
        className={styles.scatter}
        role="group"
        aria-label={`${title}: ${points.length} tasks`}
      >
        {ticks(hiX).map((v) => (
          <g key={"x" + v}>
            <line x1={sx(v)} x2={sx(v)} y1={T} y2={H - B} className={styles.grid} />
            <text x={sx(v)} y={H - B + 16} className={styles.tick} fontSize={11} textAnchor="middle">
              {log ? format(Math.round(Math.pow(10, v))) : v}
            </text>
          </g>
        ))}
        {ticks(hiY).map((v) => (
          <g key={"y" + v}>
            <line x1={L} x2={W - R} y1={sy(v)} y2={sy(v)} className={styles.grid} />
            <text x={L - 8} y={sy(v) + 4} className={styles.tick} fontSize={11} textAnchor="end">
              {log ? format(Math.round(Math.pow(10, v))) : v}
            </text>
          </g>
        ))}
        {diagonal && (
          <g>
            <line
              x1={sx(lo)}
              y1={sy(lo)}
              x2={sx(hiX)}
              y2={sy(hiY)}
              className={styles.diagonal}
            />
            <text x={sx(hiX) - 4} y={sy(hiY) + 14} className={styles.tick} fontSize={11} textAnchor="end">
              {diagonal}
            </text>
          </g>
        )}
        <text x={(L + W - R) / 2} y={H - 2} className={styles.axis} fontSize={11} textAnchor="middle">
          {xLabel}
        </text>
        <text
          x={12}
          y={mid}
          className={styles.axis}
          fontSize={11}
          textAnchor="middle"
          transform={`rotate(-90 12 ${mid})`}
        >
          {yLabel}
        </text>
        {placed.map(({ p, cx: x, cy: y }) => (
          <circle
            key={p.id}
            cx={x}
            cy={y}
            r="5"
            tabIndex={0}
            role="button"
            aria-label={`#${p.id} ${p.label}: ${xLabel} ${format(p.x)}, ${yLabel} ${format(p.y)}`}
            className={cx(styles.dot, series(p.group))}
            onClick={() => onOpen(p.id)}
            onKeyDown={(e) => key(e, p.id)}
          >
            <title>{`#${p.id} ${p.label} · ${xLabel}: ${format(p.x)} · ${yLabel}: ${format(p.y)}`}</title>
          </circle>
        ))}
      </svg>
      {note && <p className={styles.note}>{note}</p>}
    </figure>
  );
}

const color = (i: number) => styles[`c${i % 6}`];
const day = (n: number) => new Date(n).toISOString().slice(5, 10);
/**
 * Mean per completion bucket with a shaded +/-1 SD band (lower bound clipped
 * at 0, or at one minute on a log axis). Empty buckets are gaps: no marker,
 * no band, and only a faint dashed bridge between known means. When split,
 * every group draws its mean line but only the highlighted group (legend
 * button, hover or keyboard focus) draws its band, so bands never overlap.
 * Arrow keys move the readout between buckets (left/right) and groups
 * (up/down).
 */
export function TrendChart({
  title,
  subtitle,
  groups,
  series,
  weekly,
  yLabel,
  xLabel = "completion",
  log = false,
  format,
  tick = format,
  note,
  total = false,
  gaps,
}: {
  title: string;
  subtitle: string;
  groups: string[];
  /** `[group][bucket]` */
  series: TrendPoint[][];
  weekly: boolean;
  yLabel: string;
  /** What each bucket's date is: "UTC <xLabel> date". */
  xLabel?: string;
  log?: boolean;
  format: (n: number) => string;
  tick?: (n: number) => string;
  note?: string;
  /** Points are per-bucket totals, not means: no SD wording. */
  total?: boolean;
  /** Per bucket, why its figure is incomplete; shades that column. */
  gaps?: (string | null)[];
}) {
  const [active, setActive] = useState(0);
  const [focus, setFocus] = useState<{ g: number; i: number } | null>(null);
  const g0 = Math.min(active, groups.length - 1);
  const nb = series[0]?.length ?? 0;
  // The viewBox follows the rendered width so text stays at its real size.
  const box = useRef<HTMLDivElement>(null);
  const [W, setW] = useState(420);
  useEffect(() => {
    const el = box.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(([e]) =>
      setW(Math.max(280, Math.round(e.contentRect.width))),
    );
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const H = 250,
    L = 52,
    B = 50,
    T = 10,
    R = 12;
  const highs = series.flat().flatMap((p) => (p.high === null ? [] : [p.high]));
  const known = highs.length > 0;
  const t = (v: number) => (log ? Math.log10(Math.max(v, MIN)) : v);
  const top = Math.max(0, ...highs);
  const hi = log
    ? t(durationTicks.find((d) => d >= top * 1.1) ?? durationTicks.at(-1)!)
    : top <= 5
      ? Math.max(1, Math.ceil(top))
      : niceMax(top);
  const lo = log ? t(MIN) : 0;
  const ticks = log
    ? durationTicks.filter((d) => t(d) <= hi + 1e-9)
    : [0, hi / 2, hi];
  const step = (W - L - R) / Math.max(1, nb);
  const x = (i: number) => L + (i + 0.5) * step;
  const y = (v: number) =>
    H - B - ((Math.min(t(v), hi) - lo) / (hi - lo || 1)) * (H - B - T);
  // Contiguous runs of buckets with samples; a gap splits lines and bands.
  const runs = (pts: TrendPoint[]) => {
    const out: number[][] = [];
    pts.forEach((p, i) => {
      if (!p.n) return;
      const last = out.at(-1);
      if (last && last.at(-1) === i - 1) last.push(i);
      else out.push([i]);
    });
    return out;
  };
  const labelEvery = Math.max(1, Math.ceil(nb / Math.floor((W - L - R) / 46)));
  // Sample counts: digits when a bucket is wide enough, else a bar strip.
  const digits = step >= 18;
  const period = (p: TrendPoint) =>
    weekly ? `${day(p.start)} to ${day(p.end - 1)}` : day(p.start);
  const figures = (p: TrendPoint) =>
    total
      ? ` · ${format(p.mean!)}`
      : ` · mean ${format(p.mean!)} · SD ${format(p.sd!)} · band ${format(p.low!)} to ${format(p.high!)}`;
  const describe = (g: number, p: TrendPoint, i: number) =>
    `${groups[g]} · ${period(p)} · n ${p.n}${p.unknown ? ` (${p.unknown} unknown)` : ""}` +
    (p.mean === null ? " · no known samples" : figures(p)) +
    (gaps?.[i] ? ` · ${gaps[i]}` : "");
  const move = (e: KeyboardEvent) => {
    const cur = focus ?? { g: g0, i: nb };
    let { g, i } = cur;
    const has = (gg: number, ii: number) => (series[gg]?.[ii]?.n ?? 0) > 0;
    if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      const d = e.key === "ArrowRight" ? 1 : -1;
      let j = i + d;
      while (j >= 0 && j < nb && !has(g, j)) j += d;
      if (j >= 0 && j < nb) i = j;
    } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
      g = (g + (e.key === "ArrowDown" ? 1 : groups.length - 1)) % groups.length;
      if (i >= nb) i = nb - 1;
    } else if (e.key === "Home" || e.key === "End") {
      const idx = series[g].map((p, k) => (p.n ? k : -1)).filter((k) => k >= 0);
      if (idx.length) i = e.key === "Home" ? idx[0] : idx.at(-1)!;
    } else return;
    e.preventDefault();
    if (i >= nb) i = nb - 1;
    setFocus({ g, i });
    setActive(g);
  };
  const onFocus = () => {
    if (focus) return;
    const last = series[g0]?.map((p) => p.n).findLastIndex((n) => n > 0) ?? -1;
    if (last >= 0) setFocus({ g: g0, i: last });
  };
  const order = groups.map((_, g) => g).filter((g) => g !== g0).concat(g0);
  const mid = (T + H - B) / 2;
  const totals = Array.from({ length: nb }, (_, i) =>
    series.reduce((n, s) => n + s[i].n, 0),
  );
  const fp = focus && series[focus.g]?.[focus.i];
  return (
    <figure className={styles.chart} aria-label={title}>
      <figcaption className={styles.head}>
        <h3 className={styles.title}>{title}</h3>
        <p className={styles.subtitle}>{subtitle}</p>
      </figcaption>
      {groups.length > 1 && (
        <ul className={styles.legend} aria-label="Legend: highlight a group">
          {groups.map((label, g) => (
            <li key={label}>
              <button
                type="button"
                className={styles.legendButton}
                aria-pressed={g === g0}
                onClick={() => setActive(g)}
                onMouseEnter={() => setActive(g)}
              >
                <svg aria-hidden="true" viewBox="0 0 10 10" className={styles.swatch}>
                  <rect width="10" height="10" rx="2" className={styles[`s${g % 6}`]} />
                </svg>
                {label}
              </button>
            </li>
          ))}
        </ul>
      )}
      <div ref={box}>
      <svg
        viewBox={`0 0 ${W} ${H}`}
        className={styles.trend}
        tabIndex={0}
        role="group"
        aria-label={`${title}: ${total ? "total" : "mean with plus or minus 1 SD"} per ${weekly ? "week" : "day"}. Arrow keys read values.`}
        onKeyDown={move}
        onFocus={onFocus}
        onMouseLeave={() => setFocus(null)}
      >
        {gaps?.map((gap, i) =>
          gap ? (
            <rect
              key={"gap" + i}
              x={x(i) - step / 2}
              y={T}
              width={step}
              height={H - B - T}
              className={styles.gap}
            >
              <title>{gap}</title>
            </rect>
          ) : null,
        )}
        {ticks.map((v) => (
          <g key={"y" + v}>
            <line x1={L} x2={W - R} y1={y(v)} y2={y(v)} className={styles.grid} />
            <text x={L - 8} y={y(v) + 4} className={styles.tick} fontSize={11} textAnchor="end">
              {tick(v)}
            </text>
          </g>
        ))}
        {series[0]?.map((p, i) =>
          i % labelEvery === 0 ? (
            <text key={"x" + i} x={x(i)} y={H - B + 15} className={styles.tick} fontSize={10} textAnchor="middle">
              {day(p.start)}
            </text>
          ) : null,
        )}
        <text x={L - 8} y={H - B + 31} className={styles.tick} fontSize={10} textAnchor="end">
          {digits ? "n" : `n≤${Math.max(0, ...totals)}`}
        </text>
        {totals.map((n, i) =>
          digits ? (
            <text
              key={"n" + i}
              x={x(i)}
              y={H - B + 31}
              className={n ? styles.count : styles.tick}
              fontSize={10}
              textAnchor="middle"
            >
              {n}
            </text>
          ) : (
            <rect
              key={"n" + i}
              x={x(i) - step * 0.35}
              width={step * 0.7}
              y={H - B + 32 - (n / Math.max(1, ...totals)) * 10}
              height={(n / Math.max(1, ...totals)) * 10}
              className={styles.countBar}
            />
          ),
        )}
        <text x={(L + W - R) / 2} y={H - 3} className={styles.axis} fontSize={11} textAnchor="middle">
          {weekly ? `UTC ${xLabel} week (from range start)` : `UTC ${xLabel} date`}
        </text>
        <text
          x={12}
          y={mid}
          className={styles.axis}
          fontSize={11}
          textAnchor="middle"
          transform={`rotate(-90 12 ${mid})`}
        >
          {yLabel}
        </text>
        {!known && (
          <text x={(L + W - R) / 2} y={mid} className={styles.axis} fontSize={12} textAnchor="middle">
            No known samples in this range
          </text>
        )}
        {order.map((g) => {
          const pts = series[g];
          const on = g === g0;
          return (
            <g key={g} className={cx(color(g), !on && styles.muted)}>
              {on &&
                runs(pts).map((r) =>
                  r.length > 1 ? (
                    <path
                      key={"b" + r[0]}
                      className={styles.band}
                      d={
                        "M" +
                        r.map((i) => `${x(i)},${y(pts[i].high!)}`).join("L") +
                        "L" +
                        r
                          .slice()
                          .reverse()
                          .map((i) => `${x(i)},${y(pts[i].low!)}`)
                          .join("L") +
                        "Z"
                      }
                    />
                  ) : (
                    <rect
                      key={"b" + r[0]}
                      className={styles.band}
                      x={x(r[0]) - Math.min(6, step / 3)}
                      width={Math.min(12, (step * 2) / 3)}
                      y={y(pts[r[0]].high!)}
                      height={Math.max(0, y(pts[r[0]].low!) - y(pts[r[0]].high!))}
                    />
                  ),
                )}
              {runs(pts).length > 1 && (
                // Dashed bridge across empty buckets; the gap keeps no marker.
                <path
                  className={styles.bridge}
                  d={
                    "M" +
                    runs(pts)
                      .flat()
                      .map((i) => `${x(i)},${y(pts[i].mean!)}`)
                      .join("L")
                  }
                />
              )}
              {runs(pts).map((r) =>
                r.length > 1 ? (
                  <path
                    key={"l" + r[0]}
                    className={styles.line}
                    d={"M" + r.map((i) => `${x(i)},${y(pts[i].mean!)}`).join("L")}
                  />
                ) : null,
              )}
              {pts.map((p, i) =>
                p.n ? (
                  <circle
                    key={"p" + i}
                    cx={x(i)}
                    cy={y(p.mean!)}
                    r={on ? 3.5 : 2.5}
                    className={styles.marker}
                    onMouseEnter={() => {
                      setActive(g);
                      setFocus({ g, i });
                    }}
                  >
                    <title>{describe(g, p, i)}</title>
                  </circle>
                ) : null,
              )}
            </g>
          );
        })}
        {fp && (
          <g className={color(focus!.g)}>
            <line
              x1={x(focus!.i)}
              x2={x(focus!.i)}
              y1={T}
              y2={H - B}
              className={styles.cursor}
            />
            {fp.n > 0 && (
              <circle cx={x(focus!.i)} cy={y(fp.mean!)} r={6} className={styles.focusRing} />
            )}
          </g>
        )}
      </svg>
      </div>
      <p className={styles.readout} aria-live="polite">
        {fp
          ? describe(focus!.g, fp, focus!.i)
          : "Hover a point, or focus the chart and use arrow keys, for exact values."}
      </p>
      {note && <p className={styles.note}>{note}</p>}
      <ul className={styles.data}>
        {series.flatMap((pts, g) => pts.map((p, i) => <li key={`${g}-${p.start}`}>{describe(g, p, i)}</li>))}
      </ul>
    </figure>
  );
}
