# axe: Goals

Design approved by the owner on 2026-11-11 and built in phases: G1 the
primitive (this page), G2 budget accounting and the hard gate, G3 the pinned
goal board, G4 the goals reference in the axe skill, G5 the release.

## Context

Bots keep tasks moving, but nothing durable holds what they are working
toward. A bot's current goal lived in its conduct: one bot, free text, no
trajectory, no budget and no way for a second bot to share it.

A **goal** is a durable outcome contract with three parts:

- **Intent**: what success looks like and how much autonomy the goal grants,
  as freeform markdown.
- **Resources**: a deadline and weekly budgets (cost and owner attention).
- **Trajectory**: what was tried and learned, as an append-only event log.

Hypothesis, test and verdict are a method, written down in a skill, not a
primitive. Goals reuse tasks, wakes, schedules and routines for the work
itself; only the operational fields are structured.

**Design rules** (owner, 2026-11-07, `bots.md`): own tables, API and CLI
namespace; task code gains no branches beyond an event kind; shared logic
through plain modules; no plugin framework; CLI verbs not confusable with task
verbs.

## Owner decisions (2026-11-11)

- Bots propose goals and budgets; the owner activates them.
- A goal can be worked by several bots (many-to-many). Budget and trajectory
  belong to the goal, not to a bot.
- The per-goal autonomy tier lives in the freeform intent, not in a field.
- Budget is soft accounting plus one hard gate (G2), shown on a separate
  pinned goal board (G3).

## Data (schema v16, additive)

```sql
goals(id, name UNIQUE, project,           -- home project: where its tasks default
      state CHECK(proposed|active|paused|done|dropped),
      intent TEXT NOT NULL DEFAULT '',    -- freeform markdown, 16 KiB cap
      deadline TEXT NULL, budget_cost_week REAL NULL, budget_attention_week INTEGER NULL,
      created_at, updated_at)
goal_bots(goal_id, bot_id, joined_at, PRIMARY KEY(goal_id, bot_id))
goal_events(id, goal_id, at, actor, session, kind, task_id NULL, owner_ref NULL, body)
bot_schedules.goal_id INTEGER NULL        -- added column
```

`goal_events.kind` is a trajectory kind (`hypothesis`, `test`, `verdict`,
`observation`, `note`), written with `axe goal record`, or an audit kind
(`proposed`, `activated`, `intent`, `budget`, `deadline`, `join`, `leave`,
`state`), written by the server on each change. Intent edits store a unified
diff, like bot conduct edits. Audit bodies carry the `--why`.

A database lowered to `user_version` 15 still opens with a v15 binary (the new
tables and column are ignored) and upgrades again in place.

## Lifecycle and authority

| Action | From | To | Owner decision |
|---|---|---|---|
| `propose` | | proposed | Only when it attaches a bot other than the proposer |
| `activate` | proposed | active | Yes |
| `pause` | active | paused | No |
| `resume` | paused | active | No |
| `done` | active, paused | done | No |
| `drop` | proposed, active, paused | dropped | No |
| `edit` intent, deadline, budget | any | | Yes |
| `join`, `leave` | any | | Yes, unless the bot acts on itself |

`propose` attaches the proposing bot (actor `bot:<name>`). A goal with no bots
is allowed; moving a goal between bots is a join plus a leave, and leaving keeps
every event. An owner decision by a non-`human` actor (`bot:<name>`, `agent`)
needs `--owner-ref TEXT` (for example `msg #873` or `plan 2026-11-11`), stored
on the event; without it the server answers 403. The reference is recorded,
not verified by the server: the same model as bot approvals, a guardrail
rather than a security boundary.

## Linkage

- **Tasks**: a task event of kind `goal` whose value is the goal name;
  `axe add --goal G` and `axe set ID --goal G|none`. The latest event wins and
  an unknown goal is refused. There is no `tasks` column, the same pattern as
  `workload`.
- **Wakes**: `axe bot wake --goal G` and `axe bot schedule add|edit --goal G`.
  The label is stored in the wake body, so the wake line reads
  `[wake by <actor> HH:MMZ] [goal G] <note or objective>` and a dispatcher on
  an older binary still delivers it.

## Bot protocol

The protocol gains a short "Goals" section: a bot's goals are
`axe goal list --bot $AXE_BOT`, the method is in the axe skill's goals
reference (`references/goals.md`, G4), and each stage records one trajectory
event. A fresh session also runs `axe goal list --bot $AXE_BOT`. Goal intent
is never put in the system prompt, so the prompt cache stays stable when an
intent changes.

## Interfaces

- CLI: `axe goal propose|list|show|log|edit|activate|pause|resume|done|drop|join|leave|record`
  (README, "Goals").
- API: `GET /api/goals[?bot=&state=]`, `GET /api/goals/{name}`,
  `GET /api/goals/{name}/events`; token writes `POST /api/goals`,
  `PATCH /api/goals/{name}`, `POST /api/goals/{name}/{action}`,
  `POST /api/goals/{name}/events` ([HTTP API](../api-v1.md#goals)).

## Deferred

- A dashboard goals view.
- Cross-goal portfolio allocation (LV3).
- Server-checked owner references.
- Attributing chat attention (owner messages) to a goal.
