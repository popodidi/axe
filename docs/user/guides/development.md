---
title: "Development"
description: "Run the checks CI runs."
---

```sh
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
cargo test --locked --features ts --test ts_bindings   # web/lib/api.ts is current
```

CI (`.github/workflows/ci.yml`) runs these on pull requests, plus the `web`
job when `scripts/ci_scope.py` says the dashboard is affected; the release
gate runs the full matrix. Dashboard checks: [web/README.md](../../../web/README.md).
