// Scatter clustering: dots whose rendered centres fall within `reach` of a
// cluster's first dot join it; then any two clusters whose drawn circles
// (`radius` of their counts) would overlap merge, until none do. Dense areas
// draw one circle sized by its count instead of hundreds of overlapping dots,
// and no circle hides another's count or takes its clicks. Deterministic: the
// same points always give the same clusters.
export type Placed<T> = { x: number; y: number; item: T };
export type Cluster<T> = {
  /** Mean of the members' rendered centres. */
  x: number;
  y: number;
  items: T[];
};
export function clusterPoints<T>(
  points: Placed<T>[],
  reach: number,
  radius: (n: number) => number = clusterRadius,
): Cluster<T>[] {
  type Acc = { x: number; y: number; sx: number; sy: number; items: T[] };
  const out: Acc[] = [];
  const r2 = reach * reach;
  for (const p of points) {
    let best: Acc | null = null;
    let bestD = Infinity;
    for (const s of out) {
      const d = (s.x - p.x) ** 2 + (s.y - p.y) ** 2;
      if (d <= r2 && d < bestD) {
        best = s;
        bestD = d;
      }
    }
    if (best) {
      best.sx += p.x;
      best.sy += p.y;
      best.items.push(p.item);
    } else out.push({ x: p.x, y: p.y, sx: p.x, sy: p.y, items: [p.item] });
  }
  // Seeds stay put while grouping; from here a cluster sits at its mean.
  for (const c of out) {
    c.x = c.sx / c.items.length;
    c.y = c.sy / c.items.length;
  }
  // Merge the most overlapping pair until no circles overlap.
  for (;;) {
    let pair: [number, number] | null = null;
    let worst = 0;
    for (let i = 0; i < out.length; i++)
      for (let j = i + 1; j < out.length; j++) {
        const a = out[i],
          b = out[j];
        const overlap =
          radius(a.items.length) + radius(b.items.length) -
          Math.hypot(a.x - b.x, a.y - b.y);
        if (overlap > worst) {
          worst = overlap;
          pair = [i, j];
        }
      }
    if (!pair) break;
    const [a, b] = [out[pair[0]], out[pair[1]]];
    a.sx += b.sx;
    a.sy += b.sy;
    a.items.push(...b.items);
    a.x = a.sx / a.items.length;
    a.y = a.sy / a.items.length;
    out.splice(pair[1], 1);
  }
  return out.map(({ x, y, items }) => ({ x, y, items }));
}
/** Dot radius: 5 for one task, growing with the square root of the count. */
export const clusterRadius = (n: number) =>
  n <= 1 ? 5 : Math.min(18, 7 + 2 * Math.sqrt(n - 1));
/** The most common group in a cluster (ties: the lowest index). */
export function majority(groups: number[]): number {
  const counts = new Map<number, number>();
  for (const g of groups) counts.set(g, (counts.get(g) ?? 0) + 1);
  let best = groups[0] ?? 0;
  for (const [g, n] of counts) {
    const b = counts.get(best)!;
    if (n > b || (n === b && g < best)) best = g;
  }
  return best;
}
/** `cluster=` URL value: task ids, comma-separated. */
export const clusterIds = (value: string | null) =>
  (value || "")
    .split(",")
    .map(Number)
    .filter((n) => Number.isInteger(n) && n > 0);
