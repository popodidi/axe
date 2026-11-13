//! Doc store: versioned text documents kept by `axe server`, with git as the
//! backend.
//!
//! A doc is a UTF-8 text file at a slash-separated path (`notes/plan.md`). A
//! folder of docs is addressed by its path too (`skills/review`), so a skill
//! is a folder holding `SKILL.md` plus its files. Every write is one commit in
//! a bare repository (`[docs] dir`, default `docs.git` next to the database);
//! a doc's version is the id of the last commit that changed it, and its
//! history is `git log` of its path.
//!
//! Docs attach to scopes: `global`, `label:<name>`, `owner:<name>` (a bot or a
//! person) or `repo:<name>`. Attachments live in the same repository (one
//! file per scope listing the attached paths), so they are versioned too.
//!
//! Tree layout: `docs/<path>` holds the documents and `attach/global`,
//! `attach/<kind>/<name>` the attachment lists.
//!
//! Concurrent writes: the server applies writes one at a time (a lock), and
//! each commit moves the branch with a compare-and-swap on its previous
//! commit, so no write is ever lost. A write may name the version it edits
//! (`base`); when the doc changed since, the write is rejected with
//! [`Conflict`] (HTTP 409) instead of overwriting the other change. A write
//! without `base` replaces whatever is there.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{self, Docs};

/// Largest doc, in bytes.
pub const MAX_DOC_BYTES: usize = 1024 * 1024;
/// Most files one folder write may hold.
pub const MAX_FOLDER_FILES: usize = 500;
const BRANCH: &str = "refs/heads/main";
const ZERO: &str = "0000000000000000000000000000000000000000";

/// A write that names a `base` version the doc no longer has.
#[derive(Debug)]
pub struct Conflict {
    pub path: String,
    pub base: String,
    /// The doc's current version; `None` when it does not exist.
    pub current: Option<String>,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let base = if self.base.is_empty() {
            "none (create only)"
        } else {
            &self.base
        };
        match &self.current {
            Some(c) => write!(
                f,
                "conflict: {} is at version {c}, not {base}; get it, merge, and put again",
                self.path
            ),
            None => write!(f, "conflict: {} does not exist (base {base})", self.path),
        }
    }
}

impl std::error::Error for Conflict {}

/// A path, scope or version the store refuses (HTTP 400).
#[derive(Debug)]
pub struct Invalid(pub String);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Invalid {}

fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Invalid(msg.into()).into())
}

/// One file of a folder write or read; `path` is relative to the folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    pub content: String,
}

/// What a write puts at a path: one doc, or a folder of docs that replaces
/// the folder's previous files.
#[derive(Debug, Clone)]
pub enum Body {
    Doc(String),
    Folder(Vec<File>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Written {
    pub path: String,
    /// The path's version after the write.
    pub version: String,
    /// False when the write changed nothing (same content).
    pub changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Got {
    pub path: String,
    pub version: String,
    /// `doc` or `folder`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<File>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Version {
    pub version: String,
    /// RFC 3339 commit time.
    pub at: String,
    pub author: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub version: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub scope: String,
    pub path: String,
    /// False when the attached doc or folder no longer exists.
    pub exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attached {
    pub scope: String,
    pub path: String,
    pub version: String,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diff {
    pub path: String,
    /// `None`: the doc did not exist before `to`.
    pub from: Option<String>,
    pub to: String,
    pub diff: String,
}

/// An attachment scope.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Global,
    Label(String),
    Owner(String),
    Repo(String),
}

impl Scope {
    pub fn parse(s: &str) -> Result<Scope> {
        if s == "global" {
            return Ok(Scope::Global);
        }
        let Some((kind, name)) = s.split_once(':') else {
            return invalid(format!(
                "invalid scope {s:?}: want global, label:<name>, owner:<name> or repo:<name>"
            ));
        };
        check_name(name).map_err(|e| Invalid(format!("invalid scope {s:?}: {e}")))?;
        let name = name.to_string();
        match kind {
            "label" => Ok(Scope::Label(name)),
            "owner" => Ok(Scope::Owner(name)),
            "repo" => Ok(Scope::Repo(name)),
            _ => invalid(format!(
                "invalid scope kind {kind:?}: want global, label, owner or repo"
            )),
        }
    }

    /// The scope's attachment file in the tree.
    fn file(&self) -> String {
        match self {
            Scope::Global => "attach/global".into(),
            Scope::Label(n) => format!("attach/label/{n}"),
            Scope::Owner(n) => format!("attach/owner/{n}"),
            Scope::Repo(n) => format!("attach/repo/{n}"),
        }
    }

    fn from_file(f: &str) -> Option<Scope> {
        let rest = f.strip_prefix("attach/")?;
        if rest == "global" {
            return Some(Scope::Global);
        }
        let (kind, name) = rest.split_once('/')?;
        Scope::parse(&format!("{kind}:{name}")).ok()
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Global => f.write_str("global"),
            Scope::Label(n) => write!(f, "label:{n}"),
            Scope::Owner(n) => write!(f, "owner:{n}"),
            Scope::Repo(n) => write!(f, "repo:{n}"),
        }
    }
}

/// A scope name: a label, bot, person or repo name.
fn check_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("name must be 1-128 characters".into());
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err("name must not start with '.' or '-'".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-@+:".contains(c))
    {
        return Err("name may hold only letters, digits and . _ - @ + :".into());
    }
    Ok(())
}

/// Checks a doc or folder path: slash-separated segments of letters,
/// digits, `.`, `_` and `-`, none starting with `.`. Returns it without
/// leading or trailing slashes.
pub fn check_path(path: &str) -> Result<String> {
    let p = path.trim_matches('/');
    if p.is_empty() || p.len() > 512 {
        return invalid(format!("invalid doc path {path:?}: want 1-512 characters"));
    }
    for seg in p.split('/') {
        if seg.is_empty()
            || seg.starts_with('.')
            || !seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return invalid(format!(
                "invalid doc path {path:?}: segments are letters, digits, . _ -, not starting with '.'"
            ));
        }
    }
    Ok(p.to_string())
}

/// A version: a commit id, full or abbreviated (at least 7 hex digits).
fn check_version(v: &str) -> Result<()> {
    if (7..=40).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        invalid(format!("invalid version {v:?}: want a commit id"))
    }
}

pub struct Store {
    git_dir: PathBuf,
    /// Serializes writes within this server.
    write: Mutex<()>,
}

impl Store {
    /// Opens (creating as needed) the repository `cfg` describes; `dir`
    /// defaults to `docs.git` next to `db_path`. Fails when `git` is missing.
    pub fn open(cfg: &Docs, db_path: &Path) -> Result<Store> {
        let git_dir = match &cfg.dir {
            Some(d) => config::expand_tilde(d),
            None => db_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("docs.git"),
        };
        if !git_dir.join("HEAD").is_file() {
            std::fs::create_dir_all(&git_dir)
                .with_context(|| format!("create {}", git_dir.display()))?;
            let out = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&git_dir)
                .env_remove("GIT_DIR")
                .output()
                .context("run git (is it installed?)")?;
            if !out.status.success() {
                bail!(
                    "git init {}: {}",
                    git_dir.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        std::fs::create_dir_all(git_dir.join("axe-empty"))?;
        let store = Store {
            git_dir,
            write: Mutex::new(()),
        };
        store.git(&["symbolic-ref", "HEAD", BRANCH], None)?;
        Ok(store)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.arg("--git-dir")
            .arg(&self.git_dir)
            .args(["-c", "core.quotePath=false", "-c", "safe.directory=*"])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null());
        c
    }

    /// Runs git and returns stdout; `input` goes to stdin.
    fn git(&self, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        self.run(self.cmd(args), args, input)
    }

    fn run(&self, mut c: Command, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            c.stdin(Stdio::piped());
        }
        let mut child = c.spawn().context("run git")?;
        if let Some(bytes) = input {
            let mut stdin = child.stdin.take().context("git stdin")?;
            stdin.write_all(bytes)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The branch head, or `None` before the first write.
    fn head(&self) -> Result<Option<String>> {
        let out = self
            .cmd(&["rev-parse", "--verify", "--quiet", BRANCH])
            .output()
            .context("run git")?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((out.status.success() && !s.is_empty()).then_some(s))
    }

    /// Resolves a version to its full commit id (404 when unknown).
    fn resolve(&self, v: &str) -> Result<String> {
        check_version(v)?;
        let out = self
            .cmd(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{v}^{{commit}}"),
            ])
            .output()
            .context("run git")?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || s.is_empty() {
            bail!("version {v} not found");
        }
        Ok(s)
    }

    /// What `docs/<path>` is at `commit`: a blob, a tree, or nothing.
    fn kind_at(&self, commit: &str, tree_path: &str) -> Result<Option<&'static str>> {
        let out = self.git(&["ls-tree", commit, "--", tree_path], None)?;
        Ok(out
            .lines()
            .next()
            .and_then(|l| match l.split_whitespace().nth(1) {
                Some("blob") => Some("doc"),
                Some("tree") => Some("folder"),
                _ => None,
            }))
    }

    /// The last commit at or before `at` that changed `tree_path`.
    fn last_change(&self, at: &str, tree_path: &str) -> Result<Option<String>> {
        let out = self.git(&["log", "-1", "--format=%H", at, "--", tree_path], None)?;
        Ok(Some(out.trim().to_string()).filter(|s| !s.is_empty()))
    }

    /// The current version of a doc or folder; `None` when it does not exist.
    pub fn version(&self, path: &str) -> Result<Option<String>> {
        let path = check_path(path)?;
        let Some(head) = self.head()? else {
            return Ok(None);
        };
        let tp = format!("docs/{path}");
        if self.kind_at(&head, &tp)?.is_none() {
            return Ok(None);
        }
        self.last_change(&head, &tp)
    }

    /// Writes a doc or folder at `path` as one commit by `author`. With
    /// `base`, the write applies only while the path is still at that
    /// version (`Some("")`: only while it does not exist); otherwise it fails
    /// with [`Conflict`].
    pub fn put(
        &self,
        path: &str,
        body: &Body,
        base: Option<&str>,
        author: &str,
        message: &str,
    ) -> Result<Written> {
        let path = check_path(path)?;
        let tp = format!("docs/{path}");
        let mut files: Vec<(String, &str)> = match body {
            Body::Doc(c) => vec![(tp.clone(), c.as_str())],
            Body::Folder(fs) => {
                if fs.is_empty() {
                    return invalid("a folder needs at least one file");
                }
                if fs.len() > MAX_FOLDER_FILES {
                    return invalid(format!("a folder holds at most {MAX_FOLDER_FILES} files"));
                }
                let mut v = Vec::new();
                for f in fs {
                    let rel = check_path(&f.path)?;
                    v.push((format!("{tp}/{rel}"), f.content.as_str()));
                }
                v
            }
        };
        files.sort();
        if files.windows(2).any(|w| w[0].0 == w[1].0) {
            return invalid("a folder lists the same file twice");
        }
        for (p, c) in &files {
            if c.len() > MAX_DOC_BYTES {
                return invalid(format!("{p} is over the {MAX_DOC_BYTES} byte limit"));
            }
        }
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let head = self.head()?;
        let previous = match &head {
            Some(h) if self.kind_at(h, &tp)?.is_some() => self.last_change(h, &tp)?,
            _ => None,
        };
        if let Some(base) = base {
            let ok = match (&previous, base) {
                (None, "") => true,
                (Some(cur), b) if !b.is_empty() => {
                    check_version(b)?;
                    cur.starts_with(&b.to_ascii_lowercase())
                }
                _ => false,
            };
            if !ok {
                return Err(Conflict {
                    path,
                    base: base.to_string(),
                    current: previous,
                }
                .into());
            }
        }
        if let Some(h) = &head {
            // A doc's ancestor folders must not be docs themselves.
            let mut anc = String::from("docs");
            for seg in path.split('/').take(path.split('/').count() - 1) {
                anc = format!("{anc}/{seg}");
                if self.kind_at(h, &anc)? == Some("doc") {
                    return invalid(format!(
                        "{} is a doc, not a folder",
                        anc.trim_start_matches("docs/")
                    ));
                }
            }
        }
        let commit = self.commit(head.as_deref(), author, message, |idx| {
            // Replace whatever is at the path: a doc, or a folder's files.
            let old = idx.git(&["ls-files", "-z", "--", &tp], None)?;
            for f in old.split('\0').filter(|f| !f.is_empty()) {
                idx.git(&["update-index", "--force-remove", "--", f], None)?;
            }
            for (p, c) in &files {
                let blob = idx.git(&["hash-object", "-w", "--stdin"], Some(c.as_bytes()))?;
                idx.git(
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{},{p}", blob.trim()),
                    ],
                    None,
                )?;
            }
            Ok(())
        })?;
        Ok(match commit {
            Some(c) => Written {
                path,
                version: c,
                changed: true,
                previous,
            },
            None => Written {
                path,
                version: previous.clone().unwrap_or_default(),
                changed: false,
                previous,
            },
        })
    }

    /// Builds a commit on `head` from an index `edit` changes and moves the
    /// branch to it (compare-and-swap on `head`). `None` when the tree did
    /// not change. Callers hold the write lock.
    fn commit(
        &self,
        head: Option<&str>,
        author: &str,
        message: &str,
        edit: impl FnOnce(&Index) -> Result<()>,
    ) -> Result<Option<String>> {
        let idx = Index {
            store: self,
            file: self
                .git_dir
                .join(format!("axe-index-{}", uuid::Uuid::new_v4().simple())),
        };
        let result = self.commit_with(&idx, head, author, message, edit);
        let _ = std::fs::remove_file(&idx.file);
        result
    }

    fn commit_with(
        &self,
        idx: &Index,
        head: Option<&str>,
        author: &str,
        message: &str,
        edit: impl FnOnce(&Index) -> Result<()>,
    ) -> Result<Option<String>> {
        match head {
            Some(h) => idx.git(&["read-tree", h], None)?,
            None => idx.git(&["read-tree", "--empty"], None)?,
        };
        edit(idx)?;
        let tree = idx.git(&["write-tree"], None)?.trim().to_string();
        if let Some(h) = head {
            let old = self.git(&["rev-parse", &format!("{h}^{{tree}}")], None)?;
            if old.trim() == tree {
                return Ok(None);
            }
        }
        let author = if author.trim().is_empty() {
            "human"
        } else {
            author.trim()
        };
        let message = if message.trim().is_empty() {
            "update"
        } else {
            message.trim()
        };
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(h) = head {
            args.extend(["-p", h]);
        }
        let mut c = self.cmd(&args);
        let email = format!("{}@axe", author.replace(|c: char| c.is_whitespace(), "-"));
        c.env("GIT_AUTHOR_NAME", author)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_COMMITTER_NAME", author)
            .env("GIT_COMMITTER_EMAIL", &email);
        let commit = self
            .run(c, &args, Some(message.as_bytes()))?
            .trim()
            .to_string();
        self.git(&["update-ref", BRANCH, &commit, head.unwrap_or(ZERO)], None)
            .context("the docs branch moved during the write; retry")?;
        Ok(Some(commit))
    }

    /// A doc or folder at `version` (default: the current one).
    pub fn get(&self, path: &str, version: Option<&str>) -> Result<Got> {
        let path = check_path(path)?;
        let tp = format!("docs/{path}");
        let not_found = || anyhow::anyhow!("doc {path} not found");
        let at = match version {
            Some(v) => self.resolve(v)?,
            None => self.head()?.ok_or_else(not_found)?,
        };
        let kind = self.kind_at(&at, &tp)?.ok_or_else(not_found)?;
        let ver = self.last_change(&at, &tp)?.ok_or_else(not_found)?;
        if kind == "doc" {
            let content = self.git(&["show", &format!("{at}:{tp}")], None)?;
            return Ok(Got {
                path,
                version: ver,
                kind,
                content: Some(content),
                files: None,
            });
        }
        let list = self.git(
            &["ls-tree", "-r", "-z", "--name-only", &at, "--", &tp],
            None,
        )?;
        let mut files = Vec::new();
        for f in list.split('\0').filter(|f| !f.is_empty()) {
            let content = self.git(&["show", &format!("{at}:{f}")], None)?;
            let rel = f.strip_prefix(&format!("{tp}/")).unwrap_or(f).to_string();
            files.push(File { path: rel, content });
        }
        Ok(Got {
            path,
            version: ver,
            kind,
            content: None,
            files: Some(files),
        })
    }

    /// Every version of a doc or folder, newest first.
    pub fn log(&self, path: &str) -> Result<Vec<Version>> {
        let path = check_path(path)?;
        let Some(head) = self.head()? else {
            bail!("doc {path} not found");
        };
        let out = self.git(
            &[
                "log",
                "--format=%H%x1f%cI%x1f%an%x1f%s%x1e",
                &head,
                "--",
                &format!("docs/{path}"),
            ],
            None,
        )?;
        let v: Vec<Version> = out
            .split('\x1e')
            .filter_map(|r| {
                let mut f = r.trim_start_matches('\n').split('\x1f');
                Some(Version {
                    version: f.next().filter(|s| !s.is_empty())?.to_string(),
                    at: f.next()?.to_string(),
                    author: f.next()?.to_string(),
                    message: f.next()?.trim_end().to_string(),
                })
            })
            .collect();
        if v.is_empty() {
            bail!("doc {path} not found");
        }
        Ok(v)
    }

    /// A unified diff of a doc or folder between two versions. `to` defaults
    /// to the current version and `from` to the version before `to`.
    pub fn diff(&self, path: &str, from: Option<&str>, to: Option<&str>) -> Result<Diff> {
        let path = check_path(path)?;
        let tp = format!("docs/{path}");
        let to = match to {
            Some(v) => self.resolve(v)?,
            None => self
                .version(&path)?
                .ok_or_else(|| anyhow::anyhow!("doc {path} not found"))?,
        };
        let from = match from {
            Some(v) => Some(self.resolve(v)?),
            None => {
                let out = self.git(&["log", "-2", "--format=%H", &to, "--", &tp], None)?;
                out.lines().nth(1).map(String::from)
            }
        };
        // The empty tree stands in for "before the first version".
        let empty = self.git(&["hash-object", "-t", "tree", "--stdin"], Some(b""))?;
        let a = from.clone().unwrap_or_else(|| empty.trim().to_string());
        let diff = self.git(
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--relative=docs/",
                &a,
                &to,
                "--",
                &tp,
            ],
            None,
        )?;
        Ok(Diff {
            path,
            from,
            to,
            diff,
        })
    }

    /// Current docs (files) under `prefix`, with each one's version and time.
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<Entry>> {
        let Some(head) = self.head()? else {
            return Ok(Vec::new());
        };
        let tp = match prefix
            .map(|p| p.trim_matches('/'))
            .filter(|p| !p.is_empty())
        {
            Some(p) => format!("docs/{}", check_path(p)?),
            None => "docs".to_string(),
        };
        let tree = self.git(&["ls-tree", "-r", "-z", "--long", &head, "--", &tp], None)?;
        let mut sizes = BTreeMap::new();
        for line in tree.split('\0').filter(|l| !l.is_empty()) {
            let Some((meta, name)) = line.split_once('\t') else {
                continue;
            };
            let size = meta
                .split_whitespace()
                .nth(3)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            sizes.insert(name.to_string(), size);
        }
        // One pass over history: the newest commit naming a file is its version.
        let log = self.git(
            &[
                "log",
                "-z",
                "--name-only",
                "--format=%x1e%H%x1f%cI",
                &head,
                "--",
                &tp,
            ],
            None,
        )?;
        let mut seen: BTreeMap<String, (String, String)> = BTreeMap::new();
        for rec in log.split('\x1e').filter(|r| !r.is_empty()) {
            let mut parts = rec.split('\0');
            let header = parts.next().unwrap_or("");
            let (h, at) = header.trim().split_once('\x1f').unwrap_or((header, ""));
            for name in parts.map(|n| n.trim_start_matches('\n')) {
                if sizes.contains_key(name) && !seen.contains_key(name) {
                    seen.insert(name.to_string(), (h.to_string(), at.to_string()));
                }
            }
        }
        Ok(sizes
            .into_iter()
            .map(|(name, size)| {
                let (version, updated_at) = seen.remove(&name).unwrap_or_default();
                Entry {
                    path: name.trim_start_matches("docs/").to_string(),
                    size,
                    version,
                    updated_at,
                }
            })
            .collect())
    }

    /// Attachments of one scope, or of every scope.
    pub fn attachments(&self, scope: Option<&Scope>) -> Result<Vec<Attachment>> {
        let Some(head) = self.head()? else {
            return Ok(Vec::new());
        };
        let where_ = scope.map_or_else(|| "attach".to_string(), Scope::file);
        let names = self.git(
            &["ls-tree", "-r", "-z", "--name-only", &head, "--", &where_],
            None,
        )?;
        let mut out = Vec::new();
        for f in names.split('\0').filter(|f| !f.is_empty()) {
            let Some(s) = Scope::from_file(f) else {
                continue;
            };
            if scope.is_some_and(|want| *want != s) {
                continue;
            }
            for p in self.attach_list(&head, f)? {
                let exists = self.kind_at(&head, &format!("docs/{p}"))?.is_some();
                out.push(Attachment {
                    scope: s.to_string(),
                    path: p,
                    exists,
                });
            }
        }
        Ok(out)
    }

    fn attach_list(&self, head: &str, file: &str) -> Result<Vec<String>> {
        let content = self.git(&["show", &format!("{head}:{file}")], None)?;
        Ok(content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect())
    }

    /// Attaches (`on`) or detaches a doc or folder to a scope. Attaching
    /// needs the path to exist; detaching needs the attachment to exist.
    pub fn attach(&self, path: &str, scope: &Scope, on: bool, author: &str) -> Result<Attached> {
        let path = check_path(path)?;
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let head = self.head()?;
        let file = scope.file();
        let mut list = match &head {
            Some(h) if self.kind_at(h, &file)? == Some("doc") => self.attach_list(h, &file)?,
            _ => Vec::new(),
        };
        let had = list.contains(&path);
        if on {
            let exists = match &head {
                Some(h) => self.kind_at(h, &format!("docs/{path}"))?.is_some(),
                None => false,
            };
            if !exists {
                bail!("doc {path} not found");
            }
            if !had {
                list.push(path.clone());
            }
        } else {
            if !had {
                bail!("attachment of {path} to {scope} not found");
            }
            list.retain(|p| *p != path);
        }
        list.sort();
        let verb = if on { "attach" } else { "detach" };
        let msg = format!("{verb} {path} {} {scope}", if on { "to" } else { "from" });
        let commit = if had == on {
            None
        } else {
            self.commit(head.as_deref(), author, &msg, |idx| {
                if list.is_empty() {
                    idx.git(&["update-index", "--force-remove", "--", &file], None)?;
                    return Ok(());
                }
                let body = format!("{}\n", list.join("\n"));
                let blob = idx.git(&["hash-object", "-w", "--stdin"], Some(body.as_bytes()))?;
                idx.git(
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{},{file}", blob.trim()),
                    ],
                    None,
                )?;
                Ok(())
            })?
        };
        let changed = commit.is_some();
        let version = match commit {
            Some(c) => c,
            None => head.unwrap_or_default(),
        };
        Ok(Attached {
            scope: scope.to_string(),
            path,
            version,
            changed,
        })
    }
}

/// A temporary index over the store's repository.
struct Index<'a> {
    store: &'a Store,
    file: PathBuf,
}

impl Index<'_> {
    fn git(&self, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        let mut c = self.store.cmd(args);
        // update-index wants a work tree; it never touches its files here.
        c.env("GIT_INDEX_FILE", &self.file)
            .env("GIT_WORK_TREE", self.store.git_dir.join("axe-empty"));
        self.store.run(c, args, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&Docs::default(), &dir.path().join("axe.db")).unwrap();
        (dir, s)
    }

    #[test]
    fn paths_and_scopes_are_checked() {
        assert_eq!(check_path("/a/b.md/").unwrap(), "a/b.md");
        for bad in ["", "a//b", "../x", "a/.git/x", "a b", "a/../b"] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Scope::parse("global").unwrap(), Scope::Global);
        assert_eq!(
            Scope::parse("owner:bot:axe-os").unwrap(),
            Scope::Owner("bot:axe-os".into())
        );
        assert_eq!(Scope::parse("repo:axe").unwrap().to_string(), "repo:axe");
        for bad in ["label", "label:", "team:x", "label:a/b", "repo:.x"] {
            assert!(Scope::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn writes_version_and_conflict() {
        let (_d, s) = store();
        let w1 = s
            .put(
                "a.md",
                &Body::Doc("one\n".into()),
                Some(""),
                "alice",
                "first",
            )
            .unwrap();
        assert!(w1.changed);
        // Create-only again: conflict.
        let e = s
            .put("a.md", &Body::Doc("x\n".into()), Some(""), "bob", "")
            .unwrap_err();
        assert!(e.downcast_ref::<Conflict>().is_some());
        // Same content: no new version.
        let same = s
            .put("a.md", &Body::Doc("one\n".into()), None, "bob", "")
            .unwrap();
        assert!(!same.changed);
        assert_eq!(same.version, w1.version);
        let w2 = s
            .put(
                "a.md",
                &Body::Doc("two\n".into()),
                Some(&w1.version[..8]),
                "bob",
                "",
            )
            .unwrap();
        assert_eq!(w2.previous.as_deref(), Some(w1.version.as_str()));
        // A stale base is rejected and names the current version.
        let e = s
            .put(
                "a.md",
                &Body::Doc("three\n".into()),
                Some(&w1.version),
                "carol",
                "",
            )
            .unwrap_err();
        let c = e.downcast_ref::<Conflict>().unwrap();
        assert_eq!(c.current.as_deref(), Some(w2.version.as_str()));
        // A doc cannot be a folder's parent.
        assert!(s
            .put("a.md/x", &Body::Doc("y".into()), None, "a", "")
            .is_err());
    }
}
