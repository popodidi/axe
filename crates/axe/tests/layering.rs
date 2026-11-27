//! Layering guards (store doc harvis/docs/decisions/axe-layering-review.md,
//! section 4): channel code reaches the database only through core actions,
//! and the CLI reaches the server only through the HTTP API.
//!
//! Both start as allowlists of today's violations. An allowlist only ever
//! shrinks: a new violation fails, and so does a removed one until its entry
//! is dropped here, so the list always matches the code.

use std::collections::BTreeMap;
use std::path::Path;

fn src(file: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The lines of `file` outside its `#[cfg(test)]` module and comments.
fn code_lines(file: &str) -> Vec<(usize, String)> {
    let text = src(file);
    text.lines()
        .enumerate()
        .take_while(|(_, l)| !l.starts_with("#[cfg(test)]"))
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .map(|(i, l)| (i + 1, l.to_string()))
        .collect()
}

/// Database access in channel code: SQL calls, and a raw connection.
const SQL: &[&str] = &[
    "rusqlite",
    ".execute(",
    ".execute_batch(",
    ".prepare(",
    ".query_row(",
    ".query_map(",
    ".transaction(",
];
const CONN: &[&str] = &[".conn()"];

/// Counts per file. Telegram reaches the database only through `core::Axe`
/// actions since step 6 (zero). `channel.rs`, `board.rs` and `owner_views.rs`
/// are the storage and queries those actions call (`bot_messages`, the
/// board, the views): their SQL stays there, counted so it moves only on
/// purpose. Server route arms keep their raw SQL until they call actions.
const CHANNEL_ALLOWLIST: &[(&str, &str, usize)] = &[
    ("telegram.rs", "sql", 0),
    ("telegram.rs", "conn", 0),
    ("owner_views.rs", "sql", 1),
    ("board.rs", "sql", 24),
    ("channel.rs", "sql", 31),
    ("server.rs", "sql", 15),
];

/// The core's own hooks: only `core.rs` actions call them.
const CORE_HOOKS: &[&str] = &[".db()", ".changed()", "Axe::db(", "Axe::changed("];

#[test]
fn telegram_calls_core_actions_not_the_database_hooks() {
    let (n, hits) = count("telegram.rs", CORE_HOOKS);
    assert_eq!(
        n,
        0,
        "telegram.rs calls the core's database hooks; add a core::Axe action instead:\n  {}",
        hits.join("\n  ")
    );
}

fn count(file: &str, pats: &[&str]) -> (usize, Vec<String>) {
    let hits: Vec<String> = code_lines(file)
        .into_iter()
        .filter(|(_, l)| pats.iter().any(|p| l.contains(p)))
        .map(|(n, l)| format!("{file}:{n}: {}", l.trim()))
        .collect();
    (hits.len(), hits)
}

#[test]
fn channel_code_does_not_touch_the_database() {
    let mut errors = Vec::new();
    for (file, class, allowed) in CHANNEL_ALLOWLIST {
        let pats = match *class {
            "sql" => SQL,
            "conn" => CONN,
            c => panic!("unknown class {c}"),
        };
        let (n, hits) = count(file, pats);
        if n > *allowed {
            errors.push(format!(
                "{file}: {n} {class} lines, allowed {allowed}: call a core action instead \
                 of the database\n  {}",
                hits.join("\n  ")
            ));
        } else if n < *allowed {
            errors.push(format!(
                "{file}: {n} {class} lines, the allowlist says {allowed}: lower it in \
                 tests/layering.rs"
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// Local state a CLI command must not touch: the config, the database, the
/// node's files. Only the entrypoints below run on the host they manage.
const LOCAL: &[&str] = &[
    "Config::load",
    "db::open",
    "rusqlite",
    "fs::write",
    "fs::remove_file",
    "fs::create_dir",
    "OpenOptions",
];

/// `(enclosing fn, trimmed line, why it is local)`: today's allowed uses.
/// Entrypoints that run where they act (server, node, init, config, skill,
/// upgrade, repo sync, workspace, form key) and client-side downloads stay;
/// nothing else.
const CLI_ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "run_doc",
        "Some(p) => std::fs::write(p, content)?,",
        "download to a local file",
    ),
    (
        "run_doc",
        "std::fs::create_dir_all(parent)?;",
        "download to a local folder",
    ),
    (
        "run_doc",
        "std::fs::write(&p, f[\"content\"].as_str().unwrap_or(\"\"))?;",
        "download to a local folder",
    ),
    (
        "run_blob",
        "let _ = std::fs::remove_file(&tmp);",
        "download to a local file",
    ),
    (
        "send_sealed",
        "let cfg = Config::load(&config::default_path()?).unwrap_or_default();",
        "form send encrypts on the node host",
    ),
    (
        "run_web",
        "let cfg = Config::load(&config::default_path()?).unwrap_or_default();",
        "the public URL of this host",
    ),
    (
        "run_form",
        "let cfg = Config::load(&config::default_path()?).unwrap_or_default();",
        "form key: the node's key",
    ),
    (
        "run_tg",
        "std::fs::create_dir_all(&dir).with_context(|| format!(\"create {}\", dir.display()))?;",
        "download to a local folder",
    ),
    (
        "run_tg",
        "std::fs::write(&file, &bytes).with_context(|| format!(\"write {}\", file.display()))?;",
        "download to a local file",
    ),
    (
        "run_cmd",
        ".and_then(|p| Config::load(&p).ok());",
        "context explain reads the local run workspace",
    ),
    (
        "run_cmd",
        "let cfg = Config::load(&config::default_path()?)?;",
        "workspace: the local run workspace",
    ),
    (
        "run_cmd",
        "let cfg = Config::load(&config::default_path()?)?;",
        "housekeeping: runs on its host",
    ),
    (
        "run_cmd",
        "let cfg = Config::load(&config::default_path()?)?;",
        "node",
    ),
    (
        "run_cmd",
        "let cfg = Config::load(&config_path)?;",
        "server",
    ),
    ("run_cmd", "let cfg = Config::load(&cfg_path)?;", "config"),
    ("run_cmd", "std::fs::create_dir_all(dir)", "init"),
    ("run_cmd", "std::fs::OpenOptions::new()", "init"),
    (
        "run_usage",
        "None => Config::load(&config::default_path()?)?.log_dir(),",
        "usage backfill reads local logs",
    ),
];

/// The function each line of `file` is in (the nearest `fn` above it).
fn with_fn(file: &str) -> Vec<(String, usize, String)> {
    let mut current = String::new();
    code_lines(file)
        .into_iter()
        .map(|(n, l)| {
            let t = l.trim_start();
            let t = t
                .strip_prefix("pub(crate) ")
                .or(t.strip_prefix("pub "))
                .unwrap_or(t);
            if let Some(rest) = t.strip_prefix("fn ") {
                current = rest.split(['(', '<']).next().unwrap_or("").to_string();
            }
            (current.clone(), n, l.trim().to_string())
        })
        .collect()
}

#[test]
fn cli_reaches_the_server_only_through_the_api() {
    let mut allowed: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for (f, line, _) in CLI_ALLOWLIST {
        *allowed.entry((f, line)).or_default() += 1;
    }
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut errors = Vec::new();
    for (f, n, line) in with_fn("cli.rs") {
        if !LOCAL.iter().any(|p| line.contains(p)) {
            continue;
        }
        let k = seen.entry((f.clone(), line.clone())).or_default();
        *k += 1;
        if *k
            > allowed
                .get(&(f.as_str(), line.as_str()))
                .copied()
                .unwrap_or(0)
        {
            errors.push(format!(
                "cli.rs:{n} (fn {f}): {line}\n  a CLI command acts through the HTTP API; \
                 local state only in a host entrypoint (tests/layering.rs CLI_ALLOWLIST)"
            ));
        }
    }
    for ((f, line), want) in &allowed {
        let got = seen
            .get(&(f.to_string(), line.to_string()))
            .copied()
            .unwrap_or(0);
        if got < *want {
            errors.push(format!(
                "stale CLI_ALLOWLIST entry (fn {f}): {line}: drop it from tests/layering.rs"
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
