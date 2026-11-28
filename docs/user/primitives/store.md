---
title: "Store"
description: "Versioned docs and skills attached to scopes, and content-addressed files shared through expiring links."
---

The store is what the server keeps besides work items. It has two parts:

- **Docs**: versioned text documents, such as skills, docs, memory, conduct
  and bundles. Every change is a commit, so you can see a doc's history and
  diff. Docs attach to a scope (`global`, `label:<name>`, `owner:<name>`,
  `repo:<name>`), and every run gets what is attached to its scopes: whole
  skills in `skills/`, docs as an index the agent loads on use.
- **Blobs**: immutable files stored by their sha256, shared through signed
  links that expire. Screenshots, reports and other evidence go here.

## When to use it

- [Level 0](../levels/level-0-skill.md): once you explain something twice,
  make it a skill or doc and attach it where it applies.
- Give a bot or a repo its own conventions (attach to `owner:bot:<name>` or
  `repo:<name>`).
- Attach evidence to a task without committing binaries anywhere.

## Main commands

| Command | What it does |
|---|---|
| `axe doc put PATH FILE_OR_DIR` | Write a doc, or a folder such as a skill |
| `axe doc attach PATH SCOPE` / `detach` | Make it part of every run in a scope |
| `axe doc get PATH` / `ls` / `log` / `diff` | Read it, list, see history |
| `axe context explain TASK` | What a run gets, and why each doc was included or skipped |
| `axe workspace TASK_OR_BOT` | Build that run folder on your machine; `axe workspace sync` writes edits back |
| `axe blob put FILE --link 7d` | Store a file and print a signed link |
| `axe blob link SHA256 --ttl 7d` | A new link for a stored file |

## Example

```sh
mkdir -p skills/release && printf -- '---\nname: release\ndescription: How we cut a release.\n---\n\n1. ...\n' > skills/release/SKILL.md
axe doc put skills/release skills/release
axe doc attach skills/release label:web
axe context explain 1
axe blob put screenshot.png --link 7d
```

More: [Docs](../concepts/docs-store.md), [Blobs](../concepts/blobs.md) and
[Working locally](../guides/working-locally.md).
