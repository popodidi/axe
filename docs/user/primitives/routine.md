---
title: "Routine"
description: "A saved cron template that creates an ordinary task each time it fires."
---

A routine is a task template on a five-field cron schedule in a time zone.
Each time it fires, the server creates an ordinary pending task from the
template; the node runs it like any other task. After downtime only the
latest missed occurrence is created.

## When to use it

- Recurring work: a weekly dependency review, a daily report, a nightly
  data check.
- Work that usually has nothing to do: a `pre_check` shell command runs
  first, and exit 0 marks the task done with no agent run.

For something a bot should do itself on a schedule (look at its board,
check in with you), use a bot schedule instead; for a process rather than a
task, a scheduled [service](service.md).

## Main commands

| Command | What it does |
|---|---|
| `axe routine save --file r.json` | Create a routine (`--id N` replaces one) |
| `axe routine list` / `axe routine show ID` | See routines and their next run |
| `axe routine trigger ID --key K` | Fire it now; the same key returns the same task |
| `axe routine pause ID` / `resume ID` / `archive ID` | Stop or retire it |
| `axe routine history ID` | The tasks it created |

Template fields are in `axe routine save --help`: `name`, `project`, `cron`,
`timezone`, `title`, `detail`, and optionally `agent`, `owner`, `repos`,
`labels`, `pre_check`, `model`, `effort` and `skills`.

## Example

```sh
cat > r.json <<'JSON'
{
  "name": "weekly-deps",
  "project": "web",
  "repos": ["web"],
  "cron": "0 9 * * 1",
  "timezone": "Europe/London",
  "title": "Weekly dependency review",
  "detail": "## Outcome\nOutdated dependencies are listed in a task note, with a PR for safe patch bumps.\n## Verify\n- CI passes on the PR."
}
JSON
axe routine save --file r.json
axe routine trigger 1 --key first-try
```

More: [Routines, bots and goals](../concepts/routines-bots-goals.md).
