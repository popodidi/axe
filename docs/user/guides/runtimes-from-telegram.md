---
title: "Runtimes from Telegram"
description: "Manage runtimes and their logins from the Telegram owner command /runtimes."
---

Paths in `crates/axe/src/` unless noted.

**Managing runtimes from Telegram.** On a node with `[runtimes] dir`,
`/runtimes` lists every logged-in runtime on one line in plain words
(Logged in, Login expired) with its uses: `primary of <bots>`, `backup of
<bots>`, `tasks` (selection rules pick its harness) (`runtime::with_uses`,
`telegram::list_text`; `axe runtimes` shows the same uses). With none it
says "Log in Claude or Codex to start me." Per runtime: *Log in* when its
login expired, *Primary* / *Backup* (or *No backup*) for the chat's bot
(`bot edit --agent` / `--backup`), and *Remove*; then *Add Claude* / *Add
Codex* (`Registry::create`, `Registry::remove`):

- **Add** (login first) writes `<dir>/<name>/runtime.toml` under the next free name
  (`claude-2`) with its own `login_dir` (`<dir>/<name>/home`, linking the
  owner's instructions, settings and skills from the CLI's default directory,
  never its credentials) and opens its login menu. It is not listed and
  takes no runs until its login check passes (`Runtime::pending`; a second
  Add of that harness reuses it); then it shares the harness's work with the
  others. A chat bot with no runtime gets it as its primary. An Add that does not end in a login
  leaves nothing behind: the runtime is removed when its first login fails
  its check, or when none passed within `runtime::ADD_LOGIN_WAIT` (the
  longest sealed form plus 10 minutes; `added_at` in its `state.json`), and
  the owner gets a card saying so.
- **Remove** asks to confirm, is refused while the runtime has live runs and
  deletes `<dir>/<name>` (its stored credentials, and its login directory when
  that is inside); a login directory elsewhere, such as the CLI's default, is
  left in place. A session that ran on it starts fresh elsewhere.

The node applies both between picks, and they persist: the registry directory
is the state, so hand-written runtimes keep working beside them.

**Logins from Telegram.** *Log in* / *Re-login* offers the runtime's login methods
(`runtime::login_methods`):

- **Claude Code.** *Web sign-in*: the node runs `claude auth login` with its
  stdin kept open, relays the claude.ai link, and the code the page shows
  comes back in a sealed form and goes to that stdin (`Runtime::device_login`,
  `sealed::Desk::relay_code`); a passing login removes a stored token or API
  key, which would hide it. Or a `claude setup-token` token or an API key arrives in a
  sealed form (see [Sealed forms and messages](../concepts/sealed-forms.md)). It is kept as `<name>/login/CLAUDE_CODE_OAUTH_TOKEN`
  or `ANTHROPIC_API_KEY` (0600) and set in the runtime's environment, where it
  takes precedence over the login directory.
- **Codex.** An API key arrives in a sealed form and goes to `codex login
  --with-api-key` on stdin. Or `codex login --device-auth` runs on the
  node, and its link and code are relayed to the owner.

A login made this way is checked at once (`runtime::Registry::check_now`): the
runtime is ok again when the check passes, and the owner gets a runtime card
with the outcome. A runtime that needs a login is also rechecked every few
minutes (`runtime::PROBE_EVERY`). Once a runtime is ok again its open cards
close: their buttons go and the text gains a resolved line
(`telegram::close_runtime_card`). Removing a runtime closes its open cards
the same way, with a closed line, so its login card leaves the board.
