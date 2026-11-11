# axe docs

The top-level [README](../README.md) is the user guide: commands, dispatcher
behaviour, routines, bots, goals and the Telegram gateway. These pages are the
references and design records behind it.

| Page | What it holds |
|---|---|
| [Configuration](config.md) | Every config table and key, defaults, file locations and environment variables |
| [HTTP API v1](api-v1.md) | Endpoints, auth, request and response conventions |
| [One agent per task](design/one-agent-per-task.md) | Why one resumed session carries each task from spec to done |
| [Bots](design/bots.md) | The bot primitive: identity, wakes, one resumed session, Telegram as its interface |
| [Goals](design/goals.md) | The goal primitive: durable intent, budget and trajectory shared by bots |
| [Dashboard design](design/dashboard.md) | The Quiet design contract for the read-only dashboard |

The agent-facing guide is the [`axe` skill](../skills/axe/SKILL.md): states,
commands, the spec format and how sessions find the server.
