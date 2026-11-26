# axe

A self-hosted task queue for coding agents. `axe server` owns a SQLite
database and serves an HTTP JSON API; `axe node` is a separate process
(optionally on another machine) that starts one headless coding-agent session
(Claude Code, Codex, or any command you configure) per ready task. Every
command other than `server`, including `node`, is an HTTP client of the
server.

This README explains the concepts and where each lives in the code. The code
is the source of truth: `axe --help` lists the commands by primitive (task,
bot, goal, ...), store and ops, `axe <command> --help` lists every flag, `axe
config` prints the resolved configuration, and the modules named below hold
the exact rules. Task commands live under `axe task`; their older top-level
forms (`axe add`, `axe show`, ...) still work and are hidden from the help.

## Install

```sh
cargo install --locked --git https://github.com/popodidi/axe axe-os   # binary `axe`
```

The crate is `axe-os`; the binary and the library are `axe`. Cargo features
(`crates/axe/Cargo.toml`): `telegram` (default on) adds the Telegram gateway
for bots; `embed-ui` serves the dashboard from `web/` at `/` (build `web/`
first, see [web/README.md](web/README.md)); `ts` generates the dashboard's
TypeScript API types.

Releases (`.github/workflows/release.yml`) publish a server image, Linux
x86_64 and macOS arm64 binary tarballs and the `skills/axe` tarball from a
`vX.Y.Z` tag, with sha256 sums; verify with `gh release download <tag> -R popodidi/axe && sha256sum -c
SHA256SUMS`, or without a GitHub login `curl -LO
https://github.com/popodidi/axe/releases/download/<tag>/<asset>`. Other
platforms install from source with `--tag <tag>`.

This README describes `main`, and releases can lag it: a release may lack
commands or flags named here (`axe <command> --help` is authoritative for the
binary you run). `axe --version` names the commit after the version (`axe
0.4.0 (56305982f192)`) when the build knew it (`crates/axe/build.rs`), so a
`main` build is distinct from the release it follows.

### Keeping axe and its skill current

Every binary embeds the agent skill in `skills/axe` (`crates/axe/build.rs`).
`axe skill install` writes it to `~/.agents/skills/axe` (`--dir` elsewhere),
links `~/.claude/skills/axe` to it when `~/.claude` exists and that path is
free, and stamps the copy (`.axe-skill.json`: the binary's version and a hash
of the files). Re-run it to refresh the skill without upgrading the binary;
`axe skill status` compares the copy with the binary. Any other command
rewrites a stamped, unedited copy at the default path that differs from the
binary, with one line on stderr. A copy that is unstamped or edited since it
was stamped is never replaced automatically: it gets a hint at most once a
day, and `axe skill install --force` replaces it.

`axe upgrade` replaces the running binary with the latest release
(`--version vX.Y.Z` for another), checking the tarball against the release's
sha256, then runs the new binary's `axe skill install`. `axe upgrade --check`
only reports. A platform without a release binary gets the `cargo install
--tag` command (`--cargo` runs it). On a terminal, commands check the latest
release about once a day (cached under the user cache dir, `~/.cache/axe` on
Linux) and print a hint when it is newer; never in an agent session
(`$AXE_TASK` or `$AXE_BOT` set), and a network error only skips the hint.
`skill.rs` and `upgrade.rs` hold the rules. To switch either off:

```toml
[cli]
skill_refresh = false   # or AXE_SKILL_REFRESH=0
update_check = false    # or AXE_UPDATE_CHECK=0
```

## Quickstart

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
   `bot_args`; `Agent` in `crates/axe/src/config.rs`); bots need `bot_args`.
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
for a multi-repo run folder, `axe bot wake <name> --now --note ...`, then
`axe node`. The bot protocol is built in; bundles only add opinions.
Telegram is optional. The `axe doc put` commands below read
`skills/` and `bundles/` from a clone of this repository; `cargo install`
leaves none, and the release skills tarball has `skills/axe` only. For a
local agent, `axe skill install` writes the binary's own copy of
`skills/axe` (see [Keeping axe and its skill current](#keeping-axe-and-its-skill-current)).

## Not a workflow engine

A task has a project, a spec (`detail`) and a **state**, and optionally an
`owner` (the bot or person who follows it up; its creator stays the first
event), `repos` and free-form `labels` (`--owner`, `--repo`, `--label` on
`axe task add` and `axe task set`; filters treat the project as a label). Once
dispatched, one agent session carries it from start to `done` and is resumed
on every later run ([one agent per task](docs/design/one-agent-per-task.md)).
There are no pipelines or transitions in code: the spec is the workflow, and the
agent's own instructions (skills, `AGENTS.md`) say how to work a task. Every
mutation appends to an event log (`axe task log ID`), so a resumed session can see
what changed.

| State | Meaning |
|---|---|
| `pending` | Eligible to run; the node claims it and starts or resumes its session |
| `in_progress` | A session is working on it |
| `escalated` | A hold for a human decision; resumes only when set back to `pending` (or `axe task approve`) |
| `blocked` | A non-human external wait; also resumed explicitly |
| `done` | The completion contract: dependents start, so set it only when the spec's outcome holds |

Each task also has a derived `status`: `blocked` for a pending task still
waiting on prerequisites, otherwise its state. Human attention is
`axe task list --state escalated`. Block notes use one format so a hold says what
to do: `blocked:` / `resolver:` / `resume:` lines (`axe task escalate`).

Tasks may declare prerequisites (`blocked_by`, `axe task deps`). A dependency wait
is never stored: a task is ready when it is `pending`, every prerequisite is
`done` and its `not_before` has passed, computed inside each claim
transaction. Holds and dependencies stay separate; finishing a prerequisite
never lifts a hold. A rejected prerequisite is removed (`axe task deps --remove`),
never marked `done` to release its dependents. Code: `db.rs`.

The actor on each event is `--actor`, else `$AXE_ACTOR`, else `human`, with
`$AXE_SESSION` alongside. `axe task approve` records an owner decision and resumes
a held task; node task sessions are refused, a guardrail rather than a
security boundary since agents share one token.

## Architecture

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

**Selection rules.** A task's first run starts fresh with the agent from its
`--agent` pin, or else from the stored rule that matches it (`axe rule
add/list/rm/log`, `rule.rs`). A rule matches any of workload, repo, goal,
label (the project counts), bot and owner; the most specific matching rule
wins, then the higher priority, then the newer rule. Its ordered `use` list
of `harness[:model[:effort]]` (`claude-code`, `codex`) is walked down to the
first harness the node has an agent for (an agent's `harness`, or
inferred from its command), least used first; the model and effort go to the
CLI, and the task gets a `runtime:<name>` label. A bot runs its own agent
unless a rule names it (`bot=`). Every edit is a logged event. The
node imports the config's `[defaults]`/`[[rule]]` agent lists once
(`Config::rules_to_import`); those keys are deprecated and keep picking only
when stored rules cannot express them (an agent with no harness, two agents
of one harness, round-robin), with a startup line saying why.

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
status` / `codex login status` until it passes. The node reports each
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
  node, and its link and code are relayed to the owner.

A login made this way is checked at once (`runtime::Registry::check_now`): the
runtime is ok again when the check passes, and the owner gets a runtime card
with the outcome. A runtime that needs a login is also rechecked every few
minutes (`runtime::PROBE_EVERY`). Once a runtime is ok again its open cards
close: their buttons go and the text gains a resolved line
(`telegram::close_runtime_card`).

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

**Rollouts.** SIGUSR1 starts a bounded drain: nothing new is admitted, and
after `drain_abort_seconds` unprotected task runs are stopped and requeued on
the same session. `axe task protect ID` marks the live run as inside a
non-repeatable step (merge, deploy, migration) so the drain waits for it;
`--clear` ends it. `axe task abort ID` force-stops a run remotely and exits zero
only once termination is confirmed. An admission hold
(`axe admission hold --until T`) gives a quiet queue without aborting
anything.

**Run workspaces.** Every session of a task runs in one reused folder,
`<run_root>/task-<id>` (`[worktrees] run_root`, default
`<workspace_root>/.runs`; `$AXE_WORKTREE`), generated by the node
(`run_workspace.rs`). Nothing needs a project checkout. It holds one worktree
per repo the task names (`--repo`; none when omitted) under `repos/<name>`,
all on the task branch `<branch_prefix>task-<id>--work`, plus a generated
`AGENTS.md` (the task contract and an index), `skills/` linked from each
repo's `.agents/skills` and a `docs/index.md`, plus attached docs (see Run
context below). A repo's canonical clone is
`[repos.<name>] path`, default `<workspace_root>/<name>` (`run_workspace.rs`).
When that clone is missing, the node clones it there on first use from
its clone URL, with the host's own Git credentials (never a token in config
or on the server), before building the worktree; concurrent first uses clone
once, and the run's `run_start` event lists it in `repos_cloned`
(`worktree::ensure_clone_from`). The URL is `[repos.<name>] url` when this
machine's config sets one, else the server's repo registry
(`worktree::clone_url`, `repo.rs`): `axe repo add <name> <url>
[--default-branch B]`, `axe repo ls`, `axe repo show <name>`, `axe repo rm
<name>` (writes need the token; a URL with embedded credentials or a leading
`-` is refused, as in config). `path` stays local. A missing clone with no URL
stops the run with that reason, pointing at `axe repo add`; when the registry
cannot be read, the reason says so.
Worktrees are never reset. A task an older node started in
`<workspace_root>/.worktrees/<project>/task-<id>--work` (no repos) resumes
there while that folder exists; nothing creates one any more. A bot runs in
`<run_root>/bot-<name>` the same way, with its repo list
(`axe bot edit <name> --repo <r>`, `--no-repos` clears it), on branch
`bot-<name>--work`. `axe bot edit <name> --project <p>`
moves a bot to another project (its subscriptions keep the projects they
name), and `axe goal edit <name> --project <p>` moves a goal. A bot run
fetches nothing before it starts (a new worktree is fetched once, when it is
created); the run folder's `AGENTS.md` shows how long ago each repo was synced
(`repos/axe: synced 2h ago`), and the bot protocol has the bot run `axe repo
sync <repo>` before reading or changing a repo's code. That fetches and
fast-forwards `$AXE_WORKTREE/repos/<repo>` to the base branch's origin commit
when it is clean and has no local commits, and records the time in the
worktree's Git directory; otherwise it changes nothing and fails with why.
`axe workspace <bot>` still refreshes each worktree that way. Housekeeping removes a task's run
folder after the task is done for the retention: each repo worktree is
removed with `git worktree remove` and pruned, and only when the folder holds
nothing but what axe generates (`housekeeping.rs`; `axe housekeeping
--dry-run` prints each decision). Bot run folders stay.

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
  the created task's owner, repos and labels like `axe task add` (`routine::Template`).
  Each task keeps a snapshot of the template it came from. Cost controls on the
  template (`pre_check`, `model`, `effort`, `skills`) are read from that
  snapshot by the node (`dispatch/pre_check.rs`,
  [skills/axe/references/routines.md](skills/axe/references/routines.md)).
- **Bots** (`axe bot`, `bot.rs`, `dispatch/bots.rs`) are persistent
  identities with one agent session resumed on every wake. Wakes come from the
  bot itself, named cron schedules (`schedule.rs`), task-event subscriptions
  (`subscription.rs`) and the Telegram gateway (`telegram.rs`, `board.rs`).
  Design: [docs/design/bots.md](docs/design/bots.md).
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
  [docs/design/goals.md](docs/design/goals.md).

Delivery statistics (`axe stats`, `flow.rs`) and run cost (`usage.rs`) are
computed from the event log and run usage; the only recorded classifications
are an escalation's class (`axe task escalate --class`) and a task's workload.

## Blobs

`axe blob put/get/ls/link` (`blob.rs`) keeps immutable files on the server,
named by the sha256 of their bytes, so the same bytes are stored once. There
is no delete, overwrite or version. Contents live on local disk next to the
database, or in an S3-compatible bucket with `[blobs.s3]`. `axe blob link`
signs a URL (`/b/<sha256>/<name>?exp=..&sig=..`, at most seven days) that
opens without the token, for example from a phone; it renders inline in a
sandbox. The signing key is created on first start; `[blobs]` sets the
directory, size limit, link base URL and key file. `[blobs] link_url` moves
links to a separate site (a host that is not a subdomain of the server's):
there blob HTML may run scripts in an opaque-origin sandbox and load
scripts, styles, fonts and images over https, but cannot fetch, connect or
post (`connect-src 'none'`, `form-action 'none'`); the host
answers only `/b/` links and never a session, and `/b/` links on the
server's own host redirect to it.

The dashboard's Store view lists and searches blobs and docs; a selected
item shows its metadata and an "Open full" link (the signed `/b/` URL, or
`/doc?path=` for a doc) beside a preview. A task's
artifacts preview store links inline (images, text, Markdown, sandboxed
HTML), re-signing an expired link from its sha256. Older artifact URLs open
too when a link-map doc (any doc path ending in `.links.tsv`, with `url` and
`sha256` columns; `blob_refs.rs`) maps them to a blob.

## Sealed forms and messages

`axe form ask` (`sealed.rs`, `form.rs`) asks the owner for values (secret,
text, code, choice and file fields) that only the node can read, for
any task or bot session:

```sh
axe form ask --bot ops --purpose "Deploy token" --field token:secret:required:env=DEPLOY_TOKEN --wait
```

- `--bot` names the Telegram bot that sends the owner the prompt; it must be
  able to write to the owner's private chat. It is required unless
  `--no-telegram`, which only prints the link. `/runtimes` login forms come
  from the bot the owner typed `/runtimes` in.
- The prompt's "Open sealed form" button is a reply keyboard. A chat shows
  only its latest one, so while several forms are open in a chat the
  keyboard holds one button per form, labelled with its purpose. Once a form
  is delivered, rejected for good or expired, the bot tells the owner the
  outcome in that chat with a keyboard of the forms still open there, or
  removes it (`form_keyboard` and `form_reply` in `telegram.rs`,
  `Forms::take_closing` in `form.rs`).
- To test forms (CI, post-deploy checks), do not prompt the owner: use
  `axe form ask --no-telegram`, then submit a reply sealed to the printed
  link's spec with `axe form submit` (or through the link's page). No
  message reaches Telegram; the test in `crates/axe/tests/telegram.rs`
  (`a_finished_form_removes_its_button_and_no_telegram_forms_send_nothing`)
  pins this.

- The node signs the form with its key (`dispatcher.key`, P-256, 0600,
  made on first start next to the config file; `axe form key` prints its
  fingerprint, as do `axe runtimes`, `/runtimes` and every form). Compare it
  once.
- The link opens a static page (`[forms] page_url`, default
  [`DEFAULT_FORM_PAGE`](crates/axe/src/config.rs)) with the signed spec in its
  URL fragment. The page encrypts the values in the browser to the node
  key and returns `sealed.v1.<form>.<ciphertext>`: through the Telegram Mini
  App button the server sends the owner, pasted into the chat, a `.sealed`
  file, or `axe form submit`.
- The server keeps forms and ciphertext in memory only, until the node
  takes the reply (or 15 minutes pass), and records only names and outcomes.
  The node checks its signature, the expiry and one use, decrypts, and
  writes each value as a 0600 file in the target: the task's or bot's
  `$AXE_SECRETS_DIR` (`<run_root>/task-<id>/.secrets`, outside `repos/`;
  `env=VAR` fields are also set on its later runs), or a git credential for
  `--git <host>` that the node's clones fall back to
  (`[forms] git_credentials`).

The reverse, sealed messages to the owner (sealed messages in `sealed.rs`):

```sh
axe form reply-key --bot ops --wait   # once: the owner sets a passphrase
axe form send --bot ops --purpose "Staging DB password" --secret-file pw.txt
axe web login-link --tg --bot ops     # a one-time portal sign-in link, sealed
```

The owner's passphrase arrives through a sealed form; the node keeps
only the key derived from it (`[forms] reply_key_file`). `axe form send`
(`--secret-file`, `--link` or `--file`, 2 KiB in all) runs on the node
host, encrypts there and sends an inline "Open sealed message" Mini App
button whose form page link carries the ciphertext in its fragment. The page
asks for the passphrase and decrypts in the browser. Telegram and the server
see only the sender, purpose, expiry and ciphertext.

## Docs

`axe doc put/get/ls/log/diff/attach/detach/rm` (`doc.rs`) keeps versioned text
documents on the server: skills, docs, memory, conduct and bundles. A doc is a
path (`notes/plan.md`); a folder of docs is a path too, so a skill is a folder
with `SKILL.md` and its files, and a bundle is any folder. Every change is a
commit in a bare git repository (`docs.git` next to the database, or
`[docs] dir`), so `log` and `diff` show a doc's history; the server needs
`git` on its `PATH`. Docs attach to one of four scopes: `global`,
`label:<name>`, `owner:<name>` or `repo:<name>`; `axe doc ls --scope` lists
what a scope has. A write may name the version it edits (`--base`, or
`--new` for a new doc) and is refused with a conflict if the doc moved since.
`axe doc rm <path>` removes a doc, or a folder with everything under it, as
one more commit (a tombstone): `ls` and the dashboard Store stop showing it,
while `log` and `get --version` keep its history. It refuses a path a scope
still attaches unless `--detach`, which detaches it from every scope in the
same commit, and always refuses a path inside an attached folder.
Back the repository up with git (`git clone --mirror`); `axe backup` covers
the database only.

**Run context.** When the node builds a run workspace it collects the
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

**Bundles.** Every bot's system prompt starts with the bot protocol, which
is core (`crates/axe/src/bot_protocol.md`: runs and wakes, memory,
subscriptions, goals, authority, the Telegram commands and cards). A bundle's
`prompt.md` is text a bot carries after it: the bot inlines it, ordered by
its `order:` front matter, before its identity and conduct. Bundles hold
opinions, such as [`bundles/responsive`](bundles/README.md); load and attach
one with `axe doc put` and `axe doc attach`. A bot with no bundles still works
fully. `axe context explain <task>` prints each candidate, its scope and why
it was included or skipped (`context.rs`).

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
Claude Code finds the root `AGENTS.md` from the folder and from
`repos/<r>`. Codex reads it only when started in the folder, since it stops
at the repo's git root; from `repos/<r>`, run
`codex -c 'project_root_markers=[".axe"]'`.

## Configuration

One TOML file, `$AXE_CONFIG` (default `~/.config/axe/config.toml`; built-in
defaults when missing), read once at startup by both `axe server` and
`axe node`. The server uses `[server]`, `[telegram]`, `[metrics]`,
`[blobs]`, `[docs]` and the agent names; every local command reads `[cli]`;
the node uses the rest, so keep agent
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
| Admission hold (node host) | `~/.local/state/axe/admission-hold.json` | `$AXE_ADMISSION_HOLD` |
| Agent logs and node journal | `~/.local/state/axe/logs` | `log_dir` |

The database uses WAL: back it up with `axe backup PATH`, a consistent
snapshot from the running server, never by copying the file. Schema upgrades
run on server start (`db.rs`); back up before upgrading.

## HTTP API

With a token, every request needs `Authorization: Bearer <token>` or, for
reads, a browser read session; `/healthz`, `/metrics` and signed `/b/` links
are open, and a page without either gets the login page (`allowed` and
`login_route` in `crates/axe/src/server.rs`). The session is an HttpOnly
cookie that makes reads for 30 days, except Telegram message content, asks
and forms, which stay token-only (`private_read`); it also reads the store
(blobs, docs) and service logs (`store_read`). A browser gets one from
`axe web login-link` (a one-time code, `crates/axe/src/login.rs`), the token
on `/login`, or `POST /api/session` with the token; `POST /api/session-end`
clears it. The server refuses a non-loopback listen address without a token
file. Expose it only on a trusted network: reads include full agent logs. Bodies are JSON, errors are
`{"error": "..."}`, and `X-Axe-Request-Id` makes a write retry-safe.

The route table is `route`, `service_route`, `bot_route`, `goal_route`, `doc_route` and `blob_api` in
`crates/axe/src/server.rs`; node operations are in `remote.rs`. Response
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
