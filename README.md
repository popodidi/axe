# axe

A self-hosted task queue for coding agents: `axe server` owns a SQLite
database and serves an HTTP JSON API; `axe dispatch` is a separate process
(optionally on another machine) that starts one headless coding-agent session
(Claude Code, Codex, or any command you configure) per ready task. Every
command other than `server`, including `dispatch`, is an HTTP client of the
server.

## Install

```sh
cargo install --locked --git https://github.com/popodidi/axe axe-os   # binary `axe`
```

The crate is `axe-os`; the binary and the library are `axe`. Cargo features:

| Feature | Default | What it adds |
|---|---|---|
| `telegram` | on | The Telegram gateway for bots (`[telegram]`, `axe bot tg`) |

`--no-default-features` builds a server without the gateway: `bot tg` calls
answer 409, and a configured `[telegram]` table is ignored with a warning.
Stored messages and `bot tg history` work in both builds.

## Not a workflow engine

A task has a **state** (`pending` means eligible to run) and, once dispatched,
one agent session that carries it from start to `done`, resumed on every later
run. There are no owners, pipelines, stages, or transitions defined in code.
Every mutation appends to an event log (`axe log ID`) so a resumed session
can see what changed.

`ops` is an actor (intake, triage, approvals, queue cleanup). Human attention is
`axe list --state escalated`; setting the task `pending` resumes its session.
Stored `blocked` means an external wait; pending tasks with unfinished
dependencies also display blocked. Neither belongs in human attention. The
`/api/overview` `ops_queue` includes all three; only escalated entries have
`actionable: true`.

Tasks may declare prerequisites (`blocked_by`, see Dependencies). That is the
only ordering axe knows; it is not a workflow engine.

## Commands

```sh
axe add --project web --title "Fix X" --detail-file spec.md [--agent claude]   # state pending
axe add --project web --title "Ship X" --blocked-by 12,13      # waits until #12 and #13 are done
axe deps ID [--add P,...] [--remove P,...] [--json]  # edit edges atomically, then show blocked_by/blocks
axe list [--state S] [--project P] [--all] [--json]
axe show ID [--json]
axe log ID [--json] [--full]
axe set ID [--state S] [--title T] [--detail D|--detail-file F] [--note N] [--reset-runs] [--not-before T]
axe abort ID [--run RUN_ID] [--timeout 60]  # force-stop remotely; wait for confirmation
axe escalate ID --blocked B --resolver R --resume C --class CLASS   # escalated with a block note
axe classify ID [--escalation CLASS] [--workload W]   # correct the latest class or workload
axe stats --project P [--since 14d] [--until T] [--workload W]   # delivery statistics, JSON
axe usage backfill [--log-dir D] [--dry-run]   # read run usage from local logs; safe to repeat
axe wait ID (--on P,... | --until T | --for D) --note N  # resumes on its own
axe note ID TEXT            # or --file F
axe artifact ID https://github.com/org/repo/pull/1
axe approve ID --sha SHA --scope TARGET [--note N]   # blocked/escalated -> pending
axe claim [--project P] --by me                   # oldest ready task; prints id, exit 1 if none
axe server [--listen 127.0.0.1:8790]              # API; sole database owner
axe dispatch [--once] [--reconcile-note N]        # remote dispatcher + agent supervisor
axe config                  # validate and print the local $AXE_CONFIG
axe admission [--json]      # is the dispatcher admitting new sessions (resource pressure, hold)?
axe admission hold --until T [--reason R]   # no new task sessions until T (on the dispatcher host)
axe admission release       # end the hold now
axe backup PATH             # download a consistent server snapshot (WAL included) to a new file
```

`add` defaults to `--state pending`. Create work `pending` only when its spec
and authorization are complete; otherwise create it `--state escalated` and add
a block note.

`approve` records an `approval` event (body `sha=<sha> scope=<scope> by=<actor>`,
then the note on the next line) and moves a `blocked` or `escalated` task to
`pending` in one transaction; any other state is refused. Approval only records
the owner's decision and resumes the task: the agent re-checks the approved sha
and scope and every invariant. The actors `agent` and `dispatcher`, a
`$AXE_SESSION` starting with `dispatch:`, and a session belonging to a run
that has not ended are refused with `403`. All agents share one token, so this
is a guardrail, not a security boundary.

Block notes follow one format so the ops queue is actionable:

```
blocked: <what stops the next step>
resolver: <who>: <action>
resume: <condition or command, e.g. axe set ID --state pending>
```

States: `pending`, `in_progress`, `blocked`, `escalated`, `done` (stored). Each task also
has a derived `status`, which the CLI, list filter, counts, and dashboard show:
`blocked` for a pending task waiting on prerequisites, otherwise the state. The actor on events is
`--actor`, else `$AXE_ACTOR`, else `human`; `$AXE_SESSION` is recorded too.
The CLI sends both with each write and the server records them.

The event log stays compact: a `detail` edit stores a unified diff of the
spec as its body (`from_value`/`to_value` empty) rather than both versions;
other edits record the old and new value. `axe log` cuts each body line to
300 characters with a trailing `…`; `--full` prints bodies whole, and `--json`
is always complete.

Paths (server side): database `$AXE_DB` (default
`~/.local/share/axe/axe.db`), config `$AXE_CONFIG` (default
`~/.config/axe/config.toml`; built-in defaults if missing), token
`$AXE_TOKEN_FILE` (default `~/.config/axe/token`). Client side: server
`$AXE_SERVER` (default `http://127.0.0.1:8790`), token `$AXE_TOKEN`, else
the token file. `axe config` reads only the local config file.

The CLI rides out a server restart: a call that gets no response, a plain
ingress 404 (a Recreate Deployment with no endpoints) or a 5xx is retried with
backoff (0.5 s doubling to 8 s) for up to `$AXE_RETRY_SECS` (default 120;
`0` disables), then fails as before. A semantic axe error (a 4xx with a JSON
`error`) or a 401 fails at once. Each write carries one client-generated
`X-Axe-Request-Id` on all its attempts. The server reserves the id in
`dispatch_ops` (rows `cli:<id>`, the dispatcher's replay table; no schema
change) before running the write and stores the response after it, for an
hour. A repeat returns the stored response, so a write whose reply was lost is
applied once. A repeat while the reservation has no response (still running,
or the server stopped mid-write) gets a 503 and is never re-run; if that lasts
past the bound the CLI fails and the outcome must be checked by hand. A
stored server error replays as a 409. Reusing an id for a different request
is a 409. The dispatcher keeps its own journaled retry (`remote.rs`).

## Server mode

`axe server --listen <addr>` is the only process that opens the database,
including for `backup`. It binds the API, prints `axe server
listening on http://<addr>`, and never launches workers. A `PRAGMA
data_version` watcher notices every commit and wakes `/api/stream` subscribers.

## Dispatcher

`axe dispatch` is an HTTP client and worker supervisor (`--once`: one pass,
wait for its sessions, exit). Execution config (agents, workspace root, pool,
`log_dir`) lives on the dispatcher machine; it reaches the server through
`$AXE_SERVER` and the token, and agents inherit both.

- **Single owner.** The server grants one dispatcher a lease (renewed every
  10 s, expiring after 60 s) under a server-issued generation. A second
  dispatcher cannot register while the lease is active or runs are unresolved;
  operations carrying a stale generation are rejected.
- **Retry-safe operations.** Claims, run preparation and start, activity,
  completion, and blocks go through `POST /api/dispatcher/operation` with a
  stable operation id; a replayed id returns the original result. The
  dispatcher journals the pending operation and its local logs under
  `<log_dir>/.dispatcher/` before sending.
- **Outages.** While the server is unreachable the dispatcher stops admitting,
  keeps existing workers running, and retries the pending operation with
  backoff. A lost lease never requeues live work automatically.
- **Recovery.** A dispatcher that finds an unresolved journal or server-side
  runs refuses to start. After stopping the old workers, run
  `axe dispatch --reconcile-note "<why>"`: it escalates the interrupted
  tasks with that note, ends their open runs, releases ownership, and exits.
- **Logs.** Worker output stays in the dispatcher's `log_dir`. The dispatcher
  holds an outbound WebSocket (`/api/dispatcher/connect`) and the server relays
  `/api/runs/{id}/log` reads over it without storing log content. While the
  dispatcher is disconnected the endpoint answers `unavailable` with
  `retryable: true` and the web viewer keeps polling.

### Remote abort

`axe abort TASK_ID` selects the latest run once, sends an authenticated request
through the server, and waits up to 60 seconds for the owning dispatcher to
confirm termination. Exit zero means **confirmed**, never merely accepted. A
nonzero timeout says `abort requested, termination NOT confirmed`; the request
remains durable. Retry with the printed `--run RUN_ID` to keep targeting the
same run even if somebody later resumes the task. A repeated confirmed request
returns that run's historical result and the task's current state.

The server holds the task `escalated` immediately and prevents reopening it
until the run closes. It persists the request in the existing dispatcher run
phase, forwards run IDs on the generation-fenced WebSocket control channel
(within the 10-second heartbeat interval), and redelivers after reconnect.
The Linux dispatcher freezes the selected process tree, including detached
children retained by a worker subreaper, then force-kills it using pidfds and
waits for exit (bounded to five seconds per freeze/termination stage). This
requires Linux with pidfd support (kernel 5.3+). An error remains unconfirmed;
unrelated runs continue. There is no automatic fresh-session retry.

`abort_requested` records acceptance; `run_aborted` records confirmed termination
and the run closes. If normal completion wins, `abort_raced_completion` records
that no termination was confirmed; the CLI fails honestly. Missing runs, already
completed runs and stale dispatcher ownership are rejected. Logs and sessions
are retained. Explicitly resume with `axe set TASK_ID --state pending` after
review. Abort stops further execution; it cannot undo external side effects.

The CLI, server and dispatcher must all include this feature. An older CLI has
no `abort` command; an older server rejects the endpoint; an older dispatcher
cannot confirm termination. Upgrade all three before use.

### Auth

- `GET` requests are unauthenticated.
- Every other method needs `Authorization: Bearer <token>`, compared with the
  trimmed contents of `$AXE_TOKEN_FILE`; a missing or wrong token gets `401`
  and changes nothing.
- With no token file the server refuses to start on a non-loopback address. On
  loopback without a token, writes are open (local development only; a warning
  is printed).
- CORS: `GET` responses carry `Access-Control-Allow-Origin` only for origins
  listed in `server.cors_origins` (default none); preflights
  allow `GET` only, so browsers cannot write.

### API

All bodies are JSON. Errors are `{"error": "..."}` with `400` (validation),
`401`, `403` (approval from a live dispatcher session), `404` (unknown task/session/route), or `500`.

| Method and path | Purpose |
|---|---|
| `GET /healthz` | `{"ok": true}` |
| `GET /api/tasks?state=&project=&all=1` | Tasks, like `axe list` (done hidden unless `all` or `state=done`; `state` matches the derived `status`) |
| `GET /api/tasks/{id}` | One task, with derived `status`, `waiting_on` (unfinished prerequisite ids), and `deps` `{blocked_by, blocks}` (each `{id, project, title, state, status}`) |
| `GET /api/tasks/{id}/events` | Event log |
| `GET /api/runs?task=&live=1&limit=` | Dispatcher runs, newest first (`limit` default 50, max 1000) |
| `GET /api/overview` | `counts` (`{project, state, count}`, `state` is the derived status), `admission` (as `/api/admission`), `ops_queue` (stored `blocked`/`escalated` holds and dependency waits; each is the task plus `kind`, `actionable`, `blocked_by` (the actor who held it), `blocked_at`, and `blocker`, the latest hold note), `running` (live runs). `kind` is `hold` (stored `blocked` or `escalated`; only `escalated` is actionable) or `dependency` (pending, only waiting on `waiting_on`; `actionable: false`, no hold context). Overview attention lists/counts only escalated tasks |
| `GET /api/sessions/{session_id}` | `{session_id, task_id, project, agent, run}` for an agent session id (or a `dispatch:` claim); `run` is the latest run or null |
| `GET /api/stream` | Server-sent events: `event: change` with `{"seq": N}` on connect and after every task/event/run change |
| `GET /api/config` | `config_path`, `db_path`, `auth` (bool), resolved `config`; never the token |
| `GET /api/admission` | Resource admission as last reported by the dispatcher: `enabled`, `admitting` (null when unknown), `reason`, `checked_at`, `age_seconds`, `report_age_seconds`, `signals` (`name`, `level` clear/constrained/unknown, `value`, `reading`, `pause`, `resume`), `thresholds` |
| `POST /api/tasks` | `{project, title, detail?, agent?, state? = pending, blocked_by? = []}` -> `201 {id, task}`; the task and its edges commit together or not at all |
| `POST /api/tasks/{id}/deps` | `{add?: [ids], remove?: [ids]}` -> `{changed, task}`; one transaction |
| `PATCH /api/tasks/{id}` | `{state?, title?, detail?, note?, reset_runs?, not_before?}` -> `{changed, task}` |
| `POST /api/tasks/{id}/notes` | `{body}` -> `201 {task}` |
| `POST /api/tasks/{id}/artifacts` | `{value}` -> `201 {task}` |
| `POST /api/tasks/{id}/abort` | `{run: RUN_ID, actor?, session?}` -> `202 {run, status:"requested"}` or `200 {run, status:"confirmed", task_state}`; repeat the same run to poll safely; authentication required |
| `POST /api/tasks/{id}/approve` | `{sha, scope, note?}` -> `{task}`; `blocked`/`escalated` only, resumes `pending` |
| `POST /api/claim` | `{project?, by}` -> `{task}`, the oldest ready task (`null` when nothing is ready) |

Writes also take `actor` (default `human`) and `session`, recorded on events.

A task is `{id, project, title, detail, state, claimed_by, runs, created_at,
updated_at, not_before, agent, status, waiting_on, deps}`. A run is `{id, task_id,
project, agent, mode, session_id, claim, started_at, ended_at, exit_code,
log_path, current_activity, last_activity_at, live}`.

### Runs and live activity

The `runs` table has one row per spawned agent with task, project, agent, mode
(`fresh`/`resume`),
session id, claim, `started_at`/`ended_at`, exit code, log path,
`current_activity`, and `last_activity_at`. While a run is live the dispatcher
reads the tail of its log every tick and stores a short (<= 120 chars)
description of the latest JSONL item: a tool call name plus its main argument
(`Bash: cargo test`), an assistant message prefix (`says: ...`), or a result.
Claude `--output-format stream-json` and codex `exec --json` are recognized;
other output leaves the activity unchanged. Runs still open when the server
starts are closed (their tasks are escalated as orphans).

### Agent environment

Each agent gets `AXE_ACTOR=agent`, `AXE_SESSION` (its session id, or the
claim until a JSONL id is known), `AXE_TASK`, `AXE_SERVER`
(the server's own address; loopback when listening on all interfaces),
`AXE_WORKTREE` (with [`[worktrees]`](#task-worktrees)), and
`OTEL_RESOURCE_ATTRIBUTES` with the `[otel] resource` attributes, then
`axe.task_id=<id>,axe.project=<p>,axe.agent=<agent>,axe.mode=<fresh|resume>,axe.run=<claim>`
(prefix `[otel] prefix`, default `axe`) appended to any inherited value (values percent-encoded where needed). The
token reaches agents through the inherited `AXE_TOKEN_FILE` default, not the
environment. Telemetry export itself is the agents' job; axe only tags it.

## Conventions

| Situation | Set |
|---|---|
| finished | `--state done` |
| stuck, missing access, or human decision | `axe escalate ID ...` (block note) |
| non-human external wait | `--state blocked` (block note), or `axe wait` for a task or time |
| blocker resolved | `--state pending` (the task's session resumes) |
| ops approve | `axe approve ID --sha S --scope T`: `blocked`/`escalated` -> `pending` |
| real prerequisite | `axe add ... --blocked-by P` or `axe deps ID --add P` (stays `pending`) |
| prerequisite abandoned | `axe deps ID --remove P` (and re-scope the spec); never mark P `done` to dismiss it |

## Dependencies

A task can be `blocked_by` any number of other tasks, in any project, named by
explicit id. Nothing is inferred from creation order, title, or project.

- **Stored vs derived.** Edges are the only stored dependency state (`deps`
  table: `task_id`, `prereq_id`, `created_at`). A dependency wait is never
  stored: a task is *ready* when its state is `pending` and every prerequisite
  is `done`. Readiness is computed inside each claim transaction, so it is
  correct across restarts, repeated completions, and concurrent claims, and
  needs no ops step: when the last prerequisite is done the task is claimable.
- **Holds stay separate.** Stored `escalated` is a human hold (approval, access,
  failure, or manual intervention); stored `blocked` is a non-human external
  wait. Completing or removing prerequisites never
  changes either state; a held task keeps its state until someone
  resolves the hold (`set --state pending`, `approve`). Resolving a hold while
  prerequisites remain leaves a dependency wait, never ready work.
- **Shown as blocked.** A pending task with unfinished prerequisites has
  `status` `blocked`; `axe list` prints `blocked (deps #1, #2)` (or
  `blocked (hold+deps ...)` for a hold with prerequisites); `axe show` and
  `axe deps` list prerequisites and dependents with their status and say why
  the task cannot start.
- **Claims.** `claim` and the dispatcher take the oldest *ready* task. Waiting
  tasks are skipped: no `runs` increment, no `max_runs` block, no pool
  slot, and they never hold back newer ready work. `set --state in_progress` is
  refused while prerequisites are unfinished.
- **Edits.** Adding is idempotent (an existing edge is a no-op, duplicate ids
  collapse); removing a missing edge is a no-op. Missing ids, self edges, and
  cycles (direct, transitive, cross-project) are refused with `400`. Adding an
  unfinished prerequisite to an `in_progress` or `done` task is refused rather
  than stranding a live agent or reopening finished work; block or finish it
  first, or add the edge once the prerequisite is done. Every edge change
  writes a `dep_add`/`dep_remove` event on both tasks (`to_value` = the other
  id, body `blocked_by #P` / `blocks #T`) with actor and session.
- **Reopening a prerequisite** (moving it out of `done`): pending dependents
  stop being ready again; `in_progress` dependents keep running and `done`
  dependents stay done. Every dependent gets a `dep_reopened` event so the
  change is not silent; decide explicitly whether they need rework.
- **`done` is the completion contract.** A rejected or abandoned prerequisite
  must not be marked `done` to release dependents; remove the edge and
  re-scope instead. Only mark a prerequisite done when its own Outcome is met
  (for deploy outcomes that means deployed, not a PR, review, or merge).

## Dispatcher

Every `poll_seconds`, the dispatcher admits ready tasks (pending,
prerequisites done, awake) one at a time through the [admission pool](#admission-pool)
until the pool is full or no ready work is left, always claiming the oldest
ready task across projects. It spawns the configured agent with
`cwd = workspace_root/<project>` (or the [task worktree](#task-worktrees)) and
the [agent environment](#agent-environment). Output goes to
`log_dir/<task>-<run>.log`; `run_start` and `run_end` events record agent,
session id, and exit code (and a `runs` row tracks the run, see above). The
prompt is written to `log_dir/<task>-<run>.prompt.md` (recorded as
`prompt_file`), and the `cmd` in `run_start` replaces any argument longer than
200 characters with `@<prompt file>`. There is no system prompt; the agent's
own instructions (skills, `AGENTS.md`) say how to work a task.

The dispatcher sets a task `escalated` with a block note (resume with
`axe set ID --state pending`) when:

- the session exits while the task is still `in_progress` under its claim;
- the task already has `runs >= max_runs` (resume with `--reset-runs --state pending`);
- the project directory is missing, or spawning fails;
- the project has no task worktree base (before any claim, so no run is
  counted), or its task worktree cannot be allocated;
- the dispatcher restarts and finds an orphaned `dispatch:` claim;
- the dispatcher receives SIGTERM/SIGINT (running sessions are killed).

### Bounded drain

SIGUSR1 starts a drain, used to restart the dispatcher for a rollout. SIGTERM is
unchanged: it kills every session and escalates their tasks.

1. No new task is admitted. Runs that finish inside the window are untouched.
2. After `drain_abort_seconds` (default 900) every unprotected task run is
   killed. The task returns to `pending` under the same session, the run is
   refunded (`max_runs`) and costs no resume (`max_resumes`). A `run_interrupted`
   event records it, and the next prompt tells the session it was interrupted
   by a rollout and must re-check real state before repeating any step.
3. A protected run is waited for and never aborted. The deadline for protected
   runs belongs to the caller: a rollout script typically gives up after a long
   deadline and leaves the dispatcher draining.
4. Bots keep being admitted while task runs remain, so a long drain does not
   starve them. Once the last task run ends, no bot starts; live bot runs get
   60 s, then are stopped as on SIGTERM (their wakes return to the queue). The
   dispatcher exits 0 and whoever signalled it starts it again; tasks resume then.

`axe protect <id> [--note <what>]` marks the task's live run as inside a
non-repeatable step (merge, deploy, migration); `axe protect <id> --clear`
ends it. It is stored as `protected`/`unprotected` events keyed by run id, so it
ends with the run (no schema change). Protect only around the step. Once the
drain has decided to abort a run the call fails: do not start the step. The
server decides and records the abort in one operation, so the two cannot race.

Roll out the server before a dispatcher that uses `drain_abort`: an older server
rejects that operation.

### Task worktrees

With a `[worktrees]` table, every session of a task runs in one Git worktree
of the canonical workspace, `workspace_root/.worktrees/<project>/task-<id>--work`
(`cwd` and `AXE_WORKTREE`), on branch `<branch_prefix>task-<id>--work`
(`[worktrees] branch_prefix`, default `axe/`), reused across runs. The
dispatcher sees only the workspace; agents put sub-repository worktrees under
its `_repo/`. axe does not delete worktrees: remove a done task's directory
with your own cleanup job.

- **Readiness, before claiming.** The canonical clone `workspace_root/<project>`
  has its own `.git` and `refs/remotes/origin/HEAD`. Otherwise the oldest ready
  task of that project is escalated without a claim or run, with a
  note naming the gap.
- **Allocation.** A missing checkout (or an empty directory) is created after
  `git fetch origin` from `origin/HEAD`; if the task branch already exists (a
  reopened task whose directory was cleaned up) it is checked out instead. An
  existing worktree of the canonical clone is reused and never reset, checked
  out or cleaned. Anything else at the path escalates for manual
  reconciliation.
- **One session at a time.** A task is claimed by at most one run, so its
  worktree has one session at a time.

```toml
[worktrees]
```

Without `[worktrees]`, sessions start in the canonical workspace as before.

### Admission pool

`[pool] total` (default 1, at least 1) caps concurrent task agents. Each
running agent holds one slot for one task; a free slot goes to the oldest ready
task.

`[pool] bots` (at least 1) caps concurrent bot runs in a separate pool: bot
runs never take a task slot and task runs never take a bot slot, so a full task
queue cannot delay a bot's reply. Omitted, every active bot gets a slot (a bot
has at most one live run), so bots are never starved.

- Running agents are never preempted. A slot frees when its agent exits and is
  refilled on the next poll.
- Held tasks (`blocked` or `escalated`), sleeping tasks, and tasks waiting on
  prerequisites are not ready and take no slot.
- A fresh retry after a failed resume, including one deferred by resource
  admission, keeps its slot.

The dispatcher logs `axe dispatcher: pool total=N bots=M` at startup (`bots=one
per active bot` when omitted). `axe admission` shows both pools as
`used/total` slots (`pools` in `--json`).

### Resource admission

With a `[resources]` table in the dispatcher's config, `axe dispatch` starts
no new agent session while
guest disk, memory or CPU is tight. It is a global admission pause, not a task
state: waiting tasks stay `pending` and unclaimed, spend no run budget, get no
events, and start on their own once pressure clears. Running sessions, holds,
prerequisites and the pool are unchanged; nothing is killed,
throttled or cleaned up. Without the table nothing is gated.

**Bots and the hard floor.** The pause holds task sessions only. Bot runs stop
only at a hard floor: disk or memory strictly below half its pause threshold
(7.5 GiB free, 7.5% available with the defaults below), or unreadable. CPU never
holds bots, and the floor has no hysteresis. Each bot claim gets its own fresh
sample. `axe admission` adds `bots: admitting bot runs` or `bots: held at the
hard floor (...)` (`bots_admitting`, `bots_reason`); the journal logs one line
at startup and on each floor transition.

| Signal | Reading | Pauses | Recovers |
|---|---|---|---|
| disk | `statvfs` `f_bavail * f_frsize` of `path` (default `workspace_root`), as `df` "Avail" | `< 15 GiB` free | `> 20 GiB` free |
| memory | `MemAvailable / MemTotal` from `/proc/meminfo` | `< 15%` | `> 20%` |
| cpu | busy share of the `/proc/stat` `cpu` line over a 60 s monotonic window (busy = all fields but `idle` and `iowait`; `guest` is already in `user`) | `> 90%` | `< 80%` |

- **Sampling.** Every `sample_seconds` (10), and again immediately before each
  claim, so every slot gets its own fresh decision and a pass stops at the
  first refusal. The dispatcher is single-threaded: one decision per spawn.
  A gate that reopens triggers a dispatch pass at once instead of waiting for
  `poll_seconds`. A resume that fails and would retry fresh in the same pass
  is a new process too: it gets its own check, and while paused it is
  deferred, keeping its claim, slot and run count (one `fresh session fallback
  deferred` note), then starts fresh before any new claim once admission
  reopens. If ops changes the task meanwhile, the deferral is dropped; a
  dispatcher stop or drain escalates it like a stopped session.
- **Hysteresis and equality.** A clear signal pauses only strictly past its
  pause threshold; a paused one clears only strictly past its resume
  threshold. A value equal to a threshold does not cross it.
- **Unknown is not healthy.** At startup, until the CPU window spans 60 s, on a
  read error, or when the CPU window has a gap longer than
  `cpu_window_seconds + stale_seconds` (sampling stalled), the signal is
  `unknown` and admission pauses. A signal leaves `unknown` as if paused: it
  must reach its resume threshold. `dispatch --once` waits for a full CPU
  window before its single pass.
- **Status.** The dispatcher reports its decision to the server
  (`POST /api/dispatcher/admission`, lease-checked like other dispatcher
  calls) after every sample and at least every 10 s. `axe admission
  [--json]` (`GET /api/admission`, also `admission` in `/api/overview`) shows
  whether new sessions are admitted, the reason, each signal's level, reading
  and thresholds, plus `report_age_seconds`. Readings older than
  `stale_seconds` report `admitting: false` ("status stale"). With no report
  since the server started, or none for 60 s (the dispatcher lease),
  `admitting` is null ("unknown"), never healthy. `axe show` adds
  `waiting: resource admission paused (...)` to ready pending tasks, distinct
  from prerequisite waits. `axe config` prints the thresholds. The
  dispatcher journal gets one line at startup and one per signal or admission
  transition, never per sample; a failed report logs once per failure streak.

```toml
[resources]               # omit to disable; every key is optional
# path = "~/workspace"    # filesystem to watch; default workspace_root
sample_seconds = 10
stale_seconds = 30
cpu_window_seconds = 60
disk_pause_below_gib = 15
disk_resume_above_gib = 20
memory_pause_below_percent = 15
memory_resume_above_percent = 20
cpu_pause_above_percent = 90
cpu_resume_below_percent = 80
```

Validation requires resume thresholds strictly on the healthy side of pause,
percents in `0..=100`, and `cpu_window_seconds`, `stale_seconds >=
sample_seconds >= 1`. Older binaries reject the table: install the binary
first.

### Admission hold

`axe admission hold --until <T> [--reason R]` stops the dispatcher from
claiming any task for a new session until `T` (RFC3339 such as
`2026-11-10T23:00Z`, or `+<n>[smhd]`); `axe admission release` ends it
early. It gives a maintenance window (for example a cutover that needs no
`in_progress` task) a quiet queue without killing workers, and unlike the
bounded drain it never aborts a run and can be undone.

- **Scope.** Only new task claims stop. Running task sessions continue,
  fresh-session fallbacks of already claimed tasks still start, and bot runs
  are admitted as usual. Waiting tasks stay `pending` and unclaimed, spend no
  run budget and get no events, as under a resource pause.
- **Expiry.** The dispatcher reads the hold before every claim, so it ends by
  itself at `T` on the next dispatch pass (within `poll_seconds`), with no
  restart. A new `hold` replaces the current one.
- **Where it lives.** A JSON file on the dispatcher host:
  `$AXE_ADMISSION_HOLD`, default `~/.local/state/axe/admission-hold.json`.
  Run `hold` and `release` there, as the dispatcher's user. The file survives
  restarts of either process and needs no server or schema change; a file that
  cannot be read or parsed holds admission (fail closed) until it is released.
- **Status.** The dispatcher adds the hold to its admission report:
  `GET /api/admission` has `hold` (`until`, `reason`, `set_at`, `set_by`, or
  `error`; null when none), `admitting: false` and the hold first in `reason`;
  `bots_admitting` is unchanged. `axe admission` prints a `hold:` line and
  `axe show` adds `waiting: admission hold until ...` to ready pending tasks.
  `hold` and `release` return only once the server reports the change, and
  fail after 20 s otherwise (not on the dispatcher host, or a dispatcher
  binary without hold support). The journal logs each set, release and expiry.

### Session continuity

A task has one session. Its first run starts fresh with an agent chosen by the
task’s optional `agent` pin, otherwise the matching rule (`fixed` or
`round_robin`); every later run, whether after an
escalation, a hold, a `wait`, or a crash, resumes that session with the agent's
`resume_cmd`, pinned to the same agent without advancing round-robin. The
`sessions` table stores agent, session id, and resume count per task. Preset
ids are saved at spawn; JSONL ids are saved after exit. `run_start` records
`mode` (`fresh` or `resume`) and `session_id` (null until known for a fresh
JSONL agent).

Prompts are short; the agent reads the task itself:

- fresh: `Work axe task {id} in project {project}: run axe show {id} and axe log {id}, then carry it to done. Around a merge or deploy: axe protect {id}, then --clear.`
- resume: `Task {id} resumed; run axe log {id} for what changed.`

A missing stored agent, an agent without `resume_cmd`, or reaching
`max_resumes` starts fresh with a reason note. A resume that exits non-zero
without any axe change since it started retries fresh **in the same pass**,
including `dispatch --once`; this retry uses the same claim and the task pin (or the stored agent when
unpinned), with
`.fresh.log` and `.fresh.prompt.md` files to preserve the failed attempt. A
fresh session replaces the stored one and resets its resume count. Normal run
limits still apply. Codex resume supports config overrides but no explicit
cwd/sandbox flags: the template sets `sandbox_mode` via `-c`, and the
dispatcher sets the process working directory.

## Config

`axe add --agent NAME` pins the agent for fresh sessions, overriding both
project and catch-all rule agents and strategy without advancing their
round-robin cursor. Limits still come from the matching rule. Omit the pin
for existing rule behavior. The server validates pins against its configured
agents on task creation and routine save; keep agent names aligned with the
dispatcher config. If a pinned agent later disappears from dispatcher config,
fresh dispatch escalates the task with an unknown-agent note, never falls back.
Resumable sessions retain their stored agent. `show` and the API expose `agent`
(null for unpinned tasks). Pins are creation-time fields, not `set` options.


Give a reworked task a fresh run budget with
`axe set ID --reset-runs --state pending --note "<reason>"`. Resetting runs
alone does not resume a blocked or escalated task and is never an approval. It
leaves the task's session intact. It is an authenticated write, logs the old
and new count with actor/session, and refuses live dispatcher claims (400).

```toml
workspace_root = "~/workspace"
log_dir = "~/.local/state/axe/logs"
poll_seconds = 60

[server]
cors_origins = ["http://dashboard.example"]   # origins allowed to GET the API (default none)

[pool]
total = 5                 # concurrent task agents (default 1)
# bots = 2                # concurrent bot runs, separate from total; default one per active bot

[defaults]
agents = ["claude"]
strategy = "fixed"        # fixed | round_robin; picks a new session's agent
max_runs = 6              # runs per task
# max_resumes = 3         # omitted: unlimited, 0: always fresh

[agents.claude]           # built in; override here if needed
cmd = ["claude", "-p", "--output-format", "stream-json", "--verbose", "--session-id", "{session_id}", "--permission-mode", "bypassPermissions", "{prompt}"]
resume_cmd = ["claude", "-p", "--output-format", "stream-json", "--verbose", "--resume", "{session_id}", "--permission-mode", "bypassPermissions", "{prompt}"]
session = "preset"

[agents.codex]
cmd = ["codex", "exec", "--json", "--sandbox", "danger-full-access", "-C", "{cwd}", "{prompt}"]
resume_cmd = ["codex", "exec", "resume", "--json", "-c", 'sandbox_mode="danger-full-access"', "{session_id}", "{prompt}"]
session = "jsonl:thread_id"

[[rule]]                  # no match: every project
agents = ["claude", "codex"]
strategy = "round_robin"

[[rule]]
match = { project = "web" }
agents = ["codex"]

[worktrees]               # dispatcher-owned task worktrees (see Task worktrees)
branch_prefix = "axe/"    # default

[otel]                    # OTEL_RESOURCE_ATTRIBUTES for agent runs
prefix = "axe"            # per-run keys: <prefix>.task_id, .project, .agent, .mode, .run
resource = { "service.namespace" = "agents" }   # static attributes, default none

[metrics]
projects = ["web"]        # own `project` label on /metrics; every other project is `other`

[prompts]                 # dispatcher prompts; {id} and {project} are replaced
# fresh = "Work axe task {id} in project {project}: ..."
# resume = "Task {id} resumed; run axe log {id} for what changed."
# interrupted = "Your previous run was interrupted by an axe rollout, ..."
```

Rules match on `project` only. Each field (`agents`, `strategy`, `max_runs`,
`max_resumes`) comes from the most specific matching rule that sets it (project
rules over catch-all rules over defaults; later wins on ties). Round-robin
cursors are keyed by the match of the rule that supplied `agents` and persist in
the database. Unknown keys, including retired ones (`personas`, `looped`,
`persona_dir`, `pool.priority`, `continuity`, `match.persona`, `concurrency`),
fail validation.

Placeholders: `{session_id}`, `{prompt}`, `{cwd}`, `{task_id}`, `{project}`;
any other `{name}` is rejected. An agent's optional `bot_args` (see Bots) may
also use `{system_prompt_file}` and `{settings_file}`. Session modes: `preset` (dispatcher generates a
UUID), `jsonl:<key>` (read from the first JSON output line containing `<key>`,
fallback `session_id`), `none`.

The database uses WAL, so committed transactions may live only in
`tasks.db-wal`; never back up by copying `tasks.db`. `axe backup PATH` asks
the running server for a consistent snapshot (SQLite `VACUUM INTO`, WAL
included) and downloads it to a new file, refusing to overwrite one.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --locked
```


## Legacy v8 conversion

The v8 conversion is retained in the current v10 migration and runs in one
transaction; any older schema is refused (upgrade it to v8 with an earlier
release first). The conversion drops `tasks.owner` and its index,
`runs.persona`, and the retired `dream_attempts` table, and keeps one session
per task: the builder session when one exists, else the most recently prompted
one. All other rows and ids are kept.
Take `axe backup` with the old binary first. This legacy conversion removes
columns; the additive v9→v10 rollback below does not reverse it. Never restore
an older snapshot over newer committed work.

## Routines (schema v10)

The server polls once per second. Routines create ordinary pending tasks;
execution, session ownership, approvals and retries remain the dispatcher's
responsibility. Overlapping occurrences are allowed. Each task retains a
snapshot of the template and a durable occurrence backlink, even after editing
or archiving its routine. Nothing is deleted by archival.

```sh
# JSON: name, project, cron, timezone (optional; UTC), title, detail (full spec), agent (optional)
axe routine save --file template.json
axe routine save --id 1 --file revised-template.json
axe routine list
axe routine show 1
axe routine pause 1
axe routine resume 1
axe routine trigger 1 --key unique-request-id
axe routine history 1 [--before OCCURRENCE_ID]
axe routine events 1 [--before EVENT_ID]
axe routine archive 1
```

An optional `"agent":"claude"` template field is copied to every scheduled or
manually triggered task and retained in the occurrence snapshot. Old templates
without a pin still load. Editing the pin only affects future tasks.

Commands return JSON. `save --id` replaces the complete configuration. Name,
project, title and detail must be nonempty. Cron accepts exactly five numeric
fields (minute, hour, day of month, month, day of week) with `*`, lists, ranges
and steps; timezone is an IANA name. Invalid or impossible schedules are
rejected before writing. Day-of-month/day-of-week use cron OR semantics.

Scheduling uses locked `croner` 4.0.2 with `chrono-tz`. Fixed-time jobs in a
spring-forward gap run at the first valid instant after the gap; in a fall-back
fold they run only at the first instance. Interval/wildcard jobs skip the gap
and may run in both instances of the repeated hour. The integration tests pin
these behaviors. Occurrence identity uses UTC epoch seconds, never ambiguous
local timestamps. Upgrading the cron or timezone dependencies requires rerunning
these tests. Library source/API: https://docs.rs/croner/4.0.2/croner/struct.Cron.html.

Creation starts at the next future occurrence. After downtime, one task is
created for the latest missed occurrence, then the cursor advances into the
future. A pause does not catch up: resume starts at the next future occurrence.
Repeating resume on an enabled routine keeps its cursor. Schedule/timezone
edits start a new future cursor; other edits keep the cursor, and only affect
future tasks. Archive is terminal. Manual triggers work while paused, use a
caller-supplied retry key, and never move the schedule cursor. Retrying the
same key returns the same task, including after archival.

The server commits occurrence, task, created event, routine audit and cursor
in one immediate transaction. `(routine_id, identity)` is unique; a failed
transaction leaves neither task nor occurrence. No OS/Kubernetes cron objects
are created. Routine reads are public like task reads; all writes use the
existing bearer-token/actor/session authentication contract.

API: GET `/api/routines`, GET `/api/routines/:id`, POST `/api/routines` and
PUT `/api/routines/:id` with `{template:{...},actor,session}`; POST
`/api/routines/:id/{pause,resume,archive,trigger}` (trigger requires `key`).
GET `/api/routines/:id/{history,events}` returns newest first, at most 100 rows;
use `?before=<last id>` for the next page. GET `/api/tasks/:id/routine` returns
the backlink/snapshot, or null. Lists include archived routines.
## Delivery statistics

`axe stats` and `GET /api/stats/flow?project=&since=&until=&workload=` compute
delivery figures from the event log and runs, overall and per workload, with no
run cap. Two facts are recorded rather than derived, as append-only events where
the latest wins: `escalation_class` (`axe escalate --class`; the dispatcher
records `runtime`) and `workload` (`axe add --workload`, a routine's `workload`).

| Class | Meaning |
|---|---|
| `decision` | A legitimate human call: product, risk, authorization |
| `capability` | The worker lacked tooling, context, instructions or verification |
| `coordination` | Scheduling, ordering or recovery failed |
| `runtime` | axe or infrastructure failed |

Workloads are `prod-bug`, `infra`, `feature` and `ops`.

- **Intervention:** an escalation that was approved, names `owner` as resolver, or
  was resumed by `human`; an approval outside an escalation; or any other
  `human` state change or spec edit. One resolution counts once.
- **Verified shipped:** finished in the window, not reopened since, with an
  artifact. A proxy: axe does not record that Verify passed.
- **Autonomous:** an escalation resolved in the window with no intervention.
- **Backlog:** tasks ready now and the longest they have been runnable; dependency
  and timed holds do not count. The figure is as of now, not the window's end.

`owner_messages` counts inbound bot messages for context; their text is not
classified, so it is not an intervention. Tasks escalated before classes existed
report as `unclassified`.

GET `/api/stats/history` returns `{events, routines, runs}` for the dashboard Stats
view in one read: every `run_start` event and every state event whose value
changed (`id, task_id, at, kind, from_value, to_value`, ID order), the task
IDs that are routine occurrences, and every run, uncapped (`id, task_id, agent,
started_at, ended_at`, ID order).

Schema v11 adds nullable `tasks.agent`, preserving all prior columns and rows.
Schema v10 added only `routines`, `routine_occurrences`, `routine_events` and
their indexes, preserving every v9 table/column/row. v8 still migrates through
the existing v9 conversion. Back up the database before an upgrade that changes the schema.

## Bots (schema v12)

A bot is a standalone primitive next to tasks: a persistent identity tied to a
project, one agent session resumed on every wake, and one wake schedule. Wakes come from
`axe bot wake`, named schedules, task-event subscriptions and the Telegram
gateway (below). A bot with none of these due sleeps until woken.

```sh
# JSON: name, project, agent (default claude), identity, conduct, notes,
# nudge_after (default 10m)
axe bot create --file bot.json
axe bot list | show [NAME] [--json]
axe bot edit [NAME] [--identity-file F] [--conduct-file F] [--nudge-after 10m] --why W
axe bot notes [NAME]                       # print; --edit-file F --why W replaces
axe bot wake [NAME] (--in 2h | --at T | --now) --note N [--key K]
axe bot inbox [NAME] [--take] [--hook post-tool|stop]
axe bot log [NAME] [--limit 50] [--full] [--json]
axe bot pause | resume | stop | reset [NAME]
```

NAME defaults to `$AXE_BOT`. Identity and conduct are capped at 16 KiB, notes
at 32 KiB; every edit is recorded as a diff event with its `--why`.

**Readiness.** A bot is ready when it is active, has no live run, and has a due
undelivered wake; elapsed time alone never starts a run. Creating a bot queues a
`bot created` wake. A message wake (from the gateway) is due 3 s after the
newest message of its chat's burst, at most 15 s after the burst's first
message. The dispatcher checks bots every 2 s and admits them before tasks,
in their own `[pool] bots` slots; the resource gate holds them only at its
hard floor (Resource admission).

**Runs.** Each run renders `<log_dir>/bots/<name>/system.md` (the static bot
protocol, identity and conduct) and `settings.json` (PostToolUse and Stop hooks
calling `axe bot inbox --take --hook ...`), then runs the agent's `cmd` or
`resume_cmd` with `bot_args` spliced in before the `{prompt}` argument. The
prompt holds only the wakes; a fresh session adds a line to read
`axe bot notes`, `axe bot log` and `axe bot tg history` first. A run that
exits 0 with 200k or more tokens of context retires its session (event
`shrink`; `shrink_deferred` while the owner's latest message is unanswered),
so the next wake starts fresh instead of re-reading it from a cold cache. Logs are
`<log_dir>/bots/<name>/<run>.log`. The session runs in the persistent worktree
`.worktrees/<project>/bot-<name>` (branch `<branch_prefix>bot-<name>`) when
`[worktrees]` is configured, else in the canonical workspace. Environment:
`AXE_ACTOR=bot:<name>`, `AXE_BOT`, `AXE_BOT_RUN`, `AXE_SESSION`,
`AXE_SERVER`, `AXE_WORKTREE`; never `AXE_TASK`.

```toml
[agents.claude]   # the built-in claude already has these bot_args
bot_args = ["--system-prompt-snapshot", "off", "--append-system-prompt-file",
            "{system_prompt_file}", "--settings", "{settings_file}"]
```

An agent without `bot_args` is refused at `bot create`. The server validates
against its own config, so a server or VM config that redefines `agents.claude`
must include `bot_args`. Codex has no `bot_args` until its instruction
semantics on `exec resume` are verified.

**Mid-run wakes.** `inbox --take --hook post-tool` atomically delivers due
wakes to the live run (`delivered_via=hook`) and prints
`hookSpecificOutput.additionalContext`; after `nudge_after` it also adds a
long-run nudge, at most once per interval. `--hook stop` prints
`{"decision":"block","reason":...}` only when it took new wakes, so the stop
loop ends. Hook mode never fails the hook. Output is capped at 10,000 chars.
Wakes still undelivered when a run exits make the bot ready again at once; the
next run resumes the same session and lists them under "Arrived while you were
working on the previous run".

**Finish.** Exit 0 saves the session and resets `failures`. `bot stop`, `bot
pause` and a dispatcher stop are not failures (a dispatcher stop also returns
the run's prompt wakes). Any other end returns the prompt wakes (marked
`redelivered`) and counts a failure: at 2 the session is cleared, at 3 the bot
pauses with an `attention` event; `bot resume` clears the count.

**API.** Public reads: GET `/api/bots`, `/api/bots/:name`,
`/api/bots/:name/{wakes,events}` (`?limit=`, `?undelivered=1`; message text is
redacted), `/api/bots/:name/activity?limit=` (metadata only, oldest first:
`messages` `{id,direction,kind,reply_to,edit_of,attachments,at}` with an
attachment count and no text, file or chat ids, and `task_actions`
`{task_id,at,actor,kind}` for task events in the bot's project by `bot:<name>`
or `human`; each list is the latest `limit`, default and cap 1000),
`/api/bot-runs?bot=&live=&limit=`, `/api/bot-runs/:id`,
`/api/bot-runs/:id/log` (relayed as `{"kind":"bot_log"}`). `/api/overview`
gains `bots`. Writes (token): POST `/api/bots` `{bot:{...}}`, PATCH
`/api/bots/:name` `{identity,conduct,nudge_after,why}`, PUT
`/api/bots/:name/notes` `{notes,why}`, POST `/api/bots/:name/wakes`
`{in_seconds|at|now,note,key}`, POST `/api/bots/:name/inbox`
`{run,take,hook,stop_hook_active}`, POST
`/api/bots/:name/{pause,resume,stop,reset}`. Dispatcher operations:
`bot_ready`, `bot_control` (reads), `bot_claim`, `bot_started`,
`bot_activity`, `bot_finish`, fenced by dispatcher generation and replayable
like task operations. Reconciliation closes open bot runs and returns their
prompt wakes.

### Telegram gateway

With a `[telegram]` table, `axe server` runs one `getUpdates` long-poll
thread per configured bot token (no webhook; one poller per token, so run one
server replica). Without it, or when a bot's token file is missing, the server
starts as before and `bot tg` reports that Telegram is not configured.

```toml
[telegram]
owner_user_id = 123456789        # the only person who controls bots (numeric id)
# api_base = "https://api.telegram.org"
# poll_timeout = 30              # seconds, 1..=50
[[telegram.bot]]
name = "cos"                     # the axe bot it talks for
token_file = "/run/secrets/telegram/cos"
chats = [123456789]              # inbound and outbound allowlist (groups are negative)
```

Tokens live only in the server's token files (for example a mounted secret); agents call the
server, never Telegram. `/api/config` shows only the configured bot names.

**Inbound.** Requested updates: `message`, `edited_message`, `callback_query`,
`my_chat_member`, `message_reaction`. An update is accepted only when
`from.id == owner_user_id`, `from.is_bot` is false, there is no `sender_chat`
or `via_bot`, and the chat is allowlisted (callback queries and reactions get
the same check). Everything else is dropped before it is stored, with a
metadata-only `security_drop` bot event; the first drop from a new source
alerts the owner's private chat (at most one alert a minute). Being added to a
chat that is not allowlisted makes the bot `leaveChat` and alert. Accepted
messages are stored in `bot_messages` (`forward_origin` sets
`flags.forwarded`), get a 👀 reaction and `typing`, and queue a debounced
message wake (3 s quiet, 15 s cap per chat burst, which also groups media
groups and quick edits). Edits are kind `edit` with `edit_of`; button presses
become `button` wakes and get a "Got it: <label>" toast, the pressed message
loses its keyboard and shows `> <label> (selected)`, and a later press on it
only gets an "Already received" toast; `typing` is refreshed every 4 s in the
chats whose wakes a live run took;
reactions are kind `reaction`. Those three kinds store the Telegram
`update_id` as `message_id`. The poller confirms an update (the next
`getUpdates` offset) only after its effects commit; a redelivered update
dedupes on the stored message or the wake key. After 3 failed attempts an
update is skipped with an `update_skipped` event. A bot paused by failures
(`attention` event) alerts the owner.

**Owner commands**, handled by the server without waking the bot and answered
in the chat: `/status` (state, live run and activity, next wake, queued
wakes), `/stop`, `/pause`, `/resume`, `/reset`, `/wake [note]`. Only the
owner's own plain text counts (never forwarded content or captions);
`/cmd@name` counts only when `name` is this bot's username. A command that
committed is not repeated if its reply fails. `/wake` notes are redacted from
the public wakes view.

**Outbound** (`bot tg`, NAME from `--bot` or `$AXE_BOT`, MSG is the local
`bot_messages` id shown as `#N` in wakes):

```sh
axe bot tg send [--reply-to MSG] [--chat C] [--thread T] [--html] [--buttons "Yes=yes,No=no"] [--silent] (TEXT | --file F)
axe bot tg send-file PATH [--as photo|document|voice] [--caption C] [--reply-to MSG]
axe bot tg edit MSG [TEXT [--html]] [--buttons "Label=data,..." | --clear-buttons]
axe bot tg pin MSG [--silent] | unpin MSG
axe bot tg react MSG EMOJI | typing [--chat C] | delete MSG
axe bot tg file MSG[/N] [--out DIR]     # default $AXE_WORKTREE/.inbox
axe bot tg history [--limit 50] [--before MSG] [--search Q] [--json]
```

Targets must be allowlisted chats (403 otherwise); the default target is the
replied message's chat, else the latest inbound chat, else the first
allowlisted chat. Text is split into parts of at most 4096 UTF-16 units at
line boundaries, at most 20 parts (`--html` text must fit one message). Calls are serialized per
chat, spaced about 1 s apart, and retried after a 429's `retry_after` (3
attempts). Uploads are refused over 10 MB (photo) or 50 MB (document, voice);
downloads over 20 MB. Files pass through the server and are not stored.
Deleting a message keeps its record (`flags.deleted`).

`edit` works on the bot's own text messages. TEXT alone calls
`editMessageText` without `reply_markup`, as it always has: Telegram then
drops an existing keyboard, so pass `--buttons` to keep or change it.
`--buttons` replaces the keyboard (one row) and `--clear-buttons` removes it;
without TEXT these call `editMessageReplyMarkup` and keep the text. A new
keyboard clears `flags.answered`, so presses on it are new button wakes. The
output reports the buttons Telegram says remain. `pin` and `unpin`
(`pinChatMessage`, `unpinChatMessage`) take the bot's own messages and record
`flags.pinned`; `--silent` sets `disable_notification` (private chats never
notify). Groups need the bot to be an admin with `can_pin_messages`.

**Read privacy.** `GET /api/bots/:name/tg/history` and
`/api/bots/:name/tg/file/:msg[/:n]` need the bearer token. Public:
`GET /api/bots/:name/activity` (message metadata only: id, direction, kind,
time, reply/edit links, attachment count, forwarded). Writes: POST
`/api/bots/:name/tg/{send,send-file,edit,pin,unpin,react,typing,delete}`; `send-file`
takes the raw file as the body with `as`, `name`, `caption`, `reply_to`,
`chat`, `thread` query parameters.

Schema v12 only adds `bots`, `bot_messages` (written by the gateway),
`bot_wakes`, `bot_runs`, `bot_events` and their indexes. A v12 database with
`user_version` lowered to 11 opens with a v11 binary, keeps every bot row, and
upgrades again in place.

### Schedules (schema v15)

A bot can have named cron schedules, each with an objective. Off unless
created; the bot manages its own.

```bash
axe bot schedule [--bot NAME] add SCHEDULE --cron "0 9 * * 1" [--timezone Asia/Taipei] --objective TEXT --why W
axe bot schedule [--bot NAME] list [--json]
axe bot schedule [--bot NAME] pause|resume|remove SCHEDULE --why W
```

`--bot` defaults to `$AXE_BOT`. The cron is the routines' five-field numeric
syntax, read in `--timezone` (default UTC). A schedule must fire at least 15
minutes apart. Changes are `schedule_add`, `schedule_pause`, `schedule_resume`
and `schedule_remove` bot events, shown by `axe bot show` with the schedules
and their next fire time.

The server fires schedules on its one-second tick, as an ordinary `bot_wakes`
row: actor `schedule:<name>`, body `scheduled wake (automation, not an owner
message). Objective: ...`. It batches with other wakes and never counts as an
owner message. The cursor `next_at` moves in the same transaction as the wake,
so a restart neither loses nor repeats a firing, and any slots missed during
downtime fire once. A schedule has at most one undelivered wake: a firing while
one is queued coalesces (`schedule_fire` event with `coalesced`), so a firing
during a live run reaches that run through its hook or causes one follow-up run.
A paused bot's slots pass without a wake. Pausing or removing a schedule also
drops its queued wake. In Cost, a run caused by one appears under the bot wake
reason `schedule: <name>`.

`GET /api/bots/:name/schedules` lists; POST to it (`name`, `cron`, `timezone`,
`objective`, `why`) adds; POST `.../schedules/:schedule/{pause,resume,remove}`
(`why`) changes. Schema v15 only adds `bot_schedules` and its index. A v15
database with `user_version` lowered to 14 opens with a v14 binary and upgrades
again in place.

### Task-event subscriptions (schema v13)

A bot can follow task events, per project or for all projects. Off by default.

```bash
axe bot subscribe [NAME] [--project P | --all-projects] [--wake a,b] [--ride c,d] [--batch 5m] --why W
axe bot unsubscribe [NAME] [--project P | --all-projects] --why W
axe bot subscriptions [NAME] [--json]
```

Events: `new`, `pending`, `blocked`, `escalated`, `done`. A new subscription
defaults to the bot's own project, `--wake escalated`, `--ride new,done` and
`--batch 5m`; on an existing one, omitted options keep their value and `""`
clears an event list. The batch period is 1m to 24h, an event cannot both wake
and ride, and one of the lists must be non-empty. Changes are recorded as
`subscribe` / `unsubscribe` bot events and shown by `axe bot show`.

Wake events join the bot's one open batch, across projects, due after the
shortest batch period that joined it, so they cost one run. The batch lists up
to 50 tasks, then ids. Ride-along events never cause a run: the next run gets
one digest with counts and up to 10 ids per event, one line per project, at
most 12 projects.

Fan-out happens in the transaction that writes the task event, so a restart
neither loses nor repeats one, and each event reaches a bot once (a project
subscription wins over an all-projects one). Batches are ordinary `bot_wakes`,
so a run lost to a crash is redelivered like any wake. A bot's own actions
(`bot:<name>`) are not fanned out to it. `GET /api/bots/:name/subscriptions`
lists; POST to it (with `project`, `wake`, `ride`, `batch_seconds`, `why`)
creates or changes; POST `.../subscriptions/remove` deletes.

Schema v13 only adds `bot_subscriptions`, `bot_feed` and their indexes. A v13
database with `user_version` lowered to 12 opens with a v12 binary and upgrades
again in place.

## Migrating from the old name

axe was imported from an earlier tool under another name. Until every
deployment has moved, one module (`crates/axe/src/compat.rs`, which lists the
old names) keeps them working and is then deleted: the old environment
variables fill unset `AXE_*` ones (with a warning) and child agents get both;
the old config, data and state directories are used when the axe ones are
missing; both request and file-name header names are accepted and sent; and
the binary runs unchanged through a symlink with the old name.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in axe by
you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
