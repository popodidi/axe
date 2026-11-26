import { describe, expect, it } from "vitest";
import { DOCK_GAIN, DOCK_REACH, dockFalloff, dockFocus, dockLayout, dockPosition } from "./toc-dock";

describe("contents rail dock", () => {
  it("falls off smoothly from the pointer to the reach", () => {
    expect(dockFalloff(0)).toBe(1);
    expect(dockFalloff(DOCK_REACH)).toBe(0);
    expect(dockFalloff(-DOCK_REACH - 1)).toBe(0);
    expect(dockFalloff(1)).toBeCloseTo(dockFalloff(-1));
    expect(dockFalloff(1)).toBeGreaterThan(dockFalloff(2));
    expect(dockFalloff(2)).toBeGreaterThan(dockFalloff(3));
  });

  it("rests at one pitch per tick", () => {
    const { rows, height } = dockLayout(4, 1.5, 0);
    expect(rows).toEqual([0, 1, 2, 3].map((i) => ({ top: i, size: 1, zoom: 0 })));
    expect(height).toBe(4);
  });

  it("grows the pointer's tick most, its neighbours by distance, rows touching", () => {
    const { rows, height } = dockLayout(12, 5.5, 1);
    expect(rows[0].top).toBe(0);
    expect(rows[5].size).toBeCloseTo(1 + DOCK_GAIN * 0.97, 1);
    expect(rows[5].zoom).toBe(1);
    expect(rows[4].size).toBeLessThan(rows[5].size);
    expect(rows[4].size).toBeCloseTo(rows[6].size);
    expect(rows[3].size).toBeLessThan(rows[4].size);
    expect(rows[0].size).toBe(1);
    expect(rows[11].size).toBe(1);
    for (let i = 1; i < rows.length; i++) expect(rows[i].top).toBeCloseTo(rows[i - 1].top + rows[i - 1].size);
    expect(height).toBeCloseTo(12 + DOCK_GAIN * DOCK_REACH);
  });

  it("keeps the tick under the pointer", () => {
    for (const h of [0, 0.5, 1]) {
      for (const v of [0.2, 3, 7.7, 20]) {
        const u = dockFocus(v, h);
        expect(dockPosition(u, u, h)).toBeCloseTo(v, 6);
        const i = Math.floor(u);
        const { rows } = dockLayout(30, u, h);
        expect(rows[i].top).toBeLessThanOrEqual(v + 1e-6);
        expect(rows[i].top + rows[i].size).toBeGreaterThan(v);
      }
    }
  });
});
