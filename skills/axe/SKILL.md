---
name: axe
description: "Use the axe task queue: queue tasks, read their logs, change state, escalate, wait, approve and record evidence. Also use for any session started by the axe node ($AXE_TASK or $AXE_BOT is set)."
allowed-tools:
  - Read
  - Write
  - Edit
  - Glob
  - Grep
  - "Bash(axe *)"
  - "Bash(git *)"
  - "Bash(gh *)"
---

# axe

`axe` is a task queue and agent node (formerly the dispatcher), not a workflow engine. A task has a
project, a spec and a state, and every change is logged (`axe task log <id>`).
The node runs one agent session per task, from spec to done, and resumes
that same session every time the task comes back. The workflow is whatever the
spec says; how a project ships, what needs a human and how to test live in the
project's own instructions (for example its `AGENTS.md`), not in axe.

This skill covers how the tool behaves. Owner workflows (agreeing a spec,
resolving holds, delivery and authorization policy) belong to the deployment's
own skills and instructions.

## States

- **pending**: the node claims it and starts or resumes its session.
- **in_progress**: a session is working on it.
- **escalated**: a hold for a human. Needs an explicit resume.
- **blocked**: an external wait with no known time. Needs an explicit resume.
- **waiting**: `axe task wait --for/--until` resumes automatically at the time;
  `blocked_by` edges resume it when prerequisites are `done`. Edges never lift
  an escalation.
- **done**: the completion contract. Dependents start when a task is done, so
  set it only when the spec's `done-when` holds.

A session that exits with its task still `in_progress` is escalated by the
node, as are node errors and `max_runs` exhaustion (resume with
`axe task set <id> --reset-runs --state pending`).

A known wait (a queued check, a release, a contended fixture) uses a timed or
dependency wait, not an escalation. Poll in-session only for checks due within
a few minutes; for longer waits, `axe task wait` and exit, so the same session
resumes later without holding a worker slot.

Size a timed wait to what it waits on, not to a round hour. For a CI run, a
deploy, a release step or another short external check, wake when it should
finish: its usual duration from when it started (`gh run view`, recent runs),
or about 15 minutes when unsure, and at most about 15 minutes even when it
is queued behind other runs: queue estimates run long, and a short re-armed
wait costs little. If it is still running or queued on wake, re-arm. Each
wake costs a run against `max_runs`, so do not go much below 10 minutes; for
something due within a few minutes, poll in-session instead. Keep waits of an
hour or more for holds that really are that long: a timed review window, a
scheduled time, a soak period.

```bash
# CI run 123 started 10:02Z and usually takes ~12 min
axe task wait 42 --until 2026-01-05T10:15Z --note "check run 123; green -> merge at head abc123"
axe task wait 42 --for 15m --note "run 123 still queued; recheck, green -> merge"
```

## Commands

| Situation | Command |
|---|---|
| Queue a spec | `axe task add --project <p> --title "<imperative>" --detail-file <spec>` (`--blocked-by <ids>` for prerequisites, `--agent <name>` to pick an agent) |
| Owner, repos, labels | `axe task add`/`axe task set` with `--owner <bot:name\|person>` (`none` clears), `--repo <r>` (repeat; omitted or `--no-repos`: none), `--label <l>` (`--unlabel` on set); `axe task list --label <l> --owner <o>` |
| Read a task | `axe task show <id>`, `axe task log <id>` (`--full` for whole event bodies) |
| Done | `axe task set <id> --state done --note "<what holds now>"` |
| Needs a human | `axe task escalate <id> --blocked ... --resolver ... --resume ... --class decision\|capability\|coordination\|runtime` |
| A human approves shipping | `axe task approve <id> --sha <head> --scope <target>` (resumes the task) |
| Blocker resolved | `axe task set <id> --state pending --note "<what changed>"` |
| Needs another task first | `axe task wait <id> --on <ids> --note "<why>"` |
| Timed wait | `axe task wait <id> --for 15m\|--until <time> --note "<what to check>"` (sized as above) |
| Evidence | `axe task note <id> "<command and key output>"`, `axe task artifact <id> <url>` |
| Before a non-repeatable step (merge, deploy, migration) | `axe task protect <id> --note "<step>"`; if it fails, do not start the step. After it: `axe task protect <id> --clear` |
| Human attention queue | `axe task list --state escalated` |
| Owner ratings of asks (escalations, cards) | `axe ask list [--bot <name>] [--kind <kind>] [--unrated] [--by kind\|bot] [--json]`, `axe ask stats [--by kind\|bot]` (wasted-ask share per bot, kind and week). Each ask has a stable kind (`category`: approval, decision, choice, confirm, digest, access, stuck, other). Each bot sends its own retro card, holding only the asks it raised, at its own slots (only with unrated asks; settings on the bot, shown by `axe bot show`: default on, every 7d from `mon 01:00` UTC, 10 asks; the owner changes them with `axe bot edit <bot> --retro on\|off --retro-every 14d --retro-at "mon 01:00" --retro-size 10 --why W`, a bot only on an owner message, adding `--owner-ref "msg #N"`): its most useful unrated asks of the last interval (one per task or card, the latest; those the owner had to act on, decision escalations and answered cards, first; then the newest; the rest stay unrated, counted in the footer), two lines each (id and topic, then the question with links shortened), each rated on its own, paged, never pinned, with a Dismiss button that closes it and leaves its asks unrated. Asks no bot raised are on no card. A card its bot settled without an owner press (`tg edit --clear-buttons "Decided: ..."` or `"Closed: ..."`) is not an ask. When a card closes (Dismiss, or its last item rated) with ratings on it, or is still open with ratings at the bot's next slot, the bot gets one wake (`Retro #N closed: ...`) to read the ratings and propose rule changes. `axe ask retro --bot <name> [--limit N]` sends it now (`--dry` previews it); the owner rates with `axe ask rate <ask> needed\|could_decide\|should_not_exist`. The bot that raised an ask may recommend its rating before the slot: `axe ask rec <ask> needed\|decide\|drop [--why "<few words>"]`, or many at once with `--file <json>` (`[{"ask","rating","why"}]`, `-` for stdin); the card marks it `(rec)` with an "Accept recs on this page" button, the owner can override any ask, and a rec is never a rating (`axe ask stats` counts `accepted`, `agreed`, `overridden`) |
| Where a repo is cloned from | `axe repo ls`; `axe repo add <name> <url> [--default-branch B]` (no credentials in the URL; each host clones with its own Git credentials); `axe repo show <name>`; `axe repo rm <name>`. A machine's `[repos.<name>] url` wins over the registry. `axe repo sync <name>` fetches and fast-forwards the run folder's `repos/<name>` (refused, unchanged, with local changes or commits); bot runs are not fetched at start |
| A bot's long-running program (never a detached process) | `axe service add <name> --command "<cmd>" [--cwd D] [--env K=V] [--restart always\|on-failure\|never] [--bot B]` (`--bot` defaults to `$AXE_BOT`); `axe service list [--all] [--json]`, `start`, `stop`, `logs <name> [-n N]`, `remove`. The node runs it in the bot's run folder, restarts it with backoff, wakes the bot on a crash loop (then alerts the owner) and before a restart ("Services stop in N s"), and starts it again when it is back. Env is stored in plain text: secrets go in sealed forms |
| A command a bot runs on a schedule on the node (crontab, not a task) | `axe service add <name> --command "<cmd>" --cron "<5 fields>" [--timezone TZ] [--timeout 10m] [--no-catch-up]`: one run per cron time (skipped while the last still runs), killed past its timeout; a timeout or non-zero exit is a `failed` event; missed runs are not replayed beyond one catch-up at node start. `list`/`show` give last run, exit and next run |
| Which harness, model and effort runs work | `axe rule list`; `axe rule add --match <key>=<value> --use <harness>[:<model>[:<effort>]]` (keys workload, repo, goal, label, bot, owner; `--default` matches all; repeat `--use` for fallbacks; `--priority` breaks ties); `axe rule rm <id>`; `axe rule log` |
| A secret, code or file from the owner (never in chat or prompt) | `axe form ask --bot <sender> --purpose "<what for>" --field <name>:<secret\|text\|code\|choice=A\|B\|file>[:required][:env=VAR][:prefix=TEXT][:len=MIN..MAX][:chars=digits\|nospace] --wait` (declare a format for tokens and ids so the page catches bad pastes): the owner gets a Telegram button from `--bot` (required; a bot that can write to the owner's private chat, e.g. your own `$AXE_BOT` or the task owner's bot); values land as 0600 files in `$AXE_SECRETS_DIR/<name>` (`env=VAR` also on later runs). Read them only in commands (`"$(cat "$AXE_SECRETS_DIR/token")"`), never print them. `--git <host>` stores a git credential the node's clones use. `axe form show\|wait <form>`. To test forms (post-deploy or CI checks), never prompt the owner: `axe form ask --no-telegram ...` (no `--bot`), then open the printed link in a browser or `axe form submit` a sealed reply |
| A secret, link or small file to the owner (never in chat as plaintext) | `axe form send --bot <sender> --purpose "<shown in chat, never secret>" --secret-file <f\|-> \| --link <url> \| --file <f> [--ttl 15m]` on the node host: encrypted there to the owner's reply key, sent as an "Open sealed message" button; the owner decrypts it in the form page with a passphrase. Needs the reply key once: `axe form reply-key --bot <sender> --wait` (the owner types a passphrase in a sealed form) |
| The owner needs the web portal (every page and read needs a sign-in) | `axe web login-link` prints a one-time link (10m, `--ttl` up to 1h) that sets a 30-day session in the browser that opens it; `--tg --bot <sender>` sends it sealed instead (as `axe form send`). Never send the token |
| Which logins (runtimes) the node has and their health | `axe runtimes` (`--json`): each runtime's harness, state (ok, limited until T, needs_login, degraded, disabled), last failure and quota; Telegram `/runtimes`: the list with *Re-login* (a sealed form for a token or API key, or a Codex device code) and *Remove* (confirmed; refused with live runs) per runtime, and *Add Claude* / *Add Codex* (next free name, its own login dir, kept in `[runtimes] dir`) |
| Cheaper routine runs | Routine template fields `pre_check` (a command; exit 0 marks the task done with no agent), `model`, `effort` and `skills` (the only skills in the run folder): [references/routines.md](references/routines.md) |
| Attached knowledge | `axe doc attach <path> <global\|label:l\|owner:o\|repo:r>`; a run folder holds each attached skill whole under `skills/<name>/` and lists docs by name and description (load a body with `axe doc get <path>`); `axe context explain <id>` shows what a task's run includes and why; `axe doc rm <path> [--detach]` removes a doc or folder (history kept) |

Escalation classes: `decision` is a legitimate human call (product, risk,
authorization); `capability` means the worker lacked tooling, context,
instructions or verification; `coordination` means scheduling, ordering or
recovery failed; `runtime` means axe or the infrastructure failed.

Node task sessions cannot approve their own task. Run `axe --help` for
the commands by primitive and `axe <noun> --help` (`axe task --help`,
`axe task <verb> --help`) for every flag. The old top-level task forms
(`axe add`, `axe show`, ...) still work as hidden aliases.

Bots are a separate primitive with their own commands under `axe bot`
(and `axe bot tg` for the Telegram gateway); see `axe bot --help` and
`axe bot tg --help`. A bot follows a task with `axe bot watch <task>`, or all
tasks it owns with `axe bot subscribe --owned-wake done,escalated`. Service and
routine events use the same commands: `axe bot subscribe --service [BOT/]NAME
--wake started,stopped,failed,crash_loop,run_finished` or `--routine ID --wake
fired,failed` (`--all-services`, `--all-routines`; `unsubscribe` takes the
same flags); a new service or bot-owned routine starts subscribed to its
failures. To rename a bot
and keep everything it has: `axe bot rename OLD NEW --why "..." --owner-ref
"msg #N"` (an owner decision; refused while it runs). The old name stays
reserved, and the Telegram bot keeps talking for it without a config change.
To rename a project (its tasks, runs, bots, subscriptions, goals, routines,
label and `OLD/` docs at once): `axe project rename OLD NEW --why "..."
--owner-ref "msg #N" --dry-run` first, then without `--dry-run` (an owner
decision; refused while its tasks run). The old name is not kept as an alias.

Bots pursuing goals: see [references/goals.md](references/goals.md).

## Rollout interruptions

A node rollout (a bounded drain) aborts unprotected runs after
`drain_abort_seconds` (default 15 minutes) and resumes them in the same session
with no run budget spent. A run told it was interrupted must re-check real
state (PR, deploy record, worktree) before repeating any step. Protect only the
non-repeatable step, not the whole run.

## Spec format

The spec is the session's pinned working plan: what to deliver, how, what is
authorized in advance and where to stop for a human.

```markdown
## Outcome
<observable end state, one or two sentences>

## Verify
<commands or checks that prove the outcome, and the evidence to record>

## Plan
<optional: the approach as ordered steps; a human gate sits at its step>

## Context
<optional: links, constraints, related tasks, recorded decisions>

## Deliver
target: <a delivery target the project defines>
done-when: <the observable state that completes the task, e.g. "PR merged">
approval: none | owner — <before which step, and the evidence to present>
<param>: <value>
```

Outcome and Verify are required. Without `## Deliver`, the project's default
applies.

The session does everything the spec covers without asking: the Plan's steps,
routine choices delegated to it and, with `approval: none`, shipping. It stops
only at a named gate (`approval: owner` waits for `axe task approve`) or for
something the spec does not cover, after finishing the independent work.

## Where axe runs

`axe server` owns the database and serves the HTTP API; every other command,
including `axe node`, is a client of it.

| Setting | Source |
|---|---|
| Server address | `$AXE_SERVER` (default `http://127.0.0.1:8790`) |
| Write token | `$AXE_TOKEN`, or the file at `$AXE_TOKEN_FILE` (default `~/.config/axe/token`) |
| Config | `$AXE_CONFIG` (default `~/.config/axe/config.toml`) |

Node task sessions have `AXE_ACTOR=agent`, `AXE_SESSION`, `AXE_TASK`,
`AXE_SERVER` and `AXE_WORKTREE` (the task's run workspace) set. Bot sessions have
`AXE_ACTOR=bot:<name>`, `AXE_BOT`, `AXE_BOT_RUN`, `AXE_SESSION` and
`AXE_WORKTREE`, and no `AXE_TASK`.
