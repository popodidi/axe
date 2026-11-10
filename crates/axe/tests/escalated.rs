mod common;
use axe::db::{self, Actor, Changes, Claim, NewTask};

fn add(c: &mut rusqlite::Connection, state: &str, deps: &[i64]) -> i64 {
    db::add_task_with_deps(
        c,
        &Actor::new("ops"),
        &NewTask {
            agent: None,
            project: "p",
            title: "t",
            detail: "",
            state,
        },
        deps,
    )
    .unwrap()
}
fn state(c: &mut rusqlite::Connection, id: i64, s: &str) {
    db::set_task(
        c,
        &Actor::new("ops"),
        id,
        &Changes {
            state: Some(s.into()),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn escalation_survives_dependencies_restart_and_budget_reset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut c = db::open(&path).unwrap();
    let p = add(&mut c, "blocked", &[]);
    let wait = add(&mut c, "pending", &[p]);
    let hold = add(&mut c, "escalated", &[p]);
    assert_eq!(db::get_task(&c, wait).unwrap().status, "blocked");
    state(&mut c, p, "done");
    assert_eq!(db::get_task(&c, wait).unwrap().status, "pending");
    // Finish the dependency wait so only `hold` is left to claim.
    state(&mut c, wait, "done");
    assert_eq!(db::get_task(&c, hold).unwrap().status, "escalated");
    drop(c);
    let mut c = db::open(&path).unwrap();
    db::set_task(
        &mut c,
        &Actor::new("ops"),
        hold,
        &Changes {
            reset_runs: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        db::claim(&mut c, &Actor::new("dispatcher"), None, "x", None).unwrap(),
        Claim::None
    ));
    state(&mut c, p, "blocked");
    let dispatched = Actor {
        name: "human".into(),
        session: Some("dispatch:x".into()),
    };
    for actor in [Actor::new("agent"), Actor::new("dispatcher"), dispatched] {
        assert!(db::approve(&mut c, &actor, hold, "abc", "pr-merge", None).is_err());
    }
    assert!(db::approve(&mut c, &Actor::new("ops"), hold, "", "pr-merge", None).is_err());
    assert!(db::events(&c, hold)
        .unwrap()
        .iter()
        .all(|e| e.kind != "approval"));
    db::approve(&mut c, &Actor::new("ops"), hold, "abc", "pr-merge", None).unwrap();
    let t = db::get_task(&c, hold).unwrap();
    assert_eq!(
        (t.state.as_str(), t.status.as_str()),
        ("pending", "blocked")
    );
    assert!(matches!(
        db::claim(&mut c, &Actor::new("dispatcher"), None, "x", None).unwrap(),
        Claim::None
    ));
    state(&mut c, p, "done");
    assert!(matches!(
        db::claim(&mut c, &Actor::new("dispatcher"), None, "x", None).unwrap(),
        Claim::Claimed(_)
    ));
}

#[test]
fn cli_api_filters_attention_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let s = common::Server::start_with(dir.path(), &config, Some(common::TOKEN), &[], &[]);
    for title in ["esc-a", "esc-b", "esc-c"] {
        s.ok(&[
            "add",
            "--project",
            "p",
            "--title",
            title,
            "--state",
            "escalated",
        ]);
    }
    s.ok(&[
        "add",
        "--project",
        "p",
        "--title",
        "external",
        "--state",
        "blocked",
    ]);
    assert!(s.ok(&["list", "--state", "escalated"]).contains("esc-c"));
    let tasks = s.get("/api/tasks?state=escalated");
    assert_eq!(tasks.as_array().unwrap().len(), 3);
    let ov = s.get("/api/overview");
    assert_eq!(
        ov["ops_queue"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["actionable"] == true)
            .count(),
        3
    );
    s.ok(&["set", "1", "--state", "pending"]);
    assert_eq!(
        s.get("/api/tasks?state=escalated")
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!s.run(&["set", "2", "--state", "invalid"]).status.success());
}
