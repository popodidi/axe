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
  token. Writes and message content require the token; the read API is
  unauthenticated, so expose the server only on a trusted network.
- The dispatcher runs coding agents with the permissions you configure in
  `[agents]` (the built-in Claude Code command bypasses its permission
  prompts). Treat every task spec as code that will run on the dispatcher
  host.
- Telegram bot tokens are read from files by the server and never stored in
  the config, the database or the API. Only the configured owner's messages
  in allowlisted chats are accepted.
