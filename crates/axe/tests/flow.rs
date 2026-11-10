use axe::db::{self, Actor, Changes, NewTask};
use axe::flow::{self, Query};
use rusqlite::{params, Connection};
use serde_json::Value;

const OWNER_NOTE: &str = "blocked: x\nresolver: owner: decide\nresume: y";
const BOT_NOTE: &str = "blocked: x\nresolver: axe-os: retry\nresume: y";

fn open() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let c = db::open(&dir.path().join("db")).unwrap();
    (dir, c)
}
fn who(n: &str) -> Actor {
    Actor::new(n)
}
fn add(c: &mut Connection, deps: &[i64], workload: Option<&str>) -> i64 {
    let t = NewTask {
        agent: None,
        project: "p",
        title: "t",
        detail: "",
        state: "pending",
    };
    db::add_classified_task(c, &who("bot:axe-os"), &t, deps, workload).unwrap()
}
fn state(c: &mut Connection, actor: &str, id: i64, s: &str) {
    let ch = Changes {
        state: Some(s.into()),
        ..Default::default()
    };
    db::set_task(c, &who(actor), id, &ch).unwrap();
}
fn escalate(c: &mut Connection, actor: &str, id: i64, note: &str, class: Option<&str>) {
    let ch = Changes {
        state: Some("escalated".into()),
        note: Some(note.into()),
        escalation_class: class.map(Into::into),
        ..Default::default()
    };
    db::set_task(c, &who(actor), id, &ch).unwrap();
}
fn ship(c: &mut Connection, id: i64) {
    db::append(c, &who("agent"), id, "artifact", "https://pr").unwrap();
    state(c, "agent", id, "done");
}
fn stats(c: &Connection, workload: Option<&str>) -> Value {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    flow::stats(
        c,
        &Query {
            project: "p",
            since: now - 3600,
            until: now + 60,
            workload,
        },
    )
    .unwrap()
}

#[test]
fn an_approved_owner_escalation_is_one_intervention() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    escalate(&mut c, "agent", id, OWNER_NOTE, Some("decision"));
    db::approve(&mut c, &who("human"), id, "abc", "main", None).unwrap();
    ship(&mut c, id);
    let o = &stats(&c, None)["overall"];
    assert_eq!(o["interventions"], 1);
    assert_eq!(o["verified_shipped"], 1);
    assert_eq!(o["interventions_per_verified_shipped"], 1.0);
    assert_eq!(o["escalations_by_class"]["decision"]["autonomous"], 0);
}

#[test]
fn a_bot_approval_still_counts_as_the_owners_intervention() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    escalate(&mut c, "agent", id, BOT_NOTE, Some("decision"));
    db::approve(
        &mut c,
        &who("bot:axe-os"),
        id,
        "abc",
        "main",
        Some("owner msg #1"),
    )
    .unwrap();
    assert_eq!(stats(&c, None)["overall"]["interventions"], 1);
}

#[test]
fn bot_resolution_is_autonomous_and_classification_latest_wins() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    escalate(&mut c, "agent", id, BOT_NOTE, Some("capability"));
    state(&mut c, "bot:axe-os", id, "pending");
    db::classify(
        &mut c,
        &who("bot:axe-os"),
        id,
        Some("coordination"),
        None,
        None,
    )
    .unwrap();
    let o = &stats(&c, None)["overall"];
    assert_eq!(o["interventions"], 0);
    assert_eq!(o["escalations"]["autonomous_rate"], 1.0);
    assert!(o["escalations_by_class"].get("capability").is_none());
    assert_eq!(o["escalations_by_class"]["coordination"]["resolved"], 1);
}

#[test]
fn classify_reaches_an_earlier_closed_episode_and_still_the_latest() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    // Two closed episodes, both escalated without a class.
    for _ in 0..2 {
        escalate(&mut c, "agent", id, BOT_NOTE, None);
        state(&mut c, "bot:axe-os", id, "pending");
    }
    let by_class = |c: &Connection| stats(c, None)["overall"]["escalations_by_class"].clone();
    assert_eq!(by_class(&c)["unclassified"]["resolved"], 2);

    // Without an episode the latest closed episode is classified, as before.
    db::classify(&mut c, &who("bot:axe-os"), id, Some("decision"), None, None).unwrap();
    let b = by_class(&c);
    assert_eq!(b["decision"]["resolved"], 1);
    assert_eq!(b["unclassified"]["resolved"], 1);

    // The earlier episode can be classified explicitly.
    db::classify(
        &mut c,
        &who("bot:axe-os"),
        id,
        Some("runtime"),
        None,
        Some(1),
    )
    .unwrap();
    let b = by_class(&c);
    assert!(b.get("unclassified").is_none());
    assert_eq!(b["runtime"]["resolved"], 1);
    assert_eq!(b["decision"]["resolved"], 1);

    // Naming the latest episode explicitly works too; the latest event wins.
    db::classify(
        &mut c,
        &who("bot:axe-os"),
        id,
        Some("capability"),
        None,
        Some(2),
    )
    .unwrap();
    let b = by_class(&c);
    assert_eq!(b["capability"]["resolved"], 1);
    assert_eq!(b["runtime"]["resolved"], 1);
    assert!(b.get("decision").is_none());

    // A third, open episode: the default targets it, not the closed ones.
    escalate(&mut c, "agent", id, BOT_NOTE, None);
    db::classify(
        &mut c,
        &who("bot:axe-os"),
        id,
        Some("coordination"),
        None,
        None,
    )
    .unwrap();
    let b = by_class(&c);
    assert_eq!(b["coordination"]["opened"], 1);
    assert_eq!(b["capability"]["resolved"], 1);
    assert_eq!(b["runtime"]["resolved"], 1);

    // Out-of-range episodes and an episode without a class are rejected.
    for bad in [0, 4] {
        assert!(db::classify(&mut c, &who("agent"), id, Some("runtime"), None, Some(bad)).is_err());
    }
    assert!(db::classify(&mut c, &who("agent"), id, None, Some("ops"), Some(1)).is_err());
}

#[test]
fn repeat_escalations_are_separate_with_latency_percentiles() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    for _ in 0..2 {
        escalate(&mut c, "agent", id, BOT_NOTE, Some("runtime"));
        state(&mut c, "bot:axe-os", id, "pending");
    }
    let base = time::OffsetDateTime::now_utc().unix_timestamp() - 3000;
    let stamp = |t: i64| {
        time::OffsetDateTime::from_unix_timestamp(t)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    let ids: Vec<i64> = c
        .prepare("SELECT id FROM events WHERE kind='state' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for (id, t) in ids.iter().zip([0, 100, 200, 600]) {
        c.execute(
            "UPDATE events SET at=?1 WHERE id=?2",
            params![stamp(base + t), id],
        )
        .unwrap();
    }
    let e = &stats(&c, None)["overall"]["escalations_by_class"]["runtime"];
    assert_eq!(e["resolved"], 2);
    assert_eq!(e["latency_p50_seconds"], 100);
    assert_eq!(e["latency_p90_seconds"], 400);
}

#[test]
fn reopened_and_artifactless_tasks_are_not_shipped() {
    let (_d, mut c) = open();
    let reopened = add(&mut c, &[], None);
    ship(&mut c, reopened);
    state(&mut c, "agent", reopened, "pending");
    let bare = add(&mut c, &[], None);
    state(&mut c, "agent", bare, "done");
    let o = &stats(&c, None)["overall"];
    assert_eq!(o["verified_shipped"], 0);
    assert_eq!(o["reopened"], 1);
    assert_eq!(o["done_transitions"], 2);
    assert_eq!(o["reopen_rate"], 0.5);
    assert!(o["interventions_per_verified_shipped"].is_null());
}

#[test]
fn human_state_and_spec_edits_are_interventions() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    let ch = Changes {
        detail: Some("new spec".into()),
        ..Default::default()
    };
    db::set_task(&mut c, &who("human"), id, &ch).unwrap();
    state(&mut c, "human", id, "blocked");
    assert_eq!(stats(&c, None)["overall"]["interventions"], 2);
}

#[test]
fn backlog_excludes_dependency_and_timed_holds() {
    let (_d, mut c) = open();
    let prereq = add(&mut c, &[], None);
    state(&mut c, "agent", prereq, "blocked");
    add(&mut c, &[prereq], None);
    let sleeping = add(&mut c, &[], None);
    let ch = Changes {
        not_before: Some("+1d".into()),
        ..Default::default()
    };
    db::set_task(&mut c, &who("agent"), sleeping, &ch).unwrap();
    add(&mut c, &[], None);
    assert_eq!(stats(&c, None)["overall"]["backlog"]["runnable"], 1);
}

#[test]
fn run_effort_has_no_read_cap() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    ship(&mut c, id);
    for _ in 0..1200 {
        c.execute(
            "INSERT INTO runs (task_id, project, agent, mode, claim, started_at, ended_at, log_path)
             VALUES (?1, 'p', 'claude', 'fresh', 'c', '2026-11-01T00:00:00Z', '2026-11-01T00:00:10Z', 'l')",
            [id],
        )
        .unwrap();
    }
    let o = &stats(&c, None)["overall"];
    assert_eq!(o["runs_per_shipped"], 1200.0);
    assert_eq!(o["run_seconds_per_shipped"], 12000.0);
}

#[test]
fn workloads_group_and_filter() {
    let (_d, mut c) = open();
    let bug = add(&mut c, &[], Some("infra"));
    db::classify(
        &mut c,
        &who("bot:axe-os"),
        bug,
        None,
        Some("prod-bug"),
        None,
    )
    .unwrap();
    ship(&mut c, bug);
    let other = add(&mut c, &[], None);
    ship(&mut c, other);
    let s = stats(&c, None);
    assert_eq!(s["overall"]["verified_shipped"], 2);
    assert_eq!(s["workloads"]["prod-bug"]["verified_shipped"], 1);
    assert_eq!(s["workloads"]["unset"]["verified_shipped"], 1);
    assert!(s["workloads"].get("infra").is_none());
    let s = stats(&c, Some("prod-bug"));
    assert_eq!(s["overall"]["verified_shipped"], 1);
}

#[test]
fn the_dispatcher_records_runtime_and_classes_are_validated() {
    let (_d, mut c) = open();
    let id = add(&mut c, &[], None);
    escalate(&mut c, "dispatcher", id, BOT_NOTE, None);
    let o = stats(&c, None);
    assert_eq!(o["overall"]["escalations_by_class"]["runtime"]["opened"], 1);

    let id = add(&mut c, &[], None);
    let bad = Changes {
        state: Some("escalated".into()),
        escalation_class: Some("nope".into()),
        ..Default::default()
    };
    assert!(db::set_task(&mut c, &who("agent"), id, &bad).is_err());
    let not_escalating = Changes {
        escalation_class: Some("runtime".into()),
        ..Default::default()
    };
    assert!(db::set_task(&mut c, &who("agent"), id, &not_escalating).is_err());
    assert!(db::classify(&mut c, &who("agent"), id, Some("runtime"), None, None).is_err());
}

#[test]
fn escalate_requires_a_class() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_axe"))
        .args([
            "escalate",
            "1",
            "--blocked",
            "a",
            "--resolver",
            "b",
            "--resume",
            "c",
        ])
        .env("AXE_RETRY_SECS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--class"));
}
