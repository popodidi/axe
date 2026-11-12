//! Housekeeping: remove what the dispatcher's tasks leave behind once a task
//! has been `done` long enough.
//!
//! Candidates are task worktrees under `<workspace_root>/.worktrees/<project>/`
//! (`task-<id>--<purpose>`, and `<repo>/task-<id>--<purpose>` for dependency
//! worktrees) and the configured `[housekeeping] task_paths`
//! (`.../task-{id}-target` and the like, rebuildable output only). A worktree
//! is removed only when nothing in it could be lost: no live run, clean git
//! status and every commit on a remote branch. Ignored files may go.
//!
//! With `[housekeeping.pressure]`, low free space shortens the retention and
//! trims rebuildable caches; with `[housekeeping.alert]`, a long admission
//! pause on disk wakes a bot once.
//!
//! Optional reclaim rules for what that misses:
//! - `[housekeeping.build_output]`: git-ignored build output (`target`,
//!   `node_modules`, ...) in the worktrees of idle done, blocked or escalated
//!   tasks; source and commits stay.
//! - `[housekeeping.tmp]`: task-named scratch in a temp directory once the task
//!   is done (or held and idle, above a size floor), and under pressure other
//!   stale scratch.
//! - `[housekeeping.orphan_caches]`: cache-root entries no task path claims,
//!   once unused for a while.
//!
//! Runs as a tick inside `axe dispatch` (`[dispatcher] housekeep`), or once
//! with `axe housekeeping --dry-run`. Each decision is one JSON line on stdout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::admission::{self, Probe};
use crate::client::Client;
use crate::config::{expand_tilde, Config, Housekeep};

/// How often the dispatcher runs a housekeeping tick.
pub const TICK: Duration = Duration::from_secs(180);
/// Unpressured sweeps and cache trims run at most this often.
const HOURLY: Duration = Duration::from_secs(3600);

/// `[housekeeping]`, validated and resolved.
#[derive(Debug, Clone)]
pub struct Rules {
    /// `<workspace_root>/.worktrees`: one root per project directory.
    worktrees: PathBuf,
    retention: u64,
    task_paths: Vec<TaskPath>,
    pressure: Option<Pressure>,
    alert: Option<Alert>,
    build_output: Option<BuildOutput>,
    tmp: Option<Tmp>,
    orphans: Option<Orphans>,
}

#[derive(Debug, Clone)]
struct BuildOutput {
    idle: u64,
    names: Vec<String>,
}

#[derive(Debug, Clone)]
struct Tmp {
    dir: PathBuf,
    idle: u64,
    floor_bytes: u64,
    stale: u64,
    shared: Vec<String>,
}

#[derive(Debug, Clone)]
struct Orphans {
    roots: Vec<PathBuf>,
    unused: u64,
}

#[derive(Debug, Clone)]
struct Pressure {
    path: PathBuf,
    free_below_gib: u64,
    retention: u64,
    cache_trim: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
struct Alert {
    bot: String,
    after: i64,
}

/// A `task_paths` template: `<dir>/<prefix>{id}<suffix>`.
#[derive(Debug, Clone)]
struct TaskPath {
    template: String,
    dir: PathBuf,
    prefix: String,
    suffix: String,
}

impl TaskPath {
    fn parse(template: &str, worktrees: &Path) -> Result<TaskPath> {
        let path = expand_tilde(template);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("task_paths {template:?} has no file name"))?;
        let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let Some((prefix, suffix)) = name.split_once("{id}") else {
            bail!("task_paths {template:?}: the last component needs {{id}}");
        };
        ensure!(
            !prefix.is_empty(),
            "task_paths {template:?}: {{id}} needs a fixed prefix, e.g. task-{{id}}-target"
        );
        ensure!(
            !dir.to_string_lossy().contains('{') && !prefix.contains('{') && !suffix.contains('{'),
            "task_paths {template:?}: only one {{id}}, in the last component"
        );
        ensure!(dir.is_absolute(), "task_paths {template:?} is not absolute");
        ensure!(
            !dir.starts_with(worktrees),
            "task_paths {template:?} is inside {}, which holds worktrees",
            worktrees.display()
        );
        Ok(TaskPath {
            template: name.to_string(),
            dir,
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    }

    /// The task id of `name`, a positive integer without leading zeros.
    fn task(&self, name: &str) -> Option<u64> {
        let id = name
            .strip_prefix(&self.prefix)?
            .strip_suffix(&self.suffix)?;
        parse_id(id)
    }
}

/// Seconds of a `[housekeeping]` duration (`90m`, `6h`, `1d`).
fn seconds(field: &str, value: &str) -> Result<u64> {
    let s = crate::bot::parse_duration(value).with_context(|| field.to_string())?;
    Ok(s as u64)
}

impl Rules {
    pub fn new(cfg: &Config) -> Result<Rules> {
        let h = &cfg.housekeeping;
        let worktrees = cfg.workspace_root().join(".worktrees");
        let retention = seconds("retention", &h.retention)?;
        let task_paths = h
            .task_paths
            .iter()
            .map(|t| TaskPath::parse(t, &worktrees))
            .collect::<Result<Vec<_>>>()?;
        let pressure = match &h.pressure {
            None => None,
            Some(p) => {
                let path = p
                    .path
                    .as_deref()
                    .map_or_else(|| cfg.workspace_root(), expand_tilde);
                ensure!(path.is_absolute(), "pressure.path is not absolute");
                let short = seconds("pressure.retention", &p.retention)?;
                ensure!(short <= retention, "pressure.retention exceeds retention");
                ensure!(
                    p.cache_trim
                        .iter()
                        .all(|c| c.first().is_some_and(|b| Path::new(b).is_absolute())),
                    "pressure.cache_trim commands need an absolute program"
                );
                Some(Pressure {
                    path,
                    free_below_gib: p.free_below_gib,
                    retention: short,
                    cache_trim: p.cache_trim.clone(),
                })
            }
        };
        let alert = match &h.alert {
            None => None,
            Some(a) => {
                ensure!(!a.bot.is_empty(), "alert.bot is empty");
                Some(Alert {
                    bot: a.bot.clone(),
                    after: seconds("alert.after", &a.after)? as i64,
                })
            }
        };
        let build_output = match &h.build_output {
            None => None,
            Some(b) => {
                ensure!(
                    !b.names.is_empty()
                        && b.names.iter().all(|n| {
                            !n.is_empty()
                                && !n.contains('/')
                                && n != "."
                                && n != ".."
                                && n != ".git"
                        }),
                    "build_output.names must be plain directory names"
                );
                Some(BuildOutput {
                    idle: seconds("build_output.idle", &b.idle)?,
                    names: b.names.clone(),
                })
            }
        };
        let tmp = match &h.tmp {
            None => None,
            Some(t) => {
                let dir = expand_tilde(&t.dir);
                ensure!(dir.is_absolute(), "tmp.dir is not absolute");
                ensure!(
                    !worktrees.starts_with(&dir) && !dir.starts_with(&worktrees),
                    "tmp.dir {} overlaps {}, which holds worktrees",
                    dir.display(),
                    worktrees.display()
                );
                Some(Tmp {
                    dir,
                    idle: seconds("tmp.idle", &t.idle)?,
                    floor_bytes: t.floor_mib.saturating_mul(1 << 20),
                    stale: seconds("tmp.stale", &t.stale)?,
                    shared: t.shared.clone(),
                })
            }
        };
        let orphans = match &h.orphan_caches {
            None => None,
            Some(o) => {
                let roots = o.roots.iter().map(|r| expand_tilde(r)).collect::<Vec<_>>();
                for root in &roots {
                    ensure!(
                        root.is_absolute(),
                        "orphan_caches root {} is not absolute",
                        root.display()
                    );
                    ensure!(
                        !worktrees.starts_with(root) && !root.starts_with(&worktrees),
                        "orphan_caches root {} overlaps {}, which holds worktrees",
                        root.display(),
                        worktrees.display()
                    );
                }
                Some(Orphans {
                    roots,
                    unused: seconds("orphan_caches.unused", &o.unused)?,
                })
            }
        };
        Ok(Rules {
            worktrees,
            retention,
            task_paths,
            pressure,
            alert,
            build_output,
            tmp,
            orphans,
        })
    }
}

#[derive(Deserialize, Clone)]
struct TaskRow {
    id: u64,
    project: String,
    state: String,
    /// Derived: `blocked` also for a pending task that waits on a
    /// prerequisite or a wake time.
    #[serde(default)]
    status: String,
    #[serde(default)]
    updated_at: String,
}

impl TaskRow {
    fn status(&self) -> &str {
        if self.status.is_empty() {
            &self.state
        } else {
            &self.status
        }
    }

    /// `Ok(reason)` when the task is done, blocked or escalated and has not
    /// been updated for `idle` seconds: nothing runs in it soon.
    fn idle_for(&self, idle: u64, now: i64) -> Result<String, String> {
        let status = self.status();
        if !matches!(status, "done" | "blocked" | "escalated") {
            return Err(format!("task status is {status:?}"));
        }
        let at = OffsetDateTime::parse(&self.updated_at, &Rfc3339)
            .map_err(|e| format!("bad updated_at {:?}: {e}", self.updated_at))?;
        let age = (now - at.unix_timestamp()).max(0);
        if age < idle as i64 {
            return Err(format!("{status}, updated {age}s ago, idle {idle}s"));
        }
        Ok(format!("{status}, idle {age}s"))
    }
}

#[derive(Deserialize, Clone)]
struct Event {
    at: String,
    kind: String,
    to_value: Option<String>,
}

struct Candidate {
    path: PathBuf,
    task: u64,
}

fn parse_id(id: &str) -> Option<u64> {
    let digits = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    if !digits || id.starts_with('0') {
        return None;
    }
    id.parse().ok()
}

/// `task-<id>--<purpose>` with a positive id without leading zeros.
fn task_id(name: &str) -> Option<u64> {
    let (id, purpose) = name.strip_prefix("task-")?.split_once("--")?;
    if purpose.is_empty() {
        return None;
    }
    parse_id(id)
}

fn sorted_entries(dir: &Path) -> std::io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

/// Project roots: real directories directly under `.worktrees`.
fn roots(worktrees: &Path) -> Result<Vec<(PathBuf, String)>> {
    let entries = match sorted_entries(worktrees) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", worktrees.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        if entry.file_type()?.is_dir() {
            if let Some(project) = entry.file_name().to_str() {
                out.push((entry.path(), project.to_string()));
            }
        }
    }
    Ok(out)
}

/// Task-named children of `root`, plus task-named grandchildren under real
/// (non-symlink, non-task) child directories: dependency worktrees.
fn candidates(root: &Path) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    let entries = match sorted_entries(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
    };
    for entry in entries {
        let name = entry.file_name();
        if let Some(task) = name.to_str().and_then(task_id) {
            out.push(Candidate {
                path: entry.path(),
                task,
            });
        } else if entry.file_type()?.is_dir() {
            // Unreadable non-task directories are not ours to report on.
            for child in sorted_entries(&entry.path()).unwrap_or_default() {
                if let Some(task) = child.file_name().to_str().and_then(task_id) {
                    out.push(Candidate {
                        path: child.path(),
                        task,
                    });
                }
            }
        }
    }
    Ok(out)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Seconds since the latest event that moved the task to `done`.
fn done_age(events: &[Event], now: i64) -> Result<i64, String> {
    let event = events
        .iter()
        .rev()
        .find(|e| e.kind == "state" && e.to_value.as_deref() == Some("done"))
        .ok_or("no state event to done")?;
    let at = OffsetDateTime::parse(&event.at, &Rfc3339)
        .map_err(|e| format!("bad done event time {:?}: {e}", event.at))?;
    Ok((now - at.unix_timestamp()).max(0))
}

/// Whether the task's latest run has started and not ended.
fn run_is_live(events: &[Event]) -> bool {
    events
        .iter()
        .rev()
        .find(|e| matches!(e.kind.as_str(), "run_start" | "run_end"))
        .is_some_and(|e| e.kind == "run_start")
}

/// A process whose working directory is inside `path`.
fn process_inside(path: &Path) -> Option<u32> {
    let path = path.canonicalize().ok()?;
    fs::read_dir("/proc").ok()?.flatten().find_map(|entry| {
        let pid = entry.file_name().to_str()?.parse().ok()?;
        // Processes of other users are unreadable and cannot use this tree.
        let cwd = fs::read_link(entry.path().join("cwd")).ok()?;
        cwd.starts_with(&path).then_some(pid)
    })
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {} failed: {}", args[0], stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Err when the checkout has uncommitted or untracked files or unpushed commits.
fn check_checkout(dir: &Path, label: &str) -> Result<(), String> {
    let status = git_output(dir, &["status", "--porcelain", "--untracked-files=all"])?;
    let changed = status.lines().count();
    if changed > 0 {
        return Err(format!("{label} has {changed} modified or untracked paths"));
    }
    let count = git_output(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"])?;
    if count.trim() != "0" {
        return Err(format!(
            "{label} has {} commits on no remote branch",
            count.trim()
        ));
    }
    Ok(())
}

/// Nested dependency checkouts: `_repo/<name>` entries with their own `.git`.
fn nested_checkouts(path: &Path) -> Vec<PathBuf> {
    sorted_entries(&path.join("_repo"))
        .unwrap_or_default()
        .iter()
        .map(|e| e.path())
        .filter(|p| fs::symlink_metadata(p.join(".git")).is_ok())
        .collect()
}

/// The first path in the tree not owned by `uid`; these cannot be deleted.
fn foreign_path(path: &Path, uid: u32) -> std::io::Result<Option<PathBuf>> {
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.uid() != uid {
                return Ok(Some(entry.path()));
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(None)
}

fn owned(path: &Path) -> Result<(), String> {
    match foreign_path(path, euid()) {
        Ok(None) => Ok(()),
        Ok(Some(found)) => Err(format!("{} is owned by another user", found.display())),
        Err(e) => Err(format!("cannot inspect tree: {e}")),
    }
}

/// The task id a scratch entry name maps to: `task-<id>*`, `task<id>*`,
/// `t<id>*`, or `<id>-<non-digit>*` (so dates like `2026-11-12` are not ids).
fn scratch_task(name: &str) -> Option<u64> {
    fn digits(s: &str) -> (&str, &str) {
        let end = s
            .bytes()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(s.len());
        s.split_at(end)
    }
    for prefix in ["task-", "task", "t"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let (id, _) = digits(rest);
            if !id.is_empty() {
                return parse_id(id);
            }
        }
    }
    let (id, rest) = digits(name);
    let rest = rest.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    parse_id(id)
}

/// `pattern` with `*` matching any run of characters.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            let Some(rest) = name.strip_prefix(head) else {
                return false;
            };
            (0..=rest.len())
                .filter(|&i| rest.is_char_boundary(i))
                .any(|i| glob(tail, &rest[i..]))
        }
    }
}

/// What one walk of a tree (symlinks not followed) finds.
#[derive(Debug, Default, PartialEq)]
struct TreeStats {
    /// Allocated bytes.
    bytes: u64,
    /// Newest modification time of any entry, the root included.
    modified: i64,
    /// Newest access time of a regular file.
    read: i64,
    /// The first entry not owned by the walking user.
    foreign: Option<PathBuf>,
}

impl TreeStats {
    /// Seconds since last use at `now`; `reads` counts file reads as use.
    fn unused(&self, now: i64, reads: bool) -> i64 {
        let last = if reads {
            self.modified.max(self.read)
        } else {
            self.modified
        };
        (now - last).max(0)
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn tree_stats(path: &Path) -> std::io::Result<TreeStats> {
    let uid = euid();
    let meta = fs::symlink_metadata(path)?;
    let mut stats = TreeStats {
        bytes: meta.blocks() * 512,
        modified: meta.mtime(),
        read: if meta.is_file() { meta.atime() } else { 0 },
        foreign: (meta.uid() != uid).then(|| path.to_path_buf()),
    };
    if !meta.is_dir() {
        return Ok(stats);
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A directory we cannot list (mode 000 or not ours) blocks removal.
            Err(_) if stats.foreign.is_some() => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            let meta = entry.metadata()?;
            stats.bytes += meta.blocks() * 512;
            stats.modified = stats.modified.max(meta.mtime());
            if meta.is_file() {
                stats.read = stats.read.max(meta.atime());
            }
            if meta.uid() != uid {
                stats.foreign.get_or_insert_with(|| entry.path());
            } else if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(stats)
}

/// Stats of a tree, Err when another user owns part of it.
fn inspect(path: &Path) -> Result<TreeStats, String> {
    let stats = tree_stats(path).map_err(|e| format!("cannot inspect tree: {e}"))?;
    if let Some(found) = &stats.foreign {
        return Err(format!("{} is owned by another user", found.display()));
    }
    Ok(stats)
}

/// Removes `path`: a file or symlink, or a tree after giving each directory
/// `u+rwx` (Go module caches are read-only). Refuses a tree with an entry
/// owned by another user.
fn remove_tree(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_dir() {
        return fs::remove_file(path).with_context(|| format!("remove {}", path.display()));
    }
    if let Some(found) = tree_stats(path)?.foreign {
        bail!("{} is owned by another user", found.display());
    }
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mode = fs::symlink_metadata(&dir)?.permissions().mode();
        if mode & 0o700 != 0o700 {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode | 0o700))
                .with_context(|| format!("chmod u+rwx {}", dir.display()))?;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
}

/// Paths in use: every readable process's working directory and open files,
/// and every bound unix socket (a live tmux or ssh-agent listens on one).
/// Processes of other users are unreadable and cannot use our trees.
#[derive(Default)]
struct OpenPaths {
    paths: Vec<PathBuf>,
    taken: Option<Instant>,
}

impl OpenPaths {
    fn scan() -> OpenPaths {
        let mut paths = Vec::new();
        for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
            if entry
                .file_name()
                .to_str()
                .and_then(|p| p.parse::<u32>().ok())
                .is_none()
            {
                continue;
            }
            let proc = entry.path();
            paths.extend(fs::read_link(proc.join("cwd")));
            for fd in fs::read_dir(proc.join("fd"))
                .into_iter()
                .flatten()
                .flatten()
            {
                if let Ok(target) = fs::read_link(fd.path()) {
                    if target.is_absolute() {
                        paths.push(target);
                    }
                }
            }
        }
        // Path is the 8th field; abstract sockets start with `@`.
        let sockets = fs::read_to_string("/proc/net/unix").unwrap_or_default();
        paths.extend(
            sockets
                .lines()
                .skip(1)
                .filter_map(|l| l.split_whitespace().nth(7))
                .filter(|p| p.starts_with('/'))
                .map(PathBuf::from),
        );
        paths.sort();
        paths.dedup();
        OpenPaths {
            paths,
            taken: Some(Instant::now()),
        }
    }

    /// The first open path inside `path`.
    fn inside(&self, path: &Path) -> Option<&Path> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.paths
            .iter()
            .find(|p| p.starts_with(&path))
            .map(PathBuf::as_path)
    }
}

/// Err when removing the candidate could lose work or disturb a live run.
fn safety_check(path: &Path, events: &[Event]) -> Result<(), String> {
    if run_is_live(events) {
        return Err("task has a live run".into());
    }
    if let Some(pid) = process_inside(path) {
        return Err(format!("process {pid} is running inside"));
    }
    if common_dir(path).is_none() {
        return Err("not a registered git worktree".into());
    }
    check_checkout(path, "worktree")?;
    for nested in nested_checkouts(path) {
        let name = nested.file_name().unwrap_or_default().to_string_lossy();
        check_checkout(&nested, &format!("_repo/{name}"))?;
    }
    owned(path)
}

/// The owning repository's common git dir, from the worktree's `.git` file.
fn common_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    if !fs::symlink_metadata(&dot_git).ok()?.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let gitdir = worktree.join(text.trim().strip_prefix("gitdir:")?.trim());
    let common = match fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => gitdir.join(rel.trim()),
        Err(_) => gitdir.parent()?.parent()?.to_path_buf(),
    };
    let common = common.canonicalize().ok()?;
    let inside = worktree
        .canonicalize()
        .map_or(true, |w| common.starts_with(w));
    (!inside).then_some(common)
}

fn git_in(common: &Path, args: &[&str], path: Option<&Path>) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(common).args(args);
    if let Some(path) = path {
        command.arg(path);
    }
    let out = command.output().context("run git")?;
    ensure!(
        out.status.success(),
        "git {} in {} failed: {}",
        args.join(" "),
        common.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// `git worktree remove` without force, so git itself refuses a dirty tree:
/// nested dependency worktrees first, then the worktree, then prune each owner.
fn remove(path: &Path) -> Result<String> {
    let mut worktrees: Vec<(PathBuf, PathBuf)> = nested_checkouts(path)
        .into_iter()
        .filter_map(|p| common_dir(&p).map(|common| (p, common)))
        .collect();
    let own = common_dir(path).context("not a registered git worktree")?;
    worktrees.push((path.to_path_buf(), own));
    for (worktree, common) in &worktrees {
        git_in(common, &["worktree", "remove"], Some(worktree))?;
    }
    let mut owners: Vec<PathBuf> = worktrees.into_iter().map(|(_, c)| c).collect();
    owners.sort();
    owners.dedup();
    for common in &owners {
        git_in(common, &["worktree", "prune"], None)?;
    }
    let names: Vec<String> = owners.iter().map(|c| c.display().to_string()).collect();
    Ok(format!("removed; pruned {}", names.join(", ")))
}

fn under_pressure(free: u64, below_gib: u64) -> bool {
    free < below_gib.saturating_mul(1 << 30)
}

/// Whether `last` is unset or at least `interval` ago.
fn due(last: Option<Instant>, interval: Duration) -> bool {
    last.is_none_or(|t| t.elapsed() >= interval)
}

/// One step of a disk-pause episode, `(started_at, alerted)`: the next state
/// and whether to alert now.
fn alert_step(
    state: Option<(i64, bool)>,
    paused: bool,
    now: i64,
    after: i64,
) -> (Option<(i64, bool)>, bool) {
    if !paused {
        return (None, false);
    }
    let (since, alerted) = state.unwrap_or((now, false));
    (Some((since, alerted)), !alerted && now - since >= after)
}

fn read_episode(path: &Path) -> Option<(i64, bool)> {
    let text = fs::read_to_string(path).ok()?;
    let (since, alerted) = text.trim().split_once(' ')?;
    Some((since.parse().ok()?, alerted == "alerted"))
}

fn write_episode(path: &Path, since: i64, alerted: bool) -> Result<()> {
    let flag = if alerted { "alerted" } else { "waiting" };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    fs::write(path, format!("{since} {flag}\n"))
        .with_context(|| format!("write {}", path.display()))
}

/// Whether admission is paused on disk, from an admission view
/// (`admission::view` or `GET /api/admission`); `None` when not known.
pub fn paused_on_disk(view: &Value) -> Option<bool> {
    // An enabled gate that has not sampled yet says nothing.
    if view["enabled"] == true && view["checked_at"].is_null() {
        return None;
    }
    let disk = view["signals"].as_array().is_some_and(|s| {
        s.iter()
            .any(|s| s["name"] == "disk" && s["level"] == "constrained")
    });
    view["admitting"]
        .as_bool()
        .map(|admitting| !admitting && disk)
}

fn trim_cache(argv: &[String]) -> Result<()> {
    let (bin, args) = argv.split_first().context("empty cache_trim command")?;
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("run {bin}"))?;
    ensure!(
        out.status.success(),
        "{bin} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Default file for the disk-pause episode, next to the admission hold.
pub fn episode_path() -> PathBuf {
    admission::hold_path().with_file_name("housekeeping-disk-pause")
}

/// The result of one tick: decision and event lines, and whether none failed.
#[derive(Debug, Default)]
pub struct Tick {
    pub ok: bool,
    pub lines: Vec<Value>,
}

/// Housekeeping state across ticks.
pub struct Keeper {
    rules: Rules,
    client: Client,
    apply: bool,
    /// A one-off sweep (`axe housekeeping`): no hourly cadence.
    one_off: bool,
    last_sweep: Option<Instant>,
    last_trim: Option<Instant>,
    episode: PathBuf,
    /// Task rows looked up during this tick; re-checks bypass it.
    tasks: RefCell<HashMap<u64, Result<TaskRow, String>>>,
    task_events: RefCell<HashMap<u64, Result<Vec<Event>, String>>>,
    open: RefCell<OpenPaths>,
}

impl Keeper {
    /// `client` reads tasks and, in apply mode, sends alert wakes.
    pub fn new(rules: Rules, client: Client, apply: bool) -> Keeper {
        Keeper {
            rules,
            client,
            apply,
            one_off: false,
            last_sweep: None,
            last_trim: None,
            episode: episode_path(),
            tasks: RefCell::default(),
            task_events: RefCell::default(),
            open: RefCell::default(),
        }
    }

    /// Sweeps on every tick instead of hourly while there is no pressure.
    pub fn one_off(mut self) -> Keeper {
        self.one_off = true;
        self
    }

    /// Keeps the disk-pause episode in `path` instead of [`episode_path`].
    pub fn with_episode(mut self, path: PathBuf) -> Keeper {
        self.episode = path;
        self
    }

    fn task(&self, id: u64) -> Result<TaskRow, String> {
        let task: TaskRow = self
            .client
            .get(&format!("/api/tasks/{id}"), &[])
            .map_err(|e| format!("task lookup failed: {e:#}"))?;
        if task.id != id {
            return Err(format!("server returned task {}", task.id));
        }
        Ok(task)
    }

    /// The task row, cached for this tick unless `fresh`.
    fn task_row(&self, id: u64, fresh: bool) -> Result<TaskRow, String> {
        if !fresh {
            if let Some(found) = self.tasks.borrow().get(&id) {
                return found.clone();
            }
        }
        let found = self.task(id);
        self.tasks.borrow_mut().insert(id, found.clone());
        found
    }

    /// The task's events, cached for this tick unless `fresh`.
    fn events(&self, id: u64, fresh: bool) -> Result<Vec<Event>, String> {
        if !fresh {
            if let Some(found) = self.task_events.borrow().get(&id) {
                return found.clone();
            }
        }
        let found: Result<Vec<Event>, String> = self
            .client
            .get(&format!("/api/tasks/{id}/events"), &[])
            .map_err(|e| format!("events lookup failed: {e:#}"));
        self.task_events.borrow_mut().insert(id, found.clone());
        found
    }

    /// An open path inside `path`, from a process scan of this tick, or of
    /// the last second when `fresh`.
    fn in_use(&self, path: &Path, fresh: bool) -> Result<(), String> {
        let mut open = self.open.borrow_mut();
        let max_age = if fresh { Duration::from_secs(1) } else { TICK };
        if open.taken.is_none_or(|t| t.elapsed() >= max_age) {
            *open = OpenPaths::scan();
        }
        match open.inside(path) {
            Some(p) => Err(format!("{} is in use by a process", p.display())),
            None => Ok(()),
        }
    }

    /// Runs `check` (cached lookups), and in apply mode again with fresh
    /// lookups right before `remove`.
    fn act(
        &self,
        check: impl Fn(bool) -> Result<String, String>,
        remove: impl FnOnce() -> Result<()>,
    ) -> (&'static str, String) {
        match check(false) {
            Err(reason) => ("retain", reason),
            Ok(reason) if !self.apply => ("would_delete", reason),
            Ok(_) => match check(true) {
                Err(reason) => ("retain", format!("re-check: {reason}")),
                Ok(reason) => match remove() {
                    Ok(()) => ("deleted", reason),
                    Err(e) => ("error", format!("{e:#}")),
                },
            },
        }
    }

    /// Stats of a tree we may remove: Err when another user owns part of it,
    /// a socket lives in it or a process uses it.
    fn removable(&self, path: &Path, fresh: bool) -> Result<TreeStats, String> {
        let stats = inspect(path)?;
        self.in_use(path, fresh)?;
        Ok(stats)
    }

    /// Rule A: scratch named for `id`. A done task's goes after `retention`;
    /// a blocked or escalated task's once idle and at least the size floor.
    fn evaluate_scratch(
        &self,
        tmp: &Tmp,
        path: &Path,
        id: u64,
        retention: u64,
        fresh: bool,
    ) -> Result<String, String> {
        let task = self.task_row(id, fresh)?;
        let held = if task.state == "done" {
            None
        } else {
            Some(task.idle_for(tmp.idle, unix_now())?)
        };
        let events = self.events(id, fresh)?;
        if run_is_live(&events) {
            return Err("task has a live run".into());
        }
        let is_held = held.is_some();
        let reason = match held {
            None => {
                let age = done_age(&events, unix_now())?;
                if age < retention as i64 {
                    return Err(format!("done {age}s ago, retention {retention}s"));
                }
                format!("done {age}s ago")
            }
            Some(reason) => reason,
        };
        let stats = self.removable(path, fresh)?;
        if is_held && stats.bytes < tmp.floor_bytes {
            return Err(format!(
                "{reason}; {} bytes, below the floor of {}",
                stats.bytes, tmp.floor_bytes
            ));
        }
        Ok(format!(
            "{reason}, project {:?}, {} bytes",
            task.project, stats.bytes
        ))
    }

    /// Rule B: a directory no task claims, unused for `stale`.
    fn evaluate_stale(&self, tmp: &Tmp, path: &Path, fresh: bool) -> Result<String, String> {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let shared = tmp.shared.iter().any(|p| glob(p, &name));
        let stats = tree_stats(path).map_err(|e| format!("cannot inspect tree: {e}"))?;
        let unused = stats.unused(unix_now(), shared);
        if unused < tmp.stale as i64 {
            return Err(format!("used {unused}s ago, stale {}s", tmp.stale));
        }
        let stats = self.removable(path, fresh)?;
        let kind = if shared { "shared cache" } else { "scratch" };
        Ok(format!("{kind} unused {unused}s, {} bytes", stats.bytes))
    }

    /// Rules A and B over the entries of `tmp.dir` this user owns.
    fn sweep_tmp(
        &self,
        tmp: &Tmp,
        retention: u64,
        under: bool,
        stop: &AtomicBool,
        out: &mut Vec<Value>,
    ) -> bool {
        let entries = match sorted_entries(&tmp.dir) {
            Ok(entries) => entries,
            Err(e) => {
                out.push(rule_line(
                    "tmp",
                    &tmp.dir,
                    None,
                    "error",
                    &format!("read: {e}"),
                ));
                return false;
            }
        };
        let uid = euid();
        let mut ok = true;
        let mut waiting = 0;
        for entry in entries {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let path = entry.path();
            // Other users' entries (root's sockets, systemd) are not ours.
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.uid() != uid || meta.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = scratch_task(&name) {
                let (decision, reason) = self.act(
                    |fresh| self.evaluate_scratch(tmp, &path, id, retention, fresh),
                    || remove_tree(&path),
                );
                ok &= decision != "error";
                out.push(rule_line("tmp_task", &path, Some(id), decision, &reason));
                continue;
            }
            if !meta.is_dir() {
                continue;
            }
            // Recently used directories say nothing; only stale ones get a line.
            let first = self.evaluate_stale(tmp, &path, false);
            if first.as_ref().is_err_and(|r| r.starts_with("used ")) {
                continue;
            }
            let (decision, reason) = if under {
                self.act(
                    |fresh| self.evaluate_stale(tmp, &path, fresh),
                    || remove_tree(&path),
                )
            } else if !self.one_off {
                // The dispatcher counts them instead of a line each hour.
                waiting += 1;
                continue;
            } else {
                match first {
                    Ok(r) => ("retain", format!("not under pressure; would go: {r}")),
                    Err(r) => ("retain", format!("not under pressure; {r}")),
                }
            };
            ok &= decision != "error";
            out.push(rule_line("tmp_stale", &path, None, decision, &reason));
        }
        if waiting > 0 {
            out.push(json!({"event": "tmp_stale", "under_pressure": false, "stale_dirs": waiting}));
        }
        ok
    }

    /// Rule C check for one build-output directory of task `id`'s worktree.
    fn evaluate_build(
        &self,
        bo: &BuildOutput,
        project: &str,
        id: u64,
        dir: &Path,
        fresh: bool,
    ) -> Result<String, String> {
        let task = self.task_row(id, fresh)?;
        if task.project != project {
            return Err(format!("task belongs to project {:?}", task.project));
        }
        let reason = task.idle_for(bo.idle, unix_now())?;
        if run_is_live(&self.events(id, fresh)?) {
            return Err("task has a live run".into());
        }
        git_ignored_untracked(dir)?;
        let stats = self.removable(dir, fresh)?;
        Ok(format!("{reason}, git-ignored, {} bytes", stats.bytes))
    }

    /// Rule C: build output in an idle task's worktree, after its own decision
    /// retained it. Silent unless the task is idle and output exists.
    fn sweep_build_output(
        &self,
        bo: &BuildOutput,
        project: &str,
        c: &Candidate,
        stop: &AtomicBool,
        out: &mut Vec<Value>,
    ) -> bool {
        let idle = self
            .task_row(c.task, false)
            .is_ok_and(|t| t.project == project && t.idle_for(bo.idle, unix_now()).is_ok());
        if !idle {
            return true;
        }
        let mut ok = true;
        for dir in build_dirs(&c.path, &bo.names) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let (decision, reason) = self.act(
                |fresh| self.evaluate_build(bo, project, c.task, &dir, fresh),
                || remove_tree(&dir),
            );
            ok &= decision != "error";
            let mut l = rule_line("build_output", &dir, Some(c.task), decision, &reason);
            l["project"] = json!(project);
            out.push(l);
        }
        ok
    }

    /// Rule D check: an entry of a cache root, unused for `unused`.
    fn evaluate_orphan(&self, o: &Orphans, path: &Path, fresh: bool) -> Result<String, String> {
        let stats = tree_stats(path).map_err(|e| format!("cannot inspect tree: {e}"))?;
        let unused = stats.unused(unix_now(), true);
        if unused < o.unused as i64 {
            return Err(format!("used {unused}s ago, unused {}s", o.unused));
        }
        let stats = self.removable(path, fresh)?;
        Ok(format!("unused {unused}s, {} bytes", stats.bytes))
    }

    /// Rule D: cache-root entries that no `task_paths` template claims.
    fn sweep_orphans(&self, o: &Orphans, stop: &AtomicBool, out: &mut Vec<Value>) -> bool {
        let uid = euid();
        let mut ok = true;
        for root in &o.roots {
            let entries = match sorted_entries(root) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    out.push(rule_line(
                        "orphan_cache",
                        root,
                        None,
                        "error",
                        &format!("read: {e}"),
                    ));
                    ok = false;
                    continue;
                }
            };
            for entry in entries {
                if stop.load(Ordering::SeqCst) {
                    return ok;
                }
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let claimed = self
                    .rules
                    .task_paths
                    .iter()
                    .any(|t| t.dir == *root && t.task(&name).is_some());
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if claimed || meta.uid() != uid || meta.file_type().is_symlink() {
                    continue;
                }
                let (decision, reason) = self.act(
                    |fresh| self.evaluate_orphan(o, &path, fresh),
                    || remove_tree(&path),
                );
                ok &= decision != "error";
                out.push(rule_line("orphan_cache", &path, None, decision, &reason));
            }
        }
        ok
    }

    /// The done task's events and done age, Err unless done for `retention`.
    fn done_for(&self, task: &TaskRow, retention: u64) -> Result<(Vec<Event>, i64), String> {
        if task.state != "done" {
            return Err(format!("task state is {:?}", task.state));
        }
        let events: Vec<Event> = self
            .client
            .get(&format!("/api/tasks/{}/events", task.id), &[])
            .map_err(|e| format!("events lookup failed: {e:#}"))?;
        let age = done_age(&events, unix_now())?;
        if age < retention as i64 {
            return Err(format!("done {age}s ago, retention {retention}s"));
        }
        Ok((events, age))
    }

    /// `Ok(reason)` when the worktree may be deleted, `Err(reason)` to retain it.
    fn evaluate(&self, project: &str, c: &Candidate, retention: u64) -> Result<String, String> {
        let meta = fs::symlink_metadata(&c.path).map_err(|e| format!("stat failed: {e}"))?;
        if !meta.is_dir() {
            return Err("not a real directory".into());
        }
        let task = self.task(c.task)?;
        if task.project != project {
            return Err(format!("task belongs to project {:?}", task.project));
        }
        let (events, age) = self.done_for(&task, retention)?;
        safety_check(&c.path, &events)?;
        Ok(format!("done {age}s ago"))
    }

    /// Like `evaluate` for a task path: no git or project checks, since it
    /// holds only rebuildable output and task ids are global.
    fn evaluate_path(&self, c: &Candidate, retention: u64) -> Result<String, String> {
        let meta = fs::symlink_metadata(&c.path).map_err(|e| format!("stat failed: {e}"))?;
        if !meta.is_dir() {
            return Err("not a real directory".into());
        }
        let task = self.task(c.task)?;
        let (events, age) = self.done_for(&task, retention)?;
        if run_is_live(&events) {
            return Err("task has a live run".into());
        }
        if let Some(pid) = process_inside(&c.path) {
            return Err(format!("process {pid} is running inside"));
        }
        owned(&c.path)?;
        Ok(format!("done {age}s ago, project {:?}", task.project))
    }

    fn decide(&self, project: &str, c: &Candidate, retention: u64) -> (&'static str, String) {
        match self.evaluate(project, c, retention) {
            Err(reason) => ("retain", reason),
            Ok(reason) if !self.apply => ("would_delete", reason),
            // Re-check right before deleting.
            Ok(_) => match self.evaluate(project, c, retention) {
                Err(reason) => ("retain", format!("re-check: {reason}")),
                Ok(reason) => match remove(&c.path) {
                    Ok(done) => ("deleted", format!("{reason}; {done}")),
                    Err(e) => ("error", format!("{e:#}")),
                },
            },
        }
    }

    fn decide_path(&self, c: &Candidate, retention: u64) -> (&'static str, String) {
        match self.evaluate_path(c, retention) {
            Err(reason) => ("retain", reason),
            Ok(reason) if !self.apply => ("would_delete", reason),
            Ok(_) => match self.evaluate_path(c, retention) {
                Err(reason) => ("retain", format!("re-check: {reason}")),
                Ok(reason) => match fs::remove_dir_all(&c.path) {
                    Ok(()) => ("deleted", format!("{reason}; removed")),
                    Err(e) => ("error", format!("remove: {e}")),
                },
            },
        }
    }

    /// A decision line for each task-path entry of `dir`: entries matching one
    /// of `paths`, and entries starting with one of their prefixes but
    /// matching none (retained and reported with a null task).
    fn sweep_paths(
        &self,
        dir: &Path,
        paths: &[&TaskPath],
        retention: u64,
        stop: &AtomicBool,
        out: &mut Vec<Value>,
    ) -> Result<bool> {
        let entries = match sorted_entries(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        };
        let mut ok = true;
        for entry in entries {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !paths.iter().any(|p| name.starts_with(&p.prefix)) {
                continue;
            }
            let path = entry.path();
            let Some(task) = paths.iter().find_map(|p| p.task(&name)) else {
                // An orphan-cache root reports these under its own rule.
                if self
                    .rules
                    .orphans
                    .as_ref()
                    .is_some_and(|o| o.roots.iter().any(|r| r == dir))
                {
                    continue;
                }
                let expected: Vec<&str> = paths.iter().map(|p| p.template.as_str()).collect();
                let reason = format!("no task id: expected {}", expected.join(" or "));
                out.push(path_line(&path, None, "retain", &reason));
                continue;
            };
            let c = Candidate { path, task };
            let (decision, reason) = self.decide_path(&c, retention);
            ok &= decision != "error";
            out.push(path_line(&c.path, Some(task), decision, &reason));
        }
        Ok(ok)
    }

    /// Advances the episode file and wakes the bot once per episode. A failed
    /// wake is retried by the next tick; the key makes that idempotent.
    fn check_alert(&self, alert: &Alert, paused: bool, now: i64) -> Result<&'static str> {
        let (next, fire) = alert_step(read_episode(&self.episode), paused, now, alert.after);
        let Some((since, alerted)) = next else {
            match fs::remove_file(&self.episode) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok("clear"),
            }
        };
        if !fire {
            write_episode(&self.episode, since, alerted)?;
            return Ok(if alerted { "alerted" } else { "waiting" });
        }
        write_episode(&self.episode, since, false)?;
        let minutes = (now - since) / 60;
        let note = format!(
            "Housekeeping: task admission has been paused on disk for about {minutes} min. \
             Short retention and cache trim are already running; see the axe dispatch journal."
        );
        let _: Value = self.client.send(
            "POST",
            &format!("/api/bots/{}/wakes", alert.bot),
            json!({"now": true, "note": note, "key": format!("housekeeping-disk-pause-{since}")}),
        )?;
        write_episode(&self.episode, since, true)?;
        Ok("fired")
    }

    /// One tick. `admission` is the dispatcher's admission view, for the
    /// disk-pause alert; `None` skips the alert. Stops early once `stop` is set.
    pub fn tick(&mut self, admission: Option<&Value>, stop: &AtomicBool) -> Tick {
        let mut out = Tick {
            ok: true,
            lines: Vec::new(),
        };
        let rules = self.rules.clone();
        self.tasks.borrow_mut().clear();
        self.task_events.borrow_mut().clear();
        *self.open.borrow_mut() = OpenPaths::default();
        let mut under = false;
        let mut retention = rules.retention;
        // Task paths are rebuildable: under pressure they go as soon as the task is done.
        let mut path_retention = rules.retention;
        let mut sweep = true;
        if let Some(p) = &rules.pressure {
            match admission::LinuxProbe.disk_available(&p.path) {
                Err(e) => {
                    out.ok = false;
                    out.lines
                        .push(json!({"event": "pressure", "error": format!("{e:#}")}));
                }
                Ok(free) => {
                    under = under_pressure(free, p.free_below_gib);
                    if under {
                        retention = p.retention;
                        path_retention = 0;
                    } else if !self.one_off {
                        sweep = due(self.last_sweep, HOURLY);
                    }
                    out.lines.push(json!({"event": "pressure", "free_bytes": free, "under_pressure": under, "retention_seconds": retention, "sweep": sweep}));
                    if under
                        && !p.cache_trim.is_empty()
                        && (self.one_off || due(self.last_trim, HOURLY))
                    {
                        self.last_trim = Some(Instant::now());
                        for argv in &p.cache_trim {
                            let result = if self.apply { trim_cache(argv) } else { Ok(()) };
                            out.ok &= result.is_ok();
                            let outcome = match (&result, self.apply) {
                                (Err(e), _) => format!("error: {e:#}"),
                                (Ok(()), true) => "trimmed".to_string(),
                                (Ok(()), false) => "would_run".to_string(),
                            };
                            out.lines.push(
                                json!({"event": "cache_trim", "command": argv, "result": outcome}),
                            );
                        }
                    }
                }
            }
        }
        if let (Some(alert), Some(view), true) = (&rules.alert, admission, self.apply) {
            // An unknown status says nothing: the episode file stays.
            let outcome = match paused_on_disk(view) {
                Some(paused) => self.check_alert(alert, paused, unix_now()),
                None => Ok("unknown"),
            }
            .map_err(|e| format!("{e:#}"));
            out.ok &= outcome.is_ok();
            out.lines
                .push(json!({"event": "disk_pause_alert", "result": format!("{outcome:?}")}));
        }
        if !sweep {
            return out;
        }
        if rules.pressure.is_some() {
            self.last_sweep = Some(Instant::now());
        }
        match roots(&rules.worktrees) {
            Err(e) => {
                out.ok = false;
                out.lines
                    .push(line(&rules.worktrees, "", None, "error", &format!("{e:#}")));
            }
            Ok(roots) => {
                for (root, project) in roots {
                    match candidates(&root) {
                        Err(e) => {
                            out.ok = false;
                            out.lines
                                .push(line(&root, &project, None, "error", &format!("{e:#}")));
                        }
                        Ok(found) => {
                            for c in found {
                                if stop.load(Ordering::SeqCst) {
                                    return out;
                                }
                                let (decision, reason) = self.decide(&project, &c, retention);
                                out.ok &= decision != "error";
                                out.lines.push(line(
                                    &c.path,
                                    &project,
                                    Some(c.task),
                                    decision,
                                    &reason,
                                ));
                                if let (Some(bo), "retain") = (&rules.build_output, decision) {
                                    out.ok &= self.sweep_build_output(
                                        bo,
                                        &project,
                                        &c,
                                        stop,
                                        &mut out.lines,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut dirs: Vec<&Path> = rules.task_paths.iter().map(|p| p.dir.as_path()).collect();
        dirs.sort();
        dirs.dedup();
        for dir in dirs {
            let paths: Vec<&TaskPath> = rules.task_paths.iter().filter(|p| p.dir == dir).collect();
            match self.sweep_paths(dir, &paths, path_retention, stop, &mut out.lines) {
                Err(e) => {
                    out.ok = false;
                    out.lines
                        .push(path_line(dir, None, "error", &format!("{e:#}")));
                }
                Ok(ok) => out.ok &= ok,
            }
        }
        if let Some(tmp) = &rules.tmp {
            out.ok &= self.sweep_tmp(tmp, path_retention, under, stop, &mut out.lines);
        }
        if let Some(o) = &rules.orphans {
            out.ok &= self.sweep_orphans(o, stop, &mut out.lines);
        }
        out
    }
}

/// Directories named one of `names` under `root` (not `root` itself), not
/// descending into them, `.git` or symlinks.
fn build_dirs(root: &Path, names: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in sorted_entries(&dir).unwrap_or_default() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if !kind.is_dir() || name == ".git" {
                continue;
            }
            if names.iter().any(|n| name == n.as_str()) {
                out.push(entry.path());
            } else {
                stack.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Ok when git ignores `dir` and tracks nothing inside it.
fn git_ignored_untracked(dir: &Path) -> Result<(), String> {
    let parent = dir.parent().ok_or("no parent")?;
    let name = dir.file_name().ok_or("no name")?;
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(parent)
        .args(["check-ignore", "-q", "--"])
        .arg(name)
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    match out.status.code() {
        Some(0) => {}
        Some(1) => return Err("not ignored by git".into()),
        _ => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("git check-ignore failed: {}", stderr.trim()));
        }
    }
    let mut parent_name = PathBuf::from(name);
    parent_name.push("");
    let tracked = git_output(parent, &["ls-files", "--", &parent_name.to_string_lossy()])?;
    if let Some(first) = tracked.lines().next() {
        return Err(format!("git tracks {first}"));
    }
    Ok(())
}

fn rule_line(kind: &str, path: &Path, task: Option<u64>, decision: &str, reason: &str) -> Value {
    json!({"kind": kind, "path": path, "task": task, "decision": decision, "reason": reason})
}

fn line(path: &Path, project: &str, task: Option<u64>, decision: &str, reason: &str) -> Value {
    json!({"path": path, "project": project, "task": task, "decision": decision, "reason": reason})
}

fn path_line(path: &Path, task: Option<u64>, decision: &str, reason: &str) -> Value {
    json!({"kind": "cache", "path": path, "task": task, "decision": decision, "reason": reason})
}

fn print(tick: &Tick) {
    for l in &tick.lines {
        println!("{l}");
    }
}

/// `axe housekeeping --dry-run`: one sweep that prints every decision.
/// Returns whether no step failed.
pub fn once(cfg: &Config, client: Client) -> Result<bool> {
    let rules = Rules::new(cfg)?;
    let mut keeper = Keeper::new(rules, client, false).one_off();
    let tick = keeper.tick(None, &AtomicBool::new(false));
    print(&tick);
    Ok(tick.ok)
}

/// Starts the dispatcher's housekeeping thread, or nothing when `mode` is
/// off. It ticks every [`TICK`] until `stop` is set.
pub fn start(
    cfg: &Config,
    mode: Housekeep,
    client: Client,
    admission: admission::Handle,
    stop: Arc<AtomicBool>,
) -> Result<Option<JoinHandle<()>>> {
    let apply = match mode {
        Housekeep::Off => {
            eprintln!("axe dispatcher: housekeeping off");
            return Ok(None);
        }
        Housekeep::DryRun => false,
        Housekeep::Apply => true,
    };
    let rules = Rules::new(cfg)?;
    eprintln!(
        "axe dispatcher: housekeeping {} every {}s, retention {}s, roots under {}",
        if apply { "apply" } else { "dry-run" },
        TICK.as_secs(),
        rules.retention,
        rules.worktrees.display()
    );
    let mut keeper = Keeper::new(rules, client, apply);
    let handle = std::thread::Builder::new()
        .name("housekeeping".into())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let view = admission::view(&admission);
                print(&keeper.tick(Some(&view), &stop));
                let end = Instant::now() + TICK;
                while !stop.load(Ordering::SeqCst) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        })?;
    Ok(Some(handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_names() {
        assert_eq!(task_id("task-7--work"), Some(7));
        assert_eq!(task_id("task-123--dep-repo"), Some(123));
        for name in [
            "task-7",
            "task-7--",
            "task-07--work",
            "task-0--w",
            "task--work",
            "task-x--w",
            "tasks-7--w",
            "legacy",
        ] {
            assert_eq!(task_id(name), None, "{name}");
        }
    }

    #[test]
    fn task_path_names() {
        let p = TaskPath::parse("/c/task-{id}-target", Path::new("/w/.worktrees")).unwrap();
        assert_eq!(p.task("task-508-target"), Some(508));
        for name in [
            "task-target",
            "task--target",
            "task-08-target",
            "task-0-target",
            "task-x-target",
            "task-5",
            "taskx-target",
            "task-5--work",
        ] {
            assert_eq!(p.task(name), None, "{name}");
        }
        let scratch = TaskPath::parse("/tmp/axe-task-{id}", Path::new("/w/.worktrees")).unwrap();
        assert_eq!(scratch.task("axe-task-12"), Some(12));
        assert_eq!(scratch.task("axe-task-12.x"), None);
    }

    #[test]
    fn scratch_names_map_to_tasks() {
        for (name, id) in [
            ("t757", 757),
            ("t671cache2", 671),
            ("t604-reczz28Y.eKSK", 604),
            ("t5_list.json", 5),
            ("task152-m2", 152),
            ("task-12-run", 12),
            ("task-12", 12),
            ("502-build.log", 502),
            ("502-lark", 502),
        ] {
            assert_eq!(scratch_task(name), Some(id), "{name}");
        }
        for name in [
            "2026-09-26-0020-plan.html",
            "502-",
            "502",
            "t07",
            "t0",
            "tsx-1000",
            "tmp.KGRcEZcOfj",
            "tmux-1000",
            "axe-rehearsal-574",
            "node-compile-cache",
        ] {
            assert_eq!(scratch_task(name), None, "{name}");
        }
    }

    #[test]
    fn glob_star() {
        assert!(glob("tsx-*", "tsx-1000"));
        assert!(glob("tsx-*", "tsx-"));
        assert!(glob("gh-cli-cache", "gh-cli-cache"));
        assert!(glob("*-of-*", "pytest-of-harvis"));
        assert!(!glob("tsx-*", "xtsx-1"));
        assert!(!glob("gh-cli-cache", "gh-cli-cache2"));
    }

    #[test]
    fn tree_stats_and_remove_read_only_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/f"), vec![1u8; 10000]).unwrap();
        fs::set_permissions(tree.join("a/b"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(tree.join("a"), fs::Permissions::from_mode(0o500)).unwrap();
        let stats = tree_stats(&tree).unwrap();
        assert!(stats.bytes >= 10000, "{stats:?}");
        assert_eq!(stats.foreign, None);
        assert!(stats.modified > 0 && stats.read > 0);
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        let file = tmp.path().join("f");
        fs::write(&file, "x").unwrap();
        remove_tree(&file).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn idle_needs_a_held_status_and_age() {
        let row = |state: &str, status: &str, at: &str| TaskRow {
            id: 1,
            project: "p".into(),
            state: state.into(),
            status: status.into(),
            updated_at: at.into(),
        };
        let now = OffsetDateTime::parse("2026-11-12T12:00:00Z", &Rfc3339)
            .unwrap()
            .unix_timestamp();
        let two_h = "2026-11-12T10:00:00Z";
        assert!(row("pending", "blocked", two_h).idle_for(3600, now).is_ok());
        assert!(row("blocked", "", two_h).idle_for(3600, now).is_ok());
        assert!(row("escalated", "escalated", two_h)
            .idle_for(3600, now)
            .is_ok());
        assert!(row("done", "done", two_h).idle_for(3600, now).is_ok());
        assert!(row("done", "done", two_h).idle_for(7201, now).is_err());
        assert!(row("pending", "pending", two_h).idle_for(0, now).is_err());
        assert!(row("in_progress", "in_progress", two_h)
            .idle_for(0, now)
            .is_err());
        assert!(row("blocked", "blocked", "garbage")
            .idle_for(0, now)
            .is_err());
    }

    #[test]
    fn task_path_templates_are_checked() {
        let w = Path::new("/w/.worktrees");
        for bad in [
            "/c/task-target",
            "/c/{id}-target",
            "/c/{id}/target",
            "/c/task-{id}-{id}",
            "relative/task-{id}",
            "/w/.worktrees/proj/task-{id}--work",
        ] {
            assert!(TaskPath::parse(bad, w).is_err(), "{bad}");
        }
    }

    fn event(kind: &str, to: &str, at: &str) -> Event {
        Event {
            kind: kind.into(),
            to_value: Some(to.into()),
            at: at.into(),
        }
    }

    #[test]
    fn latest_done_event_wins() {
        let now = OffsetDateTime::parse("2026-10-30T00:00:00Z", &Rfc3339)
            .unwrap()
            .unix_timestamp();
        let events = vec![
            event("state", "done", "2026-10-01T00:00:00Z"),
            event("state", "pending", "2026-10-02T00:00:00Z"),
            event("state", "done", "2026-10-29T23:00:00Z"),
            event("note", "done", "2026-10-29T23:59:00Z"),
        ];
        assert_eq!(done_age(&events, now), Ok(3600));
        assert!(done_age(&events[1..2], now).is_err());
        assert_eq!(
            done_age(&[event("state", "done", "2027-01-01T00:00:00Z")], now),
            Ok(0)
        );
    }

    #[test]
    fn live_run_is_latest_start_without_end() {
        let at = "2026-10-01T00:00:00Z";
        let start = || event("run_start", "c", at);
        let end = || event("run_end", "c", at);
        assert!(!run_is_live(&[]));
        assert!(run_is_live(&[start()]));
        assert!(!run_is_live(&[start(), end()]));
        assert!(run_is_live(&[start(), end(), start()]));
        assert!(run_is_live(&[start(), event("state", "done", at)]));
    }

    #[test]
    fn pressure_switches_strictly_below_threshold() {
        const GIB: u64 = 1 << 30;
        assert!(under_pressure(30 * GIB - 1, 30));
        assert!(!under_pressure(30 * GIB, 30), "boundary is not pressure");
        assert!(!under_pressure(30 * GIB + 1, 30));
        assert!(!under_pressure(0, 0));
    }

    #[test]
    fn alert_fires_once_per_episode() {
        // Not paused: no episode.
        assert_eq!(alert_step(None, false, 100, 900), (None, false));
        // First sighting starts the clock.
        assert_eq!(
            alert_step(None, true, 100, 900),
            (Some((100, false)), false)
        );
        // Just before and at the deadline.
        let waiting = Some((100, false));
        assert_eq!(alert_step(waiting, true, 999, 900), (waiting, false));
        assert_eq!(alert_step(waiting, true, 1000, 900), (waiting, true));
        // Already alerted stays quiet however long it lasts.
        let alerted = Some((100, true));
        assert_eq!(alert_step(alerted, true, 99_999, 900), (alerted, false));
        // Clearing ends the episode; the next pause starts over.
        assert_eq!(alert_step(alerted, false, 2000, 900), (None, false));
        assert_eq!(
            alert_step(None, true, 3000, 900),
            (Some((3000, false)), false)
        );
    }

    #[test]
    fn missing_or_old_stamp_is_due() {
        assert!(due(None, HOURLY));
        assert!(!due(Some(Instant::now()), HOURLY));
        assert!(due(Some(Instant::now()), Duration::ZERO));
    }

    #[test]
    fn disk_pause_from_admission_view() {
        let view = |admitting: Value, disk: &str, checked: Value| {
            json!({"enabled": true, "admitting": admitting, "checked_at": checked,
                   "signals": [{"name": "disk", "level": disk}, {"name": "memory", "level": "clear"}]})
        };
        let at = json!("2026-11-10T00:00:00Z");
        assert_eq!(
            paused_on_disk(&view(json!(false), "constrained", at.clone())),
            Some(true)
        );
        assert_eq!(
            paused_on_disk(&view(json!(false), "clear", at.clone())),
            Some(false)
        );
        assert_eq!(
            paused_on_disk(&view(json!(true), "clear", at.clone())),
            Some(false)
        );
        assert_eq!(paused_on_disk(&view(Value::Null, "constrained", at)), None);
        // Not sampled yet: unknown, so a restart does not end an episode.
        assert_eq!(
            paused_on_disk(&view(json!(false), "constrained", Value::Null)),
            None
        );
        // No [resources]: admitting, never a disk pause.
        let off = json!({"enabled": false, "admitting": true, "checked_at": null, "signals": []});
        assert_eq!(paused_on_disk(&off), Some(false));
    }

    #[test]
    fn foreign_path_finds_other_owner() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/file"), "x").unwrap();
        let uid = unsafe { libc::geteuid() };
        assert_eq!(foreign_path(tmp.path(), uid).unwrap(), None);
        assert!(foreign_path(tmp.path(), uid + 1).unwrap().is_some());
    }

    #[test]
    fn common_dir_from_git_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let admin = tmp.join("repo/.git/worktrees/wt");
        fs::create_dir_all(&admin).unwrap();
        let wt = tmp.join("wt");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        assert_eq!(common_dir(&wt), Some(tmp.join("repo/.git")));
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        assert_eq!(common_dir(&wt), Some(tmp.join("repo/.git")));
        fs::write(wt.join(".git"), "garbage").unwrap();
        assert_eq!(common_dir(&wt), None);
        fs::remove_file(wt.join(".git")).unwrap();
        assert_eq!(common_dir(&wt), None);
    }

    #[test]
    fn rules_from_config() {
        let cfg = Config::parse(
            "workspace_root = \"/w\"\n[dispatcher]\nhousekeep = \"dry-run\"\n\
             [housekeeping]\ntask_paths = [\"/c/task-{id}-target\"]\n\
             [housekeeping.pressure]\nfree_below_gib = 30\ncache_trim = [[\"/bin/true\"]]\n\
             [housekeeping.alert]\nbot = \"ops\"\n",
        )
        .unwrap();
        assert_eq!(cfg.dispatcher.housekeep, Housekeep::DryRun);
        let r = Rules::new(&cfg).unwrap();
        assert_eq!(r.worktrees, PathBuf::from("/w/.worktrees"));
        assert_eq!(r.retention, 21600);
        let p = r.pressure.unwrap();
        assert_eq!((p.path, p.retention), (PathBuf::from("/w"), 3600));
        assert_eq!(r.alert.unwrap().after, 900);
        let cfg = Config::parse(
            "workspace_root = \"/w\"\n[housekeeping.build_output]\n[housekeeping.tmp]\n\
             [housekeeping.orphan_caches]\nroots = [\"/c\"]\n",
        )
        .unwrap();
        let r = Rules::new(&cfg).unwrap();
        let b = r.build_output.unwrap();
        assert_eq!(b.idle, 3600);
        assert_eq!(b.names, ["target", "node_modules", ".next", "out"]);
        let t = r.tmp.unwrap();
        assert_eq!(
            (t.dir, t.idle, t.floor_bytes, t.stale),
            (PathBuf::from("/tmp"), 3600, 1 << 30, 172800)
        );
        assert!(t.shared.iter().any(|p| p == "node-compile-cache"));
        let o = r.orphans.unwrap();
        assert_eq!((o.roots, o.unused), (vec![PathBuf::from("/c")], 604800));
        assert_eq!(Config::default().dispatcher.housekeep, Housekeep::Off);
        for bad in [
            "[dispatcher]\nhousekeep = \"yes\"\n",
            "[housekeeping]\nretention = \"soon\"\n",
            "[housekeeping]\nextra = 1\n",
            "[housekeeping]\nretention = \"1h\"\n[housekeeping.pressure]\nfree_below_gib = 1\nretention = \"2h\"\n",
            "[housekeeping.pressure]\nfree_below_gib = 1\ncache_trim = [[\"go\"]]\n",
            "[housekeeping]\ntask_paths = [\"/c/target\"]\n",
            "[housekeeping.alert]\nbot = \"\"\n",
            "[housekeeping.build_output]\nnames = []\n",
            "[housekeeping.build_output]\nnames = [\"a/b\"]\n",
            "[housekeeping.build_output]\nnames = [\".git\"]\n",
            "[housekeeping.tmp]\ndir = \"tmp\"\n",
            "workspace_root = \"/w\"\n[housekeeping.tmp]\ndir = \"/w\"\n",
            "[housekeeping.tmp]\nstale = \"soon\"\n",
            "workspace_root = \"/w\"\n[housekeeping.orphan_caches]\nroots = [\"/w/.worktrees/p\"]\n",
            "[housekeeping.orphan_caches]\nroots = [\"cache\"]\n",
            "[housekeeping.orphan_caches]\n",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }
}
