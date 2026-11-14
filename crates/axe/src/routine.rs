//! Durable cron cursors and atomic ordinary-task creation. All times are UTC epochs.
use crate::db::{self, Actor};
use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS routines (
 id INTEGER PRIMARY KEY, config TEXT NOT NULL, enabled INTEGER NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0, next_at INTEGER NOT NULL,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS routine_occurrences (
 id INTEGER PRIMARY KEY, routine_id INTEGER NOT NULL REFERENCES routines(id),
 identity TEXT NOT NULL, scheduled_at INTEGER, task_id INTEGER NOT NULL UNIQUE REFERENCES tasks(id),
 snapshot TEXT NOT NULL, created_at TEXT NOT NULL,
 UNIQUE(routine_id, identity)
);
CREATE INDEX IF NOT EXISTS routine_history ON routine_occurrences(routine_id, id);
CREATE TABLE IF NOT EXISTS routine_events (
 id INTEGER PRIMARY KEY, routine_id INTEGER NOT NULL REFERENCES routines(id),
 at TEXT NOT NULL, actor TEXT NOT NULL, session TEXT, kind TEXT NOT NULL, body TEXT NOT NULL
);";

/// Parses a standard five-field numeric cron and an IANA time zone.
pub(crate) fn parse_cron(cron: &str, timezone: &str) -> Result<(Cron, Tz)> {
    // Deliberately restrict v1 to standard five-field numeric cron syntax.
    if cron.split_whitespace().count() != 5
        || !cron
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_whitespace() || "*,-/".contains(c))
    {
        bail!("cron must contain five numeric fields (* , - / supported)");
    }
    Ok((
        cron.parse().context("invalid cron")?,
        timezone.parse().context("invalid timezone")?,
    ))
}
/// The first occurrence strictly after `now`.
pub(crate) fn next_after(cron: &str, timezone: &str, now: i64) -> Result<i64> {
    let (cron, tz) = parse_cron(cron, timezone)?;
    Ok(cron
        .find_next_occurrence(
            &tz.timestamp_opt(now, 0).single().context("invalid time")?,
            false,
        )?
        .timestamp())
}
/// The latest occurrence at or before `now`.
pub(crate) fn latest_at(cron: &str, timezone: &str, now: i64) -> Result<i64> {
    let (cron, tz) = parse_cron(cron, timezone)?;
    Ok(cron
        .find_previous_occurrence(
            &tz.timestamp_opt(now, 0).single().context("invalid time")?,
            true,
        )?
        .timestamp())
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub name: String,
    pub project: String,
    pub cron: String,
    #[serde(default = "utc")]
    pub timezone: String,
    pub title: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload: Option<String>,
    /// The created task's owner, repos and labels, as `axe add` sets them
    /// ([`db::Fields`]). `repos` omitted: the project checkout; `[]`: a run
    /// workspace with no repos. Unset fields are not stored, so templates saved
    /// before these fields existed are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}
fn utc() -> String {
    "UTC".into()
}
impl Template {
    pub fn next(&self, now: i64) -> Result<i64> {
        next_after(&self.cron, &self.timezone, now)
    }
    fn latest(&self, now: i64) -> Result<i64> {
        latest_at(&self.cron, &self.timezone, now)
    }
    fn validate(&self, now: i64) -> Result<i64> {
        for (key, value) in [
            ("name", &self.name),
            ("project", &self.project),
            ("title", &self.title),
            ("detail", &self.detail),
        ] {
            if value.trim().is_empty() {
                bail!("{key} must not be empty");
            }
        }
        if let Some(o) = &self.owner {
            db::validate_owner(o)?;
        }
        if let Some(r) = &self.repos {
            db::names("repo", r)?;
        }
        db::names("label", &self.labels)?;
        self.next(now)
    }
}
#[derive(Debug, Serialize)]
pub struct Routine {
    pub id: i64,
    #[serde(flatten)]
    pub template: Template,
    pub enabled: bool,
    pub archived: bool,
    pub next_at: i64,
    pub created_at: String,
    pub updated_at: String,
    pub latest_task: Option<i64>,
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Routine> {
    let raw: String = r.get(1)?;
    let template = serde_json::from_str(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Routine {
        id: r.get(0)?,
        template,
        enabled: r.get(2)?,
        archived: r.get(3)?,
        next_at: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        latest_task: r.get(7)?,
    })
}
const SELECT: &str = "SELECT r.*, (SELECT task_id FROM routine_occurrences o WHERE o.routine_id=r.id ORDER BY o.id DESC LIMIT 1) FROM routines r";
pub fn list(conn: &Connection) -> Result<Vec<Routine>> {
    Ok(conn
        .prepare(&format!("{SELECT} ORDER BY r.id DESC"))?
        .query_map([], row)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn get(conn: &Connection, id: i64) -> Result<Routine> {
    conn.query_row(&format!("{SELECT} WHERE r.id=?1"), [id], row)
        .optional()?
        .context("routine not found")
}
fn audit(conn: &Connection, id: i64, actor: &Actor, kind: &str, body: &str) -> Result<()> {
    conn.execute("INSERT INTO routine_events(routine_id,at,actor,session,kind,body) VALUES (?1,?2,?3,?4,?5,?6)", params![id, db::now(), actor.name, actor.session, kind, body])?;
    Ok(())
}
pub fn save(
    conn: &mut Connection,
    actor: &Actor,
    id: Option<i64>,
    template: &Template,
    now: i64,
) -> Result<i64> {
    let next = template.validate(now)?;
    if let Some(w) = &template.workload {
        db::validate_workload(w)?;
    }
    let raw = serde_json::to_string(template)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = if let Some(id) = id {
        let old = get(&tx, id)?;
        if old.archived {
            bail!("archived routine cannot be edited");
        }
        let next =
            if old.template.cron == template.cron && old.template.timezone == template.timezone {
                old.next_at
            } else {
                next
            };
        tx.execute(
            "UPDATE routines SET config=?1,next_at=?2,updated_at=?3 WHERE id=?4",
            params![raw, next, db::now(), id],
        )?;
        id
    } else {
        tx.execute("INSERT INTO routines(config,enabled,next_at,created_at,updated_at) VALUES (?1,1,?2,?3,?3)",params![raw,next,db::now()])?;
        tx.last_insert_rowid()
    };
    audit(&tx, id, actor, "saved", &raw)?;
    tx.commit()?;
    Ok(id)
}
pub fn change(conn: &mut Connection, actor: &Actor, id: i64, action: &str, now: i64) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let r = get(&tx, id)?;
    if r.archived {
        bail!("routine is archived");
    }
    let (enabled, archived, next) = match action {
        "pause" => (false, false, r.next_at),
        "resume" => (
            true,
            false,
            if r.enabled {
                r.next_at
            } else {
                r.template.next(now)?
            },
        ),
        "archive" => (false, true, r.next_at),
        _ => bail!("invalid routine action"),
    };
    tx.execute(
        "UPDATE routines SET enabled=?1,archived=?2,next_at=?3,updated_at=?4 WHERE id=?5",
        params![enabled, archived, next, db::now(), id],
    )?;
    audit(&tx, id, actor, action, "")?;
    tx.commit()?;
    Ok(())
}
fn occurrence(
    conn: &Transaction,
    actor: &Actor,
    r: &Routine,
    identity: &str,
    scheduled: Option<i64>,
) -> Result<i64> {
    if let Some(task) = conn
        .query_row(
            "SELECT task_id FROM routine_occurrences WHERE routine_id=?1 AND identity=?2",
            params![r.id, identity],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(task);
    }
    let t = &r.template;
    let ts = db::now();
    let repos = t
        .repos
        .as_deref()
        .map(|r| db::names("repo", r))
        .transpose()?;
    let labels = db::names("label", &t.labels)?;
    conn.execute("INSERT INTO tasks(project,title,detail,state,created_at,updated_at,agent,owner,repos,labels) VALUES (?1,?2,?3,'pending',?4,?4,?5,?6,?7,?8)",params![t.project,t.title,t.detail,ts,t.agent,t.owner,repos.as_ref().map(serde_json::to_string).transpose()?,serde_json::to_string(&labels)?])?;
    let task = conn.last_insert_rowid();
    conn.execute("INSERT INTO routine_occurrences(routine_id,identity,scheduled_at,task_id,snapshot,created_at) VALUES (?1,?2,?3,?4,?5,?6)",params![r.id,identity,scheduled,task,serde_json::to_string(t)?,ts])?;
    // Same event contract as manually added tasks; the backlink lives in its own indexed table.
    conn.execute("INSERT INTO events(task_id,at,actor,session,kind,to_value,body) VALUES (?1,?2,?3,?4,'created',?5,?6)",params![task,ts,actor.name,actor.session,t.title,format!("state=pending routine={} occurrence={identity}",r.id)])?;
    crate::subscription::fan_out(
        conn,
        &actor.name,
        conn.last_insert_rowid(),
        task,
        "created",
        None,
        Some(&t.title),
        None,
    )?;
    // The same events, in the same order, as `db::add_task_full`.
    if let Some(o) = &t.owner {
        db::write_event(conn, actor, task, "owner", None, Some(o), None)?;
    }
    if let Some(r) = &repos {
        db::write_event(conn, actor, task, "repos", None, Some(&r.join(",")), None)?;
    }
    if !labels.is_empty() {
        let l = labels.join(",");
        db::write_event(conn, actor, task, "labels", None, Some(&l), None)?;
    }
    if let Some(w) = &t.workload {
        conn.execute("INSERT INTO events(task_id,at,actor,session,kind,to_value) VALUES (?1,?2,?3,?4,'workload',?5)",params![task,ts,actor.name,actor.session,w])?;
    }
    audit(
        conn,
        r.id,
        actor,
        "triggered",
        &json!({"task_id":task,"identity":identity}).to_string(),
    )?;
    Ok(task)
}
pub fn trigger(conn: &mut Connection, actor: &Actor, id: i64, key: &str) -> Result<i64> {
    if key.is_empty() || key.len() > 128 {
        bail!("manual request key must contain 1..128 bytes");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let r = get(&tx, id)?;
    let identity = format!("manual:{key}");
    // Retries still return their original task after archival.
    let old = tx
        .query_row(
            "SELECT task_id FROM routine_occurrences WHERE routine_id=?1 AND identity=?2",
            params![id, identity],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(task) = old {
        return Ok(task);
    }
    if r.archived {
        bail!("routine is archived");
    }
    let task = occurrence(&tx, actor, &r, &identity, None)?;
    tx.commit()?;
    Ok(task)
}
pub fn tick(conn: &mut Connection, now: i64) -> Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let due: Vec<_> = list(&tx)?
        .into_iter()
        .filter(|r| r.enabled && !r.archived && r.next_at <= now)
        .collect();
    for r in &due {
        let at = r.template.latest(now)?.max(r.next_at);
        occurrence(
            &tx,
            &Actor::new("scheduler"),
            r,
            &format!("scheduled:{at}"),
            Some(at),
        )?;
        tx.execute(
            "UPDATE routines SET next_at=?1 WHERE id=?2",
            params![r.template.next(now)?, r.id],
        )?;
    }
    tx.commit()?;
    Ok(due.len())
}
pub fn history(conn: &Connection, id: i64, before: i64) -> Result<Vec<Value>> {
    get(conn, id)?;
    let mut q = conn.prepare("SELECT o.id,o.identity,o.scheduled_at,o.task_id,o.snapshot,o.created_at,t.state,t.title FROM routine_occurrences o JOIN tasks t ON t.id=o.task_id WHERE o.routine_id=?1 AND o.id<?2 ORDER BY o.id DESC LIMIT 100")?;
    let rows = q.query_map(params![id,before], |r| Ok(json!({"id":r.get::<_,i64>(0)?,"identity":r.get::<_,String>(1)?,"scheduled_at":r.get::<_,Option<i64>>(2)?,"task_id":r.get::<_,i64>(3)?,"snapshot":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"created_at":r.get::<_,String>(5)?,"state":r.get::<_,String>(6)?,"title":r.get::<_,String>(7)?})))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
pub fn backlink(conn: &Connection, task: i64) -> Result<Option<Value>> {
    Ok(conn.query_row("SELECT routine_id,identity,scheduled_at,snapshot FROM routine_occurrences WHERE task_id=?1",[task],|r|Ok(json!({"routine_id":r.get::<_,i64>(0)?,"identity":r.get::<_,String>(1)?,"scheduled_at":r.get::<_,Option<i64>>(2)?,"snapshot":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).unwrap_or(Value::Null)}))).optional()?)
}
pub fn events(conn: &Connection, id: i64, before: i64) -> Result<Vec<Value>> {
    get(conn, id)?;
    Ok(conn.prepare("SELECT id,at,actor,session,kind,body FROM routine_events WHERE routine_id=?1 AND id<?2 ORDER BY id DESC LIMIT 100")?.query_map(params![id,before],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"at":r.get::<_,String>(1)?,"actor":r.get::<_,String>(2)?,"session":r.get::<_,Option<String>>(3)?,"kind":r.get::<_,String>(4)?,"body":r.get::<_,String>(5)?})))?.collect::<rusqlite::Result<_>>()?)
}
pub fn now() -> i64 {
    Utc::now().timestamp()
}
