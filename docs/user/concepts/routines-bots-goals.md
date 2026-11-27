---
title: "Routines, bots and goals"
description: "Routines, bots, services and goals sit next to tasks."
---

Paths in `crates/axe/src/` unless noted.

These sit next to tasks with their own tables, API and CLI namespace; task
code gains no branches for them.

- **Routines** (`axe routine`, `routine.rs`) create ordinary pending tasks on a
  five-field cron in an IANA timezone. After downtime only the latest missed
  occurrence is created; manual triggers take a retry key. A template can set
  the created task's owner, repos and labels like `axe task add` (`routine::Template`).
  Each task keeps a snapshot of the template it came from. Cost controls on the
  template (`pre_check`, `model`, `effort`, `skills`) are read from that
  snapshot by the node (`dispatch/pre_check.rs`,
  [skills/axe/references/routines.md](../../../skills/axe/references/routines.md)).
- **Bots** (`axe bot`, `bot.rs`, `dispatch/bots.rs`) are persistent
  identities with one agent session resumed on every wake. Wakes come from the
  bot itself, named cron schedules (`schedule.rs`), task-event subscriptions
  (`subscription.rs`) and the Telegram gateway (`telegram.rs`, `board.rs`).
  A voice message reaches the bot as a local transcript marked
  `[voice message, auto-transcribed, may contain errors]`: the node runs
  SenseVoice-Small (sherpa-onnx) in a child process before the run, with no
  API or key, and keeps the audio as the message's attachment
  (`transcribe.rs`; off with `[defaults] transcribe_voice = false`).
  Design: [docs/design/bots.md](../../../docs/design/bots.md).
- **Services** (`axe service`, `service.rs`, `dispatch/services.rs`) are
  long-running processes a bot declares: name, shell command, cwd (default
  the bot's run folder), env and restart policy (`always`, `on-failure`,
  `never`). `axe service add <name> --command <cmd> [--cwd D] [--env K=V]
  [--restart P]`, `list`, `start`, `stop`, `logs [-n N]` and `remove`, with
  `--bot` defaulting to `$AXE_BOT`. The server stores the declarations; a
  long-running `axe node` runs every enabled one as its own child process
  group (`sh -c`, with `AXE_BOT`, `AXE_SERVICE`, `AXE_SERVER` and the bot's
  sealed values), its output in `<log_dir>/services/<bot>/<name>.log`
  (rotated past `[services] log_bytes`, one old file kept). An exit is
  restarted by the policy with a delay that doubles per quick crash; a crash
  loop wakes the bot, and one still going `escalate_after_seconds` later
  messages the owner too. Stopping the node sends SIGTERM to every
  service and SIGKILL after `stop_grace_seconds` (keep it under the unit's
  stop timeout); starting it starts every enabled service again. A drain
  (SIGUSR1) first wakes each owning bot ("Services stop in N s") and keeps the
  services running for `stop_warn_seconds`. No systemd unit or container per
  service: they live and die with the node. `[services]` in
  `config.rs` lists every knob and default. A **scheduled service** (`--cron
  "<5 fields>" [--timezone TZ] [--timeout 10m] [--no-catch-up]`, stored in
  `services.schedule`) is the node's crontab: the node runs it at each
  cron time, one run at a time (a run time during a run is skipped), stops a
  run past its timeout (SIGTERM, then SIGKILL) and reports its last exit,
  end and next run; a timeout or a non-zero exit is a `failed` service
  event. Run times missed while the node was down are not replayed: at most
  one catch-up run when it starts (`dispatch/services.rs`). Routines stay
  server-side and create tasks. Bot `/status`, `axe bot show` and the
  dashboard's Services page (per node, with recent logs) show each
  service's kind, state (crash-looping included), restarts and uptime, or
  for a scheduled one its last run, exit and next run
  (`Service::summary`, `web/lib/services.ts`).
- **Goals** (`axe goal`, `goal.rs`, `goal_board.rs`) are durable outcomes
  bots share, with weekly budgets and an append-only trajectory. Design:
  [docs/design/goals.md](../../../docs/design/goals.md).

Delivery statistics (`axe stats`, `flow.rs`) and run cost (`usage.rs`) are
computed from the event log and run usage; the only recorded classifications
are an escalation's class (`axe task escalate --class`) and a task's workload.
