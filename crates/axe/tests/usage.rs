mod common;
use axe::usage::{self, CostQuery, IngestItem, Parsed, Totals};
use axe::{db, remote};
use common::Server;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::Path;

fn open() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let c = db::open(&dir.path().join("db")).unwrap();
    (dir, c)
}

fn task(c: &Connection, id: i64, state: &str) {
    c.execute(
        "INSERT INTO tasks (id, project, title, state, created_at, updated_at)
         VALUES (?1, 'p', 't', ?2, '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
        params![id, state],
    )
    .unwrap();
}

fn run(c: &Connection, id: i64, task: i64, ended: Option<&str>, log: &str) {
    c.execute(
        "INSERT INTO runs (id, task_id, project, agent, mode, session_id, claim, started_at,
           ended_at, log_path)
         VALUES (?1, ?2, 'p', 'claude', ?3, 's', 'c', ?4, ?5, ?6)",
        params![
            id,
            task,
            if id == 1 { "fresh" } else { "resume" },
            format!("2026-11-01T00:00:{id:02}Z"),
            ended,
            log
        ],
    )
    .unwrap();
}

const ENDED: Option<&str> = Some("2026-11-02T00:00:00Z");

fn bot(c: &Connection, run_id: i64) {
    c.execute(
        "INSERT OR IGNORE INTO bots (id, name, project, agent, identity, conduct, notes,
           heartbeat_seconds, nudge_after_seconds, state, created_at, updated_at)
         VALUES (1, 'axe', 'p', 'claude', '', '', '', 14400, 600, 'active',
           '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO bot_runs (id, bot_id, agent, mode, session_id, claim, dispatcher, generation,
           started_at, ended_at, log_path)
         VALUES (?1, 1, 'claude', 'resume', 'b', 'c', 'd', 'g', '2026-11-01T00:00:00Z', ?2, 'x')",
        params![run_id, ENDED],
    )
    .unwrap();
}

fn totals(input: i64, cost: f64) -> Totals {
    Totals {
        input,
        output: input * 2,
        thinking: input / 2,
        cache_read: input * 10,
        cache_write: input * 3,
        cost,
    }
}

fn known(session: &str, models: &[(&str, Totals)]) -> Parsed {
    Parsed {
        unknown: None,
        session_id: Some(session.into()),
        models: models.iter().map(|(m, t)| ((*m).into(), *t)).collect(),
        turns: Some(3),
        context_tokens: Some(60_000),
        cache_start: Some("warm".into()),
    }
}

fn record(c: &Connection, id: i64, p: &Parsed) -> bool {
    usage::record(c, "task", id, p, "test").unwrap()
}

/// (status, input delta, cost delta) of one run and model.
fn row(c: &Connection, id: i64, model: &str) -> (String, Option<i64>, Option<f64>) {
    c.query_row(
        "SELECT u.status, m.input, m.cost FROM run_usage u JOIN run_usage_models m
           ON m.kind=u.kind AND m.run_id=u.run_id
         WHERE u.run_id=?1 AND m.model=?2",
        params![id, model],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn seed_runs(c: &Connection, n: i64) {
    task(c, 1, "done");
    for id in 1..=n {
        run(c, id, 1, ENDED, "x");
    }
}

fn stats(c: &Connection) -> Value {
    usage::cost_stats(c, 0, i64::MAX / 2, None, &CostQuery::default()).unwrap()
}

#[test]
fn fresh_and_resumed_runs_store_deltas_that_sum_to_the_latest_total() {
    let (_d, c) = open();
    seed_runs(&c, 3);
    record(&c, 1, &known("s", &[("m", totals(10, 1.0))]));
    record(&c, 2, &known("s", &[("m", totals(25, 2.5))]));
    record(&c, 3, &known("s", &[("m", totals(40, 4.25))]));
    assert_eq!(row(&c, 1, "m"), ("known".into(), Some(10), Some(1.0)));
    assert_eq!(row(&c, 2, "m"), ("known".into(), Some(15), Some(1.5)));
    assert_eq!(row(&c, 3, "m"), ("known".into(), Some(15), Some(1.75)));
    let sum: f64 = c
        .query_row("SELECT SUM(cost) FROM run_usage_models", [], |r| r.get(0))
        .unwrap();
    assert!((sum - 4.25).abs() < 1e-9, "deltas sum to latest total");
}

#[test]
fn counter_reset_is_unknown_never_negative_and_rebases_the_session() {
    let (_d, c) = open();
    seed_runs(&c, 3);
    record(&c, 1, &known("s", &[("m", totals(100, 80.0))]));
    record(&c, 2, &known("s", &[("m", totals(90, 68.0))]));
    record(&c, 3, &known("s", &[("m", totals(95, 70.0))]));
    assert_eq!(row(&c, 2, "m"), ("reset".into(), None, None));
    assert_eq!(row(&c, 3, "m"), ("known".into(), Some(5), Some(2.0)));
    let s = stats(&c);
    assert_eq!(s["coverage"]["unknown_reasons"]["counter_reset"], 1);
    assert_eq!(s["total"]["cost"].as_f64(), Some(82.0));
    // The segment after the reset still sums to its latest total above the rebase.
    let after: f64 = c
        .query_row(
            "SELECT SUM(m.cost) FROM run_usage_models m WHERE m.run_id>2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!((after - (70.0 - 68.0)).abs() < 1e-9);
}

#[test]
fn a_reset_to_a_new_session_id_is_simply_a_fresh_session() {
    let (_d, c) = open();
    seed_runs(&c, 2);
    record(&c, 1, &known("old", &[("m", totals(100, 50.0))]));
    record(&c, 2, &known("new", &[("m", totals(5, 0.5))]));
    assert_eq!(row(&c, 2, "m"), ("known".into(), Some(5), Some(0.5)));
}

#[test]
fn multi_model_runs_keep_a_delta_per_model() {
    let (_d, c) = open();
    seed_runs(&c, 2);
    record(&c, 1, &known("s", &[("opus", totals(10, 1.0))]));
    record(
        &c,
        2,
        &known("s", &[("opus", totals(30, 3.0)), ("haiku", totals(4, 0.1))]),
    );
    assert_eq!(row(&c, 2, "opus").1, Some(20));
    assert_eq!(row(&c, 2, "haiku"), ("known".into(), Some(4), Some(0.1)));
    let s = stats(&c);
    let models = s["breakdowns"]["model"].as_array().unwrap();
    let opus = models.iter().find(|m| m["key"] == "opus").unwrap();
    assert_eq!(opus["cost"].as_f64(), Some(3.0));
    assert_eq!(
        opus["runs"], 2,
        "a run counts once, for its costliest model"
    );
}

#[test]
fn a_model_dropping_out_of_the_totals_is_a_reset() {
    let (_d, c) = open();
    seed_runs(&c, 2);
    record(
        &c,
        1,
        &known("s", &[("a", totals(10, 1.0)), ("b", totals(5, 1.0))]),
    );
    record(&c, 2, &known("s", &[("a", totals(20, 2.0))]));
    assert_eq!(row(&c, 2, "a").0, "reset");
}

#[test]
fn an_unknown_run_between_known_runs_is_never_zero_and_the_gap_goes_to_the_next() {
    let (_d, c) = open();
    seed_runs(&c, 3);
    record(&c, 1, &known("s", &[("m", totals(10, 1.0))]));
    record(&c, 2, &Parsed::unknown(usage::NO_RESULT));
    record(&c, 3, &known("s", &[("m", totals(30, 3.0))]));
    assert_eq!(row(&c, 3, "m").2, Some(2.0));
    let models: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM run_usage_models WHERE run_id=2",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(models, 0);
    let s = stats(&c);
    assert_eq!(s["coverage"]["known"], 2);
    assert_eq!(s["coverage"]["unknown"], 1);
    assert_eq!(s["coverage"]["unknown_reasons"]["no_result"], 1);
}

#[test]
fn a_late_result_upgrades_unknown_but_known_data_is_never_replaced() {
    let (_d, c) = open();
    seed_runs(&c, 1);
    let good = known("s", &[("m", totals(10, 1.0))]);
    assert!(record(&c, 1, &Parsed::unknown(usage::LOG_MISSING)));
    assert!(!record(&c, 1, &Parsed::unknown(usage::NO_RESULT)));
    assert!(record(&c, 1, &good));
    assert!(!record(&c, 1, &Parsed::unknown(usage::LOG_MISSING)));
    assert!(!record(&c, 1, &known("s", &[("m", totals(99, 9.0))])));
    assert_eq!(row(&c, 1, "m"), ("known".into(), Some(10), Some(1.0)));
}

fn dump(c: &Connection) -> String {
    let mut out = String::new();
    for sql in [
        "SELECT kind||run_id||status||IFNULL(reason,'')||IFNULL(recorded_at,'') FROM run_usage ORDER BY 1",
        "SELECT kind||run_id||model||IFNULL(input,'n')||IFNULL(cost,'n') FROM run_usage_models ORDER BY 1",
    ] {
        let mut s = c.prepare(sql).unwrap();
        let rows = s.query_map([], |r| r.get::<_, String>(0)).unwrap();
        for r in rows {
            out.push_str(&r.unwrap());
            out.push('\n');
        }
    }
    out
}

fn items(rows: &[(i64, Parsed)]) -> Vec<IngestItem> {
    rows.iter()
        .map(|(run, usage)| IngestItem {
            kind: "task".into(),
            run: *run,
            usage: usage.clone(),
        })
        .collect()
}

#[test]
fn backfill_twice_changes_nothing_and_order_does_not_matter() {
    let (_d, mut c) = open();
    seed_runs(&c, 3);
    let (a, b, d) = (
        known("s", &[("m", totals(10, 1.0))]),
        known("s", &[("m", totals(25, 2.5))]),
        Parsed::unknown(usage::NO_RESULT),
    );
    // The newest run lands first, as a live finish does ahead of the backfill.
    let batch = items(&[(2, b.clone()), (1, a.clone()), (3, d.clone())]);
    assert_eq!(usage::ingest(&mut c, &batch).unwrap(), 3);
    assert_eq!(row(&c, 2, "m").1, Some(15), "delta is order independent");
    let before = dump(&c);
    assert_eq!(usage::ingest(&mut c, &batch).unwrap(), 0);
    assert_eq!(dump(&c), before);
}

#[test]
fn ingest_rejects_live_and_unknown_runs_without_partial_writes() {
    let (_d, mut c) = open();
    task(&c, 1, "in_progress");
    run(&c, 1, 1, ENDED, "x");
    run(&c, 2, 1, None, "x");
    let p = known("s", &[("m", totals(1, 0.1))]);
    assert!(usage::ingest(&mut c, &items(&[(1, p.clone()), (2, p.clone())])).is_err());
    assert!(usage::ingest(&mut c, &items(&[(99, p)])).is_err());
    let rows: i64 = c
        .query_row("SELECT COUNT(*) FROM run_usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "the failing batch rolled back");
}

const GROUPS: [&str; 15] = [
    "project", "source", "bot", "routine", "workload", "wake", "agent", "mode", "cache", "context",
    "outcome", "day", "week", "kind", "model",
];

#[test]
fn every_breakdown_agrees_with_the_totals() {
    let (_d, c) = open();
    seed_runs(&c, 3);
    task(&c, 2, "escalated");
    run(&c, 4, 2, ENDED, "x");
    run(&c, 5, 2, Some("2026-11-09T00:00:00Z"), "x"); // no usage row at all
    bot(&c, 1);
    bot(&c, 2);
    bot(&c, 3);
    c.execute(
        "INSERT INTO bot_wakes (bot_id, kind, key, due_at, created_at, actor, run_id)
         VALUES (1, 'message', 'msg:1', 'd', 'd', 'owner', 1)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO bot_wakes (bot_id, kind, key, due_at, created_at, actor, run_id)
         VALUES (1, 'wake', 'sched:1:5', 'd', 'd', 'schedule:digest', 3)",
        [],
    )
    .unwrap();
    record(&c, 1, &known("s", &[("opus", totals(10, 1.0))]));
    record(
        &c,
        2,
        &known(
            "s",
            &[("opus", totals(20, 2.0)), ("haiku", totals(3, 0.25))],
        ),
    );
    record(&c, 3, &Parsed::unknown(usage::NO_RESULT));
    record(&c, 4, &known("t", &[("opus", totals(7, 0.5))]));
    let botted = known("b", &[("opus", totals(9, 0.75))]);
    usage::record(&c, "bot", 1, &botted, "test").unwrap();
    let s = stats(&c);
    let total = &s["total"];
    assert_eq!(total["runs"], 8);
    assert_eq!(total["unknown_runs"], 4);
    for group in GROUPS {
        let rows = s["breakdowns"][group]
            .as_array()
            .unwrap_or_else(|| panic!("group {group} missing"));
        let add = |k: &str| rows.iter().map(|r| r[k].as_f64().unwrap()).sum::<f64>();
        assert!(
            (add("cost") - total["cost"].as_f64().unwrap()).abs() < 1e-9,
            "{group} cost"
        );
        for k in ["runs", "known_runs", "unknown_runs", "input", "cache_read"] {
            assert_eq!(add(k), total[k].as_f64().unwrap(), "{group} {k}");
        }
    }
    let tokens: i64 = s["token_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tokens"].as_i64().unwrap())
        .sum();
    let t = |k: &str| total[k].as_i64().unwrap();
    assert_eq!(
        tokens,
        t("input") + t("output") + t("cache_read") + t("cache_write"),
        "thinking is part of output, not added to it"
    );
    let wake = s["breakdowns"]["wake"].as_array().unwrap();
    let cost = |k: &str| wake.iter().find(|w| w["key"] == k).unwrap()["runs"].clone();
    assert_eq!(cost("owner message"), 1);
    assert_eq!(cost("schedule: digest"), 1);
    assert_eq!(cost("heartbeat"), 1);
    assert_eq!(s["finished"]["tasks"], 1);
    assert_eq!(s["finished"]["cost_per_task"].as_f64(), Some(2.25));
}

#[test]
fn the_window_filters_by_end_time() {
    let (_d, c) = open();
    seed_runs(&c, 1);
    record(&c, 1, &known("s", &[("m", totals(1, 1.0))]));
    let ended = 1_793_577_600; // 2026-11-02T00:00:00Z
    let empty = usage::cost_stats(&c, ended + 1, ended + 100, None, &CostQuery::default()).unwrap();
    assert_eq!(empty["total"]["runs"], 0);
    assert_eq!(
        usage::cost_stats(&c, ended, ended + 1, None, &CostQuery::default()).unwrap()["total"]
            ["runs"],
        1
    );
}

#[test]
fn the_series_sums_to_the_totals_and_leaves_unknown_runs_out() {
    let (_d, c) = open();
    task(&c, 1, "done");
    let day0 = 1_793_491_200; // 2026-11-01T00:00:00Z
    run(&c, 1, 1, Some("2026-11-01T12:00:00Z"), "x");
    run(&c, 2, 1, Some("2026-11-02T12:00:00Z"), "x");
    run(&c, 3, 1, Some("2026-11-02T13:00:00Z"), "x");
    run(&c, 4, 1, Some("2026-11-04T01:00:00Z"), "x");
    record(&c, 2, &known("a", &[("m1", totals(10, 2.0))]));
    record(
        &c,
        3,
        &known("b", &[("m1", totals(1, 0.5)), ("m2", totals(2, 1.5))]),
    );
    record(&c, 4, &known("c", &[("m2", totals(4, 1.0))]));
    let until = day0 + 4 * 86_400;
    let s = usage::cost_stats(&c, day0, until, Some(1), &CostQuery::default()).unwrap();
    let buckets = s["series"]["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 4);
    assert_eq!(buckets[1]["start"], day0 + 86_400);
    assert_eq!(buckets[3]["end"], until);
    assert_eq!(
        s["series"]["first_known"],
        day0 + 86_400 + 12 * 3600,
        "the earliest known run, not the earliest run"
    );
    // Run 1 has no usage: counted as an unknown run, with no cost and no split.
    assert_eq!(buckets[0]["runs"], 1);
    assert_eq!(buckets[0]["unknown_runs"], 1);
    assert_eq!(buckets[0]["cost"].as_f64(), Some(0.0));
    assert_eq!(buckets[0]["splits"], json!({}));
    assert_eq!(buckets[2]["runs"], 0);
    for key in [
        "runs",
        "known_runs",
        "unknown_runs",
        "input",
        "output",
        "cache_read",
        "cache_write",
    ] {
        let sum: i64 = buckets.iter().map(|b| b[key].as_i64().unwrap()).sum();
        assert_eq!(sum, s["total"][key].as_i64().unwrap(), "{key}");
    }
    let cost: f64 = buckets.iter().map(|b| b["cost"].as_f64().unwrap()).sum();
    assert_eq!(cost, 5.0);
    assert_eq!(s["total"]["cost"].as_f64(), Some(5.0));
    for b in buckets {
        for dim in ["project", "source", "model"] {
            let split: f64 = b["splits"][dim]
                .as_object()
                .map_or(0.0, |m| m.values().map(|v| v.as_f64().unwrap()).sum());
            assert_eq!(split, b["cost"].as_f64().unwrap(), "{dim}");
        }
    }
    assert_eq!(buckets[1]["splits"]["model"]["m2"].as_f64(), Some(1.5));
    assert_eq!(
        buckets[1]["splits"]["source"]["one-off task"].as_f64(),
        Some(4.0)
    );
}

#[test]
fn weekly_series_anchor_on_since_and_end_short() {
    let (_d, c) = open();
    let day0 = 1_793_491_200;
    let s = usage::cost_stats(&c, day0, day0 + 9 * 86_400, Some(7), &CostQuery::default()).unwrap();
    let buckets = s["series"]["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[1]["start"], day0 + 7 * 86_400);
    assert_eq!(buckets[1]["end"], day0 + 9 * 86_400);
    assert_eq!(s["series"]["first_known"], Value::Null);
    assert!(
        usage::cost_stats(&c, day0, day0 + 1, None, &CostQuery::default()).unwrap()["series"]
            .is_null()
    );
}

fn result_line(session: &str, cost: f64, output: i64, turns: i64) -> String {
    json!({"type":"result","subtype":"success","session_id":session,"num_turns":turns,
        "total_cost_usd":cost,"usage":{"output_tokens":1},
        "modelUsage":{"claude-opus-5-5":{"inputTokens":3,"outputTokens":output,
          "thinkingTokens":2,"cacheReadInputTokens":100,"cacheCreationInputTokens":9,
          "costUSD":cost}}})
    .to_string()
}

fn assistant_line(read: i64, write: i64, subagent: bool) -> String {
    json!({"type":"assistant","parent_tool_use_id":if subagent {json!("tool")} else {Value::Null},
        "message":{"usage":{"input_tokens":2,"cache_read_input_tokens":read,
          "cache_creation_input_tokens":write}}})
    .to_string()
}

fn write(p: &Path, lines: &[String]) {
    std::fs::write(p, lines.join("\n") + "\n").unwrap();
}

#[test]
fn parse_takes_the_last_cumulative_result_and_sums_turns() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("1.log");
    write(
        &p,
        &[
            "not json".to_owned(),
            assistant_line(10, 500, false),
            assistant_line(9_999_999, 1, true),
            result_line("s", 1.0, 50, 7),
            assistant_line(4000, 100, false),
            result_line("s", 2.5, 90, 2),
        ],
    );
    let u = usage::parse_log(&p);
    assert_eq!(u.unknown, None);
    assert_eq!(u.session_id.as_deref(), Some("s"));
    assert_eq!(u.turns, Some(9));
    let m = &u.models["claude-opus-5-5"];
    assert_eq!((m.output, m.thinking, m.cost), (90, 2, 2.5));
    assert_eq!(u.cache_start.as_deref(), Some("cold"), "first main call");
    assert_eq!(u.context_tokens, Some(4102), "last main call, no subagent");
}

#[test]
fn logs_without_a_usable_result_are_unknown() {
    let d = tempfile::tempdir().unwrap();
    let missing = usage::parse_log(&d.path().join("nope.log"));
    assert_eq!(missing.unknown.as_deref(), Some(usage::LOG_MISSING));
    let killed = d.path().join("killed.log");
    write(&killed, &[assistant_line(1, 1, false)]);
    assert_eq!(
        usage::parse_log(&killed).unknown.as_deref(),
        Some(usage::NO_RESULT)
    );
    let bare = d.path().join("bare.log");
    write(
        &bare,
        &[json!({"type":"result","session_id":"s","total_cost_usd":0.1}).to_string()],
    );
    assert_eq!(
        usage::parse_log(&bare).unknown.as_deref(),
        Some(usage::NO_MODEL_USAGE)
    );
    let directory = usage::parse_log(d.path());
    assert_eq!(directory.unknown.as_deref(), Some(usage::UNREADABLE));
}

#[test]
fn finish_records_usage_and_a_bad_payload_never_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("db")).unwrap();
    for _ in 0..2 {
        db::add_task(
            &mut c,
            &db::Actor::new("human"),
            &db::NewTask {
                agent: None,
                project: "p",
                title: "t",
                detail: "",
                state: "pending",
            },
        )
        .unwrap();
    }
    let reg = remote::register(&c, &json!({"id":"one","generation":""})).unwrap();
    let generation = reg["generation"].clone();
    let request = |id: &str, op: &str, body: Value| json!({"dispatcher":"one","generation":generation,"operation_id":id,"op":op,"body":body});
    let finish = |task: i64, claim: &str, usage: Value| {
        let claimed = request(
            &format!("claim{task}"),
            "claim",
            json!({"project":"p","claim":claim,"max_runs":3}),
        );
        remote::operation(&c, &claimed).unwrap();
        let prepared = request(
            &format!("prepare{task}"),
            "prepare",
            json!({"task":task,"claim":claim,"agent":"a","mode":"fresh","session_id":"s","log":format!("/tmp/{task}.log"),"session":{"agent":"a","session_id":"s","resume_count":0},"event":{}}),
        );
        let id = remote::operation(&c, &prepared).unwrap().as_i64().unwrap();
        remote::operation(
            &c,
            &request(&format!("start{task}"), "started", json!({"run":id})),
        )
        .unwrap();
        let end = request(
            &format!("finish{task}"),
            "finish",
            json!({"usage":usage,"run":id,"code":0,"resumed":false,"note":"exit","event":{}}),
        );
        remote::operation(&c, &end).unwrap();
        remote::operation(&c, &end).unwrap();
        id
    };
    let good = finish(
        1,
        "dispatch:a",
        serde_json::to_value(known("s", &[("m", totals(10, 1.0))])).unwrap(),
    );
    let bad = finish(2, "dispatch:b", json!({"models":"not a map"}));
    let stored: (String, f64) = c
        .query_row(
            "SELECT u.status, m.cost FROM run_usage u JOIN run_usage_models m USING (kind, run_id)
             WHERE u.run_id=?1",
            [good],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, ("known".to_owned(), 1.0));
    assert_eq!(db::get_run(&c, bad).unwrap().exit_code, Some(0));
}

fn backfill(srv: &Server, logs: &Path) -> Value {
    let out = srv
        .cli(&["usage", "backfill", "--log-dir", logs.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn cli_backfill_reports_coverage_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let logs = dir.path().join("logs");
    std::fs::create_dir(&logs).unwrap();
    write(&logs.join("1.log"), &[result_line("s", 1.5, 10, 3)]);
    write(&logs.join("2.log"), &[assistant_line(1, 1, false)]);
    let outside = dir.path().join("outside.log");
    write(&outside, &[result_line("z", 9.0, 10, 3)]);
    {
        let c = db::open(&srv.db).unwrap();
        task(&c, 1, "done");
        let path = |n: &str| logs.join(n).to_str().unwrap().to_owned();
        run(&c, 1, 1, ENDED, &path("1.log"));
        run(&c, 2, 1, ENDED, &path("2.log"));
        run(&c, 3, 1, ENDED, outside.to_str().unwrap());
        run(&c, 4, 1, ENDED, &path("gone.log"));
    }
    let first = backfill(&srv, &logs);
    assert_eq!(first["examined"], 4);
    assert_eq!(first["readable"], 1);
    assert_eq!(first["changed"], 4);
    assert_eq!(first["coverage"]["known"], 1);
    assert_eq!(first["coverage"]["unknown"], 3);
    assert_eq!(first["coverage"]["unknown_reasons"]["log_missing"], 2);
    // Only unknown rows are read again, and nothing about them changes.
    let second = backfill(&srv, &logs);
    assert_eq!(second["examined"], 3);
    assert_eq!(second["changed"], 0);
    assert_eq!(second["coverage"], first["coverage"]);
}

const DAY0: i64 = 1_793_491_200; // 2026-11-01T00:00:00Z

fn cost_run(c: &Connection, id: i64, task: i64, project: &str, agent: &str, day: i64) {
    c.execute(
        "INSERT INTO runs (id, task_id, project, agent, mode, session_id, claim, started_at,
           ended_at, log_path)
         VALUES (?1, ?2, ?3, ?4, 'fresh', 's', 'c', '2026-11-01T00:00:00Z', ?5, 'x')",
        params![
            id,
            task,
            project,
            agent,
            format!("2026-11-{:02}T12:00:00Z", day + 1)
        ],
    )
    .unwrap();
}

fn cost_bot_run(c: &Connection, id: i64, day: i64) {
    c.execute(
        "INSERT INTO bot_runs (id, bot_id, agent, mode, session_id, claim, dispatcher, generation,
           started_at, ended_at, log_path)
         VALUES (?1, 1, 'claude', 'resume', 'b', 'c', 'd', 'g', '2026-11-01T00:00:00Z', ?2, 'x')",
        params![id, format!("2026-11-{:02}T12:00:00Z", day + 1)],
    )
    .unwrap();
}

/// Eight runs: alpha/beta tasks, a routine and a bot, on two agents and two models,
/// with an unknown run and a never-recorded run among them.
fn cost_fixture(c: &Connection) {
    task(c, 1, "done");
    task(c, 2, "escalated");
    task(c, 3, "done");
    c.execute(
        "INSERT INTO events (task_id, at, actor, kind, to_value)
         VALUES (1, '2026-11-01T00:00:00Z', 'a', 'workload', 'feature'),
                (2, '2026-11-01T00:00:00Z', 'a', 'workload', 'bugfix')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO routines (id, config, enabled, next_at, created_at, updated_at)
         VALUES (1, '{\"name\":\"nightly\"}', 1, 0, 'd', 'd')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO routine_occurrences (routine_id, identity, task_id, snapshot, created_at)
         VALUES (1, 'i', 3, '{}', 'd')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT INTO bots (id, name, project, agent, identity, conduct, notes,
           heartbeat_seconds, nudge_after_seconds, state, created_at, updated_at)
         VALUES (1, 'axe', 'ops', 'claude', '', '', '', 14400, 600, 'active', 'd', 'd')",
        [],
    )
    .unwrap();
    cost_run(c, 1, 1, "alpha", "claude", 0);
    cost_run(c, 2, 1, "alpha", "codex", 1);
    cost_run(c, 3, 2, "beta", "claude", 1);
    cost_run(c, 4, 2, "beta", "claude", 2);
    cost_run(c, 5, 3, "alpha", "claude", 2);
    cost_run(c, 6, 3, "alpha", "claude", 3);
    cost_bot_run(c, 1, 3);
    cost_bot_run(c, 2, 3);
    record(c, 1, &known("s1", &[("m1", totals(10, 1.0))]));
    record(
        c,
        2,
        &known("s2", &[("m1", totals(20, 2.0)), ("m2", totals(5, 0.5))]),
    );
    record(c, 3, &Parsed::unknown(usage::NO_RESULT));
    record(c, 4, &known("s4", &[("m2", totals(7, 4.0))]));
    record(c, 5, &known("s5", &[("m1", totals(3, 8.0))]));
    usage::record(c, "bot", 1, &known("s7", &[("m1", totals(9, 16.0))]), "t").unwrap();
    usage::record(c, "bot", 2, &Parsed::unknown(usage::NO_RESULT), "t").unwrap();
}

const WINDOW_END: i64 = DAY0 + 5 * 86_400;

fn filtered(c: &Connection, query: CostQuery) -> Value {
    usage::cost_stats(c, DAY0, WINDOW_END, Some(1), &query).unwrap()
}

fn cost_of(s: &Value) -> f64 {
    s["total"]["cost"].as_f64().unwrap()
}

/// Every total, table, series and coverage figure describes the same runs.
fn assert_consistent(s: &Value) {
    let total = &s["total"];
    assert_eq!(total["runs"], s["coverage"]["runs"]);
    assert_eq!(total["unknown_runs"], s["coverage"]["unknown"]);
    let reasons: i64 = s["coverage"]["unknown_reasons"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_i64().unwrap())
        .sum();
    assert_eq!(reasons, total["unknown_runs"].as_i64().unwrap());
    let add = |rows: &Value, k: &str| -> f64 {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|r| r[k].as_f64().unwrap())
            .sum()
    };
    for group in usage::BREAKDOWNS {
        let rows = &s["breakdowns"][group];
        if total["runs"] == 0 {
            assert!(rows.is_null(), "{group}");
            continue;
        }
        assert!((add(rows, "cost") - cost_of(s)).abs() < 1e-9, "{group}");
        for k in ["runs", "known_runs", "unknown_runs", "input"] {
            assert_eq!(add(rows, k), total[k].as_f64().unwrap(), "{group} {k}");
        }
    }
    let buckets = &s["series"]["buckets"];
    assert!((add(buckets, "cost") - cost_of(s)).abs() < 1e-9);
    for k in ["runs", "known_runs", "unknown_runs", "input"] {
        assert_eq!(add(buckets, k), total[k].as_f64().unwrap(), "series {k}");
    }
}

#[test]
fn an_unfiltered_report_is_unchanged() {
    let (_d, c) = open();
    cost_fixture(&c);
    let golden: Value =
        serde_json::from_str(include_str!("fixtures/cost_stats_no_filter.json")).unwrap();
    assert_eq!(filtered(&c, CostQuery::default()), golden);
}

#[test]
fn each_filter_narrows_every_figure_alike() {
    let (_d, c) = open();
    cost_fixture(&c);
    let q = CostQuery::default;
    // (query, runs, unknown runs, cost)
    let cases: Vec<(CostQuery, i64, i64, f64)> = vec![
        (
            CostQuery {
                project: Some("alpha"),
                ..q()
            },
            4,
            1,
            11.5,
        ),
        (
            CostQuery {
                project: Some("beta"),
                ..q()
            },
            2,
            1,
            4.0,
        ),
        (
            CostQuery {
                source: Some("bot"),
                ..q()
            },
            2,
            1,
            16.0,
        ),
        (
            CostQuery {
                source: Some("routine"),
                ..q()
            },
            2,
            1,
            8.0,
        ),
        (
            CostQuery {
                source: Some("one-off task"),
                ..q()
            },
            4,
            1,
            7.5,
        ),
        (
            CostQuery {
                bot: Some("axe"),
                ..q()
            },
            2,
            1,
            16.0,
        ),
        (
            CostQuery {
                routine: Some("nightly"),
                ..q()
            },
            2,
            1,
            8.0,
        ),
        (
            CostQuery {
                agent: Some("codex"),
                ..q()
            },
            1,
            0,
            2.5,
        ),
        (
            CostQuery {
                workload: Some("feature"),
                ..q()
            },
            2,
            0,
            3.5,
        ),
        (
            CostQuery {
                workload: Some("bot"),
                ..q()
            },
            2,
            1,
            16.0,
        ),
        (
            CostQuery {
                project: Some("nope"),
                ..q()
            },
            0,
            0,
            0.0,
        ),
    ];
    for (query, runs, unknown, cost) in cases {
        let s = filtered(&c, query);
        assert_eq!(s["total"]["runs"], runs, "{cost}");
        assert_eq!(s["total"]["unknown_runs"], unknown, "{cost}");
        assert_eq!(cost_of(&s), cost);
        assert_consistent(&s);
    }
}

#[test]
fn filters_combine() {
    let (_d, c) = open();
    cost_fixture(&c);
    let s = filtered(
        &c,
        CostQuery {
            project: Some("alpha"),
            agent: Some("claude"),
            source: Some("one-off task"),
            ..Default::default()
        },
    );
    assert_eq!(s["total"]["runs"], 1);
    assert_eq!(cost_of(&s), 1.0);
    assert_consistent(&s);
    let none = filtered(
        &c,
        CostQuery {
            project: Some("beta"),
            agent: Some("codex"),
            ..Default::default()
        },
    );
    assert_eq!(none["total"]["runs"], 0);
    assert_consistent(&none);
}

#[test]
fn a_model_filter_keeps_that_models_spend_and_unknown_runs() {
    let (_d, c) = open();
    cost_fixture(&c);
    let s = filtered(
        &c,
        CostQuery {
            model: Some("m2"),
            ..Default::default()
        },
    );
    // Run 2 keeps only its m2 half; runs with known usage and no m2 drop out.
    assert_eq!(cost_of(&s), 4.5);
    assert_eq!(s["total"]["input"], 12);
    assert_eq!(s["total"]["known_runs"], 2);
    assert_eq!(s["total"]["unknown_runs"], 3, "unknown, not dropped");
    assert_eq!(s["coverage"]["unknown_reasons"]["no_result"], 2);
    assert_consistent(&s);
    let rows = s["breakdowns"]["model"].as_array().unwrap();
    let keys: Vec<_> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["m2", "unknown"]);
    let only = filtered(
        &c,
        CostQuery {
            model: Some("unknown"),
            ..Default::default()
        },
    );
    assert_eq!(only["total"]["known_runs"], 0);
    assert_eq!(only["total"]["unknown_runs"], 3);
    assert_eq!(cost_of(&only), 0.0);
    assert_consistent(&only);
}

#[test]
fn unknown_runs_stay_unknown_under_a_filter() {
    let (_d, c) = open();
    cost_fixture(&c);
    let s = filtered(
        &c,
        CostQuery {
            project: Some("beta"),
            ..Default::default()
        },
    );
    assert_eq!(s["total"]["unknown_runs"], 1);
    let day1 = &s["series"]["buckets"][1];
    assert_eq!(day1["unknown_runs"], 1);
    assert_eq!(day1["known_runs"], 0);
    assert_eq!(day1["splits"], json!({}), "no zero-cost split for it");
}

#[test]
fn filters_do_not_move_where_usage_data_begins() {
    let (_d, c) = open();
    cost_fixture(&c);
    let s = filtered(
        &c,
        CostQuery {
            bot: Some("axe"),
            ..Default::default()
        },
    );
    assert_eq!(s["series"]["first_known"], DAY0 + 12 * 3600);
}

#[test]
fn every_breakdown_sums_to_the_filtered_total() {
    let (_d, c) = open();
    cost_fixture(&c);
    for project in [None, Some("alpha")] {
        for breakdown in usage::BREAKDOWNS {
            let s = filtered(
                &c,
                CostQuery {
                    project,
                    breakdown: Some(breakdown),
                    ..Default::default()
                },
            );
            let series: f64 = s["series"]["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|b| b["splits"][breakdown].as_object())
                .flat_map(|m| m.values())
                .map(|v| v.as_f64().unwrap())
                .sum();
            assert!((series - cost_of(&s)).abs() < 1e-9, "{breakdown}");
            assert_consistent(&s);
        }
    }
}

#[test]
fn filter_options_list_the_period_whatever_is_selected() {
    let (_d, c) = open();
    cost_fixture(&c);
    let s = filtered(
        &c,
        CostQuery {
            project: Some("beta"),
            options: true,
            ..Default::default()
        },
    );
    let o = &s["filter_options"];
    assert_eq!(o["project"], json!(["alpha", "beta", "ops"]));
    assert_eq!(o["source"], json!(["bot", "one-off task", "routine"]));
    assert_eq!(o["bot"], json!(["axe"]), "no 'not a bot' placeholder");
    assert_eq!(o["routine"], json!(["nightly"]));
    assert_eq!(o["agent"], json!(["claude", "codex"]));
    assert_eq!(o["model"], json!(["m1", "m2", "unknown"]));
    assert_eq!(o["workload"], json!(["bot", "bugfix", "feature", "unset"]));
    assert!(filtered(&c, CostQuery::default())["filter_options"].is_null());
}

#[test]
fn the_cost_endpoint_applies_filters_and_rejects_bad_breakdowns() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    cost_fixture(&db::open(&srv.db).unwrap());
    let get = |extra: &str| {
        ureq::get(&format!(
            "{}/api/stats/cost?since=2026-11-01T00:00:00Z&until=2026-11-06T00:00:00Z&step_days=1{extra}",
            srv.url
        ))
        .call()
        .map_err(|e| matches!(e, ureq::Error::Status(400, _)))
    };
    let s: Value = get("&source=one-off%20task&project=alpha&breakdown=agent&options=1")
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(s["total"]["runs"], 2);
    assert_eq!(cost_of(&s), 3.5);
    assert_eq!(
        s["series"]["buckets"][1]["splits"]["agent"]["codex"].as_f64(),
        Some(2.5)
    );
    assert_eq!(s["filter_options"]["project"][2], "ops");
    assert_eq!(get("&breakdown=nope").err(), Some(true));
}
