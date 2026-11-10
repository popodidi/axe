//! Task dependencies: `blocked_by` edges gate claiming. Dependency waits are
//! derived (stored pending + an unfinished prerequisite), so they never replace
//! or clear a stored hold (`blocked`).

use axe::db::{self, Actor, Changes, Claim, NewTask};
use rusqlite::Connection;

fn open(dir: &tempfile::TempDir) -> Connection {
    db::open(&dir.path().join("t.db")).unwrap()
}

fn task(project: &str, state: &str) -> NewTask<'static> {
    NewTask {
        agent: None,
        project: Box::leak(project.to_string().into_boxed_str()),
        title: "t",
        detail: "",
        state: Box::leak(state.to_string().into_boxed_str()),
    }
}

fn add(conn: &mut Connection, deps: &[i64]) -> i64 {
    add_as(conn, "p", "pending", deps)
}

fn add_as(conn: &mut Connection, project: &str, state: &str, deps: &[i64]) -> i64 {
    db::add_task_with_deps(conn, &Actor::new("ops"), &task(project, state), deps).unwrap()
}

fn set_state(conn: &mut Connection, id: i64, state: &str) {
    let c = Changes {
        state: Some(state.into()),
        ..Default::default()
    };
    db::set_task(conn, &Actor::new("human"), id, &c).unwrap();
}

fn claim(conn: &mut Connection) -> Option<i64> {
    match db::claim(conn, &Actor::new("dispatcher"), None, "c", None).unwrap() {
        Claim::Claimed(t) => Some(t.id),
        Claim::Blocked(id) => panic!("unexpected max_runs block of {id}"),
        Claim::None => None,
    }
}

fn get(conn: &Connection, id: i64) -> db::Task {
    db::get_task(conn, id).unwrap()
}

fn ids(refs: &[db::DepRef]) -> Vec<i64> {
    refs.iter().map(|d| d.id).collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// kind, actor, session, to_value, body
type Audit = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn err(r: anyhow::Result<impl std::fmt::Debug>) -> String {
    format!("{:#}", r.unwrap_err())
}

#[test]
fn create_with_deps_is_atomic_and_dedups() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let b = add_as(&mut conn, "other", "pending", &[]);
    let c = add(&mut conn, &[a, b, a]);
    let t = get(&conn, c);
    assert_eq!(ids(&t.deps.blocked_by), [a, b]);
    assert_eq!(t.waiting_on, [a, b]);
    assert_eq!(
        (t.state.as_str(), t.status.as_str()),
        ("pending", "blocked")
    );
    assert!(!t.ready());

    // One invalid prerequisite rolls back the task and every edge.
    let tasks = count(&conn, "SELECT COUNT(*) FROM tasks");
    let edges = count(&conn, "SELECT COUNT(*) FROM deps");
    let e = err(db::add_task_with_deps(
        &mut conn,
        &Actor::new("ops"),
        &task("p", "pending"),
        &[a, 999],
    ));
    assert!(e.contains("prerequisite task 999 does not exist"), "{e}");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM tasks"), tasks);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM deps"), edges);

    // Creating in_progress with an unfinished prerequisite is refused.
    let e = err(db::add_task_with_deps(
        &mut conn,
        &Actor::new("ops"),
        &task("p", "in_progress"),
        &[a],
    ));
    assert!(e.contains("refusing to add unfinished prerequisite"), "{e}");
}

#[test]
fn add_remove_reverse_queries_cross_project_and_audit() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add_as(&mut conn, "orchid", "pending", &[]);
    let b = add_as(&mut conn, "fern", "pending", &[]);
    let c = add_as(&mut conn, "ivy-ops", "pending", &[]);
    let ops = Actor {
        name: "ops".into(),
        session: Some("s-1".into()),
    };

    assert!(db::edit_deps(&mut conn, &ops, c, &[a, b], &[]).unwrap());
    assert_eq!(ids(&get(&conn, c).deps.blocked_by), [a, b]);
    assert_eq!(ids(&get(&conn, a).deps.blocks), [c]);
    assert_eq!(ids(&get(&conn, b).deps.blocks), [c]);
    let dep = &get(&conn, a).deps.blocks[0];
    assert_eq!(
        (dep.project.as_str(), dep.status.as_str()),
        ("ivy-ops", "blocked")
    );

    // Duplicates are explicit no-ops: no change, no event.
    let events = count(&conn, "SELECT COUNT(*) FROM events");
    assert!(!db::edit_deps(&mut conn, &ops, c, &[a], &[]).unwrap());
    assert!(!db::edit_deps(&mut conn, &ops, a, &[], &[b]).unwrap());
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM events"), events);

    // Remove and add in one transaction.
    let d = add_as(&mut conn, "fern", "done", &[]);
    assert!(db::edit_deps(&mut conn, &ops, c, &[d], &[a]).unwrap());
    assert_eq!(ids(&get(&conn, c).deps.blocked_by), [b, d]);
    assert!(get(&conn, a).deps.blocks.is_empty());
    assert_eq!(get(&conn, c).waiting_on, [b]);

    // Edge changes are audited on both tasks with actor and session.
    let audit = |id: i64| -> Vec<Audit> {
        db::events(&conn, id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind.starts_with("dep_"))
            .map(|e| (e.kind, e.actor, e.session, e.to_value, e.body))
            .collect()
    };
    let s = Some("s-1".to_string());
    let some = |v: &str| Some(v.to_string());
    assert_eq!(
        audit(c),
        [
            (
                "dep_add".into(),
                "ops".into(),
                s.clone(),
                some(&a.to_string()),
                some(&format!("blocked_by #{a}"))
            ),
            (
                "dep_add".into(),
                "ops".into(),
                s.clone(),
                some(&b.to_string()),
                some(&format!("blocked_by #{b}"))
            ),
            (
                "dep_remove".into(),
                "ops".into(),
                s.clone(),
                some(&a.to_string()),
                some(&format!("blocked_by #{a}"))
            ),
            (
                "dep_add".into(),
                "ops".into(),
                s.clone(),
                some(&d.to_string()),
                some(&format!("blocked_by #{d}"))
            ),
        ]
    );
    assert_eq!(
        audit(a),
        [
            (
                "dep_add".into(),
                "ops".into(),
                s.clone(),
                some(&c.to_string()),
                some(&format!("blocks #{c}"))
            ),
            (
                "dep_remove".into(),
                "ops".into(),
                s.clone(),
                some(&c.to_string()),
                some(&format!("blocks #{c}"))
            ),
        ]
    );
}

#[test]
fn rejects_invalid_ids_self_edges_and_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let ops = Actor::new("ops");
    let a = add_as(&mut conn, "orchid", "pending", &[]);
    let b = add_as(&mut conn, "fern", "pending", &[a]);
    let c = add_as(&mut conn, "moss", "pending", &[b]);
    let edges = count(&conn, "SELECT COUNT(*) FROM deps");

    let e = err(db::edit_deps(&mut conn, &ops, 999, &[a], &[]));
    assert!(e.contains("task 999 not found"), "{e}");
    let e = err(db::edit_deps(&mut conn, &ops, a, &[999], &[]));
    assert!(e.contains("prerequisite task 999 does not exist"), "{e}");
    let e = err(db::edit_deps(&mut conn, &ops, a, &[], &[999]));
    assert!(e.contains("prerequisite task 999 does not exist"), "{e}");
    let e = err(db::edit_deps(&mut conn, &ops, a, &[a], &[]));
    assert!(e.contains("cannot be blocked_by itself"), "{e}");
    let e = err(db::edit_deps(&mut conn, &ops, a, &[b], &[b]));
    assert!(e.contains("both added and removed"), "{e}");
    // Direct and transitive cycles, across projects.
    let e = err(db::edit_deps(&mut conn, &ops, a, &[b], &[]));
    assert!(e.contains("dependency cycle"), "{e}");
    let e = err(db::edit_deps(&mut conn, &ops, a, &[c], &[]));
    assert!(e.contains("dependency cycle"), "{e}");
    // A failing edit in a batch rolls back the valid part too.
    let d = add(&mut conn, &[]);
    let e = err(db::edit_deps(&mut conn, &ops, a, &[d, c], &[]));
    assert!(e.contains("dependency cycle"), "{e}");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM deps"), edges);
    // The database also refuses self edges.
    assert!(conn
        .execute(
            "INSERT INTO deps (task_id, prereq_id, created_at) VALUES (?1, ?1, 'x')",
            [a]
        )
        .is_err());
}

#[test]
fn fan_in_waits_for_every_prerequisite() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let b = add(&mut conn, &[]);
    let c = add(&mut conn, &[a, b]);
    assert_eq!(claim(&mut conn), Some(a));
    assert_eq!(claim(&mut conn), Some(b));
    assert_eq!(claim(&mut conn), None);
    set_state(&mut conn, a, "done");
    assert_eq!(get(&conn, c).waiting_on, [b]);
    assert_eq!(claim(&mut conn), None);
    set_state(&mut conn, b, "done");
    assert!(get(&conn, c).ready());
    assert_eq!(claim(&mut conn), Some(c));
    assert_eq!(get(&conn, c).runs, 1);
}

#[test]
fn fan_out_and_transitive_waits() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let b = add_as(&mut conn, "p", "pending", &[a]);
    let c = add(&mut conn, &[a]);
    let d = add(&mut conn, &[c]);
    assert_eq!(claim(&mut conn), Some(a));
    assert_eq!(claim(&mut conn), None);
    set_state(&mut conn, a, "done");
    // Fan-out: both dependents become ready; the transitive one still waits.
    assert_eq!(claim(&mut conn), Some(b));
    assert_eq!(claim(&mut conn), Some(c));
    assert_eq!(get(&conn, d).waiting_on, [c]);
    assert_eq!(claim(&mut conn), None);
    set_state(&mut conn, c, "done");
    assert_eq!(claim(&mut conn), Some(d));
}

#[test]
fn waiting_tasks_use_no_run_budget_and_do_not_starve_ready_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add_as(&mut conn, "x", "blocked", &[]);
    // The oldest tasks wait; newer ready work in another project runs.
    let w1 = add_as(&mut conn, "p", "pending", &[a]);
    let w2 = add_as(&mut conn, "q", "pending", &[a]);
    let ready = add_as(&mut conn, "q", "pending", &[]);
    assert_eq!(db::pending_projects(&conn).unwrap(), ["q"]);
    // max_runs 0 would block any claimed task; waiting ones are never touched.
    let r = db::claim(&mut conn, &Actor::new("d"), None, "c", Some(0)).unwrap();
    assert!(matches!(r, Claim::Blocked(id) if id == ready), "{r:?}");
    for w in [w1, w2] {
        let t = get(&conn, w);
        assert_eq!((t.state.as_str(), t.runs), ("pending", 0));
        assert!(db::events(&conn, w)
            .unwrap()
            .iter()
            .all(|e| e.kind != "claimed" && e.kind != "state"));
    }
    assert!(db::pending_projects(&conn).unwrap().is_empty());
    assert_eq!(claim(&mut conn), None);
}

#[test]
fn dependency_completion_and_removal_never_clear_other_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let b = add(&mut conn, &[]);
    // Manual hold plus a dependency.
    let manual = add_as(&mut conn, "p", "blocked", &[a]);
    // Approval hold (blocked awaiting approval) plus a dependency.
    let approval = add_as(&mut conn, "p", "blocked", &[a]);
    // Failure hold: a claimed run that the dispatcher blocked, then a new prerequisite.
    let failed = add_as(&mut conn, "p", "pending", &[]);
    assert_eq!(claim(&mut conn), Some(a));
    assert_eq!(claim(&mut conn), Some(b));
    assert_eq!(claim(&mut conn), Some(failed));
    let tx = conn.transaction().unwrap();
    db::apply_changes(
        &tx,
        &Actor::new("dispatcher"),
        failed,
        &Changes {
            state: Some("blocked".into()),
            note: Some(db::block_note("agent exited 3", "ops", "set pending")),
            ..Default::default()
        },
        Some("c"),
    )
    .unwrap();
    tx.commit().unwrap();
    db::edit_deps(&mut conn, &Actor::new("ops"), failed, &[b], &[]).unwrap();

    set_state(&mut conn, a, "done");
    db::edit_deps(&mut conn, &Actor::new("ops"), failed, &[], &[b]).unwrap();
    for id in [manual, approval, failed] {
        let t = get(&conn, id);
        assert!(t.waiting_on.is_empty());
        assert_eq!(
            (t.state.as_str(), t.status.as_str()),
            ("blocked", "blocked")
        );
    }
    assert_eq!(claim(&mut conn), None);
    let q = db::overview(&conn).unwrap().ops_queue;
    let kinds: Vec<(i64, &str, bool)> = q
        .iter()
        .map(|i| (i.task.id, i.kind.as_str(), i.actionable))
        .collect();
    assert_eq!(
        kinds,
        [
            (manual, "hold", false),
            (approval, "hold", false),
            (failed, "hold", false)
        ]
    );
    // Explicit resolution resumes each.
    set_state(&mut conn, manual, "pending");
    assert_eq!(claim(&mut conn), Some(manual));
    set_state(&mut conn, failed, "pending");
    assert_eq!(claim(&mut conn), Some(failed));
}

#[test]
fn pending_in_progress_and_approval_cannot_bypass_prerequisites() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let held = add_as(&mut conn, "p", "blocked", &[a]);
    let approved = add_as(&mut conn, "p", "blocked", &[a]);

    // Resolving the hold leaves a dependency wait, not ready work.
    set_state(&mut conn, held, "pending");
    let t = get(&conn, held);
    assert_eq!(
        (t.state.as_str(), t.status.as_str()),
        ("pending", "blocked")
    );
    assert_eq!(claim(&mut conn), Some(a));
    assert_eq!(claim(&mut conn), None);

    // Forcing in_progress is refused while waiting.
    let e = err(db::set_task(
        &mut conn,
        &Actor::new("human"),
        held,
        &Changes {
            state: Some("in_progress".into()),
            ..Default::default()
        },
    ));
    assert!(e.contains("waiting on unfinished prerequisites"), "{e}");
    assert_eq!(get(&conn, held).state, "pending");

    // Approval lifts the hold only.
    db::approve(
        &mut conn,
        &Actor::new("human"),
        approved,
        "abc123",
        "pr-merge",
        None,
    )
    .unwrap();
    let t = get(&conn, approved);
    assert_eq!(
        (t.state.as_str(), t.status.as_str()),
        ("pending", "blocked")
    );
    assert_eq!(claim(&mut conn), None);
    let q = db::overview(&conn).unwrap().ops_queue;
    assert!(q
        .iter()
        .all(|i| i.kind == "dependency" && !i.actionable && i.task.waiting_on == [a]));
    assert_eq!(q.len(), 2);

    set_state(&mut conn, a, "done");
    assert_eq!(claim(&mut conn), Some(held));
    assert_eq!(claim(&mut conn), Some(approved));
    assert!(db::overview(&conn).unwrap().ops_queue.is_empty());
}

#[test]
fn active_and_done_tasks_reject_unfinished_prerequisites() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let ops = Actor::new("ops");
    let running = add(&mut conn, &[]);
    assert_eq!(claim(&mut conn), Some(running));
    let finished = add_as(&mut conn, "p", "done", &[]);
    let open_prereq = add_as(&mut conn, "p", "pending", &[]);
    let done_prereq = add_as(&mut conn, "p", "done", &[]);
    for id in [running, finished] {
        let e = err(db::edit_deps(&mut conn, &ops, id, &[open_prereq], &[]));
        assert!(e.contains("refusing to add unfinished prerequisite"), "{e}");
        // A satisfied prerequisite is a harmless record.
        assert!(db::edit_deps(&mut conn, &ops, id, &[done_prereq], &[]).unwrap());
    }
    let t = get(&conn, running);
    assert_eq!(
        (t.state.as_str(), t.claimed_by.as_deref()),
        ("in_progress", Some("c"))
    );
    assert_eq!(get(&conn, finished).state, "done");
}

#[test]
fn reopening_a_prerequisite_flags_but_never_cancels_dependents() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add_as(&mut conn, "p", "pending", &[]);
    let queued = add(&mut conn, &[a]);
    let active = add(&mut conn, &[a]);
    let finished = add(&mut conn, &[a]);
    set_state(&mut conn, a, "done");
    assert_eq!(claim(&mut conn), Some(queued));
    // `queued` is now running; make another queued dependent.
    let waiting = add(&mut conn, &[a]);
    set_state(&mut conn, finished, "done");
    let _ = active;
    // Reopen as a hold so `a` itself is not claimable.
    set_state(&mut conn, a, "blocked");

    // Queued work becomes ineligible again; running and done work are untouched.
    assert_eq!(get(&conn, waiting).waiting_on, [a]);
    assert_eq!(get(&conn, active).status, "blocked");
    assert_eq!(claim(&mut conn), None);
    assert_eq!(get(&conn, queued).state, "in_progress");
    assert_eq!(get(&conn, finished).state, "done");
    for id in [queued, active, finished, waiting] {
        let ev = db::events(&conn, id).unwrap();
        let reopened: Vec<_> = ev.iter().filter(|e| e.kind == "dep_reopened").collect();
        assert_eq!(reopened.len(), 1, "{id}: {ev:?}");
        assert_eq!(reopened[0].to_value.as_deref(), Some("blocked"));
    }
    set_state(&mut conn, a, "done");
    assert_eq!(claim(&mut conn), Some(active));
}

#[test]
fn concurrent_final_completion_and_claims_have_one_valid_winner() {
    for _ in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut conn = db::open(&path).unwrap();
        let a = add(&mut conn, &[]);
        let b = add_as(&mut conn, "p", "done", &[]);
        let c = add(&mut conn, &[a, b]);
        assert_eq!(claim(&mut conn), Some(a));
        drop(conn);

        let claimers: Vec<_> = (0..6)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut conn = db::open(&path).unwrap();
                    let mut won = Vec::new();
                    for _ in 0..200 {
                        let by = format!("c{i}");
                        if let Claim::Claimed(t) =
                            db::claim(&mut conn, &Actor::new("d"), None, &by, None).unwrap()
                        {
                            won.push(t.id);
                        }
                    }
                    won
                })
            })
            .collect();
        let finisher = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut conn = db::open(&path).unwrap();
                set_state(&mut conn, a, "done");
            })
        };
        finisher.join().unwrap();
        let won: Vec<i64> = claimers
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let conn = db::open(&path).unwrap();
        assert!(won.len() <= 1 && won.iter().all(|&id| id == c), "{won:?}");
        if won.is_empty() {
            assert!(get(&conn, c).ready());
            continue;
        }
        // The claim committed after the final prerequisite completed.
        let ev = |id: i64, kind: &str| {
            db::events(&conn, id)
                .unwrap()
                .into_iter()
                .find(|e| {
                    e.kind == kind && (kind != "state" || e.to_value.as_deref() == Some("done"))
                })
                .unwrap()
                .id
        };
        assert!(ev(a, "state") < ev(c, "claimed"));
        assert_eq!(get(&conn, c).runs, 1);
    }
}

#[test]
fn readiness_is_derived_and_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let c = add_as(&mut conn, "p", "pending", &[a]);
    set_state(&mut conn, a, "done");
    // Repeated completion events are harmless.
    set_state(&mut conn, a, "pending");
    set_state(&mut conn, a, "done");
    set_state(&mut conn, a, "done");
    drop(conn);
    let mut conn = open(&dir);
    assert!(get(&conn, c).ready());
    assert_eq!(claim(&mut conn), Some(c));
    assert_eq!(claim(&mut conn), None);
}

#[test]
fn list_and_counts_use_the_derived_status() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = open(&dir);
    let a = add(&mut conn, &[]);
    let waiting = add(&mut conn, &[a]);
    let held = add_as(&mut conn, "p", "blocked", &[]);
    let list = |conn: &Connection, state: &str| -> Vec<i64> {
        db::list_tasks(
            conn,
            &db::Filter {
                state: Some(state),
                ..Default::default()
            },
        )
        .unwrap()
        .iter()
        .map(|t| t.id)
        .collect()
    };
    assert_eq!(list(&conn, "pending"), [a]);
    assert_eq!(list(&conn, "blocked"), [waiting, held]);
    let counts = db::overview(&conn).unwrap().counts;
    let c: Vec<(&str, i64)> = counts.iter().map(|c| (c.state.as_str(), c.count)).collect();
    assert_eq!(c, [("blocked", 2), ("pending", 1)]);
}

mod common;

use common::{Server, TOKEN};

#[test]
fn cli_and_api_fan_in_fixture_with_separate_manual_hold() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let id = |out: String| out.trim().parse::<i64>().unwrap();
    // Several tasks created together; C waits on A and B and also has a manual hold.
    let a = id(srv.ok(&["add", "--project", "orchid", "--title", "A"]));
    let b = id(srv.ok(&["add", "--project", "fern", "--title", "B"]));
    let c = id(srv.ok(&[
        "--actor",
        "ops",
        "add",
        "--project",
        "orchid",
        "--title",
        "C",
        "--state",
        "blocked",
        "--blocked-by",
        &format!("{a},{b}"),
    ]));
    srv.ok(&[
        "note",
        &c.to_string(),
        "blocked: owner must confirm scope\nresolver: ops\nresume: axe set C --state pending",
    ]);
    let d = id(srv.ok(&[
        "add",
        "--project",
        "fern",
        "--title",
        "D",
        "--blocked-by",
        &a.to_string(),
    ]));

    let show = srv.ok(&["show", &c.to_string()]);
    assert!(show.contains("state: blocked\n"), "{show}");
    assert!(
        show.contains(&format!(
            "blocked_by:\n  #{a} [orchid] pending A\n  #{b} [fern] pending B"
        )),
        "{show}"
    );
    assert!(
        show.contains("waiting: hold (resolve it explicitly) and waiting on prerequisites"),
        "{show}"
    );
    let deps = srv.ok(&["deps", &a.to_string()]);
    assert!(
        deps.contains(&format!(
            "blocks:\n  #{c} [orchid] blocked C\n  #{d} [fern] blocked D"
        )),
        "{deps}"
    );
    let list = srv.ok(&["list"]);
    assert!(
        list.contains(&format!("blocked (hold+deps #{a}, #{b})")),
        "{list}"
    );
    assert!(list.contains(&format!("blocked (deps #{a})")), "{list}");
    let blocked = srv.ok(&["list", "--state", "blocked"]);
    assert_eq!(blocked.lines().count(), 3, "{blocked}");

    // Independent tasks are claimed; dependents are not.
    assert_eq!(id(srv.ok(&["claim", "--by", "w1"])), a);
    assert_eq!(id(srv.ok(&["claim", "--by", "w2"])), b);
    let none = srv.run(&["claim", "--by", "w3"]);
    assert_eq!(none.status.code(), Some(1));

    // Overview distinguishes the actionable hold from the dependency-only wait.
    let ov = srv.get("/api/overview");
    let q: Vec<(i64, String, bool)> = ov["ops_queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["id"].as_i64().unwrap(),
                i["kind"].as_str().unwrap().to_string(),
                i["actionable"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        q,
        [(c, "hold".into(), false), (d, "dependency".into(), false)]
    );
    assert_eq!(ov["ops_queue"][1]["waiting_on"], serde_json::json!([a]));
    assert_eq!(
        ov["ops_queue"][0]["blocker"]
            .as_str()
            .unwrap()
            .lines()
            .next(),
        Some("blocked: owner must confirm scope")
    );

    // API errors: invalid ids, self edges, cycles, and unsafe active edits are 400s.
    let post = |path: &str, body: &str| -> (u16, String) {
        match ureq::post(&format!("{}{path}", srv.url))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_string(body)
        {
            Ok(r) => (r.status(), r.into_string().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
            Err(e) => panic!("{e}"),
        }
    };
    for (path, body, msg) in [
        (
            format!("/api/tasks/{c}/deps"),
            r#"{"add":[999]}"#.to_string(),
            "does not exist",
        ),
        (
            format!("/api/tasks/{c}/deps"),
            format!(r#"{{"add":[{c}]}}"#),
            "itself",
        ),
        (
            format!("/api/tasks/{a}/deps"),
            format!(r#"{{"add":[{c}]}}"#),
            "cycle",
        ),
        (
            format!("/api/tasks/{b}/deps"),
            format!(r#"{{"add":[{d}]}}"#),
            "in_progress",
        ),
        (
            "/api/tasks".to_string(),
            r#"{"project":"p","title":"x","blocked_by":[999]}"#.to_string(),
            "does not exist",
        ),
    ] {
        let (code, text) = post(&path, &body);
        assert_eq!(code, 400, "{path} {body}: {text}");
        assert!(text.contains(msg), "{text}");
    }
    let tasks = srv.get("/api/tasks?all=1");
    assert_eq!(tasks.as_array().unwrap().len(), 4);

    // A finishes first: C still waits on B; D becomes ready on its own.
    srv.ok(&["set", &a.to_string(), "--state", "done"]);
    let t = srv.get(&format!("/api/tasks/{c}"));
    assert_eq!(t["waiting_on"], serde_json::json!([b]));
    assert_eq!(srv.get(&format!("/api/tasks/{d}"))["status"], "pending");
    srv.ok(&["set", &b.to_string(), "--state", "done"]);

    // Every prerequisite done: the manual hold remains and still needs ops.
    let t = srv.get(&format!("/api/tasks/{c}"));
    assert_eq!(t["state"].as_str(), Some("blocked"));
    assert!(t.get("owner").is_none());
    assert_eq!(t["waiting_on"], serde_json::json!([]));
    // Only D is ready: the hold on C is never claimed.
    assert_eq!(id(srv.ok(&["claim", "--by", "w4"])), d);
    let none = srv.run(&["claim", "--by", "w5"]);
    assert_eq!(none.status.code(), Some(1));
    let ov = srv.get("/api/overview");
    assert_eq!(ov["ops_queue"].as_array().unwrap().len(), 1);
    assert_eq!(ov["ops_queue"][0]["kind"], "hold");

    // Explicitly resolve the hold; C is claimed exactly once.
    srv.ok(&[
        "--actor",
        "ops",
        "set",
        &c.to_string(),
        "--state",
        "pending",
        "--note",
        "scope confirmed",
    ]);
    let claimers: Vec<_> = (0..8)
        .map(|i| {
            srv.cli(&["claim", "--by", &format!("v{i}")])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let won: Vec<String> = claimers
        .into_iter()
        .map(|ch| ch.wait_with_output().unwrap())
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8(o.stdout).unwrap())
        .collect();
    assert_eq!(won, [format!("{c}\n")]);
    let t = srv.get(&format!("/api/tasks/{c}"));
    assert_eq!(
        (t["state"].as_str(), t["runs"].as_i64()),
        (Some("in_progress"), Some(1))
    );

    // Edge edits through the CLI are audited; removal is idempotent.
    srv.ok(&[
        "--actor",
        "ops",
        "deps",
        &d.to_string(),
        "--remove",
        &a.to_string(),
    ]);
    let again = srv.ok(&["deps", &d.to_string(), "--remove", &a.to_string(), "--json"]);
    let v: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert!(v["deps"]["blocked_by"].as_array().unwrap().is_empty());
    let log = srv.ok(&["log", &d.to_string()]);
    assert!(
        log.contains(&format!("ops dep_remove {a}: blocked_by #{a}")),
        "{log}"
    );
    let log = srv.ok(&["log", &a.to_string()]);
    assert!(
        log.contains(&format!("ops dep_add {c}: blocks #{c}")),
        "{log}"
    );
    assert!(
        log.contains(&format!("human dep_add {d}: blocks #{d}")),
        "{log}"
    );
}
