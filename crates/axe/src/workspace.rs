//! Local workspaces: `axe workspace <task|bot>` builds, on the machine it
//! runs on, the folder a task's or bot's run gets (`run_workspace.rs`,
//! [`Mode::Local`]), and `axe workspace sync` writes edits to its
//! store-backed files back to the doc store.
//!
//! ```text
//! ~/axe/task-<id>/ or ~/axe/bot-<name>/    default folder (--dir to choose)
//!   AGENTS.md, skills/, docs/index.md, repos/<r>/, .agents/skills, .claude/skills
//!   docs/store/<path>                      attached docs and bundles, in full
//!   .axe/manifest.json                     store path, version and file hashes
//!                                          of every store-backed file
//!   .axe/system.md                         bots: the bot's system prompt, for
//!                                          reference
//! ```
//!
//! Building claims nothing and records no run. A task that is `in_progress`
//! is refused: a live session owns its branch. Repo worktrees are created
//! once and never reset; a missing task branch starts from `origin` when it
//! was pushed. A rebuild replaces `docs/` and `skills/`, so it refuses while
//! a store-backed file has an edit not yet synced, a conflict is open, or a
//! file there is not from the store.
//!
//! `sync` sends each store-backed file (a doc, or a whole skill or bundle
//! folder) whose content changed since it was written, as one write based on
//! the version it was written at. When the store moved on, the write is
//! refused; the local edit stays, the store's copy is written next to it as
//! `<file>.store`, and sync exits non-zero. Once the `.store` file is merged
//! and deleted, the next sync writes the edit on top of that store version.
//! Generated files and new files are never sent. With nothing left open,
//! sync then rebuilds the folder.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client::{Client, ClientError};
use crate::config::Config;
use crate::context::{self, Fetched, Source as _};
use crate::run_workspace::{self, Mode};

const MANIFEST: &str = ".axe/manifest.json";
/// Suffix of the store's copy of a file whose sync conflicted.
const STORE_COPY: &str = ".store";
/// Files the builder generates; edits to them are never synced.
const GENERATED: [&str; 2] = ["AGENTS.md", "docs/index.md"];

/// What a workspace is built for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Task(i64),
    Bot(String),
}

impl Target {
    /// A task id, else a bot name.
    pub fn parse(s: &str) -> Target {
        match s.parse() {
            Ok(id) => Target::Task(id),
            Err(_) => Target::Bot(s.to_string()),
        }
    }

    /// `~/axe/task-<id>` or `~/axe/bot-<name>`.
    pub fn default_dir(&self) -> Result<PathBuf> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join("axe").join(match self {
            Target::Task(id) => format!("task-{id}"),
            Target::Bot(name) => format!("bot-{name}"),
        }))
    }
}

/// `.axe/manifest.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    target: Target,
    /// Every store-backed doc or folder in the workspace.
    files: Vec<Entry>,
    /// The generated files' hashes, to tell an edit to one.
    generated: BTreeMap<String, String>,
}

/// One store-backed doc or folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// Store path.
    path: String,
    /// Path in the workspace.
    local: String,
    folder: bool,
    /// The store version the local copy is based on.
    version: String,
    /// sha256 of each file as written: the folder's files by relative path,
    /// or a doc under the empty name.
    sha256: BTreeMap<String, String>,
    /// The store's version when the last sync of this entry conflicted;
    /// cleared by the next successful write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conflict: Option<String>,
}

/// Builds or refreshes the workspace of `target` in `dir` (default
/// [`Target::default_dir`]) and returns the folder.
pub fn build(c: &Client, cfg: &Config, target: &Target, dir: Option<&Path>) -> Result<PathBuf> {
    let dir = match dir {
        Some(d) => absolute(d)?,
        None => target.default_dir()?,
    };
    if let Some(m) = read_manifest(&dir)? {
        if m.target != *target {
            bail!(
                "{} is the workspace of {}, not {}",
                dir.display(),
                describe(&m.target),
                describe(target)
            );
        }
        let open = local_changes(&dir, &m)?;
        if !open.is_empty() {
            bail!(
                "{} has local changes a rebuild would replace:\n  {}\nrun `axe workspace sync` \
                 first, or move them out of the folder",
                dir.display(),
                open.join("\n  ")
            );
        }
        warn_generated(&dir, &m);
    } else if std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some()) {
        bail!(
            "{} exists and is not an axe workspace (no {MANIFEST}); choose another --dir",
            dir.display()
        );
    }
    rebuild(c, cfg, target, &dir)?;
    Ok(dir)
}

/// The workspace holding `dir` (default: the current directory).
pub fn find(dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(d) = dir {
        let d = absolute(d)?;
        if !d.join(MANIFEST).is_file() {
            bail!("{} is not an axe workspace (no {MANIFEST})", d.display());
        }
        return Ok(d);
    }
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|a| a.join(MANIFEST).is_file())
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "{} is not inside an axe workspace (no {MANIFEST} here or above)",
                cwd.display()
            )
        })
}

/// Writes the edited store-backed files of the workspace `dir` back to the
/// store (module docs), then rebuilds it when nothing is left open. Fails
/// when a write conflicted or a conflict is still open.
pub fn sync(c: &Client, cfg: &Config, dir: &Path) -> Result<()> {
    let mut m = read_manifest(dir)?.context("no manifest")?;
    if let Target::Task(id) = &m.target {
        check_task(&c.get(&format!("/api/tasks/{id}"), &[])?)?;
    }
    warn_generated(dir, &m);
    let new = untracked(dir, &m)?;
    for p in &new {
        eprintln!(
            "axe: {p} is not in the store; creating store docs is out of scope here \
             (use `axe doc put`)"
        );
    }
    let mut open = 0;
    for i in 0..m.files.len() {
        let e = m.files[i].clone();
        let Some(files) = read_local(dir, &e)? else {
            eprintln!(
                "axe: {} was deleted here; deleting store docs is out of scope, \
                 so it is not synced and the next rebuild restores it",
                e.local
            );
            continue;
        };
        let hashes = hash(&files);
        let base = match &e.conflict {
            Some(_) if !store_copies(dir, &e)?.is_empty() => {
                eprintln!(
                    "conflict: {} is still open: merge {} into it, delete the {STORE_COPY} \
                     file, then sync again",
                    e.local,
                    store_copies(dir, &e)?.join(", ")
                );
                open += 1;
                continue;
            }
            Some(theirs) => theirs.clone(),
            None => e.version.clone(),
        };
        if hashes == e.sha256 {
            if e.conflict.is_some() {
                // The edit was dropped: the rebuild takes the store's copy.
                m.files[i].conflict = None;
                write_manifest(dir, &m)?;
            }
            continue;
        }
        let mut body = json!({
            "path": e.path,
            "base": base,
            "message": "axe workspace sync",
        });
        if e.folder {
            body["files"] = files
                .iter()
                .map(|(p, c)| json!({"path": p, "content": c}))
                .collect();
        } else {
            body["content"] = files.get("").cloned().unwrap_or_default().into();
        }
        match c.send::<Value>("POST", "/api/docs", body) {
            Ok(v) => {
                let version = v["version"].as_str().unwrap_or_default().to_string();
                println!(
                    "{}  {}  {}",
                    &version[..version.len().min(12)],
                    e.path,
                    if v["changed"] == true {
                        "written"
                    } else {
                        "unchanged"
                    }
                );
                let f = &mut m.files[i];
                f.version = version;
                f.sha256 = hashes;
                f.conflict = None;
            }
            Err(err) if is_conflict(&err) => {
                open += 1;
                let (got, theirs) = c
                    .get_versioned(&e.path)
                    .with_context(|| format!("{} changed in the store: read it", e.path))?;
                let copies = write_store_copies(dir, &e, &files, got)?;
                println!(
                    "conflict: {}: your edit is based on {}, the store is at {}; kept {} and \
                     wrote the store's copy to {}. Merge it, delete the {STORE_COPY} file, then \
                     run `axe workspace sync` again",
                    e.path,
                    short(&base),
                    short(&theirs),
                    e.local,
                    if copies.is_empty() {
                        "nothing (only files you have differ)".to_string()
                    } else {
                        copies.join(", ")
                    }
                );
                m.files[i].conflict = Some(theirs);
            }
            Err(err) => return Err(err.context(format!("write {}", e.path))),
        }
        write_manifest(dir, &m)?;
    }
    if open > 0 {
        bail!("{open} conflict(s) open; nothing was overwritten and the folder was not rebuilt");
    }
    if !new.is_empty() {
        eprintln!(
            "axe: not rebuilt: a rebuild would remove the files above; move them out of \
             the folder, then run `axe workspace {}`",
            arg(&m.target)
        );
        return Ok(());
    }
    rebuild(c, cfg, &m.target, dir)?;
    eprintln!("axe: rebuilt {}", dir.display());
    Ok(())
}

/// Refuses a task a live session owns.
fn check_task(t: &crate::db::Task) -> Result<()> {
    if t.state == "in_progress" {
        bail!(
            "task {} is in_progress: a live session owns it and its branch. Hold it first \
             (`axe abort {}` stops the run and holds the task), then try again",
            t.id,
            t.id
        );
    }
    Ok(())
}

fn rebuild(c: &Client, cfg: &Config, target: &Target, dir: &Path) -> Result<()> {
    let built = match target {
        Target::Task(id) => {
            let t: crate::db::Task = c.get(&format!("/api/tasks/{id}"), &[])?;
            check_task(&t)?;
            if t.repos.is_none() {
                eprintln!(
                    "axe: task {id} has no repos, so its runs use the project worktree; \
                     this folder has its context and no repos/"
                );
            }
            let repos = t.repos.clone().unwrap_or_default();
            run_workspace::build_task(cfg, &t, &repos, Some(c), dir, Mode::Local)?
        }
        Target::Bot(name) => {
            let b: crate::bot::Bot = c.get(&format!("/api/bots/{name}"), &[])?;
            let attached = context::resolve(c, &context::bot_scopes(&b.name, &b.project), &[])?;
            let repos = b.repos.clone().unwrap_or_default();
            let built = run_workspace::build_bot_in(
                cfg,
                &b.name,
                b.home(),
                &repos,
                &attached,
                c,
                dir,
                Mode::Local,
            )?;
            for r in &built.refreshed {
                if let crate::worktree::Refresh::Skipped { reason } = &r.refresh {
                    eprintln!("axe: repos/{} not refreshed: {reason}", r.repo);
                }
            }
            std::fs::create_dir_all(dir.join(".axe"))?;
            let system = crate::bot::system_prompt(&b, &context::bot_prompt_parts(&attached))
                + &context::prompt_section(&attached);
            std::fs::write(dir.join(".axe/system.md"), system)?;
            built
        }
    };
    let mut files = Vec::new();
    for s in built.stored {
        let mut e = Entry {
            path: s.path,
            local: s.local.display().to_string(),
            folder: s.folder,
            version: s.version,
            sha256: BTreeMap::new(),
            conflict: None,
        };
        e.sha256 = hash(&read_local(dir, &e)?.context("a file just written is missing")?);
        files.push(e);
    }
    let generated = GENERATED
        .iter()
        .filter_map(|g| {
            let text = std::fs::read(dir.join(g)).ok()?;
            Some((g.to_string(), crate::blob::sha256_hex(&text)))
        })
        .collect();
    write_manifest(
        dir,
        &Manifest {
            target: target.clone(),
            files,
            generated,
        },
    )
}

fn read_manifest(dir: &Path) -> Result<Option<Manifest>> {
    let p = dir.join(MANIFEST);
    match std::fs::read_to_string(&p) {
        Ok(s) => Ok(Some(
            serde_json::from_str(&s).with_context(|| format!("read {}", p.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// Replaces the manifest in one rename, so it is never half written.
fn write_manifest(dir: &Path, m: &Manifest) -> Result<()> {
    let p = dir.join(MANIFEST);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(m)? + "\n")?;
    std::fs::rename(&tmp, &p).with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

/// The local files of `e` (a doc under the empty name), without hidden files
/// and `.store` copies; `None` when it was deleted.
fn read_local(dir: &Path, e: &Entry) -> Result<Option<BTreeMap<String, String>>> {
    let at = dir.join(&e.local);
    if !e.folder {
        return match std::fs::read_to_string(&at) {
            Ok(s) => Ok(Some(BTreeMap::from([(String::new(), s)]))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read {} (docs are UTF-8 text)", e.local)),
        };
    }
    if !at.is_dir() {
        return Ok(None);
    }
    let mut out = BTreeMap::new();
    for (rel, p) in walk(&at)? {
        if rel.ends_with(STORE_COPY) {
            continue;
        }
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("read {} (docs are UTF-8 text)", p.display()))?;
        out.insert(rel, text);
    }
    Ok(Some(out))
}

/// Every regular file under `root` by its `/`-separated relative path,
/// skipping hidden names and links.
fn walk(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let kind = e.file_type()?;
            let p = e.path();
            if kind.is_dir() {
                go(root, &p, out)?;
            } else if kind.is_file() {
                let rel = p.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
                out.push((rel, p));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    go(root, root, &mut out)?;
    Ok(out)
}

fn hash(files: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|(p, c)| (p.clone(), crate::blob::sha256_hex(c.as_bytes())))
        .collect()
}

/// The `.store` copies of `e` that are still there.
fn store_copies(dir: &Path, e: &Entry) -> Result<Vec<String>> {
    if !e.folder {
        let copy = format!("{}{STORE_COPY}", e.local);
        return Ok(if dir.join(&copy).exists() {
            vec![copy]
        } else {
            Vec::new()
        });
    }
    let at = dir.join(&e.local);
    if !at.is_dir() {
        return Ok(Vec::new());
    }
    Ok(walk(&at)?
        .into_iter()
        .filter(|(rel, _)| rel.ends_with(STORE_COPY))
        .map(|(rel, _)| format!("{}/{rel}", e.local))
        .collect())
}

/// Writes the store's copy of each file of `e` that differs from `mine` as
/// `<file>.store`; returns them.
fn write_store_copies(
    dir: &Path,
    e: &Entry,
    mine: &BTreeMap<String, String>,
    theirs: Fetched,
) -> Result<Vec<String>> {
    let theirs: Vec<(String, String)> = match theirs {
        Fetched::Doc(text) => vec![(String::new(), text)],
        Fetched::Folder(files) => files.into_iter().map(|f| (f.path, f.content)).collect(),
    };
    let mut out = Vec::new();
    for (rel, text) in theirs {
        if mine.get(&rel) == Some(&text) {
            continue;
        }
        let local = if rel.is_empty() {
            e.local.clone()
        } else {
            format!("{}/{}", e.local, crate::doc::check_path(&rel)?)
        };
        let copy = format!("{local}{STORE_COPY}");
        let p = dir.join(&copy);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, text).with_context(|| format!("write {}", p.display()))?;
        out.push(copy);
    }
    Ok(out)
}

/// What a rebuild would lose: edited store-backed files, open conflicts and
/// files that are not from the store.
fn local_changes(dir: &Path, m: &Manifest) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in &m.files {
        let copies = store_copies(dir, e)?;
        if e.conflict.is_some() || !copies.is_empty() {
            out.push(format!("conflict open: {}", e.local));
        } else if let Some(files) = read_local(dir, e)? {
            if hash(&files) != e.sha256 {
                out.push(format!("edited: {}", e.local));
            }
        }
    }
    for p in untracked(dir, m)? {
        out.push(format!("not from the store: {p}"));
    }
    Ok(out)
}

/// Files under `docs/store/` and folders under `skills/` that no manifest
/// entry wrote (`.store` copies aside).
fn untracked(dir: &Path, m: &Manifest) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let store = dir.join("docs/store");
    if store.is_dir() {
        for (rel, _) in walk(&store)? {
            let local = format!("docs/store/{rel}");
            let covered = m.files.iter().any(|e| {
                local == e.local
                    || local == format!("{}{STORE_COPY}", e.local)
                    || (e.folder && local.starts_with(&format!("{}/", e.local)))
            });
            if !covered {
                out.push(local);
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir.join("skills")) {
        let mut names: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let local = format!("skills/{}", e.file_name().to_string_lossy());
            let link = e.file_type().is_ok_and(|t| t.is_symlink());
            if !link && !m.files.iter().any(|f| f.local == local) {
                out.push(local);
            }
        }
    }
    Ok(out)
}

/// Says on stderr which generated files were edited: they are never synced.
fn warn_generated(dir: &Path, m: &Manifest) {
    for (g, h) in &m.generated {
        let now = std::fs::read(dir.join(g))
            .ok()
            .map(|t| crate::blob::sha256_hex(&t));
        if now.as_ref() != Some(h) {
            eprintln!(
                "axe: {g} is generated: edits to it are not synced and a rebuild replaces it"
            );
        }
    }
}

fn is_conflict(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ClientError>()
        .is_some_and(|c| c.code == Some(409))
}

fn absolute(d: &Path) -> Result<PathBuf> {
    Ok(if d.is_absolute() {
        d.to_path_buf()
    } else {
        std::env::current_dir()?.join(d)
    })
}

fn short(v: &str) -> &str {
    &v[..v.len().min(12)]
}

fn describe(t: &Target) -> String {
    match t {
        Target::Task(id) => format!("task {id}"),
        Target::Bot(name) => format!("bot {name}"),
    }
}

fn arg(t: &Target) -> String {
    match t {
        Target::Task(id) => id.to_string(),
        Target::Bot(name) => name.clone(),
    }
}
