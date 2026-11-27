---
title: "Docs"
description: "Versioned text documents on the server: skills, docs, memory, conduct and bundles."
---

Paths in `crates/axe/src/` unless noted.

`axe doc put/get/ls/log/diff/attach/detach/rm` (`doc.rs`) keeps versioned text
documents on the server: skills, docs, memory, conduct and bundles. A doc is a
path (`notes/plan.md`); a folder of docs is a path too, so a skill is a folder
with `SKILL.md` and its files, and a bundle is any folder. Every change is a
commit in a bare git repository (`docs.git` next to the database, or
`[docs] dir`), so `log` and `diff` show a doc's history; the server needs
`git` on its `PATH`. Docs attach to one of four scopes: `global`,
`label:<name>`, `owner:<name>` or `repo:<name>`; `axe doc ls --scope` lists
what a scope has. A write may name the version it edits (`--base`, or
`--new` for a new doc) and is refused with a conflict if the doc moved since.
`axe doc rm <path>` removes a doc, or a folder with everything under it, as
one more commit (a tombstone): `ls` and the dashboard Store stop showing it,
while `log` and `get --version` keep its history. It refuses a path a scope
still attaches unless `--detach`, which detaches it from every scope in the
same commit, and always refuses a path inside an attached folder.
Back the repository up with git (`git clone --mirror`); `axe backup` covers
the database only.

**Run context.** When the node builds a run workspace it collects the
docs attached to the task's scopes: each repo, the owner, each label (the
project first) and `global`. A conflict (the same skill name, or the same path
on two scopes) goes to the more specific scope: repo > owner > label > global.
Paths under `policy/` and a repo's own `.agents/skills` are never overridden.
Each attached skill is written whole into `skills/<name>/` (`SKILL.md` and
its scripts, references and tests; found through `.claude/skills` and
`.agents/skills`); each doc is a line in `docs/index.md` whose body loads on
use with `axe doc get <path>` (`context.rs`, `write_skills`). A bot's system
prompt lists what is attached to `owner:bot:<name>`, its project and
`global`; a bot with repos also gets those skills in its run folder. Nothing
attached means nothing changes.

**The axe skill.** [`skills/axe`](../../../skills/axe/SKILL.md) loads into the store
like any skill, so runs get it from there instead of a machine install:
`axe doc put skills/axe skills/axe`, then `axe doc attach skills/axe global`.

**Bundles.** Every bot's system prompt starts with the bot protocol, which
is core (`crates/axe/src/bot_protocol.md`: runs and wakes, memory,
subscriptions, goals, authority, the Telegram commands and cards). A bundle's
`prompt.md` is text a bot carries after it: the bot inlines it, ordered by
its `order:` front matter, before its identity and conduct. Bundles hold
opinions, such as [`bundles/responsive`](../../../bundles/README.md); load and attach
one with `axe doc put` and `axe doc attach`. A bot with no bundles still works
fully. `axe context explain <task>` prints each candidate, its scope and why
it was included or skipped (`context.rs`).
