---
title: "Configuration"
description: "One TOML file read at startup by the server and the node."
---

One TOML file, `$AXE_CONFIG` (default `~/.config/axe/config.toml`; built-in
defaults when missing), read once at startup by both `axe server` and
`axe node`. The server uses `[server]`, `[telegram]`, `[metrics]`,
`[blobs]`, `[docs]` and the agent names; every local command reads `[cli]`;
the node uses the rest, so keep agent
names aligned when they run on different machines. Unknown keys fail validation.
`crates/axe/src/config.rs` defines every table, default and check;
`axe config` validates and prints the resolved file, including the built-in
`claude` agent.

| What | Default | Override |
|---|---|---|
| Database (server) | `~/.local/share/axe/axe.db` | `$AXE_DB` |
| Bearer token | `~/.config/axe/token` | `$AXE_TOKEN_FILE`; clients also take `$AXE_TOKEN` |
| Server address (clients) | `http://127.0.0.1:8790` | `$AXE_SERVER` |
| Listen address | `127.0.0.1:8790` | `axe server --listen` |
| Admission hold (node host) | `~/.local/state/axe/admission-hold.json` | `$AXE_ADMISSION_HOLD` |
| Agent logs and node journal | `~/.local/state/axe/logs` | `log_dir` |
| Speech engine and model (node) | `~/.cache/axe/stt`, fetched on first use | `[defaults] transcribe_voice = false` turns voice transcription off |

The database uses WAL: back it up with `axe backup PATH`, a consistent
snapshot from the running server, never by copying the file. Schema upgrades
run on server start (`db.rs`); back up before upgrading.
