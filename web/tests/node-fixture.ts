import type { Page, Route } from "@playwright/test";
import { mockDesk } from "./fixtures";

// Synthetic node metrics: a smooth daily load with a gap (the node down for
// a while) and a few runs of each kind.
const GIB = 2 ** 30;
const steps: Record<string, [number, number, string]> = {
  "1h": [3600, 15, "raw"],
  "6h": [6 * 3600, 60, "raw"],
  "24h": [86400, 300, "rollup"],
  "7d": [7 * 86400, 1800, "rollup"],
  "30d": [30 * 86400, 7200, "rollup"],
};

export function nodeSeries(range: string, now: number) {
  const [span, step, source] = steps[range] ?? steps["6h"];
  const to = Math.floor(now / step) * step + step;
  const from = to - span - step;
  const points = [];
  for (let t = from; t < to; t += step) {
    const k = (t - from) / span;
    // The node was down for a twentieth of the range.
    if (k > 0.55 && k < 0.6) continue;
    const wave = (Math.sin(k * Math.PI * 4) + 1) / 2;
    const cpu = 12 + 50 * wave;
    const task = Math.round(1 + 2 * wave);
    const bot = k > 0.7 ? 1 : 0;
    points.push({
      t,
      n: Math.max(1, step / 15),
      cpu_avg: cpu,
      cpu_max: Math.min(100, cpu + 18),
      mem_used_avg: (5 + 3 * wave) * GIB,
      mem_used_max: (5.6 + 3 * wave) * GIB,
      mem_total: 16 * GIB,
      load1_avg: 0.4 + 2 * wave,
      load1_max: 0.6 + 2.4 * wave,
      disk_free_min: 120 * GIB,
      task_avg: task,
      task_max: task + 1,
      bot_avg: bot,
      bot_max: bot,
      service_avg: 2,
      service_max: 2,
      scheduled_avg: k > 0.3 && k < 0.33 ? 1 : 0,
      scheduled_max: k > 0.3 && k < 0.33 ? 1 : 0,
    });
  }
  return {
    range,
    source,
    step_seconds: step,
    from,
    to,
    now,
    sample_seconds: 15,
    points,
    current: {
      at: now - 7,
      cpu_percent: 23.4,
      mem_used_bytes: 6.2 * GIB,
      mem_total_bytes: 16 * GIB,
      load1: 1.2,
      load5: 0.98,
      load15: 0.75,
      disk_free_bytes: 120.4 * GIB,
      disk_total_bytes: 500 * GIB,
      disk_path: "/srv/axe/.runs",
      runs: {
        task: { harvis: 2, tiktape: 1 },
        bot: { cos: 1 },
        service: { trader: 2 },
        scheduled: {},
      },
    },
  };
}

/** The desk plus `GET /api/node/metrics`; returns the ranges requested. */
export async function mockNode(page: Page, empty = false) {
  await mockDesk(page);
  const ranges: string[] = [];
  await page.route("http://axe.test/api/node/metrics**", (route: Route) => {
    const range = new URL(route.request().url()).searchParams.get("range") || "1h";
    ranges.push(range);
    const now = Math.floor(Date.now() / 1000);
    const s = nodeSeries(range, now);
    return route.fulfill({ json: empty ? { ...s, points: [], current: null } : s });
  });
  return ranges;
}
