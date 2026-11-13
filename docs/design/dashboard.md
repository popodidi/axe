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

The browser only reads: it holds no credentials and has no mutation UI.
Anyone who can reach the server can read full agent logs, so public
screenshots use synthetic fixtures only.
