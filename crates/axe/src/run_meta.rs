//! What the server knows about one run, for the run page's metadata panel:
//! its start and end events (command, rule, runtime, run folder, exit), the
//! task's or bot's labels and repos, and recorded usage. The prompts and the
//! harness's init record live beside the log on the dispatcher
//! (`run_log::run_files`); the server relays those separately.
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::{bot, db};

/// The listed keys of `v`, when present.
fn pick(v: Value, keys: &[&str]) -> Value {
    let Value::Object(m) = v else {
        return Value::Null;
    };
    Value::Object(
        m.into_iter()
            .filter(|(k, _)| keys.contains(&k.as_str()))
            .collect::<Map<_, _>>(),
    )
}

fn body(s: Option<&str>) -> Value {
    s.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null)
}

/// Recorded token usage and cost of a run, per model; `Null` when none.
pub fn usage(conn: &Connection, kind: &str, run: i64) -> Result<Value> {
    let head: Option<(String, Option<String>, Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT status, reason, turns, context_tokens FROM run_usage
             WHERE kind=?1 AND run_id=?2",
            params![kind, run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((status, reason, turns, context_tokens)) = head else {
        return Ok(Value::Null);
    };
    let models = conn
        .prepare(
            "SELECT model, COALESCE(input, total_input), COALESCE(output, total_output),
               COALESCE(cache_read, total_cache_read), COALESCE(cache_write, total_cache_write),
               COALESCE(cost, total_cost)
             FROM run_usage_models WHERE kind=?1 AND run_id=?2 ORDER BY model",
        )?
        .query_map(params![kind, run], |r| {
            Ok(json!({"model":r.get::<_, String>(0)?,"input":r.get::<_, i64>(1)?,
                "output":r.get::<_, i64>(2)?,"cache_read":r.get::<_, i64>(3)?,
                "cache_write":r.get::<_, i64>(4)?,"cost":r.get::<_, f64>(5)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"status":status,"reason":reason,"turns":turns,
        "context_tokens":context_tokens,"models":models}))
}

/// Task run `id`: `{run, task, start, end, outcome, usage}`. `start` and `end`
/// are the bodies of its `run_start` and `run_end` events; `outcome` is the
/// dispatcher's state change right after the run (an escalation or retry
/// note), if any; `task_state` the task's state when the run ended.
pub fn task_run(conn: &Connection, id: i64) -> Result<Value> {
    let run = db::get_run(conn, id)?;
    let task = db::get_task(conn, run.task_id)?;
    let events = db::events(conn, run.task_id)?;
    let claim = Some(run.claim.as_str());
    let start = events
        .iter()
        .rposition(|e| e.kind == "run_start" && e.to_value.as_deref() == claim);
    let end = start.and_then(|s| {
        events[s..]
            .iter()
            .position(|e| e.kind == "run_end" && e.to_value.as_deref() == claim)
            .map(|n| s + n)
    });
    let outcome = end.and_then(|n| {
        events[n + 1..]
            .iter()
            .take_while(|e| e.actor == "dispatcher" && e.kind != "claimed")
            .find(|e| e.kind == "state")
            .map(|e| json!({"from":e.from_value,"to":e.to_value,"note":e.body,"at":e.at}))
    });
    // The task's state as this run left it: the last state change up to its end.
    let task_state = start.map(|s| {
        events[..=end.unwrap_or(events.len() - 1).max(s)]
            .iter()
            .rev()
            .find(|e| e.kind == "state")
            .and_then(|e| e.to_value.clone())
    });
    Ok(json!({
        "run": run,
        "task": pick(serde_json::to_value(&task)?, &["id","title","project","state","status","owner","labels","repos"]),
        "start": start.map_or(Value::Null, |n| body(events[n].body.as_deref())),
        "end": end.map_or(Value::Null, |n| body(events[n].body.as_deref())),
        "outcome": outcome,
        "task_state": task_state.flatten(),
        "usage": usage(conn, "task", id)?,
    }))
}

/// Bot run `id`: `{run, bot, start, end, wakes, usage}`, with `start` and
/// `end` the bodies of the bot's `run_start` and `run_end` events for it and
/// `wakes` the count of each kind of wake it was handed.
pub fn bot_run(conn: &Connection, id: i64) -> Result<Value> {
    let run = bot::get_run(conn, id)?;
    let b = bot::get(conn, &run.bot)?;
    let event = |kind: &str| -> Result<Value> {
        let s: Option<String> = conn
            .query_row(
                "SELECT body FROM bot_events WHERE bot_id=?1 AND kind=?2
                   AND json_valid(body) AND json_extract(body, '$.run')=?3
                 ORDER BY id DESC LIMIT 1",
                params![b.id, kind, id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body(s.as_deref()))
    };
    let wakes = conn
        .prepare("SELECT kind, COUNT(*) FROM bot_wakes WHERE run_id=?1 GROUP BY kind ORDER BY kind")?
        .query_map([id], |r| Ok((r.get::<_, String>(0)?, json!(r.get::<_, i64>(1)?))))?
        .collect::<rusqlite::Result<Map<String, Value>>>()?;
    Ok(json!({
        "run": run,
        "bot": pick(serde_json::to_value(&b)?, &["name","project","state","agent","repos"]),
        "start": event("run_start")?,
        "end": event("run_end")?,
        "wakes": wakes,
        "usage": usage(conn, "bot", id)?,
    }))
}
