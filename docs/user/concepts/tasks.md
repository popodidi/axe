---
title: "Tasks"
description: "Tasks, their states, dependencies and actors: axe is not a workflow engine."
---

## Not a workflow engine

A task has a project, a spec (`detail`) and a **state**, and optionally an
`owner` (the bot or person who follows it up; its creator stays the first
event), `repos` and free-form `labels` (`--owner`, `--repo`, `--label` on
`axe task add` and `axe task set`; filters treat the project as a label). Once
dispatched, one agent session carries it from start to `done` and is resumed
on every later run ([one agent per task](../../../docs/design/one-agent-per-task.md)).
There are no pipelines or transitions in code: the spec is the workflow, and the
agent's own instructions (skills, `AGENTS.md`) say how to work a task. Every
mutation appends to an event log (`axe task log ID`), so a resumed session can see
what changed.

| State | Meaning |
|---|---|
| `pending` | Eligible to run; the node claims it and starts or resumes its session |
| `in_progress` | A session is working on it |
| `escalated` | A hold for a human decision; resumes only when set back to `pending` (or `axe task approve`) |
| `blocked` | A non-human external wait; also resumed explicitly |
| `done` | The completion contract: dependents start, so set it only when the spec's outcome holds |

Each task also has a derived `status`: `blocked` for a pending task still
waiting on prerequisites, otherwise its state. Human attention is
`axe task list --state escalated`. Block notes use one format so a hold says what
to do: `blocked:` / `resolver:` / `resume:` lines (`axe task escalate`).

Tasks may declare prerequisites (`blocked_by`, `axe task deps`). A dependency wait
is never stored: a task is ready when it is `pending`, every prerequisite is
`done` and its `not_before` has passed, computed inside each claim
transaction. Holds and dependencies stay separate; finishing a prerequisite
never lifts a hold. A rejected prerequisite is removed (`axe task deps --remove`),
never marked `done` to release its dependents. Code: `db.rs`.

The actor on each event is `--actor`, else `$AXE_ACTOR`, else `human`, with
`$AXE_SESSION` alongside. `axe task approve` records an owner decision and resumes
a held task; node task sessions are refused, a guardrail rather than a
security boundary since agents share one token.
