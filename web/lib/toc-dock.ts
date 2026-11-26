// Dock-style magnification for the collapsed contents rail
// (components/ui/toc.tsx). Ticks sit one pitch apart; the pointer's tick
// grows and the space around it opens, neighbours by a raised-cosine falloff
// over `DOCK_REACH` ticks. Lengths here are in pitches, measured from the
// top of the list, which stays put: the list grows downward and the tick
// under the pointer stays under it.

// How far the magnification reaches, in ticks either side.
export const DOCK_REACH = 3.5;
// Extra pitch at the pointer: its row is 1 + DOCK_GAIN pitches tall.
export const DOCK_GAIN = 2.5;

// The falloff at `t` pitches from the pointer: 1 there, 0 at the reach.
export function dockFalloff(t: number): number {
  const a = Math.abs(t);
  return a >= DOCK_REACH ? 0 : (1 + Math.cos((Math.PI * a) / DOCK_REACH)) / 2;
}

// The falloff's integral from 0 to `t` (odd in t).
function area(t: number): number {
  const a = Math.min(Math.abs(t), DOCK_REACH);
  return (Math.sign(t) * (a + (DOCK_REACH / Math.PI) * Math.sin((Math.PI * a) / DOCK_REACH))) / 2;
}

// Where the rest position `c` is drawn with the focus at rest position `u`
// and magnification `h` (0 rest, 1 full).
export function dockPosition(c: number, u: number, h: number): number {
  return c + DOCK_GAIN * h * (area(c - u) + area(u));
}

// The focus (rest position) whose drawn position is `v`: the inverse of
// dockPosition(u, u, h), which only grows with u.
export function dockFocus(v: number, h: number): number {
  let [lo, hi] = [v - DOCK_GAIN * h * DOCK_REACH, v];
  for (let i = 0; i < 32; i++) {
    const mid = (lo + hi) / 2;
    if (dockPosition(mid, mid, h) < v) lo = mid;
    else hi = mid;
  }
  return (lo + hi) / 2;
}

export type DockRow = { top: number; size: number; zoom: number };

// The `n` rows (top and size in pitches, zoom 0-1) with the focus at `u`.
export function dockLayout(n: number, u: number, h: number): { rows: DockRow[]; height: number } {
  const rows: DockRow[] = [];
  for (let i = 0; i < n; i++) {
    const top = dockPosition(i, u, h);
    rows.push({ top, size: dockPosition(i + 1, u, h) - top, zoom: h * dockFalloff(i + 0.5 - u) });
  }
  return { rows, height: dockPosition(n, u, h) };
}
