# axe

A self-hosted task queue for coding agents. `axe server` owns a SQLite
database and serves an HTTP JSON API; `axe node` is a separate process
(optionally on another machine) that starts one headless coding-agent session
(Claude Code, Codex, or any command you configure) per ready task. Every
command other than `server`, including `node`, is an HTTP client of the
server.

## Install

```sh
cargo install --locked --git https://github.com/popodidi/axe axe-os   # binary `axe`
```

The crate is `axe-os`; the binary and the library are `axe`. Cargo features
(`crates/axe/Cargo.toml`): `telegram` (default on) adds the Telegram gateway
for bots; `embed-ui` serves the dashboard from `web/` at `/` (build `web/`
first, see [web/README.md](web/README.md)); `ts` generates the dashboard's
TypeScript API types.

Releases (`.github/workflows/release.yml`) publish a server image, Linux
x86_64 and macOS arm64 binary tarballs and the `skills/axe` tarball from a
`vX.Y.Z` tag, with sha256 sums; verify with `gh release download <tag> -R popodidi/axe && sha256sum -c
SHA256SUMS`, or without a GitHub login `curl -LO
https://github.com/popodidi/axe/releases/download/<tag>/<asset>`. Other
platforms install from source with `--tag <tag>`.

This README describes `main`, and releases can lag it: a release may lack
commands or flags named here (`axe <command> --help` is authoritative for the
binary you run). `axe --version` names the commit after the version (`axe
0.4.0 (56305982f192)`) when the build knew it (`crates/axe/build.rs`), so a
`main` build is distinct from the release it follows.

Upgrades and keeping the agent skill current: [Install](docs/user/getting-started/install.md#keeping-axe-and-its-skill-current).

## Docs

The user documentation is [docs/user/](docs/user/index.md) in this
repository (start with the [Quickstart](docs/user/getting-started/quickstart.md)),
published at https://axe-os.io/docs once that site is live. Design notes are in
[docs/design/](docs/design/); reporting and the threat model are in
[SECURITY.md](SECURITY.md).

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in axe by
you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
