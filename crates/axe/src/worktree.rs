//! Git plumbing for the repo worktrees of run workspaces (`run_workspace.rs`):
//! a repo's canonical clone ([`repo_readiness`], cloned first when missing,
//! [`ensure_clone_from`]), one worktree of it per run folder
//! ([`allocate_from`]) and the safe fast-forward of a worktree to its base
//! branch: [`sync`] (`axe repo sync`, from the worktree alone) and
//! [`refresh`] (`axe workspace`), each recording when the worktree was last
//! brought up to date ([`synced_at`]). Cleanup of a done task's folder is left
//! to housekeeping.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};

use crate::config::Config;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(180);
/// A first clone can be a large repository.
const CLONE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A repo whose canonical clone can host worktrees.
#[derive(Debug, Clone)]
pub struct Ready {
    pub canonical: PathBuf,
    /// `refs/remotes/origin/<default>`, from `origin/HEAD`.
    pub base_ref: String,
}

/// Where a missing canonical clone of a repo is cloned from
/// ([`clone_url`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneUrl {
    /// `[repos.<name>] url`, else the server's repo registry.
    Url(String),
    /// Neither has one, or the canonical clone exists and none was needed.
    None,
    /// No `[repos.<name>] url`, and the registry could not be read: why.
    Unknown(String),
}

impl CloneUrl {
    pub fn url(&self) -> Option<&str> {
        match self {
            CloneUrl::Url(u) => Some(u),
            _ => None,
        }
    }
}

/// The clone URL of repo `name`: `[repos.<name>] url` wins; else, only when
/// the canonical clone is missing, `registry` (the server's repo registry,
/// `repo.rs`) is asked. A registry URL is checked like a config one.
pub fn clone_url(
    cfg: &Config,
    name: &str,
    registry: &dyn Fn(&str) -> Result<Option<String>>,
) -> CloneUrl {
    if let Some(u) = cfg.repo_url(name) {
        return CloneUrl::Url(u.to_string());
    }
    if std::fs::symlink_metadata(cfg.repo_path(name)).is_ok() {
        return CloneUrl::None;
    }
    match registry(name) {
        Ok(Some(u)) => match crate::config::validate_repo_url(&u) {
            Ok(()) => CloneUrl::Url(u),
            Err(e) => CloneUrl::Unknown(format!("the server's repo registry URL is refused: {e}")),
        },
        Ok(None) => CloneUrl::None,
        Err(e) => CloneUrl::Unknown(format!("{e:#}")),
    }
}

/// Checks that repo `name` has a canonical clone with a known default branch:
/// `[repos.<name>] path` (default `<workspace_root>/<name>`). The error names
/// the gap. Only the local config's url counts here; see
/// [`repo_readiness_with`].
pub fn repo_readiness(cfg: &Config, name: &str) -> Result<Ready, String> {
    repo_readiness_with(cfg, name, &clone_url(cfg, name, &|_| Ok(None)))
}

/// [`repo_readiness`] with the repo's resolved [`clone_url`], so a missing
/// clone's gap names why there is nothing to clone it from.
pub fn repo_readiness_with(cfg: &Config, name: &str, url: &CloneUrl) -> Result<Ready, String> {
    let canonical = cfg.repo_path(name);
    let gap = |detail: String| format!("repo {name} has no worktree base: {detail}");
    if !canonical.exists() {
        match url {
            CloneUrl::Url(_) => {}
            CloneUrl::None => {
                return Err(gap(format!(
                    "{} does not exist and no clone URL is known: register one with \
                     `axe repo add {name} <url>` (or set [repos.{name}] url in this \
                     machine's config)",
                    canonical.display()
                )))
            }
            CloneUrl::Unknown(why) => {
                return Err(gap(format!(
                    "{} does not exist, [repos.{name}] has no url, and the server's repo \
                     registry could not be read: {why}",
                    canonical.display()
                )))
            }
        }
    }
    check(canonical, gap)
}

/// [`ensure_clone_from`] with `[repos.<name>] url` only.
pub fn ensure_clone(cfg: &Config, name: &str) -> Result<bool> {
    ensure_clone_from(cfg, name, cfg.repo_url(name))
}

/// Clones repo `name` from `url` (its [`clone_url`]) into its canonical path
/// (`[repos.<name>] path`) when nothing is there yet, with the host's own Git
/// credentials; `true` when this call cloned it. An existing path, a clone or
/// not, is never touched, and a repo without a `url` is left to
/// [`repo_readiness_with`] to report. Callers naming the same missing repo at once
/// clone it once: each takes an exclusive lock (under `log_dir`) and the ones
/// that waited find the clone in place. The clone goes to a temporary sibling
/// and is renamed into place, so an interrupted clone leaves no partial
/// canonical clone.
pub fn ensure_clone_from(cfg: &Config, name: &str, url: Option<&str>) -> Result<bool> {
    use fs2::FileExt;
    let Some(url) = url else {
        return Ok(false);
    };
    let canonical = cfg.repo_path(name);
    if std::fs::symlink_metadata(&canonical).is_ok() {
        return Ok(false);
    }
    let locks = cfg.log_dir().join(".clones");
    std::fs::create_dir_all(&locks).with_context(|| format!("create {}", locks.display()))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(format!("{name}.lock")))?;
    lock.lock_exclusive()
        .with_context(|| format!("lock the clone of repo {name}"))?;
    if std::fs::symlink_metadata(&canonical).is_ok() {
        return Ok(false);
    }
    let parent = canonical
        .parent()
        .context("canonical clone path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let leaf = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .context("non-UTF-8 canonical clone path")?;
    let tmp = parent.join(format!(".{leaf}.axe-clone-{}", uuid::Uuid::new_v4()));
    let tmp_str = tmp.to_str().context("non-UTF-8 canonical clone path")?;
    let clone = |extra: &[&str]| {
        let mut args = extra.to_vec();
        args.extend(["clone", "--quiet", "--", url, tmp_str]);
        git_timeout(parent, &args, CLONE_TIMEOUT)
    };
    let mut cloned = clone(&[]);
    // A clone the usual credentials cannot make is tried again with only
    // the git credential a sealed form stored for its host (`sealed.rs`),
    // and that clone keeps using it.
    let helper = form_credential_helper(cfg, url);
    let mut with_form = false;
    if let (Err(_), Some(h)) = (&cloned, &helper) {
        let _ = std::fs::remove_dir_all(&tmp);
        cloned = clone(&[
            "-c",
            "credential.helper=",
            "-c",
            &format!("credential.helper={h}"),
        ]);
        with_form = cloned.is_ok();
    }
    let cloned = cloned
        .and_then(|_| {
            if let (true, Some(h)) = (with_form, &helper) {
                git(
                    &tmp,
                    &[
                        "config",
                        "--local",
                        "--replace-all",
                        "credential.helper",
                        "",
                    ],
                )?;
                git(
                    &tmp,
                    &["config", "--local", "--add", "credential.helper", h],
                )?;
            }
            Ok(())
        })
        .and_then(|_| {
            std::fs::rename(&tmp, &canonical)
                .with_context(|| format!("move the clone to {}", canonical.display()))
        });
    if let Err(e) = cloned {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e.context(format!(
            "clone repo {name} from {url} into {}",
            canonical.display()
        )));
    }
    Ok(true)
}

/// The `credential.helper` value that reads the sealed form git credentials
/// file, when it has a credential for `url`'s host (`https://` only).
fn form_credential_helper(cfg: &Config, url: &str) -> Option<String> {
    let host = url
        .strip_prefix("https://")?
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?;
    let path = cfg.forms.git_credentials_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let suffix = format!("@{host}");
    text.lines()
        .any(|l| l.trim_end().ends_with(&suffix))
        .then(|| format!("store --file={}", path.display()))
}

fn check(canonical: PathBuf, gap: impl Fn(String) -> String) -> Result<Ready, String> {
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
                "{} has no refs/remotes/origin/HEAD (run `git -C {} remote set-head origin {}`)",
                canonical.display(),
                canonical.display(),
                head_hint(&canonical)
            ))
        })?;
    Ok(Ready {
        canonical,
        base_ref,
    })
}

/// The branch to name in the missing-`origin/HEAD` hint. `set-head --auto`
/// asks the remote, which fails for a fresh bare origin whose HEAD names an
/// unpushed branch, so name a fetched branch: `main` or `master` when fetched,
/// else the only one, else `<branch>`.
fn head_hint(canonical: &Path) -> String {
    let refs = git(
        canonical,
        &[
            "for-each-ref",
            "--format=%(refname:lstrip=3)",
            "refs/remotes/origin/",
        ],
    )
    .unwrap_or_default();
    let branches: Vec<&str> = refs.lines().filter(|b| *b != "HEAD").collect();
    ["main", "master"]
        .into_iter()
        .find(|b| branches.contains(b))
        .or(match branches[..] {
            [only] => Some(only),
            _ => None,
        })
        .unwrap_or("<branch>")
        .to_string()
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
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            crate::trace::detail(
                start.elapsed(),
                format_args!("git -C {} {}  (timed out)", dir.display(), args.join(" ")),
            );
            bail!(
                "git {} timed out after {}s in {}",
                args.join(" "),
                timeout.as_secs(),
                dir.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    crate::trace::detail(
        start.elapsed(),
        format_args!(
            "git -C {} {}{}",
            dir.display(),
            args.join(" "),
            if status.success() { "" } else { "  (failed)" }
        ),
    );
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

/// Creates or reuses a worktree of `ready.canonical` at `path` on `branch`,
/// created from the default branch when missing. An existing checkout is
/// never reset, checked out or cleaned; anything at the path that is not a
/// worktree of the canonical clone stops the session. With `from_origin`, a
/// branch missing locally but pushed to `origin` is created from (and tracks)
/// `origin/<branch>` instead of the default branch: a local workspace picks up
/// the work a run pushed; and the canonical clone's worktrees whose folders
/// were deleted are pruned first.
pub fn allocate_from(
    ready: &Ready,
    path: &Path,
    branch: &str,
    from_origin: bool,
) -> Result<PathBuf> {
    let path = path.to_path_buf();
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
    let parent = path.parent().context("worktree path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    git_timeout(&ready.canonical, &["fetch", "origin"], FETCH_TIMEOUT)?;
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
    let pushed = format!("refs/remotes/origin/{branch}");
    if from_origin {
        // A local workspace folder deleted by hand leaves its worktrees
        // registered, and `worktree add` refuses a registered path or a
        // branch checked out there; forget the worktrees whose folders are
        // gone.
        git(&ready.canonical, &["worktree", "prune"])?;
    }
    if exists {
        git(&ready.canonical, &["worktree", "add", path_str, branch])?;
    } else if from_origin
        && git(
            &ready.canonical,
            &["rev-parse", "--verify", "--quiet", &pushed],
        )
        .is_ok()
    {
        git(
            &ready.canonical,
            &[
                "worktree", "add", "--track", "-b", branch, path_str, &pushed,
            ],
        )?;
    } else {
        git(
            &ready.canonical,
            &["worktree", "add", "-b", branch, path_str, &ready.base_ref],
        )?;
    }
    // Just fetched: a worktree that starts at its base is synced now.
    if let (Ok(head), Ok(base)) = (
        git(&path, &["rev-parse", "HEAD"]),
        git(
            &path,
            &["rev-parse", &format!("{}^{{commit}}", ready.base_ref)],
        ),
    ) {
        if head == base {
            let _ = stamp(&path);
        }
    }
    Ok(path)
}

/// What [`refresh`] or [`sync`] did to a persistent worktree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Refresh {
    /// Already at the base branch's origin commit (or ahead of nothing).
    Current { head: String },
    /// Fast-forwarded from `from` to `to`.
    Updated { from: String, to: String },
    /// Left unchanged; `reason` says why.
    Skipped { reason: String },
}

/// Brings the persistent worktree at `path` up to `ready.base_ref` after a
/// fetch, only by fast-forward and only when it is safe: the worktree is clean
/// (no changed or untracked files) and has no commits the base lacks.
/// Otherwise it changes nothing and says why. Never resets, discards or
/// forces anything; the caller guarantees no run is using the worktree.
pub fn refresh(ready: &Ready, path: &Path) -> Refresh {
    let r = git_timeout(&ready.canonical, &["fetch", "origin"], FETCH_TIMEOUT)
        .and_then(|_| fast_forward(path, &ready.base_ref));
    match r {
        Ok(r) => r,
        Err(e) => Refresh::Skipped {
            reason: format!("{e:#}"),
        },
    }
}

/// [`refresh`] from the worktree at `path` alone (`axe repo sync`): fetches
/// `origin` and fast-forwards to its `origin/HEAD` branch under the same
/// safety rules. A fetch or Git error is an error; an unsafe worktree is
/// [`Refresh::Skipped`] with its origin refs fetched.
pub fn sync(path: &Path) -> Result<Refresh> {
    let base = git(path, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .filter(|r| r.starts_with("refs/remotes/origin/"))
        .with_context(|| format!("{} has no refs/remotes/origin/HEAD", path.display()))?;
    git_timeout(path, &["fetch", "origin"], FETCH_TIMEOUT)?;
    fast_forward(path, &base)
}

/// Fast-forwards `path` to the already fetched `base` when it is clean and
/// has no local commits; records the time ([`synced_at`]) when the worktree
/// ends at `base`.
fn fast_forward(path: &Path, base: &str) -> Result<Refresh> {
    let short = base.strip_prefix("refs/remotes/").unwrap_or(base);
    let dirty = git(path, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Ok(Refresh::Skipped {
            reason: format!(
                "worktree has uncommitted or untracked changes ({} paths)",
                dirty.lines().count()
            ),
        });
    }
    let ahead = git(path, &["rev-list", "--count", &format!("{base}..HEAD")])?;
    if ahead != "0" {
        return Ok(Refresh::Skipped {
            reason: format!("worktree has {ahead} local commits not on {short}"),
        });
    }
    let head = git(path, &["rev-parse", "HEAD"])?;
    let to = git(path, &["rev-parse", &format!("{base}^{{commit}}")])?;
    let out = if head == to {
        Refresh::Current { head }
    } else {
        git(path, &["merge", "--ff-only", "--quiet", base])?;
        Refresh::Updated { from: head, to }
    };
    stamp(path)?;
    Ok(out)
}

/// Where a worktree's last sync time is kept: its own Git directory
/// (`<canonical>/.git/worktrees/<name>`), so it leaves the checkout clean and
/// goes with `git worktree remove`.
fn stamp_path(path: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git(path, &["rev-parse", "--absolute-git-dir"])?).join("axe-synced"))
}

fn stamp(path: &Path) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let at = stamp_path(path)?;
    std::fs::write(&at, format!("{now}\n")).with_context(|| format!("write {}", at.display()))
}

/// When the worktree at `path` was last brought up to its base (Unix
/// seconds): by [`sync`], [`refresh`] or its creation. `None` when never
/// recorded.
pub fn synced_at(path: &Path) -> Option<i64> {
    std::fs::read_to_string(stamp_path(path).ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_form_credential_applies_to_its_host_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("git-credentials");
        let cfg = crate::config::Config::parse(&format!(
            "[forms]\ngit_credentials = \"{}\"\n",
            path.display()
        ))
        .unwrap();
        assert_eq!(
            super::form_credential_helper(&cfg, "https://github.com/o/r.git"),
            None
        );
        crate::sealed::write_git_credential(&path, "github.com", "x-access-token", "t").unwrap();
        let want = Some(format!("store --file={}", path.display()));
        assert_eq!(
            super::form_credential_helper(&cfg, "https://github.com/o/r.git"),
            want
        );
        assert_eq!(
            super::form_credential_helper(&cfg, "https://u@github.com/o/r"),
            want
        );
        assert_eq!(
            super::form_credential_helper(&cfg, "https://gitlab.com/o/r.git"),
            None
        );
        assert_eq!(
            super::form_credential_helper(&cfg, "git@github.com:o/r.git"),
            None
        );
    }
}
