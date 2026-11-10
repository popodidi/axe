import type { Page } from "@playwright/test";
import { mockDesk } from "./fixtures";
import type {
  Activity,
  Bot,
  BotEvent,
  BotRun,
  BotSchedule,
  BotSummary,
  Wake,
} from "../lib/bots";
// Read-only bot API fixture: two generic bots, one with a live run and a
// week of runs, wakes, events and message metadata relative to today (UTC).
const DAY = 86400000;
const today = Math.floor(Date.now() / DAY) * DAY;
/** `day` days before today (UTC), at `h` hours and `m` minutes. */
export const ago = (day: number, h = 0, m = 0, s = 0) =>
  new Date(today - day * DAY + h * 3600000 + m * 60000 + s * 1000).toISOString();
const summary = (b: Partial<BotSummary> & { name: string }): BotSummary => ({
  project: "atlas",
  agent: "claude",
  state: "active",
  attention: false,
  failures: 0,
  live_run: null,
  next_wake: null,
  queued: 0,
  last_run_end: ago(1, 9),
  ...b,
});
export const bots: BotSummary[] = [
  summary({
    name: "ops-bot",
    live_run: 31,
    next_wake: ago(-1, 9),
    queued: 2,
  }),
  summary({
    name: "watch-bot",
    project: "orbit",
    state: "paused",
    attention: true,
    failures: 3,
  }),
];
const diff = (field: string, from: string, to: string) =>
  `why: tighten\n--- ${field}\n+++ ${field}\n@@ -1 +1 @@\n-${from}\n+${to}\n`;
export const bot: Bot = {
  ...bots[0],
  id: 1,
  identity: "## Ops bot\n\nKeeps the owner's queue moving.",
  conduct: "- Be brief.\n- Never merge without approval.",
  notes: "commitments: none",
  nudge_after_seconds: 1800,
  session_agent: "claude",
  session_id: "bot-session-1",
  resume_count: 4,
  created_at: ago(6),
  updated_at: ago(1, 9),
};
const run = (
  id: number,
  start: string,
  end: string | null,
  extra: Partial<BotRun> = {},
): BotRun => ({
  id,
  bot: "ops-bot",
  agent: "claude",
  mode: "resume",
  session_id: "bot-session-1",
  claim: `bot-run:${id}`,
  started_at: start,
  ended_at: end,
  exit_code: end ? 0 : null,
  stop_reason: end ? "exit" : null,
  log_path: `/home/agent/.local/state/axe/logs/bot-ops-bot-${id}.log`,
  current_activity: end ? "axe bot tg send" : "gh pr checks 12",
  last_activity_at: end ?? ago(0, 0, 1),
  live: !end,
  ...extra,
});
export const botRuns: BotRun[] = [
  run(31, ago(0, 0, 0), null),
  run(30, ago(1, 9), ago(1, 9, 4)),
  run(29, ago(1, 8), ago(1, 8, 10), { stop_reason: "stop", exit_code: null }),
  run(28, ago(2, 10), ago(2, 10, 2), { stop_reason: "exit", exit_code: 1 }),
  run(27, ago(3, 7), ago(3, 7, 6), { mode: "fresh" }),
];
const wake = (id: number, kind: string, created: string, extra: Partial<Wake> = {}): Wake => ({
  id,
  kind,
  body: "",
  due_at: created,
  created_at: created,
  actor: "owner",
  run_id: null,
  delivered_via: null,
  redelivered: false,
  message: null,
  ...extra,
});
export const schedules: BotSchedule[] = [
  {
    id: 1,
    name: "weekly-digest",
    cron: "0 9 * * 1",
    timezone: "UTC",
    objective: "Send the weekly digest",
    enabled: true,
    next_at: ago(-2, 9),
    last_fired_at: ago(5, 9),
  },
  {
    id: 2,
    name: "cleanup",
    cron: "30 3 * * *",
    timezone: "UTC",
    objective: "Sweep stale branches",
    enabled: false,
    next_at: ago(-1, 3, 30),
    last_fired_at: null,
  },
];
export const wakes: Wake[] = [
  wake(1, "wake", ago(3, 7), { body: "bot created", actor: "human", run_id: 27, delivered_via: "prompt" }),
  wake(2, "message", ago(1, 8, 1), {
    run_id: 29,
    delivered_via: "prompt",
    message: { id: 11, kind: "text", at: ago(1, 8, 1) },
  }),
  wake(3, "message", ago(1, 8, 5), {
    run_id: 30,
    delivered_via: "prompt",
    message: { id: 12, kind: "photo", at: ago(1, 8, 5) },
  }),
  wake(4, "button", ago(1, 9, 2), { run_id: 30, delivered_via: "hook" }),
  wake(5, "wake", ago(-1, 9), { body: "check PR #12 CI", actor: "bot:ops-bot" }),
];
export const events: BotEvent[] = [
  { id: 1, at: ago(3, 6), actor: "human", session: null, kind: "created", body: "" },
  { id: 2, at: ago(2, 5), actor: "human", session: null, kind: "conduct", body: diff("conduct", "- Be thorough.", "- Be brief.") },
  { id: 3, at: ago(2, 10, 2), actor: "dispatcher", session: null, kind: "run_end", body: '{"run":28,"reason":"exit","exit_code":1}' },
  { id: 4, at: ago(1, 7), actor: "telegram", session: null, kind: "security_drop", body: '{"from_id":"unknown","kind":"text"}' },
  { id: 5, at: ago(1, 8, 10), actor: "human", session: null, kind: "stop", body: "live run 29" },
];
export const activity: Activity = {
  messages: [
    { id: 11, direction: "in", kind: "text", reply_to: null, edit_of: null, attachments: 0, at: ago(1, 8, 1) },
    { id: 12, direction: "in", kind: "photo", reply_to: null, edit_of: null, attachments: 1, at: ago(1, 8, 5) },
    { id: 13, direction: "out", kind: "text", reply_to: 12, edit_of: null, attachments: 0, at: ago(1, 8, 7) },
    { id: 14, direction: "in", kind: "text", reply_to: null, edit_of: null, attachments: 0, at: ago(1, 9, 0) },
    { id: 15, direction: "out", kind: "text", reply_to: 14, edit_of: null, attachments: 0, at: ago(1, 9, 1) },
  ],
  task_actions: [
    { task_id: 3, at: ago(2, 9), actor: "human", kind: "created" },
    { task_id: 3, at: ago(1, 9, 2), actor: "bot:ops-bot", kind: "note" },
    { task_id: 4, at: ago(1, 9, 3), actor: "bot:ops-bot", kind: "created" },
    { task_id: 4, at: ago(1, 9, 3), actor: "bot:ops-bot", kind: "approval" },
  ],
};
/** Every bot read the dashboard made, for the read-only and redaction checks. */
export async function mockBots(page: Page) {
  const reads: string[] = [];
  await mockDesk(page);
  const json = (body: unknown) => ({
    json: body,
    headers: { "Access-Control-Allow-Origin": "*" },
  });
  // Task slots are full while a bot still runs in its own pool (task 345).
  await page.route("http://axe.test/api/admission", async (route) => {
    reads.push(route.request().method() + " /api/admission");
    return route.fulfill(
      json({
        enabled: true,
        admitting: false,
        reason: "disk 10.0 GiB free",
        bots_admitting: true,
        bots_reason: null,
        pools: {
          tasks: { used: 5, total: 5 },
          bots: { used: 1, total: 2, configured: 2 },
        },
      }),
    );
  });
  await page.route(/^http:\/\/axe\.test\/api\/(bots|bot-runs)/, async (route) => {
    const url = new URL(route.request().url());
    reads.push(route.request().method() + " " + url.pathname + url.search);
    const path = url.pathname.split("/").slice(2);
    if (path[0] === "bot-runs") {
      if (path[2] === "log") {
        const r = botRuns.find((x) => x.id === Number(path[1]))!;
        const text = JSON.stringify({
          type: "assistant",
          message: { content: [{ type: "text", text: `Bot run ${r.id} transcript` }] },
        });
        return route.fulfill(
          json({
            run: r,
            log_complete: !!r.ended_at,
            page: {
              generation: "bot",
              start: 0,
              end: text.length + 1,
              size: text.length + 1,
              lines: [{ start: 0, end: text.length + 1, text, fragment: false }],
            },
          }),
        );
      }
      const live = url.searchParams.has("live");
      const forBot = url.searchParams.get("bot");
      return route.fulfill(
        json(
          botRuns.filter(
            (r) => (!live || !r.ended_at) && (!forBot || r.bot === forBot),
          ),
        ),
      );
    }
    if (path.length === 1) return route.fulfill(json(bots));
    const b = bots.find((x) => x.name === path[1]);
    if (!b) return route.fulfill({ status: 404, json: { error: `bot ${path[1]} not found` } });
    const own = b.name === "ops-bot";
    if (path[2] === "schedules") return route.fulfill(json(own ? schedules : []));
    if (path[2] === "wakes") return route.fulfill(json(own ? wakes : []));
    if (path[2] === "events") return route.fulfill(json(own ? events : []));
    if (path[2] === "activity")
      return route.fulfill(json(own ? activity : { messages: [], task_actions: [] }));
    return route.fulfill(
      json(own ? bot : { ...bot, ...b, id: 2, session_id: null, session_agent: null }),
    );
  });
  return reads;
}
