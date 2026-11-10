import { test, expect, type Page } from "@playwright/test";
import { choose, mockDesk } from "./fixtures";

const row = (key: string, cost: number, runs = 2, unknown_runs = 0) => ({
  key,
  cost,
  runs,
  known_runs: runs - unknown_runs,
  unknown_runs,
  input: 10,
  output: 20,
  thinking: 5,
  cache_read: 300,
  cache_write: 40,
});
const all = [row("alpha", 7.5, 3), row("a-very-long-project-name-for-phones", 2.5, 2, 1)];
const stats = {
  since: 0,
  until: 1,
  total: { ...row("", 10, 5, 1) },
  token_types: [
    { key: "input", tokens: 1000 },
    { key: "cache read", tokens: 3000 },
  ],
  breakdowns: Object.fromEntries(
    [
      "project", "source", "bot", "routine", "workload", "wake", "agent",
      "model", "mode", "cache", "context", "outcome", "day", "week",
    ].map((k) => [k, all]),
  ),
  coverage: {
    runs: 5,
    known: 4,
    unknown: 1,
    unknown_reasons: { no_result: 1 },
    by_agent: all,
  },
  finished: { tasks: 2, cost: 8, cost_per_task: 4 },
  filter_options: {
    project: ["alpha", "a-very-long-project-name-for-phones"],
    source: ["bot", "one-off task", "routine"],
    bot: ["axe"],
    routine: ["nightly"],
    agent: ["claude"],
    model: ["opus", "unknown"],
    workload: ["bot", "feature"],
  },
  series: {
    step_days: 1,
    first_known: 1793491200,
    buckets: Array.from({ length: 7 }, (_, i) => {
      const start = 1793404800 + i * 86400;
      const end = start + 86400;
      if (i === 0)
        return { start, end, ...row("", 0, 0), input: 0, output: 0, cache_read: 0, cache_write: 0, thinking: 0, splits: {} };
      if (i === 2)
        return { start, end, ...row("", 0, 1, 1), input: 0, output: 0, cache_read: 0, cache_write: 0, thinking: 0, splits: {} };
      return {
        start,
        end,
        ...row("", 2, 2, i === 3 ? 1 : 0),
        splits: {
          project: { alpha: 1.5, "a-very-long-project-name-for-phones": 0.5 },
          source: { bot: 0.5, "one-off task": 1.5 },
          model: { "claude-opus-5-5": 2 },
          wake: { heartbeat: 2 },
        },
      };
    }),
  },
};

async function mockCost(page: Page) {
  const requests: string[] = [];
  await mockDesk(page);
  await page.route("http://taskq.test/api/stats/cost*", async (route) => {
    requests.push(route.request().url());
    await route.fulfill({ json: stats });
  });
  return requests;
}

test("shows every breakdown from one request and labels the estimate", async ({
  page,
}) => {
  const requests = await mockCost(page);
  await page.goto("/?taskq=http://taskq.test&view=cost");
  await expect(page.getByRole("heading", { name: "Cost", exact: true })).toBeVisible();
  await expect(page.getByText("API-price estimate, not a bill")).toBeVisible();
  const figures = page.getByRole("region", { name: "Key figures" });
  await expect(figures).toContainText("$10.00");
  await expect(figures).toContainText("4 / 5");
  await expect(figures).toContainText("$4.00");
  for (const name of [
    "Project",
    "Bot wake reason",
    "Model",
    "Token type",
    "Context size at run end",
    "Task outcome",
    "Week starting Monday (UTC)",
  ])
    await expect(page.getByRole("region", { name, exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "Unknown runs" })).toContainText(
    "no result event",
  );
  expect(requests).toHaveLength(1);
  await page.getByRole("button", { name: "Tasks", exact: true }).click();
  await page.getByRole("button", { name: "Cost", exact: true }).click();
  await expect(figures).toContainText("$10.00");
  expect(requests).toHaveLength(2);
});

test("draws time series from the same single request", async ({ page }) => {
  const requests = await mockCost(page);
  await page.goto("/?taskq=http://taskq.test&view=cost");
  const over = page.getByRole("region", { name: "Over time" });
  for (const name of [
    "Estimated cost",
    "Total tokens",
    "Cost by project",
    "Cost by bot, routine or one-off task",
    "Cost by model",
    "Tokens by type",
  ])
    await expect(over.getByRole("figure", { name, exact: true })).toBeVisible();
  await expect(over.getByText("Estimated at API prices").first()).toBeVisible();
  expect(requests).toHaveLength(1);
  expect(requests[0]).toContain("step_days=1");
});

test("shades incomplete periods and plots no zero for them", async ({ page }) => {
  await mockCost(page);
  await page.goto("/?taskq=http://taskq.test&view=cost");
  const cost = page.getByRole("figure", { name: "Estimated cost", exact: true });
  await expect(cost).toContainText("before usage data begins");
  await expect(cost).toContainText("all 1 runs unknown");
  await expect(cost).toContainText("1 of 2 runs unknown");
  // Seven buckets, two with no known usage: five points and three shaded columns.
  await expect(cost.locator("circle")).toHaveCount(5);
  await expect(cost.locator('rect[class*="gap"]')).toHaveCount(3);
  const project = page.getByRole("figure", { name: "Cost by project", exact: true });
  await expect(project.locator("rect[data-gap]")).toHaveCount(3);
});

test("uses weekly buckets past 31 days", async ({ page }) => {
  const requests = await mockCost(page);
  await page.goto("/?taskq=http://taskq.test&view=cost&days=90");
  await expect(page.getByText("4 / 5")).toBeVisible();
  expect(requests.at(-1)).toContain("step_days=7");
});

test("still shows the tables when the server has no series", async ({ page }) => {
  await mockDesk(page);
  await page.route("http://taskq.test/api/stats/cost*", (route) =>
    route.fulfill({ json: { ...stats, series: undefined } }),
  );
  await page.goto("/?taskq=http://taskq.test&view=cost");
  await expect(page.getByText("4 / 5")).toBeVisible();
  await expect(page.getByRole("region", { name: "Over time" })).toHaveCount(0);
});

test("changing the period makes one more request", async ({ page }) => {
  const requests = await mockCost(page);
  await page.goto("/?taskq=http://taskq.test&view=cost");
  await expect(page.getByText("4 / 5")).toBeVisible();
  await choose(page.getByLabel("Date range"), "30");
  await expect.poll(() => requests.length).toBe(2);
  expect(new URL(page.url()).searchParams.get("days")).toBe("30");
});

test("each filter and breakdown is one request kept in the URL", async ({ page }) => {
  const requests = await mockCost(page);
  await page.goto("/?taskq=http://taskq.test&view=cost");
  await expect(page.getByText("4 / 5")).toBeVisible();
  expect(requests).toHaveLength(1);
  for (const [label, value, param, api] of [
    ["project", "alpha", "project", "project"],
    ["source", "bot", "source", "source"],
    ["bot", "axe", "cost_bot", "bot"],
    ["routine", "nightly", "cost_routine", "routine"],
    ["agent", "claude", "agent", "agent"],
    ["model", "opus", "model", "model"],
    ["workload", "feature", "workload", "workload"],
  ] as const) {
    const before = requests.length;
    await choose(page.getByLabel(`Filter by ${label}`), value);
    await expect.poll(() => requests.length).toBe(before + 1);
    expect(new URL(page.url()).searchParams.get(param)).toBe(value);
    expect(new URL(requests.at(-1)!).searchParams.get(api)).toBe(value);
    await expect(page.getByText("4 / 5")).toBeVisible();
  }
  const filtered = requests.length;
  await choose(page.getByLabel("Stack cost chart by"), "wake");
  await expect.poll(() => requests.length).toBe(filtered + 1);
  expect(new URL(page.url()).searchParams.get("breakdown")).toBe("wake");
  const last = new URL(requests.at(-1)!).searchParams;
  expect(last.get("breakdown")).toBe("wake");
  expect(last.get("project")).toBe("alpha");
  await expect(
    page.getByRole("figure", { name: "Cost by bot wake reason", exact: true }),
  ).toBeVisible();
  await expect(page.getByRole("figure", { name: "Cost by project", exact: true })).toHaveCount(0);
  await page.reload();
  await expect(page.getByLabel("Filter by bot")).toHaveAttribute("data-value", "axe");
  await expect(page.getByLabel("Stack cost chart by")).toHaveAttribute("data-value", "wake");
  await page.getByRole("button", { name: "Routines", exact: true }).click();
  await expect(page.getByRole("button", { name: "Routines", exact: true })).toHaveAttribute(
    "aria-current",
    "page",
  );
  expect(new URL(page.url()).searchParams.get("routine")).toBeNull();
});

test("says when filters match nothing", async ({ page }) => {
  await mockDesk(page);
  await page.route("http://taskq.test/api/stats/cost*", (route) =>
    route.fulfill({
      json: { ...stats, coverage: { ...stats.coverage, runs: 0 } },
    }),
  );
  await page.goto("/?taskq=http://taskq.test&view=cost&project=gone");
  await expect(page.getByText("No finished runs match these filters.")).toBeVisible();
  await expect(page.getByLabel("Filter by project")).toHaveAttribute("data-value", "gone");
});

test("reports a failed load", async ({ page }) => {
  await mockDesk(page);
  await page.route("http://taskq.test/api/stats/cost*", (route) =>
    route.fulfill({ status: 500, body: "no" }),
  );
  await page.goto("/?taskq=http://taskq.test&view=cost");
  await expect(page.getByText("Could not load cost data")).toBeVisible();
});

test("fits a phone without horizontal page overflow", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 800 });
  await mockCost(page);
  await page.goto(
    "/?taskq=http://taskq.test&view=cost&project=a-very-long-project-name-for-phones&model=claude-opus-5-5&breakdown=context",
  );
  await expect(page.getByText("4 / 5")).toBeVisible();
  await expect(page.getByLabel("Stack cost chart by")).toBeVisible();
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - window.innerWidth,
  );
  expect(overflow).toBeLessThanOrEqual(0);
});
