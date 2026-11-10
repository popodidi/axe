# Dashboard design contract (Quiet)

Apply the Quiet visual language to the read-only axe dashboard (`web/`). Quiet
was chosen on 2026-11-03 for its borderless composition, monochrome appearance,
compactness, and 275ms layout transitions. Keep the existing Tasks, Stats, and
Routines information architecture, URLs, keyboard shortcuts, independent pane
scrolling, resizing, and persisted preferences.

This is the implementation contract, not a claim that the current dashboard
already implements every interaction below. The dashboard's design-token file establishes the
foundation; the dashboard migration must update its role bindings, component
styles, and focus/mount lifecycle together. Do not present a palette-only change
as the completed Quiet interface.

## Composition

- Navigation uses `--surface-nav`; content uses `--bg`. No permanent border
  between navigation and content. The active link has a neutral fill and 600
  weight. Keep the existing collapsed-rail affordances and accessible names.
- Tasks has one page heading, a compact filter row, then the list. Keep labels
  visible when filters wrap. No enclosing card, header band, or list shadow.
- Keep current compact row density (roughly 32–36px for a single-line desktop
  table row); allow wrapping or responsive metadata removal rather than clipping
  essential titles. This is a target range, not a fixed-height text constraint.
- The selected row has `--surface-selected` plus a 600-weight title. Hover uses
  `--surface-hover`; hover must not override selection. Keyboard focus has a
  separate ring so focus and selection can be on different rows.
- Detail sits on the canvas. Remove the card around the spec. Distinguish title,
  metadata, tabs, prose, code, and activity using rhythm and weight.
- The resize boundary may retain a hairline: it explains a draggable interaction.
  Increase its contrast on hover/focus while retaining a generous hit area.
- Stats uses borderless chart/table groups. Color is for data distinctions with
  explicit legends, never a substitute for axes, labels, or missing-data states.
- Routines uses the same list/detail language. Paused and archived are clear
  labels, not merely faded rows; retain readable history and timezone text.
- At phone width, detail replaces the list and has a visible Back action. Preserve
  filter and scroll restoration. Do not force the desktop split onto a phone.

## Task states and color

Use the existing axe state names and derivation. Styling must not change state
semantics. Pair every symbol with readable text; decorative symbols are hidden
from assistive technology when the adjacent label already communicates them.

| State | Appearance | Symbol / meaning |
|---|---|---|
| Pending | Secondary neutral text; no colored pill | Hollow circle; queued |
| In progress | Primary neutral text, medium weight | Partial ring; running (static, no perpetual rotation) |
| Blocked / dependency wait | Secondary neutral text | Pause or outlined square; waiting |
| Escalated | Local danger text/icon; optional small danger fill | Attention diamond; owner action required |
| Done | Secondary neutral text | Check; complete |

Routine completion and healthy connectivity are neutral. An explicit success
confirmation may use `--status-success` locally; diffs use success/danger with
addition/deletion markers. Stale/disconnected states may use warning with text.
Do not map in-progress to warning or make every completed row green.

Map app tokens to roles: pending/done/blocked → secondary neutral; running →
primary neutral; escalated → danger. Selected/focused rows → dedicated neutral
surface/focus tokens. Preserve compatibility color aliases only for unmigrated
consumers and genuine warning/info/success/error uses.

## Interaction specifications

All durations below are maxima for UI motion, not artificial data delays. Use
shared timing tokens. Actions work immediately; no interaction waits for a
transition to finish. Persist semantic preferences, never intermediate widths.

| Interaction | Visual behavior | Timing | Focus and state contract |
|---|---|---|---|
| Hover row/control | Neutral local fill; no lift or shadow | 120ms standard | No content change or layout shift; keyboard focus independent |
| Press icon button | Optional .98 scale; restore on release/cancel | 120ms standard | Only small icon buttons; never scale row text, inputs, or pane contents |
| Select task | Selected fill and title weight update immediately | Fill up to 120ms | URL updates once; list selection remains visible; no scroll reset on refresh |
| Open desktop detail | Workspace boundary moves, detail content enters by at most 4px | Layout 275ms; content 160ms | Pointer opening keeps row focus; keyboard opening focuses detail heading after mount; non-modal |
| Switch selected task | Replace detail content; no close/reopen pane motion | Immediate; optional local 160ms transition | Reset detail scroll for the new task; preserve list scroll; latest request wins |
| Close detail | Boundary returns smoothly; content may fade locally | Layout 275ms; content 160ms | Move focus to the invoking row before hiding/unmounting focused content; if absent, focus list heading |
| Collapse/expand navigation | Animate rail width; label appearance follows available space | 275ms layout | Toggle remains focused; labels retain accessible names; persist final collapsed state |
| Resize by drag | Boundary follows pointer exactly | 0ms | No easing/lag while dragging; honor min/max; preserve scroll and pointer capture |
| Resize by keys | Boundary follows arrows/Home/End; double-click resets default | 0ms keys/drag; reset may use 275ms | Preserve separator focus and aria-valuenow/min/max; retain existing step sizes |
| Change detail tab | Indicator moves; active label changes weight | 160ms standard | Keep focus on activated tab; update aria-selected; existing scroll/URL rules remain |
| Expand disclosure | Reveal measured content; chevron rotates | 200ms standard | Native disclosure semantics; focus stays on trigger; no arbitrary max-height clipping |
| Copy command | Stable-width Copy changes to Copied with check | 160ms feedback; label resets after 1500ms | Confirm only after clipboard success; polite announcement; failure remains actionable |
| Theme switch | Palette updates without moving content | 0ms | Keep focus, scroll, selection and panel size; persist before next first paint |
| Open run-log dialog | Opaque surface appears with at most 4px movement | 200ms standard | Move focus inside; trap only modal focus; Escape closes; return to Watch trigger |
| Close run-log dialog | Local fade, no layout movement behind it | 160ms feedback | Restore focus even with reduced motion or interrupted animation |
| Navigate on phone | Detail replaces list; optional 4px directional entry | 200ms standard | Focus detail heading on open; Back restores invoking row and list scroll |

For keyboard detail focus, use a programmatically focusable heading (`tabindex=-1`),
not an extra tab stop. Coordinate focus before applying inert/hidden/unmount.
Exiting content becomes inert after focus has safely moved. Hidden content must
not remain reachable by Tab during an exit transition.

### Interruptions and persistence

A user can select another task, close a pane, resize, or toggle navigation while
motion is running. The newest action wins. Reverse from the current position;
never accumulate transforms or delayed callbacks. Cancel obsolete fetches or
ignore stale responses. Do not let a completed old animation unmount new content.
Use state/lifecycle cleanup that also runs with zero-duration motion.

Do not animate initial page hydration, Back/Forward restoration, stored nav
width, stored detail width, or initial theme. Restored geometry should appear
correctly on the first paint. User-initiated layout transitions begin only after
initial state has been applied. Dragging temporarily bypasses width transitions.

## Loading, refresh, and errors

- On the first request, keep the final layout's approximate dimensions. Delay a
  skeleton by 150ms so fast responses do not flash; show static neutral blocks
  only while there is no usable content. Do not add shimmer or a looping pulse.
- For refreshes, retain existing content and selection. Use a small text status
  for stale/disconnected data. Never replace the whole page with a skeleton.
- Use honest timestamps. Do not label data fresh before a successful response.
- New log content follows only while Follow is enabled and the user is at the
  tail. Scrolling away pauses follow and offers an explicit return action.
- Do not animate incoming log lines, reorder rows under the pointer for cosmetic
  reasons, or repeatedly announce streaming content to assistive technology.
- Search updates the result set without animating every row. Preserve focus in
  the input and announce the settled result count politely.
- Empty state: explain whether there are no tasks or no matches and offer a
  relevant recovery, such as clearing filters. No decorative illustration needed.
- Error: plain-language explanation and retry action; no invented success state.
  Permission, disconnected, stale, and genuinely empty must remain distinct.

## Small details that define the finish

- Keep icon stroke/size consistent: normally 16px, with 24px minimum effective
  targets and 44px on coarse pointers. Do not use emoji as interface icons.
- Align numbers with tabular figures so ticking durations do not change width.
- Keep button widths stable through Copy/Copied, loading, and retry states.
- Tooltips explain unfamiliar icon controls after a short delay (400ms pointer;
  immediate on keyboard focus), disappear on Escape, and never contain essential
  instructions available nowhere else.
- Focus rings are crisp, unclipped, and distinguishable from selection. Borderless
  design never means removing an accessible control boundary.
- Keep sticky tab/header backgrounds opaque and matched to the canvas; content
  should not show through while scrolling.
- Use subtle separators only where they explain a boundary or data grouping.
  No nested panel/spec/code borders and no shadow under the selected row.
- Preserve readable link affordances in specs and events. Inline prose links are
  underlined even though they are neutral.

## Reduced motion

Use the shared reduced-motion tokens and disable independent CSS keyframes and
JS animations. No translation, scaling, smooth scrolling, indicator travel, or
width tweening. All final geometry and confirmation text appear immediately.
The 1500ms Copied label timeout may remain: it is feedback persistence, not
motion. No behavior or cleanup depends exclusively on transitionend/animationend.

## Delivery boundaries and checks

The design tokens are the single source for colour, spacing and timing. The
migration to this contract must not silently leave gold selection, filled
panels, or uppercase labels and call itself complete. When it lands, remove
obsolete token content and update the dashboard README.

1. Migrate role bindings (actions, selection, focus, links, task states), then
   remove redundant panel/table/spec borders and fills. Preserve compact geometry.
2. Migrate native typography and remove the obsolete external Inter font load.
3. Implement mount/unmount/focus behavior before layout motion. Preserve URLs,
   shortcuts, scroll restoration, resizer constraints and read-only API behavior.
4. Add motion and feedback with interruption and reduced-motion support.
5. Verify synthetic Tasks, Stats, Routines and run-log views in both themes.

Run the unchanged dashboard CI commands from the repository root:

```sh
cd web && npm ci && npm run lint && npm run typecheck && npm test && npm run build
cd web && npx playwright install chromium && npm run test:browser
```

Run each command from the repository root independently. Add behavior tests for
pane focus return, hidden/inert content, rapid open/close/reselect, interrupted
nav transitions, direct resize, copied/failed clipboard feedback, restored
geometry without animation, and reduced motion. Update existing status/density
expectations to the new behavior; do not simply delete them.

Rendered acceptance: 1440, 1024, 768, and 390px widths, plus a 320px overflow
check and 200% zoom. In both themes, inspect normal/hover/focus/selected/error/
stale/loading/empty states. Check text contrast on actual fills, not just canvas.
Keyboard walkthrough covers list → detail → tabs → Watch → close → originating
control. With reduced motion enabled, every workflow must still complete.

Performance acceptance: inspect rail and pane transitions with realistic long
specs and log records; avoid visible frame drops and text scaling. If width
animation is costly, reduce animated content, not keyboard correctness. Public
screenshots use synthetic data only.
