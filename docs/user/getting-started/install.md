---
title: "Install"
description: "Install axe, verify a release, and keep the binary and its agent skill current."
---

```sh
cargo install --locked --git https://github.com/popodidi/axe axe-os   # binary `axe`
```

The crate is `axe-os`; the binary and the library are `axe`. Cargo features
(`crates/axe/Cargo.toml`): `telegram` (default on) adds the Telegram gateway
for bots; `embed-ui` serves the dashboard from `web/` at `/` (build `web/`
first, see [web/README.md](../../../web/README.md)); `ts` generates the dashboard's
TypeScript API types.

Releases (`.github/workflows/release.yml`) publish a server image, Linux
x86_64 and macOS arm64 binary tarballs and the `skills/axe` tarball from a
`vX.Y.Z` tag, with sha256 sums; verify with `gh release download <tag> -R popodidi/axe && sha256sum -c
SHA256SUMS`, or without a GitHub login `curl -LO
https://github.com/popodidi/axe/releases/download/<tag>/<asset>`. Other
platforms install from source with `--tag <tag>`.

These docs describe `main`, and releases can lag it: a release may lack
commands or flags named here (`axe <command> --help` is authoritative for the
binary you run). `axe --version` names the commit after the version (`axe
0.4.0 (56305982f192)`) when the build knew it (`crates/axe/build.rs`), so a
`main` build is distinct from the release it follows.

## Keeping axe and its skill current

Every binary embeds the agent skill in `skills/axe` (`crates/axe/build.rs`).
`axe skill install` writes it to `~/.agents/skills/axe` (`--dir` elsewhere),
links `~/.claude/skills/axe` to it when `~/.claude` exists and that path is
free, and stamps the copy (`.axe-skill.json`: the binary's version and a hash
of the files). Re-run it to refresh the skill without upgrading the binary;
`axe skill status` compares the copy with the binary. Any other command
rewrites a stamped, unedited copy at the default path that differs from the
binary, with one line on stderr. A copy that is unstamped or edited since it
was stamped is never replaced automatically: it gets a hint at most once a
day, and `axe skill install --force` replaces it.

`axe upgrade` replaces the running binary with the latest release
(`--version vX.Y.Z` for another), checking the tarball against the release's
sha256, then runs the new binary's `axe skill install`. `axe upgrade --check`
only reports. A platform without a release binary gets the `cargo install
--tag` command (`--cargo` runs it). On a terminal, commands check the latest
release about once a day (cached under the user cache dir, `~/.cache/axe` on
Linux) and print a hint when it is newer; never in an agent session
(`$AXE_TASK` or `$AXE_BOT` set), and a network error only skips the hint.
`skill.rs` and `upgrade.rs` hold the rules. To switch either off:

```toml
[cli]
skill_refresh = false   # or AXE_SKILL_REFRESH=0
update_check = false    # or AXE_UPDATE_CHECK=0
```
