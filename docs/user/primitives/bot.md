---
title: "Bot"
description: "A persistent agent identity with one session, woken by messages, schedules and task events."
---

A bot is an agent that stays. It has a name, a project, an identity and a
code of conduct, and one agent session that is resumed on every wake. Wakes
come from the bot itself, named cron schedules, task-event subscriptions and,
optionally, Telegram messages. A bot keeps notes, and its board shows you
what needs your attention. Heavy work it hands off to [tasks](task.md).

## When to use it

- [Level 1](../levels/level-1-delegation.md): a chief of staff that turns
  your requests into tasks, follows them up and brings back only decisions.
- An owner for a domain (ops, a product area) that should react to events,
  not wait for you to ask.

## Main commands

| Command | What it does |
|---|---|
| `axe bot create --file bot.json` | Create a bot: `name`, `project`, `agent`, `identity`, `conduct` |
| `axe bot wake NAME --now --note TEXT` | Wake it with a message (`--in`, `--at` for later) |
| `axe bot schedule add SCHEDULE --bot NAME --cron … --objective … --why …` | A recurring wake |
| `axe bot subscribe NAME --why …` / `axe bot watch ID --bot NAME --why …` | Wake on task, service or routine events, or on one task |
| `axe bot show NAME` / `axe bot log NAME` / `axe bot notes NAME` | State, history, notebook |
| `axe bot board show` | What the owner should see (`--bot NAME`) |
| `axe bot edit NAME …` | Change identity, conduct, repos or runtime (with `--why`) |
| `axe bot pause` / `resume` / `stop` / `reset` | Control its runs and session |

The bot protocol (runs, wakes, memory, authority, Telegram) is built in;
bundles in the [store](store.md) only add opinions.

## Example

```sh
cat > bot.json <<'JSON'
{
  "name": "ops",
  "project": "web",
  "identity": "You are ops, the owner's chief of staff for the web project.",
  "conduct": "Turn requests into tasks with an Outcome and a Verify. Follow them to done. Escalate only decisions."
}
JSON
axe bot create --file bot.json
axe bot wake ops --now --note "Queue a task to document the dev loop in web"
axe node
```

More: [Routines, bots and goals](../concepts/routines-bots-goals.md), the
[bots design](../../../docs/design/bots.md) and
[runtimes from Telegram](../guides/runtimes-from-telegram.md).
