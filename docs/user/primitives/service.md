---
title: "Service"
description: "A long-running or scheduled process a bot declares and the node keeps running."
---

A service is a process a [bot](bot.md) declares: a name, a shell command, a
working directory (the bot's run folder by default), environment and a
restart policy. The server stores the declaration; a long-running `axe node`
runs it as its own process group, restarts it by its policy with backoff,
wakes the bot on a crash loop and messages you if it keeps crashing. With
`--cron` it is a scheduled service instead: the node runs it at each cron
time, one run at a time.

## When to use it

- A poller, a webhook receiver or a small dashboard a bot relies on.
- A script that should run on a schedule on the node's machine and report
  failures to the bot, without creating a task each time (a
  [routine](routine.md) creates tasks; a scheduled service runs a command).

Services live and die with the node: no systemd unit or container per
service. Keep secrets in sealed forms, not `--env`, which is stored in plain
text.

## Main commands

| Command | What it does |
|---|---|
| `axe service add NAME --bot B --command CMD` | Declare (or replace) a service (`--cwd`, `--env K=V`, `--restart always\|on-failure\|never`) |
| `axe service add NAME --bot B --command CMD --cron "*/5 * * * *"` | A scheduled service (`--timezone`, `--timeout`, `--no-catch-up`) |
| `axe service list` | Each service's state, restarts, uptime or last run |
| `axe service logs NAME` | The tail of its log, read from the node running it |
| `axe service stop NAME` / `start NAME` / `remove NAME` | Turn it off or on, or delete it |

`--bot` defaults to `$AXE_BOT`, so a bot declares its own services.

## Example

```sh
axe service add uptime --bot ops --command 'while true; do curl -fsS https://example.com/health >/dev/null || echo down; sleep 60; done' --restart always
axe service list --bot ops
axe service logs uptime --bot ops
```

More: [Routines, bots and goals](../concepts/routines-bots-goals.md) (the
Services entry lists every rule).
