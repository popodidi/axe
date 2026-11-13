use axe::db::{self, Actor, Changes, Claim, NewTask};

fn new_task(project: &str) -> NewTask<'_> {
    NewTask {
        agent: None,
        project,
        title: "t",
        detail: "",
        state: "pending",
    }
}

fn kinds(conn: &rusqlite::Connection, id: i64) -> Vec<String> {
    db::events(conn, id)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[test]
fn mutations_write_expected_events() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let actor = Actor {
        name: "agent".into(),
        session: Some("s1".into()),
    };
    let id = db::add_task(&mut conn, &actor, &new_task("orchid")).unwrap();
    assert_eq!(kinds(&conn, id), ["created"]);
    assert_eq!(
        db::events(&conn, id).unwrap()[0].body.as_deref(),
        Some("state=pending")
    );

    let c = Changes {
        state: Some("pending".into()), // unchanged
        title: Some("t".into()),       // unchanged
        note: Some("progress".into()),
        ..Default::default()
    };
    assert!(db::set_task(&mut conn, &actor, id, &c).unwrap());
    assert_eq!(kinds(&conn, id), ["created", "note"]);

    let unchanged = Changes {
        state: Some("pending".into()),
        ..Default::default()
    };
    assert!(!db::set_task(&mut conn, &actor, id, &unchanged).unwrap());
    assert_eq!(kinds(&conn, id).len(), 2);

    let both = Changes {
        state: Some("blocked".into()),
        detail: Some("new".into()),
        ..Default::default()
    };
    db::set_task(&mut conn, &actor, id, &both).unwrap();
    db::append(&mut conn, &actor, id, "note", "n").unwrap();
    db::append(&mut conn, &actor, id, "artifact", "https://pr").unwrap();
    assert_eq!(
        kinds(&conn, id),
        ["created", "note", "state", "edit", "note", "artifact"]
    );

    let ev = db::events(&conn, id).unwrap();
    assert_eq!(ev[2].from_value.as_deref(), Some("pending"));
    assert_eq!(ev[2].to_value.as_deref(), Some("blocked"));
    assert_eq!(ev[5].body.as_deref(), Some("https://pr"));
    assert!(ev
        .iter()
        .all(|e| e.actor == "agent" && e.session.as_deref() == Some("s1")));
}

#[test]
fn detail_edit_stores_unified_diff() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor::new("ops");
    let mut t = new_task("orchid");
    t.detail = "## Outcome\nold line\n## Verify\nrun tests\n";
    let id = db::add_task(&mut conn, &a, &t).unwrap();
    let c = Changes {
        title: Some("t2".into()),
        detail: Some("## Outcome\nnew line\n## Verify\nrun tests\n".into()),
        ..Default::default()
    };
    db::set_task(&mut conn, &a, id, &c).unwrap();

    let ev = db::events(&conn, id).unwrap();
    let title = &ev[1];
    assert_eq!(
        (
            title.kind.as_str(),
            title.from_value.as_deref(),
            title.to_value.as_deref(),
            title.body.as_deref()
        ),
        ("edit", Some("t"), Some("t2"), Some("title"))
    );
    let detail = &ev[2];
    assert_eq!(detail.kind, "edit");
    assert_eq!(
        (detail.from_value.as_deref(), detail.to_value.as_deref()),
        (None, None)
    );
    let diff = detail.body.as_deref().unwrap();
    assert!(diff.starts_with("--- detail\n+++ detail\n@@ "), "{diff}");
    let lines: Vec<&str> = diff.lines().collect();
    assert!(lines.contains(&"-old line"), "{diff}");
    assert!(lines.contains(&"+new line"), "{diff}");
    assert!(
        !diff.contains("-run tests") && !diff.contains("+run tests"),
        "{diff}"
    );
    assert_eq!(
        db::get_task(&conn, id).unwrap().detail,
        "## Outcome\nnew line\n## Verify\nrun tests\n"
    );
}

#[test]
fn invalid_state_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor::new("human");
    let mut weird = new_task("p");
    weird.state = "weird";
    assert!(db::add_task(&mut conn, &a, &weird).is_err());
    let id = db::add_task(&mut conn, &a, &new_task("p")).unwrap();
    let bad_state = Changes {
        state: Some("weird".into()),
        ..Default::default()
    };
    assert!(db::set_task(&mut conn, &a, id, &bad_state).is_err());
    assert_eq!(kinds(&conn, id), ["created"]);
}

#[test]
fn leaving_in_progress_clears_claim() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor::new("human");
    let id = db::add_task(&mut conn, &a, &new_task("p")).unwrap();
    let Claim::Claimed(t) = db::claim(&mut conn, &a, None, "me", None).unwrap() else {
        panic!("expected claim");
    };
    assert_eq!(
        (t.state.as_str(), t.claimed_by.as_deref(), t.runs),
        ("in_progress", Some("me"), 1)
    );
    let c = Changes {
        state: Some("pending".into()),
        ..Default::default()
    };
    db::set_task(&mut conn, &a, id, &c).unwrap();
    assert_eq!(db::get_task(&conn, id).unwrap().claimed_by, None);
}

#[test]
fn max_runs_guard_escalates() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor::new("dispatcher");
    let id = db::add_task(&mut conn, &a, &new_task("p")).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            db::claim(&mut conn, &a, Some("p"), "x", Some(2)).unwrap(),
            Claim::Claimed(_)
        ));
        let back = Changes {
            state: Some("pending".into()),
            ..Default::default()
        };
        db::set_task(&mut conn, &a, id, &back).unwrap();
    }
    assert!(matches!(
        db::claim(&mut conn, &a, Some("p"), "x", Some(2)).unwrap(),
        Claim::Blocked(i) if i == id
    ));
    let t = db::get_task(&conn, id).unwrap();
    assert_eq!((t.state.as_str(), t.runs), ("escalated", 2));
    let last = db::events(&conn, id).unwrap().pop().unwrap();
    assert_eq!(
        last.body.as_deref(),
        Some(&*format!("blocked: max_runs exceeded (2 >= 2)\nresolver: ops: review the run history and decide whether more runs are warranted\nresume: axe set {id} --reset-runs --state pending"))
    );
    // Blocked tasks are never claimed, with or without a run budget.
    for max in [None, Some(2)] {
        assert!(matches!(
            db::claim(&mut conn, &a, Some("p"), "x", max).unwrap(),
            Claim::None
        ));
    }
    // Resetting the budget alone does not resume.
    let reset = Changes {
        reset_runs: true,
        ..Default::default()
    };
    db::set_task(&mut conn, &a, id, &reset).unwrap();
    assert_eq!(db::get_task(&conn, id).unwrap().state, "escalated");
    let resume = Changes {
        state: Some("pending".into()),
        ..Default::default()
    };
    db::set_task(&mut conn, &a, id, &resume).unwrap();
    let Claim::Claimed(t) = db::claim(&mut conn, &a, Some("p"), "x", Some(2)).unwrap() else {
        panic!("expected claim after reset and resume");
    };
    assert_eq!((t.id, t.runs), (id, 1));
    assert!(!kinds(&conn, id).contains(&"approval".to_string()));
}

#[test]
fn round_robin_persists_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut c1 = db::open(&path).unwrap();
    let got: Vec<usize> = (0..3)
        .map(|_| db::rr_next(&mut c1, "r", 2).unwrap())
        .collect();
    assert_eq!(got, [0, 1, 0]);
    drop(c1);
    let mut c2 = db::open(&path).unwrap();
    assert_eq!(db::rr_next(&mut c2, "r", 2).unwrap(), 1);
    assert_eq!(db::rr_next(&mut c2, "other", 3).unwrap(), 0);
}

fn user_version(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

fn columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn tables(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn fresh_database_is_created_at_v17() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db::open(&dir.path().join("t.db")).unwrap();
    assert_eq!(user_version(&conn), 17);
    assert!(!tables(&conn).contains(&"dream_attempts".to_string()));
    // v17's `owner` is the task's follow-up owner, not the persona column v9 dropped.
    assert!(columns(&conn, "tasks").ends_with(&["owner".into(), "repos".into(), "labels".into()]));
    assert!(!columns(&conn, "runs").contains(&"persona".to_string()));
    assert_eq!(
        columns(&conn, "sessions"),
        ["task_id", "agent", "session_id", "resume_count"]
    );
}

#[test]
fn migrates_v8_to_one_session_per_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v8.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/schema-v8.sql"))
        .unwrap();
    conn.pragma_update(None, "user_version", 8).unwrap();
    conn.execute_batch(
        "INSERT INTO tasks (id, project, title, detail, owner, state, claimed_by, runs, created_at, updated_at, not_before) VALUES
           (1,'orchid','all three','spec','delivery','escalated',NULL,4,'c1','u1',NULL),
           (2,'orchid','no builder','','verifier','pending',NULL,2,'c2','u2','2030-01-01T00:00:00Z'),
           (3,'fern','no session','','builder','done',NULL,1,'c3','u3',NULL);
         INSERT INTO events (id, task_id, at, actor, kind, body) VALUES
           (1,1,'a','ops','note','kept'),(2,2,'a','ops','note','kept'),(3,3,'a','ops','note','kept');
         INSERT INTO sessions (task_id, persona, agent, session_id, last_event_id, resume_count, detail) VALUES
           (1,'verifier','codex','v-1',9,0,''),
           (1,'builder','claude','b-1',5,3,''),
           (1,'delivery','claude','d-1',12,1,''),
           (2,'verifier','codex','v-2',4,1,''),
           (2,'delivery','claude','d-2',8,2,'');
         INSERT INTO runs (id, task_id, project, persona, agent, mode, session_id, claim, started_at, ended_at, exit_code, log_path, current_activity, last_activity_at) VALUES
           (1,1,'orchid','builder','claude','fresh','b-1','dispatch:a','s1','e1',0,'/l/1-1.log','Bash: ls','t1'),
           (2,1,'orchid','verifier','codex','fresh','v-1','dispatch:b','s2','e2',0,'/l/1-2.log',NULL,NULL),
           (3,2,'orchid','delivery','claude','fresh','d-2','dispatch:c','s3',NULL,NULL,'/l/2-1.log',NULL,NULL);
         INSERT INTO dispatch_runs VALUES (1,'d','g','finished');
         INSERT INTO deps (task_id, prereq_id, created_at) VALUES (2,1,'c');
         INSERT INTO rr (rule, next) VALUES ('defaults', 5);
         INSERT INTO dream_attempts (project, day, task_id, proposal, revision, disposition, reserved_at, actor, event_id)
           VALUES ('orchid','2026-10-01',1,'p',1,'delivered','before','ops',1);",
    )
    .unwrap();
    drop(conn);

    let mut conn = db::open(&path).unwrap();
    assert_eq!(user_version(&conn), 17);
    assert!(!tables(&conn).contains(&"dream_attempts".to_string()));
    // The v8 persona `owner` is dropped; v17 adds an empty follow-up owner.
    let owners: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE owner IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owners, 0);
    assert!(!columns(&conn, "runs").contains(&"persona".to_string()));
    let index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'tasks_owner_state'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index, 0);

    // Every row survives with its id; sessions collapse to one per task.
    for (table, n) in [
        ("tasks", 3),
        ("events", 3),
        ("runs", 3),
        ("dispatch_runs", 1),
        ("deps", 1),
        ("rr", 1),
        ("sessions", 2),
    ] {
        assert_eq!(count(&conn, table), n, "{table}");
    }
    let t = db::get_task(&conn, 1).unwrap();
    assert_eq!(
        (
            t.title.as_str(),
            t.detail.as_str(),
            t.state.as_str(),
            t.runs,
            t.created_at.as_str(),
            t.updated_at.as_str()
        ),
        ("all three", "spec", "escalated", 4, "c1", "u1")
    );
    let t2 = db::get_task(&conn, 2).unwrap();
    assert_eq!(t2.not_before.as_deref(), Some("2030-01-01T00:00:00Z"));
    assert_eq!(t2.waiting_on, [1]);

    // The builder session wins; without one, the most recently prompted session.
    let s1 = db::session(&conn, 1).unwrap().unwrap();
    assert_eq!(
        (s1.agent.as_str(), s1.session_id.as_str(), s1.resume_count),
        ("claude", "b-1", 3)
    );
    let s2 = db::session(&conn, 2).unwrap().unwrap();
    assert_eq!(
        (s2.agent.as_str(), s2.session_id.as_str(), s2.resume_count),
        ("claude", "d-2", 2)
    );
    assert!(db::session(&conn, 3).unwrap().is_none());

    let runs = db::list_runs(
        &conn,
        &db::RunFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    let ids: Vec<i64> = runs.iter().map(|r| r.id).collect();
    assert_eq!(ids, [3, 2, 1]);
    let r1 = &runs[2];
    assert_eq!(
        (
            r1.agent.as_str(),
            r1.session_id.as_deref(),
            r1.claim.as_str(),
            r1.exit_code,
            r1.current_activity.as_deref()
        ),
        (
            "claude",
            Some("b-1"),
            "dispatch:a",
            Some(0),
            Some("Bash: ls")
        )
    );
    assert!(runs[0].live);
    assert_eq!(db::rr_next(&mut conn, "defaults", 10).unwrap(), 5);
    let fk: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(fk, 0);

    // The migrated schema accepts new rows and reopens as a no-op.
    db::add_task(&mut conn, &Actor::new("human"), &new_task("p")).unwrap();
    db::save_session(
        &conn,
        3,
        &db::Session {
            agent: "codex".into(),
            session_id: "s-3".into(),
            resume_count: 0,
        },
    )
    .unwrap();
    drop(conn);
    let conn = db::open(&path).unwrap();
    assert_eq!(count(&conn, "tasks"), 4);
    assert_eq!(count(&conn, "sessions"), 3);
}

#[test]
fn schemas_older_than_v8_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v7.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/schema-v8.sql"))
        .unwrap();
    conn.pragma_update(None, "user_version", 7).unwrap();
    drop(conn);
    let err = db::open(&path).unwrap_err();
    assert!(err.to_string().contains("too old"), "{err}");
}

#[test]
fn session_is_one_row_per_task() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let id = db::add_task(&mut conn, &Actor::new("human"), &new_task("p")).unwrap();
    assert!(db::session(&conn, id).unwrap().is_none());
    for (sid, n) in [("a", 0), ("a", 1), ("b", 0)] {
        db::save_session(
            &conn,
            id,
            &db::Session {
                agent: "claude".into(),
                session_id: sid.into(),
                resume_count: n,
            },
        )
        .unwrap();
        let s = db::session(&conn, id).unwrap().unwrap();
        assert_eq!((s.session_id.as_str(), s.resume_count), (sid, n));
    }
    assert_eq!(count(&conn, "sessions"), 1);
    let info = db::find_session(&conn, "b").unwrap().unwrap();
    assert_eq!(
        (info.task_id, info.agent.as_str(), info.run.is_none()),
        (id, "claude", true)
    );
    db::delete_session(&conn, id).unwrap();
    assert!(db::session(&conn, id).unwrap().is_none());
}

fn open_run(conn: &rusqlite::Connection, task: i64, session: &str, claim: &str) -> i64 {
    db::insert_run(
        conn,
        &db::NewRun {
            task_id: task,
            project: "p",
            agent: "claude",
            mode: "fresh",
            session_id: Some(session),
            claim,
            log_path: "/dev/null",
        },
    )
    .unwrap()
}

#[test]
fn approve_records_approval_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor {
        name: "ops".into(),
        session: Some("interactive".into()),
    };
    let mut t = new_task("p");
    t.state = "blocked";
    let id = db::add_task(&mut conn, &a, &t).unwrap();
    db::approve(&mut conn, &a, id, "abc123", "prod", Some("ship it")).unwrap();
    let ev = db::events(&conn, id).unwrap();
    let tail: Vec<_> = ev[1..]
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.from_value.as_deref(),
                e.to_value.as_deref(),
                e.body.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        tail,
        [
            (
                "approval",
                None,
                None,
                Some("sha=abc123 scope=prod by=ops\nship it")
            ),
            ("state", Some("blocked"), Some("pending"), None),
        ]
    );
    assert_eq!(db::get_task(&conn, id).unwrap().state, "pending");

    // Escalated tasks are approved the same way; without a note the body is just the key=value line.
    t.state = "escalated";
    let id2 = db::add_task(&mut conn, &a, &t).unwrap();
    db::approve(&mut conn, &a, id2, "def", "prod", None).unwrap();
    let ev = db::events(&conn, id2).unwrap();
    assert_eq!(ev[1].body.as_deref(), Some("sha=def scope=prod by=ops"));
}

#[test]
fn approve_validation_errors_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor::new("human");
    let mut t = new_task("p");
    t.state = "blocked";
    let ops = db::add_task(&mut conn, &a, &t).unwrap();
    // Only held tasks: not pending, in-progress or done work.
    let mut others = Vec::new();
    for state in ["pending", "in_progress", "done"] {
        let mut t = new_task("p");
        t.state = state;
        let id = db::add_task(&mut conn, &a, &t).unwrap();
        let err = db::approve(&mut conn, &a, id, "s", "x", None).unwrap_err();
        assert!(
            err.to_string().contains("only blocked or escalated tasks"),
            "{err}"
        );
        others.push(id);
    }
    for (sha, scope) in [
        ("", "x"),
        ("  ", "x"),
        ("a b", "x"),
        ("s", ""),
        ("s", " \t"),
    ] {
        let err = db::approve(&mut conn, &a, ops, sha, scope, None).unwrap_err();
        assert!(err.to_string().contains("single word"), "{err}");
    }
    assert!(db::approve(&mut conn, &a, 99, "s", "x", None).is_err());
    assert_eq!(kinds(&conn, ops), ["created"]);
    for id in others {
        assert_eq!(kinds(&conn, id), ["created"]);
    }
    assert_eq!(db::get_task(&conn, ops).unwrap().state, "blocked");
}

#[test]
fn approve_refuses_open_dispatcher_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let a = Actor::new("human");
    let mut t = new_task("p");
    t.state = "blocked";
    let id = db::add_task(&mut conn, &a, &t).unwrap();
    let run = open_run(&conn, id, "sess-1", "dispatch:c1");
    // Dispatched agents, the dispatcher, and any session of an open run.
    for (name, s) in [
        ("human", Some("sess-1")),
        ("human", Some("dispatch:c1")),
        ("human", Some("dispatch:other")),
        ("agent", None),
        ("dispatcher", None),
    ] {
        let agent = Actor {
            name: name.into(),
            session: s.map(String::from),
        };
        let err = db::approve(&mut conn, &agent, id, "s", "x", None).unwrap_err();
        assert!(err.downcast_ref::<db::Forbidden>().is_some(), "{err}");
    }
    assert_eq!(kinds(&conn, id), ["created"]);

    db::end_run(&conn, run, Some(0), None).unwrap();
    let ended = Actor {
        name: "ops".into(),
        session: Some("sess-1".into()),
    };
    db::approve(&mut conn, &ended, id, "s", "x", None).unwrap();
    assert_eq!(db::get_task(&conn, id).unwrap().state, "pending");
}

#[test]
fn migrates_v10_agent_column_and_retains_it_after_compatibility_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v10.db");
    let mut c = db::open(&path).unwrap();
    let id = db::add_task(&mut c, &Actor::new("test"), &new_task("orchid")).unwrap();
    c.execute_batch("ALTER TABLE tasks DROP COLUMN agent; PRAGMA user_version=10;")
        .unwrap();
    let before: String = c
        .query_row("SELECT title FROM tasks WHERE id=?1", [id], |r| r.get(0))
        .unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    assert_eq!(db::get_task(&c, id).unwrap().title, before);
    assert_eq!(db::get_task(&c, id).unwrap().agent, None);
    c.execute("UPDATE tasks SET agent='claude' WHERE id=?1", [id])
        .unwrap();
    c.pragma_update(None, "user_version", 10).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    assert_eq!(
        db::get_task(&c, id).unwrap().agent.as_deref(),
        Some("claude")
    );
}

#[test]
fn migrates_v12_to_v14_subscription_tables_and_keeps_rows_after_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v12.db");
    let mut c = db::open(&path).unwrap();
    let id = db::add_task(&mut c, &Actor::new("test"), &new_task("orchid")).unwrap();
    c.execute_batch("DROP TABLE bot_feed; DROP TABLE bot_subscriptions; PRAGMA user_version=12;")
        .unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    for t in ["bot_feed", "bot_subscriptions"] {
        assert!(tables(&c).contains(&t.to_string()), "{t}");
    }
    assert_eq!(db::get_task(&c, id).unwrap().title, "t");
    c.pragma_update(None, "user_version", 12).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    assert_eq!(db::get_task(&c, id).unwrap().title, "t");
}

#[test]
fn migrates_v11_to_v12_bot_tables_and_retains_them_after_compatibility_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v11.db");
    let mut c = db::open(&path).unwrap();
    let id = db::add_task(&mut c, &Actor::new("test"), &new_task("orchid")).unwrap();
    c.execute_batch(
        "DROP TABLE bot_feed; DROP TABLE bot_subscriptions; DROP TABLE bot_events; DROP TABLE bot_wakes; DROP TABLE bot_runs; DROP TABLE bot_messages;
         DROP TABLE bots; PRAGMA user_version=11;",
    )
    .unwrap();
    drop(c);
    let mut c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    for t in [
        "bots",
        "bot_messages",
        "bot_wakes",
        "bot_runs",
        "bot_events",
    ] {
        assert!(tables(&c).contains(&t.to_string()), "{t}");
    }
    assert_eq!(db::get_task(&c, id).unwrap().title, "t");
    let bot = axe::bot::NewBot {
        name: "b".into(),
        project: "orchid".into(),
        agent: None,
        identity: "i".into(),
        conduct: "c".into(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    };
    axe::bot::create(&mut c, &Actor::new("test"), &bot, 1_800_000_000).unwrap();
    // Compatibility downgrade: lowering user_version keeps every bot row, and the
    // next open upgrades in place without losing them.
    c.pragma_update(None, "user_version", 11).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    assert_eq!(count(&c, "bots"), 1);
    assert_eq!(count(&c, "bot_wakes"), 1);
}

#[test]
fn migrates_v16_task_fields_and_keeps_them_after_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v16.db");
    let mut c = db::open(&path).unwrap();
    let id = db::add_task(&mut c, &Actor::new("test"), &new_task("orchid")).unwrap();
    c.execute_batch(
        "ALTER TABLE tasks DROP COLUMN owner; ALTER TABLE tasks DROP COLUMN repos;
         ALTER TABLE tasks DROP COLUMN labels; PRAGMA user_version=16;",
    )
    .unwrap();
    drop(c);
    let mut c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    for col in ["owner", "repos", "labels"] {
        assert!(columns(&c, "tasks").contains(&col.to_string()), "{col}");
    }
    // An old task has no owner or labels and keeps its project checkout.
    let t = db::get_task(&c, id).unwrap();
    assert_eq!((t.owner, t.repos, t.labels), (None, None, vec![]));
    db::set_task(
        &mut c,
        &Actor::new("test"),
        id,
        &db::Changes {
            owner: Some("bot:b".into()),
            repos: Some(vec!["a".into(), "b".into()]),
            add_labels: vec!["x".into()],
            ..Default::default()
        },
    )
    .unwrap();
    // Compatibility downgrade keeps the columns and values; the next open
    // upgrades in place.
    c.pragma_update(None, "user_version", 16).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(user_version(&c), 17);
    let t = db::get_task(&c, id).unwrap();
    assert_eq!(t.owner.as_deref(), Some("bot:b"));
    assert_eq!(t.repos, Some(vec!["a".to_string(), "b".to_string()]));
    assert_eq!(t.labels, vec!["x".to_string()]);
}
