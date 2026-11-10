//! Compatibility with taskq, the project axe was imported from.
//!
//! This is the only module that knows the old name. It is temporary: one
//! change deletes this file and its call sites once every deployment has
//! moved to the axe names. Until then:
//!
//! - `TASKQ_*` variables are read when the matching `AXE_*` is unset
//!   ([`import_env`]), and child agents get both names ([`child_env`]);
//! - `~/.config/taskq`, `~/.local/share/taskq/tasks.db` and
//!   `~/.local/state/taskq` are used when the axe path is missing and the old
//!   one exists ([`home_path`]);
//! - the server accepts both header names and sends both, and so does the
//!   client;
//! - the binary runs unchanged when invoked through a `taskq` symlink.

use std::path::{Path, PathBuf};

/// The old binary and product name.
pub const OLD_NAME: &str = "taskq";
/// Old prefix of every environment variable.
pub const OLD_ENV_PREFIX: &str = "TASKQ_";
/// Old name of the write request id header (lowercase, as the server stores it).
pub const OLD_REQUEST_ID_HEADER: &str = "x-taskq-request-id";
/// Old name of the file name header on file downloads.
pub const OLD_FILE_NAME_HEADER: &str = "X-Taskq-File-Name";

/// Every `AXE_*` variable axe reads or sets.
pub const ENV_NAMES: [&str; 13] = [
    "ACTOR",
    "ADMISSION_HOLD",
    "BOT",
    "BOT_RUN",
    "CONFIG",
    "DB",
    "RETRY_SECS",
    "SERVER",
    "SESSION",
    "TASK",
    "TOKEN",
    "TOKEN_FILE",
    "WORKTREE",
];

/// Old paths under `$HOME`, keyed by their axe path.
const PATHS: [(&str, &str); 4] = [
    (".config/axe", ".config/taskq"),
    (".local/share/axe/axe.db", ".local/share/taskq/tasks.db"),
    (".local/share/axe", ".local/share/taskq"),
    (".local/state/axe", ".local/state/taskq"),
];

fn nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// The `AXE_*` values to take from `TASKQ_*`: each old variable that is set
/// while its axe variable is not.
pub fn imported_env(get: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    ENV_NAMES
        .iter()
        .filter(|n| get(&format!("AXE_{n}")).is_none())
        .filter_map(|n| get(&format!("{OLD_ENV_PREFIX}{n}")).map(|v| (format!("AXE_{n}"), v)))
        .collect()
}

/// Copies `TASKQ_*` into unset `AXE_*` variables, with one deprecation
/// warning. Call first in `main`, before any thread starts.
pub fn import_env() {
    let imported = imported_env(nonempty);
    if imported.is_empty() {
        return;
    }
    let names: Vec<&str> = imported.iter().map(|(k, _)| k.as_str()).collect();
    eprintln!(
        "axe: deprecated {OLD_ENV_PREFIX}* variables in use; set {} instead",
        names.join(", ")
    );
    for (k, v) in imported {
        std::env::set_var(k, v);
    }
}

/// Adds the old name of every `AXE_*` entry, so child agents and scripts
/// that still read `TASKQ_*` keep working.
pub fn child_env<K: AsRef<str>>(env: &[(K, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.as_ref().to_string(), v.clone()))
        .collect();
    for (k, v) in env {
        if let Some(rest) = k.as_ref().strip_prefix("AXE_") {
            out.push((format!("{OLD_ENV_PREFIX}{rest}"), v.clone()));
        }
    }
    out
}

/// Both names of each `AXE_*` variable in `names` (for removal from a child's
/// inherited environment).
pub fn both_names(names: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    for n in names {
        if let Some(rest) = n.strip_prefix("AXE_") {
            out.push(format!("{OLD_ENV_PREFIX}{rest}"));
        }
    }
    out
}

/// The old path for `rel` under `home` when `rel` (or the axe directory
/// containing it) is missing and the old one exists; else `home/rel`.
pub fn resolve(home: &Path, rel: &str) -> PathBuf {
    let new = home.join(rel);
    for (axe, old) in PATHS {
        let Some(rest) = rel.strip_prefix(axe) else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            continue;
        }
        let old_path = home.join(format!("{old}{rest}"));
        if !home.join(axe).exists() && home.join(old).exists() {
            return old_path;
        }
        return new;
    }
    new
}

/// `~/<rel>` with the old-directory fallback of [`resolve`].
pub fn home_path(rel: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| resolve(&h, rel))
}
