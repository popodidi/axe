// Scatter clustering: dots whose rendered centres fall within `reach` of a
// cluster's first dot join it, so dense areas draw one circle sized by its
// count instead of hundreds of overlapping dots. Greedy and order-stable:
// the same points always give the same clusters.
export type Placed<T> = { x: number; y: number; item: T };
export type Cluster<T> = {
  /** Mean of the members' rendered centres. */
  x: number;
  y: number;
  items: T[];
};
export function clusterPoints<T>(points: Placed<T>[], reach: number): Cluster<T>[] {
  const seeds: { x: number; y: number; members: Placed<T>[] }[] = [];
  const r2 = reach * reach;
  for (const p of points) {
    let best = -1;
    let bestD = Infinity;
    seeds.forEach((s, i) => {
      const d = (s.x - p.x) ** 2 + (s.y - p.y) ** 2;
      if (d <= r2 && d < bestD) {
        best = i;
        bestD = d;
      }
    });
    if (best >= 0) seeds[best].members.push(p);
    else seeds.push({ x: p.x, y: p.y, members: [p] });
  }
  return seeds.map((s) => ({
    x: s.members.reduce((n, m) => n + m.x, 0) / s.members.length,
    y: s.members.reduce((n, m) => n + m.y, 0) / s.members.length,
    items: s.members.map((m) => m.item),
  }));
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
