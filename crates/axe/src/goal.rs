//! Goals: durable outcome state (intent, budget fields, trajectory) that bots
//! share. A goal is its own primitive with its own tables, API and `axe goal`
//! CLI; tasks link to one through a `goal` task event and wakes through a
//! `[goal NAME]` label. Design: `docs/design/goals.md`.
//!
//! Bots propose goals; the owner activates them and changes their intent,
//! budget and deadline. A non-`human` actor needs an owner reference
//! (`--owner-ref`) for those, recorded on the event and not verified by the
//! server, like bot approvals.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::bot::{self, ts};
use crate::db::{Actor, Forbidden};

/// Goal tables, added by schema v16 (additive only), plus the nullable
/// `bot_schedules.goal_id` column added by [`migrate`].
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS goals (
 id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, project TEXT NOT NULL,
 state TEXT NOT NULL CHECK (state IN ('proposed','active','paused','done','dropped')),
 intent TEXT NOT NULL DEFAULT '', deadline TEXT, budget_cost_week REAL,
 budget_attention_week INTEGER, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_bots (
 goal_id INTEGER NOT NULL REFERENCES goals(id), bot_id INTEGER NOT NULL REFERENCES bots(id),
 joined_at TEXT NOT NULL,
 PRIMARY KEY (goal_id, bot_id)
);
CREATE TABLE IF NOT EXISTS goal_events (
 id INTEGER PRIMARY KEY, goal_id INTEGER NOT NULL REFERENCES goals(id),
 at TEXT NOT NULL, actor TEXT NOT NULL, session TEXT, kind TEXT NOT NULL,
 task_id INTEGER REFERENCES tasks(id), owner_ref TEXT, body TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS goal_bots_bot ON goal_bots(bot_id);
CREATE INDEX IF NOT EXISTS goal_events_goal ON goal_events(goal_id, id);
";

pub const STATES: [&str; 5] = ["proposed", "active", "paused", "done", "dropped"];
/// Trajectory event kinds, written with `axe goal record`.
pub const TRAJECTORY_KINDS: [&str; 5] = ["hypothesis", "test", "verdict", "observation", "note"];
/// Audit event kinds, written by the server on each change.
pub const AUDIT_KINDS: [&str; 8] = [
    "proposed",
    "activated",
    "intent",
    "budget",
    "deadline",
    "join",
    "leave",
    "state",
];
pub const INTENT_CAP: usize = 16 * 1024;
pub const RECORD_CAP: usize = 16 * 1024;
/// Trajectory events `axe goal show` lists.
pub const SHOW_TRAJECTORY: i64 = 10;
/// The value that clears an optional field or a task's goal.
pub const NONE: &str = "none";

/// Creates the goal tables and adds `bot_schedules.goal_id` when missing, so
/// every migration path (and a database lowered to v15) ends at v16.
pub fn migrate(tx: &Transaction) -> Result<()> {
    tx.execute_batch(SCHEMA)?;
    let has: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('bot_schedules') WHERE name='goal_id')",
        [],
        |r| r.get(0),
    )?;
    if !has {
        tx.execute_batch(
            "ALTER TABLE bot_schedules ADD COLUMN goal_id INTEGER REFERENCES goals(id)",
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Goal {
    pub id: i64,
    pub name: String,
    pub project: String,
    pub state: String,
    pub intent: String,
    pub deadline: Option<String>,
    pub budget_cost_week: Option<f64>,
    pub budget_attention_week: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    /// Attached bot names.
    #[serde(default)]
    pub bots: Vec<String>,
    /// Spend over the last 7 days against the weekly budgets; set by `list`
    /// and `detail` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub budget: Option<Budget>,
}

const GOAL_COLS: &str = "g.*, (SELECT group_concat(name, ',') FROM
 (SELECT b.name FROM goal_bots gb JOIN bots b ON b.id=gb.bot_id WHERE gb.goal_id=g.id ORDER BY b.name))
 AS bot_names";

fn from_row(r: &Row) -> rusqlite::Result<Goal> {
    let bots: Option<String> = r.get("bot_names")?;
    Ok(Goal {
        id: r.get("id")?,
        name: r.get("name")?,
        project: r.get("project")?,
        state: r.get("state")?,
        intent: r.get("intent")?,
        deadline: r.get("deadline")?,
        budget_cost_week: r.get("budget_cost_week")?,
        budget_attention_week: r.get("budget_attention_week")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        bots: bots
            .map(|b| b.split(',').map(str::to_owned).collect())
            .unwrap_or_default(),
        budget: None,
    })
}

pub fn get(conn: &Connection, name: &str) -> Result<Goal> {
    conn.query_row(
        &format!("SELECT {GOAL_COLS} FROM goals g WHERE g.name=?1"),
        [name],
        from_row,
    )
    .optional()?
    .with_context(|| format!("goal {name} not found"))
}

/// Goals, optionally only those a bot is attached to and in one state.
pub fn list(conn: &Connection, bot_name: Option<&str>, state: Option<&str>) -> Result<Vec<Goal>> {
    if let Some(s) = state {
        check_state(s)?;
    }
    let bot_id = bot_name
        .map(|b| bot::get(conn, b).map(|b| b.id))
        .transpose()?;
    let mut goals: Vec<Goal> = conn
        .prepare(&format!(
            "SELECT {GOAL_COLS} FROM goals g
             WHERE (?1 IS NULL OR EXISTS(SELECT 1 FROM goal_bots gb WHERE gb.goal_id=g.id AND gb.bot_id=?1))
               AND (?2 IS NULL OR g.state=?2)
             ORDER BY g.id"
        ))?
        .query_map(params![bot_id, state], from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let spend = Spend::load(conn, crate::remote::epoch())?;
    for g in &mut goals {
        g.budget = Some(spend.budget(g));
    }
    Ok(goals)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "GoalEvent"))]
pub struct Event {
    pub id: i64,
    pub at: String,
    pub actor: String,
    pub session: Option<String>,
    pub kind: String,
    pub task_id: Option<i64>,
    pub owner_ref: Option<String>,
    pub body: String,
}

fn event_row(r: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get("id")?,
        at: r.get("at")?,
        actor: r.get("actor")?,
        session: r.get("session")?,
        kind: r.get("kind")?,
        task_id: r.get("task_id")?,
        owner_ref: r.get("owner_ref")?,
        body: r.get("body")?,
    })
}

/// The latest `limit` events of a goal, oldest first; `trajectory` keeps only
/// trajectory kinds.
pub fn events(conn: &Connection, name: &str, limit: i64, trajectory: bool) -> Result<Vec<Event>> {
    let g = get(conn, name)?;
    let kinds = if trajectory {
        format!(
            "AND kind IN ({})",
            TRAJECTORY_KINDS.map(|k| format!("'{k}'")).join(",")
        )
    } else {
        String::new()
    };
    let mut rows: Vec<Event> = conn
        .prepare(&format!(
            "SELECT * FROM goal_events WHERE goal_id=?1 {kinds} ORDER BY id DESC LIMIT ?2"
        ))?
        .query_map(params![g.id, limit.max(0)], event_row)?
        .collect::<rusqlite::Result<_>>()?;
    rows.reverse();
    Ok(rows)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "GoalTask"))]
pub struct LinkedTask {
    pub id: i64,
    pub state: String,
    pub title: String,
}

/// Tasks whose latest `goal` event names this goal.
pub fn tasks(conn: &Connection, name: &str) -> Result<Vec<LinkedTask>> {
    Ok(conn
        .prepare(
            "SELECT t.id, t.state, t.title FROM tasks t JOIN events e ON e.id=(
               SELECT MAX(id) FROM events WHERE task_id=t.id AND kind='goal')
             WHERE e.to_value=?1 ORDER BY t.id",
        )?
        .query_map([name], |r| {
            Ok(LinkedTask {
                id: r.get(0)?,
                state: r.get(1)?,
                title: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// The next goal-tagged wake of any bot: a pending `[goal NAME]` wake, or the
/// next run of an enabled schedule with the goal (RFC3339).
pub fn next_wake(conn: &Connection, g: &Goal) -> Result<Option<String>> {
    Ok(conn.query_row(
        "SELECT MIN(due) FROM (
           SELECT due_at AS due FROM bot_wakes
            WHERE run_id IS NULL AND kind='wake' AND substr(body,1,length(?1))=?1
           UNION ALL
           SELECT next_at FROM bot_schedules WHERE goal_id=?2 AND enabled=1)",
        params![label(&g.name, "").trim_end(), g.id],
        |r| r.get(0),
    )?)
}

/// `axe goal show`: the goal, its linked tasks and the latest trajectory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "GoalDetail"))]
pub struct Detail {
    #[serde(flatten)]
    #[cfg_attr(feature = "ts", ts(flatten))]
    pub goal: Goal,
    pub tasks: Vec<LinkedTask>,
    pub trajectory: Vec<Event>,
    /// The next goal-tagged wake ([`next_wake`]).
    #[serde(default)]
    pub next_wake: Option<String>,
}

pub fn detail(conn: &Connection, name: &str) -> Result<Detail> {
    let mut goal = get(conn, name)?;
    goal.budget = Some(budget(conn, &goal, crate::remote::epoch())?);
    Ok(Detail {
        next_wake: next_wake(conn, &goal)?,
        tasks: tasks(conn, name)?,
        trajectory: events(conn, name, SHOW_TRAJECTORY, true)?,
        goal,
    })
}

fn check_state(s: &str) -> Result<()> {
    if !STATES.contains(&s) {
        bail!(
            "invalid goal state {s:?} (expected one of {})",
            STATES.join(", ")
        );
    }
    Ok(())
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name != NONE
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!("goal name {name:?} must be 1-40 chars of a-z, 0-9 and '-' (not \"none\")");
    }
    Ok(())
}

fn check_why(why: &str) -> Result<()> {
    if why.trim().is_empty() {
        bail!("--why must not be empty");
    }
    Ok(())
}

fn check_cap(field: &str, v: &str, cap: usize) -> Result<()> {
    if v.len() > cap {
        bail!(
            "{field} is {} bytes; the cap is {} KiB",
            v.len(),
            cap / 1024
        );
    }
    Ok(())
}

/// The bot an actor is (`bot:<name>`), if any.
fn actor_bot(actor: &Actor) -> Option<&str> {
    actor.name.strip_prefix("bot:").filter(|b| !b.is_empty())
}

/// The trimmed owner reference; required for a non-`human` actor doing an
/// owner decision (`what`).
fn owner_ref<'a>(actor: &Actor, owner_ref: Option<&'a str>, what: &str) -> Result<Option<&'a str>> {
    let r = owner_ref.map(str::trim).filter(|r| !r.is_empty());
    if r.is_none() && actor.name != "human" {
        return Err(Forbidden(format!(
            "{what} is an owner decision: actor {} needs --owner-ref (e.g. \"msg #873\")",
            actor.name
        ))
        .into());
    }
    Ok(r)
}

#[allow(clippy::too_many_arguments)]
fn event(
    conn: &Connection,
    goal: i64,
    actor: &Actor,
    kind: &str,
    task: Option<i64>,
    owner_ref: Option<&str>,
    body: &str,
    at: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO goal_events (goal_id, at, actor, session, kind, task_id, owner_ref, body)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            goal,
            at,
            actor.name,
            actor.session,
            kind,
            task,
            owner_ref,
            body
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn touch(conn: &Connection, goal: i64, at: &str) -> Result<()> {
    conn.execute(
        "UPDATE goals SET updated_at=?1 WHERE id=?2",
        params![at, goal],
    )?;
    Ok(())
}

fn diff(field: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .header(field, field)
        .to_string()
}

/// A deadline: `YYYY-MM-DD`, or a time (RFC3339 or `+<n>[smhd]`) stored as
/// fixed-width UTC.
fn parse_deadline(s: &str) -> Result<String> {
    let s = s.trim();
    let date = time::macros::format_description!("[year]-[month]-[day]");
    if time::Date::parse(s, date).is_ok() {
        return Ok(s.to_string());
    }
    crate::db::parse_wake(s)
        .map_err(|_| anyhow::anyhow!("bad deadline {s:?}: use YYYY-MM-DD, RFC3339 or +<n>[smhd]"))
}

fn check_cost(v: f64) -> Result<f64> {
    if !v.is_finite() || v < 0.0 {
        bail!("budget_cost_week must be a non-negative number");
    }
    Ok(v)
}

fn check_attention(v: i64) -> Result<i64> {
    if v < 0 {
        bail!("budget_attention_week must not be negative");
    }
    Ok(v)
}

fn attach(tx: &Transaction, goal: i64, bot_name: &str, at: &str) -> Result<bool> {
    let b = bot::get(tx, bot_name)?;
    Ok(tx.execute(
        "INSERT OR IGNORE INTO goal_bots (goal_id, bot_id, joined_at) VALUES (?1,?2,?3)",
        params![goal, b.id, at],
    )? == 1)
}

/// `axe goal propose --file goal.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewGoal {
    pub name: String,
    pub project: String,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub deadline: Option<String>,
    #[serde(default)]
    pub budget_cost_week: Option<f64>,
    #[serde(default)]
    pub budget_attention_week: Option<i64>,
    /// Bots to attach; the proposing bot is always attached.
    #[serde(default)]
    pub bots: Vec<String>,
}

/// Creates a `proposed` goal and attaches the proposing bot and `bots`.
/// Attaching a bot other than the proposer needs an owner reference.
pub fn propose(
    conn: &mut Connection,
    actor: &Actor,
    g: &NewGoal,
    owner: Option<&str>,
    now: i64,
) -> Result<Goal> {
    check_name(&g.name)?;
    if g.project.trim().is_empty() {
        bail!("project must not be empty");
    }
    check_cap("intent", &g.intent, INTENT_CAP)?;
    let deadline = g.deadline.as_deref().map(parse_deadline).transpose()?;
    let cost = g.budget_cost_week.map(check_cost).transpose()?;
    let attention = g.budget_attention_week.map(check_attention).transpose()?;
    let me = actor_bot(actor);
    let mut bots: Vec<String> = me.map(str::to_owned).into_iter().collect();
    for b in &g.bots {
        if !bots.contains(b) {
            bots.push(b.clone());
        }
    }
    let owner = if bots.iter().any(|b| Some(b.as_str()) != me) {
        owner_ref(actor, owner, "attaching another bot")?
    } else {
        owner.map(str::trim).filter(|r| !r.is_empty())
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM goals WHERE name=?1)",
        [&g.name],
        |r| r.get(0),
    )?;
    if exists {
        bail!("goal {} already exists", g.name);
    }
    let at = ts(now);
    tx.execute(
        "INSERT INTO goals (name, project, state, intent, deadline, budget_cost_week,
           budget_attention_week, created_at, updated_at)
         VALUES (?1,?2,'proposed',?3,?4,?5,?6,?7,?7)",
        params![g.name, g.project, g.intent, deadline, cost, attention, at],
    )?;
    let id = tx.last_insert_rowid();
    event(
        &tx,
        id,
        actor,
        "proposed",
        None,
        owner,
        &json!({"project":g.project,"deadline":deadline,"budget_cost_week":cost,
            "budget_attention_week":attention,"bots":bots})
        .to_string(),
        &at,
    )?;
    for b in &bots {
        attach(&tx, id, b, &at)?;
        event(
            &tx,
            id,
            actor,
            "join",
            None,
            owner,
            &format!("bot {b}"),
            &at,
        )?;
    }
    tx.commit()?;
    get(conn, &g.name)
}

/// Fields `axe goal edit` may change; `None` keeps the value, `"none"` clears
/// an optional one.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Edit {
    pub intent: Option<String>,
    pub deadline: Option<String>,
    pub budget_cost_week: Option<String>,
    pub budget_attention_week: Option<String>,
}

fn optional<T>(v: &str, parse: impl Fn(&str) -> Result<T>) -> Result<Option<T>> {
    if v.trim() == NONE {
        Ok(None)
    } else {
        parse(v.trim()).map(Some)
    }
}

fn show<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map_or(NONE.into(), T::to_string)
}

/// Applies an edit (an owner decision); each changed field is one event.
pub fn edit(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    e: &Edit,
    why: &str,
    owner: Option<&str>,
) -> Result<Goal> {
    check_why(why)?;
    if e.intent.is_none()
        && e.deadline.is_none()
        && e.budget_cost_week.is_none()
        && e.budget_attention_week.is_none()
    {
        bail!("nothing to edit: give --intent-file, --deadline or a budget");
    }
    let owner = owner_ref(actor, owner, "editing a goal")?;
    if let Some(i) = &e.intent {
        check_cap("intent", i, INTENT_CAP)?;
    }
    let deadline = e
        .deadline
        .as_deref()
        .map(|d| optional(d, parse_deadline))
        .transpose()?;
    let cost = e
        .budget_cost_week
        .as_deref()
        .map(|v| {
            optional(v, |s| {
                check_cost(s.parse().context("budget_cost_week must be a number")?)
            })
        })
        .transpose()?;
    let attention = e
        .budget_attention_week
        .as_deref()
        .map(|v| {
            optional(v, |s| {
                check_attention(
                    s.parse()
                        .context("budget_attention_week must be an integer")?,
                )
            })
        })
        .transpose()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let g = get(&tx, name)?;
    let at = ts(crate::remote::epoch());
    let mut changed = false;
    if let Some(new) = e.intent.as_ref().filter(|i| **i != g.intent) {
        tx.execute("UPDATE goals SET intent=?1 WHERE id=?2", params![new, g.id])?;
        let body = format!("why: {why}\n{}", diff("intent", &g.intent, new));
        event(&tx, g.id, actor, "intent", None, owner, &body, &at)?;
        changed = true;
    }
    if let Some(new) = deadline.filter(|d| *d != g.deadline) {
        tx.execute(
            "UPDATE goals SET deadline=?1 WHERE id=?2",
            params![new, g.id],
        )?;
        let body = format!("why: {why}\n{} -> {}", show(&g.deadline), show(&new));
        event(&tx, g.id, actor, "deadline", None, owner, &body, &at)?;
        changed = true;
    }
    let mut budget = Vec::new();
    if let Some(new) = cost.filter(|c| *c != g.budget_cost_week) {
        tx.execute(
            "UPDATE goals SET budget_cost_week=?1 WHERE id=?2",
            params![new, g.id],
        )?;
        budget.push(format!(
            "budget_cost_week: {} -> {}",
            show(&g.budget_cost_week),
            show(&new)
        ));
    }
    if let Some(new) = attention.filter(|a| *a != g.budget_attention_week) {
        tx.execute(
            "UPDATE goals SET budget_attention_week=?1 WHERE id=?2",
            params![new, g.id],
        )?;
        budget.push(format!(
            "budget_attention_week: {} -> {}",
            show(&g.budget_attention_week),
            show(&new)
        ));
    }
    if !budget.is_empty() {
        let body = format!("why: {why}\n{}", budget.join("\n"));
        event(&tx, g.id, actor, "budget", None, owner, &body, &at)?;
        changed = true;
    }
    if changed {
        touch(&tx, g.id, &at)?;
    }
    tx.commit()?;
    get(conn, name)
}

/// The state an action moves a goal to, and the states it applies from.
fn transition(action: &str) -> Result<(&'static str, &'static [&'static str])> {
    Ok(match action {
        "activate" => ("active", &["proposed"]),
        "pause" => ("paused", &["active"]),
        "resume" => ("active", &["paused"]),
        "done" => ("done", &["active", "paused"]),
        "drop" => ("dropped", &["proposed", "active", "paused"]),
        _ => bail!("invalid goal action {action:?}"),
    })
}

/// `activate`, `pause`, `resume`, `done` or `drop`. Activation is an owner
/// decision. Repeating the current state changes nothing.
pub fn change(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    action: &str,
    why: &str,
    owner: Option<&str>,
) -> Result<Goal> {
    check_why(why)?;
    let (to, from) = transition(action)?;
    let owner = if action == "activate" {
        owner_ref(actor, owner, "activating a goal")?
    } else {
        owner.map(str::trim).filter(|r| !r.is_empty())
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let g = get(&tx, name)?;
    if g.state == to {
        return Ok(g);
    }
    if !from.contains(&g.state.as_str()) {
        bail!(
            "goal {name} is {}; {action} applies to {}",
            g.state,
            from.join(" or ")
        );
    }
    let at = ts(crate::remote::epoch());
    tx.execute(
        "UPDATE goals SET state=?1, updated_at=?2 WHERE id=?3",
        params![to, at, g.id],
    )?;
    let kind = if action == "activate" {
        "activated"
    } else {
        "state"
    };
    let body = format!("{} -> {to}\nwhy: {why}", g.state);
    event(&tx, g.id, actor, kind, None, owner, &body, &at)?;
    tx.commit()?;
    get(conn, name)
}

/// `join` or `leave`: attaches or detaches a bot. A bot may join or leave
/// itself; anything else from a non-`human` actor needs an owner reference.
/// Leaving keeps every event.
pub fn membership(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    action: &str,
    bot_name: &str,
    why: &str,
    owner: Option<&str>,
) -> Result<Goal> {
    check_why(why)?;
    if !matches!(action, "join" | "leave") {
        bail!("invalid goal action {action:?}");
    }
    let owner = if actor_bot(actor) == Some(bot_name) {
        owner.map(str::trim).filter(|r| !r.is_empty())
    } else {
        owner_ref(actor, owner, &format!("{action} for another bot"))?
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let g = get(&tx, name)?;
    let at = ts(crate::remote::epoch());
    let changed = if action == "join" {
        attach(&tx, g.id, bot_name, &at)?
    } else {
        let b = bot::get(&tx, bot_name)?;
        tx.execute(
            "DELETE FROM goal_bots WHERE goal_id=?1 AND bot_id=?2",
            params![g.id, b.id],
        )? == 1
    };
    if changed {
        let body = format!("bot {bot_name}\nwhy: {why}");
        event(&tx, g.id, actor, action, None, owner, &body, &at)?;
        touch(&tx, g.id, &at)?;
    }
    tx.commit()?;
    get(conn, name)
}

/// Appends one trajectory event; returns its id.
pub fn record(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    kind: &str,
    task: Option<i64>,
    text: &str,
) -> Result<i64> {
    if !TRAJECTORY_KINDS.contains(&kind) {
        bail!(
            "invalid trajectory kind {kind:?} (expected one of {})",
            TRAJECTORY_KINDS.join(", ")
        );
    }
    if text.trim().is_empty() {
        bail!("text must not be empty");
    }
    check_cap("text", text, RECORD_CAP)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let g = get(&tx, name)?;
    if let Some(t) = task {
        crate::db::get_task(&tx, t)?;
    }
    let at = ts(crate::remote::epoch());
    let id = event(&tx, g.id, actor, kind, task, None, text, &at)?;
    touch(&tx, g.id, &at)?;
    tx.commit()?;
    Ok(id)
}

/// The goal id for a link: `None` for `"none"`, else the goal, which must exist.
pub fn resolve(conn: &Connection, name: &str) -> Result<Option<Goal>> {
    if name.trim() == NONE {
        return Ok(None);
    }
    get(conn, name.trim()).map(Some)
}

/// A wake body labeled with its goal.
pub fn label(goal: &str, body: &str) -> String {
    format!("[goal {goal}] {body}")
}

/// The goal a wake body is labeled with by [`label`], if any.
pub fn wake_goal(body: &str) -> Option<&str> {
    body.strip_prefix("[goal ")?
        .split_once(']')
        .map(|(g, _)| g)
        .filter(|g| !g.is_empty())
}

/// The rolling window budgets are spent over.
pub const BUDGET_WINDOW_DAYS: i64 = 7;
/// Owner goal actions that each cost one attention unit.
pub const ATTENTION_KINDS: [&str; 3] = ["activated", "budget", "intent"];

/// The window a [`Budget`] covers, `[since, until)` in RFC3339.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "BudgetWindow"))]
pub struct Window {
    pub since: String,
    pub until: String,
    pub days: i64,
}

/// A goal's spend over the last 7 days against its weekly budgets. A budget of
/// `None` is unlimited and never gates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Budget {
    pub cost_week: Option<f64>,
    /// Estimated USD of the runs attributed to the goal.
    pub cost_spent: f64,
    pub attention_week: Option<i64>,
    /// Escalation episodes and approvals on linked tasks, plus owner goal
    /// actions (activate, budget, intent): one unit each.
    pub attention_spent: i64,
    /// Bot runs whose wakes mix this goal with other work; not in `cost_spent`.
    pub unattributed_runs: i64,
    /// Attributed runs with unknown usage; not in `cost_spent`.
    pub unknown_runs: i64,
    /// Cost of linked task runs.
    pub cost_tasks: f64,
    /// Cost of goal-tagged bot runs, per bot.
    pub cost_bots: std::collections::BTreeMap<String, f64>,
    pub window: Window,
}

impl Budget {
    /// Why the goal takes no new linked tasks: a weekly budget that is spent.
    pub fn exhausted(&self) -> Option<String> {
        let mut out = Vec::new();
        if let Some(limit) = self.cost_week.filter(|l| self.cost_spent >= *l) {
            out.push(format!("cost ${:.2} of ${limit:.2}", self.cost_spent));
        }
        if let Some(limit) = self.attention_week.filter(|l| self.attention_spent >= *l) {
            out.push(format!("attention {} of {limit}", self.attention_spent));
        }
        (!out.is_empty()).then(|| out.join(", "))
    }

    /// `spent/limit` for `goal list`, `-` for no limit.
    pub fn compact(&self) -> String {
        let cost = self
            .cost_week
            .map_or("-".into(), |l| format!("${:.2}/${l:.2}", self.cost_spent));
        let attention = self
            .attention_week
            .map_or("-".into(), |l| format!("{}/{l}", self.attention_spent));
        format!("cost={cost}  attention={attention}")
    }
}

/// Spend over one window for every goal, read once.
struct Spend {
    window: Window,
    costs: std::collections::HashMap<String, crate::usage::GoalCost>,
    attention: std::collections::HashMap<String, i64>,
}

impl Spend {
    fn load(conn: &Connection, now: i64) -> Result<Spend> {
        let since = now - BUDGET_WINDOW_DAYS * 86_400;
        Ok(Spend {
            window: Window {
                since: ts(since),
                until: ts(now),
                days: BUDGET_WINDOW_DAYS,
            },
            costs: crate::usage::goal_costs(conn, since, now + 1)?,
            attention: attention(conn, since, now + 1)?,
        })
    }

    fn budget(&self, g: &Goal) -> Budget {
        let c = self.costs.get(&g.name).cloned().unwrap_or_default();
        Budget {
            cost_week: g.budget_cost_week,
            cost_spent: c.cost,
            attention_week: g.budget_attention_week,
            attention_spent: self.attention.get(&g.name).copied().unwrap_or(0),
            unattributed_runs: c.unattributed_runs,
            unknown_runs: c.unknown_runs,
            cost_tasks: c.tasks,
            cost_bots: c.bots,
            window: self.window.clone(),
        }
    }
}

/// One goal's [`Budget`] over the 7 days before `now`.
pub fn budget(conn: &Connection, g: &Goal, now: i64) -> Result<Budget> {
    Ok(Spend::load(conn, now)?.budget(g))
}

/// Owner attention per goal in `[since, until)`: each escalation episode and
/// approval on a task linked to the goal at the time, and each owner goal
/// action ([`ATTENTION_KINDS`]).
pub fn attention(
    conn: &Connection,
    since: i64,
    until: i64,
) -> Result<std::collections::HashMap<String, i64>> {
    let mut out: std::collections::HashMap<String, i64> = Default::default();
    let in_window = |at: &str| -> Result<bool> {
        let t = crate::usage::epoch(at)?;
        Ok(t >= since && t < until)
    };
    let mut stmt = conn.prepare(
        "SELECT task_id, at, kind, from_value, to_value, body FROM events
         WHERE kind IN ('goal','created','state','approval') ORDER BY task_id, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut task = None;
    let mut goal: Option<String> = None;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if task != Some(id) {
            task = Some(id);
            goal = None;
        }
        let kind: String = r.get(2)?;
        let from: Option<String> = r.get(3)?;
        let to: Option<String> = r.get(4)?;
        if kind == "goal" {
            goal = to;
            continue;
        }
        let Some(g) = &goal else { continue };
        let body: Option<String> = r.get(5)?;
        let counts = match kind.as_str() {
            "created" => body.unwrap_or_default().starts_with("state=escalated"),
            "state" => to.as_deref() == Some("escalated") && from != to,
            _ => true,
        };
        if counts && in_window(&r.get::<_, String>(1)?)? {
            *out.entry(g.clone()).or_default() += 1;
        }
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT g.name, e.at FROM goal_events e JOIN goals g ON g.id=e.goal_id
         WHERE e.kind IN ({})",
        ATTENTION_KINDS.map(|k| format!("'{k}'")).join(",")
    ))?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        if in_window(&r.get::<_, String>(1)?)? {
            *out.entry(r.get(0)?).or_default() += 1;
        }
    }
    Ok(out)
}

/// The one hard gate: refuses linking a new task to a goal whose weekly cost
/// or attention budget is spent. A goal with no budget never gates.
pub fn check_budget(conn: &Connection, g: &Goal) -> Result<()> {
    if g.budget_cost_week.is_none() && g.budget_attention_week.is_none() {
        return Ok(());
    }
    let b = budget(conn, g, crate::remote::epoch())?;
    if let Some(why) = b.exhausted() {
        bail!(
            "goal {} is over its weekly budget ({why}, last {} days); \
             the owner can raise it with `axe goal edit {} --budget-cost-week N` \
             or `--budget-attention-week N`",
            g.name,
            BUDGET_WINDOW_DAYS,
            g.name
        );
    }
    Ok(())
}
