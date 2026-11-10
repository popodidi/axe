# Harvis taskq workspace

Read-only Next.js App Router / TypeScript application. All task data stays in
 taskq; the browser uses GET only. No credentials or mutation UI are shipped.

## Development and checks

Node 24. `npm ci && npm run lint && npm run typecheck && npm test && npm run build`.
Browser regressions use the production build and a delayed read-only API fixture:
`npx playwright install chromium && npm run test:browser`.

Use Playwright’s default per-user browser cache (on Linux,
`~/.cache/ms-playwright`) so worktrees using the same browser revision reuse
one download. Do not set `PLAYWRIGHT_BROWSERS_PATH` to a checkout-local path
for routine CI or verification. Playwright selects the browser revision for
each installed package version; dependency installs and test state remain
local to each worktree. See [browser cache management](https://playwright.dev/docs/browsers#managing-browser-binaries).
Reports and test results stay ignored inside the worktree.
`npm run dev` for development. `npm start` serves the exported build on port
8080 (`PORT` override). `/healthz` returns `{ok:true}`; `/api/version` returns
`{commit:...}` from `GIT_COMMIT` (development when unset).

Release verification follows the [hlab-service runbook](../../docs/operations/runbooks/hlab-service.md).
Its HTTP check requires the dashboard title, Next.js asset references, no legacy
Go toggle markup, and working task API/SSE. Quiet uses native system fonts, so
an Inter reference is not a release requirement. Watch compatibility, readiness,
image digests and the exact served commit are checked separately by that runbook.

## URL contract

The single `/` route uses query parameters, preserved across client navigation:

| Parameter | Default | Meaning |
| --- | --- | --- |
| view | tasks | Tasks, Routines, Bots, Stats or Cost; legacy `overview`, `ops` and `live` redirect to Tasks |
| project | empty | Exact filter across views |
| state | active | Tasks only: `active` hides done; `all` includes done, or exact taskq status |
| status | empty | Routines only: empty is All; `enabled`, `paused` or `archived` |
| q | empty | Case insensitive search: Task ID/title/spec |
| days | 7 | Stats: 7, 30, 90 UTC calendar days through today, or custom; bot stats: 7, 30 or 90 |
| type | empty | Stats: empty is All; `normal` or `routine` scopes the whole report |
| start, end | empty | Custom creation dates (YYYY-MM-DD), both dates inclusive, UTC |
| sort | newest | Tasks only: creation order, newest or oldest, ID breaks ties |
| page | 1 | Tasks only: 25 tasks per page |
| task | empty | Selected task ID, including outside current filters |
| routine | empty | Selected routine ID in Routines (including archived) |
| bot | empty | Selected bot name in Bots; a bare `?bot=<name>` opens the Bots view |
| botrun | empty | Bot run ID whose log overlay is open (`/api/bot-runs/:id/log`) |
| focus | empty | Focused task row ID |
| tab | spec | Task: spec, events, runs, artifacts. Bot: profile (default), wakes, runs, events, stats |
| taskq | http://taskq.hlab | API base URL; browser origin must be allowed by taskq CORS |
| listY, detailY | 0 | Pane scroll offsets in CSS pixels |

View, filter, task, page, and tab changes push history. Row focus replaces
history. Scrolling also replaces the current URL without adding history, at
most once per 250 ms per pane (browsers rate-limit history writes); in-app
navigation snapshots both offsets first.
Back/Forward, reloads, and shared URLs restore pane offsets after async content
loads. A restored focused row is brought into view if needed. Opening another
task or tab starts its detail at the top; Back restores the previous task/tab's
offset. Changing list filters/view/page resets the list offset. Manual scrolling
and live data refresh do not snap back to the focused row.

Tasks opens with **Active** selected (all non-done tasks, including waits and
escalations). Explicit filters and task deep links survive legacy redirects.
Dependencies retain derived status and links. The detail pane works from both
Tasks and Stats; live runs and Watch remain in its Runs tab.

### Cost view

Cost makes one read, `GET /api/stats/cost?since=&until=&step_days=&options=1` (plus any of `project`, `source`, `bot`, `routine`, `agent`, `model`, `workload`, `breakdown`), whatever the run count;
taskq aggregates server-side over runs that ended in the period (`days`: 7, 30 or
90 UTC calendar days). Every breakdown covers every run, so their totals agree.
Cost is an API-price estimate (Claude Code's `modelUsage.costUSD`), not a bill,
and token types have no per-type cost. Output includes thinking, so the token
table separates them. Runs without usage are unknown and shown as such, never
as zero: agents that report no result event (Codex today), killed runs, logs
that are gone, and resumes whose session counters went backwards. A killed run's
spend lands on the session's next known run. Cold or warm compares cache read
to cache write on the run's first main-thread API call. Styled with the Harvis
Quiet brand guide (`docs/reference/brand-guide.md`) through the shared tokens.

Its Over time charts reuse the Stats charts and buckets (`step_days` 1 up to 31
days, else 7, anchored on the range start). A period with unknown runs, or one
before the earliest known run (`series.first_known`), is shaded as incomplete
and only known usage is plotted. A period with no known usage has no point.

### Stats scope and source contract

The date range selects currently done tasks whose latest actual completion
transition falls within inclusive UTC calendar dates. Reopened open tasks are
excluded; re-completed tasks use their latest completion. Creation may precede
the range. Open tasks appear only as a count, independent of dates. Project and
search filters apply throughout; state is only a Tasks filter. Presets and custom
dates persist in the URL across navigation.

Stats is a one-page delivery report: a header card holds its filters
(project, period with custom UTC dates, group by) and Refresh history; there
is no search box, and a task search in the URL does not apply. Below it: key
figures (completed, median elapsed and runtime, runs per task, share
escalated), completions per day, four metric trends, two scatter plots (runs × escalations;
elapsed × runtime on log axes with a "runtime = elapsed" line) whose dots open
the task, the histograms and the summary tables. Exact values stay available
as text (bar totals and a screen-reader data list per chart).
Four histograms show actual runs, escalation transitions, wall-clock elapsed
(including waits/reopened periods), and summed agent runtime. Runtime includes
concurrent spans additively; unfinished or invalid spans make runtime unknown.
Counts use records, never the resettable run budget. Notes and repeated state
snapshots are not escalation or completion transitions.

Trends (task 235), below completions per day, plot each of the four metrics
over time: the mean per UTC completion bucket with a shaded ±1 SD band and the
known sample count (n) per bucket. Ranges up to 31 days use daily buckets;
longer ranges use seven-day weeks from the range start (the last may be
shorter). SD is the population SD, so one sample has SD 0; buckets without
known samples are gaps (no marker or band, only a faint dashed bridge), never
zero. Unknown metrics are excluded and counted as unknown. The band's lower
bound is clipped at 0; duration charts use a log axis from one minute, like the
elapsed × runtime scatter. When split, every group draws its mean line but only
the highlighted group (legend button, hover or keyboard focus) draws its band,
so bands never overlap; more than six groups collapse to the five largest plus
Other. Focus a chart and use arrow keys (left/right buckets, up/down groups) or
hover a point for its date, n, mean and SD; a screen-reader list holds every
value.

Histograms have explicit zero and overflow buckets. Duration buckets have
unequal widths and display task counts, not density. Split by project or coding
agent stacks exclusive groups in the same buckets; exact labeled counts avoid
reliance on color. Mixed-agent tasks form `Mixed: <sorted recorded agents>`;
legacy agent names are retained. Each task contributes once. No recorded runs
means `No recorded agent`; incomplete run history means `Unknown / partial`.
Summary tables show task count and each metric's known/unknown sample counts,
median (middle-pair average), p90 (nearest rank), and max. Unknowns are excluded,
never converted to zero. Individual timelines and metrics remain in task detail.

Read-only API contracts: `/api/tasks?all=1` is unpaginated;
`/api/stats/history` returns, in one response, every task's actual state
transitions and `run_start` events (ID order, bodies omitted), the IDs of
tasks that are Routine occurrences, and every run's timing and agent (uncapped,
task 381). Stats makes exactly this one history read,
whatever the task or run count, never a read per task (task 364 replaced a
per-task events and routine-backlink fan-out that grew to 700+ requests). Every
matching done task whose creation precedes the range end is a candidate.
Changing view, range or project aborts the in-flight read; stale results are
discarded. A failed read shows History unavailable. Missing or invalid
completion history is excluded and reported as unknown membership. Finding
more historical run_start events than run records makes run counts, runtime and agent attribution unknown. Activity
never recorded in either API cannot be reconstructed. SSE updates and split
changes do not reload history; entering Stats, changing range/project/search
or Refresh history does.

Task type: Routine means the task was created by a native Routine, scheduled
or manually triggered, whatever the parent routine's current state (paused and
archived included); every other task is Normal. Title, text, cadence and state
are never used. Unknown type remains only for histories without a
classification. The header always shows completed counts by type (Normal,
Routine, Unknown type, All) for the current dates and
project, whatever the scope. All is the default; Normal or Routine scopes the
key figures, charts, tables, open and unknown-completion counts. Switching
type, or grouping by task type, reuses loaded history.

Brand guide: `docs/reference/brand-guide.md` (Quiet), using system/mono text,
neutral bars with legends and exact counts, borderless sections, compact spacing and table primitives
in both themes. Unit fixtures cover cohort boundaries, accounting, buckets,
percentiles, unknowns and read bounds; browser fixtures cover ranges/navigation,
split controls, summaries, keyboard use, narrow/wide layouts and all load states.


### Bots

Generic bot monitoring (`docs/decisions/taskq-bots.md`, Dashboard); nothing is
specific to one bot. Read-only and metadata-only: wakes come back with message
text redacted, `/api/bots/:name/activity` carries no text, file or chat ids,
and the dashboard never calls the inbox or any write.

`?view=bots` lists every bot (`/api/bots`): name, project and agent; state
(Paused, or Needs attention when paused by repeated failures); the live run
and its activity (`/api/bot-runs?live=1`); next wake and queued wakes; the
latest inbound and outbound message times (`activity?limit=200` per bot); and
failures. Search and project filter apply. A row opens `?bot=<name>` in place
of the list; Back or Esc returns, focusing the row.

Detail facts (project, agent, live run, next wake, session, failures)
and `hv <project> taskq bot show|log <name>` commands sit above five tabs:

- **profile:** identity, conduct and notes, each with its edit history (the
  `identity`/`conduct`/`notes` events, rendered as diffs);
- **wakes:** kind, due, creator, delivered run or Queued, and via prompt or hook;
- **runs:** `RunList` over `/api/bot-runs?bot=` (resume commands start in the
  bot's persistent `.worktrees/<project>/bot-<name>` worktree); View log or
  Watch opens the `run-log.tsx` overlay via `?botrun=` on `/api/bot-runs/:id/log`;
- **events:** every bot event, security drops tagged;
- **stats:** `lib/bot-stats.ts` over 7, 30 or 90 UTC days through today:
  runs per day; runtime mean ± population SD (live runs excluded) with a
  `TrendChart`; wakes by kind; reply latency (a turn opens at the first
  inbound message, not a reaction or edit, after the last outbound one and
  closes at the next outbound one; open turns are unanswered); mid-run hook
  deliveries, prompt deliveries and follow-up runs (a run whose prompt carried
  a wake created during the previous run); resumed vs fresh runs; failures,
  owner stops, pauses (owner and automatic) and security drops; and task
  actions in the bot's project by `bot:<name>` vs `human` (the Lv1 metric).
  Each read is the latest 1000 records; a read at the cap is reported as
  possibly truncated.

Detail reads (bot, wakes, events, runs, activity) reload on every taskq change
(SSE `change`, or polling while disconnected), like the task list.
`tests/bots.spec.ts` covers the list (both themes, wide and phone), the detail
tabs, stats and the bot run log overlay; `lib/bot-stats.test.ts` the metrics.

List and detail are independently scrollable. Search updates the URL as typed.
API refresh preserves scroll and stale content on temporary errors. SSE `change` reloads data, with five-second polling
while disconnected. Running duration ticks locally every second.

Clicking anywhere in a task row opens it; the title button stays the keyboard
target. Keyboard (outside form controls): j/k focus rows, Enter opens the
focused row (Tasks), Esc closes the topmost pane (task, then routine, then bot) and drops `tab`, / focuses search. Views are ordered Tasks, Routines, Bots, Stats, Cost: g then t/r/b/s/c; legacy g then v opens Tasks.
Shortcut hints sit beside each view link and in its tooltip. The refresh
button next to the connection state reloads taskq data immediately. On phones
the navigation collapses into a top bar and detail occupies the full workspace.

Layout: the chevron beside the `Harvis` brand collapses the navigation rail to
a 48px strip (view initials, connection dot, refresh, theme glyph; labels stay
the accessible names). Three dividers (`role="separator"`,
`components/dashboard/detail-resizer.tsx`) resize panes by dragging, arrow keys
(16px, Shift 64px) or Home/End; double-click restores the default: the rail's
right edge (216px default, 168-320px, yielding to 22vw on narrow desktops),
the routine pane's left edge (392px, 320-560px) and the detail pane's left
edge (48%, at least 360px, leaving the list 224px). CSS enforces the bounds at
any viewport size. The collapsed state and all three widths persist in
`localStorage` (`harvis-layout`) and apply before first paint
(`lib/layout-script.ts`). Only the collapse toggle animates the rail. Phones
ignore all of it and keep the top bar and full-width panes with Back.

Task detail header: id and state label, title, a facts grid (project, runs,
created, updated, generating routine), dependencies, then the copyable
commands in one well. Tabs: spec, events, runs and artifacts.

Runs use 64px collapsed rows: recorded run ID, process state, duration, a
single-line activity/result and Watch or View log. The labelled disclosure
supports mouse, Enter and Space; expansion reveals the full result, agent,
mode, exit code and exact copyable commands. Finished describes the process,
not task success. Task identity appears once in task detail.

Artifacts use 37px table rows, newest first, with a count, search and type
filter. PR, CI, file and Lark record labels come only from recorded URLs;
unknown URLs and non-link references stay lossless. Details exposes the full
reference, copy action, source, actor and timestamp. In narrow panes Source moves
into Details. No metadata fetches are made. `tests/compact-evidence.spec.ts`
checks collapsed geometry and expanded controls at four widths in both themes;
`EVIDENCE_DIR` optionally saves synthetic screenshots and measurements.

Copyable commands use `hv` (`hv/src/main.rs`; `hv <project> <cmd...>` runs
the command in `~/workspace/<project>` on hlab over ssh): `hv <project> taskq
log <id>`, `hv <project> taskq show <id>`, `hv <project> tail -f <log_path>`,
and for ended runs `hv <project> sh -c 'cd ~/workspace/.worktrees/<project>/task-<id>--work && exec claude --resume <sid>'`
(or `exec codex resume <sid>`). Dispatcher sessions run in the task worktree
(`taskq/src/worktree.rs`) and Claude Code finds sessions by cwd, so resume
starts there rather than in `~/workspace/<project>`. Arguments are quoted with hv's rule (safe words bare, anything
else single-quoted); `lib/model.ts` builds them and `lib/model.test.ts` pins
the exact strings.

The sidebar's Light theme toggle (`aria-pressed`) switches palettes. Dark is
the default; the choice is stored in localStorage `harvis-theme` (wrapped in
try/catch) and an inline head script (`lib/theme-script.ts`) sets
`data-theme="light"` on `<html>` before first paint.

Specs and event bodies use GFM, sanitized HTML, and highlighted fenced code;
spec edit unified diffs use addition/deletion colors. Artifact links permit
HTTP(S) only. Clipboard has a textarea fallback for plain HTTP deployments.

## Deployment

The dashboard ships as an immutable image through tagged CD (`infra/cd`,
service `dashboard`). `Dockerfile` installs locked dependencies and exports
Next.js in a build stage; the runtime stage holds only `node`, the export and
`server.mjs`, with the source commit embedded as `GIT_COMMIT` (served by
`/api/version`). It has no git, source credentials or package manager, and
runs as uid 1000 with a read-only root filesystem.

Each `main` push touching `infra/dashboard/**` runs
`.github/workflows/publish-dashboard-image.yml`: it pushes
`ghcr.io/popodidi/harvis-dashboard:sha-<commit>-run-<id>-<attempt>`, pulls it
back by digest, runs `smoke_image.sh` (offline boot, `/healthz`, embedded commit,
export, no build tooling), and uploads the `dashboard-image-release` provenance
artifact. Publication never deploys. Delivery releases a merged commit by
pushing `deploy/dashboard/YYYYMMDD.N`; CD substitutes the verified digest for
the `:cd-release` placeholder in `deployment.yaml` and the kubelet pulls with
`harvis-ghcr-read`. Ingress remains `harvis.hlab`; probes and `/api/version`
are preserved. To check an image locally:

```bash
docker build --build-arg GIT_COMMIT="$(git rev-parse HEAD)" -t harvis-dashboard:local infra/dashboard
infra/dashboard/smoke_image.sh harvis-dashboard:local "$(git rev-parse HEAD)"
```

## Task sorting

Click ID, Task, Project, State or Created to sort ascending; click again for
descending. Headers show the direction and expose it to assistive technology;
Enter and Space activate the buttons. State sorts by the displayed category,
Created by timestamp, and text alphabetically without case distinctions.
Equal values use ascending task ID; empty values stay last in both directions.
Sorting applies before pagination and resets to the first page. The URL retains
selection through filtering, automatic refresh, reload and Back/Forward.
Until a sort is selected the existing newest-first order remains unchanged;
the Sort menu still supports the original newest/oldest options.

## Design system

Harvis Quiet: `docs/reference/brand-guide.md`, `docs/reference/taskq-design.md`
(the dashboard contract) and `docs/quiet-design-tokens.css`, with owner
overrides (20261103, ported from tn-eng-dashboard): sections are bordered
neutral cards on a `--surface-nav` canvas, tables have a filled header row and
row hairlines, the sidebar and panes are `--surface-1` with one hairline,
selects are the styled listbox in `components/ui/select.tsx`, and restrained
accents (`app/styles/accent-tokens.css`) tint task-state labels (pending
purple, running blue, held amber, escalated danger, done teal), the live dot,
charts and the selected option. The Quiet description below is the base those
overrides modify. Monochrome and
borderless: group with space, alignment and weight before any line or fill.
Dark by default with the light palette behind `data-theme="light"`; the native
system font stack (no remote font); monospace only for ids, commands, paths,
code and timestamps. Composition: the navigation rail on `--surface-nav` with no
border (`Harvis`, view links with key hints, connection, refresh, taskq base,
theme toggle; the active view has a neutral fill and 600 weight); one 24px page
heading per view; filters with visible sentence-case captions; tables with no
header band or grid lines, about 32px rows, neutral hover and selected fills, a
600 selected title and a separate focus ring; detail on the canvas with no card
around the spec, opaque sticky tabs with one travelling indicator, and a single
resizer hairline that strengthens on hover/focus. Task states are a decorative
symbol plus text: pending/blocked/done secondary, running primary, escalated
danger. Healthy connection is neutral; reconnecting warns. Charts are neutral
with legends and exact counts. Diffs keep success/danger with `+`/`-`.

| Layer | Path | Rule |
| --- | --- | --- |
| Brand tokens | `app/styles/brand-tokens.css` | Verbatim copy of `docs/quiet-design-tokens.css`; `lib/tokens.test.ts` fails on any drift. Fix with `cp docs/quiet-design-tokens.css infra/dashboard/app/styles/brand-tokens.css`. `docs/design-tokens.css` is no longer used by the dashboard. |
| Breakpoints | `app/styles/breakpoints.module.css` | `@value` query tokens imported by feature modules; px allowed only here. |
| App tokens | `app/styles/app-tokens.css` | Only `var(--brand-token)` expressions (plus `1px`, unitless numbers, and durations). |
| Primitives | `components/ui/` | CSS modules on tokens: Button (primary, secondary, ghost, link, nav; `sm` default, `md` opt-in, `icon` press), Tag, StatusPill/StatusIcon, Panel/PanelHead/PageTitle (borderless sections), SectionLabel, Table/Th/Td, Input/Search/Select, Filter, CodeBlock/DiffBlock, Prose, EmptyState, Skeleton, Alert, Kbd, LiveDot, Tabs, Commands/Command/CopyButton. |
| Features | `app/dashboard.tsx`, `components/dashboard/` | Compose primitives; layout-only CSS modules. |

App tokens:

| Token | Value | Use |
| --- | --- | --- |
| `--space-half`, `--space-quarter` | `--space-4px`, `--space-2px` | Dense chrome, focus ring |
| `--space-compact`, `--space-cozy`, `--space-snug` | `--space-6px`, `--space-10px`, `--space-12px` | Quiet micro-steps |
| `--cell-padding` | `--space-1` `--space-cozy` | Table cells (rows about 32px) |
| `--pane-padding`, `--pane-padding-phone` | 12px 16px 16px, 8px | List, detail, and log viewer scroll panes |
| `--section-head-gap`, `--section-gap` | 8px, 24px | Borderless sections: heading gap, gap between groups |
| `--inset-padding`, `--inset-gap` | 8px 12px, 8px | Log records |
| `--toolbar-padding` | 8px 12px | Header/toolbars (log viewer) |
| `--control-min-height` | `--space-3` (24px) | Minimum height of `sm` buttons (WCAG 2.5.8 target) |
| `--cell-padding-compact` | 6px 8px | Detail pane table cells (`.detailPane` resets `--cell-padding`) |
| `--prose-compact-*` | `--text-control`, `--lh-control`, 8px, 16px, 8px 10px | Compact detail Markdown (`Prose compact`) |
| `--nav-width`, `--nav-collapsed-width` | `--space-8` + `--space-5` (168px), `--space-5` | Navigation rail, expanded and collapsed |
| `--detail-width`, `--detail-min-width`, `--detail-size` | 48%, `--space-8` x 3, saved px | Detail pane default, minimum, and saved width |
| `--list-min-width` | `--space-8` x 2 - `--space-2` (224px) | Narrowest list beside detail |
| `--resizer-width`, `--resizer-line` | `--space-1`, 1px | Detail divider hit area and resting hairline |
| `--search-min-width`, `--cmd-label-width`, `--dot-size`, `--icon-size`, `--skeleton-line` | spacing/line-height tokens | Controls and state symbols |
| `--row-hover-bg`, `--row-selected-bg` | `--surface-hover`, `--surface-selected` | Neutral row feedback (focus uses Quiet `--focus-ring`) |
| `--z-sticky`, `--z-toast` | 1, 10 | Sticky tabs, copy feedback |
| `--transition-hover`, `-feedback`, `-disclosure`, `-layout` | 120, 160, 200, 275ms with Quiet easing | Hover, local feedback, disclosure, pane/rail layout |
| `--skeleton-delay` | 150ms | First-load skeleton delay (static, no pulse) |
| `--state-<state>-fg` | secondary; in_progress primary; escalated `--status-danger` | StatusPill |
| `--live`, `--idle`, `--warn`, `--diff-add`, `--diff-remove` | primary, disabled, warning, success, danger | Connection, live dot, diffs |

### Interaction and motion

Implements `docs/reference/taskq-design.md`. Durations are Quiet tokens, which
reduced motion zeroes; JS timers read the same tokens (`lib/motion.ts`) and
cleanup never waits for `transitionend`/`animationend`. The newest action wins.

- **Detail pane** (desktop): opening moves the boundary 275ms and content
  enters by 4px (160ms); switching tasks only replaces content; closing keeps
  the old content mounted but `inert` until the boundary returns, then
  unmounts. Keyboard opening focuses the detail title (`tabindex=-1`); closing
  returns focus to the invoking row, or the page heading. Back/Forward, reloads
  and stored widths paint without motion; dragging/keys resize with no easing.
- **Navigation** collapse animates the rail width only after hydration
  (`data-motion="ready"` on `<html>`), so the stored state paints correctly.
- **Tabs**: one indicator travels 160ms; the active label is 600 weight with
  reserved width. **Disclosures** reveal measured content (200ms,
  `interpolate-size`) and rotate a chevron. The **run-log dialog** enters with
  at most 4px movement and returns focus to Watch.
- **Copy**: the button changes to "Copied" with a check, at a stable width, only
  after clipboard success, and resets after 1.5s; a polite region announces it.
  Failure stays "Copy" and shows a visible message.
- **Theme** switching changes the palette in one frame (transitions suspended).
- **Loading/empty**: the first-load skeleton appears after 150ms and is static;
  refreshes keep content. Empty task lists distinguish no tasks from no
  matches and offer Clear filters.
- `tests/quiet.spec.ts` covers focus return, inert exit, rapid
  open/close/reselect, restored geometry, nav interruption, direct resize,
  copy success/failure, reduced motion and phone Back.

### Compact defaults

The dashboard is a dense data surface. New features inherit compactness from
the primitives and tokens instead of choosing their own spacing:

- **Buttons** default to `sm` (11px text, 2px 8px, at least 24px tall). Use
  `size="md"` only for a prominent standalone action; never in cards, rows,
  toolbars, or the detail pane. Row-level actions (Watch, View log, Copy) sit
  inline in an existing row, not in a row of their own.
- **Filters** use `Filter` (caption + select in one label). It keeps its
  natural width and wraps to the next line; it never shrinks to a chevron. Put
  filter rows in a `flex-wrap: wrap` container with `--space-1` gaps.
- **Spacing**: panes use `--pane-padding`, sections `--section-*` (no
  padding: they are not boxes), log records `--inset-*`, and toolbars
  `--toolbar-padding`. Do not write raw `--space-2`/`--space-3` padding for
  these roles.
- **Coverage**: `tests/density.spec.ts` asserts the rendered geometry at
  1440/1024/768/390 in both themes: pane padding, borderless sections and spec, 24-28px action
  heights, Watch inside the run card's activity row, viewer toolbar and record
  spacing, filter caption/value fit, no control overlap, and no horizontal
  page overflow. Extend it when adding a dense surface; style lint alone only
  proves tokens are used.

Brand tokens also define `--syntax-*` highlight colors; every color token
switches under `:root[data-theme="light"]`, and `app-tokens.css` sets
`color-scheme` to match.

`npm run lint` enforces the system: stylelint rejects hex, named colors, and
color functions, and the `lint/no-off-token-px.mjs` plugin rejects any px other
than `0`/`1px` (signed, exponent, any case, escaped) in declarations outside
`brand-tokens.css`; ESLint rejects color/px literals and inline `style` in TSX.
The plugin also checks at-rule params, so `@media`/`@container` conditions
cannot carry px either. Breakpoints are CSS Modules `@value` tokens in
`app/styles/breakpoints.module.css` (`bp-phone` 760px, `bp-list-narrow` 640px), substituted at build time because queries cannot
read custom properties; that file is the only place query lengths may appear.

## Run log viewer

**Watch** on an active Runs card opens the same viewer. **View log**
opens an ended attempt. The URL's `run` is the recorded run ID, independent of
`task`; task/tab and both pane positions survive closing or browser Back.
The header retains run/task ID, agent, start time and final exit status.
Agent messages use sanitized Markdown; commands, outputs and unknown records
use escaped text. Long output is collapsed behind an expand control.

The viewer polls `GET /api/runs/<id>/log` once per second with at most one
request outstanding. This avoids another long-lived SSE thread in taskq's
thread-per-connection server. Existing task-change SSE and terminal commands
remain unchanged. Initial reads return recent complete records; `before=<byte>`
loads older history and `after=<byte>` follows. Both carry the returned
`generation` (device/inode). The cursor advances only after a successful
response, so connection failures retry the same range. Short partial JSONL
records wait for a newline; final unterminated output is displayed on completion.
A finished run drains any remaining pages and then stops polling.

Each read is at most 256 KiB plus one boundary byte; responses contain at most
1,000 records. Records longer than a page are explicitly displayed as fragments
without JSON formatting (UTF-8 split at a fragment edge may show replacement
characters). Older pages remain accessible. The browser retains at most 4,000
records / 2 MiB of raw UTF-16 text, with bounded rendering overhead. At the limit
it visibly pauses **before** discarding anything. Loading older history can
move to an older bounded window; **Jump to latest** explicitly replaces the
window with a recent tail and resumes. Scrolling up stops auto-scroll, while
Pause also stops requests. Closing/switching aborts requests and clears timers.
No browser disk cache or localStorage log archive is created.

The API resolves only numeric recorded run IDs. It rejects unknown IDs, path
parameters, invalid offsets, and combined before/after queries. Missing files,
truncation and replaced files are explicit unavailable states; reopen after
investigating a changed file. Old records whose paths were reused by a newer
attempt are unavailable, not mislabeled as the newer log. The dispatcher now
creates logs exclusively, selecting a unique suffix on collisions, including
a reset run budget. No schema migration is needed.

### Access and sensitive output

This preserves taskq's existing **network-only read boundary**: GET is readable
without the write token and browser reads use the same CORS allowlist. CORS is
not authentication. Anyone able to reach taskq can now read complete recorded
agent logs, including prompts, command output, internal paths and any secrets
an agent printed. There is no automatic redaction. Do not expose taskq or its
logs to an untrusted network. Public screenshot evidence must use synthetic
fixtures only. A wider-access deployment needs a separate authentication and
redaction decision before rollout.

### Local reproduction

From the repository root, run the full checks:

```bash
(cd taskq && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --locked)
cd infra/dashboard && npm ci && npm run lint && npm run typecheck && npm test && npm run build
```

From the repository root, run browser fixtures:

```bash
cd infra/dashboard && npx playwright install chromium && npm run test:browser
```

`tests/density.spec.ts` pins the compact layout contract (see Compact defaults).
`tests/run-log.spec.ts` appends deterministic Claude/Codex messages, tool calls
and results, simulates disconnection, tests pause and scrollback, loads two
historical attempts, checks missing logs and return position on mobile, and
exercises bounded buffering. Rust `run_log` and `server` tests cover real file
appends, partial records, oversized records, cursor retries, pagination, run
isolation and API validation. `dispatch` checks preserved logs after budget resets.

For a browser against the actual taskq API (three terminals, all from repo root):

```bash
cargo build --manifest-path taskq/Cargo.toml
PORT=18080 STATIC_DIR=infra/dashboard/out node infra/dashboard/server.mjs
python3 infra/dashboard/tests/run_log_fixture.py
```

Open the URL printed by the fixture. It starts a temporary server with
dispatch off, creates synthetic Claude/Codex streams and two finished
attempts, and removes its temporary database/logs on Ctrl-C. Stop the static
server with Ctrl-C too. Screenshots use `docs/reference/brand-guide.md` (Quiet):
neutral canvas, system body text, monospace command output, neutral disclosure
controls and local danger color for errors, in both themes.

### API-first rollout (ops authorization required)

1. Obtain a separate explicit ops decision for the taskq-server/dispatcher
   restart. It is excluded from the automatic `vm-service` target. Arrange a
   quiet dispatcher window first: stopping the server can interrupt active
   agents.
2. In a checkout of the approved merged revision, build with
   `cargo build --release --locked --manifest-path taskq/Cargo.toml`. Preserve
   the currently installed binary for rollback. Under that ops authorization,
   run `install -m 0755 taskq/target/release/taskq ~/.cargo/bin/taskq` and
   `systemctl --user restart taskq-server`, then
   `systemctl --user is-active taskq-server` and
   `journalctl --user -u taskq-server -n 50 --no-pager`.
3. Before every dashboard release, run
   `python3 infra/dashboard/watch_preflight.py --base http://taskq.hlab`
   from the repository root. This selects an existing recorded run and checks
   the real log route without printing logs. A missing route fails the feature
   preflight: stop release and block for ops. A supported route with a missing
   log passes capability checking but still requires the post-rollout Watch
   check. Browser fixtures are not deployed compatibility evidence. Upgrade
   the API first under separate authorization.
4. Queue a separate dashboard `hlab-service` delivery (`mode: tagged`,
   `service: dashboard`) and follow the runbook's tagged mode: release tag,
   workflow, ledger, readiness, digest and version checks. Check Watch on
   both surfaces and a finished attempt after rollout.
5. For rollback, request a tagged rollback using that runbook; if the server
   also needs reverting, ops restores the saved binary and explicitly restarts
   `taskq-server` in the agreed quiet window. Existing run records and log files
   remain compatible. Do not run the general provisioning script for this change.

### Failure and recovery contract

Unsupported routes and other permanent HTTP failures stop automatic requests
and explain the corrective action. Unknown run IDs are distinct from missing
log files and connected streams with no output. Network errors, HTTP 408/429,
and 5xx responses retry three times (1, 2, 4 seconds), then stop. Retry is
explicit and resumes the preserved cursor; unavailable log files also require
Retry. Existing output remains visible on a disconnect. Completed empty logs
say no output was recorded; completed nonempty logs remain history.

Browser tests default to port 18080. Set `PLAYWRIGHT_PORT` to an unused port
when another isolated test server is running; the CI command is unchanged.

### Conversation presentation

Run logs replay the loaded byte window into a conversation (`lib/transcript.ts`).
User and assistant messages share the sanitized GFM renderer used by task details.
Claude tool-use/result IDs and Codex item IDs join activity into a single disclosure
at the invocation's position. A result loaded without its call says so until older
history arrives. Claude finalized blocks can share a message ID: only an observed
streamed block is replaced, never unrelated blocks with that message ID. Unknown
records and partial records remain inspectable; raw mode retains every loaded event
in byte order. Empty thinking/signatures and routine metadata appear only there.
Available thinking is collapsed and is never synthesized from opaque payloads.

The viewer consumes the shared compact contract: `--toolbar-padding`,
`--pane-padding`, `--inset-padding`, `--inset-gap`, and `--control-min-height`.
These shared tokens now come from the merged compact defaults; controls stay
at least 24px high, message body stays 13px, activity rows use 4px vertical padding,
and code/output scroll within a 360px-high region. Native summary controls support
keyboard disclosure. `tests/conversation.spec.ts` measures geometry at
1440/1024/768/390 in both themes; `tests/run-log.spec.ts` covers cursors,
follow-tail, pause/resume, reconnect, history prepend, and streamed finalization.
Use synthetic fixtures for public evidence; never publish actual session contents.

## Routines

`view=routines` is a separate read-only view (`g r`), laid out like Tasks: a
routine table (schedule/timezone, status, next trigger, latest task; filtered
by project/search). Clicking a row opens the routine in a resizable side pane
(`routine=<id>`): facts, generated-task history and the template. A history
row opens its task in a second pane stacked on the right (`task=<id>`); Back
to routine (or Esc) closes it, then Close (or Esc) closes the routine. Below
1600px wide the list hides while both panes are open. Generated task detail
links back to the routine and keeps the task stacked.
Paused/archived routines retain history and do not show a future trigger.
Management stays in `taskq routine`; the browser has no write credentials.
Routine reads poll every five seconds with one request in flight and retain
stale data on errors. History pages contain at most 100 occurrences.

The API must ship first. For the authorized schema v10 rollout, follow
`docs/operations/runbooks/taskq-routines-upgrade.md`, then the normal dashboard
tagged-CD runbook. Task 140 explicitly combines both deliveries.
