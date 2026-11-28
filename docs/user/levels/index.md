---
title: "The level model"
description: "Three operating modes for working with agents. You work by the model; axe's primitives help you get there."
---

The levels describe how you work with agents, not what axe is. Each level
removes a category of work from you. They are operating modes, not tiers: you
can run at Level 2 in one domain and Level 0 in another, and most people
start at Level 0 everywhere.

axe does not make you climb. It gives you a small set of
[primitives](../primitives/index.md) that make each mode hold up once you
stop watching: work that survives the end of a conversation, evidence you can
check, and a clear point where a decision comes back to you.

| Level | You stop doing | You still own | Primitives it leans on |
|---|---|---|---|
| [0 · Skill](level-0-skill.md) | Re-explaining context and steps for each job | Each piece of work, start to finish | [Store](../primitives/store.md) (skills, docs), [Task](../primitives/task.md) |
| [1 · Delegation](level-1-delegation.md) | Running the operational loop: planning, dispatching, chasing, checking | Decisions and priorities | [Task](../primitives/task.md), [Bot](../primitives/bot.md), [Routine](../primitives/routine.md), [Service](../primitives/service.md) |
| [2 · Autonomy](level-2-autonomy.md) | Choosing what to try next toward an outcome | The goal, its limits and its budget | [Goal](../primitives/goal.md), [Bot](../primitives/bot.md), [Task](../primitives/task.md), [Routine](../primitives/routine.md) |

## Moving up a level

Move a domain up when the level below is boring: the skills cover the work,
tasks finish with evidence you trust, and escalations are real decisions
rather than missing context. If tasks keep escalating for context or
tooling (`axe task escalate --class capability`), the fix is one level down:
a better skill or doc, not more autonomy.

Three principles hold at every level:

- **Ownership over execution.** One agent session carries a task from spec
  to verified delivery, and follows up, recovers or escalates.
- **Continuous, not session-bound.** Tasks, routines and goals outlive
  conversations, so work keeps moving while you are away.
- **Autonomy with accountability.** Permissions, evidence and escalation keep
  you the principal. Consequential decisions come back to you.

Start with [Get started](../getting-started/quickstart.md): it ends at Level 0,
with your agent driving axe.
