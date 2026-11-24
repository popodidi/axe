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
