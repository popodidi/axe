---
title: "Primitives"
description: "The six things axe gives you: task, routine, goal, bot, service and store."
---

You work by the [level model](../levels/index.md); axe gives you a few
durable building blocks so that work holds up when you are not watching.
Each primitive has its own `axe <noun>` command namespace; `axe --help`
lists them and `axe <noun> --help` every flag.

| Primitive | What it is | Why it exists |
|---|---|---|
| [Task](task.md) | A unit of work with a spec and a state, carried by one agent session from spec to done | Work outlives the conversation that started it, with evidence and a clear hold for your decisions |
| [Routine](routine.md) | A cron template that creates an ordinary task each time it fires | Recurring work happens without anyone remembering it |
| [Goal](goal.md) | A durable outcome with an intent, a deadline, weekly budgets and a trajectory | Agents can pursue an outcome, not just finish a job, within limits you set |
| [Bot](bot.md) | A persistent agent identity with one session, woken by messages, schedules and task events | Someone owns the operational loop between tasks, so you do not |
| [Service](service.md) | A long-running or scheduled process a bot declares and the node keeps running | Things that must stay up live next to the bot that needs them, with restarts and crash alerts |
| [Store](store.md) | Versioned docs and skills attached to scopes, plus content-addressed files (blobs) | Every run starts with the right context, and evidence has a durable home |

## How they fit

- `axe server` holds all of them in one SQLite database and serves the HTTP
  API. `axe node` claims ready tasks, wakes bots and runs services; it can
  run on another machine.
- Tasks are the unit of work. Routines and goals create or link tasks; bots
  write and follow them; nothing else runs agent work.
- The store feeds every run: the node builds each task's or bot's run folder
  from the docs and skills attached to its scopes.

The [How it works](../concepts/architecture.md) pages explain the internals
and where each rule lives in the code.
