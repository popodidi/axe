mod common;

use axe::db::{self, Actor, Changes, Claim, NewTask};
use common::Server;

fn add(conn: &mut rusqlite::Connection, project: &str) -> i64 {
    db::add_task(
        conn,
        &Actor::new("ops"),
        &NewTask {
            agent: None,
            project,
            title: "t",
            detail: "",
            state: "pending",
        },
    )
    .unwrap()
}

fn set(conn: &mut rusqlite::Connection, id: i64, c: Changes) -> anyhow::Result<bool> {
    db::set_task(conn, &Actor::new("agent"), id, &c)
}

fn claim(conn: &mut rusqlite::Connection) -> Option<i64> {
    let tx = conn.transaction().unwrap();
    let c = db::claim_in(&tx, &Actor::new("dispatcher"), None, "d1", None).unwrap();
    tx.commit().unwrap();
    match c {
        Claim::Claimed(t) => Some(t.id),
        _ => None,
    }
}

#[test]
fn sleeping_task_is_not_claimable_until_its_wake_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let id = add(&mut conn, "a");
    set(
        &mut conn,
        id,
        Changes {
            not_before: Some("+1h".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let t = db::get_task(&conn, id).unwrap();
    assert!(t.sleeping() && !t.ready());
    assert_eq!(t.status, "blocked");
    assert_eq!(db::pending_projects(&conn).unwrap(), Vec::<String>::new());
    assert_eq!(claim(&mut conn), None);

    set(
        &mut conn,
        id,
        Changes {
            not_before: Some("2000-01-01T00:00:00Z".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(db::get_task(&conn, id).unwrap().ready());
    assert_eq!(claim(&mut conn), Some(id));
    // A claim clears the passed wake time.
    assert_eq!(db::get_task(&conn, id).unwrap().not_before, None);
}

#[test]
fn state_change_clears_wake_time_and_logs_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let id = add(&mut conn, "a");
    set(
        &mut conn,
        id,
        Changes {
            not_before: Some("2099-01-01T00:00Z".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db::get_task(&conn, id).unwrap().not_before.as_deref(),
        Some("2099-01-01T00:00:00Z")
    );
    set(
        &mut conn,
        id,
        Changes {
            state: Some("escalated".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(db::get_task(&conn, id).unwrap().not_before, None);
    let wakes: Vec<(Option<String>, Option<String>)> = db::events(&conn, id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "wake")
        .map(|e| (e.from_value, e.to_value))
        .collect();
    assert_eq!(
        wakes,
        [
            (None, Some("2099-01-01T00:00:00Z".into())),
            (Some("2099-01-01T00:00:00Z".into()), None)
        ]
    );
}

#[test]
fn wake_time_requires_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let id = add(&mut conn, "a");
    let err = set(
        &mut conn,
        id,
        Changes {
            state: Some("escalated".into()),
            not_before: Some("+1h".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("pending"), "{err}");
}

#[test]
fn parse_wake_accepts_rfc3339_and_relative() {
    assert_eq!(
        db::parse_wake("2026-10-29T22:00:00+02:00").unwrap(),
        "2026-10-29T20:00:00Z"
    );
    assert_eq!(
        db::parse_wake("2026-10-29T20:00Z").unwrap(),
        "2026-10-29T20:00:00Z"
    );
    let in_90m = db::parse_wake("+90m").unwrap();
    assert_eq!(in_90m.len(), "2026-10-29T20:00:00Z".len());
    assert!(in_90m > db::parse_wake("+89m").unwrap());
    for bad in ["+0h", "+5x", "+h", "tomorrow"] {
        assert!(db::parse_wake(bad).is_err(), "{bad}");
    }
}

#[test]
fn pre_v8_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (id INTEGER PRIMARY KEY, project TEXT NOT NULL);
             PRAGMA user_version = 6;",
        )
        .unwrap();
    }
    let err = db::open(&path).unwrap_err();
    assert!(err.to_string().contains("too old"), "{err}");
}

#[test]
fn cli_verbs_enforce_the_worker_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let id = srv
        .ok(&[
            "add",
            "--project",
            "orchid",
            "--title",
            "t",
            "--detail",
            "x",
        ])
        .trim()
        .to_string();
    let other = srv
        .ok(&[
            "add",
            "--project",
            "orchid",
            "--title",
            "p",
            "--detail",
            "x",
        ])
        .trim()
        .to_string();
    let show = |id: &str| srv.get(&format!("/api/tasks/{id}"));

    assert!(!srv
        .run(&["handoff", &id, "verifier", "--note", "PR ready"])
        .status
        .success());

    // Escalate refuses an incomplete block note and writes the three-line format.
    let out = srv.run(&[
        "escalate",
        &id,
        "--blocked",
        "x",
        "--resolver",
        " ",
        "--resume",
        "y",
        "--class",
        "decision",
    ]);
    assert!(!out.status.success());
    srv.ok(&[
        "escalate",
        &id,
        "--blocked",
        "needs a decision",
        "--resolver",
        "owner: pick A or B",
        "--resume",
        "decision recorded",
        "--class",
        "decision",
    ]);
    let t = show(&id);
    assert_eq!(t["state"].as_str(), Some("escalated"));
    assert!(t["owner"].is_null());
    let log = srv.ok(&["log", &id, "--full"]);
    assert!(log.contains(
        "blocked: needs a decision\nresolver: owner: pick A or B\nresume: decision recorded"
    ));

    // Wait on another task: pending with an edge, shown as a dependency wait.
    srv.ok(&["wait", &id, "--on", &other, "--note", "needs #p first"]);
    let t = show(&id);
    assert_eq!(t["state"].as_str(), Some("pending"));
    assert_eq!(t["waiting_on"][0].as_i64(), other.parse().ok());

    // Wait for a duration: pending, asleep, listed as waiting until <time>.
    srv.ok(&["deps", &id, "--remove", &other]);
    srv.ok(&["wait", &id, "--for", "2h", "--note", "check rollout"]);
    let t = show(&id);
    assert_eq!(t["state"].as_str(), Some("pending"));
    assert!(t["not_before"].as_str().is_some());
    assert!(srv.ok(&["list"]).contains("waiting until "));
    assert!(srv.ok(&["show", &id]).contains("not_before: "));

    // --on and --until are exclusive, and one wait kind is required.
    assert!(!srv.run(&["wait", &id, "--note", "x"]).status.success());
    assert!(!srv
        .run(&["wait", &id, "--on", &other, "--until", "+1h", "--note", "x"])
        .status
        .success());
}
