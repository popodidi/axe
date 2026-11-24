import { describe, expect, it } from "vitest";
import { clusterIds, clusterPoints, clusterRadius, majority } from "./scatter-cluster";

describe("clusterPoints", () => {
  it("keeps isolated dots single and groups overlapping ones", () => {
    const c = clusterPoints(
      [
        { x: 0, y: 0, item: 1 },
        { x: 3, y: 4, item: 2 },
        { x: 100, y: 100, item: 3 },
        { x: 0, y: 0, item: 4 },
      ],
      10,
    );
    expect(c.map((k) => k.items)).toEqual([[1, 2, 4], [3]]);
    expect(c[0].x).toBe(1);
    expect(c[0].y).toBeCloseTo(4 / 3);
  });
  it("joins the nearest seed within reach", () => {
    const c = clusterPoints(
      [
        { x: 0, y: 0, item: "a" },
        { x: 20, y: 0, item: "b" },
        { x: 12, y: 0, item: "c" },
      ],
      10,
    );
    expect(c.map((k) => k.items)).toEqual([["a"], ["b", "c"]]);
  });
  it("collapses hundreds of identical points into one cluster", () => {
    const pts = Array.from({ length: 669 }, (_, i) => ({ x: 5, y: 5, item: i }));
    const c = clusterPoints(pts, 10);
    expect(c).toHaveLength(1);
    expect(c[0].items).toHaveLength(669);
  });
});
describe("helpers", () => {
  it("sizes by count with a cap", () => {
    expect(clusterRadius(1)).toBe(5);
    expect(clusterRadius(2)).toBe(9);
    expect(clusterRadius(10000)).toBe(18);
  });
  it("picks the majority group", () => {
    expect(majority([2, 1, 2])).toBe(2);
    expect(majority([1, 0])).toBe(0);
  });
  it("parses cluster ids", () => {
    expect(clusterIds("3,12,x,-1,0")).toEqual([3, 12]);
    expect(clusterIds(null)).toEqual([]);
  });
});
describe("no overlap", () => {
  it("merges clusters whose drawn circles would overlap", () => {
    // A big cluster at 0 and a single dot 14 away: 5 + 18 > 14, so one circle.
    const pts = [
      ...Array.from({ length: 200 }, (_, i) => ({ x: 0, y: 0, item: i })),
      { x: 14, y: 0, item: 999 },
    ];
    const c = clusterPoints(pts, 10);
    expect(c).toHaveLength(1);
    expect(c[0].items).toHaveLength(201);
  });
  it("leaves dense random data with no overlapping circles", () => {
    let seed = 7;
    const rnd = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
    const pts = Array.from({ length: 1600 }, (_, i) => ({
      x: 50 + rnd() * 120 * rnd(),
      y: 200 - rnd() * 150 * rnd(),
      item: i,
    }));
    const c = clusterPoints(pts, 10);
    expect(c.reduce((n, k) => n + k.items.length, 0)).toBe(1600);
    for (let i = 0; i < c.length; i++)
      for (let j = i + 1; j < c.length; j++)
        expect(
          Math.hypot(c[i].x - c[j].x, c[i].y - c[j].y),
        ).toBeGreaterThanOrEqual(
          clusterRadius(c[i].items.length) + clusterRadius(c[j].items.length) - 1e-9,
        );
  });
});
