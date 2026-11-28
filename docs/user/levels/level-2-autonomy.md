---
title: "Level 2 · Autonomy"
description: "Things improve without me: give agents a goal and its constraints, get outcomes back."
---

**Things improve without me.** You give agents a [goal](../primitives/goal.md)
and its constraints. They run hypothesis, test and verdict loops and report
outcomes, not activity.

## How you work

- A bot proposes a goal: the intent (what success looks like and how much
  autonomy it grants), a deadline and weekly budgets for cost and your
  attention. You activate it; only you change intent, deadline or budget.
- The bots on the goal decide what to try next. Each try is ordinary work:
  [tasks](../primitives/task.md), [routines](../primitives/routine.md) and
  scheduled [bot](../primitives/bot.md) wakes labelled with the goal.
- Every hypothesis, test and verdict is appended to the goal's trajectory,
  so you read what was learned, not a log of activity.
- The goal's line on the bot's board shows spend against its budget.

## What axe gives you here

| Need | Primitive | Start with |
|---|---|---|
| A durable outcome with limits | [Goal](../primitives/goal.md) | `axe goal propose`, `axe goal activate` |
| The agents that pursue it | [Bot](../primitives/bot.md) | `axe goal join`, `axe bot schedule add` |
| The experiments | [Task](../primitives/task.md), [Routine](../primitives/routine.md) | `axe task add --goal <name>` |
| What was learned | [Goal](../primitives/goal.md) | `axe goal show`, `axe goal log` |

Run Level 2 in a domain only once Level 1 is dull there: the bot already
delivers reliably, and its escalations are real decisions.
