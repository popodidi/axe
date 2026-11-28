---
title: "Level 0 · Skill"
description: "I can do more: reusable skills and context make complex work close to one-shot."
---

**I can do more.** You are the practitioner and your agent executes. What
changes is how much context the agent starts with: reusable skills and docs
make complex work close to one-shot, so you explain a job once, not every
time.

## How you work

- Work in your own agent session (Claude Code, Codex, ...), as you do today.
- When you explain the same thing twice, write it down as a skill or a doc
  and keep it in the [store](../primitives/store.md), attached to the scope
  it applies to (`global`, a label, a repo).
- Hand work you do not need to watch to axe as a [task](../primitives/task.md):
  a spec with an outcome and how to verify it. The node runs it in its own
  session with the same skills and docs attached.

## What axe gives you here

| Need | Primitive | Start with |
|---|---|---|
| Your agent knows how to use axe | the `axe` skill | `axe skill install` |
| Skills and docs every run gets | [Store](../primitives/store.md) | `axe doc put`, `axe doc attach` |
| Work that runs without your session | [Task](../primitives/task.md) | `axe task add`, `axe node` |
| See what a run would get | [Store](../primitives/store.md) | `axe context explain <task>`, `axe workspace <task>` |

## Done when

Your agent can queue, follow and close tasks on its own through the axe
skill, and you mostly read results. [Get started](../getting-started/quickstart.md)
takes you here. Next: [Level 1 · Delegation](level-1-delegation.md).
