//! Project rename: `axe project rename OLD NEW`.
//!
//! A project has no table of its own: it is the name tasks, runs, bots,
//! subscriptions, the bot feed, goals, routines and their occurrences carry,
//! and the label a task (and a selection rule) matches it by. A rename moves
//! every one of those in one database transaction, and the doc store's
//! `OLD/` folder, the attachments naming paths under it and the
//! `label:OLD` scope in one doc store commit ([`crate::doc::Store::rename_prefix`]).
//! The doc commit is made inside the database transaction, just before it
//! commits, so a doc store failure rolls the database back.
//!
//! The old name is not kept as an alias: every lookup by project would have to
//! resolve it, and a later project reusing the name would collide. Each moved
//! task gets a `project` event (`from_value` OLD, `to_value` NEW) instead, so
//! its history keeps the old name. Past event bodies, run logs and free text
//! (task details, bot conduct, routine details, schedule objectives, doc
//! bodies) stay as written. Node folders keyed by a project name (the legacy
//! `.worktrees/<project>/task-<id>--work`) stay where they are: an agent
//! session resumes by its directory, so the node finds them by task id.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::{self, Actor};

/// What a rename moves (or, with `dry_run`, would move).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Renamed {
    pub from: String,
    pub to: String,
    pub dry_run: bool,
    /// Rows changed per table (`tasks.labels`, `routines`, ... name the
    /// column when a table has more than one).
    pub tables: BTreeMap<String, usize>,
    /// The doc store part; `None` when the server has no doc store.
    pub docs: Option<crate::doc::PrefixMove>,
}

/// A rename request.
pub struct Request<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub why: &'a str,
    pub owner_ref: Option<&'a str>,
    pub dry_run: bool,
}

fn count(tx: &Transaction, sql: &str, name: &str) -> Result<usize> {
    let n: i64 = tx.query_row(sql, [name], |r| r.get(0))?;
    Ok(n as usize)
}

/// Rows of `sql` (`id`, JSON text) whose JSON `rewrite` changes, with the new text.
fn rewrites(
    tx: &Transaction,
    sql: &str,
    rewrite: impl Fn(&mut Value) -> bool,
) -> Result<Vec<(i64, String)>> {
    let rows: Vec<(i64, String)> = tx
        .prepare(sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, text) in rows {
        let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if rewrite(&mut v) {
            out.push((id, serde_json::to_string(&v)?));
        }
    }
    Ok(out)
}

/// Replaces `from` with `to` in the string array `v` (no duplicates).
fn rename_in_list(v: &mut Value, from: &str, to: &str) -> bool {
    let Some(list) = v.as_array_mut() else {
        return false;
    };
    if !list.iter().any(|x| x == from) {
        return false;
    }
    let mut seen = Vec::new();
    list.retain_mut(|x| {
        if *x == from {
            *x = Value::from(to);
        }
        let keep = !seen.contains(x);
        seen.push(x.clone());
        keep
    });
    true
}

/// A routine template or occurrence snapshot: its `project` and `labels`.
fn rename_template(v: &mut Value, from: &str, to: &str) -> bool {
    let mut changed = false;
    if v.get("project").and_then(Value::as_str) == Some(from) {
        v["project"] = Value::from(to);
        changed = true;
    }
    if let Some(labels) = v.get_mut("labels") {
        changed |= rename_in_list(labels, from, to);
    }
    changed
}

fn rename_rule(v: &mut Value, from: &str, to: &str) -> bool {
    if v.get("label").and_then(Value::as_str) == Some(from) {
        v["label"] = Value::from(to);
        return true;
    }
    false
}

const ROUTINES: &str = "SELECT id, config FROM routines ORDER BY id";
const OCCURRENCES: &str = "SELECT id, snapshot FROM routine_occurrences ORDER BY id";
const RULES: &str = "SELECT id, matcher FROM rules ORDER BY id";
const LABELLED: &str = "SELECT id, labels FROM tasks WHERE EXISTS
    (SELECT 1 FROM json_each(tasks.labels) WHERE value=?1)";

/// Rows naming `name` as a project or label, per table; zero counts left out.
fn uses(tx: &Transaction, name: &str) -> Result<BTreeMap<String, usize>> {
    let mut out = BTreeMap::new();
    for (table, sql) in [
        ("tasks", "SELECT COUNT(*) FROM tasks WHERE project=?1"),
        (
            "tasks.labels",
            "SELECT COUNT(*) FROM tasks WHERE EXISTS
             (SELECT 1 FROM json_each(tasks.labels) WHERE value=?1)",
        ),
        ("runs", "SELECT COUNT(*) FROM runs WHERE project=?1"),
        ("bots", "SELECT COUNT(*) FROM bots WHERE project=?1"),
        (
            "bot_subscriptions",
            "SELECT COUNT(*) FROM bot_subscriptions WHERE project=?1",
        ),
        ("bot_feed", "SELECT COUNT(*) FROM bot_feed WHERE project=?1"),
        ("goals", "SELECT COUNT(*) FROM goals WHERE project=?1"),
    ] {
        let n = count(tx, sql, name)?;
        if n > 0 {
            out.insert(table.to_string(), n);
        }
    }
    for (table, sql, f) in [
        (
            "routines",
            ROUTINES,
            rename_template as fn(&mut Value, &str, &str) -> bool,
        ),
        ("routine_occurrences", OCCURRENCES, rename_template),
        ("rules", RULES, rename_rule),
    ] {
        let n = rewrites(tx, sql, |v| f(v, name, "\0"))?.len();
        if n > 0 {
            out.insert(table.to_string(), n);
        }
    }
    Ok(out)
}

/// Renames project `req.from` to `req.to` (an owner decision; a non-human
/// actor needs `owner_ref`). Refused when `to` is in use (as a project, a
/// label or a doc folder), when a task of `from` is in progress, or while a
/// run of one of its tasks or bots is live. With `dry_run`, reports the
/// same counts and changes nothing. `docs` is the server's doc store.
pub fn rename(
    conn: &mut Connection,
    actor: &Actor,
    req: &Request,
    docs: Option<&crate::doc::Store>,
) -> Result<Renamed> {
    let (from, to) = (req.from.trim(), req.to.trim());
    if req.why.trim().is_empty() {
        bail!("--why must not be empty");
    }
    db::validate_name("project", to)?;
    if from == to {
        bail!("project {from} already has that name");
    }
    let owner_ref = req.owner_ref.map(str::trim).filter(|r| !r.is_empty());
    if owner_ref.is_none() && actor.name != "human" {
        return Err(db::Forbidden(format!(
            "renaming a project is an owner decision: actor {} needs --owner-ref (e.g. \"msg #873\")",
            actor.name
        ))
        .into());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let taken = uses(&tx, to)?;
    if !taken.is_empty() {
        let list: Vec<String> = taken.iter().map(|(t, n)| format!("{t} {n}")).collect();
        bail!(
            "project {to} already exists ({}); pick another name",
            list.join(", ")
        );
    }
    let busy = count(
        &tx,
        "SELECT COUNT(*) FROM tasks WHERE project=?1 AND state='in_progress'",
        from,
    )?;
    if busy > 0 {
        bail!("project {from} has {busy} task(s) in progress: wait for them to stop, then rename");
    }
    let live = count(
        &tx,
        "SELECT (SELECT COUNT(*) FROM runs r JOIN tasks t ON t.id=r.task_id
                  WHERE t.project=?1 AND r.ended_at IS NULL)
              + (SELECT COUNT(*) FROM bot_runs r JOIN bots b ON b.id=r.bot_id
                  WHERE b.project=?1 AND r.ended_at IS NULL)",
        from,
    )?;
    if live > 0 {
        bail!("project {from} has {live} live run(s): stop them (`axe bot stop`) or wait for them to end, then rename");
    }
    let tables = uses(&tx, from)?;
    let doc_plan = docs
        .map(|d| d.rename_prefix(from, to, &actor.name, false))
        .transpose()?;
    if tables.is_empty() && doc_plan.as_ref().is_none_or(|d| d.is_empty()) {
        bail!("project {from} not found");
    }
    if req.dry_run {
        return Ok(Renamed {
            from: from.into(),
            to: to.into(),
            dry_run: true,
            tables,
            docs: doc_plan,
        });
    }
    let ids: Vec<i64> = tx
        .prepare("SELECT id FROM tasks WHERE project=?1 ORDER BY id")?
        .query_map([from], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in &ids {
        db::write_event(
            &tx,
            actor,
            *id,
            "project",
            Some(from),
            Some(to),
            Some(req.why),
        )?;
    }
    for sql in [
        "UPDATE tasks SET project=?2 WHERE project=?1",
        "UPDATE runs SET project=?2 WHERE project=?1",
        "UPDATE bot_subscriptions SET project=?2 WHERE project=?1",
        "UPDATE bot_feed SET project=?2 WHERE project=?1",
    ] {
        tx.execute(sql, params![from, to])?;
    }
    let labelled: Vec<(i64, String)> = tx
        .prepare(LABELLED)?
        .query_map([from], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, text) in labelled {
        let mut v: Value = serde_json::from_str(&text)?;
        rename_in_list(&mut v, from, to);
        tx.execute(
            "UPDATE tasks SET labels=?2 WHERE id=?1",
            params![id, serde_json::to_string(&v)?],
        )?;
    }
    let audit = json!({"from": from, "to": to, "why": req.why, "owner_ref": owner_ref});
    // Bots and goals record a project change as `why: ...\nOLD -> NEW`, as
    // their edits do.
    let body = match owner_ref {
        Some(r) => format!("why: {} ({r})\n{from} -> {to}", req.why),
        None => format!("why: {}\n{from} -> {to}", req.why),
    };
    let at = crate::bot::ts(crate::remote::epoch());
    let bots: Vec<i64> = tx
        .prepare("SELECT id FROM bots WHERE project=?1 ORDER BY id")?
        .query_map([from], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in bots {
        tx.execute(
            "UPDATE bots SET project=?2, updated_at=?3 WHERE id=?1",
            params![id, to, at],
        )?;
        crate::bot::event(&tx, id, actor, "project", &body)?;
    }
    let goals: Vec<i64> = tx
        .prepare("SELECT id FROM goals WHERE project=?1 ORDER BY id")?
        .query_map([from], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in goals {
        tx.execute(
            "UPDATE goals SET project=?2, updated_at=?3 WHERE id=?1",
            params![id, to, at],
        )?;
        crate::goal::event(&tx, id, actor, "project", None, owner_ref, &body, &at)?;
    }
    for (id, text) in rewrites(&tx, ROUTINES, |v| rename_template(v, from, to))? {
        tx.execute(
            "UPDATE routines SET config=?2, updated_at=?3 WHERE id=?1",
            params![id, text, db::now()],
        )?;
        crate::routine::audit(&tx, id, actor, "project-renamed", &audit.to_string())?;
    }
    for (id, text) in rewrites(&tx, OCCURRENCES, |v| rename_template(v, from, to))? {
        tx.execute(
            "UPDATE routine_occurrences SET snapshot=?2 WHERE id=?1",
            params![id, text],
        )?;
    }
    for (id, text) in rewrites(&tx, RULES, |v| rename_rule(v, from, to))? {
        tx.execute("UPDATE rules SET matcher=?2 WHERE id=?1", params![id, text])?;
        crate::rule::audit(&tx, Some(id), actor, "project-renamed", &audit)?;
    }
    let left = uses(&tx, from)?;
    if !left.is_empty() {
        bail!("rename left rows on {from}: {left:?}");
    }
    let docs = docs
        .map(|d| d.rename_prefix(from, to, &actor.name, true))
        .transpose()?;
    tx.commit()?;
    Ok(Renamed {
        from: from.into(),
        to: to.into(),
        dry_run: false,
        tables,
        docs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_rename_keeps_order_and_drops_duplicates() {
        let mut v = json!(["a", "old", "b"]);
        assert!(rename_in_list(&mut v, "old", "new"));
        assert_eq!(v, json!(["a", "new", "b"]));
        let mut v = json!(["new", "old"]);
        assert!(rename_in_list(&mut v, "old", "new"));
        assert_eq!(v, json!(["new"]));
        assert!(!rename_in_list(&mut json!(["x"]), "old", "new"));
    }
}
