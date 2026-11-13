//! Run workspaces: the folder a task with its own `repos` runs in.
//!
//! ```text
//! <run_root>/task-<id>/      cwd and $AXE_WORKTREE
//!   AGENTS.md                generated: the task contract and an index
//!   skills/                  links to each repo's .agents/skills/<name>
//!   docs/index.md            generated: each repo's docs, by title
//!   repos/<name>/            worktree of the repo, on the task branch
//!   .agents/skills, .claude/skills -> ../skills
//! ```
//!
//! Every repo worktree is on the same branch, `<branch_prefix>task-<id>--work`.
//! Worktrees are created once and never reset; `AGENTS.md`, `skills/` and
//! `docs/` are generated again at the start of every session. A task with an
//! empty repo list gets the folder without `repos/`. Tasks without `repos`
//! keep the project worktree (`worktree.rs`).
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::config::Config;
use crate::db::Task;
use crate::worktree;

/// Most docs listed per repo in `docs/index.md`.
const DOCS_MAX: usize = 200;

/// Builds or refreshes the run workspace of `task`, which must have `repos`.
pub fn build(cfg: &Config, task: &Task) -> Result<PathBuf> {
    let repos = task
        .repos
        .as_deref()
        .context("task has no repos; it runs in its project worktree")?;
    let dir = cfg.run_root().join(format!("task-{}", task.id));
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if !m.is_dir() => bail!("{} exists and is not a directory", dir.display()),
        _ => {}
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let prefix = cfg.worktrees.clone().unwrap_or_default().branch_prefix;
    let branch = format!("{prefix}task-{}--work", task.id);
    let mut built = Vec::new();
    for name in repos {
        crate::db::validate_name("repo", name)?;
        let ready = worktree::repo_readiness(cfg, name).map_err(anyhow::Error::msg)?;
        let path = worktree::allocate_at(&ready, &dir.join("repos").join(name), &branch)?;
        built.push((name.clone(), path));
    }
    let skills = link_skills(&dir, &built)?;
    let docs = docs_index(&built)?;
    replace_dir(&dir.join("docs"))?;
    std::fs::write(dir.join("docs/index.md"), docs)?;
    for agent_dir in [".agents", ".claude"] {
        let link = dir.join(agent_dir).join("skills");
        std::fs::create_dir_all(dir.join(agent_dir))?;
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("../skills", &link)?;
        }
    }
    std::fs::write(
        dir.join("AGENTS.md"),
        agents_md(task, &branch, &built, &skills),
    )?;
    Ok(dir)
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
/// `skills/<name>`; the first repo in the task's order wins a name.
fn link_skills(dir: &Path, repos: &[(String, PathBuf)]) -> Result<Vec<Skill>> {
    let skills_dir = dir.join("skills");
    replace_dir(&skills_dir)?;
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
            std::os::unix::fs::symlink(
                Path::new("../repos")
                    .join(repo)
                    .join(".agents/skills")
                    .join(&name),
                skills_dir.join(&name),
            )?;
            out.push(Skill {
                description: frontmatter(&text, "description").unwrap_or_default(),
                name,
                repo: repo.clone(),
            });
        }
    }
    Ok(out)
}

/// A `key: value` line of a Markdown file's YAML front matter.
fn frontmatter(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---\n")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|l| {
        l.strip_prefix(key)?
            .strip_prefix(':')
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// `docs/index.md`: every Markdown file under each repo's `docs/` (two levels),
/// by its first heading.
fn docs_index(repos: &[(String, PathBuf)]) -> Result<String> {
    let mut s = String::from("# Docs\n\nGenerated by axe for this run; edits here are lost.\n");
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
fn agents_md(task: &Task, branch: &str, repos: &[(String, PathBuf)], skills: &[Skill]) -> String {
    let mut s = format!(
        "# Task {}: {}\n\nGenerated by axe at the start of every session; edits here are lost.\n\n",
        task.id, task.title
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
    s.push_str("\n\n## Repos\n\n");
    if repos.is_empty() {
        s.push_str("This task has no repos.\n");
    }
    for (repo, path) in repos {
        let _ = write!(s, "- `repos/{repo}`");
        if path.join("AGENTS.md").is_file() {
            let _ = write!(s, ": its rules are in `repos/{repo}/AGENTS.md`");
        }
        s.push('\n');
    }
    s.push_str("\n## Skills\n\n");
    if skills.is_empty() {
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
    s.push_str("\n## Docs\n\nSee `docs/index.md`.\n");
    s
}
