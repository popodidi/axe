# axe web dashboard

Read-only Next.js / TypeScript dashboard for an axe server: Tasks, Routines,
Bots, Goals, Stats and Cost views over the server's GET API. All data stays
in the server; the browser holds no credentials and has no mutation UI.
Design intent: [docs/design/dashboard.md](../docs/design/dashboard.md).

## Development and checks

Node 24. The CI commands (`.github/workflows/ci.yml`, `web` job):

```sh
npm ci && npm run lint && npm run typecheck && npm test && npm run build
npx playwright install chromium && npm run test:browser -- --workers=1
```

`npm run dev` for development. Browser tests run the production build
through `npm start` (`server.mjs`) against read-only API fixtures in
`tests/`; set `PLAYWRIGHT_PORT` when the default port is taken. Public
screenshots use synthetic fixtures only.

## Embedded in the server

`npm run build` writes a static export to `web/out`. Building the crate with
the `embed-ui` feature embeds it (`crates/axe/src/web.rs`) and `axe server`
serves it at `/`:

```sh
(cd web && npm ci && npm run build)
cargo build --release --features embed-ui
```

The embedded page reads the server that serves it. A standalone build points
at a server with `?server=`, and that server must list the page's origin in
`[server] cors_origins`.

## API types

`lib/api.ts` is generated from the Rust API types (cargo feature `ts`);
`lib/api-contract.ts` checks at typecheck time that each generated type still
has the shape the dashboard reads. CI fails when `lib/api.ts` is stale;
regenerate it from the repository root with
`AXE_BLESS=1 cargo test -p axe-os --features ts --test ts_bindings`.

## Where things are

| What | Where |
|---|---|
| Page shell, URL state (`view`, `task`, `tab`, filters, pane scroll), keyboard shortcuts, live refresh | `app/dashboard.tsx`, `lib/use-pane-scroll.ts`, `lib/layout.ts` |
| Task list and detail model, copyable commands | `lib/model.ts` (`lib/model.test.ts` pins the command strings) |
| Views | `components/dashboard/` (`routines`, `bots`, `goals`, `stats`, `cost`) |
| Run log viewer and conversation replay | `components/dashboard/run-log.tsx`, `lib/run-log.ts`, `lib/transcript.ts` |
| Stats, bot stats, cost and goal calculations | `lib/stats.ts`, `lib/bot-stats.ts`, `lib/cost.ts`, `lib/goals.ts` |
| Theme and tokens | `app/styles/`, `lib/theme-script.ts` |

The URL is the state: every view, selection and filter is a query parameter
on the single `/` route, so a link reproduces what you see. Live data comes
from the server's change stream, with polling while disconnected; refreshes
keep content, selection and scroll.
