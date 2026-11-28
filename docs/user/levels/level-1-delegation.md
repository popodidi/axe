---
title: "Level 1 · Delegation"
description: "More can happen without me: a chief-of-staff bot owns the operational loop."
---

**More can happen without me.** A chief-of-staff [bot](../primitives/bot.md)
owns the operational loop: it plans, dispatches, verifies, and escalates only
what needs your judgment. You talk to it (Telegram is the main interface) and
answer decisions; you stop chasing work.

## How you work

- Tell the bot what you want. It writes the spec and queues
  [tasks](../primitives/task.md), each carried by one agent session from
  spec to done.
- The bot subscribes to task events, so it wakes when work finishes or
  escalates, checks the evidence and follows up.
- Recurring work becomes a [routine](../primitives/routine.md): a cron
  template that creates an ordinary task each time.
- Anything that must keep running (a poller, a small web hook) is a
  [service](../primitives/service.md) the bot declares and the node keeps
  alive.
- Holds come back to you as escalations with a block note: what is
  blocked, who must act, and when it resumes.

## What axe gives you here

| Need | Primitive | Start with |
|---|---|---|
| A persistent assistant | [Bot](../primitives/bot.md) | `axe bot create --file bot.json` |
| Work the bot hands out | [Task](../primitives/task.md) | `axe task add`, `axe task list --state escalated` |
| Recurring work | [Routine](../primitives/routine.md) | `axe routine save --file r.json` |
| Long-running processes | [Service](../primitives/service.md) | `axe service add` |
| Your decisions | [Task](../primitives/task.md) | `axe task approve`, `axe ask` |

## Done when

Most days you only answer escalations and read what was delivered. Next:
[Level 2 · Autonomy](level-2-autonomy.md).
