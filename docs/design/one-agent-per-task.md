# One agent per task

Design decision, 2026-10-30. axe used to run each task through three personas
(builder, verifier, delivery), each with its own session, persona system prompt
and a handoff command. That is retired: one agent session carries a task from
spec to done, and the dispatcher resumes the same session whenever the task
returns to `pending`.

## Why

- Agents follow a well-written spec reliably across one long session. Splitting
  it cost round trips and lost context without improving quality, security or
  accuracy.
- The split fit one kind of work (build, verify, ship). Operations tasks that
  only needed delivery still passed through builder and verifier.
- Almost none of the separation was enforced in code; it lived in prompts.
- What an operator needs from axe is orchestration: many sessions organized in
  one place, each resumable, with a human stepping in only when needed.

## What changed

- axe has no `owner` or persona: no `handoff`, no `--owner`, no persona-keyed
  admission (one `pool.total`), one session per task (schema v9). Dispatcher
  sessions start as plain sessions in the task worktree with a one-line prompt;
  no system prompt or persona files.
- The task protocol lives in the deployment's agent instructions (for example
  a shared `AGENTS.md`) and the `axe` skill. Project rules stay in each
  project's own instructions and delivery policy.
- The spec is the workflow: Outcome and Verify required; Plan, Context and
  Deliver optional (see the `axe` skill, Spec format).
- Independent review for risky work is the human approval gate
  (`approval: owner` in the spec, then `axe approve`). Dispatcher task sessions
  cannot approve. A bot session (actor `bot:<name>`, see [Bots](bots.md)) may
  approve on an explicit owner message, citing its id in the note; that counts
  as the owner's approval. A session may run a review subagent of its own.
- Stepping in means resuming the task's agent session with the command the
  dashboard shows; axe has no attach feature.
- No backward compatibility was kept: older schemas, config keys and the legacy
  `ops` owner migration were deleted.

Standard workflows for a kind of task, if they emerge, belong in project docs
or skills, not in axe.
