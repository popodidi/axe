# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately
through GitHub's "Report a vulnerability" button on this repository's
Security tab (private vulnerability reporting). Include the affected version
or commit, the impact, and steps to reproduce.

You should get an acknowledgement within a week. Fixes are released as a new
version; the advisory is published once a fixed release is available.

## Supported versions

Only the latest release receives security fixes.

## Threat model in brief

- `axe server` refuses to listen on a non-loopback address without a bearer
  token. With a token, every request needs it or a browser read session
  (`allowed` in `crates/axe/src/server.rs`); only `/healthz`, `/metrics`
  and signed `/b/` links are open. A session (one-time `axe web login-link`
  codes or the token on `/login`) only reads and lasts 30 days; it is an
  HMAC cookie, revoked only by replacing the blob link key. Plain HTTP
  carries the cookie and token in the clear, so keep the server on a
  trusted network.
- The dispatcher runs coding agents with the permissions you configure in
  `[agents]` (the built-in Claude Code command bypasses its permission
  prompts). Treat every task spec as code that will run on the dispatcher
  host.
- Telegram bot tokens are read from files by the server and never stored in
  the config, the database or the API. Only the configured owner's messages
  in allowlisted chats are accepted.
- Sealed forms (`axe form`) carry owner input to the dispatcher encrypted to
  the dispatcher key (`dispatcher.key`, 0600). The channel and the server see
  only the form's metadata and ciphertext, which the server keeps in memory
  until delivery. The form page's code is the trust root; a server could swap
  the key on the first form a browser sees, so compare the fingerprint once
  (`axe form key`). Values are files readable by the dispatcher's user, like
  its logins: every task run as that user can read them.
- Sealed messages (`axe form send`, `axe web login-link --tg`) go the other
  way, encrypted on the dispatcher host to a key derived from the owner's
  passphrase (`axe form reply-key`; `ReplyKey` in `crates/axe/src/sealed.rs`).
  The server and the channel see the sender, purpose, expiry and ciphertext.
  The ciphertext stays in the chat and allows offline guessing of the
  passphrase, so choose a long one and send material that expires or can be
  rotated. Like forms, the page's code is the trust root.
