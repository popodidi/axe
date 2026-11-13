# Bundles

A bundle is a folder of docs in the axe doc store, attached like any doc
(`axe doc attach <path> <scope>`; design: `crates/axe/src/context.rs`). A
bundle's `prompt.md` is text a bot carries in its system prompt: a bot inlines
the `prompt.md` of every bundle attached to its scopes (`owner:bot:<name>`, its
project's label, `global`) before its identity and conduct, ordered by the
`order:` front matter. A bot with no bundles gets its identity and conduct
only.

| Bundle | What it holds |
|---|---|
| `axe-bot` | The bot protocol: runs and wakes, memory, responsiveness, task-event triage, goals, authority |
| `telegram` | The Telegram channel protocol: replies, files, buttons, cards and the pinned inbox |

Load and attach them, for example to every bot:

```bash
axe doc put bundles/axe-bot bundles/axe-bot
axe doc put bundles/telegram bundles/telegram
axe doc attach bundles/axe-bot global
axe doc attach bundles/telegram global
```

Attach to `owner:bot:<name>` instead to give one bot a bundle. Changes reach
a bot's next run.
