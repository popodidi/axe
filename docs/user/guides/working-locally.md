---
title: "Working locally"
description: "Build the folder a run of a task or bot gets on your own machine."
---

Paths in `crates/axe/src/` unless noted.

**Working locally.** `axe workspace <task-id|bot-name> [--dir D]` builds, on
the machine it runs on, the folder a run of that task or bot gets, through
the node's own builder (`run_workspace.rs`, `Mode::Local`): the same
`AGENTS.md`, `skills/`, `docs/index.md`, skill links and `repos/<r>`
worktrees of this machine's clones (`[repos.<name>] path`, else
`<workspace_root>/<name>`, cloned on first use from the repo's clone URL:
`[repos.<name>] url`, else the server's repo registry, so a machine with no
`[repos]` entries works for any repo registered with `axe repo add`). The
default folder is `~/axe/task-<id>` or `~/axe/bot-<name>`. It claims nothing
and records no run, and refuses an `in_progress` task: hold it first. A
missing task branch starts from `origin` when a run pushed it; worktrees are
never reset. Attached docs and bundles are written in full under
`docs/store/<path>`, and `.axe/manifest.json` records the store version of
every store-backed file. A bot's folder also holds its system prompt in
`.axe/system.md`, for reference: you work there as yourself, not as the bot.
`axe workspace sync [D]` writes each edited store-backed file back to the
store, based on the version it was built from, then rebuilds the folder. When
the store moved on, the edit is kept, the store's copy is written next to it
as `<file>.store` and sync exits non-zero: merge, delete the copy and sync
again. Generated files and new files are never sent (create docs with
`axe doc put`), and repo changes go through git as usual (`workspace.rs`).
Both read the server without retries and give a host lookup 10s, so a network
failure ends the command at once and names the server and the step. A build
that did not finish (a failed clone or fetch, say) resumes when run again,
and `sync` there says so instead of syncing. `--force` rebuilds the generated
files and the repo worktrees from scratch (branches and commits stay); it
refuses, listing them, while a repo has uncommitted changes or a store-backed
file an unsynced edit. `-v` prints each step with its duration, `-vv` also
each git command and HTTP call.
Claude Code finds the root `AGENTS.md` from the folder and from
`repos/<r>`. Codex reads it only when started in the folder, since it stops
at the repo's git root; from `repos/<r>`, run
`codex -c 'project_root_markers=[".axe"]'`.
