# axe: Goals

Design approved by the owner on 2026-11-11. This page records why goals are
shaped the way they are; the code is the reference for exact behavior
(`axe goal --help`, `crates/axe/src/goal.rs`, `goal_board.rs`). The method
bots follow is in the axe skill's goals reference
(`skills/axe/references/goals.md`).

## Context

Bots keep tasks moving, but nothing durable held what they were working
toward: a bot's current goal lived in its conduct, as free text for one bot,
with no trajectory, no budget and no way for a second bot to share it.

A **goal** is a durable outcome contract with three parts:

- **Intent**: what success looks like and how much autonomy the goal grants,
  as freeform markdown.
- **Resources**: a deadline and weekly budgets (cost and owner attention).
- **Trajectory**: what was tried and learned, as an append-only event log.

Hypothesis, test and verdict are a method written down in a skill, not a
primitive. Goals reuse tasks, wakes, schedules and routines for the work
itself; only the operational fields are structured. They follow the same
design rules as [bots](bots.md): own tables, API and CLI namespace, and no
branches in task code beyond an event kind.

## Owner decisions (2026-11-11)

- Bots propose goals and budgets; the owner activates them.
- A goal can be worked by several bots. Budget and trajectory belong to the
  goal, not to a bot.
- The per-goal autonomy tier lives in the freeform intent, not in a field.
- Budget is soft accounting plus one hard gate, shown on Telegram in each
  active goal's own pinned message (owner decision 2026-11-13; it replaced
  the goal lines of the pinned overview).

## Authority

A goal moves `proposed` -> `active` <-> `paused` -> `done`, and any open goal
can be dropped. Activation and edits to intent, deadline or budget are owner
decisions, as is attaching or detaching a bot other than the actor itself.
An owner decision by a non-human actor must cite the owner's instruction
(`--owner-ref`). The reference is stored, not verified: the same model as bot
approvals, a guardrail rather than a security boundary.

Tasks link to a goal through a task event (latest wins), not a task column,
and wakes and schedules carry a `[goal NAME]` label in their body, so neither
needed a change to task or wake storage.

## Budget

Spend is computed on read over a rolling seven-day window; nothing is stored.

- **Cost** is the estimated spend of runs of linked tasks, and of bot runs
  whose wakes all carry the goal's label. A bot run mixing the goal with other
  work is reported as unattributed, and runs with unknown usage as unknown,
  never as zero.
- **Attention** counts what the goal cost the owner: escalation episodes and
  approvals on linked tasks. Setting the goal up (propose, edits, activate) is
  not counted, so a fresh goal starts at 0.
- **The gate.** While either weekly budget is spent, linking more tasks to the
  goal is refused until the owner raises it. Nothing else is refused, and a
  goal with no budget never gates.

Each active goal has one pinned message in its bots' chats, a scoreboard
(owner's layout, 2026-11-14): name, status and deadline; a one-line summary;
the metric; the next linked task and how many linked tasks wait on the owner;
cost and attention as text bars against the weekly limits, always shown; and
a link to the goal's web page when the server's public URL is set (`[blobs]
public_url`). Status, summary and metric come from the latest observation
that opens with a status (`at risk: ...`, `goal::status_of`), so they need no
new field; without one the pin shows the goal state and the intent's first
sentence. It is edited in place whenever its text changes. Edits are silent,
and pausing, finishing or dropping the goal unpins it. Goal intent never goes into a bot's system prompt, so the
prompt cache stays stable when an intent changes.

## Deferred

Cross-goal portfolio allocation, server-checked owner references, and
attributing chat attention to a goal.
