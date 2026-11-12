//! Goals: lifecycle and audit, owner-decision rules, shared trajectory, task
//! and wake linkage, the v15 -> v16 migration, and the CLI over the API.
mod common;

use axe::bot::{self, NewBot};
use axe::db::{self, Actor, Changes, Forbidden, NewTask};
use axe::goal::{self, Edit, NewGoal};
use axe::schedule::{self, NewSchedule};
use rusqlite::Connection;

/// 2027-01-15T08:00:00Z.
const T0: i64 = 1_800_000_000;

fn human() -> Actor {
    Actor::new("human")
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

fn setup() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    for b in ["a", "b"] {
        bot::create(&mut c, &human(), &new_bot(b), T0).unwrap();
    }
    (dir, c)
}

fn new_goal(name: &str) -> NewGoal {
    NewGoal {
        name: name.into(),
        project: "p".into(),
        intent: "ship it\n".into(),
        ..Default::default()
    }
}

fn kinds(c: &Connection, name: &str) -> Vec<String> {
    goal::events(c, name, 1000, false)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

fn forbidden(e: anyhow::Error) -> bool {
    e.downcast_ref::<Forbidden>().is_some()
}

#[test]
fn lifecycle_writes_an_audit_event_per_transition() {
    let (_d, mut c) = setup();
    let g = goal::propose(&mut c, &human(), &new_goal("g"), None, T0).unwrap();
    assert_eq!(g.state, "proposed");
    assert!(g.bots.is_empty(), "a goal with zero bots is allowed");
    for (action, state) in [
        ("activate", "active"),
        ("pause", "paused"),
        ("resume", "active"),
        ("done", "done"),
    ] {
        let g = goal::change(&mut c, &human(), "g", action, "w", None).unwrap();
        assert_eq!(g.state, state, "{action}");
    }
    assert_eq!(
        kinds(&c, "g"),
        ["proposed", "activated", "state", "state", "state"]
    );
    let bodies: Vec<String> = goal::events(&c, "g", 1000, false)
        .unwrap()
        .into_iter()
        .map(|e| e.body)
        .collect();
    assert_eq!(bodies[1], "proposed -> active\nwhy: w");
    assert_eq!(bodies[4], "active -> done\nwhy: w");

    // A finished goal does not move again; repeating its state is a no-op.
    let err = goal::change(&mut c, &human(), "g", "pause", "w", None).unwrap_err();
    assert!(err.to_string().contains("is done"), "{err}");
    goal::change(&mut c, &human(), "g", "done", "w", None).unwrap();
    assert_eq!(kinds(&c, "g").len(), 5);

    goal::propose(&mut c, &human(), &new_goal("h"), None, T0).unwrap();
    let h = goal::change(&mut c, &human(), "h", "drop", "no longer needed", None).unwrap();
    assert_eq!(h.state, "dropped");
    assert_eq!(kinds(&c, "h"), ["proposed", "state"]);

    let err = goal::propose(&mut c, &human(), &new_goal("h"), None, T0).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(goal::list(&c, None, Some("done")).unwrap().len(), 1);
    assert!(goal::get(&c, "missing")
        .unwrap_err()
        .to_string()
        .ends_with("not found"));
}

#[test]
fn owner_decisions_need_an_owner_ref_from_non_humans() {
    let (_d, mut c) = setup();
    for (i, who) in ["bot:a", "agent"].into_iter().enumerate() {
        let actor = Actor::new(who);
        let name = format!("g{i}");
        // Anyone may propose; a bot is attached to its own proposal.
        let g = goal::propose(&mut c, &actor, &new_goal(&name), None, T0).unwrap();
        assert_eq!(g.bots, if who == "bot:a" { vec!["a"] } else { vec![] });

        let err = goal::change(&mut c, &actor, &name, "activate", "w", None).unwrap_err();
        assert!(forbidden(err));
        let edits = [
            Edit {
                intent: Some("new\n".into()),
                ..Default::default()
            },
            Edit {
                budget_cost_week: Some("12.5".into()),
                ..Default::default()
            },
            Edit {
                budget_attention_week: Some("3".into()),
                ..Default::default()
            },
            Edit {
                deadline: Some("2027-03-01".into()),
                ..Default::default()
            },
        ];
        for e in &edits {
            let err = goal::edit(&mut c, &actor, &name, e, "w", None).unwrap_err();
            assert!(forbidden(err), "{e:?}");
            // A blank reference is no reference.
            let err = goal::edit(&mut c, &actor, &name, e, "w", Some("  ")).unwrap_err();
            assert!(forbidden(err), "{e:?}");
        }
        for action in ["join", "leave"] {
            let err = goal::membership(&mut c, &actor, &name, action, "b", "w", None).unwrap_err();
            assert!(forbidden(err), "{action}");
        }
        // Nothing changed before the references were given.
        assert_eq!(goal::get(&c, &name).unwrap().state, "proposed");
        assert_eq!(goal::get(&c, &name).unwrap().intent, "ship it\n");

        let r = Some("msg #873");
        goal::change(&mut c, &actor, &name, "activate", "w", r).unwrap();
        for e in &edits {
            goal::edit(&mut c, &actor, &name, e, "w", r).unwrap();
        }
        goal::membership(&mut c, &actor, &name, "join", "b", "w", r).unwrap();
        let g = goal::get(&c, &name).unwrap();
        assert_eq!(g.state, "active");
        assert_eq!(g.intent, "new\n");
        assert_eq!(g.budget_cost_week, Some(12.5));
        assert_eq!(g.budget_attention_week, Some(3));
        assert_eq!(g.deadline.as_deref(), Some("2027-03-01"));
        assert!(g.bots.contains(&"b".to_string()));
        goal::membership(&mut c, &actor, &name, "leave", "b", "w", r).unwrap();

        let events = goal::events(&c, &name, 1000, false).unwrap();
        let decided: Vec<_> = events
            .iter()
            .filter(|e| ["activated", "intent", "budget", "deadline"].contains(&e.kind.as_str()))
            .collect();
        assert_eq!(decided.len(), 5);
        for e in decided {
            assert_eq!(e.owner_ref.as_deref(), Some("msg #873"), "{e:?}");
            assert_eq!(e.actor, who);
        }
        let intent = events.iter().find(|e| e.kind == "intent").unwrap();
        assert!(
            intent.body.starts_with("why: w\n--- intent"),
            "{}",
            intent.body
        );
        assert!(intent.body.contains("-ship it\n+new"), "{}", intent.body);
    }

    // A bot may join and leave itself without a reference.
    let a = Actor::new("bot:a");
    goal::membership(&mut c, &a, "g1", "join", "a", "w", None).unwrap();
    goal::membership(&mut c, &a, "g1", "leave", "a", "w", None).unwrap();
    assert!(goal::get(&c, "g1").unwrap().bots.is_empty());
    // Attaching another bot at proposal is also an owner decision.
    let mut other = new_goal("g9");
    other.bots = vec!["b".into()];
    assert!(forbidden(
        goal::propose(&mut c, &a, &other, None, T0).unwrap_err()
    ));
    let g = goal::propose(&mut c, &a, &other, Some("plan 2026-11-11"), T0).unwrap();
    assert_eq!(g.bots, ["a", "b"]);

    // Humans need no reference; clearing an optional value uses "none".
    let clear = Edit {
        deadline: Some("none".into()),
        budget_cost_week: Some("none".into()),
        ..Default::default()
    };
    let g = goal::edit(&mut c, &human(), "g0", &clear, "w", None).unwrap();
    assert_eq!(g.deadline, None);
    assert_eq!(g.budget_cost_week, None);
    assert_eq!(g.budget_attention_week, Some(3));
}

#[test]
fn two_bots_share_one_trajectory() {
    let (_d, mut c) = setup();
    let mut g = new_goal("shared");
    g.bots = vec!["b".into()];
    goal::propose(&mut c, &Actor::new("bot:a"), &g, Some("msg #1"), T0).unwrap();
    goal::propose(&mut c, &human(), &new_goal("other"), None, T0).unwrap();
    for b in ["a", "b"] {
        let names: Vec<String> = goal::list(&c, Some(b), None)
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["shared"], "{b}");
    }
    assert_eq!(goal::list(&c, None, None).unwrap().len(), 2);
    assert!(goal::list(&c, Some("missing"), None).is_err());

    let task = db::add_task(
        &mut c,
        &human(),
        &NewTask {
            agent: None,
            project: "p",
            title: "probe",
            detail: "",
            state: "pending",
        },
    )
    .unwrap();
    goal::record(
        &mut c,
        &Actor::new("bot:a"),
        "shared",
        "hypothesis",
        None,
        "H1",
    )
    .unwrap();
    goal::record(
        &mut c,
        &Actor::new("bot:b"),
        "shared",
        "test",
        Some(task),
        "T1",
    )
    .unwrap();
    goal::record(
        &mut c,
        &Actor::new("bot:b"),
        "shared",
        "verdict",
        None,
        "V1",
    )
    .unwrap();
    assert!(goal::record(&mut c, &human(), "shared", "guess", None, "x").is_err());
    assert!(goal::record(&mut c, &human(), "shared", "note", None, " ").is_err());
    assert!(goal::record(&mut c, &human(), "shared", "note", Some(9999), "x").is_err());

    goal::membership(
        &mut c,
        &Actor::new("bot:b"),
        "shared",
        "leave",
        "b",
        "done",
        None,
    )
    .unwrap();
    assert!(goal::list(&c, Some("b"), None).unwrap().is_empty());

    let d = goal::detail(&c, "shared").unwrap();
    assert_eq!(d.goal.bots, ["a"]);
    let traj: Vec<(String, String, Option<i64>)> = d
        .trajectory
        .iter()
        .map(|e| (e.actor.clone(), e.body.clone(), e.task_id))
        .collect();
    assert_eq!(
        traj,
        [
            ("bot:a".to_string(), "H1".to_string(), None),
            ("bot:b".to_string(), "T1".to_string(), Some(task)),
            ("bot:b".to_string(), "V1".to_string(), None),
        ]
    );
    assert_eq!(
        kinds(&c, "shared"),
        [
            "proposed",
            "join",
            "join",
            "hypothesis",
            "test",
            "verdict",
            "leave"
        ]
    );
}

#[test]
fn tasks_link_through_goal_events_and_latest_wins() {
    let (_d, mut c) = setup();
    for g in ["g", "h"] {
        goal::propose(&mut c, &human(), &new_goal(g), None, T0).unwrap();
    }
    let t = |title| NewTask {
        agent: None,
        project: "p",
        title,
        detail: "",
        state: "pending",
    };
    let id = db::add_linked_task(&mut c, &human(), &t("one"), &[], None, Some("g")).unwrap();
    let err = db::add_linked_task(&mut c, &human(), &t("bad"), &[], None, Some("nope"));
    assert!(err.unwrap_err().to_string().contains("goal nope not found"));
    assert_eq!(db::list_tasks(&c, &Default::default()).unwrap().len(), 1);

    let set = |c: &mut Connection, goal: &str| {
        db::set_task(
            c,
            &human(),
            id,
            &Changes {
                goal: Some(goal.into()),
                ..Default::default()
            },
        )
    };
    assert_eq!(db::current_goal(&c, id).unwrap().as_deref(), Some("g"));
    assert!(set(&mut c, "h").unwrap());
    assert!(!set(&mut c, "h").unwrap(), "the same goal is no change");
    assert!(set(&mut c, "nope")
        .unwrap_err()
        .to_string()
        .contains("not found"));
    let ids = |c: &Connection, g| -> Vec<i64> {
        goal::tasks(c, g)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect()
    };
    assert_eq!(ids(&c, "g"), Vec::<i64>::new());
    assert_eq!(ids(&c, "h"), [id]);
    assert!(set(&mut c, "none").unwrap());
    assert!(ids(&c, "h").is_empty());
    assert_eq!(db::current_goal(&c, id).unwrap(), None);

    let events: Vec<(Option<String>, Option<String>)> = db::events(&c, id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "goal")
        .map(|e| (e.from_value, e.to_value))
        .collect();
    assert_eq!(
        events,
        [
            (None, Some("g".into())),
            (Some("g".into()), Some("h".into())),
            (Some("h".into()), None),
        ]
    );
}

#[test]
fn schedule_wakes_carry_the_goal_label() {
    let (_d, mut c) = setup();
    goal::propose(&mut c, &human(), &new_goal("g"), None, T0).unwrap();
    // Consume the "bot created" wake.
    let tx = c.transaction().unwrap();
    let run = bot::claim(&tx, "a", "bot:1", "d", "g", T0)
        .unwrap()
        .unwrap()
        .run;
    tx.commit().unwrap();
    bot::finish(&c, run, Some(0), "exit", Some("s"), None, T0 + 1).unwrap();

    let s = NewSchedule {
        name: "probe".into(),
        cron: "0 9 * * *".into(),
        timezone: "UTC".into(),
        objective: "check the metric".into(),
    };
    let err = schedule::add_for_goal(&mut c, &human(), "a", &s, Some("nope"), "w", T0);
    assert!(err.unwrap_err().to_string().contains("not found"));
    let added = schedule::add_for_goal(&mut c, &human(), "a", &s, Some("g"), "w", T0).unwrap();
    assert_eq!(added.goal.as_deref(), Some("g"));
    assert_eq!(schedule::tick(&mut c, T0 + 3600).unwrap(), 1);
    let w = bot::wakes(&c, "a", true, 10).unwrap();
    assert_eq!(w.len(), 1);
    let line = bot::wake_line(&w[0]);
    assert!(
        line.ends_with(
            "] [goal g] scheduled wake (automation, not an owner message). Objective: check the metric"
        ),
        "{line}"
    );

    // Edit clears the goal; the next firing is unlabeled.
    let edited = schedule::set_goal(&mut c, &human(), "a", "probe", "none", "w", T0).unwrap();
    assert_eq!(edited.goal, None);
    let edits: Vec<String> = bot::events(&c, "a", 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "schedule_edit")
        .map(|e| e.body)
        .collect();
    assert_eq!(edits.len(), 1);
    assert!(edits[0].contains(r#""from":"g""#), "{}", edits[0]);
    let again = schedule::set_goal(&mut c, &human(), "a", "probe", "g", "w", T0).unwrap();
    assert_eq!(again.goal.as_deref(), Some("g"));
    assert_eq!(
        schedule::list(&c, "a").unwrap()[0].goal.as_deref(),
        Some("g")
    );
}

#[test]
fn v15_database_migrates_to_v16_additively() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v15.db");
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch(include_str!("fixtures/schema-v15.sql"))
            .unwrap();
        c.execute_batch(
            "INSERT INTO tasks (project, title, detail, state, created_at, updated_at)
               VALUES ('p', 't', '', 'pending', '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z');
             INSERT INTO bots (name, project, agent, state, created_at, updated_at)
               VALUES ('a', 'p', 'claude', 'active', '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z');
             INSERT INTO bot_schedules (bot_id, name, cron, timezone, objective, enabled, next_at,
               created_at, updated_at)
               VALUES (1, 's', '0 9 * * *', 'UTC', 'o', 1, '2026-11-02T09:00:00Z',
               '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z');
             PRAGMA user_version = 15;",
        )
        .unwrap();
    }
    let mut c = db::open(&path).unwrap();
    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 16);
    assert_eq!(db::get_task(&c, 1).unwrap().title, "t");
    let s = &schedule::list(&c, "a").unwrap()[0];
    assert_eq!((s.name.as_str(), s.goal.as_deref()), ("s", None));
    goal::propose(&mut c, &human(), &new_goal("g"), None, T0).unwrap();
    schedule::set_goal(&mut c, &human(), "a", "s", "g", "w", T0).unwrap();
    let fk: i64 = c
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk, 0);

    // A compatibility downgrade (user_version lowered to 15) keeps every goal
    // row and the column, and upgrades again in place.
    c.pragma_update(None, "user_version", 15).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(goal::list(&c, None, None).unwrap().len(), 1);
    assert_eq!(
        schedule::list(&c, "a").unwrap()[0].goal.as_deref(),
        Some("g")
    );
}

#[test]
fn cli_drives_goals_through_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let srv = common::Server::start(dir.path());
    let bot_file = dir.path().join("bot.json");
    for b in ["a", "b"] {
        std::fs::write(&bot_file, format!(r#"{{"name":"{b}","project":"p"}}"#)).unwrap();
        srv.ok(&["bot", "create", "--file", bot_file.to_str().unwrap()]);
    }
    let goal_file = dir.path().join("goal.json");
    std::fs::write(
        &goal_file,
        r#"{"name":"g","project":"p","intent":"grow\n","budget_cost_week":20,"bots":["b"]}"#,
    )
    .unwrap();
    let as_bot = |args: &[&str]| {
        let mut c = srv.cli(args);
        c.env("AXE_ACTOR", "bot:a").env("AXE_BOT", "a");
        c.output().unwrap()
    };
    let gf = goal_file.to_str().unwrap();
    let out = as_bot(&["goal", "propose", "--file", gf]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--owner-ref"),
        "{out:?}"
    );
    let out = as_bot(&["goal", "propose", "--file", gf, "--owner-ref", "msg #1"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("g  proposed  p  bots=a,b"));

    let out = as_bot(&["goal", "activate", "g", "--why", "go"]);
    assert!(!out.status.success(), "{out:?}");
    srv.ok(&["goal", "activate", "g", "--why", "go"]);
    let out = as_bot(&["goal", "record", "g", "--kind", "hypothesis", "H1"]);
    assert!(out.status.success(), "{out:?}");

    let id = srv.ok(&["add", "--project", "p", "--title", "t", "--goal", "g"]);
    let id = id.trim();
    let out = srv.run(&["add", "--project", "p", "--title", "t", "--goal", "nope"]);
    assert!(!out.status.success());
    srv.ok(&["goal", "record", "g", "--kind", "test", "--task", id, "T1"]);

    let show: serde_json::Value =
        serde_json::from_str(&srv.ok(&["goal", "show", "g", "--json"])).unwrap();
    assert_eq!(show["state"], "active");
    assert_eq!(show["bots"], serde_json::json!(["a", "b"]));
    assert_eq!(show["budget_cost_week"], 20.0);
    assert_eq!(show["tasks"][0]["id"].to_string(), id);
    assert_eq!(show["trajectory"][1]["task_id"].to_string(), id);
    assert_eq!(show["next_wake"], serde_json::Value::Null);
    let text = srv.ok(&["goal", "show", "g"]);
    assert!(text.contains("# Trajectory"), "{text}");
    assert!(text.contains(&format!("#{id} pending t")), "{text}");
    let listed = srv.ok(&["goal", "list", "--bot", "b", "--state", "active"]);
    assert!(listed.starts_with("g  active"), "{listed}");

    srv.ok(&["set", id, "--goal", "none"]);
    let show = srv.get("/api/goals/g");
    assert_eq!(show["tasks"], serde_json::json!([]));
    let log = srv.ok(&["log", id]);
    assert!(log.lines().any(|l| l.ends_with(" human goal g")), "{log}");
    assert!(log.lines().any(|l| l.ends_with(" human goal")), "{log}");

    srv.ok(&[
        "bot",
        "schedule",
        "--bot",
        "a",
        "add",
        "probe",
        "--cron",
        "0 9 * * *",
        "--objective",
        "check",
        "--goal",
        "g",
        "--why",
        "w",
    ]);
    let out = srv.ok(&[
        "bot", "schedule", "--bot", "a", "edit", "probe", "--goal", "none", "--why", "w",
    ]);
    assert!(!out.contains("[goal"), "{out}");
    srv.ok(&[
        "bot",
        "wake",
        "a",
        "--now",
        "--note",
        "look at it",
        "--goal",
        "g",
    ]);
    let wakes = srv.get("/api/bots/a/wakes?undelivered=1");
    let bodies: Vec<&str> = wakes
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|w| w["body"].as_str())
        .collect();
    assert!(bodies.contains(&"[goal g] look at it"), "{wakes}");
    let due = wakes
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["body"] == "[goal g] look at it")
        .unwrap()["due_at"]
        .clone();
    assert_eq!(srv.get("/api/goals/g")["next_wake"], due);
    let out = srv.run(&["bot", "wake", "a", "--now", "--note", "x", "--goal", "nope"]);
    assert!(!out.status.success());

    let log = srv.ok(&["goal", "log", "g"]);
    assert!(log.contains("activated: proposed -> active"), "{log}");
    assert!(log.contains(r#"owner_ref="msg #1""#), "{log}");
}
