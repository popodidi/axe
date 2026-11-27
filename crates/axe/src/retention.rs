//! Retention of what runs leave on the node: run logs under `log_dir` (task
//! runs, `bots/<bot>/`, `services/<bot>/`, each log with the files named
//! after it) and the session transcripts of the node's runtimes (Claude Code
//! `<login>/projects/<dir>/<session>.jsonl` with its `<session>/` folder,
//! Codex `<login>/sessions/**/rollout-*-<session>.jsonl`).
//!
//! `[housekeeping.logs]` sets a max age (default 30 days) and an optional max
//! total size of both together; a sweep deletes the oldest item first until
//! both hold ([`plan`]). Never deleted: a log the node journal lists as
//! unfinished, a file a process has open, anything modified in the last
//! [`RECENT`] seconds, a transcript of a session no axe run had (the
//! owner's own CLI use) and one whose session a task not done, a bot or a
//! live run can still resume (`GET /api/sessions`). When the journal or that
//! lookup fails, its kind is left alone for the sweep. The housekeeper
//! runs it (`housekeeping.rs`); a pruned run's log reads as pruned (`remote.rs`).
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::config::{Config, HousekeepingLogs};

/// Anything modified this recently is in use, whatever the limits say.
pub const RECENT: i64 = 3600;

/// `[housekeeping.logs]`, parsed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limits {
    /// Seconds; `None` keeps any age.
    pub max_age: Option<u64>,
    /// Bytes; `None` means no size limit.
    pub max_size: Option<u64>,
}

impl Limits {
    pub fn new(c: &HousekeepingLogs) -> Result<Limits> {
        let max_age = match c.max_age.trim() {
            "off" => None,
            a => Some(crate::bot::parse_duration(a).context("logs.max_age")? as u64),
        };
        let max_size = c
            .max_size
            .as_deref()
            .map(parse_size)
            .transpose()
            .context("logs.max_size")?;
        Ok(Limits { max_age, max_size })
    }

    pub fn off(&self) -> bool {
        self.max_age.is_none() && self.max_size.is_none()
    }
}

/// Bytes of `20G`, `500M`, `64KiB` or a plain number (binary units).
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: u64 = n
        .parse()
        .with_context(|| format!("bad size {s:?}: use <n>[KMGT]"))?;
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        _ => bail!("bad size {s:?}: use <n>[KMGT]"),
    };
    if n == 0 {
        bail!("size {s:?} must be positive");
    }
    n.checked_mul(1 << shift)
        .with_context(|| format!("size {s:?} is too large"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Log,
    Session,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Log => "log",
            Kind::Session => "session",
        }
    }
}

/// One deletable unit: a run log with the files named after it, or one
/// session's transcript files.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: Kind,
    /// The log or transcript first.
    pub paths: Vec<PathBuf>,
    /// Allocated bytes, as `du` counts them.
    pub bytes: u64,
    /// Newest modification time (unix seconds) of anything in it.
    pub modified: i64,
    /// The session id of a transcript.
    pub session: Option<String>,
}

impl Item {
    fn new(kind: Kind, mut paths: Vec<PathBuf>, session: Option<String>) -> Option<Item> {
        paths.sort_by_key(|p| main_rank(p));
        let (mut bytes, mut modified) = (0, 0);
        for p in &paths {
            let (b, m) = usage(p).ok()?;
            bytes += b;
            modified = modified.max(m);
        }
        Some(Item {
            kind,
            paths,
            bytes,
            modified,
            session,
        })
    }

    /// The item as it is on disk now (`None` once gone).
    pub fn restat(&self) -> Option<Item> {
        let paths = self.paths.iter().filter(|p| p.exists()).cloned().collect();
        Item::new(self.kind, paths, self.session.clone()).filter(|i| !i.paths.is_empty())
    }
}

/// Sorts the log or transcript ahead of the files beside it.
fn main_rank(p: &Path) -> (u8, PathBuf) {
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    let rank = if name.ends_with(".log") || name.ends_with(".jsonl") {
        0
    } else {
        1
    };
    (rank, p.to_path_buf())
}

/// Allocated bytes and newest mtime of a file or tree (symlinks not followed).
fn usage(path: &Path) -> std::io::Result<(u64, i64)> {
    let meta = fs::symlink_metadata(path)?;
    let (mut bytes, mut modified) = (meta.blocks() * 512, meta.mtime());
    if !meta.is_dir() {
        return Ok((bytes, modified));
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            bytes += meta.blocks() * 512;
            modified = modified.max(meta.mtime());
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok((bytes, modified))
}

/// Subdirectories of `dir` (not symlinks), sorted; none when it is missing.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// `<digits>` (a bot run) or `<digits>-<digits>` (a task run).
fn run_stem(stem: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match stem.split_once('-') {
        Some((a, b)) => digits(a) && digits(b),
        None => digits(stem),
    }
}

/// Run logs under `root`: in it, and in each `bots/<bot>/` and
/// `services/<bot>/`. Files group by the name before the first `.`
/// (`12-3.log`, `12-3.prompt.md`, `12-3.fresh.log`); a group is a run log
/// when it holds a `.log` (or a rotated `.log.1`) or has a run's name.
/// Hidden files, folders and other files (a bot's `system.md`) stay.
pub fn scan_logs(root: &Path) -> Vec<Item> {
    let mut dirs = vec![root.to_path_buf()];
    for sub in ["bots", "services"] {
        dirs.extend(subdirs(&root.join(sub)));
    }
    let mut items = Vec::new();
    for dir in dirs {
        let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let stem = name.split('.').next().unwrap_or_default().to_string();
            groups.entry(stem).or_default().push(entry.path());
        }
        for (stem, paths) in groups {
            let has_log = paths.iter().any(|p| {
                let n = p.file_name().unwrap_or_default().to_string_lossy();
                n.ends_with(".log") || n.contains(".log.")
            });
            if has_log || run_stem(&stem) {
                items.extend(Item::new(Kind::Log, paths, None));
            }
        }
    }
    items
}

/// A session id: a UUID (`8-4-4-4-12` hex digits).
fn session_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Claude Code transcripts: `<login>/projects/<dir>/<id>.jsonl` and the
/// `<id>/` folder beside it (subagents, tool results). Other entries (a
/// project's `memory/`) stay.
pub fn scan_claude(login: &Path) -> Vec<Item> {
    let mut items = Vec::new();
    for project in subdirs(&login.join("projects")) {
        let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for entry in fs::read_dir(&project).into_iter().flatten().flatten() {
            let Ok(t) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            let id = if t.is_file() {
                name.strip_suffix(".jsonl")
            } else if t.is_dir() {
                Some(name.as_str())
            } else {
                None
            };
            if let Some(id) = id.filter(|id| session_id(id)) {
                groups.entry(id.to_string()).or_default().push(entry.path());
            }
        }
        for (id, paths) in groups {
            items.extend(Item::new(Kind::Session, paths, Some(id)));
        }
    }
    items
}

/// Codex transcripts: `<login>/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
pub fn scan_codex(login: &Path) -> Vec<Item> {
    let mut items = Vec::new();
    let mut stack = vec![(login.join("sessions"), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(t) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().to_string();
            if t.is_dir() && depth < 4 {
                stack.push((entry.path(), depth + 1));
                continue;
            }
            let Some(stem) = name
                .strip_prefix("rollout-")
                .and_then(|n| n.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let id = stem
                .get(stem.len().saturating_sub(36)..)
                .unwrap_or_default();
            if t.is_file() && session_id(id) {
                items.extend(Item::new(
                    Kind::Session,
                    vec![entry.path()],
                    Some(id.to_string()),
                ));
            }
        }
    }
    items
}

/// The login directory of each runtime of the node, with its harness:
/// where its CLI keeps session transcripts. Each directory once.
pub fn login_roots(cfg: &Config) -> Result<Vec<(String, PathBuf)>> {
    let reg = crate::runtime::Registry::load(cfg)?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for rt in &reg.runtimes {
        let Some(dir) = rt.login_root() else { continue };
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen.insert((rt.harness.clone(), key)) {
            out.push((rt.harness.clone(), dir));
        }
    }
    Ok(out)
}

/// Transcripts under the login directories `roots` returns.
pub fn scan_sessions(roots: &[(String, PathBuf)]) -> Vec<Item> {
    let mut items = Vec::new();
    for (harness, dir) in roots {
        match crate::harness::get(harness).map(|h| h.transcripts) {
            Some(crate::harness::Transcripts::Claude) => items.extend(scan_claude(dir)),
            Some(crate::harness::Transcripts::Codex) => items.extend(scan_codex(dir)),
            None => {}
        }
    }
    items
}

/// Logs the node journal (`<log_dir>/.dispatcher/journal.json`) lists as not
/// finished: runs still going, task and bot alike. A missing journal lists
/// none.
pub fn unfinished_logs(log_dir: &Path) -> Result<HashSet<PathBuf>> {
    let path = log_dir.join(".dispatcher").join("journal.json");
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let journal: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let mut out = HashSet::new();
    for key in ["logs", "bot_logs"] {
        for log in journal[key]
            .as_object()
            .into_iter()
            .flat_map(|m| m.values())
        {
            if log["finished"] != Value::Bool(true) {
                if let Some(p) = log["path"].as_str() {
                    let p = PathBuf::from(p);
                    out.insert(p.canonicalize().unwrap_or(p));
                }
            }
        }
    }
    Ok(out)
}

/// Whether one of the item's files is in `paths` (canonical).
pub fn listed(item: &Item, paths: &HashSet<PathBuf>) -> bool {
    item.paths.iter().any(|p| {
        let p = p.canonicalize().unwrap_or_else(|_| p.clone());
        paths.contains(&p)
    })
}

/// One decision of [`plan`]: delete `items[index]`, or keep it although it
/// is over a limit (`keep` says why).
#[derive(Debug, PartialEq)]
pub struct Decision {
    pub index: usize,
    pub reason: String,
    pub keep: Option<String>,
}

/// Oldest first: each item over the max age, then each while the total is
/// over the max size, is deleted unless `protect` (or [`RECENT`]) keeps it.
/// Kept items still count toward the total.
pub fn plan(
    items: &[Item],
    limits: &Limits,
    now: i64,
    protect: impl Fn(&Item) -> Option<String>,
) -> Vec<Decision> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| {
        (items[a].modified, &items[a].paths).cmp(&(items[b].modified, &items[b].paths))
    });
    let mut total: u64 = items.iter().map(|i| i.bytes).sum();
    let mut out = Vec::new();
    for index in order {
        let item = &items[index];
        let age = (now - item.modified).max(0);
        let reason = match (limits.max_age, limits.max_size) {
            (Some(max), _) if age > max as i64 => format!("{age}s old, max age {max}s"),
            (_, Some(max)) if total > max => {
                format!("total {total} bytes over max size {max}, {age}s old")
            }
            // Sorted oldest first: nothing after this is over a limit.
            _ => break,
        };
        let keep = if age < RECENT {
            Some(format!("modified {age}s ago"))
        } else {
            protect(item)
        };
        if keep.is_none() {
            total = total.saturating_sub(item.bytes);
        }
        out.push(Decision {
            index,
            reason,
            keep,
        });
    }
    out
}

/// Deletes the item's files and folders; a Codex day folder left empty goes too.
pub fn remove(item: &Item) -> Result<()> {
    for p in &item.paths {
        let meta = match fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat {}", p.display())),
        };
        if meta.is_dir() {
            fs::remove_dir_all(p)
        } else {
            fs::remove_file(p)
        }
        .with_context(|| format!("remove {}", p.display()))?;
    }
    if item.kind == Kind::Session {
        if let Some(mut dir) = item.paths[0].parent().map(Path::to_path_buf) {
            // `sessions/YYYY/MM/DD`: up to three empty levels; fails on the first non-empty one.
            for _ in 0..3 {
                let date = dir
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().bytes().all(|b| b.is_ascii_digit()));
                if !date || fs::remove_dir(&dir).is_err() {
                    break;
                }
                match dir.parent() {
                    Some(p) => dir = p.to_path_buf(),
                    None => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    const DAY: i64 = 86400;

    fn now() -> i64 {
        crate::remote::epoch()
    }

    /// Writes `bytes` bytes to `path`, modified `age` seconds ago.
    fn put(path: &Path, bytes: usize, age: i64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![b'x'; bytes]).unwrap();
        touch(path, age);
    }

    fn touch(path: &Path, age: i64) {
        let t = SystemTime::now() - Duration::from_secs(age as u64);
        fs::File::open(path).unwrap().set_modified(t).unwrap();
    }

    fn names(items: &[Item], decisions: &[Decision], deleted: bool) -> Vec<String> {
        let mut out: Vec<String> = decisions
            .iter()
            .filter(|d| d.keep.is_none() == deleted)
            .map(|d| {
                items[d.index].paths[0]
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn limits_and_sizes() {
        let l = Limits::new(&HousekeepingLogs::default()).unwrap();
        assert_eq!(l.max_age, Some(30 * DAY as u64));
        assert_eq!(l.max_size, None);
        let l = Limits::new(&HousekeepingLogs {
            max_age: "off".into(),
            max_size: Some("2G".into()),
        })
        .unwrap();
        assert_eq!((l.max_age, l.max_size), (None, Some(2 << 30)));
        assert!(!l.off());
        assert_eq!(parse_size("500MiB").unwrap(), 500 << 20);
        assert_eq!(parse_size("64k").unwrap(), 64 << 10);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        for bad in ["", "0", "G", "1X", "-1G", "1.5G"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
        assert!(Limits::new(&HousekeepingLogs {
            max_age: "soon".into(),
            max_size: None
        })
        .is_err());
    }

    #[test]
    fn logs_group_by_run_and_skip_bot_state() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        put(&root.join("12-3.log"), 10, 0);
        put(&root.join("12-3.prompt.md"), 10, 0);
        put(&root.join("12-3.fresh.log"), 10, 0);
        put(&root.join("12-4.prompt.md"), 10, 0);
        put(&root.join("manual.log"), 10, 0);
        put(&root.join("manual.sid"), 10, 0);
        put(&root.join("notes.txt"), 10, 0);
        put(&root.join(".dispatcher/journal.json"), 10, 0);
        put(&root.join("bots/ops/7.log"), 10, 0);
        put(&root.join("bots/ops/7.times"), 10, 0);
        put(&root.join("bots/ops/system.md"), 10, 0);
        put(&root.join("bots/ops/settings.json"), 10, 0);
        put(&root.join("services/ops/probe.log"), 10, 0);
        put(&root.join("services/ops/probe.log.1"), 10, 0);
        let mut groups: Vec<Vec<String>> = scan_logs(root)
            .iter()
            .map(|i| {
                i.paths
                    .iter()
                    .map(|p| p.strip_prefix(root).unwrap().display().to_string())
                    .collect()
            })
            .collect();
        groups.sort();
        assert_eq!(
            groups,
            vec![
                vec!["12-3.fresh.log", "12-3.log", "12-3.prompt.md"],
                vec!["12-4.prompt.md"],
                vec!["bots/ops/7.log", "bots/ops/7.times"],
                vec!["manual.log", "manual.sid"],
                vec!["services/ops/probe.log", "services/ops/probe.log.1"],
            ]
        );
    }

    #[test]
    fn age_limit_deletes_old_and_keeps_recent() {
        let d = tempfile::tempdir().unwrap();
        put(&d.path().join("1-1.log"), 10, 40 * DAY);
        put(&d.path().join("1-1.prompt.md"), 10, 40 * DAY);
        put(&d.path().join("2-1.log"), 10, 10 * DAY);
        let items = scan_logs(d.path());
        let limits = Limits {
            max_age: Some(30 * DAY as u64),
            max_size: None,
        };
        let ds = plan(&items, &limits, now(), |_| None);
        assert_eq!(names(&items, &ds, true), ["1-1.log"]);
        assert!(names(&items, &ds, false).is_empty());
        for d in &ds {
            remove(&items[d.index]).unwrap();
        }
        assert!(!d.path().join("1-1.log").exists());
        assert!(!d.path().join("1-1.prompt.md").exists());
        assert!(d.path().join("2-1.log").exists());
    }

    #[test]
    fn size_limit_deletes_oldest_until_under() {
        let d = tempfile::tempdir().unwrap();
        for (i, age) in [(1, 5), (2, 4), (3, 3), (4, 2)] {
            put(&d.path().join(format!("{i}-1.log")), 8192, age * DAY);
        }
        let items = scan_logs(d.path());
        let total: u64 = items.iter().map(|i| i.bytes).sum();
        let each = items[0].bytes;
        let limits = Limits {
            max_age: None,
            max_size: Some(total - each - 1),
        };
        // Two must go to get under the limit: the two oldest.
        let ds = plan(&items, &limits, now(), |_| None);
        assert_eq!(names(&items, &ds, true), ["1-1.log", "2-1.log"]);
        // The oldest is protected: it counts, and the next two go instead.
        let ds = plan(&items, &limits, now(), |i| {
            i.paths[0]
                .ends_with("1-1.log")
                .then(|| "running".to_string())
        });
        assert_eq!(names(&items, &ds, true), ["2-1.log", "3-1.log"]);
        assert_eq!(names(&items, &ds, false), ["1-1.log"]);
        assert_eq!(ds[0].keep.as_deref(), Some("running"));
    }

    #[test]
    fn running_logs_from_the_journal_are_protected() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        put(&root.join("1-1.log"), 10, 40 * DAY);
        put(&root.join("2-1.log"), 10, 40 * DAY);
        put(&root.join("bots/ops/9.log"), 10, 40 * DAY);
        let journal = serde_json::json!({
            "id": "x", "generation": "g", "pending": null,
            "logs": {
                "1": {"path": root.join("1-1.log"), "finished": false},
                "2": {"path": root.join("2-1.log"), "finished": true},
            },
            "bot_logs": {"5": {"path": root.join("bots/ops/9.log"), "finished": false}},
        });
        fs::create_dir_all(root.join(".dispatcher")).unwrap();
        fs::write(root.join(".dispatcher/journal.json"), journal.to_string()).unwrap();
        let running = unfinished_logs(root).unwrap();
        let items = scan_logs(root);
        let limits = Limits {
            max_age: Some(30 * DAY as u64),
            max_size: None,
        };
        let ds = plan(&items, &limits, now(), |i| {
            listed(i, &running).then(|| "run still going".to_string())
        });
        assert_eq!(names(&items, &ds, true), ["2-1.log"]);
        assert_eq!(names(&items, &ds, false), ["1-1.log", "9.log"]);
        // No journal: nothing is running.
        assert!(unfinished_logs(&root.join("bots")).unwrap().is_empty());
        fs::write(root.join(".dispatcher/journal.json"), "{").unwrap();
        assert!(unfinished_logs(root).is_err());
    }

    #[test]
    fn recent_files_are_kept_even_over_the_size_limit() {
        let d = tempfile::tempdir().unwrap();
        put(&d.path().join("1-1.log"), 4096, 60);
        let items = scan_logs(d.path());
        let limits = Limits {
            max_age: None,
            max_size: Some(1),
        };
        let ds = plan(&items, &limits, now(), |_| None);
        assert_eq!(ds.len(), 1);
        assert!(ds[0].keep.as_deref().unwrap().starts_with("modified "));
    }

    #[test]
    fn resumable_sessions_are_protected() {
        let d = tempfile::tempdir().unwrap();
        let claude = d.path().join("claude");
        let (a, b, c) = (
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222",
            "33333333-3333-4333-8333-333333333333",
        );
        let p = claude.join("projects/-w-task");
        put(&p.join(format!("{a}.jsonl")), 10, 40 * DAY);
        put(&p.join(format!("{a}/subagents/x.jsonl")), 10, 40 * DAY);
        touch(&p.join(a), 40 * DAY);
        touch(&p.join(format!("{a}/subagents")), 40 * DAY);
        put(&p.join(format!("{b}.jsonl")), 10, 40 * DAY);
        put(&p.join(format!("{c}.jsonl")), 10, DAY);
        put(&p.join("memory/MEMORY.md"), 10, 40 * DAY);
        let codex = d.path().join("codex");
        let old = codex.join(format!(
            "sessions/2026/10/01/rollout-2026-10-01T00-00-00-{b}.jsonl"
        ));
        put(&old, 10, 40 * DAY);
        put(
            &codex.join(format!(
                "sessions/2026/10/02/rollout-2026-10-02T00-00-00-{a}.jsonl"
            )),
            10,
            40 * DAY,
        );
        let roots = vec![
            ("claude-code".to_string(), claude.clone()),
            ("codex".to_string(), codex.clone()),
            ("other".to_string(), d.path().to_path_buf()),
        ];
        let items = scan_sessions(&roots);
        assert_eq!(items.len(), 5, "{items:?}");
        let resumable: HashSet<String> = [b.to_string()].into();
        let limits = Limits {
            max_age: Some(30 * DAY as u64),
            max_size: None,
        };
        let ds = plan(&items, &limits, now(), |i| {
            i.session
                .as_ref()
                .filter(|s| resumable.contains(*s))
                .map(|_| "resumable".to_string())
        });
        let gone: Vec<&Item> = ds
            .iter()
            .filter(|d| d.keep.is_none())
            .map(|d| &items[d.index])
            .collect();
        assert_eq!(gone.len(), 2);
        assert!(gone.iter().all(|i| i.session.as_deref() == Some(a)));
        for i in gone {
            remove(i).unwrap();
        }
        assert!(!p.join(format!("{a}.jsonl")).exists());
        assert!(!p.join(a).exists());
        assert!(p.join(format!("{b}.jsonl")).exists());
        assert!(p.join(format!("{c}.jsonl")).exists());
        assert!(p.join("memory/MEMORY.md").exists());
        assert!(old.exists());
        // The emptied Codex day folder went with it.
        assert!(!codex.join("sessions/2026/10/02").exists());
        assert!(codex.join("sessions/2026/10/01").exists());
        assert!(codex.join("sessions").exists());
    }
}
