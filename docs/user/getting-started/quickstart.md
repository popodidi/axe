---
title: "Quickstart"
description: "Every command from install to the point where your agent drives axe on its own: Level 0 on one machine."
---

One machine, a loopback server, the built-in defaults. At the end your
coding agent has the axe skill and queues, runs and follows tasks itself:
[Level 0](../levels/level-0-skill.md). Each step names the command;
`axe <command> --help` has every flag.

## 1. Install axe

Needs a Rust toolchain ([rustup](https://rustup.rs)) and `git`.

```sh
cargo install --locked --git https://github.com/popodidi/axe axe-os   # binary `axe`
axe --version
```

Release binaries (Linux x86_64, macOS arm64) and checksums are on the
[releases page](https://github.com/popodidi/axe/releases); see
[Install](install.md).

## 2. Write a config

```sh
axe init      # writes ~/.config/axe/config.toml with a `stub` agent (no model)
axe config    # validates it and prints the resolved config, built-in `claude` agent included
```

## 3. Start the server

In its own terminal, and leave it running. On loopback it needs no token.

```sh
axe server    # http://127.0.0.1:8790, database in ~/.local/share/axe/axe.db
```

## 4. Run a first task, without a model

The `stub` agent notes where it ran and marks its task done, so this checks
the whole loop before any model is involved.

```sh
mkdir -p ~/workspace/demo
axe task add --project demo --title "first task" --agent stub   # prints the task id
axe node --once      # claims the task and runs one pass
axe task list --all  # 1  demo  done  1  first task
axe task log 1       # created, claimed, the stub's note, done
```

## 5. Install and log in to your agent

Install [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and run
`claude` once to log in. This also creates `~/.claude`, which the next step
links the skill into. Codex or any other command works too: add it as an
`[agents.<name>]` table in the config ([Configuration](../reference/config.md)).

## 6. Give your agent the axe skill

```sh
axe skill install   # writes ~/.agents/skills/axe, links ~/.claude/skills/axe
axe skill status    # up to date (<version>)
```

The skill is built into the binary and teaches the agent the task states,
the commands and the spec format. Re-run `axe skill install` after an
upgrade, or let `axe upgrade` do it.

## 7. Let your agent drive

Keep the node running so it picks up new work (it starts a `claude` session
for each task not pinned to another agent):

```sh
axe node
```

Then, in your agent, ask in plain words, for example:

> Use the axe skill. Queue a task in project demo: add a README with one
> line describing the project. Then follow it until it is done and show me
> the log.

From here your agent queues tasks with an `## Outcome` and a `## Verify`,
the node runs each one in its own session, and you read results with
`axe task list` and `axe task log <id>`. Anything that needs you shows up in
`axe task list --state escalated`.

## Next

- To work on a Git repo, register it: `axe repo add <name> <url>` (or a
  clone at `~/workspace/<name>` whose `origin/HEAD` is set), then
  `axe task add --repo <name> …`. Without it a run naming the repo
  escalates with "has no worktree base".
- Learn the [level model](../levels/index.md) and the
  [primitives](../primitives/index.md).
- Off loopback (a node on another machine) the server needs a bearer
  token: see [Configuration](../reference/config.md).
