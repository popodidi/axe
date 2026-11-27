//! `axe project rename`: every row keyed by the project name and its docs
//! move at once; refusals; dry-run counts.
mod common;

use axe::db::{self, Actor, Fields, NewTask};
use axe::doc::{Body, Scope, Store};
use axe::project::{self, Request};
use common::Server;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const NOW: &str = "2026-01-01T00:00:00Z";

fn task(conn: &mut Connection, project: &str, state: &str, labels: &[&str]) -> i64 {
    db::add_task_full(
        conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project,
            title: "t",
            detail: "",
            state,
        },
        &[],
        None,
        None,
        &Fields {
            owner: None,
            repos: None,
            labels: labels.iter().map(|l| l.to_string()).collect(),
        },
    )
    .unwrap()
}

fn exec(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> i64 {
    conn.execute(sql, p).unwrap();
    conn.last_insert_rowid()
}

/// A database with one of everything naming project `old`, plus rows of
/// project `keep` that must not move, and a doc store with `old/` docs.
fn fixture(dir: &std::path::Path) -> (Connection, Store, i64) {
    let mut c = db::open(&dir.join("axe.db")).unwrap();
    let t1 = task(&mut c, "old", "pending", &["old", "x"]);
    task(&mut c, "old", "done", &[]);
    // Another project's task labelled with the project: the label moves too.
    let other = task(&mut c, "keep", "pending", &["old"]);
    task(&mut c, "keep", "pending", &["x"]);
    exec(
        &c,
        "INSERT INTO runs (task_id, project, agent, mode, claim, started_at, ended_at, log_path)
         VALUES (?1, 'old', 'a', 'fresh', 'c', ?2, ?2, 'l')",
        params![t1, NOW],
    );
    let bot = exec(
        &c,
        "INSERT INTO bots (name, project, agent, state, created_at, updated_at)
         VALUES ('b', 'old', 'a', 'active', ?1, ?1)",
        [NOW],
    );
    exec(
        &c,
        "INSERT INTO bots (name, project, agent, state, created_at, updated_at)
         VALUES ('k', 'keep', 'a', 'active', ?1, ?1)",
        [NOW],
    );
    for p in ["old", "keep"] {
        exec(
            &c,
            "INSERT INTO bot_subscriptions (bot_id, project, wake_events, ride_events,
             batch_seconds, created_at, updated_at) VALUES (?1, ?2, '[]', '[]', 300, ?3, ?3)",
            params![bot, p, NOW],
        );
    }
    exec(
        &c,
        "INSERT INTO bot_feed (bot_id, event_id, project, task_id, event, title, mode, at)
         VALUES (?1, 1, 'old', ?2, 'new', 't', 'ride', ?3)",
        params![bot, t1, NOW],
    );
    exec(
        &c,
        "INSERT INTO goals (name, project, state, created_at, updated_at)
         VALUES ('g', 'old', 'active', ?1, ?1)",
        [NOW],
    );
    let routine = |project: &str| {
        json!({"name": "r", "project": project, "cron": "0 9 * * *", "timezone": "UTC",
               "title": "t", "detail": "d", "labels": ["old", "y"]})
        .to_string()
    };
    let r = exec(
        &c,
        "INSERT INTO routines (config, enabled, next_at, created_at, updated_at)
         VALUES (?1, 1, 0, ?2, ?2)",
        params![routine("old"), NOW],
    );
    exec(
        &c,
        "INSERT INTO routine_occurrences (routine_id, identity, task_id, snapshot, created_at)
         VALUES (?1, 'i', ?2, ?3, ?4)",
        params![r, t1, routine("old"), NOW],
    );
    exec(
        &c,
        "INSERT INTO rules (matcher, uses, created_at) VALUES (?1, '[]', ?2)",
        params![json!({"label": "old"}).to_string(), NOW],
    );
    exec(
        &c,
        "INSERT INTO rules (matcher, uses, created_at) VALUES (?1, '[]', ?2)",
        params![json!({"label": "keep"}).to_string(), NOW],
    );

    let store = Store::open(&Default::default(), &dir.join("axe.db")).unwrap();
    let doc = |s: &str| Body::Doc(s.to_string());
    for p in [
        "old/docs/index.md",
        "old/docs/ops/run.md",
        "older/x.md",
        "keep/y.md",
    ] {
        store.put(p, &doc(p), None, "human", "add").unwrap();
    }
    store
        .attach(
            "old/docs/index.md",
            &Scope::parse("label:old").unwrap(),
            true,
            "h",
        )
        .unwrap();
    store
        .attach("keep/y.md", &Scope::parse("label:old").unwrap(), true, "h")
        .unwrap();
    store
        .attach("old/docs", &Scope::parse("owner:bot:k").unwrap(), true, "h")
        .unwrap();
    store
        .attach("older/x.md", &Scope::parse("global").unwrap(), true, "h")
        .unwrap();
    (c, store, other)
}

fn req<'a>(from: &'a str, to: &'a str, dry_run: bool) -> Request<'a> {
    Request {
        from,
        to,
        why: "clearer",
        owner_ref: None,
        dry_run,
    }
}

fn n(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows still naming `name`, over every column a rename moves.
fn left(c: &Connection, name: &str) -> i64 {
    let q = |sql: &str| -> i64 { c.query_row(sql, [name], |r| r.get(0)).unwrap() };
    q("SELECT COUNT(*) FROM tasks WHERE project=?1")
        + q("SELECT COUNT(*) FROM tasks WHERE labels LIKE '%\"' || ?1 || '\"%'")
        + q("SELECT COUNT(*) FROM runs WHERE project=?1")
        + q("SELECT COUNT(*) FROM bots WHERE project=?1")
        + q("SELECT COUNT(*) FROM bot_subscriptions WHERE project=?1")
        + q("SELECT COUNT(*) FROM bot_feed WHERE project=?1")
        + q("SELECT COUNT(*) FROM goals WHERE project=?1")
        + q("SELECT COUNT(*) FROM routines WHERE config LIKE '%\"' || ?1 || '\"%'")
        + q("SELECT COUNT(*) FROM routine_occurrences WHERE snapshot LIKE '%\"' || ?1 || '\"%'")
        + q("SELECT COUNT(*) FROM rules WHERE matcher LIKE '%\"' || ?1 || '\"%'")
}

#[test]
fn rename_moves_every_keyed_row_and_the_docs() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, store, other) = fixture(dir.path());
    let human = Actor::new("human");

    let plan = project::rename(&mut c, &human, &req("old", "new", true), Some(&store)).unwrap();
    assert!(plan.dry_run);
    let want: Value = json!({
        "tasks": 2, "tasks.labels": 2, "runs": 1, "bots": 1, "bot_subscriptions": 1,
        "bot_feed": 1, "goals": 1, "routines": 1, "routine_occurrences": 1, "rules": 1,
    });
    assert_eq!(serde_json::to_value(&plan.tables).unwrap(), want);
    let docs = plan.docs.clone().unwrap();
    assert_eq!(docs.docs, ["old/docs/index.md", "old/docs/ops/run.md"]);
    let moved: Vec<(String, String)> = docs
        .attachments
        .iter()
        .map(|a| (a.scope.clone(), a.path.clone()))
        .collect();
    assert_eq!(
        moved,
        [
            ("label:old".to_string(), "old/docs/index.md".to_string()),
            ("owner:bot:k".to_string(), "old/docs".to_string()),
        ]
    );
    assert_eq!(docs.label_attachments, 2);
    assert_eq!(docs.version, None);
    // The dry run changed nothing.
    assert_eq!(left(&c, "old"), 12);
    assert_eq!(left(&c, "new"), 0);
    assert!(store.version("old/docs/index.md").unwrap().is_some());

    let done = project::rename(&mut c, &human, &req("old", "new", false), Some(&store)).unwrap();
    assert!(!done.dry_run);
    assert_eq!(done.tables, plan.tables, "dry-run counts match the rename");
    assert_eq!(left(&c, "old"), 0);
    assert_eq!(left(&c, "new"), 12);
    // Other projects stay; the other project's label moved.
    assert_eq!(n(&c, "SELECT COUNT(*) FROM tasks WHERE project='keep'"), 2);
    assert_eq!(n(&c, "SELECT COUNT(*) FROM bots WHERE project='keep'"), 1);
    assert_eq!(
        n(
            &c,
            "SELECT COUNT(*) FROM bot_subscriptions WHERE project='keep'"
        ),
        1
    );
    assert_eq!(db::get_task(&c, other).unwrap().labels, ["new"]);
    let t1 = db::get_task(&c, 1).unwrap();
    assert_eq!(
        (t1.project.as_str(), t1.labels.clone()),
        ("new", vec!["new".to_string(), "x".to_string()])
    );
    let e = db::events(&c, 1).unwrap().pop().unwrap();
    assert_eq!(
        (
            e.kind.as_str(),
            e.from_value.as_deref(),
            e.to_value.as_deref()
        ),
        ("project", Some("old"), Some("new"))
    );
    assert_eq!(
        n(&c, "SELECT COUNT(*) FROM bot_events WHERE kind='project'"),
        1
    );
    assert_eq!(
        n(&c, "SELECT COUNT(*) FROM goal_events WHERE kind='project'"),
        1
    );
    assert_eq!(
        n(
            &c,
            "SELECT COUNT(*) FROM routine_events WHERE kind='project-renamed'"
        ),
        1
    );
    assert_eq!(
        n(
            &c,
            "SELECT COUNT(*) FROM rule_events WHERE kind='project-renamed'"
        ),
        1
    );

    // Docs: moved with their content; history stays at the old path.
    let docs = done.docs.unwrap();
    assert!(docs.version.is_some());
    assert!(store.version("old/docs/index.md").unwrap().is_none());
    let got = store.get("new/docs/ops/run.md", None).unwrap();
    assert!(
        format!("{got:?}").contains("old/docs/ops/run.md"),
        "{got:?}"
    );
    assert!(store.version("older/x.md").unwrap().is_some());
    let att = |s: &str| -> Vec<String> {
        store
            .attachments(Some(&Scope::parse(s).unwrap()))
            .unwrap()
            .into_iter()
            .map(|a| {
                assert!(a.exists, "{a:?}");
                a.path
            })
            .collect()
    };
    assert!(att("label:old").is_empty());
    assert_eq!(att("label:new"), ["keep/y.md", "new/docs/index.md"]);
    assert_eq!(att("owner:bot:k"), ["new/docs"]);
    assert_eq!(att("global"), ["older/x.md"]);
}

#[test]
fn rename_refusals_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, store, _) = fixture(dir.path());
    let human = Actor::new("human");
    let mut err = |r: Request, actor: &Actor| -> String {
        format!(
            "{:#}",
            project::rename(&mut c, actor, &r, Some(&store)).unwrap_err()
        )
    };
    // NEW in use as a project, a label, or a doc folder.
    assert!(err(req("old", "keep", false), &human).contains("project keep already exists"));
    assert!(err(req("old", "x", false), &human).contains("project x already exists"));
    assert!(err(req("old", "older", false), &human).contains("docs under older/ already exist"));
    assert!(err(req("old", "old", false), &human).contains("already has that name"));
    assert!(err(req("nope", "new", false), &human).contains("project nope not found"));
    assert!(err(req("old", "bad name", false), &human).contains("invalid project"));
    assert!(err(req("old", "new", false), &Actor::new("bot:b")).contains("--owner-ref"));
    let mut no_why = req("old", "new", false);
    no_why.why = " ";
    assert!(err(no_why, &human).contains("--why"));
    drop(err);

    // A task in progress, then a live bot run.
    let t = task(&mut c, "old", "pending", &[]);
    c.execute("UPDATE tasks SET state='in_progress' WHERE id=?1", [t])
        .unwrap();
    let e = project::rename(&mut c, &human, &req("old", "new", true), Some(&store)).unwrap_err();
    assert!(format!("{e:#}").contains("1 task(s) in progress"), "{e:#}");
    c.execute("UPDATE tasks SET state='pending' WHERE id=?1", [t])
        .unwrap();
    exec(
        &c,
        "INSERT INTO bot_runs (bot_id, agent, mode, claim, dispatcher, generation, started_at)
         VALUES (1, 'a', 'fresh', 'c', 'd', 'g', ?1)",
        [NOW],
    );
    let e = project::rename(&mut c, &human, &req("old", "new", false), Some(&store)).unwrap_err();
    assert!(format!("{e:#}").contains("1 live run(s)"), "{e:#}");

    assert_eq!(left(&c, "new"), 0);
    assert!(store.version("old/docs/index.md").unwrap().is_some());
    assert!(store.version("new/docs/index.md").unwrap().is_none());
}

/// The CLI and route: dry run, then the rename, as the owner.
#[test]
fn cli_dry_run_then_rename() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "old", "--title", "a", "--label", "old"]);
    srv.ok(&["add", "--project", "old", "--title", "b"]);
    let note = dir.path().join("n.md");
    std::fs::write(&note, "hi\n").unwrap();
    srv.ok(&["doc", "put", "old/docs/n.md", note.to_str().unwrap()]);
    srv.ok(&["doc", "attach", "old/docs/n.md", "label:old"]);

    let rename = |extra: &[&str]| {
        let mut a = vec!["project", "rename", "old", "new", "--why", "clearer"];
        a.extend_from_slice(extra);
        srv.ok(&a)
    };
    let plan: Value = serde_json::from_str(&rename(&["--dry-run", "--json"])).unwrap();
    let r = &plan["renamed"];
    assert_eq!(r["dry_run"], true);
    assert_eq!(r["tables"], json!({"tasks": 2, "tasks.labels": 1}));
    assert_eq!(r["docs"]["docs"], json!(["old/docs/n.md"]));
    assert_eq!(r["docs"]["label_attachments"], 1);
    assert!(srv.ok(&["list", "--project", "new"]).lines().count() <= 1);

    let out = rename(&[]);
    assert!(out.contains("project old renamed to new"), "{out}");
    assert!(out.contains("tasks                  2"), "{out}");
    let listed = srv.ok(&["list", "--project", "new"]);
    assert_eq!(listed.lines().count(), 3, "{listed}");
    assert_eq!(srv.ok(&["doc", "get", "new/docs/n.md"]), "hi\n");
    assert!(srv
        .ok(&["doc", "ls", "--scope", "label:new"])
        .contains("new/docs/n.md"));

    // Again: the old project is gone, the new one exists.
    let out = srv.run(&["project", "rename", "old", "new", "--why", "w"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already exists"), "{err}");
}
