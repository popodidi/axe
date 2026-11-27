---
title: "Runtimes"
description: "A runtime is a harness plus exactly one login on the node."
---

Paths in `crates/axe/src/` unless noted.

**Runtimes.** A runtime is a harness plus exactly one login on the
node (`runtime.rs`). With `[runtimes] dir`, each `<dir>/<name>/runtime.toml`
names its `harness`, optionally the `agent` template that drives it, a
`login_dir` (the CLI's own default, `~/.claude` or `~/.codex`, is adopted in
place: the node only sets `CLAUDE_CONFIG_DIR` or `CODEX_HOME` for any
other directory and never touches the login), `plan` and `disabled`. Rules
pick a harness; the node takes its least-used healthy runtime, skips to
the next `use` entry when none is healthy, and otherwise returns the task to
pending until one may be (run refunded). A stored session resumes on its own
runtime while it is healthy; sessions recorded before runtimes resolve to the
runtime on the default login. Quota, login and provider failures
(`failure.rs`) mark the runtime limited, needs_login or degraded
(`<name>/state.json`); a needs_login runtime is probed with `claude auth
status` / `codex login status` until it passes. A runtime with no
`state.json` yet (never checked) needs its login probe to pass before it
takes a run (no card); one that never passed a login and never failed a run
shows as "not logged in". The node reports each
runtime's state, last failure and quota with its admission report, never a
login directory or credential: `axe runtimes`, `GET /api/runtimes` and the
Telegram owner command `/runtimes` show them, and a runtime turning limited
or needs_login sends the owner one card (`[telegram] ops_bot`, default the
first bot). Without `[runtimes]`, every agent is a runtime on the default
login and always healthy.
