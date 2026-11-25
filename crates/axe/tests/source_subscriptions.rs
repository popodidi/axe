//! Service and routine event subscriptions: the failures-only default on
//! creation, a reported failure waking the subscribed bot in a batch, and
//! unsubscribing stopping it. Fan-out stamps real time, so tests claim at
//! `epoch() + offset`.
use std::collections::BTreeMap;

use axe::bot::{self, NewBot};
use axe::db::{self, Actor, Changes};
use axe::remote::epoch;
use axe::routine::{self, Template};
use axe::service::{self, NewService, Status};
use axe::subscription::{self as sub, Change, Target};
use rusqlite::Connection;
use serde_json::json;

/// Past the default 1m batch of a service or routine subscription.
const PAST_BATCH: i64 = 62;

fn who() -> Actor {
    Actor::new("agent")
}

fn new_bot(c: &mut Connection, name: &str) {
    let b = NewBot {
        name: name.into(),
        project: "a".into(),
        agent: None,
        identity: "i".into(),
        conduct: "c".into(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    };
    bot::create(c, &who(), &b, epoch()).unwrap();
    let first = claim(c, name, epoch()).expect("creation run");
    done(c, first.run);
}

/// Bots `b` (owning service `s`) and `o` (another bot), creation wakes
/// handled, and a registered dispatcher `d`.
fn rig() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    new_bot(&mut c, "b");
    new_bot(&mut c, "o");
    axe::remote::register(&c, &json!({"id":"d","generation":"g"})).unwrap();
    let svc = NewService {
        command: "sleep 1".into(),
        cwd: None,
        env: BTreeMap::new(),
        restart: "on-failure".into(),
        schedule: None,
    };
    service::put(&mut c, &who(), "b", "s", &svc).unwrap();
    (dir, c)
}

fn claim(c: &mut Connection, name: &str, now: i64) -> Option<bot::Claimed> {
    let tx = c.transaction().unwrap();
    let out = bot::claim(&tx, name, None, &format!("bot:{now}"), "d", "g", now).unwrap();
    tx.commit().unwrap();
    out
}

fn done(c: &Connection, run: i64) {
    bot::finish(c, run, Some(0), "exit", Some("s"), None, epoch()).unwrap();
}

fn ready(c: &Connection, offset: i64) -> Vec<String> {
    bot::ready(c, epoch() + offset).unwrap()
}

fn bodies(c: &bot::Claimed) -> String {
    c.wakes
        .iter()
        .map(|w| w.body.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn report(c: &mut Connection, st: Status) {
    let mut m = BTreeMap::new();
    m.insert("b/s".to_string(), st);
    service::report(c, "d", "g", &m).unwrap();
}

fn running(started: &str, failures: u64) -> Status {
    Status {
        state: "running".into(),
        pid: Some(42),
        started_at: Some(started.into()),
        failures,
        ..Status::default()
    }
}

/// The service ran, then crashed once and is waiting to restart.
fn crash(c: &mut Connection) {
    report(c, running("2026-11-25T00:00:00Z", 0));
    report(
        c,
        Status {
            state: "backoff".into(),
            started_at: Some("2026-11-25T00:00:00Z".into()),
            last_exit: Some("exit 7".into()),
            failures: 1,
            ..Status::default()
        },
    );
}

#[test]
fn a_new_service_subscribes_its_owner_to_failures_only() {
    let (_d, c) = rig();
    let subs = sub::list(&c, "b").unwrap();
    assert_eq!(subs.len(), 1, "{subs:?}");
    assert_eq!(subs[0].service.as_deref(), Some("b/s"));
    assert_eq!(subs[0].wake, ["failed", "crash_loop"]);
    assert_eq!(subs[0].batch_seconds, 60);
    assert!(sub::list(&c, "o").unwrap().is_empty());
    let ev = bot::events(&c, "b", 20).unwrap();
    assert!(
        ev.iter()
            .any(|e| e.kind == "subscribe" && e.body.contains("default for a new service")),
        "{ev:?}"
    );
}

#[test]
fn a_reported_failure_wakes_the_subscribed_bot_in_one_batch() {
    let (_d, mut c) = rig();
    // Starting is not a failure: nothing for the failures-only default.
    report(&mut c, running("2026-11-25T00:00:00Z", 0));
    assert!(ready(&c, PAST_BATCH).is_empty());
    report(
        &mut c,
        Status {
            state: "backoff".into(),
            started_at: Some("2026-11-25T00:00:00Z".into()),
            last_exit: Some("exit 7".into()),
            failures: 1,
            ..Status::default()
        },
    );
    // Batched: not before the period, then once.
    assert!(ready(&c, 10).is_empty());
    assert_eq!(ready(&c, PAST_BATCH), ["b"]);
    let run = claim(&mut c, "b", epoch() + PAST_BATCH).expect("batch run");
    let text = bodies(&run);
    assert!(text.starts_with("Subscribed events, 1"), "{text}");
    assert!(text.contains("service failed: b/s: exit 7"), "{text}");
    done(&c, run.run);
    // The same report again is no new event.
    report(
        &mut c,
        Status {
            state: "backoff".into(),
            started_at: Some("2026-11-25T00:00:00Z".into()),
            last_exit: Some("exit 7".into()),
            failures: 1,
            ..Status::default()
        },
    );
    assert!(ready(&c, PAST_BATCH * 2).is_empty());
}

#[test]
fn other_bots_subscribe_to_chosen_events_and_unsubscribing_stops_them() {
    let (_d, mut c) = rig();
    let ch = Change {
        service: Some("b/s".into()),
        wake: Some(vec!["started".into(), "failed".into()]),
        ..Change::default()
    };
    let s = sub::subscribe(&mut c, &who(), "o", &ch, "watch b", epoch()).unwrap();
    assert_eq!(s.service.as_deref(), Some("b/s"));
    assert!(sub::subscribe(
        &mut c,
        &who(),
        "o",
        &Change {
            service: Some("b/s".into()),
            wake: Some(vec!["escalated".into()]),
            ..Change::default()
        },
        "bad",
        epoch()
    )
    .is_err());
    report(&mut c, running("2026-11-25T00:00:00Z", 0));
    let run = claim(&mut c, "o", epoch() + PAST_BATCH).expect("started wakes o");
    assert!(
        bodies(&run).contains("service started: b/s"),
        "{}",
        bodies(&run)
    );
    done(&c, run.run);

    // The owner unsubscribes from its default: a failure wakes only `o`.
    sub::unsubscribe_from(&mut c, &who(), "b", &Target::Service("s"), "quiet").unwrap();
    assert!(sub::list(&c, "b").unwrap().is_empty());
    report(
        &mut c,
        Status {
            state: "backoff".into(),
            started_at: Some("2026-11-25T00:00:00Z".into()),
            last_exit: Some("signal 9".into()),
            failures: 1,
            ..Status::default()
        },
    );
    assert_eq!(ready(&c, PAST_BATCH), ["o"]);
    let run = claim(&mut c, "o", epoch() + PAST_BATCH).unwrap();
    assert!(bodies(&run).contains("service failed: b/s: signal 9"));
    done(&c, run.run);

    // `o` unsubscribes too: the next failure wakes nobody.
    sub::unsubscribe_from(&mut c, &who(), "o", &Target::Service("b/s"), "done").unwrap();
    crash(&mut c);
    assert!(ready(&c, PAST_BATCH * 2).is_empty());
}

#[test]
fn an_all_services_subscription_hears_every_service_and_removal_drops_rows() {
    let (_d, mut c) = rig();
    let ch = Change {
        service: Some("*".into()),
        wake: Some(vec!["stopped".into(), "crash_loop".into()]),
        ..Change::default()
    };
    sub::subscribe(&mut c, &who(), "o", &ch, "all", epoch()).unwrap();
    report(&mut c, running("2026-11-25T00:00:00Z", 0));
    report(
        &mut c,
        Status {
            state: "stopped".into(),
            ..Status::default()
        },
    );
    let run = claim(&mut c, "o", epoch() + PAST_BATCH).expect("stopped wakes o");
    assert!(bodies(&run).contains("service stopped: b/s"));
    done(&c, run.run);
    service::remove(&mut c, &who(), "b", "s").unwrap();
    assert!(sub::list(&c, "b").unwrap().is_empty(), "the default goes");
    assert_eq!(sub::list(&c, "o").unwrap().len(), 1, "the * row stays");
}

#[test]
fn service_events_from_reports() {
    let r = |state: &str, started: &str| Status {
        state: state.into(),
        started_at: (!started.is_empty()).then(|| started.to_string()),
        ..Status::default()
    };
    let names = |old: &Status, new: &Status| -> Vec<&str> {
        service::events(old, new).into_iter().map(|e| e.0).collect()
    };
    assert_eq!(names(&Status::default(), &r("running", "t1")), ["started"]);
    assert_eq!(
        names(&r("running", "t1"), &r("running", "t1")),
        [] as [&str; 0]
    );
    assert_eq!(names(&r("running", "t1"), &r("running", "t2")), ["started"]);
    assert_eq!(names(&r("running", "t1"), &r("stopped", "t1")), ["stopped"]);
    assert_eq!(
        names(&Status::default(), &r("stopped", "")),
        [] as [&str; 0]
    );
    assert_eq!(
        names(&r("running", "t1"), &r("exited", "t1")),
        ["run_finished"]
    );
    let mut looping = r("backoff", "t1");
    looping.failures = 3;
    looping.crash_loop_since = Some("t".into());
    assert_eq!(names(&running("t1", 1), &looping), ["failed", "crash_loop"]);
    assert!(service::events(&running("t1", 1), &looping)[0]
        .1
        .contains("(2 times)"));
    // A restarted dispatcher counts from 0 again.
    assert_eq!(names(&running("t1", 5), &running("t1", 0)), [] as [&str; 0]);
    let mut again = r("backoff", "t1");
    again.failures = 1;
    assert_eq!(names(&running("t1", 5), &again), ["failed"]);
}

fn template(owner: Option<&str>) -> Template {
    Template {
        agent: None,
        name: "Nightly".into(),
        project: "x".into(),
        cron: "* * * * *".into(),
        timezone: "UTC".into(),
        title: "Nightly report".into(),
        detail: "d".into(),
        workload: None,
        owner: owner.map(String::from),
        repos: None,
        labels: vec![],
        pre_check: None,
        model: None,
        effort: None,
        skills: None,
    }
}

#[test]
fn routines_subscribe_their_owning_bot_to_failures_and_wake_on_them() {
    let (_d, mut c) = rig();
    let id = routine::save(&mut c, &who(), None, &template(Some("bot:b")), epoch()).unwrap();
    let unowned = routine::save(&mut c, &who(), None, &template(None), epoch()).unwrap();
    let subs = sub::list(&c, "b").unwrap();
    let r: Vec<_> = subs.iter().filter(|s| s.routine.is_some()).collect();
    assert_eq!(r.len(), 1, "{subs:?}");
    assert_eq!(r[0].routine.as_deref(), Some(id.to_string().as_str()));
    assert_eq!(r[0].wake, ["failed"]);

    // `o` follows every routine's firing.
    let ch = Change {
        routine: Some("*".into()),
        wake: Some(vec!["fired".into()]),
        ..Change::default()
    };
    sub::subscribe(&mut c, &who(), "o", &ch, "all", epoch()).unwrap();
    let task = routine::trigger(&mut c, &who(), id, "k1").unwrap();
    routine::trigger(&mut c, &who(), unowned, "k1").unwrap();
    assert_eq!(
        ready(&c, PAST_BATCH),
        ["o"],
        "firing is not a failure for b"
    );
    let run = claim(&mut c, "o", epoch() + PAST_BATCH).unwrap();
    let text = bodies(&run);
    assert!(
        text.contains(&format!(
            "routine fired: #{id} Nightly: created task #{task}"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("#{unowned} Nightly")), "{text}");
    done(&c, run.run);

    // The routine's task escalates: its owning bot hears it.
    let ch = Changes {
        state: Some("escalated".into()),
        ..Default::default()
    };
    db::set_task(&mut c, &who(), task, &ch).unwrap();
    let run = claim(&mut c, "b", epoch() + PAST_BATCH).expect("failure wakes b");
    assert!(
        bodies(&run).contains(&format!(
            "routine failed: #{id} Nightly: task #{task} escalated"
        )),
        "{}",
        bodies(&run)
    );
    done(&c, run.run);

    // Unsubscribed, and archiving drops the rest.
    sub::unsubscribe_from(&mut c, &who(), "b", &Target::Routine(&id.to_string()), "q").unwrap();
    let t2 = routine::trigger(&mut c, &who(), id, "k2").unwrap();
    db::set_task(&mut c, &who(), t2, &ch).unwrap();
    let r = ready(&c, PAST_BATCH * 2);
    assert!(!r.contains(&"b".to_string()), "{r:?}");
    routine::change(&mut c, &who(), unowned, "archive", epoch()).unwrap();
    assert!(sub::subscribe(
        &mut c,
        &who(),
        "b",
        &Change {
            routine: Some(unowned.to_string()),
            ..Change::default()
        },
        "late",
        epoch()
    )
    .is_err());
}

#[test]
fn service_rows_are_not_projects() {
    let (_d, mut c) = rig();
    assert!(sub::subscribe(
        &mut c,
        &who(),
        "b",
        &Change {
            project: Some("service:1".into()),
            ..Change::default()
        },
        "x",
        epoch()
    )
    .is_err());
    // A task-event subscription and the service default live side by side.
    sub::subscribe(&mut c, &who(), "b", &Change::default(), "tasks", epoch()).unwrap();
    let subs = sub::list(&c, "b").unwrap();
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].project, "a");
    assert!(subs[1].service.is_some());
    crash(&mut c);
    let run = claim(&mut c, "b", epoch() + PAST_BATCH).unwrap();
    assert!(bodies(&run).contains("service failed"));
}
