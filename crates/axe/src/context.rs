//! Run context: the docs and skills attached to a run's scopes, resolved into
//! one index.
//!
//! A run's scopes, most specific first, are each of its repos (in the task's
//! order), its owner, each label (the project first) and `global`. Every doc
//! or folder attached to one of them (`doc.rs`) is a candidate:
//!
//! - a folder with a `SKILL.md` is a skill, named by its last path segment;
//! - any other folder is a bundle, and a single file is a doc.
//!
//! A bundle may hold a `prompt.md`: text a bot carries in its system prompt
//! (the bot protocol, a channel protocol, a role). For a bot run, the prompt
//! parts of its included bundles are inlined before its identity, ordered by
//! the `order:` front matter of each `prompt.md` (default 0), then path
//! ([`bot_prompt_parts`]); the bundle is then not listed again in the index.
//! The front matter `description:` describes the bundle. Task runs list
//! bundles like any other.
//!
//! Two candidates conflict when they would take the same place in the run: a
//! skill name, or the same path attached to more than one scope. One wins and
//! the others are shadowed, in this order:
//!
//! 1. policy: a path under `policy/` is never overridden, whatever its scope;
//! 2. a repo's own skills (`.agents/skills` in its worktree): attached docs
//!    never override a repo's rules;
//! 3. the more specific scope: repo > owner > label > global.
//!
//! Only names and descriptions are written into the run; bodies stay in the
//! store and load when used (`axe doc get <path>`). A run with nothing
//! attached is built exactly as before. `axe context explain <task>` prints
//! the resolution.
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::Value;

use crate::client::Client;
use crate::doc::{self, Attachment, Scope};

/// The file of a bundle that a bot inlines into its system prompt.
pub const BUNDLE_PROMPT: &str = "prompt.md";

/// What a run's context is read from: the server (`Client`) or a store.
pub trait Source {
    /// Every attachment of every scope.
    fn attachments(&self) -> Result<Vec<Attachment>>;
    /// The current doc or folder at `path`.
    fn get(&self, path: &str) -> Result<Fetched>;
}

/// A doc's content, or a folder's files.
pub enum Fetched {
    Doc(String),
    Folder(Vec<doc::File>),
}

impl Source for Client {
    fn attachments(&self) -> Result<Vec<Attachment>> {
        self.get_private("/api/docs/attachments", &[])
    }
    fn get(&self, path: &str) -> Result<Fetched> {
        let v: Value = self.get_private("/api/docs/file", &[("path", path)])?;
        fetched(v)
    }
}

impl Source for doc::Store {
    fn attachments(&self) -> Result<Vec<Attachment>> {
        doc::Store::attachments(self, None)
    }
    fn get(&self, path: &str) -> Result<Fetched> {
        fetched(serde_json::to_value(doc::Store::get(self, path, None)?)?)
    }
}

fn fetched(v: Value) -> Result<Fetched> {
    if let Some(c) = v["content"].as_str() {
        return Ok(Fetched::Doc(c.to_string()));
    }
    let files = v
        .get("files")
        .cloned()
        .context("doc has neither content nor files")?;
    Ok(Fetched::Folder(serde_json::from_value(files)?))
}

/// The scopes of a task, most specific first.
pub fn task_scopes(task: &crate::db::Task) -> Vec<Scope> {
    let mut labels = vec![task.project.clone()];
    labels.extend(task.labels.iter().filter(|l| **l != task.project).cloned());
    scopes(
        task.repos.as_deref().unwrap_or_default(),
        task.owner.as_deref(),
        &labels,
    )
}

/// The scopes of a bot's runs: the bot as owner (`bot:<name>`), its project
/// as a label, and global.
pub fn bot_scopes(name: &str, project: &str) -> Vec<Scope> {
    scopes(&[], Some(&format!("bot:{name}")), &[project.to_string()])
}

fn scopes(repos: &[String], owner: Option<&str>, labels: &[String]) -> Vec<Scope> {
    let mut out = Vec::new();
    let names = repos
        .iter()
        .map(|r| format!("repo:{r}"))
        .chain(owner.map(|o| format!("owner:{o}")))
        .chain(labels.iter().map(|l| format!("label:{l}")))
        .chain(["global".to_string()]);
    for n in names {
        // A name the store cannot hold has nothing attached.
        if let Ok(s) = Scope::parse(&n) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Skill,
    Bundle,
    Doc,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Kind::Skill => "skill",
            Kind::Bundle => "bundle",
            Kind::Doc => "doc",
        })
    }
}

/// One candidate and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub kind: Kind,
    /// The skill name, or the path of a doc or bundle.
    pub name: String,
    /// Store path; for a repo's own skill, its path in the run folder.
    pub path: String,
    pub scope: String,
    pub description: String,
    /// A repo's own skill, already in the run through its worktree.
    pub own: bool,
    pub included: bool,
    pub reason: String,
    /// A bundle's prompt part (its `prompt.md` without front matter) and
    /// order; bots inline it.
    #[serde(skip)]
    pub prompt: Option<(i64, String)>,
}

/// A repo's own skill (`repos/<repo>/.agents/skills/<name>`).
pub struct OwnSkill {
    pub name: String,
    pub repo: String,
    pub description: String,
}

/// Every candidate, included ones first in precedence order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Resolution {
    pub scopes: Vec<String>,
    pub items: Vec<Item>,
}

impl Resolution {
    /// Included attached items (not a repo's own skills) of `kind`.
    pub fn attached(&self, kind: Kind) -> impl Iterator<Item = &Item> {
        self.items
            .iter()
            .filter(move |i| i.included && !i.own && i.kind == kind)
    }

    /// True when nothing attached made it into the run.
    pub fn is_empty(&self) -> bool {
        !self.items.iter().any(|i| i.included && !i.own)
    }
}

/// Collects what is attached to `scopes` and resolves conflicts (module docs).
pub fn resolve(source: &dyn Source, scopes: &[Scope], own: &[OwnSkill]) -> Result<Resolution> {
    let rank: BTreeMap<String, usize> = scopes
        .iter()
        .enumerate()
        .map(|(i, s)| (s.to_string(), i))
        .collect();
    let mut attached: Vec<(usize, Attachment)> = source
        .attachments()?
        .into_iter()
        .filter_map(|a| rank.get(&a.scope).map(|r| (*r, a)))
        .collect();
    attached.sort_by(|a, b| (a.0, &a.1.path).cmp(&(b.0, &b.1.path)));

    // (class, scope rank, item); class 0 = policy, 1 = a repo's own, 2 = other.
    let mut candidates: Vec<(u8, usize, Item)> = Vec::new();
    let mut skipped = Vec::new();
    for s in own {
        let scope = format!("repo:{}", s.repo);
        candidates.push((
            1,
            rank.get(&scope).copied().unwrap_or(0),
            Item {
                kind: Kind::Skill,
                name: s.name.clone(),
                path: format!("repos/{}/.agents/skills/{}", s.repo, s.name),
                scope,
                description: s.description.clone(),
                own: true,
                included: false,
                reason: String::new(),
                prompt: None,
            },
        ));
    }
    let mut cache: BTreeMap<String, Result<Described, String>> = BTreeMap::new();
    for (r, a) in attached {
        let mut item = Item {
            kind: Kind::Doc,
            name: a.path.clone(),
            path: a.path.clone(),
            scope: a.scope.clone(),
            description: String::new(),
            own: false,
            included: false,
            reason: String::new(),
            prompt: None,
        };
        if !a.exists {
            item.reason = format!("attached to {} but no longer exists", a.scope);
            skipped.push(item);
            continue;
        }
        let got = cache
            .entry(a.path.clone())
            .or_insert_with(|| describe(source, &a.path).map_err(|e| format!("{e:#}")));
        match got {
            Ok((kind, description, prompt)) => {
                item.kind = *kind;
                item.description = description.clone();
                item.prompt = prompt.clone();
                if *kind == Kind::Skill {
                    item.name = a.path.rsplit('/').next().unwrap_or(&a.path).to_string();
                }
            }
            Err(e) => {
                item.reason = format!("attached to {} but could not be read: {e}", a.scope);
                skipped.push(item);
                continue;
            }
        }
        let class = if a.path.starts_with("policy/") { 0 } else { 2 };
        candidates.push((class, r, item));
    }

    candidates.sort_by_key(|a| (a.0, a.1));
    let mut winners: BTreeMap<String, (u8, String, String)> = BTreeMap::new();
    let mut items = Vec::new();
    let mut shadowed = Vec::new();
    for (class, _, mut item) in candidates {
        let key = match item.kind {
            Kind::Skill => format!("skill:{}", item.name),
            _ => format!("path:{}", item.path),
        };
        match winners.get(&key) {
            None => {
                item.included = true;
                item.reason = match class {
                    0 => format!("attached to {} (policy: not overridable)", item.scope),
                    1 => format!("{}'s own skill (not overridable)", item.scope),
                    _ => format!("attached to {}", item.scope),
                };
                winners.insert(key, (class, item.scope.clone(), item.path.clone()));
                items.push(item);
            }
            Some((wclass, wscope, wpath)) => {
                let why = match wclass {
                    0 => "policy is not overridable",
                    1 => "a repo's own skills are not overridable",
                    _ => "the more specific scope wins",
                };
                item.reason = if *wpath == item.path {
                    format!("also attached to {wscope}, which wins: {why}")
                } else {
                    format!("shadowed by {wpath} from {wscope}: {why}")
                };
                shadowed.push(item);
            }
        }
    }
    items.extend(shadowed);
    items.extend(skipped);
    Ok(Resolution {
        scopes: scopes.iter().map(Scope::to_string).collect(),
        items,
    })
}

/// Kind, description and, for a bundle with a `prompt.md`, its prompt part.
type Described = (Kind, String, Option<(i64, String)>);

/// The kind and description of a stored doc or folder. A doc's description
/// is its front matter `description`, else its first heading; a skill's is
/// its `SKILL.md` front matter; a bundle's is its `prompt.md` front matter,
/// else its file count.
fn describe(source: &dyn Source, path: &str) -> Result<Described> {
    Ok(match source.get(path)? {
        Fetched::Doc(text) => (Kind::Doc, doc_description(&text), None),
        Fetched::Folder(files) => {
            if let Some(f) = files.iter().find(|f| f.path == "SKILL.md") {
                (
                    Kind::Skill,
                    frontmatter(&f.content, "description").unwrap_or_default(),
                    None,
                )
            } else if let Some(f) = files.iter().find(|f| f.path == BUNDLE_PROMPT) {
                let order = match frontmatter(&f.content, "order") {
                    None => 0,
                    Some(o) => o.parse().with_context(|| {
                        format!("{path}/{BUNDLE_PROMPT}: order {o:?} is not an integer")
                    })?,
                };
                (
                    Kind::Bundle,
                    frontmatter(&f.content, "description")
                        .unwrap_or_else(|| format!("{} files", files.len())),
                    Some((order, strip_frontmatter(&f.content).to_string())),
                )
            } else {
                (Kind::Bundle, format!("{} files", files.len()), None)
            }
        }
    })
}

/// A Markdown file's text after its YAML front matter, if any.
pub fn strip_frontmatter(text: &str) -> &str {
    text.strip_prefix("---\n")
        .and_then(|body| {
            let end = body.find("\n---\n")?;
            Some(&body[end + "\n---\n".len()..])
        })
        .unwrap_or(text)
}

/// The prompt parts of the included attached bundles, in order (`order:`,
/// then path): what a bot inlines into its system prompt.
pub fn bot_prompt_parts(res: &Resolution) -> Vec<String> {
    let mut parts: Vec<(i64, &str, &str)> = res
        .attached(Kind::Bundle)
        .filter_map(|i| {
            i.prompt
                .as_ref()
                .map(|(o, t)| (*o, i.path.as_str(), t.as_str()))
        })
        .collect();
    parts.sort();
    parts.into_iter().map(|(_, _, t)| t.to_string()).collect()
}

fn doc_description(text: &str) -> String {
    frontmatter(text, "description")
        .or_else(|| {
            text.lines()
                .find_map(|l| l.strip_prefix("# ").map(|h| h.trim().to_string()))
        })
        .unwrap_or_default()
}

/// A `key: value` line of a Markdown file's YAML front matter.
pub fn frontmatter(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---\n")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|l| {
        l.strip_prefix(key)?
            .strip_prefix(':')
            .map(|v| v.trim().trim_matches('"').to_string())
    })
}

/// Writes each included attached skill as `skills_dir/<name>/SKILL.md`: its
/// name and description, and how to load the body. Never replaces an
/// existing entry (a repo's own skill).
pub fn write_skills(skills_dir: &Path, res: &Resolution) -> Result<()> {
    for s in res.attached(Kind::Skill) {
        let dir = skills_dir.join(&s.name);
        if std::fs::symlink_metadata(&dir).is_ok() {
            continue;
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), skill_stub(s))?;
    }
    Ok(())
}

fn skill_stub(s: &Item) -> String {
    // A JSON string is a valid YAML double-quoted scalar.
    let q = |v: &str| serde_json::to_string(v).unwrap_or_default();
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n# {}\n\nAttached by axe ({}). This is an index entry: \
         the skill is kept in the axe doc store. Before using it, load it with \
         `axe doc get {}` (every file of the skill), or \
         `axe doc get {} --output <dir>` to write it to a directory.\n",
        q(&s.name),
        q(&s.description),
        s.name,
        s.scope,
        s.path,
        s.path,
    )
}

/// Index lines for included attached docs and bundles; empty when none.
pub fn docs_section(res: &Resolution) -> String {
    let mut s = String::new();
    let items: Vec<&Item> = res
        .attached(Kind::Doc)
        .chain(res.attached(Kind::Bundle))
        .collect();
    if items.is_empty() {
        return s;
    }
    s.push_str(
        "\n## Attached\n\nDocs attached to this run's scopes, kept in the axe doc store. \
         Read one with `axe doc get <path>`.\n\n",
    );
    for i in items {
        let _ = writeln!(
            s,
            "- {} `{}` ({}): {}",
            i.kind,
            i.path,
            i.scope,
            dash(&i.description)
        );
    }
    s
}

/// Index lines for included attached skills; empty when none.
pub fn skills_lines(res: &Resolution, prefix: &str) -> String {
    let mut s = String::new();
    for k in res.attached(Kind::Skill) {
        let _ = writeln!(
            s,
            "- `{prefix}{}` ({}, attached): {}",
            k.name,
            k.scope,
            dash(&k.description)
        );
    }
    s
}

/// A prompt section listing what is attached, for runs without a run folder
/// (bots); empty when nothing is. Bundles whose prompt part the bot inlines
/// ([`bot_prompt_parts`]) are not listed again.
pub fn prompt_section(res: &Resolution) -> String {
    let inlined = |i: &Item| i.included && !i.own && i.prompt.is_some();
    if res.items.iter().all(|i| !i.included || i.own || inlined(i)) {
        return String::new();
    }
    let listed = Resolution {
        scopes: res.scopes.clone(),
        items: res.items.iter().filter(|i| !inlined(i)).cloned().collect(),
    };
    let res = &listed;
    let mut s = String::from(
        "\n# Attached knowledge\n\nGenerated by axe from the docs attached to this run's scopes. \
         Bodies stay in the axe doc store: load one with `axe doc get <path>` when you need it.\n",
    );
    let skills: String = res
        .attached(Kind::Skill)
        .map(|k| format!("- `{}` ({}): {}\n", k.path, k.scope, dash(&k.description)))
        .collect();
    if !skills.is_empty() {
        let _ = write!(s, "\n## Skills\n\n{skills}");
    }
    s.push_str(
        docs_section(res)
            .replace("\n## Attached\n", "\n## Docs\n")
            .as_str(),
    );
    s
}

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

/// Resolves a run's context, or an empty one when the store cannot be read
/// (an older or doc-less server): a run never fails for its context.
pub fn resolve_or_empty(source: &dyn Source, scopes: &[Scope], own: &[OwnSkill]) -> Resolution {
    resolve(source, scopes, own).unwrap_or_else(|e| {
        eprintln!("axe: run context skipped: {e:#}");
        Resolution::default()
    })
}

/// `axe context explain` text: the scopes, then each item and why.
pub fn explain(res: &Resolution) -> String {
    let mut s = format!("scopes (most specific first): {}\n", res.scopes.join(", "));
    if res.items.is_empty() {
        s.push_str("nothing attached\n");
        return s;
    }
    let rows: Vec<[String; 5]> = res
        .items
        .iter()
        .map(|i| {
            [
                if i.included { "included" } else { "skipped" }.to_string(),
                i.kind.to_string(),
                i.path.clone(),
                i.scope.clone(),
                i.reason.clone(),
            ]
        })
        .collect();
    let mut w = [0usize; 4];
    for r in &rows {
        for (k, width) in w.iter_mut().enumerate() {
            *width = (*width).max(r[k].len());
        }
    }
    for r in rows {
        let _ = writeln!(
            s,
            "{:w0$}  {:w1$}  {:w2$}  {:w3$}  {}",
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3]
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Body, File};

    fn store() -> (tempfile::TempDir, doc::Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = doc::Store::open(&Default::default(), &dir.path().join("axe.db")).unwrap();
        (dir, s)
    }

    fn skill(s: &doc::Store, path: &str, desc: &str, body: &str) {
        let files = vec![
            File {
                path: "SKILL.md".into(),
                content: format!("---\nname: x\ndescription: {desc}\n---\n{body}\n"),
            },
            File {
                path: "ref.md".into(),
                content: "reference\n".into(),
            },
        ];
        s.put(path, &Body::Folder(files), None, "t", "m").unwrap();
    }

    fn put_doc(s: &doc::Store, path: &str, text: &str) {
        s.put(path, &Body::Doc(text.into()), None, "t", "m")
            .unwrap();
    }

    fn attach(s: &doc::Store, path: &str, scope: &str) {
        s.attach(path, &Scope::parse(scope).unwrap(), true, "t")
            .unwrap();
    }

    fn task_like() -> Vec<Scope> {
        scopes(
            &["one".into(), "two".into()],
            Some("bot:b"),
            &["proj".into(), "x".into()],
        )
    }

    fn find<'a>(r: &'a Resolution, path: &str, scope: &str) -> &'a Item {
        r.items
            .iter()
            .find(|i| i.path == path && i.scope == scope)
            .unwrap_or_else(|| panic!("{path} {scope} in {r:#?}"))
    }

    #[test]
    fn scopes_are_ordered_most_specific_first() {
        let s: Vec<String> = task_like().iter().map(Scope::to_string).collect();
        assert_eq!(
            s,
            [
                "repo:one",
                "repo:two",
                "owner:bot:b",
                "label:proj",
                "label:x",
                "global"
            ]
        );
        let b: Vec<String> = bot_scopes("ops", "p")
            .iter()
            .map(Scope::to_string)
            .collect();
        assert_eq!(b, ["owner:bot:ops", "label:p", "global"]);
    }

    #[test]
    fn precedence_across_the_four_scopes() {
        let (_d, s) = store();
        for (path, desc) in [
            ("g/review", "global review"),
            ("l/review", "label review"),
            ("o/review", "owner review"),
            ("r/review", "repo review"),
        ] {
            skill(&s, path, desc, "BODY");
        }
        attach(&s, "g/review", "global");
        attach(&s, "l/review", "label:x");
        attach(&s, "o/review", "owner:bot:b");
        attach(&s, "r/review", "repo:two");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        let won: Vec<_> = r.attached(Kind::Skill).collect();
        assert_eq!(won.len(), 1);
        assert_eq!(
            (won[0].path.as_str(), won[0].scope.as_str()),
            ("r/review", "repo:two")
        );
        assert_eq!(won[0].description, "repo review");
        for (p, sc) in [
            ("o/review", "owner:bot:b"),
            ("l/review", "label:x"),
            ("g/review", "global"),
        ] {
            let i = find(&r, p, sc);
            assert!(!i.included);
            assert_eq!(
                i.reason,
                "shadowed by r/review from repo:two: the more specific scope wins"
            );
        }

        // Without the repo's, the owner's wins; then the label's; then global.
        s.attach("r/review", &Scope::parse("repo:two").unwrap(), false, "t")
            .unwrap();
        assert_eq!(
            resolve(&s, &task_like(), &[])
                .unwrap()
                .attached(Kind::Skill)
                .next()
                .unwrap()
                .scope,
            "owner:bot:b"
        );
        s.attach(
            "o/review",
            &Scope::parse("owner:bot:b").unwrap(),
            false,
            "t",
        )
        .unwrap();
        assert_eq!(
            resolve(&s, &task_like(), &[])
                .unwrap()
                .attached(Kind::Skill)
                .next()
                .unwrap()
                .scope,
            "label:x"
        );
        s.attach("l/review", &Scope::parse("label:x").unwrap(), false, "t")
            .unwrap();
        assert_eq!(
            resolve(&s, &task_like(), &[])
                .unwrap()
                .attached(Kind::Skill)
                .next()
                .unwrap()
                .scope,
            "global"
        );
    }

    #[test]
    fn same_doc_on_two_scopes_is_listed_once() {
        let (_d, s) = store();
        put_doc(
            &s,
            "notes/a.md",
            "---\ndescription: The A notes\n---\nbody\n",
        );
        attach(&s, "notes/a.md", "global");
        attach(&s, "notes/a.md", "label:proj");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        let docs: Vec<_> = r.attached(Kind::Doc).collect();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].scope, "label:proj");
        assert_eq!(docs[0].description, "The A notes");
        assert_eq!(
            find(&r, "notes/a.md", "global").reason,
            "also attached to label:proj, which wins: the more specific scope wins"
        );
    }

    #[test]
    fn repo_rules_and_policy_are_not_overridable() {
        let (_d, s) = store();
        skill(&s, "o/build", "owner build", "B");
        attach(&s, "o/build", "owner:bot:b");
        attach(&s, "o/build", "repo:one");
        let own = [OwnSkill {
            name: "build".into(),
            repo: "one".into(),
            description: "Build the thing".into(),
        }];
        let r = resolve(&s, &task_like(), &own).unwrap();
        let build = r
            .items
            .iter()
            .find(|i| i.included && i.name == "build")
            .unwrap();
        assert!(build.own);
        assert_eq!(build.reason, "repo:one's own skill (not overridable)");
        for sc in ["repo:one", "owner:bot:b"] {
            assert!(find(&r, "o/build", sc)
                .reason
                .ends_with("a repo's own skills are not overridable"));
        }
        assert!(r.is_empty(), "a repo's own skills are not attached items");

        // A global policy skill beats a repo's attachment of the same name.
        skill(&s, "policy/safety", "the policy", "P");
        skill(&s, "r/safety", "repo safety", "R");
        attach(&s, "policy/safety", "global");
        attach(&s, "r/safety", "repo:one");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        let p = find(&r, "policy/safety", "global");
        assert!(p.included);
        assert_eq!(p.reason, "attached to global (policy: not overridable)");
        assert_eq!(
            find(&r, "r/safety", "repo:one").reason,
            "shadowed by policy/safety from global: policy is not overridable"
        );
    }

    #[test]
    fn index_has_descriptions_only_and_bodies_load_on_use() {
        let (d, s) = store();
        skill(&s, "skills/review", "Review a diff", "SECRET-SKILL-BODY");
        put_doc(&s, "notes/plan.md", "# The plan\n\nSECRET-DOC-BODY\n");
        put_doc(&s, "bundle/a.md", "a\n");
        put_doc(&s, "unrelated.md", "# Not attached\n");
        attach(&s, "skills/review", "label:x");
        attach(&s, "notes/plan.md", "global");
        attach(&s, "bundle", "owner:bot:b");
        attach(&s, "notes/plan.md", "label:other");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        assert_eq!(r.items.len(), 3, "{r:#?}");

        let dir = d.path().join("skills");
        write_skills(&dir, &r).unwrap();
        let stub = std::fs::read_to_string(dir.join("review/SKILL.md")).unwrap();
        assert!(
            stub.starts_with("---\nname: \"review\"\ndescription: \"Review a diff\"\n---\n"),
            "{stub}"
        );
        assert!(stub.contains("`axe doc get skills/review`"), "{stub}");
        assert!(!stub.contains("SECRET"), "{stub}");
        assert!(!dir.join("review/ref.md").exists());

        let docs = docs_section(&r);
        assert_eq!(
            docs,
            "\n## Attached\n\nDocs attached to this run's scopes, kept in the axe doc store. \
             Read one with `axe doc get <path>`.\n\n\
             - doc `notes/plan.md` (global): The plan\n\
             - bundle `bundle` (owner:bot:b): 1 files\n"
        );
        assert!(!docs.contains("SECRET"));
        assert_eq!(
            skills_lines(&r, "skills/"),
            "- `skills/review` (label:x, attached): Review a diff\n"
        );
        let prompt = prompt_section(&r);
        assert!(
            prompt.contains("\n## Skills\n\n- `skills/review` (label:x): Review a diff\n"),
            "{prompt}"
        );
        assert!(prompt.contains("\n## Docs\n"), "{prompt}");
        assert!(!prompt.contains("SECRET"));

        // The body is one store read away.
        match Source::get(&s, "skills/review").unwrap() {
            Fetched::Folder(f) => assert!(f[0].content.contains("SECRET-SKILL-BODY")),
            Fetched::Doc(_) => panic!("folder"),
        }
    }

    #[test]
    fn bundle_prompts_are_ordered_inlined_and_not_listed_again() {
        let (_d, s) = store();
        let bundle = |path: &str, prompt: &str| {
            let files = vec![
                File {
                    path: BUNDLE_PROMPT.into(),
                    content: prompt.into(),
                },
                File {
                    path: "ref.md".into(),
                    content: "reference\n".into(),
                },
            ];
            s.put(path, &Body::Folder(files), None, "t", "m").unwrap();
        };
        bundle(
            "b/chan",
            "---\ndescription: The channel\norder: 20\n---\n## Channel\nC\n",
        );
        bundle("b/proto", "---\norder: 10\n---\n# Protocol\nP\n");
        bundle("b/plain", "# No front matter\n");
        put_doc(&s, "b/other/a.md", "a\n");
        attach(&s, "b/chan", "global");
        attach(&s, "b/proto", "owner:bot:b");
        attach(&s, "b/plain", "label:proj");
        attach(&s, "b/other", "global");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        assert_eq!(
            bot_prompt_parts(&r),
            ["# No front matter\n", "# Protocol\nP\n", "## Channel\nC\n"],
            "order 0, 10, 20; front matter stripped"
        );
        assert_eq!(find(&r, "b/chan", "global").description, "The channel");
        assert_eq!(find(&r, "b/proto", "owner:bot:b").description, "2 files");
        let prompt = prompt_section(&r);
        assert!(
            prompt.contains("- bundle `b/other` (global): 1 files\n"),
            "{prompt}"
        );
        for inlined in ["b/chan", "b/proto", "b/plain"] {
            assert!(!prompt.contains(inlined), "{prompt}");
        }
        // Task runs list every bundle; explain shows them all.
        let docs = docs_section(&r);
        assert!(
            docs.contains("- bundle `b/chan` (global): The channel\n"),
            "{docs}"
        );
        assert_eq!(explain(&r).lines().count(), 5);

        // Only inlined bundles attached: no listing at all.
        s.attach("b/other", &Scope::parse("global").unwrap(), false, "t")
            .unwrap();
        let r = resolve(&s, &task_like(), &[]).unwrap();
        assert_eq!(bot_prompt_parts(&r).len(), 3);
        assert_eq!(prompt_section(&r), "");

        // A bad order is reported, not guessed.
        bundle("b/bad", "---\norder: soon\n---\nX\n");
        attach(&s, "b/bad", "global");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        assert!(find(&r, "b/bad", "global")
            .reason
            .contains("order \"soon\" is not an integer"));
        assert_eq!(bot_prompt_parts(&r).len(), 3);
    }

    #[test]
    fn strip_frontmatter_keeps_text_without_one() {
        assert_eq!(strip_frontmatter("---\na: b\n---\nbody\n"), "body\n");
        assert_eq!(strip_frontmatter("body\n---\n"), "body\n---\n");
        assert_eq!(
            strip_frontmatter("---\nunterminated\n"),
            "---\nunterminated\n"
        );
    }

    #[test]
    fn nothing_attached_writes_nothing() {
        let (d, s) = store();
        put_doc(&s, "x.md", "x\n");
        attach(&s, "x.md", "label:elsewhere");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        assert!(r.items.is_empty() && r.is_empty());
        let dir = d.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();
        write_skills(&dir, &r).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(docs_section(&r), "");
        assert_eq!(skills_lines(&r, ""), "");
        assert_eq!(prompt_section(&r), "");
        assert!(explain(&r).ends_with("nothing attached\n"));
    }

    /// A source with a dangling attachment and an unreadable one.
    struct Broken;
    impl Source for Broken {
        fn attachments(&self) -> Result<Vec<Attachment>> {
            Ok(vec![
                Attachment {
                    scope: "global".into(),
                    path: "gone.md".into(),
                    exists: false,
                },
                Attachment {
                    scope: "label:proj".into(),
                    path: "bad.md".into(),
                    exists: true,
                },
            ])
        }
        fn get(&self, _: &str) -> Result<Fetched> {
            anyhow::bail!("boom")
        }
    }

    #[test]
    fn missing_or_unreadable_attachments_are_skipped_and_explained() {
        let r = resolve(&Broken, &task_like(), &[]).unwrap();
        assert!(r.is_empty());
        assert_eq!(
            find(&r, "gone.md", "global").reason,
            "attached to global but no longer exists"
        );
        assert_eq!(
            find(&r, "bad.md", "label:proj").reason,
            "attached to label:proj but could not be read: boom"
        );
    }

    #[test]
    fn explain_lists_each_item_with_its_scope() {
        let (_d, s) = store();
        skill(&s, "g/review", "g", "B");
        skill(&s, "l/review", "l", "B");
        put_doc(&s, "notes/a.md", "# A\n");
        attach(&s, "g/review", "global");
        attach(&s, "l/review", "label:proj");
        attach(&s, "notes/a.md", "owner:bot:b");
        let r = resolve(&s, &task_like(), &[]).unwrap();
        let text = explain(&r);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "scopes (most specific first): repo:one, repo:two, owner:bot:b, label:proj, label:x, global"
        );
        assert_eq!(lines.len(), 4, "{text}");
        let row = |l: &str| l.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
        assert_eq!(row(lines[1]), "included doc notes/a.md owner:bot:b");
        assert_eq!(row(lines[2]), "included skill l/review label:proj");
        assert_eq!(row(lines[3]), "skipped skill g/review global");
        assert!(lines[3]
            .ends_with("shadowed by l/review from label:proj: the more specific scope wins"));
    }
}
