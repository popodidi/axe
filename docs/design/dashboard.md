# Dashboard design contract

The read-only dashboard (`web/`) is a dense data surface for watching an axe
server. This page records the design intent; the tokens, components and
browser tests are the reference for exact values and geometry.

| What | Where |
|---|---|
| Colour, spacing, type and motion tokens (both themes) | `web/app/styles/brand-tokens.css`, `app-tokens.css` |
| Accents for state and data | `web/app/styles/accent-tokens.css` |
| Breakpoints | `web/app/styles/breakpoints.module.css` |
| Enforcement | `npm run lint`: stylelint and `web/lint/` reject literal colours and off-token lengths outside the token files |
| Rendered contract | `web/tests/quiet.spec.ts` (focus, motion, interruption), `density.spec.ts` (compact geometry), `status-colors.spec.ts` |

## Visual language

The base is **Quiet** (chosen 2026-11-03): group with space, alignment and
weight before any line or fill; compact rows; detail on the canvas rather than
in a card; the native system font, with monospace only for ids, commands,
paths, code and timestamps. Dark is the default theme, with a light
palette behind `data-theme="light"`.

Colour carries meaning, never decoration. A restrained accent set tints task
state labels, the live dot, charts and the selected option; chrome,
navigation and focus stay neutral. Every state also has a symbol and text, so
nothing depends on colour alone, and charts keep legends and exact counts.
Escalated is the only state that reads as danger; in progress is never a
warning.

## Compact defaults

New features inherit compactness from the primitives and tokens instead of
choosing their own spacing. Buttons default to the small size; a larger one
is for a prominent standalone action, never inside rows, cards, toolbars or
the detail pane. Row actions (Watch, View log, Copy) sit inline in an existing
row. Filters keep their caption visible and wrap rather than shrink. Panes,
sections, log records and toolbars use their role tokens, not raw spacing.
When adding a dense surface, extend `density.spec.ts`; lint alone only proves
tokens are used.

## List and detail

Tasks, Routines, Services, Bots, Goals and the Store share one grammar: a
search and filter bar, the list, and the selected row opening its item in a
right side pane (never inline below the list) with the same header (kind,
state badge, Close/Esc, title), a field grid and titled sections. The item's
URL parameter (`task`, `routine`, `service`, `bot`, `goal`, `blob`/`doc`) is
its deep link; `j`/`k` move between rows, Enter opens, Escape closes the
topmost pane, and a task opened from an item stacks to its right. The shared
pieces are `web/components/dashboard/side-pane.tsx`.
Every side pane, the run page's details rail included, resizes with the
shared `PaneResizer` (`web/components/dashboard/detail-resizer.tsx`) and keeps
its width per browser.

## Interaction

- Actions work immediately; motion never delays them, and the newest action
  wins over a running transition.
- Initial load, Back/Forward restoration, stored widths and the stored theme
  are not animated: restored geometry is right on first paint.
- Focus moves before content is hidden or unmounted, so a keyboard user never
  lands in exiting content. Focus and selection are visibly distinct.
- Dragging a resizer follows the pointer with no easing.
- With reduced motion, every workflow completes with no translation, scaling
  or width tweening.

## Loading, refresh and errors

Refreshes keep existing content, selection and scroll; stale or disconnected
data says so in text. Timestamps are honest: nothing is labelled fresh before
a successful response. Empty, no matches, disconnected, permission and error
states stay distinct, and an error offers a retry rather than an invented
success. Logs follow the tail only while the user is at it.

## Boundaries

The browser keeps no token and its only mutation UI is the bot Board tab's
presses (tick or reopen an item, park a decision), the same the Telegram pin
offers; the server takes them from a read session only from its own pages
(`board_press` and `same_origin` in `crates/axe/src/server.rs`) and records
them as the owner. Every page and read needs a read session, an HttpOnly
cookie the owner gets once per browser from a one-time `axe web login-link`
(or the token on `/login`); a browser without one gets the login page, and
signing in there returns to the page it asked for.
Panes follow server changes without restarting a read in flight
(`web/lib/live-load.ts`): changes come a second or more apart while runs are
live, and a restart on each never finished on a slow phone link.
Store previews never run content on the dashboard's origin: HTML renders in
a frame sandboxed without scripts or same-origin access, and the server
serves every blob with `Content-Security-Policy: sandbox`.
Anyone with a session can read full agent logs, so public screenshots use
synthetic fixtures only.
