# axe HTTP API v1

Reference for the HTTP JSON API served by `axe server`. Derived from
`crates/axe/src/server.rs`, `remote.rs` and `client.rs`; the README sections
"Auth", "API", "Runs and live activity" and "Remote abort" describe the same
behavior in prose.

## Base URL and versioning

- `axe server --listen <addr>` binds the API; the default listen address is
  `127.0.0.1:8790` (`DEFAULT_LISTEN`). Clients use `$AXE_SERVER`, default
  `http://127.0.0.1:8790` (`client::DEFAULT_SERVER`).
- Examples below use `$AXE_SERVER`, for example `http://localhost:8790`.
- Paths are unversioned: everything lives under `/api/*`, plus `/healthz` and
  `/metrics`. "v1" in this document means the current stable surface; there
  is no `/v1/` prefix and no version header.
- HTTP/1.1, one request per connection (`Connection: close`). Every response
  carries `Cache-Control: no-store`. At most 256 concurrent connections;
  socket read/write timeout 30 s.
- Unknown routes return `404 {"error":"no route for METHOD /path"}`.

## Auth

The server token is the trimmed contents of `$AXE_TOKEN_FILE` (default
`~/.config/axe/token`). Clients send `Authorization: Bearer <token>`; the CLI
takes the token from `$AXE_TOKEN`, else the token file. Comparison is
constant-time.

| Request | Token required |
|---|---|
| `GET` (all paths not listed below) | No |
| `GET /api/bots/{name}/tg/...` (history, files) | Yes |
| `GET /api/dispatcher/connect` (WebSocket) | Yes |
| `OPTIONS` (CORS preflight) | No; answers `204` |
| Every other method (`POST`, `PUT`, `PATCH`, ...) | Yes |

- Missing or wrong token: `401 {"error":"unauthorized"}`; nothing changes.
- With no token file the server refuses to start on a non-loopback address.
  On loopback without a token every request is accepted, including writes and
  private reads (local development only; a warning is printed).
- CORS: `GET` responses carry `Access-Control-Allow-Origin` (and
  `Vary: Origin`) only for origins listed in `server.cors_origins` (default
  none). Preflights allow `GET` only, so browsers cannot write. Write
  responses never carry CORS headers.
- Some public reads are redacted: bot wake lists blank owner wake bodies and
  message text/attachments; `/api/config` replaces the `telegram` table with
  the configured bot names and never includes the token.

## Conventions

### Bodies

- Request and response bodies are JSON (`Content-Type: application/json`)
  except: `/metrics` (Prometheus text), `/api/stream` (SSE), `POST
  /api/backup` and Telegram file downloads (binary), and the `send-file`
  upload (raw bytes).
- An empty request body is parsed as `{}`. Invalid JSON: `400 {"error":"invalid JSON body: ..."}`.
- A request with `Transfer-Encoding` and no `Content-Length` gets `411`;
  chunked bodies are not supported.

### Actor and session (`Who`)

Most write bodies flatten the `Who` struct:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `actor` | string | `"human"` (also when empty) | Recorded on events, e.g. `agent`, `bot:<name>` |
| `session` | string | null (also when empty) | Agent session id, recorded on events |

The CLI client adds `actor` and `session` (from `$AXE_SESSION`) to every
write. Dispatcher endpoints, `/api/usage/ingest` and Telegram actions do not
read `Who`.

### Errors and status codes

Errors are `{"error": "<message>"}`.

| Status | When |
|---|---|
| `200` / `201` / `202` | Success; `201` on creation, `202` for an accepted but unconfirmed abort |
| `400` | Validation or any otherwise unclassified error |
| `401` | Missing or wrong token |
| `403` | Forbidden action (`db::Forbidden`), e.g. an approval from a live dispatcher session |
| `404` | Unknown route, or an error message ending in `not found` (task, session, run, bot) |
| `409` | Request id reused with a different request or replay of a failed request; Telegram not configured or not compiled in |
| `411` | Body without `Content-Length` |
| `413` | Body over the size limit, or an upload over the Telegram limit |
| `431` | Request head over 64 KiB |
| `500` | Database error |
| `503` | `dispatcher lease expired`; or a request id whose original attempt is still running or was interrupted |

The status line's reason phrase comes from a fixed table that has no entry for
`409`, so a `409` response reads `409 Internal Server Error`. Clients should
use the numeric code.

### Size limits

| Limit | Value |
|---|---|
| Request head (request line plus headers) | 64 KiB (`431`) |
| Request body | 8 MiB (`413`) |
| Authorized `POST /api/bots/{name}/tg/send-file` | 51 MiB transport limit; then `photo` max 10 MiB, `document`/`voice` max 50 MiB (`413`) |

### Headers

| Header | Direction | Meaning |
|---|---|---|
| `Authorization: Bearer <token>` | request | See Auth |
| `X-Axe-Request-Id` | request (writes only) | Makes a write retry-safe (below) |
| `X-Axe-File-Name` | response | Percent-encoded file name on Telegram file downloads |
| `Origin` | request | Checked against `server.cors_origins` |

The server also accepts and sends legacy header names (see compat.rs) for the
request id and file name; new clients should use the `X-Axe-*` names.

### Retry-safe writes (`X-Axe-Request-Id`)

- Ignored on `GET`. Value: 1-64 characters of `[A-Za-z0-9-]`, else `400 invalid request id`.
- The id is reserved before the write runs and its response stored after;
  ids are remembered for one hour.
- A retry with the same id and identical method, path, query and body returns
  the stored response without re-running the write.
- Same id with a different request: `409`. Original still running or
  interrupted by a restart: `503` (outcome unknown, never re-run). Original
  ended with a `5xx`: `409 request <id> already failed: ...`.
- The CLI sets one id (a UUID) per logical write and retries transient
  failures (no response, `5xx`, or a `404` without a JSON error body) for
  `$AXE_RETRY_SECS`, default 120 s; `0` disables retries.

### Change notification

Every successful (`< 300`) non-GET request, and every committed change, bumps
a change counter that `/api/stream` subscribers see.

## Resource shapes

A task is `{id, project, title, detail, state, claimed_by, runs, created_at,
updated_at, not_before, agent, status, waiting_on, deps}`. `state` is one of
`pending`, `in_progress`, `blocked`, `escalated`, `done`; `status` is the
derived state; `deps` is `{blocked_by, blocks}`, each entry `{id, project,
title, state, status}`.

A run is `{id, task_id, project, agent, mode, session_id, claim, started_at,
ended_at, exit_code, log_path, current_activity, last_activity_at, live}`.
`mode` is `fresh` or `resume`; `current_activity` is a short (at most 120
characters) description of the latest log item.

Exact shapes of other responses (bots, routines, stats, usage) are the
serialized Rust structs in `bot.rs`, `routine.rs`, `flow.rs`, `usage.rs`,
`schedule.rs`, `subscription.rs` and `channel.rs`; this document summarizes
them.

## Health and metrics

| Method | Path | Auth | Response |
|---|---|---|---|
| GET | `/healthz` | public | `{"ok": true}` |
| GET | `/metrics` | public | Prometheus text format (`text/plain; version=0.0.4`). Per-project labels only for `[metrics] projects`; others are `other` |

## Event stream

| Method | Path | Auth | Response |
|---|---|---|---|
| GET | `/api/stream` | public | `text/event-stream` |

The stream starts with `retry: 3000`, then `event: change` with data
`{"seq":N}` immediately and after every committed task, event or run change.
A `: keepalive` comment is sent after 15 s without changes. The payload carries
no data beyond the counter: clients refetch what they display.

## Tasks

| Method | Path | Query / body | Response |
|---|---|---|---|
| GET | `/api/tasks` | query `state`, `project`, `all=1` | Task list; done tasks hidden unless `all` or `state=done`. `state` must be a valid state and matches the derived `status` |
| GET | `/api/tasks/{id}` | | Task |
| GET | `/api/tasks/{id}/events` | | Event log of the task |
| GET | `/api/tasks/{id}/routine` | | Routine backlink/snapshot, or `null` |
| POST | `/api/tasks` | `{project, title, detail? = "", agent?, state? = "pending", blocked_by? = [], workload?}` + `Who` | `201 {id, task}`; task and dependency edges commit together. `agent` must be configured |
| PATCH | `/api/tasks/{id}` | `{state?, title?, detail?, note?, reset_runs? = false, not_before?, escalation_class?}` + `Who` | `{changed, task}` |
| POST | `/api/tasks/{id}/deps` | `{add? = [ids], remove? = [ids]}` + `Who` | `{changed, task}`; one transaction |
| POST | `/api/tasks/{id}/notes` | `{body}` + `Who` | `201 {task}` |
| POST | `/api/tasks/{id}/artifacts` | `{value}` + `Who` | `201 {task}` |
| POST | `/api/tasks/{id}/classify` | `{escalation?, workload?}` + `Who` | `{task}`; records escalation class and/or workload as events |
| POST | `/api/tasks/{id}/approve` | `{sha, scope, note?}` + `Who` | `{task}`; only from `blocked`/`escalated`, resumes `pending`. `403` from a live dispatcher session |
| POST | `/api/tasks/{id}/protect` | `{clear? = false, note?}` + `Who` | `{run, protected}`; marks the task's live run as protected (or clears it). Fails if there is no live run or it is being aborted |
| POST | `/api/tasks/{id}/abort` | `{run}` + `Who` | See Remote abort |
| POST | `/api/claim` | `{project?, by}` + `Who` | `{task}`, the oldest ready task, or `{task: null}` |

Escalation classes: `decision`, `capability`, `coordination`, `runtime`.
Workloads: `prod-bug`, `infra`, `feature`, `ops`.

### Remote abort

`POST /api/tasks/{id}/abort` with `{run: RUN_ID}`; `run` is required and must
belong to the task.

| Response | Meaning |
|---|---|
| `202 {run, status: "requested"}` | Accepted and persisted; the task is held `escalated` and cannot be reopened until the run closes; termination not yet confirmed |
| `200 {run, status: "confirmed", task_state}` | The owning dispatcher confirmed termination (also returned for repeats after confirmation) |
| `400` | Run not in task, run already ended, or stale dispatcher generation |

Repeat the same request with the same `run` to poll. The CLI (`axe abort`)
polls for up to 60 s and treats only `confirmed` as success. The server
forwards abort run ids to the dispatcher over the control WebSocket.

## Runs

| Method | Path | Query | Response |
|---|---|---|---|
| GET | `/api/runs` | `task`, `live=1`, `limit` (default 50, clamped to 1..1000) | Runs, newest first |
| GET | `/api/runs/{id}/log` | `after` or `before` (byte offsets, not both), `generation`; other parameters are `400` | Log page relayed from the owning dispatcher (below) |

The server stores no log content. A log read is forwarded over the
dispatcher's WebSocket and waits up to 5 s. Responses are always `200`:

| Body | Meaning |
|---|---|
| `{run, page: {generation, start, end, size, lines: [{start, end, text, fragment}]}, log_complete}` | A page of the log |
| `{run, unavailable: "Dispatcher unavailable" \| "Dispatcher busy", retryable: true}` | Owning dispatcher not connected, busy, or timed out; poll again |
| `{run, unavailable: "..."}` | Not retryable, e.g. the historical log path was reused by another attempt, or the log is not registered on this dispatcher |

## Sessions

| Method | Path | Response |
|---|---|---|
| GET | `/api/sessions/{session_id}` | `{session_id, task_id, project, agent, run}` for an agent session id or a `dispatch:` claim; `run` is the latest run or null. `404` if unknown |

## Overview, admission, config, stats, usage

| Method | Path | Query / body | Response |
|---|---|---|---|
| GET | `/api/overview` | | `counts` (`{project, state, count}`, `state` is the derived status), `admission` (as `/api/admission`), `ops_queue` (holds and dependency waits: task plus `kind` `hold`/`dependency`, `actionable`, `blocked_by`, `blocked_at`, `blocker`), `running` (live runs), `bots` (bot summaries) |
| GET | `/api/admission` | | Last dispatcher admission report: `enabled`, `admitting` (null when unknown), `reason`, `checked_at`, `age_seconds`, `report_age_seconds`, `signals`, `thresholds`, `hold` (when a dispatcher reports one), `bots_admitting` |
| GET | `/api/config` | | `{config_path, db_path, auth, config}`; `auth` is whether a token is set; `config.telegram` is reduced to `{bots: [names]}` |
| GET | `/api/stats/flow` | `project` (required), `since` (default `14d`), `until` (default `0s`), `workload` | Delivery figures for the project |
| GET | `/api/stats/history` | | `{events, routines, runs}` for the dashboard Stats view, uncapped |
| GET | `/api/stats/cost` | `since` (default `30d`), `until` (default `0s`), `step_days`, `breakdown`, `project`, `source`, `bot`, `routine`, `agent`, `workload`, `model`, `options=1` | Cost report (`usage::cost_stats`). `breakdown` is one of `project, source, bot, routine, workload, wake, agent, model, mode, cache, context, outcome, day, week, kind`; `step_days` must be > 0 and yield fewer than 400 buckets |
| GET | `/api/usage/catalog` | | Runs with usage records, used by the CLI to backfill usage |
| POST | `/api/usage/ingest` | `{items: [{kind, run, usage}]}` | `{changed}`; token required, no `Who` |

`since`/`until` take relative durations such as `14d` or `0s` (parsed by
`flow::parse_bound`); see that function for the accepted forms.

## Backup

| Method | Path | Auth | Response |
|---|---|---|---|
| POST | `/api/backup` | token | `application/octet-stream`: a consistent SQLite copy of the database |

## Routines

Reads are public; writes need the token and take `Who`.

| Method | Path | Query / body | Response |
|---|---|---|---|
| GET | `/api/routines` | | All routines, including archived |
| GET | `/api/routines/{id}` | | One routine |
| GET | `/api/routines/{id}/history` | `before` (id cursor) | Occurrences, newest first, at most 100 rows |
| GET | `/api/routines/{id}/events` | `before` (id cursor) | Routine events, newest first, at most 100 rows |
| POST | `/api/routines` | `{template: {name, project, cron, timezone? = "UTC", title, detail, agent?, workload?}}` + `Who` | `200` routine (not `201`) |
| PUT | `/api/routines/{id}` | same as POST | `200` routine |
| POST | `/api/routines/{id}/pause` \| `resume` \| `archive` | `Who` | Routine |
| POST | `/api/routines/{id}/trigger` | `{key}` (retry key, required) + `Who` | `{task_id}` |

## Bots

Reads are public unless noted; writes need the token and take `Who`. Several
writes require a `why` audit string.

| Method | Path | Query / body | Response |
|---|---|---|---|
| GET | `/api/bots` | | Bot summaries |
| GET | `/api/bots/{name}` | | One bot |
| GET | `/api/bots/{name}/wakes` | `undelivered=1`, `limit` (default 50, max 1000) | Wakes; owner wake bodies and message text/attachments redacted |
| GET | `/api/bots/{name}/events` | `limit` (default 50, max 1000) | Bot events |
| GET | `/api/bots/{name}/activity` | `limit` (default and max 1000) | Metadata only: `messages` `{id, direction, kind, reply_to, edit_of, attachments, at}` (attachment count, no text) and `task_actions` `{task_id, at, actor, kind}` |
| POST | `/api/bots` | `{bot: {name, project, agent? = "claude", identity?, conduct?, notes?, nudge_after?}}` + `Who` | `201` created bot. `heartbeat` is retired and refused |
| PATCH | `/api/bots/{name}` | `{identity?, conduct?, nudge_after?, why}` + `Who` | Edited bot |
| PUT | `/api/bots/{name}/notes` | `{notes, why}` + `Who` | Result of `bot::set_notes` |
| POST | `/api/bots/{name}/wakes` | exactly one of `in_seconds` (> 0), `at`, `now: true`; plus `{note, key?}` + `Who` | `201 {wake, due_at}` |
| POST | `/api/bots/{name}/inbox` | `{run?, take? = false, hook? ("post-tool" \| "stop"), stop_hook_active?}` + `Who` | Inbox result (`bot::inbox`); `hook` implies `take`. Used by the bot's agent hooks |
| POST | `/api/bots/{name}/pause` \| `resume` \| `stop` \| `reset` | `Who` | Bot after the change |

### Bot schedules

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/bots/{name}/schedules` | | Schedules |
| POST | `/api/bots/{name}/schedules` | `{name, cron, timezone? = "UTC", objective, why}` + `Who` | `201` schedule |
| POST | `/api/bots/{name}/schedules/{schedule}/pause` \| `resume` \| `remove` | `{why}` + `Who` | `{schedule, action}` |

### Bot subscriptions

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/bots/{name}/subscriptions` | | Subscriptions |
| POST | `/api/bots/{name}/subscriptions` | `{project?, wake?: [..], ride?: [..], batch_seconds?, why}` + `Who` | `200` subscription (creates or changes) |
| POST | `/api/bots/{name}/subscriptions/remove` | `{project?, why}` + `Who` | `{removed: true}` |

## Bot runs

| Method | Path | Query | Response |
|---|---|---|---|
| GET | `/api/bot-runs` | `bot`, `live=1`, `limit` (default 50, max 1000) | Bot runs |
| GET | `/api/bot-runs/{id}` | | One bot run |
| GET | `/api/bot-runs/{id}/log` | as `/api/runs/{id}/log` | Log page relayed from the run's dispatcher (`kind: "bot_log"`) |

## Telegram (`tg`) actions

Available only when the server is built with the `telegram` feature and has a
`[telegram]` table; otherwise `409`. All `tg` routes need the token,
including reads.

| Method | Path | Query / body | Response |
|---|---|---|---|
| GET | `/api/bots/{name}/tg/history` | `limit` (default 50, max 1000), `before` (message id), `search` | Messages, oldest first |
| GET | `/api/bots/{name}/tg/file/{msg}[/{n}]` | | Raw attachment bytes streamed from Telegram (not stored); `Content-Type` from the file, name in `X-Axe-File-Name`. `n` selects an attachment |
| POST | `/api/bots/{name}/tg/send` | `{text, chat?, reply_to?, thread?, html? = false, buttons? = [[label, data], ...], silent? = false}` | Send result |
| POST | `/api/bots/{name}/tg/send-file` | raw file body; query `as` (`photo` \| `document` \| `voice`, default `document`), `name` (default `file`), `caption`, `chat`, `reply_to`, `thread` | Send result |
| POST | `/api/bots/{name}/tg/edit` | `{msg, text?, html? = false, buttons?}`; `buttons: []` removes the keyboard, absent leaves it | Edit result |
| POST | `/api/bots/{name}/tg/pin` \| `unpin` | `{msg, silent? = false}` | Result |
| POST | `/api/bots/{name}/tg/react` | `{msg, emoji? = ""}` | Result |
| POST | `/api/bots/{name}/tg/delete` | `{msg}` | Result |
| POST | `/api/bots/{name}/tg/typing` | `{chat?}` | Result |

`msg` and `reply_to` are local message ids (from history), not Telegram ids.
Default `chat`: the chat of `reply_to`, else the latest inbound chat, else the
first allowlisted chat. Unknown actions return `404`. Result shapes are those
of the gateway methods in `telegram.rs` and are not specified here.

## Dispatcher (internal)

These endpoints are used by `axe dispatch` and are not a stable client API.
All need the token. Lease-checked calls fail with `stale dispatcher
ownership` (`400`) for a wrong id/generation, or `dispatcher lease expired`
(`503`) when the lease lapsed. The lease is 60 s, renewed by heartbeats.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/dispatcher` | | `{owner: [id, expires] \| null, connected}`; public read |
| POST | `/api/dispatcher/register` | `{id, generation}` (`""` to request a new one) | `{generation}`; refused while another owner's lease is active or runs/claims are unresolved |
| POST | `/api/dispatcher/operation` | `{dispatcher, generation, operation_id, op, body}` | Operation result; a replayed `operation_id` returns the stored result, a reused id with a different request is `400` |
| POST | `/api/dispatcher/recovery` | `{id}` | `{unresolved}`: open runs, claims or bot runs owned by `id` |
| POST | `/api/dispatcher/reconcile` | `{id, generation, note}` (note required) | `{ok: true}`; escalates in-progress dispatcher claims (class `runtime`), ends open runs, closes bot runs, releases ownership |
| POST | `/api/dispatcher/admission` | `{id, generation, admission}` | `{ok: true}`; stores the admission report served by `/api/admission` |
| POST | `/api/dispatcher/log-catalog` | `{id, generation}` | Ended runs not owned by another dispatcher |
| POST | `/api/dispatcher/adopt-logs` | `{id, generation, runs: [ids]}` | `{ok: true}`; registers ended runs' logs to this dispatcher |
| GET | `/api/dispatcher/connect` | query `id`, `generation`; WebSocket upgrade (version 13) | `101`; control channel. The server sends `{kind: "heartbeat" \| "log" \| "bot_log", ..., aborts, dispatcher_generation}` at least every 10 s and the dispatcher replies to each. A new connection replaces the previous one |

Operation names (`op`): `pending`, `events`, `task`, `session`, `claim`,
`hold`, `round_robin`, `note`, `block`, `prepare`, `started`, `activity`,
`drain_abort`, `finish`, `bot_ready`, `bot_control`, `bot_claim`,
`bot_started`, `bot_activity`, `bot_finish`. Each runs in one transaction
together with its replay record. Their body fields are defined only in
`remote.rs::operation` and are subject to change.

## Not specified by the code

- No response schema is declared for bot, routine, stats, usage or Telegram
  results beyond the Rust structs that serialize them.
- There is no pagination beyond `limit`/`before` where listed; `/api/tasks`
  is unpaged.
- The dispatcher claims through `op: "claim"`, while `/api/claim` is used by
  the `axe claim` CLI command. Whether `/api/claim` is a stable client API is
  not stated in the code or README.
