# axe

A self-hosted task queue for coding agents. `axe server` owns a SQLite
database and serves an HTTP JSON API; `axe dispatch` is a separate process
(optionally on another machine) that starts one headless coding-agent session
(Claude Code, Codex, or any command you configure) per ready task. Every
command other than `server`, including `dispatch`, is an HTTP client of the
server.

This README explains the concepts and where each lives in the code. The code
is the source of truth: `axe --help` and `axe <command> --help` list every
command and flag, `axe config` prints the resolved configuration, and the
modules named below hold the exact rules.

## Install

```sh
cargo install --locked --git https://github.com/popodidi/axe axe-os   # binary `axe`
```

The crate is `axe-os`; the binary and the library are `axe`. Cargo features
(`crates/axe/Cargo.toml`): `telegram` (default on) adds the Telegram gateway
for bots; `embed-ui` serves the dashboard from `web/` at `/` (build `web/`
first, see [web/README.md](web/README.md)); `ts` generates the dashboard's
TypeScript API types.

Releases (`.github/workflows/release.yml`) publish a server image, a Linux
binary tarball and the `skills/axe` tarball from a `vX.Y.Z` tag, with sha256
sums; verify with `gh release download <tag> -R popodidi/axe && sha256sum -c
SHA256SUMS`. Other platforms install from source with `--tag <tag>`.

## Not a workflow engine

A task has a project, a spec (`detail`) and a **state**, and optionally an
`owner` (the bot or person who follows it up; its creator stays the first
event), `repos` and free-form `labels` (`--owner`, `--repo`, `--label` on
`axe add` and `axe set`; filters treat the project as a label). Once
dispatched, one agent session carries it from start to `done` and is resumed
on every later run ([one agent per task](docs/design/one-agent-per-task.md)).
There are no pipelines or transitions in code: the spec is the workflow, and the
agent's own instructions (skills, `AGENTS.md`) say how to work a task. Every
mutation appends to an event log (`axe log ID`), so a resumed session can see
what changed.

| State | Meaning |
|---|---|
| `pending` | Eligible to run; the dispatcher claims it and starts or resumes its session |
| `in_progress` | A session is working on it |
| `escalated` | A hold for a human decision; resumes only when set back to `pending` (or `axe approve`) |
| `blocked` | A non-human external wait; also resumed explicitly |
| `done` | The completion contract: dependents start, so set it only when the spec's outcome holds |

Each task also has a derived `status`: `blocked` for a pending task still
waiting on prerequisites, otherwise its state. Human attention is
`axe list --state escalated`. Block notes use one format so a hold says what
to do: `blocked:` / `resolver:` / `resume:` lines (`axe escalate`).

Tasks may declare prerequisites (`blocked_by`, `axe deps`). A dependency wait
is never stored: a task is ready when it is `pending`, every prerequisite is
`done` and its `not_before` has passed, computed inside each claim
transaction. Holds and dependencies stay separate; finishing a prerequisite
never lifts a hold. A rejected prerequisite is removed (`axe deps --remove`),
never marked `done` to release its dependents. Code: `db.rs`.

The actor on each event is `--actor`, else `$AXE_ACTOR`, else `human`, with
`$AXE_SESSION` alongside. `axe approve` records an owner decision and resumes
a held task; dispatcher task sessions are refused, a guardrail rather than a
security boundary since agents share one token.

## Architecture

| Piece | What it does | Code |
|---|---|---|
| Server | Sole owner of the database; HTTP API, change stream, routine and schedule ticks, Telegram gateway | `server.rs`, `db.rs` |
| CLI | HTTP client; rides out a server restart by retrying with one request id per write, so a write is applied once | `cli.rs`, `client.rs` |
| Dispatcher | Holds a lease on the server, claims ready work, supervises agent processes, relays their logs | `dispatch.rs`, `remote.rs`, `agent_proc.rs` |
| Dashboard | Read-only Next.js UI over the GET API | `web/` |

Paths in `crates/axe/src/` unless noted.

**Dispatcher.** One dispatcher at a time holds a server-issued lease; its
operations go through a journaled, replayable endpoint so a retry after an
outage is applied once (`remote.rs`). While the server is unreachable it
admits nothing and keeps workers running; a dispatcher that finds unresolved
runs refuses to start until `axe dispatch --reconcile-note` escalates them.
Worker logs stay on the dispatcher host and the server relays reads over the
dispatcher's WebSocket without storing them (`run_log.rs`, `relay_log` in `server.rs`).

**Session continuity.** A task's first run starts fresh with the agent from
its `--agent` pin or the matching `[[rule]]`; every later run resumes that
session with the agent's `resume_cmd`. A failed resume with no axe change
retries fresh in the same pass. Prompts are one line (`[prompts]`); the agent
reads the task itself.

**When the dispatcher escalates.** A session that exits with its task still
`in_progress`, a task over `max_runs`, a missing project directory or
worktree base, a spawn failure, an orphaned claim after restart, and a
SIGTERM. Each gets a block note.

**Admission.** A task session starts only when a pool slot is free, the
optional resource gate is open, and no admission hold is set
(`admission.rs`). Bots have their own pool and are held only at the resource
gate's hard floor, so a full task queue never delays a bot's reply. Waiting
tasks stay `pending` and spend nothing. `axe admission` shows why.

**Rollouts.** SIGUSR1 starts a bounded drain: nothing new is admitted, and
after `drain_abort_seconds` unprotected task runs are stopped and requeued on
the same session. `axe protect ID` marks the live run as inside a
non-repeatable step (merge, deploy, migration) so the drain waits for it;
`--clear` ends it. `axe abort ID` force-stops a run remotely and exits zero
only once termination is confirmed. An admission hold
(`axe admission hold --until T`) gives a quiet queue without aborting
anything.

**Task worktrees.** With `[worktrees]`, every session of a task runs in one
reused Git worktree of the project checkout
(`<workspace_root>/.worktrees/<project>/task-<id>--work`, `$AXE_WORKTREE`).
It is never reset; housekeeping removes it after the task is done only when
nothing in it could be lost (`worktree.rs`, `housekeeping.rs`;
`axe housekeeping --dry-run` prints each decision).

**Run workspaces.** A task with `repos` (even none, `--no-repos`) runs in
`<run_root>/task-<id>` instead (default `<workspace_root>/.runs`): one worktree
per repo under `repos/<name>`, all on the task branch, plus a generated
`AGENTS.md` (the task contract and an index), `skills/` linked from each
repo's `.agents/skills` and a `docs/index.md`, plus attached docs (see Run
context below). A repo's canonical clone is
`[repos.<name>] path`, default `<workspace_root>/<name>` (`run_workspace.rs`).
Tasks without `repos` keep the project worktree. Housekeeping does not remove
run workspaces yet.

**Agent environment.** Sessions get `AXE_ACTOR=agent`, `AXE_SESSION`,
`AXE_TASK`, `AXE_SERVER`, `AXE_WORKTREE` and OpenTelemetry resource
attributes tagging the task, project, agent and run (`[otel]`). The token
reaches them through the inherited token file, not the environment.

## Routines, bots and goals

These sit next to tasks with their own tables, API and CLI namespace; task
code gains no branches for them.

- **Routines** (`axe routine`, `routine.rs`) create ordinary pending tasks on a
  five-field cron in an IANA timezone. After downtime only the latest missed
  occurrence is created; manual triggers take a retry key. Each task keeps a
  snapshot of the template it came from.
- **Bots** (`axe bot`, `bot.rs`, `dispatch/bots.rs`) are persistent
  identities with one agent session resumed on every wake. Wakes come from the
  bot itself, named cron schedules (`schedule.rs`), task-event subscriptions
  (`subscription.rs`) and the Telegram gateway (`telegram.rs`, `board.rs`).
  Design: [docs/design/bots.md](docs/design/bots.md).
- **Goals** (`axe goal`, `goal.rs`, `goal_board.rs`) are durable outcomes
  bots share, with weekly budgets and an append-only trajectory. Design:
  [docs/design/goals.md](docs/design/goals.md).

Delivery statistics (`axe stats`, `flow.rs`) and run cost (`usage.rs`) are
computed from the event log and run usage; the only recorded classifications
are an escalation's class (`axe escalate --class`) and a task's workload.

## Blobs

`axe blob put/get/ls/link` (`blob.rs`) keeps immutable files on the server,
named by the sha256 of their bytes, so the same bytes are stored once. There
is no delete, overwrite or version. Contents live on local disk next to the
database, or in an S3-compatible bucket with `[blobs.s3]`. `axe blob link`
signs a URL (`/b/<sha256>/<name>?exp=..&sig=..`, at most seven days) that
opens without the token, for example from a phone; it renders inline in a
sandbox. The signing key is created on first start; `[blobs]` sets the
directory, size limit, link base URL and key file.

## Docs

`axe doc put/get/ls/log/diff/attach/detach` (`doc.rs`) keeps versioned text
documents on the server: skills, docs, memory, conduct and bundles. A doc is a
path (`notes/plan.md`); a folder of docs is a path too, so a skill is a folder
with `SKILL.md` and its files, and a bundle is any folder. Every change is a
commit in a bare git repository (`docs.git` next to the database, or
`[docs] dir`), so `log` and `diff` show a doc's history; the server needs
`git` on its `PATH`. Docs attach to one of four scopes: `global`,
`label:<name>`, `owner:<name>` or `repo:<name>`; `axe doc ls --scope` lists
what a scope has. A write may name the version it edits (`--base`, or
`--new` for a new doc) and is refused with a conflict if the doc moved since.
Back the repository up with git (`git clone --mirror`); `axe backup` covers
the database only.

**Run context.** When the dispatcher builds a run workspace it collects the
docs attached to the task's scopes: each repo, the owner, each label (the
project first) and `global`. A conflict (the same skill name, or the same path
on two scopes) goes to the more specific scope: repo > owner > label > global.
Paths under `policy/` and a repo's own `.agents/skills` are never overridden.
Only names and descriptions go into the run: each attached skill becomes an
index entry `skills/<name>/SKILL.md` (found through `.claude/skills` and
`.agents/skills`), each doc a line in `docs/index.md`; bodies load on use with
`axe doc get <path>`. A bot's system prompt lists what is attached to
`owner:bot:<name>`, its project and `global`. Nothing attached means nothing
changes.

**Bundles.** A bundle's `prompt.md` is text a bot carries in its system
prompt: the bot inlines it, ordered by its `order:` front matter, before its
identity and conduct. The bot protocol and the Telegram channel protocol ship
as bundles in [`bundles/`](bundles/README.md); load and attach them
(`axe doc put bundles/axe-bot bundles/axe-bot`, then `axe doc attach`) to give
bots the protocol. A bot with no bundles runs on its identity and conduct
alone. `axe context explain <task>` prints each candidate, its scope and why
it was included or skipped (`context.rs`).

## Configuration

One TOML file, `$AXE_CONFIG` (default `~/.config/axe/config.toml`; built-in
defaults when missing), read once at startup by both `axe server` and
`axe dispatch`. The server uses `[server]`, `[telegram]`, `[metrics]`,
`[blobs]`, `[docs]` and the agent names; the dispatcher uses the rest, so keep agent
names aligned when they run on different machines. Unknown keys fail validation.
`crates/axe/src/config.rs` defines every table, default and check;
`axe config` validates and prints the resolved file, including the built-in
`claude` agent.

| What | Default | Override |
|---|---|---|
| Database (server) | `~/.local/share/axe/axe.db` | `$AXE_DB` |
| Bearer token | `~/.config/axe/token` | `$AXE_TOKEN_FILE`; clients also take `$AXE_TOKEN` |
| Server address (clients) | `http://127.0.0.1:8790` | `$AXE_SERVER` |
| Listen address | `127.0.0.1:8790` | `axe server --listen` |
| Admission hold (dispatcher host) | `~/.local/state/axe/admission-hold.json` | `$AXE_ADMISSION_HOLD` |
| Agent logs and dispatcher journal | `~/.local/state/axe/logs` | `log_dir` |

The database uses WAL: back it up with `axe backup PATH`, a consistent
snapshot from the running server, never by copying the file. Schema upgrades
run on server start (`db.rs`); back up before upgrading.

## HTTP API

`GET` is unauthenticated, except Telegram message content, `/api/blobs`,
`/api/docs` and
the dispatcher connection (a signed `/b/` link needs only its signature);
every other method needs `Authorization: Bearer <token>`. The server refuses
a non-loopback listen address without a token file. Expose it only on a
trusted network: reads include full agent logs. Bodies are JSON, errors are
`{"error": "..."}`, and `X-Axe-Request-Id` makes a write retry-safe.

The route table is `route`, `bot_route`, `goal_route`, `doc_route` and `blob_api` in
`crates/axe/src/server.rs`; dispatcher operations are in `remote.rs`. Response
shapes are the serialized Rust types, exported to `web/lib/api.ts`. There is
no version prefix.

## Development

```sh
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
cargo test --locked --features ts --test ts_bindings   # web/lib/api.ts is current
```

CI (`.github/workflows/ci.yml`) runs these on pull requests, plus the `web`
job when `scripts/ci_scope.py` says the dashboard is affected; the release
gate runs the full matrix. Dashboard checks: [web/README.md](web/README.md).

## Docs

- [docs/design/](docs/design/): why axe is shaped this way:
  [one agent per task](docs/design/one-agent-per-task.md),
  [bots](docs/design/bots.md), [goals](docs/design/goals.md) and the
  [dashboard design contract](docs/design/dashboard.md).
- [`skills/axe`](skills/axe/SKILL.md): the agent-facing guide (states,
  commands, the spec format), released with axe.
- [SECURITY.md](SECURITY.md): reporting and the threat model.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in axe by
you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
