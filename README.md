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
SHA256SUMS`, or without a GitHub login `curl -LO
https://github.com/popodidi/axe/releases/download/<tag>/<asset>`. Other
platforms install from source with `--tag <tag>`.

This README describes `main`, and releases can lag it: a release may lack
commands or flags named here (`axe <command> --help` is authoritative for the
binary you run). A `main` build may still report the last release's version
in `axe --version`.

## Quickstart

One host, a loopback server, one task, using the built-in defaults.

1. **Install the agent CLI and log in** on the host that runs `axe dispatch`
   (`claude` for the built-in agent; `axe config` prints its command). The
   dispatcher only spawns it; a missing binary escalates the task with
   `spawn "claude" failed`. To try axe without a model, declare a stub agent
   whose command is any script that ends with `axe set "$AXE_TASK" --state done`.
2. **Config** (optional): `~/.config/axe/config.toml`. Agents are
   `[agents.<name>]` tables (`cmd`, `resume_cmd`, `session`, `harness`,
   `bot_args`; `Agent` in `crates/axe/src/config.rs`); bots need `bot_args`.
   Repos are `[repos.<name>]` (`path`, `url`).
3. **Server**: `axe server`. On loopback it needs no token. It reads the
   config once, and `axe add --agent` checks names against the server's
   agents, so restart the server after adding an agent.
4. **A repo**: a Git clone at `~/workspace/<name>` (or `[repos.<name>] url`
   to clone on first use) with an `origin` remote whose `origin/HEAD` is set
   (`git remote set-head origin <branch>`; `worktree.rs`). Without it the run
   escalates with "has no worktree base".
5. **A task**: `axe add --project <p> --title <t> --repo <name> --detail
   "## Outcome ..."` (add `--agent <name>` to pin one), then `axe dispatch`
   (`--once` for one pass). Follow it with `axe list --all` and `axe log <id>`;
   an escalated task says why in its block note.

A bot works the same way: `axe bot create --file bot.json` (`name`,
`project`, `agent`, `identity`, `conduct`), `axe bot edit <name> --repo a,b`
for a multi-repo run folder, `axe bot wake <name> --now --note ...`, then
`axe dispatch`. Telegram is optional. The `axe doc put` commands below read
`skills/` and `bundles/` from a clone of this repository; `cargo install`
leaves none, and the release skills tarball has `skills/axe` only.

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

**Selection rules.** A task's first run starts fresh with the agent from its
`--agent` pin, or else from the stored rule that matches it (`axe rule
add/list/rm/log`, `rule.rs`). A rule matches any of workload, repo, goal,
label (the project counts), bot and owner; the most specific matching rule
wins, then the higher priority, then the newer rule. Its ordered `use` list
of `harness[:model[:effort]]` (`claude-code`, `codex`) is walked down to the
first harness the dispatcher has an agent for (an agent's `harness`, or
inferred from its command), least used first; the model and effort go to the
CLI, and the task gets a `runtime:<name>` label. A bot runs its own agent
unless a rule names it (`bot=`). Every edit is a logged event. The
dispatcher imports the config's `[defaults]`/`[[rule]]` agent lists once
(`Config::rules_to_import`); those keys are deprecated and keep picking only
when stored rules cannot express them (an agent with no harness, two agents
of one harness, round-robin), with a startup line saying why.

**Runtimes.** A runtime is a harness plus exactly one login on the
dispatcher (`runtime.rs`). With `[runtimes] dir`, each `<dir>/<name>/runtime.toml`
names its `harness`, optionally the `agent` template that drives it, a
`login_dir` (the CLI's own default, `~/.claude` or `~/.codex`, is adopted in
place: the dispatcher only sets `CLAUDE_CONFIG_DIR` or `CODEX_HOME` for any
other directory and never touches the login), `plan` and `disabled`. Rules
pick a harness; the dispatcher takes its least-used healthy runtime, skips to
the next `use` entry when none is healthy, and otherwise returns the task to
pending until one may be (run refunded). A stored session resumes on its own
runtime while it is healthy; sessions recorded before runtimes resolve to the
runtime on the default login. Quota, login and provider failures
(`failure.rs`) mark the runtime limited, needs_login or degraded
(`<name>/state.json`); a needs_login runtime is probed with `claude auth
status` / `codex login status` until it passes. The dispatcher reports each
runtime's state, last failure and quota with its admission report, never a
login directory or credential: `axe runtimes`, `GET /api/runtimes` and the
Telegram owner command `/runtimes` show them, and a runtime turning limited
or needs_login sends the owner one card (`[telegram] ops_bot`, default the
first bot). Without `[runtimes]`, every agent is a runtime on the default
login and always healthy.

**Logins from Telegram.** `/runtimes` has a *Log in* button per registered
runtime (`runtime::login_methods`):

- **Claude Code.** A `claude setup-token` token or an API key arrives in a
  sealed form (see below). It is kept as `<name>/login/CLAUDE_CODE_OAUTH_TOKEN`
  or `ANTHROPIC_API_KEY` (0600) and set in the runtime's environment, where it
  takes precedence over the login directory.
- **Codex.** An API key arrives in a sealed form and goes to `codex login
  --with-api-key` on stdin. Or `codex login --device-auth` runs on the
  dispatcher, and its link and code are relayed to the owner.

A runtime that needs a login is ok again at its next login check.

**Session continuity.** Every later run resumes the first run's session with
the agent's `resume_cmd`. A failed resume with no axe change
retries fresh in the same pass. Prompts are one line (`[prompts]`); the agent
reads the task itself.

**When the dispatcher escalates.** A session that exits with its task still
`in_progress`, a task over `max_runs`, a missing project directory or
worktree base, a spawn failure, an orphaned claim after restart, and a
SIGTERM. Each gets a block note. A run that a quota, login or provider
failure ended goes back to `pending` with a timed retry instead, its run
refunded and the class named in its note (`failure.rs`).

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
When that clone is missing and the repo has `[repos.<name>] url`, the
dispatcher clones it there on first use, with the host's own Git credentials
(never a token in config), before building the worktree; concurrent first uses
clone once, and the run's `run_start` event lists it in `repos_cloned`
(`worktree::ensure_clone`). A missing clone without `url` stops the run with
that reason.
Tasks without `repos` keep the project worktree. A bot with a repo list
(`axe bot edit <name> --repo <r>`, `--no-repos` clears it) runs in
`<run_root>/bot-<name>` the same way, on branch `bot-<name>--work`; a bot
without one keeps its project worktree. `axe bot edit <name> --project <p>`
moves a bot to another project (its subscriptions keep the projects they
name), and `axe goal edit <name> --project <p>` moves a goal. Before each bot run the dispatcher
fetches and fast-forwards each of its repo worktrees to the base branch's
origin commit when it is clean and has no local commits; otherwise it leaves it
unchanged and records why in the run's `run_start` event (`repos_refresh`). Housekeeping removes a task's run
folder like its worktree: after the task is done for the retention, each repo
worktree is removed with `git worktree remove` and pruned, and only when the
folder holds nothing but what axe generates. Bot run folders stay.

**Agent environment.** Sessions get `AXE_ACTOR=agent`, `AXE_SESSION`,
`AXE_TASK`, `AXE_SERVER`, `AXE_WORKTREE` and OpenTelemetry resource
attributes tagging the task, project, agent and run (`[otel]`). The token
reaches them through the inherited token file, not the environment.

## Routines, bots and goals

These sit next to tasks with their own tables, API and CLI namespace; task
code gains no branches for them.

- **Routines** (`axe routine`, `routine.rs`) create ordinary pending tasks on a
  five-field cron in an IANA timezone. After downtime only the latest missed
  occurrence is created; manual triggers take a retry key. A template can set
  the created task's owner, repos and labels like `axe add` (`routine::Template`).
  Each task keeps a snapshot of the template it came from.
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

## Sealed forms

`axe form ask` (`sealed.rs`, `form.rs`) asks the owner for values (secret,
text, code, choice and file fields) that only the dispatcher can read, for
any task or bot session:

```sh
axe form ask --bot ops --purpose "Deploy token" --field token:secret:required:env=DEPLOY_TOKEN --wait
```

- `--bot` names the Telegram bot that sends the owner the prompt; it must be
  able to write to the owner's private chat. It is required unless
  `--no-telegram`, which only prints the link. `/runtimes` login forms come
  from the bot the owner typed `/runtimes` in.
- The prompt's "Open sealed form" button is a reply keyboard. Once the form
  is delivered, rejected for good or expired, the bot tells the owner the
  outcome in that chat and removes the keyboard, unless another open form's
  button is there (`Forms::take_closing` in `form.rs`, `form_reply` in
  `telegram.rs`).
- To test forms (CI, post-deploy checks), do not prompt the owner: use
  `axe form ask --no-telegram`, then submit a reply sealed to the printed
  link's spec with `axe form submit` (or through the link's page). No
  message reaches Telegram; the test in `crates/axe/tests/telegram.rs`
  (`a_finished_form_removes_its_button_and_no_telegram_forms_send_nothing`)
  pins this.

- The dispatcher signs the form with its key (`dispatcher.key`, P-256, 0600,
  made on first start next to the config file; `axe form key` prints its
  fingerprint, as do `axe runtimes`, `/runtimes` and every form). Compare it
  once.
- The link opens a static page (`[forms] page_url`, default
  [`DEFAULT_FORM_PAGE`](crates/axe/src/config.rs)) with the signed spec in its
  URL fragment. The page encrypts the values in the browser to the dispatcher
  key and returns `sealed.v1.<form>.<ciphertext>`: through the Telegram Mini
  App button the server sends the owner, pasted into the chat, a `.sealed`
  file, or `axe form submit`.
- The server keeps forms and ciphertext in memory only, until the dispatcher
  takes the reply (or 15 minutes pass), and records only names and outcomes.
  The dispatcher checks its signature, the expiry and one use, decrypts, and
  writes each value as a 0600 file in the target: the task's or bot's
  `$AXE_SECRETS_DIR` (`<run_root>/task-<id>/.secrets`, outside `repos/`;
  `env=VAR` fields are also set on its later runs), or a git credential for
  `--git <host>` that the dispatcher's clones fall back to
  (`[forms] git_credentials`).

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
Each attached skill is written whole into `skills/<name>/` (`SKILL.md` and
its scripts, references and tests; found through `.claude/skills` and
`.agents/skills`); each doc is a line in `docs/index.md` whose body loads on
use with `axe doc get <path>` (`context.rs`, `write_skills`). A bot's system
prompt lists what is attached to `owner:bot:<name>`, its project and
`global`; a bot with repos also gets those skills in its run folder. Nothing
attached means nothing changes.

**The axe skill.** [`skills/axe`](skills/axe/SKILL.md) loads into the store
like any skill, so runs get it from there instead of a machine install:
`axe doc put skills/axe skills/axe`, then `axe doc attach skills/axe global`.

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
