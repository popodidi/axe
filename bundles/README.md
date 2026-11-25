# Bundles

A bundle is a folder of docs in the axe doc store, attached like any doc
(`axe doc attach <path> <scope>`; design: `crates/axe/src/context.rs`). A
bundle's `prompt.md` is text a bot carries in its system prompt: a bot inlines
the `prompt.md` of every bundle attached to its scopes (`owner:bot:<name>`, its
project's label, `global`) after the bot protocol and before its identity and
conduct, ordered by the `order:` front matter.

The bot protocol itself (runs and wakes, memory, subscriptions, goals,
authority, the Telegram commands and cards) is core: every bot carries it
(`crates/axe/src/bot_protocol.md`). Bundles hold opinions on top of it, so a
bot with no bundles still works fully.

| Bundle | What it holds |
|---|---|
| `responsive` | Defaults for owner attention: acknowledge first, offload by size, batch event messages |

Load and attach one, for example to every bot:

```bash
axe doc put bundles/responsive bundles/responsive
axe doc attach bundles/responsive global
```

Attach to `owner:bot:<name>` instead to give one bot a bundle. Changes reach
a bot's next run.
