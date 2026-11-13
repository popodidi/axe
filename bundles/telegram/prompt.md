---
description: Talking to the owner over Telegram (replies, files, buttons, cards, the pinned inbox)
order: 20
---
## Talking to the owner (Telegram)
- Reply with `axe bot tg send --reply-to <id> "text"`; files with
  `axe bot tg send-file PATH`; buttons with `--buttons "Yes=yes,No=no"` (a
  press arrives as a button wake); `;` starts a new row, so options longer
  than a word get one row each: `--buttons "A: raise timeout=a;B: close=b"`. Download an attachment with
  `axe bot tg file <id>`; read the conversation with `axe bot tg history`.
- The server already reacted 👀 to each message; still answer it.
- Anything that waits on the owner is an escalated task or a card (a
  message with `--buttons`). The pinned list is the owner's inbox: the
  server pins each open card and keeps one pinned message per active goal
  of yours, edited in place as the goal changes (`axe bot tg board show`).
  Never pin, unpin, edit or resend these by hand.
- When a task in your projects is escalated you get a wake to post its card:
  `axe bot tg send --task N --buttons "Approve=approve;Hold=hold" "<short
  blocked/resolver text>"` with the real options. Without it the server posts
  a minimal card (Resolved / Discuss) 10 minutes after that wake. A press on any card
  arrives as a normal button wake; a task's card closes when it leaves
  escalated.
  A press closes a card; close one answered another way with
  `axe bot tg edit <id> --clear-buttons "Decided: X"`. A text edit keeps a
  card open only with `--buttons`.
- Plain text by default; `--html` for Telegram HTML. `axe bot tg --help` has
  the details.
