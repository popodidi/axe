mod common;
use axe::{
    db::{self, Actor},
    routine::{self, Template},
};
use serde_json::{json, Value};
fn template() -> Template {
    Template {
        agent: None,
        name: "Acceptance".into(),
        project: "orchid".into(),
        cron: "* * * * *".into(),
        timezone: "UTC".into(),
        title: "Record acceptance".into(),
        detail: "## Outcome\nRecord acceptance only.\n## Verify\nRead the note.".into(),
        workload: None,
        owner: None,
        repos: None,
        labels: vec![],
    }
}
fn at(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
}
#[test]
fn coalesce_restart_pause_edit_archive_and_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut c = db::open(&path).unwrap();
    let a = Actor::new("test");
    let now = at("2026-01-01T00:00:00Z");
    let mut t = template();
    let id = routine::save(&mut c, &a, None, &t, now).unwrap();
    assert_eq!(routine::get(&c, id).unwrap().next_at, now + 60);
    assert_eq!(routine::tick(&mut c, now).unwrap(), 0);
    assert_eq!(routine::tick(&mut c, now + 601).unwrap(), 1);
    let h = routine::history(&c, id, i64::MAX).unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0]["scheduled_at"], now + 600);
    let task = h[0]["task_id"].as_i64().unwrap();
    assert_eq!(
        routine::backlink(&c, task).unwrap().unwrap()["routine_id"],
        id
    );
    drop(c);
    let mut c = db::open(&path).unwrap();
    assert_eq!(routine::tick(&mut c, now + 601).unwrap(), 0);
    // Overlap is intentional: prior task remains pending.
    assert_eq!(routine::tick(&mut c, now + 660).unwrap(), 1);
    let next = routine::get(&c, id).unwrap().next_at;
    let manual = routine::trigger(&mut c, &a, id, "retry-1").unwrap();
    assert_eq!(routine::trigger(&mut c, &a, id, "retry-1").unwrap(), manual);
    assert_eq!(routine::get(&c, id).unwrap().next_at, next);
    routine::change(&mut c, &a, id, "pause", now + 661).unwrap();
    assert_eq!(routine::tick(&mut c, now + 1200).unwrap(), 0);
    routine::change(&mut c, &a, id, "resume", now + 1200).unwrap();
    assert_eq!(routine::get(&c, id).unwrap().next_at, now + 1260);
    t.title = "Edited future task".into();
    routine::save(&mut c, &a, Some(id), &t, now + 1201).unwrap();
    assert_eq!(routine::get(&c, id).unwrap().next_at, now + 1260);
    assert_eq!(db::get_task(&c, task).unwrap().title, "Record acceptance");
    assert_eq!(
        routine::backlink(&c, task).unwrap().unwrap()["snapshot"]["title"],
        "Record acceptance"
    );
    t.cron = "0 * * * *".into();
    routine::save(&mut c, &a, Some(id), &t, now + 1202).unwrap();
    assert_eq!(routine::get(&c, id).unwrap().next_at, now + 3600);
    routine::change(&mut c, &a, id, "archive", now + 1203).unwrap();
    assert_eq!(routine::tick(&mut c, now + 7200).unwrap(), 0);
    assert!(routine::trigger(&mut c, &a, id, "new").is_err());
    assert!(routine::change(&mut c, &a, id, "resume", now).is_err());
    assert_eq!(routine::trigger(&mut c, &a, id, "retry-1").unwrap(), manual);
    assert_eq!(routine::history(&c, id, i64::MAX).unwrap().len(), 3);
    assert!(!routine::events(&c, id, i64::MAX).unwrap().is_empty());
}
#[test]
fn atomic_failure_and_concurrent_ticks() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let mut c = db::open(&path).unwrap();
    let a = Actor::new("test");
    let id = routine::save(&mut c, &a, None, &template(), 0).unwrap();
    c.execute_batch("CREATE TRIGGER fail_occurrence BEFORE INSERT ON routine_occurrences BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(routine::tick(&mut c, 60).is_err());
    assert_eq!(
        c.query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(routine::get(&c, id).unwrap().next_at, 60);
    c.execute_batch("DROP TRIGGER fail_occurrence").unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || routine::tick(&mut db::open(&path).unwrap(), 120).unwrap())
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .sum::<usize>(),
        1
    );
    assert_eq!(routine::history(&c, id, i64::MAX).unwrap().len(), 1);
}
#[test]
fn validation_and_dst() {
    let mut t = template();
    for cron in [
        "* * * *",
        "* * * * * *",
        "60 * * * *",
        "@daily",
        "0 0 30 2 *",
    ] {
        t.cron = cron.into();
        assert!(t.next(0).is_err(), "{cron}");
    }
    t.cron = "30 2 * * *".into();
    t.timezone = "Not/AZone".into();
    assert!(t.next(0).is_err());
    t.timezone = "Europe/Stockholm".into();
    // Croner's fixed-time gap policy: first valid instant after the spring gap.
    assert_eq!(
        t.next(at("2026-03-29T00:59:59Z")).unwrap(),
        at("2026-03-29T01:00:00Z")
    );
    assert_eq!(
        t.next(at("2026-10-25T00:00:00Z")).unwrap(),
        at("2026-10-25T00:30:00Z")
    );
    assert_eq!(
        t.next(at("2026-10-25T00:30:00Z")).unwrap(),
        at("2026-10-26T01:30:00Z")
    );
    t.cron = "*/15 * * * *".into();
    assert_eq!(
        t.next(at("2026-10-25T00:45:00Z")).unwrap(),
        at("2026-10-25T01:00:00Z")
    );
}
#[test]
fn migration_preserves_all_existing_rows() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let c = db::open(&path).unwrap();
    c.execute_batch("ALTER TABLE tasks DROP COLUMN agent; ALTER TABLE tasks DROP COLUMN owner; ALTER TABLE tasks DROP COLUMN repos; ALTER TABLE tasks DROP COLUMN labels; DROP TABLE routine_events; DROP TABLE routine_occurrences; DROP TABLE routines; PRAGMA user_version=9;
 INSERT INTO tasks VALUES(7,'orchid','active','spec','in_progress','dispatch:x',1,'a','b',NULL);
 INSERT INTO sessions VALUES(7,'codex','session',2);
 INSERT INTO runs VALUES(3,7,'orchid','codex','fresh','session','dispatch:x','a',NULL,NULL,'log',NULL,NULL);
 INSERT INTO events VALUES(5,7,'a','actor','session','created',NULL,'active','body');").unwrap();
    fn rows(c: &rusqlite::Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let cols = if table == "tasks" {
            "id,project,title,detail,state,claimed_by,runs,created_at,updated_at,not_before"
        } else {
            "*"
        };
        let mut q = c.prepare(&format!("SELECT {cols} FROM {table}")).unwrap();
        let n = q.column_count();
        let result = q
            .query_map([], |r| (0..n).map(|i| r.get(i)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        result
    }
    let tables = ["tasks", "sessions", "runs", "events"];
    let before: Vec<_> = tables.iter().map(|t| rows(&c, t)).collect();
    drop(c);
    let c = db::open(&path).unwrap();
    for (i, t) in tables.iter().enumerate() {
        assert_eq!(before[i], rows(&c, t));
    }
    assert_eq!(
        c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        18
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        0
    );
}
#[test]
fn authenticated_api_and_cli() {
    let d = tempfile::tempdir().unwrap();
    let s = common::Server::start(d.path());
    let file = d.path().join("template.json");
    std::fs::write(&file, serde_json::to_vec(&template()).unwrap()).unwrap();
    let no =
        ureq::post(&format!("{}/api/routines", s.url)).send_json(json!({"template":template()}));
    assert!(no.unwrap_err().to_string().contains("401"));
    let r: Value =
        serde_json::from_str(&s.ok(&["routine", "save", "--file", file.to_str().unwrap()]))
            .unwrap();
    let id = r["id"].to_string();
    let r: Value =
        serde_json::from_str(&s.ok(&["routine", "trigger", &id, "--key", "acceptance"])).unwrap();
    assert!(r["task_id"].is_number());
    for action in [
        "list", "show", "history", "events", "pause", "resume", "archive",
    ] {
        let mut args = vec!["routine", action];
        if action != "list" {
            args.push(&id);
        }
        s.ok(&args);
    }
}
#[test]
fn template_sets_owner_repos_and_labels() {
    let d = tempfile::tempdir().unwrap();
    let mut c = db::open(&d.path().join("db")).unwrap();
    let a = Actor::new("test");
    // An existing template without the fields stores and creates as before.
    let plain = routine::save(&mut c, &a, None, &template(), 0).unwrap();
    let stored: String = c
        .query_row("SELECT config FROM routines WHERE id=?1", [plain], |r| {
            r.get(0)
        })
        .unwrap();
    let stored: Value = serde_json::from_str(&stored).unwrap();
    for k in ["owner", "repos", "labels"] {
        assert!(stored.get(k).is_none(), "{k} stored for a plain template");
    }
    let task = routine::trigger(&mut c, &a, plain, "k").unwrap();
    let task = db::get_task(&c, task).unwrap();
    assert_eq!((task.owner, task.repos, task.labels), (None, None, vec![]));
    // A template with the fields creates tasks carrying them, with the same
    // events `axe add` writes.
    let mut t = template();
    t.owner = Some("bot:ops".into());
    t.repos = Some(vec!["axe".into(), "web".into(), "axe".into()]);
    t.labels = vec!["infra".into(), "nightly".into()];
    let id = routine::save(&mut c, &a, None, &t, 0).unwrap();
    for task in [routine::trigger(&mut c, &a, id, "k").unwrap(), {
        routine::tick(&mut c, 120).unwrap();
        routine::history(&c, id, i64::MAX).unwrap()[0]["task_id"]
            .as_i64()
            .unwrap()
    }] {
        let got = db::get_task(&c, task).unwrap();
        assert_eq!(got.owner.as_deref(), Some("bot:ops"));
        assert_eq!(got.repos, Some(vec!["axe".to_string(), "web".to_string()]));
        assert_eq!(got.labels, vec!["infra", "nightly"]);
        let kinds: Vec<String> = c
            .prepare("SELECT kind FROM events WHERE task_id=?1 ORDER BY id")
            .unwrap()
            .query_map([task], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(kinds, ["created", "owner", "repos", "labels"]);
    }
    // repos: [] is a run workspace with no repos, as `axe add --no-repos`.
    t.repos = Some(vec![]);
    routine::save(&mut c, &a, Some(id), &t, 0).unwrap();
    let task = routine::trigger(&mut c, &a, id, "empty").unwrap();
    assert_eq!(db::get_task(&c, task).unwrap().repos, Some(vec![]));
    // Invalid fields are rejected at save, as `axe add` rejects them.
    for bad in [
        Template {
            owner: Some("two words".into()),
            ..template()
        },
        Template {
            owner: Some("none".into()),
            ..template()
        },
        Template {
            repos: Some(vec!["bad name".into()]),
            ..template()
        },
        Template {
            labels: vec!["".into()],
            ..template()
        },
    ] {
        assert!(routine::save(&mut c, &a, None, &bad, 0).is_err());
    }
    // The JSON shape `axe routine save --file` reads.
    let parsed: Template = serde_json::from_value(json!({
        "name": "n", "project": "p", "cron": "* * * * *", "title": "t",
        "detail": "d", "owner": "o", "repos": ["axe"], "labels": ["l"]
    }))
    .unwrap();
    assert_eq!(parsed.repos, Some(vec!["axe".to_string()]));
}
