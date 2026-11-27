---
title: "Architecture"
description: "The server, CLI, node and dashboard, and how the node selects, runs and supervises work."
---

| Piece | What it does | Code |
|---|---|---|
| Server | Sole owner of the database; HTTP API, change stream, routine and schedule ticks, Telegram gateway | `server.rs`, `db.rs` |
| CLI | HTTP client; rides out a server restart by retrying with one request id per write, so a write is applied once | `cli.rs`, `client.rs` |
| Node | Holds a lease on the server, claims ready work, supervises agent processes, relays their logs | `dispatch.rs`, `remote.rs`, `agent_proc.rs` |
| Dashboard | Read-only Next.js UI over the GET API | `web/` |

Paths in `crates/axe/src/` unless noted.

**Node.** `axe node` (formerly the dispatcher; `axe dispatch` still works,
and the config section may be `[dispatcher]` or `[node]`) is the server's
worker on one machine. Wire names keep `dispatcher` (the event actor,
`/api/dispatcher/*`, `dispatch:` claims, `dispatcher.key`) so an older
server or node keeps working with a newer one. One node at a time holds a
server-issued lease; its operations go through a journaled, replayable endpoint so a retry after an
outage is applied once (`remote.rs`). While the server is unreachable it
admits nothing and keeps workers running; a node that finds unresolved
runs refuses to start until `axe node --reconcile-note` escalates them.
Worker logs stay on the node host and the server relays reads over the
node's WebSocket without storing them (`run_log.rs`, `relay_log` in `server.rs`).
Beside each log the node keeps when it saw each line, the run's user
prompt and its system prompt or run instructions; the run page's details
(`/api/runs/<id>/meta`, `/api/bot-runs/<id>/meta`) join them with the run's
events and usage (`run_meta.rs`).

**Selection rules.** One resolver, `runtime_config.rs`, merges a run's
layers: the built-in default harness, the project (the node config's
limits and the most specific stored rule, `axe rule add/list/rm/log`,
`rule.rs`), the bot (its primary and backup) and the task (its `--agent`
pin); the node only applies runtime health to the result (`Dispatcher::pick`).
`axe rule explain --task N | --bot NAME` shows what a task or bot resolves to.
A rule matches workload, repo, goal, label (the project counts), bot and
owner; a bot sees only rules that name it. The node imports the config's
deprecated `[defaults]`/`[[rule]]` agent lists once
(`Config::rules_to_import`); one stored rules cannot express is the
resolver's config layer instead (`runtime_config::ConfigLayer`), with a
startup line saying why.

**Session continuity.** Every later run resumes the first run's session with
the agent's `resume_cmd`. A failed resume with no axe change
retries fresh in the same pass. Prompts are one line (`[prompts]`); the agent
reads the task itself.

**When the node escalates.** A session that exits with its task still
`in_progress`, a task over `max_runs`, a repo with no worktree base, a spawn
failure, an orphaned claim after restart, and a
SIGTERM. Each gets a block note. A run that a quota, login or provider
failure ended goes back to `pending` with a timed retry instead, its run
refunded and the class named in its note (`failure.rs`).

**Admission.** A task session starts only when a pool slot is free, the
optional resource gate is open, and no admission hold is set
(`admission.rs`). Bots have their own pool and are held only at the resource
gate's hard floor, so a full task queue never delays a bot's reply. Waiting
tasks stay `pending` and spend nothing. `axe admission` shows why.

**Node observability.** The node samples host CPU, memory, load, the run
volume's free disk and its running runs by kind (task runs per project, bot
runs per bot, services and scheduled service runs per bot) and sends them
with its admission report; the server keeps raw samples and 5-minute rollups,
pruned hourly (`[node_metrics]`, `node_metrics.rs`). The web Node view
charts them (`GET /api/node/metrics`) and the Telegram owner command `/node`
shows the latest sample.

**Rollouts.** SIGUSR1 starts a bounded drain: nothing new is admitted, and
after `drain_abort_seconds` unprotected task runs are stopped and requeued on
the same session. `axe task protect ID` marks the live run as inside a
non-repeatable step (merge, deploy, migration) so the drain waits for it;
`--clear` ends it. `axe task abort ID` force-stops a run remotely and exits zero
only once termination is confirmed. An admission hold
(`axe admission hold --until T`) gives a quiet queue without aborting
anything.

**Agent environment.** Sessions get `AXE_ACTOR=agent`, `AXE_SESSION`,
`AXE_TASK`, `AXE_SERVER`, `AXE_WORKTREE` and OpenTelemetry resource
attributes tagging the task, project, agent and run (`[otel]`). The token
reaches them through the inherited token file, not the environment.
