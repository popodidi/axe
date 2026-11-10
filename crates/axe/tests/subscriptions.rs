//! Bot task-event subscriptions: batching, ride-alongs, validation and
//! durability. Fan-out stamps real time, so tests claim at `epoch() + offset`.
use axe::bot::{self, NewBot};
use axe::db::{self, Actor, Changes, NewTask};
use axe::remote::epoch;
use axe::subscription::{self as sub, Change};
use rusqlite::Connection;

const BATCH: i64 = 300;
const PAST_BATCH: i64 = BATCH + 2;

fn who() -> Actor {
    Actor::new("agent")
}

/// A bot on project `a` whose creation wake has already been handled.
fn rig() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut c = db::open(&path).unwrap();
    let b = NewBot {
        name: "b".into(),
        project: "a".into(),
        agent: None,
        identity: "i".into(),
        conduct: "c".into(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    };
    bot::create(&mut c, &who(), &b, epoch()).unwrap();
    let first = claim(&mut c, epoch()).expect("creation run");
    done(&c, first.run);
    (dir, path, c)
}

fn claim(c: &mut Connection, now: i64) -> Option<bot::Claimed> {
    let tx = c.transaction().unwrap();
    let out = bot::claim(&tx, "b", &format!("bot:{now}"), "d", "g", now).unwrap();
    tx.commit().unwrap();
    out
}

fn done(c: &Connection, run: i64) {
    bot::finish(c, run, Some(0), "exit", Some("s"), None, epoch()).unwrap();
}

fn subscribe_default(c: &mut Connection) {
    sub::subscribe(c, &who(), "b", &Change::default(), "test", epoch()).unwrap();
}

fn add(c: &mut Connection, project: &str, actor: &Actor) -> i64 {
    let t = NewTask {
        agent: None,
        project,
        title: "a   task\ntitle",
        detail: "",
        state: "pending",
    };
    db::add_task(c, actor, &t).unwrap()
}

fn set(c: &mut Connection, id: i64, state: &str, actor: &Actor) {
    let ch = Changes {
        state: Some(state.into()),
        ..Default::default()
    };
    assert!(db::set_task(c, actor, id, &ch).unwrap());
}

fn escalated(c: &mut Connection, project: &str) -> i64 {
    let id = add(c, project, &who());
    set(c, id, "escalated", &who());
    id
}

fn ready(c: &Connection, offset: i64) -> bool {
    !bot::ready(c, epoch() + offset).unwrap().is_empty()
}

fn bodies(c: &bot::Claimed) -> String {
    c.wakes
        .iter()
        .map(|w| w.body.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn batches(c: &bot::Claimed) -> usize {
    c.wakes
        .iter()
        .filter(|w| w.body.starts_with("Subscribed task events"))
        .count()
}

#[test]
fn off_by_default() {
    let (_d, _p, mut c) = rig();
    escalated(&mut c, "a");
    assert!(!ready(&c, PAST_BATCH));
    assert!(sub::list(&c, "b").unwrap().is_empty());
}

#[test]
fn default_subscription_has_the_agreed_settings_and_is_audited() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let s = &sub::list(&c, "b").unwrap()[0];
    assert_eq!(s.project, "a");
    assert_eq!(s.wake, ["escalated"]);
    assert_eq!(s.ride, ["new", "done"]);
    assert_eq!(s.batch_seconds, 300);
    let ev = bot::events(&c, "b", 5).unwrap();
    assert!(
        ev.iter()
            .any(|e| e.kind == "subscribe" && e.body.contains("\"why\":\"test\"")),
        "{ev:?}"
    );
    sub::unsubscribe(&mut c, &who(), "b", None, "bye").unwrap();
    assert!(sub::list(&c, "b").unwrap().is_empty());
    let ev = bot::events(&c, "b", 5).unwrap();
    assert!(ev.iter().any(|e| e.kind == "unsubscribe"), "{ev:?}");
}

#[test]
fn three_escalations_in_a_window_are_one_run_listing_all() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let ids: Vec<i64> = (0..3).map(|_| escalated(&mut c, "a")).collect();
    assert!(!ready(&c, BATCH - 5), "not due inside the window");
    let run = claim(&mut c, epoch() + PAST_BATCH).expect("one run");
    let body = bodies(&run);
    for id in ids {
        assert!(
            body.contains(&format!("#{id} a escalated: a task title")),
            "{body}"
        );
    }
    done(&c, run.run);
    assert!(!ready(&c, PAST_BATCH), "exactly one run");
}

#[test]
fn several_projects_share_one_batch_and_an_event_is_delivered_once() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let all = Change {
        project: Some(sub::ALL_PROJECTS.into()),
        ..Default::default()
    };
    sub::subscribe(&mut c, &who(), "b", &all, "all", epoch()).unwrap();
    let in_a = escalated(&mut c, "a");
    let in_z = escalated(&mut c, "z");
    let run = claim(&mut c, epoch() + PAST_BATCH).expect("run");
    assert_eq!(batches(&run), 1, "{:?}", run.wakes);
    let body = bodies(&run);
    assert_eq!(
        body.matches(&format!("#{in_a} a escalated")).count(),
        1,
        "{body}"
    );
    assert_eq!(
        body.matches(&format!("#{in_z} z escalated")).count(),
        1,
        "{body}"
    );
}

#[test]
fn a_task_created_escalated_counts_as_an_escalation() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let t = NewTask {
        agent: None,
        project: "a",
        title: "held from birth",
        detail: "",
        state: "escalated",
    };
    let id = db::add_task(&mut c, &who(), &t).unwrap();
    let run = claim(&mut c, epoch() + PAST_BATCH).expect("wakes");
    let body = bodies(&run);
    assert!(body.contains(&format!("#{id} a escalated")), "{body}");
}

#[test]
fn changed_batch_and_event_set_take_effect() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let change = Change {
        wake: Some(vec!["done".into()]),
        ride: Some(vec![]),
        batch_seconds: Some(60),
        ..Default::default()
    };
    let s = sub::subscribe(&mut c, &who(), "b", &change, "tune", epoch()).unwrap();
    assert_eq!(
        (s.wake.as_slice(), s.batch_seconds),
        (&["done".to_string()][..], 60)
    );
    assert!(s.ride.is_empty());
    let id = escalated(&mut c, "a");
    assert!(!ready(&c, 3600), "escalated no longer wakes");
    set(&mut c, id, "done", &who());
    assert!(!ready(&c, 50));
    assert!(ready(&c, 62), "60s batch applies");
}

#[test]
fn unchanged_fields_are_kept_on_a_partial_update() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let change = Change {
        batch_seconds: Some(600),
        ..Default::default()
    };
    let s = sub::subscribe(&mut c, &who(), "b", &change, "slower", epoch()).unwrap();
    assert_eq!(s.wake, ["escalated"]);
    assert_eq!(s.ride, ["new", "done"]);
}

#[test]
fn ride_along_never_causes_a_run_and_appears_once() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let id = add(&mut c, "a", &who());
    set(&mut c, id, "done", &who());
    assert!(!ready(&c, 3600), "ride-along must not wake");
    bot::add_wake(&mut c, &who(), "b", epoch(), "ping", None, epoch()).unwrap();
    let run = claim(&mut c, epoch() + 1).expect("run");
    let body = bodies(&run);
    assert!(body.contains(&format!("new 1 (#{id})")), "{body}");
    assert!(body.contains(&format!("done 1 (#{id})")), "{body}");
    done(&c, run.run);
    bot::add_wake(&mut c, &who(), "b", epoch(), "ping2", None, epoch()).unwrap();
    let next = claim(&mut c, epoch() + 1).expect("run");
    assert!(
        !bodies(&next).contains("Task events since"),
        "{}",
        bodies(&next)
    );
}

#[test]
fn ride_along_digest_is_capped() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    for _ in 0..200 {
        add(&mut c, "a", &who());
    }
    bot::add_wake(&mut c, &who(), "b", epoch(), "ping", None, epoch()).unwrap();
    let body = bodies(&claim(&mut c, epoch() + 1).expect("run"));
    assert!(body.contains("new 200 ("), "{body}");
    assert!(body.contains("+190 more"), "{body}");
    assert!(body.len() < 400, "{}", body.len());
}

#[test]
fn large_batches_are_capped_but_count_everything() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    for _ in 0..80 {
        escalated(&mut c, "a");
    }
    let body = bodies(&claim(&mut c, epoch() + PAST_BATCH).expect("run"));
    assert!(body.contains("events, 80 "), "{body}");
    assert!(body.contains("+30 more:"), "{body}");
    assert!(body.contains("+10 more"), "{body}");
    assert_eq!(body.matches("escalated:").count(), 50, "{body}");
}

#[test]
fn invalid_config_is_rejected_and_stores_nothing() {
    let (_d, _p, mut c) = rig();
    let bad = [
        Change {
            wake: Some(vec!["exploded".into()]),
            ..Default::default()
        },
        Change {
            ride: Some(vec!["nope".into()]),
            ..Default::default()
        },
        Change {
            batch_seconds: Some(59),
            ..Default::default()
        },
        Change {
            batch_seconds: Some(86_401),
            ..Default::default()
        },
        Change {
            wake: Some(vec![]),
            ride: Some(vec![]),
            ..Default::default()
        },
        Change {
            wake: Some(vec!["done".into()]),
            ride: Some(vec!["done".into()]),
            ..Default::default()
        },
        Change {
            project: Some("has space".into()),
            ..Default::default()
        },
    ];
    for b in &bad {
        assert!(
            sub::subscribe(&mut c, &who(), "b", b, "x", epoch()).is_err(),
            "{b:?}"
        );
    }
    assert!(sub::subscribe(&mut c, &who(), "b", &Change::default(), " ", epoch()).is_err());
    assert!(sub::list(&c, "b").unwrap().is_empty());
    assert!(sub::unsubscribe(&mut c, &who(), "b", None, "none").is_err());
}

#[test]
fn restart_mid_batch_loses_and_duplicates_nothing() {
    let (_d, path, mut c) = rig();
    subscribe_default(&mut c);
    let before = escalated(&mut c, "a");
    drop(c);
    let mut c = db::open(&path).unwrap();
    let after = escalated(&mut c, "a");
    drop(c);
    let mut c = db::open(&path).unwrap();
    let run = claim(&mut c, epoch() + PAST_BATCH).expect("run");
    assert_eq!(batches(&run), 1);
    let body = bodies(&run);
    assert_eq!(
        body.matches(&format!("#{before} a escalated")).count(),
        1,
        "{body}"
    );
    assert_eq!(
        body.matches(&format!("#{after} a escalated")).count(),
        1,
        "{body}"
    );
    done(&c, run.run);
    assert!(!ready(&c, PAST_BATCH));
}

#[test]
fn run_lost_to_a_restart_redelivers_once_and_ride_alongs_do_not_force_a_run() {
    let (_d, path, mut c) = rig();
    subscribe_default(&mut c);
    let esc = escalated(&mut c, "a");
    let run = claim(&mut c, epoch() + PAST_BATCH).expect("run");
    assert!(
        bodies(&run).contains("Task events since"),
        "{}",
        bodies(&run)
    );
    drop(c);
    let mut c = db::open(&path).unwrap();
    bot::reconcile(&c, "d", "server restart").unwrap();
    let again = claim(&mut c, epoch() + PAST_BATCH).expect("redelivered");
    let body = bodies(&again);
    assert_eq!(
        body.matches(&format!("#{esc} a escalated")).count(),
        1,
        "{body}"
    );
    assert_eq!(body.matches("Task events since").count(), 1, "{body}");
    done(&c, again.run);
    assert!(!ready(&c, PAST_BATCH));

    // A failure with only ride-alongs leaves no wake to retry.
    let (_d2, _p2, mut c) = rig();
    subscribe_default(&mut c);
    add(&mut c, "a", &who());
    bot::add_wake(&mut c, &who(), "b", epoch(), "ping", None, epoch()).unwrap();
    let run = claim(&mut c, epoch() + 1).expect("run");
    bot::reconcile(&c, "d", "restart").unwrap();
    c.execute(
        "DELETE FROM bot_wakes WHERE key NOT LIKE 'ride:%' AND run_id IS NULL",
        [],
    )
    .unwrap();
    assert!(
        !ready(&c, 1),
        "ride-along wake was left queued by run {}",
        run.run
    );
}

#[test]
fn a_bots_own_actions_do_not_reach_it() {
    let (_d, _p, mut c) = rig();
    subscribe_default(&mut c);
    let me = Actor::new("bot:b");
    let id = add(&mut c, "a", &me);
    set(&mut c, id, "escalated", &me);
    set(&mut c, id, "done", &me);
    assert!(!ready(&c, 3600));
    bot::add_wake(&mut c, &who(), "b", epoch(), "ping", None, epoch()).unwrap();
    let run = claim(&mut c, epoch() + 1).expect("run");
    let body = bodies(&run);
    assert!(!body.contains("Task events"), "{body}");
    done(&c, run.run);
    // Another bot's action is not this bot's own.
    let id = add(&mut c, "a", &who());
    set(&mut c, id, "escalated", &Actor::new("bot:other"));
    assert!(ready(&c, PAST_BATCH));
}

#[test]
fn protocol_states_the_triage_rules() {
    assert!(bot::PROTOCOL.contains("Triage it"));
    assert!(bot::PROTOCOL.contains("never clear an escalation"));
    assert!(bot::PROTOCOL.contains("Never relay `new` or `done`"));
}
