//! Named bot cron schedules. A firing is an ordinary `bot_wakes` row labeled in
//! its body, so readiness, batching and delivery are the normal wake path, and
//! the dispatcher needs no change. The server ticks these next to routines.
//!
//! `next_at` is a durable cursor advanced in the same transaction as the wake,
//! so a restart neither loses nor repeats a firing, and missed slots after
//! downtime collapse into one. A schedule has at most one undelivered wake.
use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::bot::{self, epoch_of, ts};
use crate::db::Actor;
use crate::goal;
use crate::routine;

/// `bot_wakes.key` prefix; `axe bot wake` keys always start with `wake:`.
pub const KEY_PREFIX: &str = "sched:";
/// `bot_wakes.actor` prefix, followed by the schedule name.
pub const ACTOR_PREFIX: &str = "schedule:";
/// Audited changes, as `axe bot show` lists them (firings are excluded).
pub const CHANGE_KINDS: [&str; 5] = [
    "schedule_add",
    "schedule_edit",
    "schedule_pause",
    "schedule_resume",
    "schedule_remove",
];
/// Shortest allowed gap between two firings of one schedule.
pub const MIN_INTERVAL_SECONDS: i64 = 15 * 60;
pub const OBJECTIVE_CAP: usize = 2 * 1024;
/// Consecutive occurrences checked against the minimum interval. A short gap
/// comes from the minute set or a field boundary, so it shows up early.
const INTERVAL_SAMPLES: usize = 512;
/// Fixed start of that check (2024-01-01Z, a leap year), so the verdict does
/// not depend on today. It runs in UTC: the interval is a property of the
/// expression, and a zone's DST shift must not reject a valid one on some days.
const INTERVAL_BASE: i64 = 1_704_067_200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "BotSchedule"))]
pub struct Schedule {
    pub id: i64,
    pub name: String,
    pub cron: String,
    pub timezone: String,
    pub objective: String,
    pub enabled: bool,
    pub next_at: String,
    pub last_fired_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// The goal each firing is labeled with (`[goal NAME]`).
    #[serde(default)]
    pub goal: Option<String>,
}

/// Schedule rows with their goal name, as [`from_row`] reads them.
const COLS: &str = "s.*, (SELECT name FROM goals g WHERE g.id=s.goal_id) AS goal";

fn from_row(r: &Row) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get("id")?,
        name: r.get("name")?,
        cron: r.get("cron")?,
        timezone: r.get("timezone")?,
        objective: r.get("objective")?,
        enabled: r.get("enabled")?,
        next_at: r.get("next_at")?,
        last_fired_at: r.get("last_fired_at")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        goal: r.get("goal")?,
    })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewSchedule {
    pub name: String,
    pub cron: String,
    #[serde(default = "utc")]
    pub timezone: String,
    pub objective: String,
}

fn utc() -> String {
    "UTC".into()
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("schedule name {name:?} must be 1-40 chars of a-z, 0-9 and '-'");
    }
    Ok(())
}

fn check_why(why: &str) -> Result<()> {
    if why.trim().is_empty() {
        bail!("--why must not be empty");
    }
    Ok(())
}

/// Rejects an expression that would ever fire twice within the minimum interval.
fn check_interval(cron: &str, timezone: &str) -> Result<()> {
    let (cron, _) = routine::parse_cron(cron, timezone)?;
    let mut at = Utc
        .timestamp_opt(INTERVAL_BASE, 0)
        .single()
        .context("invalid time")?;
    let mut previous: Option<chrono::DateTime<Utc>> = None;
    for _ in 0..INTERVAL_SAMPLES {
        // Occurrences can run out for a sparse date; what was seen is enough.
        let Ok(next) = cron.find_next_occurrence(&at, false) else {
            break;
        };
        if let Some(p) = previous {
            let gap = (next - p).num_seconds();
            if gap < MIN_INTERVAL_SECONDS {
                bail!(
                    "cron fires {gap}s apart; the minimum interval is {} minutes",
                    MIN_INTERVAL_SECONDS / 60
                );
            }
        }
        previous = Some(next);
        at = next;
    }
    Ok(())
}

pub fn list(conn: &Connection, bot_name: &str) -> Result<Vec<Schedule>> {
    let b = bot::get(conn, bot_name)?;
    Ok(conn
        .prepare(&format!(
            "SELECT {COLS} FROM bot_schedules s WHERE s.bot_id=?1 ORDER BY s.name"
        ))?
        .query_map([b.id], from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

fn find(conn: &Connection, bot_id: i64, name: &str) -> Result<Schedule> {
    conn.query_row(
        &format!("SELECT {COLS} FROM bot_schedules s WHERE s.bot_id=?1 AND s.name=?2"),
        params![bot_id, name],
        from_row,
    )
    .optional()?
    .with_context(|| format!("schedule {name} not found"))
}

pub fn add(
    conn: &mut Connection,
    actor: &Actor,
    bot_name: &str,
    s: &NewSchedule,
    why: &str,
    now: i64,
) -> Result<Schedule> {
    add_for_goal(conn, actor, bot_name, s, None, why, now)
}

/// [`add`] with a goal its firings are labeled with (`none` for no goal).
pub fn add_for_goal(
    conn: &mut Connection,
    actor: &Actor,
    bot_name: &str,
    s: &NewSchedule,
    goal: Option<&str>,
    why: &str,
    now: i64,
) -> Result<Schedule> {
    check_name(&s.name)?;
    check_why(why)?;
    if s.objective.trim().is_empty() {
        bail!("objective must not be empty");
    }
    if s.objective.len() > OBJECTIVE_CAP {
        bail!("objective exceeds {OBJECTIVE_CAP} bytes");
    }
    let next = routine::next_after(&s.cron, &s.timezone, now)?;
    check_interval(&s.cron, &s.timezone)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot_name)?;
    let goal = goal.map(|g| goal::resolve(&tx, g)).transpose()?.flatten();
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM bot_schedules WHERE bot_id=?1 AND name=?2)",
        params![b.id, s.name],
        |r| r.get(0),
    )?;
    if exists {
        bail!("schedule {} already exists", s.name);
    }
    let at = ts(now);
    tx.execute(
        "INSERT INTO bot_schedules (bot_id, name, cron, timezone, objective, enabled, next_at,
           created_at, updated_at, goal_id)
         VALUES (?1,?2,?3,?4,?5,1,?6,?7,?7,?8)",
        params![
            b.id,
            s.name,
            s.cron,
            s.timezone,
            s.objective,
            ts(next),
            at,
            goal.as_ref().map(|g| g.id)
        ],
    )?;
    let mut detail = json!({"schedule":s.name,"cron":s.cron,"timezone":s.timezone,
        "objective":s.objective,"next_at":ts(next),"why":why});
    if let Some(g) = &goal {
        detail["goal"] = json!(g.name);
    }
    bot::event(&tx, b.id, actor, "schedule_add", &detail.to_string())?;
    let created = find(&tx, b.id, &s.name)?;
    tx.commit()?;
    Ok(created)
}

/// `axe bot schedule edit`: sets the goal its firings are labeled with
/// (`none` clears it). Repeating the current goal changes nothing.
pub fn set_goal(
    conn: &mut Connection,
    actor: &Actor,
    bot_name: &str,
    name: &str,
    goal: &str,
    why: &str,
    now: i64,
) -> Result<Schedule> {
    check_why(why)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot_name)?;
    let s = find(&tx, b.id, name)?;
    let g = goal::resolve(&tx, goal)?;
    let to = g.as_ref().map(|g| g.name.clone());
    if to == s.goal {
        return Ok(s);
    }
    tx.execute(
        "UPDATE bot_schedules SET goal_id=?1, updated_at=?2 WHERE id=?3",
        params![g.as_ref().map(|g| g.id), ts(now), s.id],
    )?;
    bot::event(
        &tx,
        b.id,
        actor,
        "schedule_edit",
        &json!({"schedule":name,"goal":{"from":s.goal,"to":to},"why":why}).to_string(),
    )?;
    let updated = find(&tx, b.id, name)?;
    tx.commit()?;
    Ok(updated)
}

/// Drops a schedule's undelivered wake, so a stopped schedule fires no more.
fn drop_queued(conn: &Connection, bot_id: i64, schedule_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM bot_wakes WHERE bot_id=?1 AND run_id IS NULL AND key LIKE ?2",
        params![bot_id, format!("{KEY_PREFIX}{schedule_id}:%")],
    )?;
    Ok(())
}

/// `pause`, `resume` or `remove`. Repeating the current state changes nothing.
pub fn change(
    conn: &mut Connection,
    actor: &Actor,
    bot_name: &str,
    name: &str,
    action: &str,
    why: &str,
    now: i64,
) -> Result<()> {
    check_why(why)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot_name)?;
    let s = find(&tx, b.id, name)?;
    let mut detail = json!({"schedule":name,"why":why});
    match action {
        "pause" if !s.enabled => return Ok(()),
        "resume" if s.enabled => return Ok(()),
        "pause" => {
            drop_queued(&tx, b.id, s.id)?;
            tx.execute(
                "UPDATE bot_schedules SET enabled=0, updated_at=?1 WHERE id=?2",
                params![ts(now), s.id],
            )?;
        }
        "resume" => {
            let next = ts(routine::next_after(&s.cron, &s.timezone, now)?);
            tx.execute(
                "UPDATE bot_schedules SET enabled=1, next_at=?1, updated_at=?2 WHERE id=?3",
                params![next, ts(now), s.id],
            )?;
            detail["next_at"] = json!(next);
        }
        "remove" => {
            drop_queued(&tx, b.id, s.id)?;
            tx.execute("DELETE FROM bot_schedules WHERE id=?1", [s.id])?;
        }
        _ => bail!("invalid schedule action {action:?}"),
    }
    bot::event(
        &tx,
        b.id,
        actor,
        &format!("schedule_{action}"),
        &detail.to_string(),
    )?;
    tx.commit()?;
    Ok(())
}

/// The wake text, prefixed `[goal NAME]` for a goal's schedule. The label is
/// stored rather than rendered, so a dispatcher running an older binary still
/// delivers it.
fn wake_body(objective: &str, goal: Option<&str>) -> String {
    let body = format!("scheduled wake (automation, not an owner message). Objective: {objective}");
    match goal {
        Some(g) => goal::label(g, &body),
        None => body,
    }
}

/// Fires every due schedule once and advances its cursor; returns the number of
/// wakes created. A paused bot only advances. A firing whose schedule still has
/// an undelivered wake coalesces into it.
pub fn tick(conn: &mut Connection, now: i64) -> Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let due: Vec<(Schedule, i64, String)> = tx
        .prepare(&format!(
            "SELECT {COLS}, b.state AS bot_state FROM bot_schedules s JOIN bots b ON b.id=s.bot_id
             WHERE s.enabled=1 AND s.next_at<=?1"
        ))?
        .query_map([ts(now)], |r| {
            Ok((from_row(r)?, r.get("bot_id")?, r.get("bot_state")?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let actor = Actor::new("scheduler");
    let mut fired = 0;
    for (s, bot_id, state) in &due {
        let next = ts(routine::next_after(&s.cron, &s.timezone, now)?);
        tx.execute(
            "UPDATE bot_schedules SET next_at=?1 WHERE id=?2",
            params![next, s.id],
        )?;
        if state != "active" {
            continue;
        }
        let cursor = epoch_of(&s.next_at).unwrap_or(now);
        let at = routine::latest_at(&s.cron, &s.timezone, now)?.max(cursor);
        let key = format!("{KEY_PREFIX}{}:{at}", s.id);
        let queued: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM bot_wakes WHERE bot_id=?1
               AND (key=?2 OR (run_id IS NULL AND key LIKE ?3)))",
            params![bot_id, key, format!("{KEY_PREFIX}{}:%", s.id)],
            |r| r.get(0),
        )?;
        let body = if queued {
            json!({"schedule":s.name,"at":ts(at),"coalesced":true})
        } else {
            let created = ts(now);
            let wake = bot::insert_wake(
                &tx,
                *bot_id,
                "wake",
                &key,
                None,
                &wake_body(&s.objective, s.goal.as_deref()),
                &created,
                &created,
                &Actor::new(&format!("{ACTOR_PREFIX}{}", s.name)),
            )?;
            tx.execute(
                "UPDATE bot_schedules SET last_fired_at=?1 WHERE id=?2",
                params![created, s.id],
            )?;
            fired += 1;
            json!({"schedule":s.name,"at":ts(at),"wake":wake})
        };
        bot::event(&tx, *bot_id, &actor, "schedule_fire", &body.to_string())?;
    }
    tx.commit()?;
    Ok(fired)
}
