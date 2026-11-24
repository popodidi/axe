//! The agent skill embedded in this binary (`skills/axe`, see `build.rs`):
//! `axe skill install` writes it to the local agent skill directory with a
//! stamp, and every other command refreshes a stamped copy that is older than
//! the binary ([`auto_refresh`]).
//!
//! The stamp (`.axe-skill.json` in the installed directory) records the
//! binary that wrote it and a hash of the files it wrote. Only a stamped copy
//! whose files still match that hash is ever rewritten without `--force`: an
//! unstamped or hand-edited copy is left alone with a hint.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/skill_files.rs"));

/// The stamp's file name inside an installed skill directory.
pub const STAMP: &str = ".axe-skill.json";

/// What `axe --version` prints, which the stamp records.
pub const BUILD: &str = env!("AXE_VERSION");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// The crate version of the binary that wrote the copy.
    pub version: String,
    /// Its full version string, with the commit (`axe --version`).
    pub build: String,
    /// [`tree_hash`] of the files written.
    pub sha256: String,
}

/// The state of a skill directory, as [`inspect`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Missing,
    /// A symlink: never followed or replaced.
    Symlink,
    /// No (valid) stamp. `current` when its files equal the embedded skill.
    Unstamped {
        current: bool,
    },
    /// Stamped, and its files changed since: never rewritten automatically.
    Modified(Stamp),
    /// Stamped and unmodified; `current` when stamp and files match this binary.
    Stamped {
        stamp: Stamp,
        current: bool,
    },
}

/// `~/.agents/skills/axe`, where `axe skill install` writes by default.
pub fn default_dir() -> Result<PathBuf> {
    Ok(home()?.join(".agents/skills/axe"))
}

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("cannot determine home directory")
}

/// The embedded skill's files, as `(relative path, bytes)`.
pub fn embedded() -> Vec<(String, Vec<u8>)> {
    SKILL_FILES
        .iter()
        .map(|(p, b)| (p.to_string(), b.to_vec()))
        .collect()
}

/// sha256 over the sorted `(path, bytes)` list, each entry length-prefixed.
pub fn hash_files(files: &[(String, Vec<u8>)]) -> String {
    let mut sorted: Vec<_> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (path, bytes) in sorted {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// [`hash_files`] of a directory's files, without the stamp. A symlink or
/// other non-file entry hashes as its own marker, so it counts as an edit.
pub fn tree_hash(dir: &Path) -> Result<String> {
    Ok(hash_files(&read_tree(dir)?))
}

fn read_tree(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            let rel: Vec<_> = path
                .strip_prefix(root)?
                .iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            let rel = rel.join("/");
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                walk(root, &path, out)?;
            } else if meta.is_file() {
                if rel != STAMP {
                    out.push((rel, std::fs::read(&path)?));
                }
            } else {
                out.push((rel, b"\0not a regular file\0".to_vec()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

fn read_stamp(dir: &Path) -> Option<Stamp> {
    serde_json::from_slice(&std::fs::read(dir.join(STAMP)).ok()?).ok()
}

/// What is at `dir`.
pub fn inspect(dir: &Path) -> Result<State> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::Missing),
        Err(e) => return Err(e).with_context(|| format!("stat {}", dir.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(State::Symlink);
    }
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let have = tree_hash(dir)?;
    let want = hash_files(&embedded());
    Ok(match read_stamp(dir) {
        None => State::Unstamped {
            current: have == want,
        },
        Some(stamp) if stamp.sha256 != have => State::Modified(stamp),
        Some(stamp) => {
            let current = stamp.sha256 == want && stamp.build == BUILD;
            State::Stamped { stamp, current }
        }
    })
}

/// Writes the embedded skill and its stamp to `dir`, replacing what is there
/// in one rename (built beside it first).
fn write(dir: &Path) -> Result<()> {
    let files = embedded();
    let parent = dir
        .parent()
        .with_context(|| format!("{} has no parent directory", dir.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.axe-new-{}", std::process::id()));
    let old = parent.join(format!(".{name}.axe-old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let built = (|| -> Result<()> {
        for (rel, bytes) in &files {
            let path = tmp.join(rel);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(&path, bytes)?;
        }
        let stamp = Stamp {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: BUILD.to_string(),
            sha256: hash_files(&files),
        };
        std::fs::write(
            tmp.join(STAMP),
            serde_json::to_string_pretty(&stamp)? + "\n",
        )?;
        Ok(())
    })();
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e.context(format!("write {}", tmp.display())));
    }
    let existed = dir.exists();
    if existed {
        std::fs::rename(dir, &old).with_context(|| format!("move aside {}", dir.display()))?;
    }
    if let Err(e) = std::fs::rename(&tmp, dir) {
        if existed {
            let _ = std::fs::rename(&old, dir);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e).with_context(|| format!("install {}", dir.display()));
    }
    if existed {
        let _ = std::fs::remove_dir_all(&old);
    }
    Ok(())
}

/// `axe skill install`: writes the embedded skill to `dir` (default
/// [`default_dir`]) and, unless `no_link`, links `~/.claude/skills/axe` to it.
/// Refuses an unstamped or hand-edited copy unless `force`. Returns the lines
/// to print.
pub fn install(dir: Option<&Path>, force: bool, no_link: bool) -> Result<Vec<String>> {
    if SKILL_FILES.is_empty() {
        bail!(
            "this axe build has no embedded skill (it was built outside the axe repository); \
             install a release binary or build from a clone"
        );
    }
    let default = default_dir()?;
    let dir = dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default.clone());
    let mut lines = Vec::new();
    let _lock = lock(true);
    match inspect(&dir)? {
        State::Symlink => bail!(
            "{} is a symlink; axe does not replace it (remove it, or pass --dir)",
            dir.display()
        ),
        State::Stamped { current: true, .. } => {
            lines.push(format!("{} is up to date ({BUILD})", dir.display()));
        }
        State::Modified(_) if !force => bail!(
            "{} was edited since axe installed it; `axe skill install --force` replaces it",
            dir.display()
        ),
        State::Unstamped { current: false } if !force && !is_empty_dir(&dir) => bail!(
            "{} was not installed by axe; `axe skill install --force` replaces it",
            dir.display()
        ),
        _ => {
            write(&dir)?;
            lines.push(format!(
                "installed the axe skill ({BUILD}) at {}",
                dir.display()
            ));
        }
    }
    if !no_link {
        lines.extend(link(&dir, dir == default)?);
    }
    Ok(lines)
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none())
}

/// Links `~/.claude/skills/axe` to `dir` when `~/.claude` exists and the
/// link path is free or already that link; anything else is left alone.
fn link(dir: &Path, default: bool) -> Result<Vec<String>> {
    let claude = home()?.join(".claude");
    if !claude.is_dir() {
        return Ok(vec![]);
    }
    let path = claude.join("skills/axe");
    let shown = path.display();
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path.parent().unwrap())?;
            // The default layout links relatively, as `~/.claude/skills/axe ->
            // ../../.agents/skills/axe`, so it survives a moved home.
            let target = if default {
                PathBuf::from("../../.agents/skills/axe")
            } else {
                std::path::absolute(dir)?
            };
            symlink(&target, &path).with_context(|| format!("link {shown}"))?;
            Ok(vec![format!("linked {shown} -> {}", target.display())])
        }
        Err(e) => Err(e).with_context(|| format!("stat {shown}")),
        Ok(m) if m.file_type().is_symlink() => {
            let same = path.canonicalize().ok() == dir.canonicalize().ok();
            Ok(if same {
                vec![]
            } else {
                vec![format!(
                    "{shown} links elsewhere; left alone (point it at {})",
                    dir.display()
                )]
            })
        }
        Ok(_) => Ok(vec![format!(
            "{shown} is not a symlink; left alone (replace it with a link to {} to use this copy)",
            dir.display()
        )]),
    }
}

#[cfg(unix)]
fn symlink(target: &Path, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("symlinks are not supported here"))
}

/// `axe skill status`: the state of `dir` (default [`default_dir`]).
pub fn status(dir: Option<&Path>) -> Result<String> {
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => default_dir()?,
    };
    let shown = dir.display();
    Ok(match inspect(&dir)? {
        State::Missing => format!("{shown}: not installed (`axe skill install`)"),
        State::Symlink => format!("{shown}: a symlink, not managed by axe"),
        State::Unstamped { current: true } => {
            format!("{shown}: not installed by axe, matches this binary ({BUILD})")
        }
        State::Unstamped { current: false } => format!(
            "{shown}: not installed by axe, differs from this binary ({BUILD}); \
             `axe skill install --force` replaces it"
        ),
        State::Modified(s) => format!(
            "{shown}: installed by axe {}, edited since; `axe skill install --force` replaces it",
            s.build
        ),
        State::Stamped { stamp, current } if current => {
            format!("{shown}: up to date ({})", stamp.build)
        }
        State::Stamped { stamp, .. } => format!(
            "{shown}: installed by axe {}, this binary is {BUILD}; `axe skill install` refreshes it",
            stamp.build
        ),
    })
}

/// Whether the session is an agent's (`$AXE_TASK` or `$AXE_BOT` set).
pub fn agent_session() -> bool {
    ["AXE_TASK", "AXE_BOT"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// `~/.cache/axe` (the platform's user cache dir).
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("axe"))
}

/// Run before every command except `skill` and `upgrade`: rewrites a stale
/// stamped copy at [`default_dir`] with one stderr line, and hints (at most
/// once a day per state, never in an agent session) at an unstamped or
/// hand-edited copy that differs from this binary. Off with `[cli]
/// skill_refresh = false` or `AXE_SKILL_REFRESH=0`. Never fails.
pub fn auto_refresh() {
    if SKILL_FILES.is_empty() || !crate::config::cli_switch("AXE_SKILL_REFRESH", "skill_refresh") {
        return;
    }
    let Ok(dir) = default_dir() else {
        return;
    };
    // Another command is installing or refreshing it: leave it to that one.
    let Some(_lock) = lock(false) else {
        return;
    };
    let hint = match inspect(&dir) {
        Ok(State::Stamped {
            current: false,
            stamp,
        }) => {
            let changed = stamp.sha256 != hash_files(&embedded());
            match write(&dir) {
                Ok(()) if changed => {
                    eprintln!("axe: updated the skill at {} to {BUILD}", dir.display())
                }
                Ok(()) => {}
                Err(e) => eprintln!(
                    "axe: could not update the skill at {}: {e:#}",
                    dir.display()
                ),
            }
            return;
        }
        Ok(State::Modified(_)) => "was edited since axe installed it",
        Ok(State::Unstamped { current: false }) => "was not installed by axe",
        _ => return,
    };
    if agent_session() || !hint_due(&dir) {
        return;
    }
    eprintln!(
        "axe: the skill at {} {hint} and differs from this binary's ({BUILD}); \
         left as is, `axe skill install --force` replaces it",
        dir.display()
    );
}

/// The skill lock (`<cache dir>/axe/skill.lock`), held while a copy is
/// inspected and rewritten so concurrent commands never interleave renames.
/// `wait` false returns None when another process holds it. Without a cache
/// dir there is no lock (`Some(None)`).
fn lock(wait: bool) -> Option<Option<std::fs::File>> {
    use fs2::FileExt;
    let Some(dir) = cache_dir() else {
        return Some(None);
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("skill.lock"))
    else {
        return Some(None);
    };
    let locked = if wait {
        file.lock_exclusive().is_ok()
    } else {
        file.try_lock_exclusive().is_ok()
    };
    match (locked, wait) {
        (true, _) => Some(Some(file)),
        (false, true) => Some(None),
        (false, false) => None,
    }
}

/// Whether the hint for `dir` in its current state has not been printed in
/// the last day; records it as printed when due.
fn hint_due(dir: &Path) -> bool {
    let Some(cache) = cache_dir() else {
        return true;
    };
    let file = cache.join("skill-hint.json");
    let key = tree_hash(dir).unwrap_or_default();
    let now = now_secs();
    let last: Option<(String, u64)> = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Some((k, at)) = last {
        if k == key && now.saturating_sub(at) < 24 * 3600 {
            return false;
        }
    }
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::write(&file, serde_json::to_vec(&(key, now)).unwrap_or_default());
    true
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
