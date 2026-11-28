---
title: "Goal"
description: "A durable outcome with an intent, a deadline, weekly budgets and a trajectory that bots share."
---

A goal is an outcome bots work toward over time, rather than a single job.
It has three parts: the **intent** (what success looks like and how much
autonomy it grants, as free text), **resources** (a deadline and weekly
budgets for cost and your attention) and a **trajectory** (an append-only log
of what was tried and learned). Several bots can share one goal.

## When to use it

- [Level 2](../levels/level-2-autonomy.md) work: "raise the signup rate",
  "keep p95 latency under 200 ms", where the next step is a judgement call.
- When you want to cap what an effort may spend. While either weekly budget
  is spent, linking more tasks to the goal is refused until you raise it.

Bots propose goals; you activate them, and only you change the intent,
deadline or budget.

## Main commands

| Command | What it does |
|---|---|
| `axe goal propose --file g.json` | Propose a goal (usually a bot does) |
| `axe goal activate NAME --why TEXT` | Your decision: start it |
| `axe goal list` / `axe goal show NAME` / `axe goal log NAME` | Intent, budget, linked tasks, trajectory |
| `axe goal edit NAME …` | Change intent, deadline or budget |
| `axe goal record NAME TEXT --kind hypothesis\|test\|verdict\|observation\|note` | Append to the trajectory |
| `axe task add … --goal NAME` | Link a task (an experiment) to the goal |
| `axe goal pause` / `resume` / `done` / `drop` | Lifecycle |

## Example

```sh
cat > g.json <<'JSON'
{
  "name": "faster-builds",
  "project": "web",
  "intent": "CI for web finishes in under 5 minutes at p50. Try anything that keeps the test suite unchanged; ask before changing runners.",
  "deadline": "2027-01-31",
  "budget_cost_week": 20,
  "budget_attention_week": 3,
  "bots": ["ops"]
}
JSON
axe goal propose --file g.json
axe goal activate faster-builds --why "Agreed: CI time is the bottleneck this quarter"
axe goal record faster-builds "Caching node_modules should save about 90 s" --kind hypothesis
```

More: [Routines, bots and goals](../concepts/routines-bots-goals.md) and the
[goals design](../../../docs/design/goals.md).
