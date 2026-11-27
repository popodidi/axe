---
title: "Quickstart"
description: "One host, a loopback server, one task, using the built-in defaults."
---

One host, a loopback server, one task, using the built-in defaults.

1. **Install the agent CLI and log in** on the host that runs `axe node`
   (`claude` for the built-in agent; `axe config` prints its command). The
   node only spawns it; a missing binary escalates the task with
   `spawn "claude" failed`. To try axe without a model, use the `stub` agent
   from `axe init`.
2. **Config** (optional): `~/.config/axe/config.toml`. `axe init` writes a
   starter one with a `stub` agent (no model) and prints the commands for a
   first task (`STARTER` in `crates/axe/src/config.rs`). Agents are
   `[agents.<name>]` tables (`cmd`, `resume_cmd`, `session`, `harness`,
   `bot_args`; `Agent` in `crates/axe/src/config.rs`); bots need `bot_args`,
   which default for agents that run `claude` or `codex`.
   Repos are `[repos.<name>]` (`path`, `url`); a clone URL can instead be
   registered once on the server with `axe repo add <name> <url>`.
3. **Server**: `axe server`. On loopback it needs no token. It reads the
   config at start; `axe task add --agent` re-reads it for a name it does not
   know, so a new agent needs no restart (other config edits do).
4. **A repo** (optional): a Git clone at `~/workspace/<name>` (or a clone
   URL, from `axe repo add <name> <url>` or `[repos.<name>] url`, to clone on
   first use) with an `origin` remote whose `origin/HEAD` is set
   (`git remote set-head origin <branch>`; `worktree.rs`). Without it a run
   naming the repo escalates with "has no worktree base". A project is only a
   name: it needs no checkout.
5. **A task**: `axe task add --project <p> --title <t> [--repo <name>] --detail
   "## Outcome ..."` (add `--agent <name>` to pin one), then `axe node`
   (`--once` for one pass). Follow it with `axe task list --all` and `axe task log <id>`;
   an escalated task says why in its block note.

A bot works the same way: `axe bot create --file bot.json` (`name`,
`project`, `agent`, `identity`, `conduct`), `axe bot edit <name> --repo a,b`
for a multi-repo run folder, `axe bot edit <name> --agent codex-1 --why W`
to move it to another runtime (fresh session), `axe bot wake <name> --now --note ...`, then
`axe node`. The bot protocol is built in; bundles only add opinions.
Telegram is optional. The `axe doc put` commands in [Docs](../concepts/docs-store.md) read
`skills/` and `bundles/` from a clone of this repository; `cargo install`
leaves none, and the release skills tarball has `skills/axe` only. For a
local agent, `axe skill install` writes the binary's own copy of
`skills/axe` (see [Keeping axe and its skill current](install.md#keeping-axe-and-its-skill-current)).
