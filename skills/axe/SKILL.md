---
name: axe
description: "Use the axe task queue: queue tasks, read their logs, change state, escalate, wait, approve and record evidence. Also use for any session started by the axe dispatcher ($AXE_TASK or $AXE_BOT is set)."
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

`axe` is a task queue and dispatcher, not a workflow engine. A task has a
project, a spec and a state, and every change is logged (`axe log <id>`).
The dispatcher runs one agent session per task, from spec to done, and resumes
that same session every time the task comes back. The workflow is whatever the
spec says; how a project ships, what needs a human and how to test live in the
project's own instructions (for example its `AGENTS.md`), not in axe.

This skill covers how the tool behaves. Owner workflows (agreeing a spec,
resolving holds, delivery and authorization policy) belong to the deployment's
own skills and instructions.

## States

- **pending**: the dispatcher claims it and starts or resumes its session.
- **in_progress**: a session is working on it.
- **escalated**: a hold for a human. Needs an explicit resume.
- **blocked**: an external wait with no known time. Needs an explicit resume.
- **waiting**: `axe wait --for/--until` resumes automatically at the time;
  `blocked_by` edges resume it when prerequisites are `done`. Edges never lift
  an escalation.
- **done**: the completion contract. Dependents start when a task is done, so
  set it only when the spec's `done-when` holds.

A session that exits with its task still `in_progress` is escalated by the
dispatcher, as are dispatcher errors and `max_runs` exhaustion (resume with
`axe set <id> --reset-runs --state pending`).

A known wait (a queued check, a release, a contended fixture) uses a timed or
dependency wait, not an escalation. Poll in-session only for checks due within
a few minutes; for longer waits, `axe wait` and exit, so the same session
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
axe wait 42 --until 2026-01-05T10:15Z --note "check run 123; green -> merge at head abc123"
axe wait 42 --for 15m --note "run 123 still queued; recheck, green -> merge"
```

## Commands

| Situation | Command |
|---|---|
| Queue a spec | `axe add --project <p> --title "<imperative>" --detail-file <spec>` (`--blocked-by <ids>` for prerequisites, `--agent <name>` to pick an agent) |
| Owner, repos, labels | `axe add`/`axe set` with `--owner <bot:name\|person>` (`none` clears), `--repo <r>` (repeat; `--no-repos` for none), `--label <l>` (`--unlabel` on set); `axe list --label <l> --owner <o>` |
| Read a task | `axe show <id>`, `axe log <id>` (`--full` for whole event bodies) |
| Done | `axe set <id> --state done --note "<what holds now>"` |
| Needs a human | `axe escalate <id> --blocked ... --resolver ... --resume ... --class decision\|capability\|coordination\|runtime` |
| A human approves shipping | `axe approve <id> --sha <head> --scope <target>` (resumes the task) |
| Blocker resolved | `axe set <id> --state pending --note "<what changed>"` |
| Needs another task first | `axe wait <id> --on <ids> --note "<why>"` |
| Timed wait | `axe wait <id> --for 15m\|--until <time> --note "<what to check>"` (sized as above) |
| Evidence | `axe note <id> "<command and key output>"`, `axe artifact <id> <url>` |
| Before a non-repeatable step (merge, deploy, migration) | `axe protect <id> --note "<step>"`; if it fails, do not start the step. After it: `axe protect <id> --clear` |
| Human attention queue | `axe list --state escalated` |
| Attached knowledge | `axe doc attach <path> <global\|label:l\|owner:o\|repo:r>`; a run folder holds each attached skill whole under `skills/<name>/` and lists docs by name and description (load a body with `axe doc get <path>`); `axe context explain <id>` shows what a task's run includes and why |

Escalation classes: `decision` is a legitimate human call (product, risk,
authorization); `capability` means the worker lacked tooling, context,
instructions or verification; `coordination` means scheduling, ordering or
recovery failed; `runtime` means axe or the infrastructure failed.

Dispatcher task sessions cannot approve their own task. Run `axe --help` and
`axe <command> --help` for every flag.

Bots are a separate primitive with their own commands under `axe bot`
(and `axe bot tg` for the Telegram gateway); see `axe bot --help` and
`axe bot tg --help`. A bot follows a task with `axe bot watch <task>`, or all
tasks it owns with `axe bot subscribe --owned-wake done,escalated`.

Bots pursuing goals: see [references/goals.md](references/goals.md).

## Rollout interruptions

A dispatcher rollout (a bounded drain) aborts unprotected runs after
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
only at a named gate (`approval: owner` waits for `axe approve`) or for
something the spec does not cover, after finishing the independent work.

## Where axe runs

`axe server` owns the database and serves the HTTP API; every other command,
including `axe dispatch`, is a client of it.

| Setting | Source |
|---|---|
| Server address | `$AXE_SERVER` (default `http://127.0.0.1:8790`) |
| Write token | `$AXE_TOKEN`, or the file at `$AXE_TOKEN_FILE` (default `~/.config/axe/token`) |
| Config | `$AXE_CONFIG` (default `~/.config/axe/config.toml`) |

Dispatcher task sessions have `AXE_ACTOR=agent`, `AXE_SESSION`, `AXE_TASK`,
`AXE_SERVER` and, with task worktrees, `AXE_WORKTREE` set. Bot sessions have
`AXE_ACTOR=bot:<name>`, `AXE_BOT`, `AXE_BOT_RUN`, `AXE_SESSION` and
`AXE_WORKTREE`, and no `AXE_TASK`.
