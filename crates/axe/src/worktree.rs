//! Dispatcher-owned task worktrees.
//!
//! Every session of a task runs in `<workspace_root>/.worktrees/<project>/task-<id>--work`,
//! a Git worktree of the canonical workspace clone on `<branch_prefix>task-<id>--work`
//! (`[worktrees] branch_prefix`, default `axe/`). The dispatcher sees only the
//! workspace: sub-repositories are the worker's business. Cleanup of a done
//! task's directory is left to the operator.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};

use crate::config::Config;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);

/// A project whose canonical clone can host task worktrees.
#[derive(Debug, Clone)]
pub struct Ready {
    pub canonical: PathBuf,
    pub root: PathBuf,
    /// `refs/remotes/origin/<default>`, from `origin/HEAD`.
    pub base_ref: String,
    /// Prefix of new task branches (`[worktrees] branch_prefix`).
    pub branch_prefix: String,
}

/// Checks that `project` has a canonical clone with a known default branch.
/// The error names the gap, for the escalation note.
pub fn readiness(cfg: &Config, project: &str) -> Result<Ready, String> {
    let workspace = cfg.workspace_root();
    let canonical = workspace.join(project);
    let gap = |detail: String| format!("project {project} has no task worktree base: {detail}");
    if !canonical.is_dir() {
        return Err(gap(format!("{} does not exist", canonical.display())));
    }
    match git(
        &canonical,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        Ok(common) if Path::new(&common) == canonical.join(".git") => {}
        _ => {
            return Err(gap(format!(
                "{} is not a canonical Git clone",
                canonical.display()
            )))
        }
    }
    let base_ref = git(&canonical, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .filter(|r| r.starts_with("refs/remotes/origin/"))
        .ok_or_else(|| {
            gap(format!(
                "{} has no refs/remotes/origin/HEAD (run `git remote set-head origin --auto`)",
                canonical.display()
            ))
        })?;
    Ok(Ready {
        root: workspace.join(".worktrees").join(project),
        canonical,
        base_ref,
        branch_prefix: cfg.worktrees.clone().unwrap_or_default().branch_prefix,
    })
}

/// Runs git with a deadline; never inherits redirecting Git environment.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    git_timeout(dir, args, GIT_TIMEOUT)
}

fn git_timeout(dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn git")?;
    let end = Instant::now() + timeout;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} timed out", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    ensure!(
        status.success(),
        "git {} failed: {}",
        args.join(" "),
        stderr.trim()
    );
    Ok(stdout.trim().to_string())
}

/// Creates or reuses the task's workspace worktree. An existing checkout is
/// never reset, checked out or cleaned; anything at the path that is not a
/// worktree of the canonical clone stops the session.
pub fn allocate(ready: &Ready, task: i64) -> Result<PathBuf> {
    allocate_dir(ready, &format!("task-{task}--work"))
}

/// [`allocate`] for any directory name under the project's worktree root, on
/// branch `<branch_prefix><dir>`. Bots use a persistent `bot-<name>`.
pub fn allocate_dir(ready: &Ready, dir: &str) -> Result<PathBuf> {
    ensure!(
        !dir.is_empty() && !dir.contains('/') && !dir.starts_with('.'),
        "invalid worktree directory name {dir:?}"
    );
    let path = ready.root.join(dir);
    let path_str = path.to_str().context("non-UTF-8 worktree path")?;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() && std::fs::read_dir(&path)?.next().is_none() => {
            // An empty directory left by an interrupted allocation holds nothing.
            std::fs::remove_dir(&path)?;
        }
        Ok(_) => {
            let top = git(&path, &["rev-parse", "--show-toplevel"]).ok();
            let common = git(
                &path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .ok();
            ensure!(
                top.as_deref() == Some(path_str)
                    && common.as_deref().map(Path::new) == Some(&ready.canonical.join(".git")),
                "{} exists but is not a worktree of {}; reconcile it manually",
                path.display(),
                ready.canonical.display()
            );
            return Ok(path);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    std::fs::create_dir_all(&ready.root)
        .with_context(|| format!("create {}", ready.root.display()))?;
    git_timeout(&ready.canonical, &["fetch", "origin"], FETCH_TIMEOUT)?;
    let branch = format!("{}{dir}", ready.branch_prefix);
    // Cleanup deletes directories, never branches: a reopened task resumes its
    // existing branch.
    let exists = git(
        &ready.canonical,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    if exists {
        git(&ready.canonical, &["worktree", "add", path_str, &branch])?;
    } else {
        git(
            &ready.canonical,
            &["worktree", "add", "-b", &branch, path_str, &ready.base_ref],
        )?;
    }
    Ok(path)
}
