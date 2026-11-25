//! Run workspaces: the folder every task and bot run works in.
//!
//! ```text
//! <run_root>/task-<id>/      cwd and $AXE_WORKTREE
//!   AGENTS.md                generated: the task contract and an index
//!   skills/                  links to each repo's .agents/skills/<name>, and
//!                            every attached skill's folder from the store
//!                            (context.rs)
//!   docs/index.md            generated: each repo's docs, by title, and the
//!                            attached docs
//!   repos/<name>/            worktree of the repo, on the task branch
//!   .agents/skills, .claude/skills -> ../skills
//! ```
//!
//! Every repo worktree is on the same branch, `<branch_prefix>task-<id>--work`.
//! A task's worktrees are created once and never reset; `AGENTS.md`, `skills/`
//! and `docs/` are generated again at the start of every session. A task with
//! no repos (an empty or absent list) gets the folder without `repos/`;
//! nothing needs a project checkout.
//!
//! A bot gets the same layout at `<run_root>/bot-<name>` ([`build_bot`]), on
//! branch `<branch_prefix>bot-<name>--work`, with the skills attached to the
//! bot's scopes, and also without `repos/` when it has none. A bot's
//! worktrees outlive many runs, so before each run every one
//! is fast-forwarded to its base branch's origin commit when that is safe
//! ([`worktree::refresh`]: clean, no local commits) and left unchanged, with
//! the reason, when it is not.
//!
//! A repo whose canonical clone is missing is cloned first
//! ([`worktree::ensure_clone_from`]) from `[repos.<name>] url`, else from the
//! URL the server's repo registry holds for it (`axe repo add`, `repo.rs`),
//! for a task or a bot alike; the run's `run_start` event lists it in
//! `repos_cloned`.
//!
//! `axe workspace` builds the same folder anywhere, through the same code in
//! [`Mode::Local`] (`workspace.rs`): a missing task branch starts from its
//! `origin` copy when pushed, attached docs are written in full under
//! `docs/store/<path>`, and every file written from the store is returned
//! with its version ([`Stored`]) for the workspace's manifest. The
//! node's folders ([`Mode::Run`]) are unchanged.
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::context::{self, frontmatter};
use crate::db::Task;
use crate::worktree;

/// Most docs listed per repo in `docs/index.md`.
const DOCS_MAX: usize = 200;

/// Builds or refreshes the run workspace of `task` (no repos when it has
/// none), with the docs and skills attached to its scopes from `store` (none when
/// `None` or when the store cannot be read). Also returns the repos whose
/// canonical clone was cloned first ([`worktree::ensure_clone`]). `skills`,
/// when set, is the only skills the folder gets (a routine's profile).
pub fn build(
    cfg: &Config,
    task: &Task,
    store: Option<&dyn context::Source>,
    skills: Option<&[String]>,
) -> Result<(PathBuf, Vec<String>)> {
    let repos = task.repos.as_deref().unwrap_or_default();
    let dir = cfg.run_root().join(format!("task-{}", task.id));
    let out = build_task(cfg, task, repos, store, &dir, Mode::Run, skills)?;
    Ok((dir, out.cloned))
}

/// Who a folder is built for: a node run, or `axe workspace`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Run,
    Local,
}

/// A doc or folder written from the store into a local workspace.
#[derive(Debug, Clone)]
pub struct Stored {
    /// Store path.
    pub path: String,
    /// Where it is, relative to the workspace.
    pub local: PathBuf,
    pub folder: bool,
    /// The version that was written.
    pub version: String,
}

/// What one build did.
#[derive(Debug, Default)]
pub struct Built {
    pub cloned: Vec<String>,
    pub refreshed: Vec<RepoRefresh>,
    pub stored: Vec<Stored>,
}

/// The task layout in `dir`; [`build`] in [`Mode::Run`], `axe workspace`
/// in [`Mode::Local`] (store errors fail the build instead of leaving the
/// context out). `only`, when set, keeps just the skills it names, of the
/// repos' own and the attached ones; the rest are left out of `skills/` and
/// `AGENTS.md`.
pub fn build_task(
    cfg: &Config,
    task: &Task,
    repos: &[String],
    store: Option<&dyn context::Source>,
    dir: &Path,
    mode: Mode,
    only: Option<&[String]>,
) -> Result<Built> {
    let branch = format!("{}task-{}--work", branch_prefix(cfg), task.id);
    let (built, cloned) = checkout(cfg, dir, repos, &branch, mode, store)?;
    let skills = link_skills(dir, &built, only)?;
    let mut attached = match (store, mode) {
        (Some(src), Mode::Run) => {
            context::resolve_or_empty(src, &context::task_scopes(task), &own(&skills))
        }
        (Some(src), Mode::Local) => {
            context::resolve(src, &context::task_scopes(task), &own(&skills))?
        }
        (None, _) => context::Resolution::default(),
    };
    if let Some(only) = only {
        context::only_skills(&mut attached, only);
    }
    let mut stored = Vec::new();
    if let Some(src) = store {
        match mode {
            Mode::Run => context::write_skills(&dir.join("skills"), &attached, src)?,
            Mode::Local => stored = write_skills_local(dir, &attached, src)?,
        }
    }
    let mut docs = docs_index(&built, mode)?;
    docs.push_str(&match mode {
        Mode::Run => context::docs_section(&attached),
        Mode::Local => local_docs_section(&attached),
    });
    finish(
        dir,
        &docs,
        &agents_md(task, &branch, &built, &skills, &attached, mode),
    )?;
    if let (Some(src), Mode::Local) = (store, mode) {
        stored.extend(write_docs_local(dir, &attached, src)?);
    }
    Ok(Built {
        cloned,
        refreshed: Vec::new(),
        stored,
    })
}

/// The run folder of bot `name` with `repos` (maybe none): `<run_root>/bot-<home>`,
/// laid out like a task's, each repo on branch `<branch_prefix>bot-<home>--work`.
/// `home` is the bot's first name (`Bot::home`), so a rename keeps the folder,
/// its worktrees and the agent session that lives there.
/// Worktrees persist across runs and are refreshed before each one
/// ([`worktree::refresh`]); the outcome per repo is returned for the run's
/// start event, with the repos cloned first. The caller guarantees the bot has no live run. The skills
/// attached to the bot's scopes (`attached`, read from `store`) are written
/// into `skills/` like a task's; attached docs and bundles stay in the bot's
/// system prompt.
pub fn build_bot(
    cfg: &Config,
    name: &str,
    home: &str,
    repos: &[String],
    attached: &context::Resolution,
    store: &dyn context::Source,
) -> Result<(PathBuf, Vec<RepoRefresh>, Vec<String>)> {
    let dir = cfg.run_root().join(format!("bot-{home}"));
    let out = build_bot_in(cfg, name, home, repos, attached, store, &dir, Mode::Run)?;
    Ok((dir, out.refreshed, out.cloned))
}

/// The bot layout in `dir`; [`build_bot`] in [`Mode::Run`]. In
/// [`Mode::Local`] the attached docs and bundles are also written in full and
/// indexed, and `AGENTS.md` names the bot's system prompt (`.axe/system.md`,
/// written by `axe workspace`).
#[allow(clippy::too_many_arguments)]
pub fn build_bot_in(
    cfg: &Config,
    name: &str,
    home: &str,
    repos: &[String],
    attached: &context::Resolution,
    store: &dyn context::Source,
    dir: &Path,
    mode: Mode,
) -> Result<Built> {
    let branch = format!("{}bot-{home}--work", branch_prefix(cfg));
    let (built, cloned) = checkout(cfg, dir, repos, &branch, mode, Some(store))?;
    let mut refreshed = Vec::new();
    for (repo, path) in &built {
        let ready = worktree::repo_readiness(cfg, repo).map_err(anyhow::Error::msg)?;
        refreshed.push(RepoRefresh {
            repo: repo.clone(),
            refresh: worktree::refresh(&ready, path),
        });
    }
    let skills = link_skills(dir, &built, None)?;
    let skills_only = context::Resolution {
        scopes: attached.scopes.clone(),
        items: attached
            .items
            .iter()
            // A repo's own skill keeps its name, as in a task's run.
            .filter(|i| i.kind == context::Kind::Skill && !skills.iter().any(|k| k.name == i.name))
            .cloned()
            .collect(),
    };
    let mut stored = Vec::new();
    let mut docs = docs_index(&built, mode)?;
    match mode {
        Mode::Run => context::write_skills(&dir.join("skills"), &skills_only, store)?,
        Mode::Local => {
            stored = write_skills_local(dir, &skills_only, store)?;
            docs.push_str(&local_docs_section(attached));
        }
    }
    finish(
        dir,
        &docs,
        &bot_agents_md(name, &branch, &built, &skills, &skills_only, mode),
    )?;
    if mode == Mode::Local {
        stored.extend(write_docs_local(dir, attached, store)?);
    }
    Ok(Built {
        cloned,
        refreshed,
        stored,
    })
}

/// Writes each included attached skill like [`context::write_skills`],
/// returning those read from the store with their versions. A skill that
/// cannot be read fails the build: a local workspace is not a run that must
/// start anyway.
fn write_skills_local(
    dir: &Path,
    res: &context::Resolution,
    source: &dyn context::Source,
) -> Result<Vec<Stored>> {
    let mut out = Vec::new();
    for s in res.attached(context::Kind::Skill) {
        let local = Path::new("skills").join(&s.name);
        let at = dir.join(&local);
        if std::fs::symlink_metadata(&at).is_ok() {
            continue;
        }
        let (got, version) = source
            .get_versioned(&s.path)
            .with_context(|| format!("read skill {}", s.path))?;
        let context::Fetched::Folder(files) = got else {
            bail!("skill {} is a doc, not a folder", s.path);
        };
        std::fs::create_dir_all(&at)?;
        for f in &files {
            context::write_file(&at, f)?;
        }
        out.push(Stored {
            path: s.path.clone(),
            local,
            folder: true,
            version,
        });
    }
    Ok(out)
}

/// Writes each included attached doc and bundle in full under
/// `docs/store/<path>`.
fn write_docs_local(
    dir: &Path,
    res: &context::Resolution,
    source: &dyn context::Source,
) -> Result<Vec<Stored>> {
    let mut out = Vec::new();
    for i in res
        .attached(context::Kind::Doc)
        .chain(res.attached(context::Kind::Bundle))
    {
        let local = Path::new("docs/store").join(crate::doc::check_path(&i.path)?);
        let at = dir.join(&local);
        let (got, version) = source
            .get_versioned(&i.path)
            .with_context(|| format!("read doc {}", i.path))?;
        let folder = match got {
            context::Fetched::Doc(text) => {
                std::fs::create_dir_all(at.parent().context("doc path has no parent")?)?;
                std::fs::write(&at, text).with_context(|| format!("write {}", at.display()))?;
                false
            }
            context::Fetched::Folder(files) => {
                std::fs::create_dir_all(&at)?;
                for f in &files {
                    context::write_file(&at, f)?;
                }
                true
            }
        };
        out.push(Stored {
            path: i.path.clone(),
            local,
            folder,
            version,
        });
    }
    Ok(out)
}

/// `docs/index.md`'s section for the attached docs a local workspace holds
/// in full; empty when none.
fn local_docs_section(res: &context::Resolution) -> String {
    let mut s = String::new();
    let items: Vec<&context::Item> = res
        .attached(context::Kind::Doc)
        .chain(res.attached(context::Kind::Bundle))
        .collect();
    if items.is_empty() {
        return s;
    }
    s.push_str(
        "\n## Attached\n\nDocs attached to this workspace's scopes, written in full from the axe \
         doc store under `docs/store/`. Edit one, then run `axe workspace sync` to write it back.\n\n",
    );
    for i in items {
        let slash = if i.kind == context::Kind::Bundle {
            "/"
        } else {
            ""
        };
        let _ = writeln!(
            s,
            "- {} [`{}`](store/{}{slash}) ({}): {}",
            i.kind,
            i.path,
            i.path.replace(' ', "%20"),
            i.scope,
            if i.description.is_empty() {
                "-"
            } else {
                &i.description
            }
        );
    }
    s
}

/// What [`build_bot`] did to one repo's worktree before the run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoRefresh {
    pub repo: String,
    #[serde(flatten)]
    pub refresh: worktree::Refresh,
}

fn branch_prefix(cfg: &Config) -> String {
    cfg.worktrees.clone().unwrap_or_default().branch_prefix
}

/// Each repo's name and worktree path, in the task's order.
type Repos = Vec<(String, PathBuf)>;

/// Creates `dir` and a worktree of each repo under `dir/repos/<name>` on
/// `branch`, first cloning a missing canonical clone from its
/// [`worktree::clone_url`] (`[repos.<name>] url`, else the server's repo
/// registry through `store`); also returns the repos it cloned.
fn checkout(
    cfg: &Config,
    dir: &Path,
    repos: &[String],
    branch: &str,
    mode: Mode,
    store: Option<&dyn context::Source>,
) -> Result<(Repos, Vec<String>)> {
    match std::fs::symlink_metadata(dir) {
        Ok(m) if !m.is_dir() => bail!("{} exists and is not a directory", dir.display()),
        _ => {}
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut built = Vec::new();
    let mut cloned = Vec::new();
    for name in repos {
        crate::db::validate_name("repo", name)?;
        let url = worktree::clone_url(cfg, name, &|n| store.map_or(Ok(None), |s| s.repo_url(n)));
        if worktree::ensure_clone_from(cfg, name, url.url())? {
            eprintln!(
                "{}: cloned repo {name} into {}",
                match mode {
                    Mode::Run => "axe node",
                    Mode::Local => "axe",
                },
                cfg.repo_path(name).display()
            );
            cloned.push(name.clone());
        }
        let ready = worktree::repo_readiness_with(cfg, name, &url).map_err(anyhow::Error::msg)?;
        let path = worktree::allocate_from(
            &ready,
            &dir.join("repos").join(name),
            branch,
            mode == Mode::Local,
        )?;
        built.push((name.clone(), path));
    }
    Ok((built, cloned))
}

/// Writes `docs/index.md`, the `.agents`/`.claude` skill links and `AGENTS.md`.
fn finish(dir: &Path, docs: &str, agents: &str) -> Result<()> {
    replace_dir(&dir.join("docs"))?;
    std::fs::write(dir.join("docs/index.md"), docs)?;
    for agent_dir in [".agents", ".claude"] {
        let link = dir.join(agent_dir).join("skills");
        std::fs::create_dir_all(dir.join(agent_dir))?;
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("../skills", &link)?;
        }
    }
    std::fs::write(dir.join("AGENTS.md"), agents)?;
    Ok(())
}

/// Removes a generated directory (only links and generated files) and makes it
/// empty again; refuses a symlink in its place.
fn replace_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            bail!("{} is not a generated directory", path.display())
        }
        Ok(_) => std::fs::remove_dir_all(path)?,
        Err(_) => {}
    }
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// A skill found in a repo: its name, description and repo.
struct Skill {
    name: String,
    description: String,
    repo: String,
}

/// Links every `repos/<r>/.agents/skills/<name>` with a `SKILL.md` as
/// `skills/<name>`; the first repo in the task's order wins a name. With
/// `only`, just the skills it names.
fn link_skills(
    dir: &Path,
    repos: &[(String, PathBuf)],
    only: Option<&[String]>,
) -> Result<Vec<Skill>> {
    let skills_dir = dir.join("skills");
    replace_dir(&skills_dir)?;
    let mut skills = repo_skills(repos);
    if let Some(only) = only {
        skills.retain(|s| only.contains(&s.name));
    }
    for s in &skills {
        std::os::unix::fs::symlink(
            Path::new("../repos")
                .join(&s.repo)
                .join(".agents/skills")
                .join(&s.name),
            skills_dir.join(&s.name),
        )?;
    }
    Ok(skills)
}

/// The skills in each repo's `.agents/skills` (a folder with `SKILL.md`);
/// the first repo in order wins a name.
fn repo_skills(repos: &[(String, PathBuf)]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for (repo, path) in repos {
        let Ok(entries) = std::fs::read_dir(path.join(".agents/skills")) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(e.path().join("SKILL.md")) else {
                continue;
            };
            if name.starts_with('.') || out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Skill {
                description: frontmatter(&text, "description").unwrap_or_default(),
                name,
                repo: repo.clone(),
            });
        }
    }
    out
}

fn own(skills: &[Skill]) -> Vec<context::OwnSkill> {
    skills
        .iter()
        .map(|s| context::OwnSkill {
            name: s.name.clone(),
            repo: s.repo.clone(),
            description: s.description.clone(),
        })
        .collect()
}

/// What `axe context explain` resolves for `task`: its attached docs, and the
/// repos' own skills when its run workspace is on this machine.
pub fn explain(
    cfg: Option<&Config>,
    task: &Task,
    store: &dyn context::Source,
) -> Result<context::Resolution> {
    let skills = match (cfg, task.repos.as_deref()) {
        (Some(cfg), Some(repos)) => {
            let dir = cfg.run_root().join(format!("task-{}", task.id));
            let repos: Vec<(String, PathBuf)> = repos
                .iter()
                .map(|r| (r.clone(), dir.join("repos").join(r)))
                .collect();
            repo_skills(&repos)
        }
        _ => Vec::new(),
    };
    context::resolve(store, &context::task_scopes(task), &own(&skills))
}

/// `docs/index.md`: every Markdown file under each repo's `docs/` (two levels),
/// by its first heading.
fn docs_index(repos: &[(String, PathBuf)], mode: Mode) -> Result<String> {
    let mut s = String::from(match mode {
        Mode::Run => "# Docs\n\nGenerated by axe for this run; edits here are lost.\n",
        Mode::Local => {
            "# Docs\n\nGenerated by `axe workspace`; edits here are lost when it rebuilds.\n"
        }
    });
    for (repo, path) in repos {
        let mut files = Vec::new();
        markdown_files(&path.join("docs"), 2, &mut files);
        if files.is_empty() {
            continue;
        }
        files.sort();
        let _ = write!(s, "\n## {repo}\n\n");
        for f in files.iter().take(DOCS_MAX) {
            let rel = f.strip_prefix(path).unwrap_or(f);
            let title = std::fs::read_to_string(f)
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find_map(|l| l.strip_prefix("# ").map(|h| h.trim().to_string()))
                })
                .unwrap_or_else(|| rel.display().to_string());
            let _ = writeln!(
                s,
                "- [{title}](../repos/{repo}/{})",
                rel.display().to_string().replace(' ', "%20")
            );
        }
        if files.len() > DOCS_MAX {
            let _ = writeln!(s, "- ... {} more", files.len() - DOCS_MAX);
        }
    }
    Ok(s)
}

fn markdown_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if kind.is_dir() && depth > 1 {
            markdown_files(&path, depth - 1, out);
        } else if kind.is_file() && path.extension().is_some_and(|x| x == "md") {
            out.push(path);
        }
    }
}

/// The generated `AGENTS.md`: the task contract, then where things are.
fn agents_md(
    task: &Task,
    branch: &str,
    repos: &[(String, PathBuf)],
    skills: &[Skill],
    attached: &context::Resolution,
    mode: Mode,
) -> String {
    let mut s = format!(
        "# Task {}: {}\n\n{}\n\n",
        task.id,
        task.title,
        match mode {
            Mode::Run => "Generated by axe at the start of every session; edits here are lost.",
            Mode::Local => LOCAL_NOTE,
        }
    );
    let _ = writeln!(s, "- Owner: {}", task.owner.as_deref().unwrap_or("-"));
    let mut labels = vec![task.project.clone()];
    labels.extend(task.labels.iter().filter(|l| **l != task.project).cloned());
    let _ = writeln!(s, "- Labels: {}", labels.join(", "));
    if repos.is_empty() {
        s.push_str("- Repos: none\n");
    } else {
        let _ = writeln!(
            s,
            "- Repos: {} (each a worktree under `repos/`, on branch `{branch}`)",
            repos
                .iter()
                .map(|(r, _)| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    s.push_str("\n## Spec\n\n");
    s.push_str(task.detail.trim_end());
    s.push_str("\n\n");
    s.push_str(&index_md(
        repos,
        skills,
        attached,
        "This task has no repos.\n",
    ));
    if !attached.is_empty() {
        let _ = writeln!(
            s,
            "\n{} `axe context explain {}` shows what was included and why.",
            match mode {
                Mode::Run =>
                    "Attached skills are written in full under `skills/`; attached docs are \
                     index entries: load a body with `axe doc get <path>`.",
                Mode::Local => LOCAL_ATTACHED,
            },
            task.id
        );
    }
    s
}

/// How a local workspace's `AGENTS.md` describes itself.
const LOCAL_NOTE: &str = "Generated by `axe workspace`; edits here are lost when it rebuilds.";

/// How a local workspace's `AGENTS.md` describes its attached files.
const LOCAL_ATTACHED: &str = "Attached skills are written in full under `skills/` and attached \
     docs under `docs/store/`; after editing them, `axe workspace sync` writes them back to the \
     store.";

/// The generated `AGENTS.md` of a bot's run folder: where things are. The
/// bot's identity and conduct stay in its system prompt.
fn bot_agents_md(
    name: &str,
    branch: &str,
    repos: &[(String, PathBuf)],
    skills: &[Skill],
    attached: &context::Resolution,
    mode: Mode,
) -> String {
    let mut s = match mode {
        Mode::Run => format!(
            "# Bot {name}: run folder\n\nGenerated by axe at the start of every run; edits here are lost. \
             Your identity and conduct are in your system prompt.\n\n"
        ),
        Mode::Local => format!(
            "# Bot {name}: local workspace\n\n{LOCAL_NOTE} The bot's system prompt (protocol, \
             identity, conduct) is in `.axe/system.md`, for reference: you work here as yourself, \
             not as the bot. {LOCAL_ATTACHED}\n\n"
        ),
    };
    if repos.is_empty() {
        s.push_str("- Repos: none\n\n");
    } else {
        let _ = writeln!(
            s,
            "- Repos: {} (each a worktree under `repos/`, on branch `{branch}`)\n",
            repos
                .iter()
                .map(|(r, _)| r.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    s.push_str(&index_md(repos, skills, attached, ""));
    s
}

/// The `## Repos`, `## Skills` and `## Docs` sections.
fn index_md(
    repos: &[(String, PathBuf)],
    skills: &[Skill],
    attached: &context::Resolution,
    no_repos: &str,
) -> String {
    let mut s = String::from("## Repos\n\n");
    if repos.is_empty() {
        s.push_str(no_repos);
    }
    for (repo, path) in repos {
        let _ = write!(s, "- `repos/{repo}`");
        if path.join("AGENTS.md").is_file() {
            let _ = write!(s, ": its rules are in `repos/{repo}/AGENTS.md`");
        }
        s.push('\n');
    }
    s.push_str("\n## Skills\n\n");
    let attached_skills = context::skills_lines(attached, "skills/");
    if skills.is_empty() && attached_skills.is_empty() {
        s.push_str("None.\n");
    }
    for k in skills {
        let _ = writeln!(
            s,
            "- `skills/{}` ({}): {}",
            k.name,
            k.repo,
            if k.description.is_empty() {
                "-"
            } else {
                &k.description
            }
        );
    }
    s.push_str(&attached_skills);
    s.push_str("\n## Docs\n\nSee `docs/index.md`.\n");
    s
}
