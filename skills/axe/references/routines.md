# Routine cost controls

A routine template (`axe routine save --file <json>`, `routine::Template`)
can set four optional fields that change how the node runs the tasks it
creates. Each task keeps the template snapshot it was created from, so a later
routine edit does not change an existing task. Omit a field to keep today's
behaviour. A server older than these fields refuses a template that sets
them; an older node ignores them and runs the agent as before.

| Field | Value | Effect |
|---|---|---|
| `pre_check` | shell command (`sh -c`), up to 4096 bytes | Runs before the task's first agent run, in its run folder, with `AXE_TASK`, `AXE_SERVER`, `AXE_WORKTREE`, `AXE_SECRETS_DIR` and `AXE_ACTOR=pre-check`. **Exit 0**: the task is set `done` with no agent run, and the routine gets a `pre-check-skipped` event. **Any other exit, a signal or 15 minutes without exiting**: the agent starts as usual under the same claim. Either way the last 3000 bytes of its output are a task note (`pre-check exit N: ...`), and the full output is in `<log_dir>/<task>-<run>.pre-check.log`. A resumed or retried task never runs it again. A node stop or rollout drain kills it (and a drain starts no agent after it) and returns the task to pending, and the next claim runs it again |
| `model` | model name (letters, digits, `. _ - [ ] /`) | The agent's model, over the selection rule's (`axe rule`) |
| `effort` | effort name (same characters) | The agent's reasoning effort, over the rule's. Claude Code gets `--effort`, Codex `-c model_reasoning_effort=...` |
| `skills` | list of skill names | The only skills in the run folder (`skills/`, `.claude/skills`, the `AGENTS.md` index), from the repos' own (`.agents/skills`) and the attached ones. `[]`: none. Needs a task with `repos`, because only a run workspace has a generated `skills/` folder. Skills installed for the CLI's user (`~/.claude/skills`, plugins) are outside the run folder and still load |

`model` and `effort` replace only the part they set: a rule entry
`claude-code:opus:high` with `"effort": "low"` runs `opus` at `low`. They
apply on whichever runtime the rule (or a pinned `agent`) picks, if its
harness is Claude Code or Codex.

The run's `run_start` event shows the result: `use` is the entry it ran with
(`claude-code:claude-sonnet-5-5:low`) and `routine` is the profile
(`{"routine":5,"model":...,"effort":...,"skills":[...],"pre_check":...}`).
Read it with `axe task log <id> --full`.

```json
{
  "name": "Issue intake", "project": "p", "cron": "30 */2 * * *",
  "title": "Queue tickets", "detail": "## Outcome\n...", "repos": ["tn-clam"],
  "pre_check": "python3 skills/intake/scripts/precheck.py",
  "model": "claude-sonnet-5-5", "effort": "low",
  "skills": ["intake"]
}
```

A pre-check that exits 0 must be sure there is nothing for the agent to do.
Exit nonzero on any doubt, including its own errors, and the agent runs.
