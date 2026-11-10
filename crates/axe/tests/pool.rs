//! Admission policy: one pool of `[pool] total` concurrent agents, filled with
//! the oldest ready task first.

use std::time::{Duration, Instant};

use axe::config::Config;
use axe::db::{self, Actor, NewTask};
mod common;
use common::{Server, TOKEN};

#[test]
fn pool_total_defaults_to_one_and_must_be_positive() {
    assert_eq!(Config::parse("").unwrap().pool_total(), 1);
    assert_eq!(Config::parse("[pool]\ntotal = 4").unwrap().pool_total(), 4);
    let err = Config::parse("[pool]\ntotal = 0").unwrap_err();
    assert!(
        format!("{err:#}").contains("pool.total must be >= 1"),
        "{err:#}"
    );
}

// ---- dispatcher integration -------------------------------------------------

struct Env {
    dir: tempfile::TempDir,
    db: std::path::PathBuf,
}

fn env(total: usize, agents: &str, rules: &str) -> (Env, Config) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    for p in ["a", "b", "quick"] {
        std::fs::create_dir_all(ws.join(p)).unwrap();
    }
    let cfg = Config::parse(&format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[defaults]
agents = ["run"]
[pool]
total = {total}
{agents}
{rules}
"#,
        ws = ws.display(),
        logs = dir.path().join("logs").display(),
    ))
    .unwrap();
    let db = dir.path().join("t.db");
    (Env { dir, db }, cfg)
}

fn add(conn: &mut rusqlite::Connection, project: &str, state: &str) -> i64 {
    db::add_task(
        conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project,
            title: "t",
            detail: "",
            state,
        },
    )
    .unwrap()
}

/// The task of every run_start, in admission order.
fn admissions(conn: &rusqlite::Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT task_id FROM events WHERE kind = 'run_start' ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn dispatcher_admits_oldest_ready_tasks_up_to_pool_total() {
    let (env, cfg) = env(3, "[agents.run]\ncmd = [\"sh\", \"-c\", \"sleep 1\"]", "");
    let mut conn = db::open(&env.db).unwrap();
    let a0 = add(&mut conn, "a", "pending");
    // Ineligible work: a hold, and a task waiting on a prerequisite.
    let held = add(&mut conn, "b", "blocked");
    let waiting = db::add_task_with_deps(
        &mut conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project: "a",
            title: "waits",
            detail: "",
            state: "pending",
        },
        &[a0],
    )
    .unwrap();
    let b0 = add(&mut conn, "b", "pending");
    let a1 = add(&mut conn, "a", "pending");
    let b1 = add(&mut conn, "b", "pending");
    let a2 = add(&mut conn, "a", "pending");

    let config = env.dir.path().join("config.toml");
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let srv = Server::start_with(env.dir.path(), &config, Some(TOKEN), &[], &[]);
    let out = srv.dispatcher(&["--once"], &[]).wait();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr
            .lines()
            .any(|l| l == "axe dispatcher: pool total=3 bots=one per active bot"),
        "{stderr}"
    );

    // One pass spawned three concurrent agents, oldest ready first across projects.
    assert_eq!(admissions(&conn), [a0, b0, a1]);
    for id in [held, waiting, b1, a2] {
        assert_eq!(db::get_task(&conn, id).unwrap().runs, 0, "#{id}");
    }
    assert_eq!(db::get_task(&conn, held).unwrap().state, "blocked");
    assert_eq!(db::get_task(&conn, waiting).unwrap().state, "pending");
    drop(env.dir);
}

#[test]
fn live_dispatcher_refills_a_freed_slot_with_the_oldest_ready_task() {
    let (env, cfg) = env(
        3,
        r#"
[agents.run]
cmd = ["sh", "-c", "sleep 60"]
[agents.quick]
cmd = ["sh", "-c", "while [ ! -e ../go ]; do sleep 0.1; done"]
"#,
        r#"
[[rule]]
match = { project = "quick" }
agents = ["quick"]
"#,
    );
    let mut conn = db::open(&env.db).unwrap();
    let a0 = add(&mut conn, "a", "pending");
    let q0 = add(&mut conn, "quick", "pending");
    // Older than the queued work below, but on hold until it is released.
    let later = add(&mut conn, "a", "blocked");
    let a1 = add(&mut conn, "a", "pending");
    let a2 = add(&mut conn, "a", "pending");
    let a3 = add(&mut conn, "a", "pending");

    let config = env.dir.path().join("config.toml");
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let srv = Server::start_with(env.dir.path(), &config, Some(TOKEN), &[], &[]);
    let mut worker = srv.dispatcher(&[], &[]);
    let wait_for = |what: &str, f: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let state = |id: i64| db::get_task(&db::open(&env.db).unwrap(), id).unwrap();
    wait_for("pool full", &|| {
        admissions(&db::open(&env.db).unwrap()).len() == 3
    });
    assert_eq!(admissions(&conn), [a0, q0, a1]);
    let claims: Vec<Option<String>> = [a0, a1].iter().map(|id| state(*id).claimed_by).collect();

    // A task becomes ready while the pool is full: it waits for a free slot.
    db::set_task(
        &mut conn,
        &Actor::new("human"),
        later,
        &db::Changes {
            state: Some("pending".into()),
            ..Default::default()
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(2500)); // two polls
    assert_eq!(admissions(&conn).len(), 3);
    assert_eq!(state(later).state, "pending");

    // The quick session finishes; the oldest ready task takes its slot ahead of
    // newer queued work.
    std::fs::write(env.dir.path().join("ws/go"), "").unwrap();
    wait_for("refill", &|| {
        admissions(&db::open(&env.db).unwrap()).len() == 4
    });
    assert_eq!(admissions(&conn)[3..], [later]);
    std::thread::sleep(Duration::from_millis(2500));
    for id in [a2, a3] {
        assert_eq!(
            (state(id).state.as_str(), state(id).runs),
            ("pending", 0),
            "#{id}"
        );
    }
    // Running agents were never preempted: same claims, still in progress.
    let still: Vec<Option<String>> = [a0, a1].iter().map(|id| state(*id).claimed_by).collect();
    assert_eq!(still, claims);
    for id in [a0, a1, later] {
        assert_eq!(state(id).state, "in_progress", "#{id}");
    }
    assert_eq!(admissions(&conn).len(), 4);

    std::process::Command::new("kill")
        .args(["-TERM", &worker.0.id().to_string()])
        .status()
        .unwrap();
    assert!(worker.0.wait().unwrap().success());
}
