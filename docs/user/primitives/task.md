---
title: "Task"
description: "A unit of work with a spec and a state, carried by one agent session from spec to done."
---

A task is a piece of work: a project, a spec and a state. The node gives it
one agent session that carries it from spec to `done` and is resumed every
time the task comes back, so the agent keeps its context across holds and
waits. Every change is an event in the task's log.

## When to use it

- Work you do not need to watch: a fix, a feature, a report, a migration.
- Anything that should leave evidence (notes, links, artifacts) and stop at
  a clear point when it needs your decision.
- The experiments of a [goal](goal.md) and the output of a
  [routine](routine.md) are tasks too.

Write the spec as `## Outcome` (what must be true) and `## Verify` (how to
check it), optionally `## Plan`, `## Context` and `## Deliver`. The spec is
the workflow: axe has no pipelines.

## Main commands

| Command | What it does |
|---|---|
| `axe task add --project P --title T --detail-file spec.md` | Queue a task (`--repo`, `--owner`, `--label`, `--agent`, `--goal`, `--blocked-by`) |
| `axe node` | Claim ready tasks and run their agent sessions (`--once` for one pass) |
| `axe task list` / `axe task show ID` / `axe task log ID` | Follow work; `--state escalated` is what waits on you |
| `axe task note ID TEXT` / `axe task artifact ID URL` | Record evidence |
| `axe task escalate ID --blocked … --resolver … --resume … --class …` | Hold for a human, with a block note |
| `axe task approve ID` | Your decision: resume a held task |
| `axe task wait ID --for 15m --note …` / `--on IDS` | Wait for a time or other tasks, then resume on its own |
| `axe task deps ID` | Show or edit prerequisites |

States: `pending` (ready), `in_progress`, `escalated` (waits on you),
`blocked` (an external wait), `done` (the outcome holds; dependents start).

## Example

```sh
cat > spec.md <<'SPEC'
## Outcome
README has a "Development" section with the build and test commands.
## Verify
- The commands in it run on a fresh clone.
SPEC
axe task add --project web --repo web --title "Document the dev loop" --detail-file spec.md
axe node --once
axe task log 1
```

More: [Tasks](../concepts/tasks.md) (states, dependencies, actors) and
[Run workspaces](../concepts/run-workspaces.md).
