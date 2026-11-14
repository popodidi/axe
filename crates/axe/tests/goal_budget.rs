//! Goal budgets (G2): 7-day cost attribution, owner-attention accounting, the
//! one hard gate on new goal-linked tasks, the `goal` Cost view dimension, and
//! `axe stats --origin`.
mod common;

use axe::bot::{self, ts, NewBot};
use axe::db::{self, Actor, Changes, NewTask};
use axe::goal::{self, Edit, NewGoal};
use axe::usage::{self, CostQuery, Parsed, Totals};
use rusqlite::{params, Connection};
use serde_json::Value;

const DAY: i64 = 86_400;
const NOTE: &str = "blocked: x\nresolver: owner: decide\nresume: y";

fn human() -> Actor {
    Actor::new("human")
}

fn now() -> i64 {
    axe::remote::epoch()
}

fn new_bot(name: &str) -> NewBot {
    NewBot {
        name: name.into(),
        project: "p".into(),
        agent: Some("fake".into()),
        identity: String::new(),
        conduct: String::new(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    }
}

/// Bots `a` (id 1) and `b` (id 2); goals `g` and `h`, proposed and activated.
fn setup() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    for b in ["a", "b"] {
        bot::create(&mut c, &human(), &new_bot(b), now()).unwrap();
    }
    for g in ["g", "h"] {
        let new = NewGoal {
            name: g.into(),
            project: "p".into(),
            ..Default::default()
        };
        goal::propose(&mut c, &human(), &new, None, now()).unwrap();
        goal::change(&mut c, &human(), g, "activate", "go", None).unwrap();
    }
    (dir, c)
}

fn task(c: &mut Connection, goal: Option<&str>) -> anyhow::Result<i64> {
    let t = NewTask {
        agent: None,
        project: "p",
        title: "t",
        detail: "",
        state: "pending",
    };
    db::add_linked_task(c, &human(), &t, &[], None, goal)
}

fn set_goal(c: &mut Connection, id: i64, goal: &str) -> anyhow::Result<()> {
    let ch = Changes {
        goal: Some(goal.into()),
        ..Default::default()
    };
    db::set_task(c, &human(), id, &ch).map(|_| ())
}

fn known(session: String, cost: f64) -> Parsed {
    Parsed {
        unknown: None,
        session_id: Some(session),
        models: [(
            "m".to_owned(),
            Totals {
                cost,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        turns: Some(1),
        context_tokens: Some(1),
        cache_start: Some("warm".into()),
    }
}

fn task_run(c: &Connection, id: i64, task: i64, ended: i64, cost: f64) {
    c.execute(
        "INSERT INTO runs (id, task_id, project, agent, mode, session_id, claim, started_at,
           ended_at, log_path)
         VALUES (?1, ?2, 'p', 'claude', 'fresh', 's', 'c', ?3, ?3, 'x')",
        params![id, task, ts(ended)],
    )
    .unwrap();
    usage::record(c, "task", id, &known(format!("t{id}"), cost), "test").unwrap();
}

/// A bot run that delivered wakes with these `(key, body)` pairs.
fn bot_run(c: &Connection, id: i64, bot: i64, ended: i64, cost: f64, wakes: &[(&str, &str)]) {
    c.execute(
        "INSERT INTO bot_runs (id, bot_id, agent, mode, session_id, claim, dispatcher, generation,
           started_at, ended_at, log_path)
         VALUES (?1, ?2, 'claude', 'resume', 'b', 'c', 'd', 'g', ?3, ?3, 'x')",
        params![id, bot, ts(ended)],
    )
    .unwrap();
    for (key, body) in wakes {
        c.execute(
            "INSERT INTO bot_wakes (bot_id, kind, key, body, due_at, created_at, actor, run_id)
             VALUES (?1, 'wake', ?2, ?3, ?4, ?4, 'human', ?5)",
            params![bot, format!("{key}{id}"), body, ts(ended), id],
        )
        .unwrap();
    }
    usage::record(c, "bot", id, &known(format!("b{id}"), cost), "test").unwrap();
}

fn budget(c: &Connection, name: &str) -> goal::Budget {
    goal::detail(c, name).unwrap().goal.budget.unwrap()
}

fn edit(c: &mut Connection, name: &str, e: Edit) {
    goal::edit(c, &human(), name, &e, "owner", None).unwrap();
}

fn cost_edit(v: &str) -> Edit {
    Edit {
        budget_cost_week: Some(v.into()),
        ..Default::default()
    }
}

fn attention_edit(v: &str) -> Edit {
    Edit {
        budget_attention_week: Some(v.into()),
        ..Default::default()
    }
}

/// A linked task whose link is ten days old, so older runs belong to it too.
fn old_linked_task(c: &mut Connection, goal: &str) -> i64 {
    let id = task(c, Some(goal)).unwrap();
    c.execute(
        "UPDATE events SET at=?1 WHERE task_id=?2 AND kind='goal'",
        params![ts(now() - 10 * DAY), id],
    )
    .unwrap();
    id
}

#[test]
fn cost_counts_linked_task_runs_and_single_goal_bot_runs_in_the_window() {
    let (_d, mut c) = setup();
    let t = now();
    let linked = old_linked_task(&mut c, "g");
    let other = task(&mut c, None).unwrap();
    task_run(&c, 1, linked, t - 3600, 2.0);
    task_run(&c, 2, linked, t - 8 * DAY, 5.0); // outside the 7-day window
    task_run(&c, 3, other, t - 3600, 9.0); // not linked
    bot_run(
        &c,
        10,
        1,
        t - 60,
        1.5,
        &[("k", "[goal g] a"), ("j", "[goal g] b")],
    );
    bot_run(
        &c,
        11,
        2,
        t - 60,
        7.0,
        &[("k", "[goal g] a"), ("j", "owner says hi")],
    );
    bot_run(
        &c,
        12,
        2,
        t - 60,
        0.25,
        &[("k", "[goal g] a"), ("ride:", "digest")],
    );
    bot_run(&c, 13, 1, t - 60, 3.0, &[("k", "[goal h] a")]);
    bot_run(
        &c,
        14,
        1,
        t - 60,
        4.0,
        &[("k", "[goal g] a"), ("j", "[goal h] b")],
    );
    bot_run(&c, 15, 2, t - 9 * DAY, 6.0, &[("k", "[goal g] a")]);

    let b = budget(&c, "g");
    assert!((b.cost_spent - 3.75).abs() < 1e-9, "{b:?}");
    assert!((b.cost_tasks - 2.0).abs() < 1e-9, "{b:?}");
    assert_eq!(b.cost_bots.len(), 2, "{b:?}");
    assert!((b.cost_bots["a"] - 1.5).abs() < 1e-9);
    assert!((b.cost_bots["b"] - 0.25).abs() < 1e-9);
    assert_eq!(b.unattributed_runs, 2, "mixed wakes (runs 11 and 14)");
    assert_eq!(b.window.days, 7);
    let h = budget(&c, "h");
    assert!((h.cost_spent - 3.0).abs() < 1e-9, "{h:?}");
    assert_eq!(h.unattributed_runs, 1);

    // A task run counts for the goal its task had when the run ended.
    set_goal(&mut c, linked, "h").unwrap();
    task_run(&c, 4, linked, now(), 0.5);
    assert!((budget(&c, "g").cost_spent - 3.75).abs() < 1e-9);
    assert!((budget(&c, "h").cost_spent - 3.5).abs() < 1e-9);

    // `goal list` carries the same budget.
    let listed = goal::list(&c, None, None).unwrap();
    assert_eq!(listed[0].budget.as_ref().unwrap().cost_spent, 3.75);
    assert!(goal::get(&c, "g").unwrap().budget.is_none());
}

#[test]
fn goal_is_a_cost_breakdown_and_filter() {
    let (_d, mut c) = setup();
    let t = now();
    let linked = old_linked_task(&mut c, "g");
    task_run(&c, 1, linked, t - 3600, 2.0);
    task_run(&c, 2, linked, t - 8 * DAY, 5.0);
    bot_run(&c, 10, 1, t - 60, 1.5, &[("k", "[goal g] a")]);
    bot_run(&c, 11, 2, t - 60, 7.0, &[("k", "[goal g] a"), ("j", "hi")]);
    bot_run(&c, 12, 2, t - 60, 1.0, &[("k", "untagged")]);
    let all = usage::cost_stats(&c, 0, t + 60, None, &CostQuery::default()).unwrap();
    let by: Vec<(String, f64)> = all["breakdowns"]["goal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["key"].as_str().unwrap().to_owned(),
                v["cost"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by,
        [
            ("g".to_owned(), 8.5),
            ("mixed goals".to_owned(), 7.0),
            ("no goal".to_owned(), 1.0)
        ]
    );
    let only_g = usage::cost_stats(
        &c,
        0,
        t + 60,
        None,
        &CostQuery {
            goal: Some("g"),
            options: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(only_g["total"]["runs"], 3);
    assert_eq!(only_g["total"]["cost"], 8.5);
    assert_eq!(only_g["filter_options"]["goal"], serde_json::json!(["g"]));
    assert!(usage::BREAKDOWNS.contains(&"goal"));
    let series = usage::cost_stats(
        &c,
        t - 2 * DAY,
        t + 60,
        Some(1),
        &CostQuery {
            breakdown: Some("goal"),
            ..Default::default()
        },
    )
    .unwrap();
    let splits: f64 = series["series"]["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["splits"]["goal"]["g"].as_f64())
        .sum();
    assert!((splits - 3.5).abs() < 1e-9, "{series}");
}

fn escalate(c: &mut Connection, id: i64) {
    let ch = Changes {
        state: Some("escalated".into()),
        note: Some(NOTE.into()),
        escalation_class: Some("decision".into()),
        ..Default::default()
    };
    db::set_task(c, &Actor::new("agent"), id, &ch).unwrap();
}

fn pending(c: &mut Connection, id: i64) {
    let ch = Changes {
        state: Some("pending".into()),
        ..Default::default()
    };
    db::set_task(c, &human(), id, &ch).unwrap();
}

#[test]
fn attention_counts_escalations_and_approvals_not_goal_setup() {
    let (_d, mut c) = setup();
    // propose + activate (setup) and every edit kind are free.
    assert_eq!(budget(&c, "g").attention_spent, 0);
    edit(&mut c, "g", cost_edit("50"));
    edit(&mut c, "g", attention_edit("4"));
    edit(
        &mut c,
        "g",
        Edit {
            intent: Some("new intent\n".into()),
            ..Default::default()
        },
    );
    edit(
        &mut c,
        "g",
        Edit {
            deadline: Some("2030-01-01".into()),
            ..Default::default()
        },
    );
    goal::record(&mut c, &human(), "g", "note", None, "trajectory is free").unwrap();
    assert_eq!(budget(&c, "g").attention_spent, 0, "goal setup is free");
    assert!(budget(&c, "g").exhausted().is_none());

    // One owner-resolved escalation on a linked task counts 1.
    let linked = task(&mut c, Some("g")).unwrap();
    escalate(&mut c, linked);
    pending(&mut c, linked);
    assert_eq!(budget(&c, "g").attention_spent, 1, "1 episode");

    escalate(&mut c, linked);
    db::approve(&mut c, &human(), linked, "abc", "main", None).unwrap();
    let unlinked = task(&mut c, None).unwrap();
    escalate(&mut c, unlinked);
    db::approve(&mut c, &human(), unlinked, "abc", "main", None).unwrap();
    assert_eq!(
        budget(&c, "g").attention_spent,
        3,
        "2 episodes + 1 approval"
    );
    assert_eq!(budget(&c, "h").attention_spent, 0);

    // Work before a task was linked is not the goal's.
    set_goal(&mut c, unlinked, "h").unwrap();
    assert_eq!(budget(&c, "h").attention_spent, 0);

    // Events outside the window do not count.
    c.execute(
        "UPDATE events SET at=?1 WHERE task_id=?2 AND kind='approval'",
        params![ts(now() - 8 * DAY), linked],
    )
    .unwrap();
    assert_eq!(budget(&c, "g").attention_spent, 2);
}

#[test]
fn attention_counts_needed_and_unrated_asks_not_wasted_ones() {
    let (_d, mut c) = setup();
    let linked = task(&mut c, Some("g")).unwrap();
    // Episode 1: escalated and approved (2 units); episode 2: escalated (1);
    // episode 3: escalated and approved (2). An approval outside an episode (1).
    escalate(&mut c, linked);
    db::approve(&mut c, &human(), linked, "abc", "main", None).unwrap();
    escalate(&mut c, linked);
    pending(&mut c, linked);
    escalate(&mut c, linked);
    db::approve(&mut c, &human(), linked, "abc", "main", None).unwrap();
    let blocked = Changes {
        state: Some("blocked".into()),
        ..Default::default()
    };
    db::set_task(&mut c, &human(), linked, &blocked).unwrap();
    db::approve(&mut c, &human(), linked, "abc", "main", None).unwrap();
    assert_eq!(budget(&c, "g").attention_spent, 6, "all unrated count");

    let owner = Actor::new("owner");
    let rate = |c: &mut Connection, ask: &str, r: &str| {
        axe::ask::rate(c, &owner, ask, r).unwrap();
    };
    rate(&mut c, &format!("t{linked}.1"), "needed");
    assert_eq!(budget(&c, "g").attention_spent, 6, "needed still counts");
    // A wasted episode drops its escalation and the approval inside it.
    rate(&mut c, &format!("t{linked}.1"), "could_decide");
    assert_eq!(budget(&c, "g").attention_spent, 4);
    rate(&mut c, &format!("t{linked}.2"), "should_not_exist");
    assert_eq!(budget(&c, "g").attention_spent, 3);
    // Latest rating wins: back to needed restores the units.
    rate(&mut c, &format!("t{linked}.1"), "needed");
    assert_eq!(budget(&c, "g").attention_spent, 5);
    // Episode 3 wasted: its approval goes, the later one outside any episode stays.
    rate(&mut c, &format!("t{linked}.3"), "could_decide");
    assert_eq!(budget(&c, "g").attention_spent, 3);
}

fn refused(r: anyhow::Result<impl std::fmt::Debug>, want: &str) {
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("over its weekly budget"), "{e}");
    assert!(e.contains(want), "{e}");
}

#[test]
fn a_spent_budget_refuses_new_linked_tasks_until_raised() {
    let (_d, mut c) = setup();
    let t = now();
    let linked = old_linked_task(&mut c, "g");
    task_run(&c, 1, linked, t - 3600, 4.0);
    let free = task(&mut c, None).unwrap();

    // No budget never gates, whatever the spend.
    task(&mut c, Some("g")).unwrap();
    edit(&mut c, "g", cost_edit("3"));
    refused(task(&mut c, Some("g")), "cost $4.00 of $3.00");
    refused(set_goal(&mut c, free, "g"), "cost $4.00 of $3.00");
    // Keeping, clearing or moving away from a goal is never refused.
    set_goal(&mut c, linked, "g").unwrap();
    set_goal(&mut c, free, "none").unwrap();
    assert!(db::current_goal(&c, free).unwrap().is_none());
    // Nothing else is refused: unlinked tasks and other goals.
    task(&mut c, None).unwrap();
    task(&mut c, Some("h")).unwrap();

    edit(&mut c, "g", cost_edit("10"));
    task(&mut c, Some("g")).unwrap();
    set_goal(&mut c, free, "g").unwrap();

    // Attention: setting the budget costs nothing; one escalation spends 1 of 1.
    edit(&mut c, "h", attention_edit("1"));
    let on_h = task(&mut c, Some("h")).unwrap();
    escalate(&mut c, on_h);
    refused(task(&mut c, Some("h")), "attention 1 of 1");
    edit(&mut c, "h", attention_edit("5"));
    task(&mut c, Some("h")).unwrap();
    edit(&mut c, "h", attention_edit("none"));
    assert!(budget(&c, "h").exhausted().is_none());
}

#[test]
fn cli_shows_spend_and_reports_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let srv = common::Server::start(dir.path());
    let goal_file = dir.path().join("goal.json");
    std::fs::write(
        &goal_file,
        r#"{"name":"g","project":"p","budget_cost_week":1}"#,
    )
    .unwrap();
    srv.ok(&["goal", "propose", "--file", goal_file.to_str().unwrap()]);
    srv.ok(&["goal", "activate", "g", "--why", "go"]);
    let id = srv.ok(&["add", "--project", "p", "--title", "t", "--goal", "g"]);
    {
        let c = Connection::open(dir.path().join("t.db")).unwrap();
        task_run(&c, 1, id.trim().parse().unwrap(), now(), 2.5);
    }
    let show: Value = srv.get("/api/goals/g");
    assert_eq!(show["budget"]["cost_week"], 1.0);
    assert_eq!(show["budget"]["cost_spent"], 2.5);
    assert_eq!(show["budget"]["attention_spent"], 0);
    assert_eq!(show["budget"]["window"]["days"], 7);
    let text = srv.ok(&["goal", "show", "g"]);
    assert!(
        text.contains("spent (last 7 days): cost $2.50  attention 0"),
        "{text}"
    );
    assert!(text.contains("over budget: cost $2.50 of $1.00"), "{text}");
    let listed = srv.ok(&["goal", "list"]);
    assert!(listed.contains("cost=$2.50/$1.00  attention=-"), "{listed}");

    let out = srv.run(&["add", "--project", "p", "--title", "t2", "--goal", "g"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cost $2.50 of $1.00"), "{err}");
    srv.ok(&[
        "goal",
        "edit",
        "g",
        "--budget-cost-week",
        "5",
        "--why",
        "more",
    ]);
    srv.ok(&["add", "--project", "p", "--title", "t2", "--goal", "g"]);

    // The run ended this second; the default `until` (now) is exclusive.
    let cost = srv.get(&format!(
        "/api/stats/cost?since=1d&until={}&breakdown=goal&goal=g",
        ts(now() + 60)
    ));
    assert_eq!(cost["total"]["cost"], 2.5);
    assert_eq!(cost["breakdowns"]["goal"][0]["key"], "g");
}

#[test]
fn stats_origin_splits_routine_tasks_from_normal_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let srv = common::Server::start(dir.path());
    let normal = srv.ok(&["add", "--project", "p", "--title", "n"]);
    let routine = srv.ok(&["add", "--project", "p", "--title", "r"]);
    {
        let c = Connection::open(dir.path().join("t.db")).unwrap();
        c.execute(
            "INSERT INTO routines (id, config, enabled, next_at, created_at, updated_at)
             VALUES (1, '{\"name\":\"nightly\"}', 1, 0, 'd', 'd')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO routine_occurrences (routine_id, identity, task_id, snapshot, created_at)
             VALUES (1, 'i', ?1, '{}', 'd')",
            [routine.trim().parse::<i64>().unwrap()],
        )
        .unwrap();
    }
    let _ = normal;
    let stats = |extra: &[&str]| -> Value {
        let mut args = vec!["stats", "--project", "p"];
        args.extend_from_slice(extra);
        serde_json::from_str(&srv.ok(&args)).unwrap()
    };
    let all = stats(&[]);
    assert_eq!(all["overall"]["tasks"], 2);
    assert!(
        all.get("origin").is_none(),
        "no origin keeps today's output"
    );
    let task = stats(&["--origin", "task"]);
    assert_eq!(task["overall"]["tasks"], 1);
    assert_eq!(task["origin"], "task");
    assert_eq!(task["overall"]["backlog"]["runnable"], 1);
    let routine = stats(&["--origin", "routine"]);
    assert_eq!(routine["overall"]["tasks"], 1);
    let out = srv.run(&["stats", "--project", "p", "--origin", "bogus"]);
    assert!(!out.status.success());
    let r = ureq::get(&format!(
        "{}/api/stats/flow?project=p&origin=bogus",
        srv.url
    ))
    .call();
    assert!(matches!(r, Err(ureq::Error::Status(400, _))), "{r:?}");
}
