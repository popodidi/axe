"use client";
import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { niceMax, segments, stamp, timeLabel, timeTicks } from "../../lib/node";
import { cx } from "../ui";
import chart from "./stats-charts.module.css";
import styles from "./node.module.css";

export type TimeSeries = {
  label: string;
  /** Series slot: `stats-charts.module.css` s0-s5 / c0-c5. */
  slot: number;
  /** Per point; null is unknown (a gap, never zero). */
  values: (number | null)[];
  /** Per point peak (max) over the point's span; drawn as a band above the line. */
  peaks?: (number | null)[];
};

/**
 * Time-series chart for the Node view: one line per series with an optional
 * avg-to-peak band, or stacked areas (`stacked`). Missing steps split the
 * marks. A crosshair snaps to the nearest point on hover; arrow keys move it
 * when the chart has focus; the tooltip lists every series at that time.
 * Exact values are also in a screen-reader list.
 */
export function TimeChart({
  title,
  subtitle,
  points,
  step,
  from,
  to,
  series,
  stacked = false,
  yMax,
  reference,
  format,
  tick = format,
  yLabel,
}: {
  title: string;
  subtitle: string;
  points: { t: number }[];
  step: number;
  from: number;
  to: number;
  series: TimeSeries[];
  stacked?: boolean;
  /** A fixed axis maximum (100 for percent); otherwise from the data. */
  yMax?: number;
  /** A dashed reference line (memory total). */
  reference?: { value: number; label: string };
  format: (n: number) => string;
  tick?: (n: number) => string;
  yLabel: string;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [W, setW] = useState(560);
  const [focus, setFocus] = useState<number | null>(null);
  useEffect(() => {
    const el = box.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(([e]) => setW(Math.max(280, Math.round(e.contentRect.width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const H = 220,
    L = 52,
    // Room for direct labels only where the plot stays wide; the legend
    // names every series either way.
    direct = series.length > 1 && W >= 520,
    R = direct ? 104 : 16,
    T = 12,
    B = 26;
  const n = points.length;
  // Cumulative tops when stacked.
  const tops = series.map((_, g) =>
    points.map((_, i) =>
      stacked
        ? series.slice(0, g + 1).reduce((a, s) => a + (s.values[i] ?? 0), 0)
        : series[g].values[i],
    ),
  );
  const highs = series.flatMap((s, g) =>
    (s.peaks ?? tops[g]).filter((v): v is number => v !== null),
  );
  const top = yMax ?? niceMax(Math.max(reference?.value ?? 0, ...highs) * (stacked ? 1 : 1.05));
  const x = (t: number) => L + ((t + step / 2 - from) / Math.max(1, to - from)) * (W - L - R);
  const y = (v: number) => H - B - (Math.min(v, top) / top) * (H - B - T);
  const runs = segments(points, step);
  const yTicks = [0, top / 2, top];
  const span = to - from;
  const nearest = (px: number) => {
    let best = -1,
      d = Infinity;
    points.forEach((p, i) => {
      const e = Math.abs(x(p.t) - px);
      if (e < d) {
        d = e;
        best = i;
      }
    });
    return best >= 0 ? best : null;
  };
  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    setFocus(nearest(((e.clientX - r.left) / r.width) * W));
  };
  const onKey = (e: KeyboardEvent) => {
    if (!n) return;
    const cur = focus ?? n - 1;
    const next =
      e.key === "ArrowLeft"
        ? Math.max(0, cur - 1)
        : e.key === "ArrowRight"
          ? Math.min(n - 1, cur + 1)
          : e.key === "Home"
            ? 0
            : e.key === "End"
              ? n - 1
              : null;
    if (next === null) return;
    e.preventDefault();
    setFocus(next);
  };
  const value = (s: TimeSeries, i: number) => {
    const v = s.values[i];
    if (v === null) return "no data";
    const peak = s.peaks?.[i];
    return peak !== null && peak !== undefined && peak !== v
      ? `${format(v)} avg, ${format(peak)} peak`
      : format(v);
  };
  const describe = (i: number) =>
    `${stamp(points[i].t)} · ` + series.map((s) => `${s.label} ${value(s, i)}`).join(" · ");
  // Direct labels at the right edge: each series at its last known value
  // (mid-layer when stacked), skipped when it would collide.
  const labels: { g: number; y: number }[] = [];
  if (direct && n) {
    const last = n - 1;
    series.forEach((s, g) => {
      const v = tops[g][last];
      if (v === null) return;
      const lo = stacked && g > 0 ? (tops[g - 1][last] ?? 0) : null;
      if (stacked && (s.values[last] ?? 0) <= 0) return;
      const at = lo === null ? y(v) : (y(v) + y(lo)) / 2;
      if (labels.every((l) => Math.abs(l.y - at) >= 13)) labels.push({ g, y: at });
    });
  }
  const tip = focus !== null && points[focus] ? focus : null;
  const tipLeft = tip !== null ? (x(points[tip].t) / W) * 100 : 0;
  // The tooltip follows the crosshair (a computed position, so set directly).
  const tipRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (tipRef.current) tipRef.current.style.left = `${tipLeft}%`;
  }, [tipLeft, tip]);
  return (
    <figure className={chart.chart} aria-label={title}>
      <figcaption className={chart.head}>
        <h3 className={chart.title}>{title}</h3>
        <p className={chart.subtitle}>{subtitle}</p>
      </figcaption>
      {series.length > 1 && (
        <ul className={chart.legend} aria-label="Legend">
          {series.map((s) => (
            <li key={s.label}>
              <svg aria-hidden="true" viewBox="0 0 10 10" className={chart.swatch}>
                <rect width="10" height="10" rx="2" className={chart[`s${s.slot}`]} />
              </svg>
              {s.label}
            </li>
          ))}
        </ul>
      )}
      <div ref={box} className={styles.plot}>
        <svg
          viewBox={`0 0 ${W} ${H}`}
          className={chart.trend}
          tabIndex={0}
          role="group"
          aria-label={`${title}. Arrow keys read values.`}
          onPointerMove={onMove}
          onPointerLeave={() => setFocus(null)}
          onKeyDown={onKey}
          onBlur={() => setFocus(null)}
        >
          {yTicks.map((v) => (
            <g key={"y" + v}>
              <line x1={L} x2={W - R} y1={y(v)} y2={y(v)} className={chart.grid} />
              <text x={L - 8} y={y(v) + 4} className={chart.tick} fontSize={11} textAnchor="end">
                {tick(v)}
              </text>
            </g>
          ))}
          {timeTicks(from, to).map((t) => (
            <text
              key={"x" + t}
              x={L + ((t - from) / Math.max(1, span)) * (W - L - R)}
              y={H - B + 16}
              className={chart.tick}
              fontSize={10}
              textAnchor="middle"
            >
              {timeLabel(t, span)}
            </text>
          ))}
          <text
            x={12}
            y={(T + H - B) / 2}
            className={chart.axis}
            fontSize={11}
            textAnchor="middle"
            transform={`rotate(-90 12 ${(T + H - B) / 2})`}
          >
            {yLabel}
          </text>
          {!n && (
            <text x={(L + W - R) / 2} y={(T + H - B) / 2} className={chart.axis} fontSize={12} textAnchor="middle">
              No samples in this range
            </text>
          )}
          {reference && (
            <g>
              <line
                x1={L}
                x2={W - R}
                y1={y(reference.value)}
                y2={y(reference.value)}
                className={styles.reference}
              />
              <text x={L + 4} y={y(reference.value) - 4} className={chart.tick} fontSize={10}>
                {reference.label}
              </text>
            </g>
          )}
          {series.map((s, g) => (
            <g key={s.label} className={chart[`c${s.slot}`]}>
              {runs.map((r) => {
                const at = (i: number) => x(points[i].t);
                const known = r.filter((i) => tops[g][i] !== null);
                if (!known.length) return null;
                if (stacked) {
                  const lo = (i: number) => (g > 0 ? (tops[g - 1][i] ?? 0) : 0);
                  const d =
                    "M" +
                    known.map((i) => `${at(i)},${y(tops[g][i]!)}`).join("L") +
                    "L" +
                    known
                      .slice()
                      .reverse()
                      .map((i) => `${at(i)},${y(lo(i))}`)
                      .join("L") +
                    "Z";
                  return known.length > 1 ? (
                    <path key={r[0]} d={d} className={styles.layer} />
                  ) : (
                    <rect
                      key={r[0]}
                      x={at(known[0]) - 2}
                      width={4}
                      y={y(tops[g][known[0]]!)}
                      height={Math.max(0, y(lo(known[0])) - y(tops[g][known[0]]!))}
                      className={styles.layer}
                    />
                  );
                }
                const peaks = s.peaks;
                return (
                  <g key={r[0]}>
                    {peaks && known.length > 1 && (
                      <path
                        className={chart.band}
                        d={
                          "M" +
                          known.map((i) => `${at(i)},${y(peaks[i] ?? tops[g][i]!)}`).join("L") +
                          "L" +
                          known
                            .slice()
                            .reverse()
                            .map((i) => `${at(i)},${y(tops[g][i]!)}`)
                            .join("L") +
                          "Z"
                        }
                      />
                    )}
                    {known.length > 1 ? (
                      <path
                        className={chart.line}
                        d={"M" + known.map((i) => `${at(i)},${y(tops[g][i]!)}`).join("L")}
                      />
                    ) : (
                      <circle cx={at(known[0])} cy={y(tops[g][known[0]]!)} r={4} className={chart.marker} />
                    )}
                  </g>
                );
              })}
            </g>
          ))}
          {labels.map((l) => (
            <g key={"l" + l.g}>
              <line
                x1={W - R + 4}
                x2={W - R + 12}
                y1={l.y}
                y2={l.y}
                className={cx(chart[`c${series[l.g].slot}`], styles.labelKey)}
              />
              <text x={W - R + 16} y={l.y + 4} className={styles.directLabel} fontSize={11}>
                {series[l.g].label}
              </text>
            </g>
          ))}
          {tip !== null && (
            <line x1={x(points[tip].t)} x2={x(points[tip].t)} y1={T} y2={H - B} className={chart.cursor} />
          )}
        </svg>
        {tip !== null && (
          <div
            ref={tipRef}
            className={cx(styles.tooltip, tipLeft > 60 && styles.tooltipLeft)}
            aria-hidden="true"
          >
            <div className={styles.tipTime}>{stamp(points[tip].t)}</div>
            {series
              .slice()
              .reverse()
              .map((s) => (
                <div key={s.label} className={styles.tipRow}>
                  <span className={cx(styles.tipKey, chart[`c${s.slot}`])} />
                  <strong>{value(s, tip)}</strong>
                  <span className={styles.tipLabel}>{s.label}</span>
                </div>
              ))}
          </div>
        )}
      </div>
      <p className={chart.readout} aria-live="polite">
        {tip !== null ? describe(tip) : "Hover the chart, or focus it and use arrow keys, for exact values."}
      </p>
      <ul className={chart.data}>
        {points.map((p, i) => (
          <li key={p.t}>{describe(i)}</li>
        ))}
      </ul>
    </figure>
  );
}
