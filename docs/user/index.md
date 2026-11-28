---
title: "axe"
description: "A self-hosted task queue for coding agents."
---

A self-hosted task queue for coding agents. `axe server` owns a SQLite
database and serves an HTTP JSON API; `axe node` is a separate process
(optionally on another machine) that starts one headless coding-agent session
(Claude Code, Codex, or any command you configure) per ready task. Every
command other than `server`, including `node`, is an HTTP client of the
server.

These pages explain the concepts and where each lives in the code. The code
is the source of truth: `axe --help` lists the commands by primitive (task,
bot, goal, ...), store and ops, `axe <command> --help` lists every flag, `axe
config` prints the resolved configuration, and the modules they name hold
the exact rules. Task commands live under `axe task`; their older top-level
forms (`axe add`, `axe show`, ...) still work and are hidden from the help.

## How these docs are organised

- [Get started](getting-started/quickstart.md): every command from install
  to the point where your agent drives axe on its own.
- [Levels](levels/index.md): the mental model for how you work with agents,
  from Level 0 (skill) to Level 2 (autonomy).
- [Primitives](primitives/index.md): what axe gives you to get there: task,
  routine, goal, bot, service and store.
- [How it works](concepts/architecture.md), [Guides](guides/working-locally.md)
  and [Reference](reference/config.md): internals, how-tos and every setting.

## Design notes

- [docs/design/](../../docs/design/): why axe is shaped this way:
  [one agent per task](../../docs/design/one-agent-per-task.md),
  [bots](../../docs/design/bots.md), [goals](../../docs/design/goals.md) and the
  [dashboard design contract](../../docs/design/dashboard.md).
- [`skills/axe`](../../skills/axe/SKILL.md): the agent-facing guide (states,
  commands, the spec format), released with axe.
- [SECURITY.md](../../SECURITY.md): reporting and the threat model.
