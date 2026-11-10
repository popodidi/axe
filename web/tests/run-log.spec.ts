import { test, expect, type Page } from "@playwright/test";
import { mockDesk, runs } from "./fixtures";
import type { Run } from "../lib/model";
import type { LogLine } from "../lib/run-log";
const base = "/?server=http://axe.test";
const message = (text: string, agent: string) =>
  agent === "claude"
    ? { type: "assistant", message: { content: [{ type: "text", text }] } }
    : { type: "item.completed", item: { type: "agent_message", text } };
async function fixture(page: Page, agent = "claude") {
  await mockDesk(page);
  const run: Run = { ...runs[0], agent };
  const history: Run[] = [
    run,
    { ...run, id: 12, ended_at: "2026-10-28T02:00:00Z", exit_code: 0 },
    { ...run, id: 13, ended_at: "2026-10-28T03:00:00Z", exit_code: 9 },
    { ...run, id: 14, ended_at: "2026-10-28T03:00:00Z", exit_code: 0 },
  ];
  const lines: LogLine[] = [];
  let cursor = 0;
  function append(value: unknown) {
    const text = typeof value === "string" ? value : JSON.stringify(value);
    const start = cursor;
    cursor += new TextEncoder().encode(text).length + 1;
    lines.push({ start, end: cursor, text, fragment: false });
  }
  for (let i = 0; i < 40; i++) append(message(`Initial message ${i}`, agent));
  let disconnect = false;
  const requests: string[] = [];
  await page.route("http://axe.test/api/runs?*", (route) => {
    const url = new URL(route.request().url());
    return route.fulfill({
      json: url.searchParams.has("task") ? history : [run],
    });
  });
  await page.route("http://axe.test/api/runs/*/log?*", async (route) => {
    requests.push(route.request().url());
    if (disconnect) return route.abort();
    const url = new URL(route.request().url());
    const id = Number(url.pathname.split("/")[3]);
    const r = history.find((r) => r.id === id)!;
    if (id === 14)
      return route.fulfill({
        json: { run: r, unavailable: "Log missing or unreadable" },
      });
    if (id !== 11)
      return route.fulfill({
        json: {
          run: r,
          page: {
            generation: String(id),
            start: 0,
            end: 20,
            size: 20,
            lines: [
              {
                start: 0,
                end: 20,
                text: JSON.stringify(
                  message(`Historical attempt ${id}`, agent),
                ),
                fragment: false,
              },
            ],
          },
        },
      });
    const after = url.searchParams.get("after"),
      before = url.searchParams.get("before");
    const selected =
      after !== null
        ? lines.filter((l) => l.start >= Number(after)).slice(0, 1000)
        : before !== null
          ? lines.filter((l) => l.end <= Number(before)).slice(-20)
          : lines.slice(-20);
    await route.fulfill({
      json: {
        run,
        page: {
          generation: "fixture",
          start: selected[0]?.start ?? Number(after),
          end: selected.at(-1)?.end ?? Number(after),
          size: cursor,
          lines: selected,
        },
      },
    });
  });
  return {
    append,
    run,
    requests,
    subsequentRun: () => {
      history.unshift({ ...run, id: 15 });
    },
    disconnect: (value: boolean) => {
      disconnect = value;
    },
  };
}
for (const agent of ["claude", "codex"])
  test(`${agent}: live from task Runs, pause, reconnect and completion`, async ({
    page,
  }) => {
    const f = await fixture(page, agent);
    await page.goto(base + "&task=3&tab=runs");
    await page.getByRole("button", { name: "Watch", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Run log" });
    await expect(
      dialog.getByText("Initial message 39", { exact: true }),
    ).toBeVisible();
    await expect(
      dialog.getByText("Initial message 0", { exact: true }),
    ).toHaveCount(0);
    const scroll = dialog.getByLabel("Log output");
    await scroll.evaluate((el) => {
      el.scrollTop = 300;
      el.dispatchEvent(new Event("scroll"));
    });
    const top = await scroll.evaluate((el) => el.scrollTop);
    f.append(message("Appended agent message", agent));
    await expect(
      dialog.getByText("Appended agent message", { exact: true }),
    ).toHaveCount(1);
    expect(await scroll.evaluate((el) => el.scrollTop)).toBe(top);
    await dialog.getByRole("button", { name: "Pause", exact: true }).click();
    f.append(message("Held while paused", agent));
    await page.waitForTimeout(1200);
    await expect(
      dialog.getByText("Held while paused", { exact: true }),
    ).toHaveCount(0);
    await dialog.getByRole("button", { name: "Resume", exact: true }).click();
    await expect(
      dialog.getByText("Held while paused", { exact: true }),
    ).toHaveCount(1);
    f.disconnect(true);
    await expect(dialog.getByRole("status")).toContainText("Reconnecting");
    f.append(message("After reconnect", agent));
    f.disconnect(false);
    await expect(
      dialog.getByText("After reconnect", { exact: true }),
    ).toHaveCount(1);
    await expect(
      dialog.getByText("Held while paused", { exact: true }),
    ).toHaveCount(1);
    if (agent === "claude") {
      f.append({
        type: "assistant",
        message: {
          content: [
            {
              type: "tool_use",
              id: "tool1",
              name: "Bash",
              input: { command: "echo fixture" },
            },
          ],
        },
      });
      f.append({
        type: "user",
        message: {
          content: [
            {
              type: "tool_result",
              tool_use_id: "tool1",
              content: "long output\n".repeat(250),
            },
          ],
        },
      });
    } else {
      f.append({
        type: "item.started",
        item: {
          type: "command_execution",
          id: "tool1",
          command: "echo fixture",
        },
      });
      f.append({
        type: "item.completed",
        item: {
          type: "command_execution",
          id: "tool1",
          command: "echo fixture",
          aggregated_output: "long output\n".repeat(250),
          exit_code: 0,
        },
      });
    }
    f.append("<script>window.injected=true</script>");
    await expect(dialog.locator('article[data-kind="tool"]')).toHaveCount(1);
    await dialog.getByRole("button", { name: "Jump to latest" }).click();
    await dialog.locator('article[data-kind="tool"] summary').click();
    await expect(
      dialog.locator('article[data-kind="tool"] details'),
    ).toHaveAttribute("open", "");
    expect(await page.evaluate(() => "injected" in window)).toBe(false);
    await dialog.getByRole("button", { name: "Load older history" }).click();
    await expect(dialog.getByText("Beginning of log")).toBeVisible();
    await expect(
      dialog.getByText("Initial message 0", { exact: true }),
    ).toHaveCount(1);
    await dialog.locator('article[data-kind="tool"] summary').click();
    await dialog.getByRole("button", { name: "Jump to latest" }).click();
    await page.screenshot({
      path: test.info().outputPath(`${agent}-viewer.png`),
    });
    await dialog.getByRole("button", { name: "Back to dashboard" }).click();
    const count = f.requests.length;
    await page.waitForTimeout(1200);
    expect(f.requests.length).toBe(count);
    await page.locator("#task-3").getByRole("button").click();
    await page.getByRole("tab", { name: "runs", exact: true }).click();
    await page
      .getByLabel("Task detail")
      .getByRole("button", { name: "Watch", exact: true })
      .click();
    await expect(
      dialog.getByText("After reconnect", { exact: true }),
    ).toHaveCount(1);
    f.subsequentRun();
    await expect(dialog.getByRole("heading", { level: 2 })).toContainText(
      "Run #11",
    );
    const offsets = await dialog
      .locator("article")
      .evaluateAll((els) =>
        els.map((el) => Number(el.getAttribute("data-offset"))),
      );
    expect(offsets).toEqual([...new Set(offsets)].sort((a, b) => a - b));
    f.run.ended_at = "2026-10-28T04:00:00Z";
    f.run.exit_code = 0;
    f.append(message("Final message", agent));
    await expect(
      dialog.getByText("Final message", { exact: true }),
    ).toHaveCount(1);
    await expect(
      dialog.locator("header").getByText("Completed", { exact: true }),
    ).toBeVisible();
    const ended = f.requests.length;
    await page.waitForTimeout(1200);
    expect(f.requests.length).toBe(ended);
  });

test("historical attempts, unavailable logs, mobile and return position", async ({
  page,
}) => {
  const f = await fixture(page);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(base + "&task=3&tab=runs");
  const detail = page.getByLabel("Task detail");
  await detail
    .getByRole("button", { name: "View log", exact: true })
    .nth(1)
    .scrollIntoViewIfNeeded();
  const top = await detail.evaluate((el) => el.scrollTop);
  await detail
    .getByRole("button", { name: "View log", exact: true })
    .nth(1)
    .click();
  const dialog = page.getByRole("dialog");
  await expect(
    dialog.getByText("Historical attempt 13", { exact: true }),
  ).toBeVisible();
  await expect(
    dialog.getByText("Failed · exit 9", { exact: true }),
  ).toHaveAttribute("data-run-state", "failed");
  const count = f.requests.length;
  await page.waitForTimeout(1200);
  expect(f.requests.length).toBe(count);
  await page.screenshot({
    path: test.info().outputPath("historical-mobile.png"),
  });
  expect(
    await dialog.evaluate((el) => el.scrollWidth <= window.innerWidth),
  ).toBe(true);
  await dialog.getByRole("button", { name: "Back to dashboard" }).click();
  await expect(
    page.getByRole("tab", { name: "runs", exact: true }),
  ).toHaveAttribute("aria-selected", "true");
  expect(
    Math.abs((await detail.evaluate((el) => el.scrollTop)) - top),
  ).toBeLessThan(3);
  await detail
    .getByRole("button", { name: "View log", exact: true })
    .nth(0)
    .click();
  await expect(
    dialog.getByText("Historical attempt 12", { exact: true }),
  ).toBeVisible();
  await expect(
    dialog.getByText("Historical attempt 13", { exact: true }),
  ).toHaveCount(0);
  await expect(
    dialog.locator("header").getByText("Completed", { exact: true }),
  ).toBeVisible();
  await page.goBack();
  await detail
    .getByRole("button", { name: "View log", exact: true })
    .nth(2)
    .click();
  await expect(dialog.getByRole("alert")).toContainText("History unavailable");
});

test("bounded buffering pauses explicitly and Jump to latest resets the window", async ({
  page,
}) => {
  const f = await fixture(page, "codex");
  await page.goto(base + "&run=11");
  const dialog = page.getByRole("dialog");
  await expect(
    dialog.getByText("Initial message 39", { exact: true }),
  ).toHaveCount(1);
  for (let i = 0; i < 4100; i++) f.append(message(`Bulk ${i}`, "codex"));
  await expect(dialog.getByRole("status")).toContainText("Window full", {
    timeout: 15000,
  });
  expect(await dialog.locator("article").count()).toBeLessThanOrEqual(4000);
  const count = f.requests.length;
  await page.waitForTimeout(1200);
  expect(f.requests.length).toBe(count);
  await dialog.getByRole("button", { name: "Jump to latest" }).click();
  await expect(dialog.getByText("Bulk 4099", { exact: true })).toHaveCount(1);
  expect(await dialog.locator("article").count()).toBe(20);
  await expect(dialog.getByRole("status")).toHaveText("Connected");
});

test("unsupported API stops polling and offers explicit recovery", async ({
  page,
}) => {
  await fixture(page);
  let requests = 0;
  await page.route("http://axe.test/api/runs/*/log?*", (route) => {
    requests++;
    return route.fulfill({
      status: 404,
      json: { error: "no route for GET /api/runs/236/log" },
    });
  });
  await page.goto(base + "&run=236");
  const dialog = page.getByRole("dialog", { name: "Run log" });
  await expect(dialog.getByRole("alert")).toContainText(
    "API does not support run logs",
  );
  await expect(dialog.getByText("Loading run", { exact: true })).toHaveCount(0);
  await expect(
    dialog.getByText("Waiting for output", { exact: true }),
  ).toHaveCount(0);
  await page.waitForTimeout(2200);
  expect(requests).toBe(1);
  await dialog.getByRole("button", { name: "Retry", exact: true }).click();
  await expect.poll(() => requests).toBe(2);
});

for (const scenario of [
  "unknown",
  "forbidden",
  "missing",
  "empty",
  "completed",
  "disconnect",
])
  test(`Watch distinguishes ${scenario} and recovers`, async ({ page }) => {
    const f = await fixture(page);
    let requests = 0;
    let recovered = false;
    await page.route("http://axe.test/api/runs/*/log?*", (route) => {
      requests++;
      if (!recovered) {
        if (scenario === "unknown")
          return route.fulfill({
            status: 404,
            json: { error: "run 11 not found" },
          });
        if (scenario === "forbidden")
          return route.fulfill({ status: 403, body: "Forbidden" });
        if (scenario === "disconnect")
          return route.fulfill({ status: 503, json: { error: "unavailable" } });
        if (scenario === "missing")
          return route.fulfill({
            json: { run: f.run, unavailable: "Log missing or unreadable" },
          });
      }
      return route.fulfill({
        json: {
          run:
            scenario === "completed"
              ? { ...f.run, ended_at: "2026-10-28T04:00:00Z", exit_code: 0 }
              : f.run,
          page: { generation: "empty", start: 0, end: 0, size: 0, lines: [] },
        },
      });
    });
    await page.goto(base + "&run=11");
    const dialog = page.getByRole("dialog", { name: "Run log" });
    if (scenario === "empty") {
      await expect(dialog.getByRole("status")).toHaveText("Connected");
      await expect(
        dialog.getByText("Waiting for output", { exact: true }),
      ).toBeVisible();
      await expect.poll(() => requests).toBeGreaterThan(1);
      return;
    }
    if (scenario === "completed") {
      await expect(dialog.getByRole("status")).toHaveText("History");
      await expect(dialog.getByText("No output recorded")).toBeVisible();
    } else {
      const text = {
        unknown: "Run not found",
        forbidden: "HTTP 403",
        missing: "Log missing or unreadable",
        disconnect: "failed after 3 retries",
      }[scenario]!;
      await expect(dialog.getByRole("alert")).toContainText(text, {
        timeout: 15000,
      });
    }
    const count = requests;
    expect(count).toBe(scenario === "disconnect" ? 4 : 1);
    await page.waitForTimeout(2200);
    expect(requests).toBe(count);
    if (scenario !== "completed") {
      recovered = true;
      await dialog.getByRole("button", { name: "Retry", exact: true }).click();
      await expect(dialog.getByRole("status")).toHaveText("Connected");
      await expect(dialog.getByRole("alert")).toHaveCount(0);
    }
  });

test("Claude partial messages reconcile after reconnect and history prepend", async ({
  page,
}) => {
  const f = await fixture(page);
  const stream = (event: unknown) => f.append({ type: "stream_event", event });
  stream({ type: "message_start", message: { id: "streamed" } });
  stream({
    type: "content_block_start",
    index: 0,
    content_block: { type: "text", text: "" },
  });
  stream({
    type: "content_block_delta",
    index: 0,
    delta: { type: "text_delta", text: "Streaming paragraph" },
  });
  await page.goto(base + "&run=11");
  const dialog = page.getByRole("dialog");
  await expect(
    dialog.getByText("Streaming paragraph", { exact: true }),
  ).toHaveCount(1);
  f.disconnect(true);
  await expect(dialog.getByRole("status")).toHaveText("Reconnecting");
  stream({
    type: "content_block_delta",
    index: 0,
    delta: { type: "text_delta", text: " completed." },
  });
  f.append({
    type: "assistant",
    message: {
      id: "streamed",
      content: [{ type: "text", text: "Streaming paragraph completed." }],
    },
  });
  f.disconnect(false);
  await expect(
    dialog.getByText("Streaming paragraph completed.", { exact: true }),
  ).toHaveCount(1);
  await expect(
    dialog.getByText("Streaming paragraph", { exact: true }),
  ).toHaveCount(0);
  await dialog.getByRole("button", { name: "Load older history" }).click();
  await expect(
    dialog.getByText("Initial message 5", { exact: true }),
  ).toHaveCount(1);
  await expect(
    dialog.getByText("Streaming paragraph completed.", { exact: true }),
  ).toHaveCount(1);
  const offsets = await dialog
    .locator("article")
    .evaluateAll((els) =>
      els.map((el) => Number(el.getAttribute("data-offset"))),
    );
  expect(offsets).toEqual([...new Set(offsets)].sort((a, b) => a - b));
});
