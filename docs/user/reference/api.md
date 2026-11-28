---
title: "HTTP API"
description: "Authentication, sessions and where the route table lives."
---

With a token, every request needs `Authorization: Bearer <token>` or, for
reads, a browser read session; `/healthz`, `/metrics` and signed `/b/` links
are open, and a page without either gets the login page (`allowed` and
`login_route` in `crates/axe/src/server.rs`). The session is an HttpOnly
cookie that makes reads for 30 days, except Telegram message content, asks
and forms, which stay token-only (`private_read`); it also reads the store
(blobs, docs) and service logs (`store_read`). Its only writes are the web
board's presses (`POST .../board/items/<id>/done|reopen` and
`.../board/park`), from the server's own pages (`Origin` must match), recorded
as the owner. A browser gets one from `axe web login-link` (a one-time code,
`crates/axe/src/login.rs`; `--next board` lands on the bot's board), the token
on `/login`, or `POST /api/session` with the token; `POST /api/session-end`
clears it. Signing in on a page's login screen returns to that page. The server refuses a non-loopback listen address without a token
file. Expose it only on a trusted network: reads include full agent logs. Bodies are JSON, errors are
`{"error": "..."}`, and `X-Axe-Request-Id` makes a write retry-safe.

The route table is `route`, `service_route`, `bot_route`, `goal_route`, `doc_route` and `blob_api` in
`crates/axe/src/server.rs`; node operations are in `remote.rs`. Response
shapes are the serialized Rust types, exported to `web/lib/api.ts`. There is
no version prefix.
