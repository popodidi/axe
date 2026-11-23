//! Selection rules: server rows that pick the harness, model and effort of a
//! run. A rule matches a task or bot on any of workload, repo, goal, label, bot
//! and owner (none set: the default) and holds an ordered `use` list. At claim
//! the most specific matching rule wins, then the higher priority, then the
//! newer rule; the dispatcher walks its `use` list for the first harness it can
//! run (`dispatch.rs`). Every edit is a row in `rule_events`.
use crate::db::{self, Actor};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::str::FromStr;

/// Harnesses a rule can name: the coding CLIs axe drives.
pub const HARNESSES: [&str; 2] = ["claude-code", "codex"];

/// `rule_events.rule_id` is not a foreign key: a removed rule keeps its history.
/// The one `import` event (no rule id) marks the one-time config import.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rules (
 id INTEGER PRIMARY KEY, matcher TEXT NOT NULL, uses TEXT NOT NULL,
 priority INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS rule_events (
 id INTEGER PRIMARY KEY, rule_id INTEGER, at TEXT NOT NULL, actor TEXT NOT NULL,
 session TEXT, kind TEXT NOT NULL, body TEXT NOT NULL
);";

/// What a rule matches. Every set key must hold; none set is the default rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    /// One of the task's (or bot's) repos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// One of the task's labels; the project counts as a label, as in filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A bot's runs. Only rules with `bot` set apply to bots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Match {
    fn keys(&self) -> [(&'static str, Option<&String>); 6] {
        [
            ("workload", self.workload.as_ref()),
            ("repo", self.repo.as_ref()),
            ("goal", self.goal.as_ref()),
            ("label", self.label.as_ref()),
            ("bot", self.bot.as_ref()),
            ("owner", self.owner.as_ref()),
        ]
    }

    /// Number of keys set: more is more specific.
    pub fn specificity(&self) -> usize {
        self.keys().iter().filter(|(_, v)| v.is_some()).count()
    }

    /// Sets one `key=value` pair; a key may be given once.
    pub fn set(&mut self, pair: &str) -> Result<()> {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("match {pair:?}: expected key=value"))?;
        let slot = match key {
            "workload" => &mut self.workload,
            "repo" => &mut self.repo,
            "goal" => &mut self.goal,
            "label" => &mut self.label,
            "bot" => &mut self.bot,
            "owner" => &mut self.owner,
            _ => bail!("match key {key:?}: expected workload, repo, goal, label, bot or owner"),
        };
        if slot.is_some() {
            bail!("match key {key:?} given twice");
        }
        *slot = Some(value.to_string());
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(w) = &self.workload {
            db::validate_workload(w)?;
        }
        for (kind, v) in [
            ("repo", &self.repo),
            ("goal", &self.goal),
            ("label", &self.label),
            ("bot", &self.bot),
        ] {
            if let Some(v) = v {
                db::validate_name(kind, v)?;
            }
        }
        if let Some(o) = &self.owner {
            db::validate_owner(o)?;
            if o == "none" {
                bail!("match owner: \"none\" is not an owner");
            }
        }
        Ok(())
    }

    pub fn matches(&self, s: &Subject) -> bool {
        let eq = |want: &Option<String>, have: &Option<String>| {
            want.as_ref().is_none_or(|w| have.as_ref() == Some(w))
        };
        let within =
            |want: &Option<String>, have: &[String]| want.as_ref().is_none_or(|w| have.contains(w));
        eq(&self.workload, &s.workload)
            && within(&self.repo, &s.repos)
            && eq(&self.goal, &s.goal)
            && within(&self.label, &s.labels)
            && eq(&self.bot, &s.bot)
            && eq(&self.owner, &s.owner)
    }
}

impl fmt::Display for Match {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set: Vec<String> = self
            .keys()
            .iter()
            .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
            .collect();
        if set.is_empty() {
            f.write_str("default")
        } else {
            f.write_str(&set.join(","))
        }
    }
}

/// One `use` entry: `harness[:model[:effort]]`; an empty part is the
/// harness's own default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Use {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// A model or effort value: passed as one argument, never through a shell.
fn check_param(kind: &str, v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.len() <= 64
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-[]/".contains(c));
    if !ok {
        bail!("invalid {kind} {v:?}: use letters, digits and . _ - [ ] /, at most 64 characters");
    }
    Ok(())
}

impl Use {
    pub fn validate(&self) -> Result<()> {
        if !HARNESSES.contains(&self.harness.as_str()) {
            bail!(
                "unknown harness {:?} (expected one of {})",
                self.harness,
                HARNESSES.join(", ")
            );
        }
        if let Some(m) = &self.model {
            check_param("model", m)?;
        }
        if let Some(e) = &self.effort {
            check_param("effort", e)?;
        }
        Ok(())
    }

    /// Arguments that pass this entry's model and effort to its harness's CLI.
    pub fn args(&self) -> Vec<String> {
        let mut out = Vec::new();
        let codex = self.harness == "codex";
        if let Some(m) = &self.model {
            out.push(if codex { "-m" } else { "--model" }.to_string());
            out.push(m.clone());
        }
        if let Some(e) = &self.effort {
            if codex {
                out.push("-c".into());
                out.push(format!("model_reasoning_effort=\"{e}\""));
            } else {
                out.push("--effort".into());
                out.push(e.clone());
            }
        }
        out
    }
}

impl FromStr for Use {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let mut parts = s.splitn(3, ':');
        let harness = parts.next().unwrap_or_default().to_string();
        let part = |p: Option<&str>| p.filter(|p| !p.is_empty()).map(String::from);
        let u = Use {
            harness,
            model: part(parts.next()),
            effort: part(parts.next()),
        };
        u.validate().with_context(|| format!("use {s:?}"))?;
        Ok(u)
    }
}

impl fmt::Display for Use {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.harness)?;
        match (&self.model, &self.effort) {
            (None, None) => Ok(()),
            (m, None) => write!(f, ":{}", m.as_deref().unwrap_or_default()),
            (m, Some(e)) => write!(f, ":{}:{e}", m.as_deref().unwrap_or_default()),
        }
    }
}

/// A rule as written: what it matches, its `use` list and its priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRule {
    #[serde(rename = "match", default)]
    pub matcher: Match,
    #[serde(rename = "use")]
    pub uses: Vec<Use>,
    #[serde(default)]
    pub priority: i64,
}

impl NewRule {
    pub fn validate(&self) -> Result<()> {
        self.matcher.validate()?;
        if self.uses.is_empty() {
            bail!("a rule needs at least one use entry");
        }
        for u in &self.uses {
            u.validate()?;
        }
        Ok(())
    }
}

/// A stored rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    #[serde(rename = "match")]
    pub matcher: Match,
    #[serde(rename = "use")]
    pub uses: Vec<Use>,
    pub priority: i64,
    pub created_at: String,
}

impl Rule {
    /// Names the rule on runs and in notes: `rule:<id> (<match>)`.
    pub fn key(&self) -> String {
        format!("rule:{} ({})", self.id, self.matcher)
    }
}

/// What a rule is matched against: one task, or one bot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub workload: Option<String>,
    pub repos: Vec<String>,
    pub goal: Option<String>,
    /// The task's labels plus its project.
    pub labels: Vec<String>,
    pub bot: Option<String>,
    pub owner: Option<String>,
}

/// The rule that applies: most specific, then highest priority, then newest.
/// A bot is matched only by rules that name a bot.
pub fn select<'a>(rules: &'a [Rule], s: &Subject) -> Option<&'a Rule> {
    rules
        .iter()
        .filter(|r| r.matcher.matches(s) && (s.bot.is_none() || r.matcher.bot.is_some()))
        .max_by_key(|r| (r.matcher.specificity(), r.priority, r.id))
}

fn parse<T: serde::de::DeserializeOwned>(i: usize, raw: String) -> rusqlite::Result<T> {
    serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: r.get(0)?,
        matcher: parse(1, r.get(1)?)?,
        uses: parse(2, r.get(2)?)?,
        priority: r.get(3)?,
        created_at: r.get(4)?,
    })
}

/// Rules in the order they win: most specific first, then priority, then newest.
pub fn list(conn: &Connection) -> Result<Vec<Rule>> {
    let mut rules: Vec<Rule> = conn
        .prepare("SELECT id, matcher, uses, priority, created_at FROM rules")?
        .query_map([], row)?
        .collect::<rusqlite::Result<_>>()?;
    rules.sort_by_key(|r| std::cmp::Reverse((r.matcher.specificity(), r.priority, r.id)));
    Ok(rules)
}

pub fn get(conn: &Connection, id: i64) -> Result<Rule> {
    conn.query_row(
        "SELECT id, matcher, uses, priority, created_at FROM rules WHERE id=?1",
        [id],
        row,
    )
    .optional()?
    .with_context(|| format!("rule {id} not found"))
}

fn audit(
    conn: &Connection,
    id: Option<i64>,
    actor: &Actor,
    kind: &str,
    body: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO rule_events(rule_id,at,actor,session,kind,body) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            id,
            db::now(),
            actor.name,
            actor.session,
            kind,
            body.to_string()
        ],
    )?;
    Ok(())
}

fn insert(conn: &Connection, actor: &Actor, r: &NewRule, kind: &str) -> Result<i64> {
    r.validate()?;
    conn.execute(
        "INSERT INTO rules(matcher,uses,priority,created_at) VALUES (?1,?2,?3,?4)",
        params![
            serde_json::to_string(&r.matcher)?,
            serde_json::to_string(&r.uses)?,
            r.priority,
            db::now()
        ],
    )?;
    let id = conn.last_insert_rowid();
    audit(conn, Some(id), actor, kind, &serde_json::to_value(r)?)?;
    Ok(id)
}

/// Moves every rule that names bot `old` (`bot=old` or `owner=bot:old`) to
/// `new`, inside the caller's transaction, each with a `bot-renamed` event.
/// Returns how many rules changed.
pub(crate) fn rename_bot(tx: &Transaction, actor: &Actor, old: &str, new: &str) -> Result<usize> {
    let rows: Vec<(i64, String)> = tx
        .prepare("SELECT id, matcher FROM rules ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (old_owner, new_owner) = (format!("bot:{old}"), format!("bot:{new}"));
    let mut n = 0;
    for (id, matcher) in rows {
        let Ok(mut m) = serde_json::from_str::<Match>(&matcher) else {
            continue;
        };
        let mut changed = false;
        if m.bot.as_deref() == Some(old) {
            m.bot = Some(new.to_string());
            changed = true;
        }
        if m.owner.as_deref() == Some(old_owner.as_str()) {
            m.owner = Some(new_owner.clone());
            changed = true;
        }
        if !changed {
            continue;
        }
        tx.execute(
            "UPDATE rules SET matcher=?2 WHERE id=?1",
            params![id, serde_json::to_string(&m)?],
        )?;
        audit(
            tx,
            Some(id),
            actor,
            "bot-renamed",
            &json!({"from": old, "to": new, "matcher": m}),
        )?;
        n += 1;
    }
    Ok(n)
}

/// Adds a rule and logs an `added` event.
pub fn add(conn: &mut Connection, actor: &Actor, r: &NewRule) -> Result<Rule> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = insert(&tx, actor, r, "added")?;
    let rule = get(&tx, id)?;
    tx.commit()?;
    Ok(rule)
}

/// Removes a rule and logs a `removed` event holding what it was.
pub fn remove(conn: &mut Connection, actor: &Actor, id: i64) -> Result<Rule> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let old = get(&tx, id)?;
    tx.execute("DELETE FROM rules WHERE id=?1", [id])?;
    audit(
        &tx,
        Some(id),
        actor,
        "removed",
        &serde_json::to_value(&old)?,
    )?;
    tx.commit()?;
    Ok(old)
}

/// Rule edits, newest first, 100 per page before event id `before`.
pub fn events(conn: &Connection, before: i64) -> Result<Vec<Value>> {
    Ok(conn
        .prepare(
            "SELECT id,rule_id,at,actor,session,kind,body FROM rule_events WHERE id<?1
             ORDER BY id DESC LIMIT 100",
        )?
        .query_map([before], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "rule_id": r.get::<_, Option<i64>>(1)?,
                "at": r.get::<_, String>(2)?,
                "actor": r.get::<_, String>(3)?,
                "session": r.get::<_, Option<String>>(4)?,
                "kind": r.get::<_, String>(5)?,
                "body": serde_json::from_str::<Value>(&r.get::<_, String>(6)?).unwrap_or(Value::Null),
            }))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// The one-time import of a dispatcher's config rules. The first call writes
/// the `import` event and, when no rule is stored yet, the rules (each with an
/// `imported` event); every later call changes nothing. Returns the import
/// event's body.
pub fn import(conn: &Connection, actor: &Actor, rules: &[NewRule], source: &str) -> Result<Value> {
    if let Some(body) = conn
        .query_row(
            "SELECT body FROM rule_events WHERE kind='import' ORDER BY id LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        let mut v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        v["already"] = json!(true);
        return Ok(v);
    }
    for r in rules {
        r.validate()?;
    }
    let present: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM rules)", [], |r| r.get(0))?;
    let mut ids = Vec::new();
    if !present {
        for r in rules {
            ids.push(insert(conn, actor, r, "imported")?);
        }
    }
    let body = json!({
        "source": source,
        "rules": ids,
        "skipped": if present { Some("rules already stored") } else { None },
    });
    audit(conn, None, actor, "import", &body)?;
    Ok(body)
}

/// The subject of task `id`: its latest workload and goal, repos, labels
/// (with the project) and owner.
pub fn task_subject(conn: &Connection, id: i64) -> Result<Subject> {
    let t = db::get_task(conn, id)?;
    let workload = conn
        .query_row(
            "SELECT to_value FROM events WHERE task_id=?1 AND kind='workload' ORDER BY id DESC LIMIT 1",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let mut labels = t.labels;
    if !labels.contains(&t.project) {
        labels.push(t.project);
    }
    Ok(Subject {
        workload,
        repos: t.repos.unwrap_or_default(),
        goal: db::current_goal(conn, id)?,
        labels,
        bot: None,
        owner: t.owner,
    })
}

/// The subject of bot `name`: its name and repos.
pub fn bot_subject(conn: &Connection, name: &str) -> Result<Subject> {
    let b = crate::bot::get(conn, name)?;
    Ok(Subject {
        repos: b.repos.unwrap_or_default(),
        bot: Some(b.name),
        ..Subject::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: i64, m: &[&str], uses: &[&str], priority: i64) -> Rule {
        let mut matcher = Match::default();
        for p in m {
            matcher.set(p).unwrap();
        }
        Rule {
            id,
            matcher,
            uses: uses.iter().map(|u| u.parse().unwrap()).collect(),
            priority,
            created_at: String::new(),
        }
    }

    fn task(labels: &[&str], workload: Option<&str>) -> Subject {
        Subject {
            workload: workload.map(String::from),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..Subject::default()
        }
    }

    #[test]
    fn specificity_beats_priority_and_priority_beats_age() {
        let rules = vec![
            rule(1, &[], &["claude-code"], 100),
            rule(2, &["label=web"], &["codex"], 0),
            rule(
                3,
                &["label=web", "workload=prod-bug"],
                &["claude-code:opus:high"],
                -5,
            ),
            rule(4, &["label=web"], &["claude-code:sonnet"], 1),
            rule(5, &["workload=prod-bug"], &["codex::high"], 1),
        ];
        let pick = |s: &Subject| select(&rules, s).map(|r| r.id);
        // The most specific rule wins even with the lowest priority.
        assert_eq!(pick(&task(&["web"], Some("prod-bug"))), Some(3));
        // Equally specific: the higher priority wins (4 over 2).
        assert_eq!(pick(&task(&["web"], None)), Some(4));
        // Equally specific and equal priority: the newer rule wins.
        let tied = vec![
            rule(1, &["label=a"], &["codex"], 0),
            rule(2, &["label=b"], &["claude-code"], 0),
        ];
        assert_eq!(
            select(&tied, &task(&["a", "b"], None)).map(|r| r.id),
            Some(2)
        );
        assert_eq!(pick(&task(&["other"], Some("prod-bug"))), Some(5));
        assert_eq!(pick(&task(&["other"], None)), Some(1));
        assert_eq!(select(&[], &task(&[], None)), None);
    }

    #[test]
    fn bots_match_only_rules_that_name_a_bot() {
        let rules = vec![
            rule(1, &[], &["claude-code"], 0),
            rule(2, &["bot=ops"], &["claude-code:sonnet:medium"], 0),
            rule(3, &["owner=bot:ops"], &["codex"], 0),
        ];
        let bot = |n: &str| Subject {
            bot: Some(n.into()),
            ..Subject::default()
        };
        assert_eq!(select(&rules, &bot("ops")).map(|r| r.id), Some(2));
        assert_eq!(select(&rules, &bot("other")), None);
        // A task owned by the bot matches the owner rule, not the bot rule.
        let owned = Subject {
            owner: Some("bot:ops".into()),
            ..Subject::default()
        };
        assert_eq!(select(&rules, &owned).map(|r| r.id), Some(3));
    }

    #[test]
    fn use_entries_parse_validate_and_render() {
        let u: Use = "claude-code:opus:high".parse().unwrap();
        assert_eq!(u.args(), ["--model", "opus", "--effort", "high"]);
        assert_eq!(u.to_string(), "claude-code:opus:high");
        let u: Use = "codex::high".parse().unwrap();
        assert_eq!(u.model, None);
        assert_eq!(u.args(), ["-c", "model_reasoning_effort=\"high\""]);
        assert_eq!(u.to_string(), "codex::high");
        let u: Use = "codex:gpt-5".parse().unwrap();
        assert_eq!(u.args(), ["-m", "gpt-5"]);
        let u: Use = "claude-code".parse().unwrap();
        assert!(u.args().is_empty());
        assert_eq!(u.to_string(), "claude-code");
        assert!(
            "claude".parse::<Use>().is_err(),
            "harness names are claude-code and codex"
        );
        assert!("codex:a b".parse::<Use>().is_err());
        assert!("codex:m:$(x)".parse::<Use>().is_err());
    }

    #[test]
    fn match_keys_validate() {
        let mut m = Match::default();
        assert!(m.set("project=x").is_err());
        m.set("workload=feature").unwrap();
        assert!(m.set("workload=ops").is_err(), "a key once");
        m.validate().unwrap();
        assert_eq!(m.to_string(), "workload=feature");
        let mut bad = Match::default();
        bad.set("workload=nope").unwrap();
        assert!(bad.validate().is_err());
        assert_eq!(Match::default().to_string(), "default");
    }

    fn conn() -> Connection {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep().join("axe.db");
        db::open(&path).unwrap()
    }

    fn new(m: &[&str], uses: &[&str], priority: i64) -> NewRule {
        let r = rule(0, m, uses, priority);
        NewRule {
            matcher: r.matcher,
            uses: r.uses,
            priority,
        }
    }

    #[test]
    fn edits_emit_events() {
        let mut c = conn();
        let a = Actor::new("tester");
        let r = add(
            &mut c,
            &a,
            &new(&["label=web"], &["codex", "claude-code"], 2),
        )
        .unwrap();
        assert_eq!(list(&c).unwrap(), vec![r.clone()]);
        remove(&mut c, &a, r.id).unwrap();
        assert!(list(&c).unwrap().is_empty());
        assert!(remove(&mut c, &a, r.id).is_err());
        let ev = events(&c, i64::MAX).unwrap();
        let kinds: Vec<_> = ev.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["removed", "added"]);
        assert!(ev
            .iter()
            .all(|e| e["rule_id"] == r.id && e["actor"] == "tester"));
        assert_eq!(ev[1]["body"]["use"][0]["harness"], "codex");
        assert_eq!(ev[0]["body"]["match"]["label"], "web");
        // Invalid rules are refused and log nothing.
        assert!(add(&mut c, &a, &new(&[], &[], 0)).is_err());
        assert_eq!(events(&c, i64::MAX).unwrap().len(), 2);
    }

    #[test]
    fn import_runs_once() {
        let c = conn();
        let a = Actor::new("dispatcher");
        let rules = vec![
            new(&[], &["claude-code"], 0),
            new(&["label=p"], &["codex"], 1),
        ];
        let first = import(&c, &a, &rules, "config").unwrap();
        assert_eq!(first["rules"].as_array().unwrap().len(), 2);
        let again = import(&c, &a, &[new(&[], &["codex"], 0)], "config").unwrap();
        assert_eq!(again["already"], true);
        assert_eq!(list(&c).unwrap().len(), 2);
        let kinds: Vec<_> = events(&c, i64::MAX)
            .unwrap()
            .into_iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, ["import", "imported", "imported"]);
    }

    #[test]
    fn import_keeps_rules_added_first() {
        let mut c = conn();
        let a = Actor::new("human");
        add(&mut c, &a, &new(&[], &["codex"], 0)).unwrap();
        let out = import(&c, &a, &[new(&[], &["claude-code"], 0)], "config").unwrap();
        assert_eq!(out["skipped"], "rules already stored");
        let rules = list(&c).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].uses[0].harness, "codex");
    }
}
