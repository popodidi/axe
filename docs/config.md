# axe configuration reference

This reference is derived from `crates/axe/src/config.rs` and the modules that
read it. When this file and the code disagree, the code wins.

## Files and precedence

| What | Default path | Override | Notes |
|---|---|---|---|
| Config file | `~/.config/axe/config.toml` | `AXE_CONFIG` | A missing file means all defaults. A file that exists but fails to parse or validate is an error. |
| Bearer token | `~/.config/axe/token` | `AXE_TOKEN_FILE` (path); `AXE_TOKEN` (value, CLI only) | Trimmed; a missing or blank file means no token. |
| Database | `~/.local/share/axe/axe.db` | `AXE_DB` | Opened only by `axe server`. |
| Agent logs | `~/.local/state/axe/logs` | `log_dir` in the config | Dispatcher side. |
| Admission hold | `~/.local/state/axe/admission-hold.json` | `AXE_ADMISSION_HOLD` | Dispatcher host. |
| Server address (client side) | `http://127.0.0.1:8790` | `AXE_SERVER` | Used by every command except `server` and `config`. |
| Listen address (server side) | `127.0.0.1:8790` | `axe server --listen <addr>` | |

Precedence: an environment variable that is set and non-empty wins over the
default path; there is no CLI flag for the config, token or database paths.
Config values themselves have no environment overrides.

`~` and `~/...` are expanded in `workspace_root`, `log_dir`,
`resources.path` and `telegram.bot.token_file`. Other paths are taken as given.

Both `axe server` and `axe dispatch` load the same config path. The server
uses `[server]`, `[telegram]`, `[metrics]` and the agent names (to validate
task and routine agent pins); the dispatcher uses everything else. Keep agent
names aligned when the two run on different machines. The config is read once
at startup; there is no reload.

Inspect the effective config with `axe config` (prints the config and DB
paths, then the resolved TOML, including the built-in `claude` agent) or
`GET /api/config` (never the token; Telegram ids and token paths are hidden).

Unknown keys are rejected everywhere except inside `[worktrees]`, which ignores
unknown keys from older configs. Retired keys (`personas`, `looped`,
`persona_dir`, `pool.priority`, `continuity`, `match.persona`, `concurrency`)
therefore fail validation.

## Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `workspace_root` | string (path) | `"~/workspace"` | Parent of project checkouts. A task of project `p` runs in `<workspace_root>/p` (or its task worktree). |
| `log_dir` | string (path) | `"~/.local/state/axe/logs"` | Agent run logs and prompts (`<task>-<run>.log`, `<task>-<run>.prompt.md`), bot run files under `bots/<name>/`, dispatcher journal under `.dispatcher/`. |
| `poll_seconds` | integer | `60` | Dispatch pass interval. Must be >= 1. |
| `drain_abort_seconds` | integer | `900` | After SIGUSR1 starts a drain, seconds before unprotected task runs are aborted. |

## `[server]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cors_origins` | array of strings | `[]` | Browser origins that get `Access-Control-Allow-Origin` on `GET` responses. Preflights allow `GET` only. |

Auth is not configured here: writes require `Authorization: Bearer <token>`
matching the token file. Without a token file the server refuses to listen on
a non-loopback address; on loopback it starts with unauthenticated writes and
a warning.

## `[pool]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `total` | integer | `1` | Concurrent task agents. Must be >= 1. |
| `bots` | integer | omitted | Concurrent bot runs, a separate pool from `total`. Omitted means one slot per active bot (never capped). If set, must be >= 1. |

## `[defaults]`

Dispatch settings for every project, before `[[rule]]` overrides.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `agents` | array of agent names | `["claude"]` | Candidate agents for a new session. Must be non-empty and name configured agents. |
| `strategy` | `"fixed"` or `"round_robin"` | `"fixed"` | `fixed` takes the first agent; `round_robin` rotates through `agents` with a cursor stored in the database. |
| `max_runs` | integer | `6` | Runs per task; a ready task with `runs >= max_runs` is escalated instead of claimed. |
| `max_resumes` | integer | omitted (unlimited) | Resumes of one session before the next run starts fresh. `0` means always fresh. A run aborted by a drain costs no resume. |

## `[[rule]]`

Per-project overrides. Every field except `match` is optional.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `match` | inline table `{ project = "<name>" }` | `{}` (every project) | Only `project` is supported. |
| `agents` | array of agent names | inherit | Same checks as `defaults.agents`. |
| `strategy` | `"fixed"` or `"round_robin"` | inherit | |
| `max_runs` | integer | inherit | |
| `max_resumes` | integer | inherit | |

Resolution: catch-all rules are applied first, then project rules, each in
file order; each field comes from the last applicable rule that sets it,
falling back to `[defaults]`. The round-robin cursor is keyed by the match of
the rule that supplied `agents` (`project=<name>` or `project=*`, or
`defaults`). A task pinned with `axe add --agent NAME` uses that agent for
fresh sessions regardless of rules; limits still come from the rules.

## `[agents.<name>]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `cmd` | array of strings | required | Command for a fresh session. Must be non-empty. |
| `resume_cmd` | array of strings | omitted | Command to resume a stored session. Without it the agent's sessions always restart fresh. If present, must be non-empty. |
| `session` | string | `"none"` | `preset`: the dispatcher generates a UUID and passes it as `{session_id}`. `jsonl:<key>`: the session id is read from the first JSON output line carrying `<key>`, falling back to `session_id`. `none`: no session tracking. |
| `bot_args` | array of strings | omitted | Bot runs only: arguments spliced in before the first argument containing `{prompt}` (appended if none). Agents without it cannot run bots. |

Placeholders, replaced in each argument:

| Placeholder | Allowed in | Value |
|---|---|---|
| `{session_id}` | `cmd`, `resume_cmd`, `bot_args` | Session id (empty when unknown, e.g. a fresh `jsonl` or `none` agent) |
| `{prompt}` | all | The rendered prompt text |
| `{cwd}` | all | Working directory of the run |
| `{task_id}` | all | Task id (empty for bot runs) |
| `{project}` | all | Project name |
| `{system_prompt_file}` | `bot_args` only | `<log_dir>/bots/<name>/system.md` |
| `{settings_file}` | `bot_args` only | `<log_dir>/bots/<name>/settings.json` |

Any other `{identifier}` is rejected at load time. Braces that do not enclose
a plain identifier (letters, digits, `_`) are left alone, so literal JSON in an
argument is fine.

The built-in `claude` agent is added when no `[agents.claude]` is defined.
Defining `[agents.claude]` replaces it entirely (it is not merged), so restate
`session = "preset"` and `bot_args` if you still want them. Built-in values:

```toml
[agents.claude]
cmd = ["claude", "-p", "--output-format", "stream-json", "--verbose", "--session-id", "{session_id}", "--permission-mode", "bypassPermissions", "{prompt}"]
resume_cmd = ["claude", "-p", "--output-format", "stream-json", "--verbose", "--resume", "{session_id}", "--permission-mode", "bypassPermissions", "{prompt}"]
session = "preset"
bot_args = ["--system-prompt-snapshot", "off", "--append-system-prompt-file", "{system_prompt_file}", "--settings", "{settings_file}"]
```

## `[worktrees]`

Presence of the table turns on dispatcher-owned task worktrees: every session
of a task runs in `<workspace_root>/.worktrees/<project>/task-<id>--work`, a Git
worktree of the canonical clone `<workspace_root>/<project>` (bots use
`.../bot-<name>`). Without the table, sessions start in
`<workspace_root>/<project>`. The canonical clone needs its own `.git` and
`refs/remotes/origin/HEAD`; otherwise the project's oldest ready task is
escalated before any claim.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `branch_prefix` | string | `"axe/"` | Branch for a new task worktree is `<branch_prefix>task-<id>--work`. |

An empty `[worktrees]` table is valid and uses the default prefix.

## `[resources]`

Presence of the table turns on resource admission: no new task session starts
while disk, memory or CPU is tight. Bot runs stop only at a hard floor (disk
or memory strictly below half its pause threshold, or unreadable). Every key
is optional.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `path` | string (path) | `workspace_root` | Filesystem whose free space is watched. |
| `sample_seconds` | integer | `10` | Sampling interval. Must be >= 1. |
| `stale_seconds` | integer | `30` | A reading older than this, or a CPU window spanning more than `cpu_window_seconds + stale_seconds`, is unknown (and pauses). Must be >= `sample_seconds`. |
| `cpu_window_seconds` | integer | `60` | CPU busy-share window. Must be >= `sample_seconds`. |
| `disk_pause_below_gib` | float | `15.0` | Pause when free space is strictly below. |
| `disk_resume_above_gib` | float | `20.0` | Recover when free space is strictly above. Must be > `disk_pause_below_gib`. |
| `memory_pause_below_percent` | float | `15.0` | Pause when `MemAvailable / MemTotal` is strictly below. |
| `memory_resume_above_percent` | float | `20.0` | Recover when strictly above. Must be > `memory_pause_below_percent`. |
| `cpu_pause_above_percent` | float | `90.0` | Pause when CPU busy share is strictly above. |
| `cpu_resume_below_percent` | float | `80.0` | Recover when strictly below. Must be < `cpu_pause_above_percent`. |

All thresholds must be finite and >= 0; percent values must be <= 100.

The admission hold (`axe admission hold|release`) is not configured in TOML;
it is the JSON file at `AXE_ADMISSION_HOLD` on the dispatcher host.

## `[otel]`

Controls the `OTEL_RESOURCE_ATTRIBUTES` the dispatcher passes to each agent
run. axe only tags runs; export is the agent's job.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `prefix` | string | `"axe"` | Prefix of per-run keys. |
| `resource` | table of string to string | `{}` | Static attributes, written first. |

Per-run keys: task runs get `<prefix>.task_id`, `<prefix>.project`,
`<prefix>.agent`, `<prefix>.mode` (`fresh` or `resume`) and `<prefix>.run`
(the claim id); bot runs get `<prefix>.bot`, `<prefix>.project`,
`<prefix>.agent`, `<prefix>.mode` and `<prefix>.bot_run`. These are appended
to any inherited `OTEL_RESOURCE_ATTRIBUTES`, with `%`, `,`, `=`, `;`, `"`,
`\`, whitespace and control characters percent-encoded.

## `[metrics]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `projects` | array of strings | `[]` | Projects reported under their own `project` label on the server's `/metrics`; every other project is `other`. Names must be non-empty ASCII letters, digits, `-`, `_` or `.`; `other` and `all` are reserved. |

## `[prompts]`

Dispatcher task prompts. `{id}` and `{project}` are replaced; other text is
literal (no placeholder validation). Omitted keys use the built-in text.

| Key | Type | Default |
|---|---|---|
| `fresh` | string | `Work axe task {id} in project {project}: run axe show {id} and axe log {id}, then carry it to done. Around a merge or deploy: axe protect {id}, then --clear.` |
| `resume` | string | `Task {id} resumed; run axe log {id} for what changed.` |
| `interrupted` | string | `Your previous run was interrupted by an axe rollout, not by a failure. Its last step may or may not have taken effect: re-check real state (PR, deploy ledger, worktree) before repeating any step.` |

`fresh` is used for a task's first run and for any fresh-session fallback;
`resume` for resuming a stored session; `interrupted` is appended (after a
space) to the prompt of the run that follows a drain abort. Bot prompts are
built in and not configurable here.

## `[telegram]`

Server only. Presence of the table starts one long-poll gateway per bot
token inside `axe server`. It requires the `telegram` cargo feature (on by
default); a server built without it logs a warning and ignores the table.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `owner_user_id` | integer | required | Numeric Telegram user id of the only person who may control the bots. Must be > 0. |
| `api_base` | string | `"https://api.telegram.org"` | Bot API base URL (tests point it at a stub). |
| `poll_timeout` | integer | `30` | `getUpdates` long-poll timeout in seconds. Must be 1..=50. |
| `board` | `"repost"` \| `"edit"` | `"repost"` | No effect: it configured the retired pinned overview. Still accepted so older configs load. |

### `[[telegram.bot]]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | required | The axe bot this Telegram bot talks for. Must be unique within `[telegram]`. |
| `token_file` | string (path) | required | File holding the BotFather token. Must be non-blank. A missing or empty file disables that bot with a warning; the server still starts. |
| `chats` | array of integers | required | Chat allowlist for inbound and outbound messages (groups are negative). Must be non-empty. |

Tokens are read from files only and never stored in the config or database.

## Full example

```toml
# ~/.config/axe/config.toml (or $AXE_CONFIG)

workspace_root = "~/workspace"          # project checkouts live in <root>/<project>
log_dir = "~/.local/state/axe/logs"     # run logs, prompts, dispatcher journal
poll_seconds = 60                       # dispatch pass interval, >= 1
drain_abort_seconds = 900               # SIGUSR1 drain grace before aborting unprotected runs

[server]
cors_origins = ["http://localhost:3000"]  # origins allowed to GET the API; default none

[pool]
total = 4                               # concurrent task agents; default 1
# bots = 2                              # concurrent bot runs; default one per active bot

[defaults]
agents = ["claude"]
strategy = "fixed"                      # fixed | round_robin
max_runs = 6                            # runs per task before escalation
# max_resumes = 3                       # omitted: unlimited; 0: always fresh

# The built-in claude agent exists without this table; define it only to replace it.
[agents.codex]
cmd = ["codex", "exec", "--json", "--sandbox", "danger-full-access", "-C", "{cwd}", "{prompt}"]
resume_cmd = ["codex", "exec", "resume", "--json", "-c", 'sandbox_mode="danger-full-access"', "{session_id}", "{prompt}"]
session = "jsonl:thread_id"             # preset | jsonl:<key> | none (default none)
# bot_args = [...]                      # required only to run bots with this agent

[[rule]]                                # no match: every project
agents = ["claude", "codex"]
strategy = "round_robin"

[[rule]]
match = { project = "web" }             # project rules apply after catch-all rules
agents = ["codex"]
max_runs = 10

[worktrees]                             # presence enables task worktrees
branch_prefix = "axe/"                  # default

[resources]                             # presence enables resource admission
# path = "~/workspace"                  # default workspace_root
sample_seconds = 10
stale_seconds = 30
cpu_window_seconds = 60
disk_pause_below_gib = 15
disk_resume_above_gib = 20
memory_pause_below_percent = 15
memory_resume_above_percent = 20
cpu_pause_above_percent = 90
cpu_resume_below_percent = 80

[otel]
prefix = "axe"
resource = { "service.namespace" = "agents" }

[metrics]
projects = ["web"]                      # own label on /metrics; others are "other"

[prompts]                               # {id} and {project} are replaced
# fresh = "Work axe task {id} in project {project}: ..."
# resume = "Task {id} resumed; run axe log {id} for what changed."
# interrupted = "Your previous run was interrupted by an axe rollout, ..."

[telegram]                              # server only; presence enables the gateway
owner_user_id = 123456789
# api_base = "https://api.telegram.org"
# poll_timeout = 30                     # 1..=50

[[telegram.bot]]
name = "assistant"
token_file = "/run/secrets/telegram/assistant"
chats = [123456789]
```

## Environment variables

Empty values count as unset everywhere.

### Read by the CLI, server and dispatcher

| Variable | Read by | Meaning |
|---|---|---|
| `AXE_CONFIG` | all that load config (`server`, `dispatch`, `config`, `usage backfill`) | Config file path. |
| `AXE_DB` | `server`, `config` (prints it) | Database path. |
| `AXE_TOKEN_FILE` | server, CLI, dispatcher | Token file path. The server compares writes against it; clients send its contents. |
| `AXE_TOKEN` | CLI and dispatcher (clients) | Token value; takes precedence over the token file. Not read by the server. |
| `AXE_SERVER` | CLI and dispatcher (clients) | Server base URL, default `http://127.0.0.1:8790`. |
| `AXE_RETRY_SECS` | CLI | How long CLI commands retry while the server is unavailable; default `120`, `0` disables retries. Non-numeric values fall back to the default. |
| `AXE_ACTOR` | CLI (`--actor`) | Actor recorded on events; default `human`. |
| `AXE_SESSION` | CLI and dispatcher (clients) | Session recorded on writes. |
| `AXE_BOT` | CLI `bot` subcommands | Default bot name. |
| `AXE_BOT_RUN` | CLI `axe bot inbox` (`--run`) | Default bot run id. |
| `AXE_WORKTREE` | CLI attachment download | Default output directory is `$AXE_WORKTREE/.inbox`, else `./.inbox`. |
| `AXE_ADMISSION_HOLD` | dispatcher, `axe admission hold/release` | Hold file path. |
| `OTEL_RESOURCE_ATTRIBUTES` | dispatcher | Inherited value that per-run attributes are appended to. |

### Set by the dispatcher for agent sessions

| Variable | Task runs | Bot runs |
|---|---|---|
| `AXE_ACTOR` | `agent` | `bot:<name>` |
| `AXE_SESSION` | Session id, or the `dispatch:<uuid>` claim until a JSONL id is known | Session id, or `bot-run:<run>` |
| `AXE_TASK` | Task id | Removed from the inherited environment |
| `AXE_BOT` | Inherited unchanged | Bot name |
| `AXE_BOT_RUN` | Inherited unchanged | Bot run id |
| `AXE_SERVER` | The dispatcher's own server URL | Same |
| `AXE_WORKTREE` | Worktree path with `[worktrees]`; removed otherwise | Same (bot worktree) |
| `OTEL_RESOURCE_ATTRIBUTES` | Inherited value plus `[otel]` attributes | Same |

The token is not placed in the agent environment; agents inherit
`AXE_TOKEN_FILE` (or its default) from the dispatcher.

## Validation errors worth knowing

Errors are reported at load time, prefixed with `config <path>`.

| Error | Cause |
|---|---|
| `unknown field ...` | Typo, retired key, or a key in the wrong table (TOML parse error). |
| `poll_seconds must be >= 1` | `poll_seconds = 0`. |
| `agents.<name>.cmd is empty` / `agents.<name>.resume_cmd is empty` | Empty command array. |
| `agents.<name>.session: invalid session mode ...` | Not `preset`, `none` or `jsonl:<non-empty key>`. |
| `agents.<name>: unknown template variable {x} (expected one of ...)` | Unsupported placeholder; `{system_prompt_file}` and `{settings_file}` outside `bot_args` also land here. |
| `defaults: agents list is empty` / `rule[i] (project=...): unknown agent "x"` | Empty or unknown agent list in `[defaults]` or a rule. |
| `metrics.projects: "x" must be ASCII letters, digits, '-', '_' or '.'` / `... is a reserved label` | Bad or reserved (`other`, `all`) label. |
| `pool.total must be >= 1` / `pool.bots must be >= 1 (omit it ...)` | Zero pool size. |
| `resources.sample_seconds must be >= 1`, `resources.cpu_window_seconds must be >= sample_seconds`, `resources.stale_seconds must be >= sample_seconds` | Timing constraints. |
| `resources.<key> must be a finite number >= 0` / `must be <= 100` | Threshold out of range. |
| `resources.disk_resume_above_gib must be greater than disk_pause_below_gib` (and the memory and CPU equivalents) | Resume threshold not strictly on the healthy side of pause. |
| `telegram.owner_user_id must be the owner's numeric user id` | `owner_user_id <= 0`. |
| `telegram.poll_timeout must be 1..=50 seconds` | Out of range. |
| `telegram.bot "x" is configured twice` / `chats allowlist is empty` / `token_file is empty` | Bot entry problems. |

Checked later, not at load:

- Creating a task or saving a routine with an agent pin the server's config
  does not know: `unknown agent "x"`.
- A bot whose agent has no `bot_args`: `agent "x" has no bot_args; it cannot
  run bots`.
- `axe server` on a non-loopback `--listen` without a token file: `refusing to
  listen on non-loopback ... without a token`.
- Dispatch of a task whose pinned agent was removed from the dispatcher config
  escalates the task with an unknown-agent note.
