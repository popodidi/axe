//! Dispatcher protocol. All durable queue mutations run under the server's DB lock.
use crate::{
    client::Client,
    db::{self, Actor, Changes},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str().with_context(|| format!("missing {k}"))
}
fn number(v: &Value, k: &str) -> Result<i64> {
    v[k].as_i64().with_context(|| format!("missing {k}"))
}

/// Reconnection retains its generation; expired ownership is not transferable
/// while claims or runs are unresolved. An explicit operator reconciliation is required.
pub fn register(conn: &Connection, v: &Value) -> Result<Value> {
    let id = field(v, "id")?;
    let requested = field(v, "generation")?;
    let generated = uuid::Uuid::new_v4().to_string();
    let generation = if requested.is_empty() {
        generated.as_str()
    } else {
        requested
    };
    let tx = conn.unchecked_transaction()?;
    let old: Option<(String, String, i64)> = tx
        .query_row(
            "SELECT id,generation,expires FROM dispatcher WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((old_id, old_generation, expires)) = old {
        if old_id == id && requested.is_empty() {
            return Ok(json!({"generation":old_generation}));
        }
        if old_id != id || old_generation != generation {
            let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE ended_at IS NULL) OR EXISTS(SELECT 1 FROM tasks WHERE state='in_progress' AND claimed_by LIKE 'dispatch:%') OR EXISTS(SELECT 1 FROM bot_runs WHERE ended_at IS NULL)",[],|r|r.get(0))?;
            if expires > epoch() || active {
                bail!("dispatcher ownership is active or unresolved; reconcile before replacement");
            }
        }
    }
    let unresolved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE ended_at IS NULL AND id NOT IN (SELECT run_id FROM dispatch_runs)) OR EXISTS(SELECT 1 FROM tasks WHERE state='in_progress' AND claimed_by LIKE 'dispatch:%' AND claimed_by NOT IN (SELECT claim FROM dispatch_claims))",[],|r|r.get(0))?;
    if unresolved {
        bail!("unowned dispatcher runs or claims require explicit reconciliation");
    }
    tx.execute("INSERT INTO dispatcher VALUES(1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET id=?1,generation=?2,expires=?3",params![id,generation,epoch()+60])?;
    tx.commit()?;
    Ok(json!({"generation":generation}))
}
pub fn heartbeat(conn: &Connection, id: &str, generation: &str) -> Result<()> {
    if conn.execute(
        "UPDATE dispatcher SET expires=?3 WHERE id=?1 AND generation=?2",
        params![id, generation, epoch() + 60],
    )? != 1
    {
        bail!("stale dispatcher ownership");
    }
    Ok(())
}

/// The run id is mandatory: retrying a request must never select a newer run.
pub fn abort(conn: &Connection, task: i64, run: i64, actor: &Actor) -> Result<Value> {
    let tx = conn.unchecked_transaction()?;
    let task_state = db::get_task(&tx, task)?.state;
    let r = db::get_run(&tx, run)?;
    if r.task_id != task {
        bail!("run does not belong to task");
    }
    let phase: String = tx.query_row(
        "SELECT phase FROM dispatch_runs WHERE run_id=?1",
        [run],
        |r| r.get(0),
    )?;
    if phase == "aborted" {
        return Ok(json!({"run":run,"status":"confirmed","task_state":task_state}));
    }
    if r.ended_at.is_some() {
        bail!("run already ended; nothing aborted");
    }
    let current: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM dispatch_runs r JOIN dispatcher d ON r.dispatcher=d.id AND r.generation=d.generation WHERE r.run_id=?1)", [run], |r| r.get(0))?;
    if !current {
        bail!("stale dispatcher generation; reconcile the run");
    }
    if phase != "abort_requested" {
        tx.execute(
            "UPDATE dispatch_runs SET phase='abort_requested' WHERE run_id=?1",
            [run],
        )?;
        db::write_event(
            &tx,
            actor,
            task,
            "abort_requested",
            None,
            Some(&run.to_string()),
            Some("termination requested; awaiting dispatcher confirmation"),
        )?;
        db::apply_changes(
            &tx,
            actor,
            task,
            &Changes {
                state: Some("escalated".into()),
                note: Some("abort requested; awaiting termination confirmation".into()),
                ..Default::default()
            },
            None,
        )?;
    }
    tx.commit()?;
    Ok(json!({"run":run,"status":"requested"}))
}

/// Whether the run's latest protection event set it. Events are per run, so
/// protection ends with the run.
fn run_protected(conn: &Connection, task: i64, run: i64) -> Result<bool> {
    let kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM events WHERE task_id=?1 AND to_value=?2
             AND kind IN ('protected','unprotected') ORDER BY id DESC LIMIT 1",
            params![task, run.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(kind.as_deref() == Some("protected"))
}

/// Marks the task's live run as inside (or, with `clear`, past) a
/// non-repeatable step. A bounded drain waits for protected runs. Refused once
/// the run is being aborted, so the caller never starts the step.
pub fn protect(
    conn: &Connection,
    task: i64,
    clear: bool,
    note: Option<&str>,
    actor: &Actor,
) -> Result<Value> {
    let tx = conn.unchecked_transaction()?;
    let live: Option<(i64, String)> = tx
        .query_row(
            "SELECT r.id,d.phase FROM runs r JOIN dispatch_runs d ON d.run_id=r.id
             WHERE r.task_id=?1 AND r.ended_at IS NULL ORDER BY r.id DESC LIMIT 1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((run, phase)) = live else {
        bail!("task {task} has no live run to protect");
    };
    if !clear && matches!(phase.as_str(), "drain_abort" | "abort_requested") {
        bail!("run {run} is being aborted; do not start the step");
    }
    let kind = if clear { "unprotected" } else { "protected" };
    db::write_event(&tx, actor, task, kind, None, Some(&run.to_string()), note)?;
    tx.commit()?;
    Ok(json!({"run":run,"protected":!clear}))
}

pub fn pending_aborts(conn: &Connection, id: &str, generation: &str) -> Result<Vec<i64>> {
    let mut q = conn.prepare("SELECT d.run_id FROM dispatch_runs d JOIN runs r ON r.id=d.run_id WHERE d.dispatcher=?1 AND d.generation=?2 AND d.phase='abort_requested' AND r.ended_at IS NULL")?;
    let rows = q
        .query_map(params![id, generation], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One operation and its replay result commit together.
pub fn operation(conn: &Connection, v: &Value) -> Result<Value> {
    let id = field(v, "dispatcher")?;
    let generation = field(v, "generation")?;
    let op_id = field(v, "operation_id")?;
    let op = field(v, "op")?;
    let b = &v["body"];
    let tx = conn.unchecked_transaction()?;
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM dispatcher WHERE id=?1 AND generation=?2 AND expires>?3)",
        params![id, generation, epoch()],
        |r| r.get(0),
    )?;
    if !valid {
        let same: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM dispatcher WHERE id=?1 AND generation=?2)",
            params![id, generation],
            |r| r.get(0),
        )?;
        if same {
            bail!("dispatcher lease expired");
        }
        bail!("stale dispatcher ownership");
    }
    let request = serde_json::to_string(&json!({"op":op,"body":b}))?;
    let prior: Option<(String, String, String)> = tx
        .query_row(
            "SELECT dispatcher,request,response FROM dispatch_ops WHERE id=?1",
            [op_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((owner, req, response)) = prior {
        if owner != id || req != request {
            bail!("operation ID reused with different request");
        }
        return Ok(serde_json::from_str(&response)?);
    }
    let actor = Actor::new("dispatcher");
    let task = || number(b, "task");
    let claim = || field(b, "claim");
    let check_claim = || -> Result<()> {
        let valid: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM dispatch_claims WHERE claim=?1 AND dispatcher=?2 AND generation=?3)",params![claim()?,id,generation],|r|r.get(0))?;
        if !valid {
            bail!("claim does not belong to dispatcher");
        }
        Ok(())
    };
    let check_run = || -> Result<i64> {
        let run = number(b, "run")?;
        let valid: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM dispatch_runs WHERE run_id=?1 AND dispatcher=?2 AND generation=?3)",params![run,id,generation],|r|r.get(0))?;
        if !valid {
            bail!("run does not belong to dispatcher");
        }
        Ok(run)
    };
    let check_bot_run = || -> Result<i64> {
        let run = number(b, "run")?;
        if !crate::bot::run_owned(&tx, run, id, generation)? {
            bail!("bot run does not belong to dispatcher");
        }
        Ok(run)
    };
    let result = match op {
        "bot_ready" => json!(crate::bot::ready(&tx, epoch())?),
        "bot_control" => json!(crate::bot::control(&tx, id, generation)?),
        "bot_claim" => json!(crate::bot::claim(
            &tx,
            field(b, "bot")?,
            claim()?,
            id,
            generation,
            epoch()
        )?),
        "bot_started" => {
            let run = check_bot_run()?;
            crate::bot::started(
                &tx,
                run,
                field(b, "mode")?,
                b["session_id"].as_str(),
                field(b, "log")?,
                &b["event"],
            )?;
            Value::Null
        }
        "bot_activity" => {
            let run = check_bot_run()?;
            crate::bot::set_activity(&tx, run, field(b, "activity")?)?;
            Value::Null
        }
        "bot_finish" => {
            let run = check_bot_run()?;
            let finished = crate::bot::finish(
                &tx,
                run,
                b["code"].as_i64(),
                field(b, "reason")?,
                b["session_id"].as_str(),
                b["note"].as_str(),
                epoch(),
            )?;
            crate::usage::record_best_effort(&tx, "bot", run, &b["usage"]);
            let mut finished = finished;
            // A shrink is an optimization; it must never fail the finish.
            tx.execute_batch("SAVEPOINT bot_shrink")?;
            match crate::bot::shrink_after(&tx, run, b["usage"]["context_tokens"].as_i64()) {
                Ok(shrink) => {
                    tx.execute_batch("RELEASE bot_shrink")?;
                    if let Some(shrink) = shrink {
                        finished["shrink"] = shrink;
                    }
                }
                Err(e) => {
                    tx.execute_batch("ROLLBACK TO bot_shrink; RELEASE bot_shrink")?;
                    eprintln!("axe: bot run {run} shrink check failed: {e:#}");
                }
            }
            finished
        }
        "pending" => json!(db::pending_projects(&tx)?),
        "events" => json!(db::events(&tx, task()?)?),
        "task" => json!(db::get_task(&tx, task()?)?),
        "session" => json!(db::session(&tx, task()?)?),
        "claim" => {
            let result = db::claim_in(
                &tx,
                &actor,
                b["project"].as_str(),
                claim()?,
                b["max_runs"].as_i64(),
            )?;
            if matches!(result, db::Claim::Claimed(_)) {
                tx.execute(
                    "INSERT INTO dispatch_claims VALUES(?1,?2,?3)",
                    params![claim()?, id, generation],
                )?;
            }
            json!(result)
        }
        "hold" => json!(db::hold_in(
            &tx,
            &actor,
            field(b, "project")?,
            field(b, "note")?,
        )?),
        "round_robin" => {
            let len = number(b, "len")?;
            if len <= 0 {
                bail!("empty agent list");
            }
            let rule = field(b, "rule")?;
            let next: i64 = tx
                .query_row("SELECT next FROM rr WHERE rule=?1", [rule], |r| r.get(0))
                .optional()?
                .unwrap_or(0);
            tx.execute(
                "INSERT INTO rr VALUES(?1,?2) ON CONFLICT(rule) DO UPDATE SET next=?2",
                params![rule, next + 1],
            )?;
            json!(next.rem_euclid(len))
        }
        "note" => {
            check_claim()?;
            db::write_event(
                &tx,
                &actor,
                task()?,
                "note",
                None,
                None,
                Some(field(b, "note")?),
            )?;
            Value::Null
        }
        "block" => {
            check_claim()?;
            let changes = Changes {
                state: Some("escalated".into()),
                note: Some(field(b, "note")?.into()),
                ..Default::default()
            };
            json!(db::apply_changes(
                &tx,
                &actor,
                task()?,
                &changes,
                Some(claim()?)
            )?)
        }
        "prepare" => {
            check_claim()?;
            let t = db::get_task(&tx, task()?)?;
            if t.state != "in_progress" || t.claimed_by.as_deref() != Some(claim()?) {
                bail!("task no longer claimed");
            }
            let run = db::insert_run(
                &tx,
                &db::NewRun {
                    task_id: t.id,
                    project: &t.project,
                    agent: field(b, "agent")?,
                    mode: field(b, "mode")?,
                    session_id: b["session_id"].as_str(),
                    claim: claim()?,
                    log_path: field(b, "log")?,
                },
            )?;
            tx.execute(
                "INSERT INTO dispatch_runs VALUES(?1,?2,?3,'prepared')",
                params![run, id, generation],
            )?;
            // Recorded before spawn so the worker's own updates always follow it.
            db::write_event(
                &tx,
                &actor,
                t.id,
                "run_start",
                None,
                Some(claim()?),
                Some(&b["event"].to_string()),
            )?;
            // A run without a known session id replaces the stored one at finish.
            if b["session"].is_null() {
                db::delete_session(&tx, t.id)?;
            } else {
                let s: db::Session = serde_json::from_value(b["session"].clone())?;
                db::save_session(&tx, t.id, &s)?;
            }
            json!(run)
        }
        "started" => {
            let run = check_run()?;
            tx.execute(
                "UPDATE dispatch_runs SET phase='running' WHERE run_id=?1 AND phase='prepared'",
                [run],
            )?;
            Value::Null
        }
        "activity" => {
            let run = check_run()?;
            db::set_run_activity(&tx, run, field(b, "activity")?)?;
            Value::Null
        }
        // Decides and records in one step, so a later `protect` cannot race the kill.
        "drain_abort" => {
            let run = check_run()?;
            let r = db::get_run(&tx, run)?;
            let phase: String = tx.query_row(
                "SELECT phase FROM dispatch_runs WHERE run_id=?1",
                [run],
                |r| r.get(0),
            )?;
            let abort = r.ended_at.is_none()
                && matches!(phase.as_str(), "prepared" | "running" | "drain_abort")
                && !run_protected(&tx, r.task_id, run)?;
            if abort {
                tx.execute(
                    "UPDATE dispatch_runs SET phase='drain_abort' WHERE run_id=?1",
                    [run],
                )?;
            }
            json!(abort)
        }
        "finish" => {
            let run = check_run()?;
            let r = db::get_run(&tx, run)?;
            if r.ended_at.is_some() {
                bail!("run already ended");
            }
            let aborted: bool = tx.query_row(
                "SELECT phase='abort_requested' FROM dispatch_runs WHERE run_id=?1",
                [run],
                |r| r.get(0),
            )?;
            // Task changes since this run started; a claim's latest run_start is this run's.
            let events = db::events(&tx, r.task_id)?;
            let start = events
                .iter()
                .rfind(|e| e.kind == "run_start" && e.to_value.as_deref() == Some(&r.claim))
                .map_or(0, |e| e.id);
            let changed = events
                .iter()
                .any(|e| e.id > start && e.actor != "dispatcher");
            // A model-API outage the dispatcher saw end a plain failed exit: keep the
            // session, refund the run and retry after a backoff instead of escalating.
            let outage = !aborted && b["drained"] != true && b["api_error"].is_object();
            let retry = !aborted
                && !outage
                && b["resumed"] == true
                && b["code"].as_i64() != Some(0)
                && !changed
                && b["note"] != "dispatcher stopped"
                && b["drained"] != true;
            if !b["session"].is_null() {
                let s: db::Session = serde_json::from_value(b["session"].clone())?;
                db::save_session(&tx, r.task_id, &s)?;
            }
            db::end_run(
                &tx,
                run,
                b["code"].as_i64().map(|x| x as i32),
                b["session_id"].as_str(),
            )?;
            crate::usage::record_best_effort(&tx, "task", run, &b["usage"]);
            tx.execute(
                "UPDATE dispatch_runs SET phase=?2 WHERE run_id=?1",
                params![
                    run,
                    if aborted && b["aborted"] == true {
                        "aborted"
                    } else {
                        "finished"
                    }
                ],
            )?;
            let mut end = b["event"].clone();
            if outage {
                end["api_error"] = b["api_error"].clone();
            }
            db::write_event(
                &tx,
                &actor,
                r.task_id,
                "run_end",
                None,
                Some(&r.claim),
                Some(&end.to_string()),
            )?;
            if aborted {
                db::write_event(
                    &tx,
                    &actor,
                    r.task_id,
                    if b["aborted"] == true {
                        "run_aborted"
                    } else {
                        "abort_raced_completion"
                    },
                    None,
                    Some(&run.to_string()),
                    Some(if b["aborted"] == true {
                        "dispatcher confirmed process termination"
                    } else {
                        "run completed before abort was handled; termination not confirmed"
                    }),
                )?;
                db::apply_changes(
                    &tx,
                    &actor,
                    r.task_id,
                    &Changes {
                        state: Some("escalated".into()),
                        note: Some(format!(
                            "run {run}: {}; explicitly resume with axe set {} --state pending",
                            if b["aborted"] == true {
                                "abort confirmed"
                            } else {
                                "completion raced abort; termination not confirmed"
                            },
                            r.task_id
                        )),
                        ..Default::default()
                    },
                    None,
                )?;
            }
            if b["drained"] == true && !aborted {
                db::write_event(
                    &tx,
                    &actor,
                    r.task_id,
                    "run_interrupted",
                    None,
                    Some(&run.to_string()),
                    Some("aborted by a dispatcher rollout drain; re-check real state before repeating any step"),
                )?;
                tx.execute(
                    "UPDATE tasks SET runs = MAX(runs - 1, 0) WHERE id = ?1",
                    [r.task_id],
                )?;
                let changes = Changes {
                    state: Some("pending".into()),
                    note: Some(format!(
                        "run {run} aborted by a rollout drain; resumes in the same session"
                    )),
                    ..Default::default()
                };
                db::apply_changes(&tx, &actor, r.task_id, &changes, Some(&r.claim))?;
            } else if outage {
                let streak = 1 + events
                    .iter()
                    .rev()
                    .filter(|e| e.kind == "run_end")
                    .take_while(|e| {
                        e.body
                            .as_deref()
                            .and_then(|t| serde_json::from_str::<Value>(t).ok())
                            .is_some_and(|v| v["api_error"].is_object())
                    })
                    .count() as i64;
                let wait = crate::agent_proc::api_backoff_seconds(streak);
                let changes = Changes {
                    state: Some("pending".into()),
                    not_before: Some(format!("+{wait}s")),
                    note: Some(format!(
                        "run {run} ended on a model API error ({} {}); not counted against the \
                         run budget; retrying in the same session in {wait}s \
                         (outage run {streak} in a row)",
                        b["api_error"]["status"],
                        b["api_error"]["message"].as_str().unwrap_or("")
                    )),
                    ..Default::default()
                };
                if db::apply_changes(&tx, &actor, r.task_id, &changes, Some(&r.claim))? {
                    tx.execute(
                        "UPDATE tasks SET runs = MAX(runs - 1, 0) WHERE id = ?1",
                        [r.task_id],
                    )?;
                }
            } else if !retry && !aborted {
                let changes = Changes {
                    state: Some("escalated".into()),
                    note: Some(db::block_note(
                        field(b, "note")?,
                        "ops: inspect the run log and fix the cause",
                        &format!("axe set {} --state pending", r.task_id),
                    )),
                    ..Default::default()
                };
                db::apply_changes(&tx, &actor, r.task_id, &changes, Some(&r.claim))?;
            }
            json!(retry)
        }
        _ => bail!("unknown dispatcher operation {op}"),
    };
    // Read operations are not retained; writes are replayable after a lost response.
    if !read_only(op) {
        tx.execute(
            "INSERT INTO dispatch_ops VALUES(?1,?2,?3,?4)",
            params![op_id, id, request, result.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(result)
}

/// Operations that change nothing; they are neither journaled nor retained.
fn read_only(op: &str) -> bool {
    matches!(
        op,
        "pending" | "events" | "task" | "session" | "bot_ready" | "bot_control"
    )
}

#[derive(Default, Serialize, Deserialize)]
pub struct Journal {
    pub id: String,
    pub generation: String,
    pub pending: Option<Value>,
    pub logs: BTreeMap<i64, LocalLog>,
    /// Bot run logs, keyed by bot run id (a separate id space from task runs).
    #[serde(default)]
    pub bot_logs: BTreeMap<i64, LocalLog>,
}
#[derive(Serialize, Deserialize)]
pub struct LocalLog {
    pub path: PathBuf,
    pub finished: bool,
}
pub struct Store {
    pub aborts: Arc<Mutex<Vec<i64>>>,
    pub client: Client,
    pub journal: Arc<Mutex<Journal>>,
    path: PathBuf,
    _lock: std::fs::File,
    pub stop: Arc<AtomicBool>,
    pub draining: Arc<AtomicBool>,
}
pub fn save(path: &Path, journal: &Journal) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(journal)?)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    std::fs::File::open(path.parent().context("journal parent")?)?.sync_all()?;
    Ok(())
}
impl Store {
    pub fn open(
        cfg: &crate::config::Config,
        stop: Arc<AtomicBool>,
        reconcile: Option<&str>,
    ) -> Result<Self> {
        Self::open_with(cfg, Client::from_env("dispatcher".into()), stop, reconcile)
    }

    /// `open` against an explicit server client (tests pass an isolated server).
    pub fn open_with(
        cfg: &crate::config::Config,
        client: Client,
        stop: Arc<AtomicBool>,
        reconcile: Option<&str>,
    ) -> Result<Self> {
        use fs2::FileExt;
        let dir = cfg.log_dir().join(".dispatcher");
        std::fs::create_dir_all(&dir)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("lock"))?;
        lock.try_lock_exclusive()
            .context("another dispatcher uses this journal")?;
        let path = dir.join("journal.json");
        let mut journal: Journal = if path.exists() {
            serde_json::from_slice(&std::fs::read(&path)?)?
        } else {
            Journal {
                id: uuid::Uuid::new_v4().to_string(),
                generation: String::new(),
                ..Default::default()
            }
        };
        if let Some(note) = reconcile {
            if note.trim().is_empty() {
                bail!("reconciliation requires an audit note");
            }
            let _: Value = Client::from_env("ops".into()).send(
                "POST",
                "/api/dispatcher/reconcile",
                json!({"id":journal.id,"generation":journal.generation,"note":note}),
            )?;
            journal.pending = None;
            for log in journal
                .logs
                .values_mut()
                .chain(journal.bot_logs.values_mut())
            {
                log.finished = true;
            }
            journal.generation.clear();
        }
        if journal.pending.is_some()
            || journal
                .logs
                .values()
                .chain(journal.bot_logs.values())
                .any(|l| !l.finished)
        {
            bail!(
                "unresolved dispatcher journal {}; reconcile interrupted runs before restarting",
                path.display()
            );
        }
        let check: Value =
            client.send("POST", "/api/dispatcher/recovery", json!({"id":journal.id}))?;
        if check["unresolved"] == true {
            bail!(
                "server has unresolved runs or claims; stop old workers and use --reconcile-note"
            );
        }
        save(&path, &journal)?;
        Ok(Self {
            aborts: Arc::default(),
            client,
            journal: Arc::new(Mutex::new(journal)),
            path,
            _lock: lock,
            stop,
            draining: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn request<T: serde::de::DeserializeOwned>(&self, op: &str, body: Value) -> Result<T> {
        let durable = !read_only(op);
        let request = {
            let mut j = self.journal.lock().unwrap();
            let v = json!({"dispatcher":j.id,"generation":j.generation,"operation_id":uuid::Uuid::new_v4().to_string(),"op":op,"body":body});
            if durable {
                j.pending = Some(v.clone());
                save(&self.path, &j)?;
            }
            v
        };
        let mut delay = 1;
        loop {
            match self
                .client
                .send::<Value>("POST", "/api/dispatcher/operation", request.clone())
            {
                Ok(v) => {
                    let mut j = self.journal.lock().unwrap();
                    if durable {
                        j.pending = None;
                        save(&self.path, &j)?;
                    }
                    return Ok(serde_json::from_value(v)?);
                }
                Err(e) => {
                    if self.stop.load(Ordering::SeqCst) {
                        return Err(e);
                    }
                    if !crate::client::retryable(&e) {
                        return Err(e);
                    }
                    eprintln!("axe dispatcher: server unavailable; retaining operation: {e}");
                    for _ in 0..delay * 10 {
                        if self.stop.load(Ordering::SeqCst) {
                            return Err(e);
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    delay = (delay * 2).min(10);
                }
            }
        }
    }
    pub fn log(&self, run: i64, path: PathBuf, finished: bool) -> Result<()> {
        let mut j = self.journal.lock().unwrap();
        j.logs.insert(run, LocalLog { path, finished });
        save(&self.path, &j)
    }
    pub fn log_bot(&self, run: i64, path: PathBuf, finished: bool) -> Result<()> {
        let mut j = self.journal.lock().unwrap();
        j.bot_logs.insert(run, LocalLog { path, finished });
        save(&self.path, &j)
    }
}

pub struct LogJob {
    pub query: Value,
    pub reply: std::sync::mpsc::SyncSender<Value>,
}
/// Connected dispatcher: (dispatcher id, connection id, log request sender).
pub type Relay = Mutex<Option<(String, String, std::sync::mpsc::SyncSender<LogJob>)>>;

pub fn socket_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(2 * 1024 * 1024))
        .max_frame_size(Some(2 * 1024 * 1024))
}
impl Store {
    /// Dispatcher dials out; no inbound port or shared filesystem is needed.
    pub fn start_control(&self, cfg: &crate::config::Config) -> Result<()> {
        use tungstenite::client::IntoClientRequest;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let aborts = Arc::clone(&self.aborts);
        let journal = Arc::clone(&self.journal);
        let stop = Arc::clone(&self.stop);
        let path = self.path.clone();
        let log_root = cfg.log_dir();
        let base = self.client.base().to_owned();
        let token = self.client.token()?;
        let control = self.client.clone();
        std::thread::spawn(move || {
            let mut first = true;
            while !stop.load(Ordering::SeqCst) {
                let result = (|| -> Result<()> {
                    let client = control.clone();
                    let (id, generation) = {
                        let j = journal.lock().unwrap();
                        (j.id.clone(), j.generation.clone())
                    };
                    let registered: Value = client.send(
                        "POST",
                        "/api/dispatcher/register",
                        json!({"id":id,"generation":generation}),
                    )?;
                    let generation = field(&registered, "generation")?.to_owned();
                    {
                        let mut j = journal.lock().unwrap();
                        j.generation = generation.clone();
                        save(&path, &j)?;
                    }
                    let catalog: Vec<db::RunRecord> = client.send(
                        "POST",
                        "/api/dispatcher/log-catalog",
                        json!({"id":id,"generation":generation}),
                    )?;
                    let root = log_root.canonicalize()?;
                    let mut adopted = Vec::new();
                    {
                        let mut j = journal.lock().unwrap();
                        for r in catalog {
                            if let Ok(p) = Path::new(&r.log_path).canonicalize() {
                                if p.starts_with(&root) && p.is_file() {
                                    j.logs.entry(r.id).or_insert(LocalLog {
                                        path: p,
                                        finished: r.ended_at.is_some(),
                                    });
                                    adopted.push(r.id);
                                }
                            }
                        }
                        save(&path, &j)?;
                    }
                    let _: Value = client.send(
                        "POST",
                        "/api/dispatcher/adopt-logs",
                        json!({"id":id,"generation":generation,"runs":adopted}),
                    )?;
                    let url = format!(
                        "{}/api/dispatcher/connect?id={id}&generation={generation}",
                        base.replacen("http", "ws", 1)
                    );
                    let mut request = url.into_client_request()?;
                    if let Some(t) = &token {
                        request
                            .headers_mut()
                            .insert("Authorization", format!("Bearer {t}").parse()?);
                    }
                    let (mut ws, _) = tungstenite::client::connect_with_config(
                        request,
                        Some(socket_config()),
                        3,
                    )?;
                    match ws.get_mut() {
                        tungstenite::stream::MaybeTlsStream::Plain(s) => {
                            s.set_read_timeout(Some(Duration::from_secs(20)))?;
                            s.set_write_timeout(Some(Duration::from_secs(10)))?;
                        }
                        tungstenite::stream::MaybeTlsStream::Rustls(s) => {
                            s.sock.set_read_timeout(Some(Duration::from_secs(20)))?;
                            s.sock.set_write_timeout(Some(Duration::from_secs(10)))?;
                        }
                        _ => bail!("unsupported dispatcher transport"),
                    }
                    if first {
                        let _ = ready_tx.send(Ok(()));
                        first = false;
                    }
                    while !stop.load(Ordering::SeqCst) {
                        let message = ws.read()?;
                        if !message.is_text() {
                            continue;
                        }
                        let q: Value = serde_json::from_str(message.to_text()?)?;
                        if q["dispatcher_generation"] == generation {
                            if let Some(ids) = q["aborts"].as_array() {
                                *aborts.lock().unwrap() =
                                    ids.iter().filter_map(Value::as_i64).collect();
                            }
                        }
                        // `log` reads a task run, `bot_log` a bot run: separate id spaces.
                        let response = if q["kind"] == "log" || q["kind"] == "bot_log" {
                            let local = {
                                let j = journal.lock().unwrap();
                                let logs = if q["kind"] == "bot_log" {
                                    &j.bot_logs
                                } else {
                                    &j.logs
                                };
                                q["run"]
                                    .as_i64()
                                    .and_then(|id| logs.get(&id))
                                    .map(|l| (l.path.clone(), l.finished))
                            };
                            match local {
                                Some((path, finished)) => match crate::run_log::read(
                                    &path,
                                    q["after"].as_u64(),
                                    q["before"].as_u64(),
                                    q["generation"].as_str(),
                                    finished,
                                ) {
                                    Ok(page) => json!({"page":page,"log_complete":finished}),
                                    Err(e) => json!({"unavailable":e.to_string()}),
                                },
                                None => {
                                    json!({"unavailable":"Log not registered on this dispatcher"})
                                }
                            }
                        } else {
                            json!({"ok":true})
                        };
                        ws.send(tungstenite::Message::text(response.to_string()))?;
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    if first {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                    eprintln!("axe dispatcher control disconnected: {e}");
                }
                for _ in 0..10 {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(30))?
            .map_err(anyhow::Error::msg)
    }
}

pub fn reconcile(conn: &Connection, v: &Value) -> Result<Value> {
    let id = field(v, "id")?;
    let generation = field(v, "generation")?;
    let note = field(v, "note")?;
    if note.trim().is_empty() {
        bail!("audit note required");
    }
    let tx = conn.unchecked_transaction()?;
    let owner: Option<(String, String)> = tx
        .query_row(
            "SELECT id,generation FROM dispatcher WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if owner
        .as_ref()
        .is_some_and(|(i, g)| i != id || g != generation)
    {
        bail!("cannot reconcile another dispatcher");
    }
    let actor = Actor::new("ops");
    let claims = db::in_progress_claims(&tx, "dispatch:")?;
    for (task, claim) in claims {
        db::apply_changes(
            &tx,
            &actor,
            task,
            &Changes {
                state: Some("escalated".into()),
                escalation_class: Some("runtime".into()),
                note: Some(format!("dispatcher reconciliation: {note}")),
                ..Default::default()
            },
            Some(&claim),
        )?;
    }
    db::end_open_runs(&tx)?;
    crate::bot::reconcile(&tx, id, note)?;
    tx.execute(
        "UPDATE dispatch_runs SET phase='finished' WHERE dispatcher=?1 AND phase!='aborted'",
        [id],
    )?;
    tx.execute("DELETE FROM dispatcher WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(json!({"ok":true}))
}
