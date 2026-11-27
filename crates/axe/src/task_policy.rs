//! What happens to a task when its run ends: the post-run policy (outage
//! retry, drain refund, abort and failure escalation), owned by the task
//! primitive. The dispatcher protocol (`remote::operation` `finish`) calls
//! [`finish`] under the server's lock.
use crate::db::{self, Actor, Changes};
use anyhow::{bail, Result};
use rusqlite::{params, Transaction};
use serde_json::{json, Value};

fn field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    crate::remote::field(v, k)
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Ends run `run` (already checked to belong to the calling node) with the
/// node's report `b` and applies the post-run task policy; returns whether
/// the node should retry the resume fresh.
pub fn finish(tx: &Transaction, actor: &Actor, run: i64, b: &Value) -> Result<Value> {
    let r = db::get_run(tx, run)?;
    if r.ended_at.is_some() {
        bail!("run already ended");
    }
    let aborted: bool = tx.query_row(
        "SELECT phase='abort_requested' FROM dispatch_runs WHERE run_id=?1",
        [run],
        |r| r.get(0),
    )?;
    // Task changes since this run started; a claim's latest run_start is this run's.
    let events = db::events(tx, r.task_id)?;
    let start = events
        .iter()
        .rfind(|e| e.kind == "run_start" && e.to_value.as_deref() == Some(&r.claim))
        .map_or(0, |e| e.id);
    let changed = events
        .iter()
        .any(|e| e.id > start && e.actor != "dispatcher");
    // A quota, login or provider failure the node saw end a plain failed
    // exit (`failure.rs`): keep the session, refund the run and retry later
    // instead of escalating.
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
        db::save_session(tx, r.task_id, &s)?;
    }
    db::end_run(
        tx,
        run,
        b["code"].as_i64().map(|x| x as i32),
        b["session_id"].as_str(),
    )?;
    crate::usage::record_best_effort(tx, "task", run, &b["usage"]);
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
        tx,
        actor,
        r.task_id,
        "run_end",
        None,
        Some(&r.claim),
        Some(&end.to_string()),
    )?;
    if aborted {
        db::write_event(
            tx,
            actor,
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
            tx,
            actor,
            r.task_id,
            &Changes {
                state: Some("escalated".into()),
                note: Some(format!(
                    "run {run}: {}; explicitly resume with axe task set {} --state pending",
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
            tx,
            actor,
            r.task_id,
            "run_interrupted",
            None,
            Some(&run.to_string()),
            Some("aborted by a node rollout drain; re-check real state before repeating any step"),
        )?;
        db::refund_run(tx, r.task_id)?;
        let changes = Changes {
            state: Some("pending".into()),
            note: Some(format!(
                "run {run} aborted by a rollout drain; resumes in the same session"
            )),
            ..Default::default()
        };
        db::apply_changes(tx, actor, r.task_id, &changes, Some(&r.claim))?;
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
        let wait = retry_wait(&b["api_error"], streak, now_unix());
        let changes = Changes {
            state: Some("pending".into()),
            not_before: Some(format!("+{wait}s")),
            note: Some(format!(
                "run {run} ended on a model API error ({}); not counted against the \
                 run budget; retrying in the same session in {wait}s \
                 (API failure run {streak} in a row)",
                crate::failure::note(&b["api_error"])
            )),
            ..Default::default()
        };
        if db::apply_changes(tx, actor, r.task_id, &changes, Some(&r.claim))? {
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
                &format!("axe task set {} --state pending", r.task_id),
            )),
            ..Default::default()
        };
        db::apply_changes(tx, actor, r.task_id, &changes, Some(&r.claim))?;
    }
    Ok(json!(retry))
}

/// Seconds before a task retries after the run ended on `api_error` (a
/// `failure::Failure` object): an exhausted quota waits for its reset (an hour
/// when unknown), anything else backs off by `streak`.
pub fn retry_wait(api_error: &Value, streak: i64, now: i64) -> i64 {
    if api_error["class"] == crate::failure::QUOTA_EXHAUSTED {
        return api_error["retry_at"]
            .as_i64()
            .map_or(QUOTA_RETRY_SECONDS, |at| at - now)
            .max(crate::agent_proc::api_backoff_seconds(1));
    }
    crate::agent_proc::api_backoff_seconds(streak)
}

/// Retry delay after an exhausted quota with no reported reset time.
pub const QUOTA_RETRY_SECONDS: i64 = 3600;
