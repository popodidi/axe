use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub const STATES: [&str; 5] = ["pending", "in_progress", "blocked", "escalated", "done"];
/// Why a task needs escalation: a legitimate human call, a worker capability gap,
/// a coordination failure, or an axe/infrastructure failure.
pub const ESCALATION_CLASSES: [&str; 4] = ["decision", "capability", "coordination", "runtime"];
pub const WORKLOADS: [&str; 4] = ["prod-bug", "infra", "feature", "ops"];

const SCHEMA_VERSION: i64 = 16;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS dispatcher (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), id TEXT NOT NULL,
 generation TEXT NOT NULL, expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS dispatch_ops (
 id TEXT PRIMARY KEY, dispatcher TEXT NOT NULL, request TEXT NOT NULL, response TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS dispatch_runs (
 run_id INTEGER PRIMARY KEY REFERENCES runs(id), dispatcher TEXT NOT NULL,
 generation TEXT NOT NULL, phase TEXT NOT NULL DEFAULT 'prepared'
);
CREATE TABLE IF NOT EXISTS dispatch_claims (
 claim TEXT PRIMARY KEY, dispatcher TEXT NOT NULL, generation TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY,
  project TEXT NOT NULL,
  title TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  state TEXT NOT NULL CHECK (state IN ('pending','in_progress','blocked','escalated','done')),
  claimed_by TEXT, runs INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  not_before TEXT,
  agent TEXT
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL REFERENCES tasks(id),
  at TEXT NOT NULL, actor TEXT NOT NULL,
  session TEXT,
  kind TEXT NOT NULL,
  from_value TEXT, to_value TEXT, body TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
  task_id INTEGER PRIMARY KEY REFERENCES tasks(id),
  agent TEXT NOT NULL, session_id TEXT NOT NULL, resume_count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS rr (rule TEXT PRIMARY KEY, next INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL REFERENCES tasks(id),
  project TEXT NOT NULL, agent TEXT NOT NULL, mode TEXT NOT NULL,
  session_id TEXT, claim TEXT NOT NULL,
  started_at TEXT NOT NULL, ended_at TEXT, exit_code INTEGER, log_path TEXT NOT NULL,
  current_activity TEXT, last_activity_at TEXT
);
CREATE INDEX IF NOT EXISTS runs_task ON runs(task_id);
CREATE INDEX IF NOT EXISTS runs_session ON runs(session_id);
CREATE INDEX IF NOT EXISTS events_task ON events(task_id);
CREATE TABLE IF NOT EXISTS deps (
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  prereq_id INTEGER NOT NULL REFERENCES tasks(id),
  created_at TEXT NOT NULL,
  PRIMARY KEY (task_id, prereq_id),
  CHECK (task_id != prereq_id)
);
CREATE INDEX IF NOT EXISTS deps_prereq ON deps(prereq_id);
";

/// v8 to v9: one agent per task. Drops `tasks.owner`, `runs.persona` and the retired
/// `dream_attempts` table, and keeps
/// one session per task: the builder's (the implementation session) when present,
/// else the most recently prompted one. v8 sessions carry no timestamp;
/// `last_event_id` is the event watermark of their latest prompt, so it orders them.
const MIGRATE_V8: &str = "
DROP INDEX IF EXISTS tasks_owner_state;
DROP TABLE IF EXISTS dream_attempts;
INSERT INTO tasks_new (id, project, title, detail, state, claimed_by, runs, created_at,
  updated_at, not_before)
SELECT id, project, title, detail, state, claimed_by, runs, created_at, updated_at, not_before
FROM tasks;
DROP TABLE tasks; ALTER TABLE tasks_new RENAME TO tasks;
INSERT INTO sessions_new (task_id, agent, session_id, resume_count)
SELECT task_id, agent, session_id, resume_count FROM (
  SELECT s.*, ROW_NUMBER() OVER (PARTITION BY task_id
    ORDER BY persona = 'builder' DESC, last_event_id DESC, rowid DESC) AS n
  FROM sessions s)
WHERE n = 1;
DROP TABLE sessions; ALTER TABLE sessions_new RENAME TO sessions;
INSERT INTO runs_new (id, task_id, project, agent, mode, session_id, claim, started_at,
  ended_at, exit_code, log_path, current_activity, last_activity_at)
SELECT id, task_id, project, agent, mode, session_id, claim, started_at, ended_at, exit_code,
  log_path, current_activity, last_activity_at
FROM runs;
DROP TABLE runs; ALTER TABLE runs_new RENAME TO runs;
";

/// The v9 `CREATE TABLE` statement for `name`, renamed to `<name>_new`.
fn new_table(name: &str) -> String {
    let head = format!("CREATE TABLE IF NOT EXISTS {name} (");
    let start = SCHEMA.find(&head).expect("table in schema");
    let len = SCHEMA[start..].find(");").expect("table end") + 2;
    SCHEMA[start..start + len].replace(&head, &format!("CREATE TABLE {name}_new ("))
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("AXE_DB").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    crate::compat::home_path(".local/share/axe/axe.db").context("cannot determine home directory")
}

/// Opens the database: creates a fresh v16 schema, or migrates v8-v15 to v16. Older
/// schemas are refused; migrate them to v8 with an earlier axe release first.
/// v12, v13 and v15 only add bot tables (`bot::SCHEMA`), v14 only adds usage
/// tables (`usage::SCHEMA`) and v16 only adds goal tables and a nullable column
/// (`goal::migrate`), so a database lowered to `user_version` 15, 14, 13, 12 or
/// 11 still opens with that binary and upgrades again in place.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(conn);
    }
    if version > SCHEMA_VERSION {
        bail!("database schema {version} is newer than supported {SCHEMA_VERSION}");
    }
    let empty: bool = conn.query_row(
        "SELECT NOT EXISTS (SELECT 1 FROM sqlite_master WHERE type='table' AND name='tasks')",
        [],
        |r| r.get(0),
    )?;
    if empty {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.execute_batch(crate::routine::SCHEMA)?;
        tx.execute_batch(crate::bot::SCHEMA)?;
        tx.execute_batch(crate::usage::SCHEMA)?;
        crate::goal::migrate(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        return Ok(conn);
    }
    if (11..=15).contains(&version) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(crate::bot::SCHEMA)?;
        tx.execute_batch(crate::usage::SCHEMA)?;
        crate::goal::migrate(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        return Ok(conn);
    }
    if version == 9 || version == 10 {
        let tx = conn.unchecked_transaction()?;
        // A compatibility rollback can retain this additive column at v10.
        let has_agent: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name='agent')",
            [],
            |r| r.get(0),
        )?;
        if !has_agent {
            tx.execute_batch("ALTER TABLE tasks ADD COLUMN agent TEXT")?;
        }
        tx.execute_batch(crate::routine::SCHEMA)?;
        tx.execute_batch(crate::bot::SCHEMA)?;
        tx.execute_batch(crate::usage::SCHEMA)?;
        crate::goal::migrate(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        return Ok(conn);
    }
    if version != 8 {
        bail!(
            "database schema {version} is too old; migrate it to schema 8 with an earlier axe release first"
        );
    }
    // SQLite drops columns by rebuilding tables. Disable FK enforcement outside the
    // transaction; retain child tables and validate before commit.
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let tx = conn.unchecked_transaction()?;
    for t in ["tasks", "sessions", "runs"] {
        tx.execute_batch(&new_table(t))?;
    }
    tx.execute_batch(MIGRATE_V8)?;
    tx.execute_batch(SCHEMA)?;
    tx.execute_batch(crate::routine::SCHEMA)?;
    tx.execute_batch(crate::bot::SCHEMA)?;
    tx.execute_batch(crate::usage::SCHEMA)?;
    crate::goal::migrate(&tx)?;
    let violations: i64 =
        tx.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    if violations != 0 {
        bail!("schema migration found {violations} foreign key violations");
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Writes a consistent snapshot of the whole database, including commits still in
/// the WAL, to `to` with `VACUUM INTO`. Refuses to overwrite an existing file.
/// Unlike copying `tasks.db`, this never misses uncheckpointed transactions.
pub fn backup(conn: &Connection, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("backup target {} already exists", to.display());
    }
    let target = to
        .to_str()
        .with_context(|| format!("backup path {} is not UTF-8", to.display()))?;
    conn.execute("VACUUM INTO ?1", [target])
        .with_context(|| format!("backup to {}", to.display()))?;
    Ok(())
}

/// Fixed-width UTC format for wake times; string order is time order.
const WAKE_FORMAT: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

fn wake_now() -> String {
    OffsetDateTime::now_utc()
        .format(WAKE_FORMAT)
        .expect("UTC formatting cannot fail")
}

/// Parses a wake time: RFC3339 (`2026-10-29T20:00:00Z`, `...T20:00Z`) or relative
/// (`+90s`, `+30m`, `+2h`, `+1d`). Returns fixed-width UTC.
pub fn parse_wake(s: &str) -> Result<String> {
    let s = s.trim();
    let at = if let Some(rel) = s.strip_prefix('+') {
        let (n, unit) = rel.split_at(rel.len().saturating_sub(1));
        let n: i64 = n
            .parse()
            .with_context(|| format!("bad relative wake time {s:?}"))?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            "d" => n * 86400,
            _ => bail!("bad relative wake time {s:?}: use s, m, h or d"),
        };
        if n <= 0 {
            bail!("relative wake time {s:?} must be positive");
        }
        OffsetDateTime::now_utc() + time::Duration::seconds(secs)
    } else {
        // Accept minute precision by adding seconds before RFC3339 parsing.
        let full = if s.len() == 17 && s.ends_with('Z') {
            format!("{}:00Z", &s[..16])
        } else {
            s.to_string()
        };
        OffsetDateTime::parse(&full, &Rfc3339)
            .with_context(|| format!("bad wake time {s:?}: use RFC3339 or +<n>[smhd]"))?
    };
    Ok(at.to_offset(time::UtcOffset::UTC).format(WAKE_FORMAT)?)
}

pub fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC3339 formatting cannot fail for UTC")
}

/// Who is acting; recorded on every event.
#[derive(Debug, Clone)]
pub struct Actor {
    pub name: String,
    pub session: Option<String>,
}

impl Actor {
    pub fn new(name: &str) -> Self {
        Actor {
            name: name.to_string(),
            session: None,
        }
    }
}

/// SQL for a `tasks` row with no unfinished prerequisite. Dependency waits are never
/// stored: eligibility is always derived from the edges and prerequisite states.
const NO_UNMET: &str = "NOT EXISTS (SELECT 1 FROM deps d JOIN tasks p ON p.id = d.prereq_id
    WHERE d.task_id = tasks.id AND p.state != 'done')";

/// SQL for a `tasks` row whose wake time, if any, has passed. Wake times are stored
/// as fixed-width UTC (`WAKE_FORMAT`), so string comparison is time order.
const AWAKE: &str =
    "(tasks.not_before IS NULL OR tasks.not_before <= strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))";

/// SQL for a `tasks` row whose derived `status` is `blocked`: stored blocked,
/// or pending on an unmet prerequisite or a wake time still ahead.
pub(crate) fn waiting_sql() -> String {
    format!(
        "(tasks.state = 'blocked' OR (tasks.state = 'pending' AND NOT ({NO_UNMET} AND {AWAKE})))"
    )
}

/// Columns for [`Task::from_row`]: the stored row plus the derived `status`.
fn task_cols() -> String {
    format!(
        "tasks.*, CASE WHEN tasks.state = 'pending' AND NOT ({NO_UNMET} AND {AWAKE}) THEN 'blocked'
         ELSE tasks.state END AS status"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub detail: String,
    /// Stored state.
    pub state: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub claimed_by: Option<String>,
    pub runs: i64,
    pub created_at: String,
    pub updated_at: String,
    /// Wake time: a pending task is not ready before it (UTC, `WAKE_FORMAT`).
    #[serde(default)]
    pub not_before: Option<String>,
    /// Derived state users see: `blocked` for a stored hold and for a pending task
    /// waiting on prerequisites; otherwise the stored state.
    #[serde(default)]
    pub status: String,
    /// Derived: prerequisites that are not done yet.
    #[serde(default)]
    pub waiting_on: Vec<i64>,
    #[serde(default)]
    pub deps: Deps,
}

/// Dependency edges of one task, both directions, with each other task's status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Deps {
    /// Prerequisites: tasks that must be done before this one can be claimed.
    pub blocked_by: Vec<DepRef>,
    /// Dependents: tasks waiting on this one.
    pub blocks: Vec<DepRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DepRef {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub state: String,
    pub status: String,
}

impl Task {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Task {
            id: r.get("id")?,
            project: r.get("project")?,
            title: r.get("title")?,
            detail: r.get("detail")?,
            state: r.get("state")?,
            agent: r.get("agent")?,
            claimed_by: r.get("claimed_by")?,
            runs: r.get("runs")?,
            created_at: r.get("created_at")?,
            updated_at: r.get("updated_at")?,
            not_before: r.get("not_before")?,
            status: r.get("status")?,
            waiting_on: Vec::new(),
            deps: Deps::default(),
        })
    }

    /// Eligible to claim: stored pending, every prerequisite done, and awake.
    pub fn ready(&self) -> bool {
        self.state == "pending" && self.waiting_on.is_empty() && !self.sleeping()
    }

    /// Pending with a wake time still in the future.
    pub fn sleeping(&self) -> bool {
        self.not_before
            .as_deref()
            .is_some_and(|t| t > wake_now().as_str())
    }
}

fn dep_refs(conn: &Connection, join_on: &str, where_col: &str, id: i64) -> Result<Vec<DepRef>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM deps e JOIN tasks ON tasks.id = e.{join_on}
         WHERE e.{where_col} = ?1 ORDER BY tasks.id",
        task_cols()
    ))?;
    let rows = stmt.query_map([id], |r| {
        Ok(DepRef {
            id: r.get("id")?,
            project: r.get("project")?,
            title: r.get("title")?,
            state: r.get("state")?,
            status: r.get("status")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Fills the derived dependency fields of `t`.
fn attach_deps(conn: &Connection, t: &mut Task) -> Result<()> {
    let blocked_by = dep_refs(conn, "prereq_id", "task_id", t.id)?;
    t.waiting_on = blocked_by
        .iter()
        .filter(|d| d.state != "done")
        .map(|d| d.id)
        .collect();
    t.deps = Deps {
        blocked_by,
        blocks: dep_refs(conn, "task_id", "prereq_id", t.id)?,
    };
    Ok(())
}

fn query_tasks(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(sql)?;
    let mut tasks = stmt
        .query_map(p, Task::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for t in &mut tasks {
        attach_deps(conn, t)?;
    }
    Ok(tasks)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Event {
    pub id: i64,
    pub task_id: i64,
    pub at: String,
    pub actor: String,
    pub session: Option<String>,
    pub kind: String,
    pub from_value: Option<String>,
    pub to_value: Option<String>,
    pub body: Option<String>,
}

pub fn validate_state(state: &str) -> Result<()> {
    if !STATES.contains(&state) {
        bail!(
            "invalid state {state:?} (expected one of {})",
            STATES.join(", ")
        );
    }
    Ok(())
}

pub fn validate_escalation_class(class: &str) -> Result<()> {
    if !ESCALATION_CLASSES.contains(&class) {
        bail!(
            "invalid escalation class {class:?} (expected one of {})",
            ESCALATION_CLASSES.join(", ")
        );
    }
    Ok(())
}

pub fn validate_workload(workload: &str) -> Result<()> {
    if !WORKLOADS.contains(&workload) {
        bail!(
            "invalid workload {workload:?} (expected one of {})",
            WORKLOADS.join(", ")
        );
    }
    Ok(())
}

pub fn write_event(
    tx: &Transaction,
    actor: &Actor,
    task_id: i64,
    kind: &str,
    from: Option<&str>,
    to: Option<&str>,
    body: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO events (task_id, at, actor, session, kind, from_value, to_value, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            task_id,
            now(),
            actor.name,
            actor.session,
            kind,
            from,
            to,
            body
        ],
    )?;
    crate::subscription::fan_out(
        tx,
        &actor.name,
        tx.last_insert_rowid(),
        task_id,
        kind,
        from,
        to,
        body,
    )
}

/// Formats a block note: what blocks the task, who resolves it and how, and how
/// it resumes.
pub fn block_note(blocker: &str, resolver: &str, resume: &str) -> String {
    format!("blocked: {blocker}\nresolver: {resolver}\nresume: {resume}")
}

pub struct NewTask<'a> {
    pub agent: Option<&'a str>,
    pub project: &'a str,
    pub title: &'a str,
    pub detail: &'a str,
    pub state: &'a str,
}

pub fn add_task(conn: &mut Connection, actor: &Actor, t: &NewTask) -> Result<i64> {
    add_task_with_deps(conn, actor, t, &[])
}

/// Creates a task and its `blocked_by` edges in one transaction; nothing is
/// written if any edge is invalid.
pub fn add_task_with_deps(
    conn: &mut Connection,
    actor: &Actor,
    t: &NewTask,
    blocked_by: &[i64],
) -> Result<i64> {
    add_classified_task(conn, actor, t, blocked_by, None)
}

/// [`add_task_with_deps`] that also records the task's workload.
pub fn add_classified_task(
    conn: &mut Connection,
    actor: &Actor,
    t: &NewTask,
    blocked_by: &[i64],
    workload: Option<&str>,
) -> Result<i64> {
    add_linked_task(conn, actor, t, blocked_by, workload, None)
}

/// Writes a `goal` event linking a task to a goal (`none` unlinks); the goal
/// must exist. The value is the goal name; the latest event wins.
fn link_goal(tx: &Transaction, actor: &Actor, id: i64, goal: &str) -> Result<bool> {
    let goal = crate::goal::resolve(tx, goal)?;
    let to = goal.as_ref().map(|g| g.name.clone());
    let from = current_goal(tx, id)?;
    if from == to {
        return Ok(false);
    }
    if let Some(g) = &goal {
        crate::goal::check_budget(tx, g)?;
    }
    write_event(tx, actor, id, "goal", from.as_deref(), to.as_deref(), None)?;
    Ok(true)
}

/// The goal a task is linked to: the latest `goal` event's value.
pub fn current_goal(conn: &Connection, id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT to_value FROM events WHERE task_id=?1 AND kind='goal' ORDER BY id DESC LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// [`add_classified_task`] that also links the task to a goal.
pub fn add_linked_task(
    conn: &mut Connection,
    actor: &Actor,
    t: &NewTask,
    blocked_by: &[i64],
    workload: Option<&str>,
    goal: Option<&str>,
) -> Result<i64> {
    validate_state(t.state)?;
    if let Some(w) = workload {
        validate_workload(w)?;
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ts = now();
    tx.execute(
        "INSERT INTO tasks (project, title, detail, state, created_at, updated_at, agent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![t.project, t.title, t.detail, t.state, ts, t.agent],
    )?;
    let id = tx.last_insert_rowid();
    let body = format!("state={}", t.state);
    write_event(&tx, actor, id, "created", None, Some(t.title), Some(&body))?;
    if let Some(w) = workload {
        write_event(&tx, actor, id, "workload", None, Some(w), None)?;
    }
    if let Some(g) = goal {
        link_goal(&tx, actor, id, g)?;
    }
    for p in dedup(blocked_by) {
        add_dep_in(&tx, actor, id, p)?;
    }
    tx.commit()?;
    Ok(id)
}

fn dedup(ids: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    for &i in ids {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// Adds `task blocked_by prereq` inside `tx`; `Ok(false)` if the edge already
/// exists (adding is idempotent). Rejects missing tasks, self edges, cycles, and an
/// unfinished prerequisite on in_progress or done work, which would otherwise
/// strand a live worker or silently reopen completed work.
fn add_dep_in(tx: &Transaction, actor: &Actor, id: i64, prereq: i64) -> Result<bool> {
    let task = get_task_in(tx, id)?.with_context(|| format!("task {id} not found"))?;
    if prereq == id {
        bail!("task {id} cannot be blocked_by itself");
    }
    let Some(p) = get_task_in(tx, prereq)? else {
        bail!("prerequisite task {prereq} does not exist");
    };
    let exists: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM deps WHERE task_id = ?1 AND prereq_id = ?2)",
        [id, prereq],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    // A cycle exists if `id` is already a (transitive) prerequisite of `prereq`.
    let cycle: bool = tx.query_row(
        "WITH RECURSIVE up(id) AS (
           SELECT ?1 UNION SELECT d.prereq_id FROM deps d JOIN up ON d.task_id = up.id)
         SELECT EXISTS (SELECT 1 FROM up WHERE id = ?2)",
        [prereq, id],
        |r| r.get(0),
    )?;
    if cycle {
        bail!("task {id} blocked_by {prereq} would create a dependency cycle");
    }
    if p.state != "done" && (task.state == "in_progress" || task.state == "done") {
        bail!(
            "task {id} is {}; refusing to add unfinished prerequisite {prereq} ({}). \
             Block or finish the task first, or add the edge once {prereq} is done",
            task.state,
            p.state
        );
    }
    let ts = now();
    tx.execute(
        "INSERT INTO deps (task_id, prereq_id, created_at) VALUES (?1, ?2, ?3)",
        params![id, prereq, ts],
    )?;
    dep_events(tx, actor, "dep_add", id, prereq, &ts)?;
    Ok(true)
}

/// Removes `task blocked_by prereq` inside `tx`; `Ok(false)` if there was no edge.
fn remove_dep_in(tx: &Transaction, actor: &Actor, id: i64, prereq: i64) -> Result<bool> {
    get_task_in(tx, id)?.with_context(|| format!("task {id} not found"))?;
    if get_task_in(tx, prereq)?.is_none() {
        bail!("prerequisite task {prereq} does not exist");
    }
    let n = tx.execute(
        "DELETE FROM deps WHERE task_id = ?1 AND prereq_id = ?2",
        [id, prereq],
    )?;
    if n == 0 {
        return Ok(false);
    }
    dep_events(tx, actor, "dep_remove", id, prereq, &now())?;
    Ok(true)
}

/// Audits an edge change on both tasks: `blocked_by #P` on the dependent and
/// `blocks #T` on the prerequisite, each with the other task's id as `to_value`.
fn dep_events(
    tx: &Transaction,
    actor: &Actor,
    kind: &str,
    id: i64,
    prereq: i64,
    ts: &str,
) -> Result<()> {
    write_event(
        tx,
        actor,
        id,
        kind,
        None,
        Some(&prereq.to_string()),
        Some(&format!("blocked_by #{prereq}")),
    )?;
    write_event(
        tx,
        actor,
        prereq,
        kind,
        None,
        Some(&id.to_string()),
        Some(&format!("blocks #{id}")),
    )?;
    tx.execute(
        "UPDATE tasks SET updated_at = ?1 WHERE id IN (?2, ?3)",
        params![ts, id, prereq],
    )?;
    Ok(())
}

/// Adds and removes `blocked_by` edges of task `id` in one transaction. Returns
/// whether any edge changed; existing additions and missing removals are no-ops.
pub fn edit_deps(
    conn: &mut Connection,
    actor: &Actor,
    id: i64,
    add: &[i64],
    remove: &[i64],
) -> Result<bool> {
    if let Some(both) = add.iter().find(|a| remove.contains(a)) {
        bail!("prerequisite {both} is both added and removed");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut changed = false;
    for p in dedup(remove) {
        changed |= remove_dep_in(&tx, actor, id, p)?;
    }
    for p in dedup(add) {
        changed |= add_dep_in(&tx, actor, id, p)?;
    }
    if !changed {
        // Still validate the task exists for a no-op edit.
        get_task_in(&tx, id)?.with_context(|| format!("task {id} not found"))?;
    }
    tx.commit()?;
    Ok(changed)
}

fn get_task_in(conn: &Connection, id: i64) -> Result<Option<Task>> {
    let mut t = conn
        .query_row(
            &format!("SELECT {} FROM tasks WHERE id = ?1", task_cols()),
            [id],
            Task::from_row,
        )
        .optional()?;
    if let Some(t) = &mut t {
        attach_deps(conn, t)?;
    }
    Ok(t)
}

pub fn get_task(conn: &Connection, id: i64) -> Result<Task> {
    get_task_in(conn, id)?.with_context(|| format!("task {id} not found"))
}

#[derive(Debug, Default)]
pub struct Filter<'a> {
    pub state: Option<&'a str>,
    pub project: Option<&'a str>,
    pub include_done: bool,
}

/// Lists tasks; the `state` filter matches the derived `status`, so `blocked`
/// includes dependency waits and `pending` is only work that can be claimed.
pub fn list_tasks(conn: &Connection, f: &Filter) -> Result<Vec<Task>> {
    query_tasks(
        conn,
        &format!(
            "SELECT * FROM (SELECT {} FROM tasks)
             WHERE (?1 IS NULL OR status = ?1)
               AND (?2 IS NULL OR project = ?2)
               AND (?1 IS NOT NULL OR ?3 OR state != 'done')
             ORDER BY id",
            task_cols()
        ),
        params![f.state, f.project, f.include_done],
    )
}

pub fn events(conn: &Connection, task_id: i64) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, at, actor, session, kind, from_value, to_value, body
         FROM events WHERE task_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([task_id], |r| {
        Ok(Event {
            id: r.get(0)?,
            task_id: r.get(1)?,
            at: r.get(2)?,
            actor: r.get(3)?,
            session: r.get(4)?,
            kind: r.get(5)?,
            from_value: r.get(6)?,
            to_value: r.get(7)?,
            body: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Stats history in three queries instead of reads per task: every actual
/// state transition and run start (bodies omitted), the ids of tasks that are
/// routine occurrences, and every run's timing. The dashboard's Stats view
/// reads only these.
pub fn stats_history(conn: &Connection) -> Result<serde_json::Value> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, at, kind, from_value, to_value FROM events
         WHERE kind = 'run_start' OR (kind = 'state' AND from_value IS NOT to_value)
         ORDER BY id",
    )?;
    let events = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "task_id": r.get::<_, i64>(1)?,
                "at": r.get::<_, String>(2)?,
                "kind": r.get::<_, String>(3)?,
                "from_value": r.get::<_, Option<String>>(4)?,
                "to_value": r.get::<_, Option<String>>(5)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let routines = conn
        .prepare("SELECT task_id FROM routine_occurrences ORDER BY task_id")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let runs = conn
        .prepare("SELECT id, task_id, agent, started_at, ended_at FROM runs ORDER BY id")?
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "task_id": r.get::<_, i64>(1)?,
                "agent": r.get::<_, String>(2)?,
                "started_at": r.get::<_, String>(3)?,
                "ended_at": r.get::<_, Option<String>>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::json!({ "events": events, "routines": routines, "runs": runs }))
}

#[derive(Debug, Default, Clone)]
pub struct Changes {
    pub reset_runs: bool,
    pub state: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub note: Option<String>,
    /// Wake time for a pending task; any state change clears it unless set here.
    pub not_before: Option<String>,
    /// Why the task is being escalated; only valid when it ends up escalated.
    pub escalation_class: Option<String>,
    /// Link the task to a goal by name, or `none` to unlink; latest wins.
    pub goal: Option<String>,
}

/// Applies changes inside an existing transaction. When `expect_claim` is set the
/// update only happens if the task is still in_progress under that claim; returns
/// whether anything was applied.
pub fn apply_changes(
    tx: &Transaction,
    actor: &Actor,
    id: i64,
    c: &Changes,
    expect_claim: Option<&str>,
) -> Result<bool> {
    if c.state.as_deref().is_some_and(|s| s != "escalated") {
        let aborting: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs r JOIN dispatch_runs d ON d.run_id=r.id WHERE r.task_id=?1 AND r.ended_at IS NULL AND d.phase='abort_requested')", [id], |r| r.get(0))?;
        if aborting {
            bail!("abort awaiting termination confirmation; task must remain escalated");
        }
    }
    let task = get_task_in(tx, id)?.with_context(|| format!("task {id} not found"))?;
    if let Some(claim) = expect_claim {
        if task.state != "in_progress" || task.claimed_by.as_deref() != Some(claim) {
            return Ok(false);
        }
    }
    if let Some(s) = &c.state {
        validate_state(s)?;
    }
    if let Some(class) = &c.escalation_class {
        validate_escalation_class(class)?;
        if c.state.as_deref().unwrap_or(&task.state) != "escalated" {
            bail!("task {id}: an escalation class needs the task to be escalated");
        }
    }
    if c.state.as_deref() == Some("in_progress")
        && task.state != "in_progress"
        && !task.waiting_on.is_empty()
    {
        bail!(
            "task {id} is waiting on unfinished prerequisites {}; it cannot start until they are done",
            ids(&task.waiting_on)
        );
    }

    let not_before = c.not_before.as_deref().map(parse_wake).transpose()?;
    if not_before.is_some() && c.state.as_deref().unwrap_or(&task.state) != "pending" {
        bail!("task {id}: a wake time needs the task to be pending");
    }

    if c.reset_runs
        && task.state == "in_progress"
        && task
            .claimed_by
            .as_deref()
            .is_some_and(|claim| claim.starts_with("dispatch:"))
    {
        bail!("cannot reset runs for an in_progress task under a dispatcher claim");
    }

    let mut changed = false;
    if c.reset_runs {
        tx.execute("UPDATE tasks SET runs = 0 WHERE id = ?1", [id])?;
        write_event(
            tx,
            actor,
            id,
            "runs",
            Some(&task.runs.to_string()),
            Some("0"),
            None,
        )?;
        changed = true;
    }
    if let Some(s) = c.state.as_deref().filter(|s| *s != task.state) {
        tx.execute("UPDATE tasks SET state = ?1 WHERE id = ?2", params![s, id])?;
        if s != "in_progress" {
            tx.execute("UPDATE tasks SET claimed_by = NULL WHERE id = ?1", [id])?;
        }
        write_event(tx, actor, id, "state", Some(&task.state), Some(s), None)?;
        if task.state == "done" {
            // Reopening a prerequisite never cancels or reopens dependents: pending
            // ones simply stop being eligible. Flag the rest so it is not silent.
            for d in &task.deps.blocks {
                write_event(
                    tx,
                    actor,
                    d.id,
                    "dep_reopened",
                    Some("done"),
                    Some(s),
                    Some(&format!(
                        "prerequisite #{id} reopened while this task was {}; not cancelled or reopened, review whether it needs rework",
                        d.state
                    )),
                )?;
            }
        }
        changed = true;
    }
    let entering = c.state.as_deref() == Some("escalated") && task.state != "escalated";
    let class = c
        .escalation_class
        .as_deref()
        .or((entering && actor.name == "dispatcher").then_some("runtime"));
    if let Some(class) = class {
        write_event(tx, actor, id, "escalation_class", None, Some(class), None)?;
        changed = true;
    }
    // Any state change clears the wake time unless this change sets a new one.
    let state_changed = c.state.as_deref().is_some_and(|s| s != task.state);
    if (not_before.is_some() || state_changed) && not_before != task.not_before {
        tx.execute(
            "UPDATE tasks SET not_before = ?1 WHERE id = ?2",
            params![not_before, id],
        )?;
        write_event(
            tx,
            actor,
            id,
            "wake",
            task.not_before.as_deref(),
            not_before.as_deref(),
            None,
        )?;
        changed = true;
    }
    if let Some(t) = c.title.as_deref().filter(|t| *t != task.title) {
        tx.execute("UPDATE tasks SET title = ?1 WHERE id = ?2", params![t, id])?;
        write_event(
            tx,
            actor,
            id,
            "edit",
            Some(&task.title),
            Some(t),
            Some("title"),
        )?;
        changed = true;
    }
    if let Some(d) = c.detail.as_deref().filter(|d| *d != task.detail) {
        tx.execute("UPDATE tasks SET detail = ?1 WHERE id = ?2", params![d, id])?;
        let diff = similar::TextDiff::from_lines(task.detail.as_str(), d)
            .unified_diff()
            .header("detail", "detail")
            .to_string();
        write_event(tx, actor, id, "edit", None, None, Some(&diff))?;
        changed = true;
    }
    if let Some(g) = &c.goal {
        changed |= link_goal(tx, actor, id, g)?;
    }
    if let Some(n) = &c.note {
        write_event(tx, actor, id, "note", None, None, Some(n))?;
        changed = true;
    }
    if changed {
        tx.execute(
            "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
    }
    Ok(changed)
}

/// Formats task ids as `#1, #2`.
pub fn ids(ids: &[i64]) -> String {
    ids.iter()
        .map(|i| format!("#{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn set_task(conn: &mut Connection, actor: &Actor, id: i64, c: &Changes) -> Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed = apply_changes(&tx, actor, id, c, None)?;
    tx.commit()?;
    Ok(changed)
}

/// Corrects a task's escalation class and/or its workload; latest wins. The class
/// applies to the latest escalation episode, or to the 1-based `episode` (in
/// event order; see `axe log`), which is recorded as `episode=N`.
pub fn classify(
    conn: &mut Connection,
    actor: &Actor,
    id: i64,
    escalation: Option<&str>,
    workload: Option<&str>,
    episode: Option<i64>,
) -> Result<()> {
    if escalation.is_none() && workload.is_none() {
        bail!("classify needs an escalation class or a workload");
    }
    if episode.is_some() && escalation.is_none() {
        bail!("an episode needs an escalation class");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    get_task_in(&tx, id)?.with_context(|| format!("task {id} not found"))?;
    if let Some(class) = escalation {
        validate_escalation_class(class)?;
        let episodes: i64 = tx.query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND (
               (kind = 'state' AND to_value = 'escalated' AND from_value IS NOT 'escalated')
               OR (kind = 'created' AND body LIKE 'state=escalated%'))",
            [id],
            |r| r.get(0),
        )?;
        if episodes == 0 {
            bail!("task {id} has never been escalated");
        }
        let body = match episode {
            Some(n) if !(1..=episodes).contains(&n) => {
                bail!("task {id} has escalation episodes 1..={episodes}, not {n}")
            }
            Some(n) => Some(format!("episode={n}")),
            None => None,
        };
        write_event(
            &tx,
            actor,
            id,
            "escalation_class",
            None,
            Some(class),
            body.as_deref(),
        )?;
    }
    if let Some(w) = workload {
        validate_workload(w)?;
        write_event(&tx, actor, id, "workload", None, Some(w), None)?;
    }
    tx.execute(
        "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
        params![now(), id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Appends a `note` or `artifact` event.
pub fn append(conn: &mut Connection, actor: &Actor, id: i64, kind: &str, body: &str) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    get_task_in(&tx, id)?.with_context(|| format!("task {id} not found"))?;
    write_event(&tx, actor, id, kind, None, None, Some(body))?;
    tx.execute(
        "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
        params![now(), id],
    )?;
    tx.commit()?;
    Ok(())
}

/// An approval refused because the caller is a live dispatcher session.
#[derive(Debug)]
pub struct Forbidden(pub String);

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Forbidden {}

fn single_word(name: &str, v: &str) -> Result<()> {
    if v.is_empty() || v.chars().any(char::is_whitespace) {
        bail!("{name} must be a non-empty single word, got {v:?}");
    }
    Ok(())
}

/// Records an `approval` event on a blocked or escalated task and resumes it as
/// pending, in one transaction. The agent still re-checks every invariant against
/// the approved sha and scope; approval only records the owner's decision. The
/// dispatcher, dispatched agents, and any session belonging to an open run are
/// refused with [`Forbidden`]; this is a guardrail, not a security boundary.
pub fn approve(
    conn: &mut Connection,
    actor: &Actor,
    id: i64,
    sha: &str,
    scope: &str,
    note: Option<&str>,
) -> Result<()> {
    if matches!(actor.name.as_str(), "agent" | "dispatcher")
        || actor
            .session
            .as_deref()
            .is_some_and(|s| s.starts_with("dispatch:"))
    {
        return Err(Forbidden("the dispatcher and dispatched agents cannot approve".into()).into());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(s) = &actor.session {
        let live: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM runs WHERE ended_at IS NULL
               AND (session_id = ?1 OR claim = ?1))",
            [s],
            |r| r.get(0),
        )?;
        if live {
            return Err(Forbidden(format!(
                "session {s} is a running dispatcher session; dispatched agents cannot approve"
            ))
            .into());
        }
    }
    single_word("sha", sha)?;
    single_word("scope", scope)?;
    let task = get_task_in(&tx, id)?.with_context(|| format!("task {id} not found"))?;
    if !matches!(task.state.as_str(), "blocked" | "escalated") {
        bail!(
            "task {id} is {}; only blocked or escalated tasks can be approved",
            task.state
        );
    }
    let mut body = format!("sha={sha} scope={scope} by={}", actor.name);
    if let Some(n) = note.filter(|n| !n.is_empty()) {
        body.push('\n');
        body.push_str(n);
    }
    write_event(&tx, actor, id, "approval", None, None, Some(&body))?;
    let c = Changes {
        state: Some("pending".into()),
        ..Default::default()
    };
    apply_changes(&tx, actor, id, &c, None)?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Claim {
    Claimed(Box<Task>),
    /// The oldest ready task hit max_runs and was escalated instead.
    Blocked(i64),
    None,
}

/// Atomically claims the oldest ready task: pending with every prerequisite done,
/// checked in the same transaction as the claim. Tasks still waiting on
/// prerequisites are skipped, so they use no run budget and never hold back ready
/// work. With `max_runs`, a ready task whose runs already reached the limit is
/// escalated rather than claimed.
pub fn claim(
    conn: &mut Connection,
    actor: &Actor,
    project: Option<&str>,
    by: &str,
    max_runs: Option<i64>,
) -> Result<Claim> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = claim_in(&tx, actor, project, by, max_runs)?;
    tx.commit()?;
    Ok(result)
}

pub fn claim_in(
    tx: &Transaction,
    actor: &Actor,
    project: Option<&str>,
    by: &str,
    max_runs: Option<i64>,
) -> Result<Claim> {
    let Some((id, runs)) = oldest_ready(tx, project)? else {
        return Ok(Claim::None);
    };
    if let Some(max) = max_runs.filter(|m| runs >= *m) {
        let c = Changes {
            state: Some("escalated".into()),
            note: Some(block_note(
                &format!("max_runs exceeded ({runs} >= {max})"),
                "ops: review the run history and decide whether more runs are warranted",
                &format!("axe set {id} --reset-runs --state pending"),
            )),
            ..Default::default()
        };
        apply_changes(tx, actor, id, &c, None)?;
        return Ok(Claim::Blocked(id));
    }
    let claimed: Option<i64> = tx
        .query_row(
            "UPDATE tasks SET state = 'in_progress', claimed_by = ?1, runs = runs + 1, updated_at = ?2,
               not_before = NULL
             WHERE id = ?3 AND state = 'pending' RETURNING id",
            params![by, now(), id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = claimed else {
        return Ok(Claim::None);
    };
    write_event(tx, actor, id, "claimed", Some("pending"), Some(by), None)?;
    let task = get_task_in(tx, id)?.context("claimed task vanished")?;
    Ok(Claim::Claimed(Box::new(task)))
}

/// The oldest ready task (id, runs), as `claim_in` would pick it.
fn oldest_ready(tx: &Transaction, project: Option<&str>) -> Result<Option<(i64, i64)>> {
    Ok(tx
        .query_row(
            &format!(
                "SELECT id, runs FROM tasks WHERE state = 'pending'
                   AND (?1 IS NULL OR project = ?1) AND {NO_UNMET} AND {AWAKE} ORDER BY id LIMIT 1"
            ),
            params![project],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Escalates the task `claim_in` would pick next in `project`, without claiming
/// it or spending run budget: its project cannot host a run yet.
pub fn hold_in(tx: &Transaction, actor: &Actor, project: &str, blocker: &str) -> Result<Claim> {
    let Some((id, _)) = oldest_ready(tx, Some(project))? else {
        return Ok(Claim::None);
    };
    let c = Changes {
        state: Some("escalated".into()),
        note: Some(block_note(
            blocker,
            "ops: give the project a canonical Git clone under workspace_root with origin/HEAD set",
            &format!("axe set {id} --state pending"),
        )),
        ..Default::default()
    };
    apply_changes(tx, actor, id, &c, None)?;
    Ok(Claim::Blocked(id))
}

/// Returns the next round-robin index for `rule` and advances the cursor.
pub fn rr_next(conn: &mut Connection, rule: &str, len: usize) -> Result<usize> {
    if len == 0 {
        bail!("round-robin over an empty agent list");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let next: i64 = tx
        .query_row("SELECT next FROM rr WHERE rule = ?1", [rule], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    tx.execute(
        "INSERT INTO rr (rule, next) VALUES (?1, ?2)
         ON CONFLICT(rule) DO UPDATE SET next = excluded.next",
        params![rule, next + 1],
    )?;
    tx.commit()?;
    Ok((next.rem_euclid(len as i64)) as usize)
}

/// Projects with ready tasks (pending, prerequisites done, awake), ordered by
/// their oldest ready task.
pub fn pending_projects(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT project FROM tasks WHERE state = 'pending' AND {NO_UNMET} AND {AWAKE}
         GROUP BY project ORDER BY MIN(id)"
    ))?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn in_progress_claims(conn: &Connection, prefix: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, claimed_by FROM tasks
         WHERE state = 'in_progress' AND substr(claimed_by, 1, length(?1)) = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The one agent session a task resumes across runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub agent: String,
    pub session_id: String,
    pub resume_count: u64,
}

pub fn session(conn: &Connection, task: i64) -> Result<Option<Session>> {
    Ok(conn
        .query_row(
            "SELECT agent, session_id, resume_count FROM sessions WHERE task_id = ?1",
            [task],
            |r| {
                Ok(Session {
                    agent: r.get(0)?,
                    session_id: r.get(1)?,
                    resume_count: r.get(2)?,
                })
            },
        )
        .optional()?)
}

pub fn save_session(conn: &Connection, task: i64, s: &Session) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (task_id, agent, session_id, resume_count) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(task_id) DO UPDATE SET agent = excluded.agent,
           session_id = excluded.session_id, resume_count = excluded.resume_count",
        params![task, s.agent, s.session_id, s.resume_count],
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, task: i64) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE task_id = ?1", [task])?;
    Ok(())
}

/// One dispatcher run (one spawned agent process).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "Run"))]
pub struct RunRecord {
    pub id: i64,
    pub task_id: i64,
    pub project: String,
    pub agent: String,
    pub mode: String,
    pub session_id: Option<String>,
    pub claim: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub current_activity: Option<String>,
    pub last_activity_at: Option<String>,
    pub live: bool,
}

impl RunRecord {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        let ended_at: Option<String> = r.get("ended_at")?;
        Ok(RunRecord {
            id: r.get("id")?,
            task_id: r.get("task_id")?,
            project: r.get("project")?,
            agent: r.get("agent")?,
            mode: r.get("mode")?,
            session_id: r.get("session_id")?,
            claim: r.get("claim")?,
            started_at: r.get("started_at")?,
            live: ended_at.is_none(),
            ended_at,
            exit_code: r.get("exit_code")?,
            log_path: r.get("log_path")?,
            current_activity: r.get("current_activity")?,
            last_activity_at: r.get("last_activity_at")?,
        })
    }
}

pub struct NewRun<'a> {
    pub task_id: i64,
    pub project: &'a str,
    pub agent: &'a str,
    pub mode: &'a str,
    pub session_id: Option<&'a str>,
    pub claim: &'a str,
    pub log_path: &'a str,
}

pub fn insert_run(conn: &Connection, r: &NewRun) -> Result<i64> {
    conn.execute(
        "INSERT INTO runs (task_id, project, agent, mode, session_id, claim, started_at, log_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            r.task_id,
            r.project,
            r.agent,
            r.mode,
            r.session_id,
            r.claim,
            now(),
            r.log_path
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Marks a run ended; a known `session_id` replaces the stored one.
pub fn end_run(
    conn: &Connection,
    id: i64,
    exit_code: Option<i32>,
    session_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE runs SET ended_at = ?1, exit_code = ?2, session_id = COALESCE(?3, session_id) WHERE id = ?4",
        params![now(), exit_code, session_id, id],
    )?;
    Ok(())
}

pub fn set_run_activity(conn: &Connection, id: i64, activity: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET current_activity = ?1, last_activity_at = ?2 WHERE id = ?3",
        params![activity, now(), id],
    )?;
    Ok(())
}

/// Ends runs left open by a previous dispatcher process.
pub fn end_open_runs(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE runs SET ended_at = ?1 WHERE ended_at IS NULL",
        [now()],
    )?)
}

#[derive(Debug, Default)]
pub struct RunFilter {
    pub task: Option<i64>,
    pub live: bool,
    pub limit: i64,
}

/// Runs, newest first.
pub fn list_runs(conn: &Connection, f: &RunFilter) -> Result<Vec<RunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM runs WHERE (?1 IS NULL OR task_id = ?1) AND (NOT ?2 OR ended_at IS NULL)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![f.task, f.live, f.limit], RunRecord::from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Resolve exactly one recorded attempt, never a client path.
pub fn get_run(conn: &Connection, id: i64) -> Result<RunRecord> {
    conn.query_row(
        "SELECT * FROM runs WHERE id = ?1",
        [id],
        RunRecord::from_row,
    )
    .optional()?
    .with_context(|| format!("run {id} not found"))
}

/// Older dispatchers could reuse a path after resetting a task's run budget.
pub fn run_log_reused(conn: &Connection, run: &RunRecord) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM runs WHERE log_path = ?1 AND id > ?2)",
        params![run.log_path, run.id],
        |r| r.get(0),
    )?)
}

/// Maps an agent session id (or a dispatcher claim used as AXE_SESSION) to its task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub task_id: i64,
    pub project: String,
    pub agent: String,
    pub run: Option<RunRecord>,
}

pub fn find_session(conn: &Connection, session_id: &str) -> Result<Option<SessionInfo>> {
    let run = conn
        .query_row(
            "SELECT * FROM runs WHERE session_id = ?1 OR claim = ?1 ORDER BY id DESC LIMIT 1",
            [session_id],
            RunRecord::from_row,
        )
        .optional()?;
    if let Some(run) = run {
        return Ok(Some(SessionInfo {
            session_id: session_id.to_string(),
            task_id: run.task_id,
            project: run.project.clone(),
            agent: run.agent.clone(),
            run: Some(run),
        }));
    }
    Ok(conn
        .query_row(
            "SELECT s.task_id, t.project, s.agent FROM sessions s
             JOIN tasks t ON t.id = s.task_id WHERE s.session_id = ?1 LIMIT 1",
            [session_id],
            |r| {
                Ok(SessionInfo {
                    session_id: session_id.to_string(),
                    task_id: r.get(0)?,
                    project: r.get(1)?,
                    agent: r.get(2)?,
                    run: None,
                })
            },
        )
        .optional()?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Count {
    pub project: String,
    /// Derived task `status` (dependency waits count as blocked).
    pub state: String,
    pub count: i64,
}

/// A held or dependency-waiting task in the ops queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    #[serde(flatten)]
    pub task: Task,
    /// `hold`: stored blocked or escalated; only escalated is actionable.
    /// `dependency`: pending and only waiting on prerequisites in `waiting_on`; it
    /// becomes ready on its own when they are done, so no ops decision is needed.
    pub kind: String,
    pub actionable: bool,
    /// Actor that last moved the task to blocked.
    pub blocked_by: Option<String>,
    pub blocked_at: Option<String>,
    /// Latest note since the task last blocked (else the latest note).
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overview {
    pub counts: Vec<Count>,
    /// Every blocked or escalated task: stored holds
    /// (`kind` hold, actionable by ops) and pending tasks waiting on prerequisites
    /// (`kind` dependency, not actionable). Ready, running and done tasks are excluded.
    pub ops_queue: Vec<QueueItem>,
    /// Live dispatcher runs.
    pub running: i64,
}

fn queue_item(conn: &Connection, task: Task) -> Result<QueueItem> {
    if !matches!(task.state.as_str(), "blocked" | "escalated") {
        // A dependency wait: the reason is the unfinished prerequisites themselves
        // (`waiting_on`, `deps`), never a free-text note.
        return Ok(QueueItem {
            kind: "dependency".into(),
            actionable: false,
            task,
            blocked_by: None,
            blocked_at: None,
            blocker: None,
        });
    }
    let blocked: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, actor, at FROM events WHERE task_id = ?1 AND kind = 'state'
               AND to_value = ?2 ORDER BY id DESC LIMIT 1",
            params![task.id, task.state],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let since = blocked.as_ref().map_or(0, |b| b.0);
    let note = |since: i64| -> Result<Option<String>> {
        Ok(conn
            .query_row(
                "SELECT body FROM events WHERE task_id = ?1 AND kind = 'note' AND id > ?2
                 ORDER BY id DESC LIMIT 1",
                params![task.id, since],
                |r| r.get(0),
            )
            .optional()?)
    };
    let blocker = match note(since)? {
        Some(n) => Some(n),
        None => note(0)?,
    };
    let (blocked_by, blocked_at) = blocked.map_or((None, None), |b| (Some(b.1), Some(b.2)));
    Ok(QueueItem {
        kind: "hold".into(),
        actionable: task.state == "escalated",
        task,
        blocked_by,
        blocked_at,
        blocker,
    })
}

pub fn overview(conn: &Connection) -> Result<Overview> {
    let mut stmt = conn.prepare(&format!(
        "SELECT project, status, COUNT(*) FROM (SELECT {} FROM tasks)
         GROUP BY project, status ORDER BY project, status",
        task_cols()
    ))?;
    let counts = stmt
        .query_map([], |r| {
            Ok(Count {
                project: r.get(0)?,
                state: r.get(1)?,
                count: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let blocked = query_tasks(
        conn,
        &format!(
            "SELECT * FROM (SELECT {} FROM tasks) WHERE status IN ('blocked', 'escalated') ORDER BY id",
            task_cols()
        ),
        [],
    )?;
    let ops_queue = blocked
        .into_iter()
        .map(|t| queue_item(conn, t))
        .collect::<Result<_>>()?;
    let running = conn.query_row(
        "SELECT COUNT(*) FROM runs WHERE ended_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(Overview {
        counts,
        ops_queue,
        running,
    })
}
