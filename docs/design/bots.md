# axe: Bots

Design approved 2026-11-07. This page records why bots are shaped the way they
are; the code is the reference for exact behavior (`axe bot --help`,
`crates/axe/src/bot.rs`, `dispatch/bots.rs`, `telegram.rs`).

## Context

Tasks run well in axe, but nothing proactive owns the portfolio between
tasks: the owner (the human operating axe) did intake, unblocking and
follow-ups by hand. A **bot** fills that gap. It is a primitive separate from
tasks:

- a persistent identity tied to a project (its default working directory);
- one continuous coding session, resumed on every wake;
- one wake queue that merges self-wakes, named schedules, task events and
  Telegram messages;
- identity and a code of conduct, held on the server and delivered as the
  system prompt, after the prompt parts of its attached bundles (the bot
  protocol and channel protocols, `bundles/` and `context.rs`);
- notes the bot reads and edits through the CLI.

Telegram is the main interface. Heavy work is still offloaded to tasks and
routines. Nothing in code or in the dashboard is specific to one kind of bot.

**Design rules (owner, 2026-11-07):**

- The bot is a standalone primitive with its own tables, API, CLI namespace
  and lifecycle. Task code gains no bot branches.
- Shared logic is reused through plain modules (agent process spawning,
  activity parsing, the dispatcher operation journal, the log relay,
  worktrees). There is no plugin framework.
- CLI verbs must not be confusable between tasks and bots (`axe bot ...`,
  `axe bot tg ...`).

## Concepts

| Concept | Meaning |
|---|---|
| bot | Name, project, agent, identity, conduct, notes, active or paused, and its stored session |
| wake | A queued reason to run (self or owner wake, message, button, schedule, subscription batch), with a due time |
| bot run | One spawned agent process. A bot has at most one live run, so wakes are serialized |
| bot event | Append-only audit: identity, conduct and notes diffs, lifecycle, run start and end, security drops |

A bot is ready when it is active, has no live run, and has a due wake.
Elapsed time alone never starts a run, and sleeping means exiting: a bot with
no schedules sleeps until something wakes it. Recurring work is a named
schedule with one objective each, not a periodic heartbeat, because a wake
with several objectives is bad for focus (2026-11-09).

## What goes where in the agent's context

| Content | Channel | Why |
|---|---|---|
| Bot protocol (attached bundles, `bundles/`), identity, conduct | System prompt, rendered on every spawn | They change rarely. In the system prompt they survive compaction, are never resent as conversation text and keep the prompt cache warm |
| Owner preferences and standing rules | The conduct's "Owner preferences" section | They must apply on every run. The bot records one the moment the owner states it, citing the message |
| Wakes | User prompt of each run | The only content that is new on each wake |
| Notes: working state (commitments, follow-ups, lessons) | CLI (`axe bot notes`) | They change often; in the system prompt every edit would break the cache, and resending them would bloat context. A fresh session reads them first |
| History | CLI (`axe bot log`, `axe bot tg history`) and the run logs | Retrieval on demand; nothing is distilled |

The agent needs a way to take a system prompt file and hook settings: an
agent's `bot_args` (`config.rs`), which the built-in Claude agent has. An
agent without them cannot run bots. Codex has none until its instruction
semantics on resume are verified.

Compaction is native. Everything durable is on the server, so nothing is
lost when a session summarizes. Because native compaction did not keep
sessions small in practice and cold re-reads dominated bot spend, a run that
exits cleanly with a very large context retires its session, and the next
wake starts fresh from the system prompt, notes and history (2026-11-10). This
reuses the fresh-session path instead of a headless compaction run. A session
is kept while the owner's latest message is unanswered.

## Wake delivery

- **Bursts.** A message wake is due after a short quiet window per chat, with
  a cap, so consecutive messages, a media group or a quick edit land in one
  run.
- **Mid-run messages.** For Claude, the run's hook settings call
  `axe bot inbox --take --hook` after each tool call and at stop. New wakes
  are delivered into the live run, labelled as possibly amending the current
  work; the stop hook blocks exit only when it took new wakes, so the loop
  ends. The same hook adds a nudge to offload when a run is long.
- **Fallback for any agent.** Wakes still undelivered when a run exits make
  the bot ready again at once, and the next run resumes the same session.
- **Owner interrupt.** Telegram `/stop` kills the live run without counting a
  failure; the next wake resumes the session. `/pause` also stops new wakes.
- **Failures.** A failed run returns its wakes for redelivery. Repeated
  failures first clear the session, then pause the bot and alert the owner.
- **Model-API outages.** A run that a model-API outage ended (an edge block,
  rate limit or server error in the agent's structured result) is not a
  failure: the bot keeps its session and failure count, and its wakes wait
  out a growing backoff before the next run. Task runs get the same treatment
  without spending run budget. Detection reads Claude Code's stream-json
  result only; other agents' errors count as failures
  (`api_outage` in `crates/axe/src/agent_proc.rs`).

Task-event subscriptions (2026-11-08) exist so escalations are not silent
while token use and Telegram noise stay bounded: chosen events wake the bot in
batches, others only ride along in the next run's prompt. Fan-out happens in
the transaction that writes the task event, so nothing lags or replays.
Following single tasks (2026-11-13) extends the same path: events on tasks a
bot owns (`--owned-wake`) or watches (`axe bot watch`) wake it on a 1m batch,
so it need not subscribe to a whole project's `done` or poll with self-wakes.
A watch is a `bot_subscriptions` row keyed `#<task>` (`subscription.rs`).

## Telegram and security

Telegram's Bot API shaped the gateway: one long-poll consumer per token, so
the gateway runs inside the single-replica server; no history method, so the
server stores both directions; per-chat rate limits, so outbound calls are
queued per chat. Agents call the server, never Telegram.

Only the owner controls a bot:

1. **Token isolation.** Each bot has its own Telegram bot. Tokens live only in
   files the server reads, never on the dispatcher host or in agent
   environments.
2. **Sender filter before anything is stored.** An update is accepted only
   from the configured owner's user id, not from a bot, an anonymous admin, a
   channel or an inline bot, and only in an allowlisted chat. Everything else
   is dropped with a metadata-only event and the owner is alerted.
3. **Forwarded content is data**, never instructions, and is marked so.
4. **Outbound allowlist.** A bot can only send to its allowlisted chats, and
   leaves any other chat it is added to.
5. **Read privacy.** Message text and files need the token, unlike other axe
   reads; the dashboard shows metadata only and files are proxied, not stored.

The owner's inbox is the bot chat's pinned list, rendered by the server
(`board.rs`, `goal_board.rs`): one pinned card per open decision and one
pinned message per active goal, edited in place as the goal changes, so the
owner never keeps a list by hand. The single pinned overview with task stats
was retired on 2026-11-13 at the owner's request: it only repeated what the
cards and goals show. The inbox is on by default: a bot's first plain
`bot tg send` (not a card, alert or command reply) becomes its board
message, so a new bot's chat gets its pins without a manual
`board set-message`; a bot with earlier messages, or whose inbox was turned
off, is left as it is (2026-11-14). With the inbox on, every escalated task in
the bot's subscribed projects gets a card, posted by the server if the bot
does not post one.

## Capabilities: set by conduct, not by hard limits

A bot session has the same capabilities as a normal session in its project:
it can edit through worktrees and PRs, create tasks and routines, resume holds
and run `axe approve`. Its focus comes from its conduct. A bot approval made
on an explicit owner message, with the message id in the note, counts as the
owner's; the residual risk of an injected instruction is resisted by conduct
alone and kept auditable by that note. A server-checked owner reference is
deferred. Hard limits stay only where they protect the Telegram channel
itself: the sender filter, the outbound allowlist and the token staying on the
server.

**Responsiveness.** A bot runs one wake at a time, so a long run delays every
new message. The protocol says to acknowledge first, do quick work inline,
use subagents for parallel lookups, and turn anything long into a task or
routine followed up with `axe bot wake --in`. The dashboard tracks reply
latency and run duration per bot so blocking shows up.

## Deferred

Telegram approval buttons as an approval path, a server-checked owner
reference, the local Bot API server, codex bots and mid-run hooks, confirmed
remote abort for bot runs.
