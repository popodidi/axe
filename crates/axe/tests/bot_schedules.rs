//! Bot schedules: firing, coalescing, downtime catch-up, restart safety, and validation.
use axe::bot::{self, NewBot};
use axe::db::{self, Actor};
use axe::schedule::{self, NewSchedule};
use rusqlite::Connection;

/// 2027-01-15T08:00:00Z.
const T0: i64 = 1_800_000_000;
const HOUR: i64 = 3600;

fn who() -> Actor {
    Actor::new("bot:b")
}

fn setup() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    let b = NewBot {
        name: "b".into(),
        project: "a".into(),
        agent: Some("fake".into()),
        identity: String::new(),
        conduct: String::new(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    };
    bot::create(&mut c, &who(), &b, T0 - 10).unwrap();
    // Consume the "bot created" wake so counts below are only schedule wakes.
    let run = claim(&mut c, T0 - 10).unwrap().run;
    bot::finish(&c, run, Some(0), "exit", Some("s"), None, T0 - 5).unwrap();
    (dir, c)
}

fn new(name: &str, cron: &str) -> NewSchedule {
    NewSchedule {
        name: name.into(),
        cron: cron.into(),
        timezone: "UTC".into(),
        objective: format!("objective of {name}"),
    }
}

fn add(c: &mut Connection, name: &str, cron: &str) {
    schedule::add(c, &who(), "b", &new(name, cron), "test", T0).unwrap();
}

fn claim(c: &mut Connection, now: i64) -> Option<bot::Claimed> {
    let tx = c.transaction().unwrap();
    let got = bot::claim(&tx, "b", &format!("bot:{now}"), "d", "g", now).unwrap();
    tx.commit().unwrap();
    got
}

fn queued(c: &Connection) -> Vec<bot::Wake> {
    bot::wakes(c, "b", true, 100).unwrap()
}

fn events(c: &Connection, kind: &str) -> Vec<String> {
    bot::events(c, "b", 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.body)
        .collect()
}

#[test]
fn fires_at_the_expected_time_with_a_labeled_objective() {
    let (_d, mut c) = setup();
    add(&mut c, "digest", "0 9 * * *");
    let s = &schedule::list(&c, "b").unwrap()[0];
    assert_eq!(s.next_at, "2027-01-15T09:00:00Z");
    assert_eq!(schedule::tick(&mut c, T0 + HOUR - 1).unwrap(), 0);
    assert!(queued(&c).is_empty());
    assert_eq!(schedule::tick(&mut c, T0 + HOUR).unwrap(), 1);
    let w = queued(&c);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].kind, "wake");
    assert_eq!(w[0].actor, "schedule:digest");
    assert_eq!(w[0].due_at, "2027-01-15T09:00:00Z");
    assert!(w[0].message.is_none(), "never an owner message");
    let line = bot::wake_line(&w[0]);
    assert!(line.contains("schedule:digest"), "{line}");
    assert!(line.contains("scheduled wake"), "{line}");
    assert!(line.contains("not an owner message"), "{line}");
    assert!(line.contains("objective of digest"), "{line}");
    let s = &schedule::list(&c, "b").unwrap()[0];
    assert_eq!(s.next_at, "2027-01-16T09:00:00Z");
    assert_eq!(s.last_fired_at.as_deref(), Some("2027-01-15T09:00:00Z"));
    // The run takes it like any other wake.
    let got = claim(&mut c, T0 + HOUR).unwrap();
    assert_eq!(got.wakes.len(), 1);
}

#[test]
fn the_cron_is_read_in_the_schedule_time_zone() {
    let (_d, mut c) = setup();
    let mut s = new("ny", "0 9 * * *");
    s.timezone = "America/New_York".into();
    let got = schedule::add(&mut c, &who(), "b", &s, "test", T0).unwrap();
    assert_eq!(got.next_at, "2027-01-15T14:00:00Z");
}

#[test]
fn several_schedules_fire_independently_and_batch_into_one_run() {
    let (_d, mut c) = setup();
    add(&mut c, "a", "0 9 * * *");
    add(&mut c, "b", "0 9 * * *");
    add(&mut c, "later", "0 12 * * *");
    assert_eq!(schedule::tick(&mut c, T0 + HOUR).unwrap(), 2);
    let got = claim(&mut c, T0 + HOUR).unwrap();
    assert_eq!(got.wakes.len(), 2, "one run carries both");
    assert_eq!(schedule::list(&c, "b").unwrap().len(), 3);
}

#[test]
fn firing_while_a_run_is_live_coalesces_to_one_queued_wake() {
    let (_d, mut c) = setup();
    add(&mut c, "often", "*/15 * * * *");
    let run = {
        bot::add_wake(&mut c, &who(), "b", T0, "keep busy", None, T0).unwrap();
        claim(&mut c, T0).unwrap().run
    };
    assert_eq!(schedule::tick(&mut c, T0 + 900).unwrap(), 1);
    assert_eq!(schedule::tick(&mut c, T0 + 1800).unwrap(), 0);
    assert_eq!(schedule::tick(&mut c, T0 + 2700).unwrap(), 0);
    assert_eq!(queued(&c).len(), 1, "still one wake while busy");
    let fires = events(&c, "schedule_fire");
    assert_eq!(fires.iter().filter(|e| e.contains("coalesced")).count(), 2);
    // Once delivered, the next slot queues a fresh wake.
    bot::finish(&c, run, Some(0), "exit", Some("s"), None, T0 + 2800).unwrap();
    let got = claim(&mut c, T0 + 2800).unwrap();
    assert_eq!(got.wakes.len(), 1);
    assert_eq!(schedule::tick(&mut c, T0 + 3600).unwrap(), 1);
}

#[test]
fn downtime_catch_up_fires_once_not_once_per_missed_slot() {
    let (_d, mut c) = setup();
    add(&mut c, "often", "*/15 * * * *");
    // A day of slots missed while the server was down.
    let later = T0 + 24 * HOUR + 60;
    assert_eq!(schedule::tick(&mut c, later).unwrap(), 1);
    assert_eq!(schedule::tick(&mut c, later + 1).unwrap(), 0);
    assert_eq!(queued(&c).len(), 1);
    let s = &schedule::list(&c, "b").unwrap()[0];
    assert_eq!(s.next_at, bot::ts(T0 + 24 * HOUR + 900));
}

#[test]
fn restart_mid_window_neither_loses_nor_doubles_a_firing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut c = db::open(&path).unwrap();
    let b = NewBot {
        name: "b".into(),
        project: "a".into(),
        agent: Some("fake".into()),
        identity: String::new(),
        conduct: String::new(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    };
    bot::create(&mut c, &who(), &b, T0).unwrap();
    add(&mut c, "digest", "0 9 * * *");
    // Stopped before the slot, back after it: the persisted cursor still fires.
    drop(c);
    let mut c = db::open(&path).unwrap();
    assert_eq!(schedule::tick(&mut c, T0 + HOUR + 30).unwrap(), 1);
    // Restarted right after firing: the same slot does not fire again.
    drop(c);
    let mut c = db::open(&path).unwrap();
    assert_eq!(schedule::tick(&mut c, T0 + HOUR + 31).unwrap(), 0);
    let fired: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM bot_wakes WHERE key LIKE 'sched:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fired, 1);
    // A wake returned by a dispatcher stop is redelivered, not doubled.
    let run = claim(&mut c, T0 + HOUR + 32).unwrap().run;
    bot::finish(
        &c,
        run,
        None,
        "dispatcher_stopped",
        None,
        None,
        T0 + HOUR + 33,
    )
    .unwrap();
    assert_eq!(schedule::tick(&mut c, T0 + 2 * HOUR).unwrap(), 0);
    assert_eq!(
        queued(&c)
            .iter()
            .filter(|w| w.actor.starts_with("schedule:"))
            .count(),
        1
    );
}

#[test]
fn a_paused_bot_accumulates_nothing() {
    let (_d, mut c) = setup();
    add(&mut c, "often", "*/15 * * * *");
    bot::change(&mut c, &who(), "b", "pause").unwrap();
    for i in 1..=8 {
        assert_eq!(schedule::tick(&mut c, T0 + i * 900).unwrap(), 0);
    }
    assert!(queued(&c).is_empty());
    assert!(events(&c, "schedule_fire").is_empty());
    bot::change(&mut c, &who(), "b", "resume").unwrap();
    assert_eq!(schedule::tick(&mut c, T0 + 8 * 900 + 1).unwrap(), 0);
    assert_eq!(schedule::tick(&mut c, T0 + 9 * 900).unwrap(), 1);
    assert_eq!(queued(&c).len(), 1);
}

#[test]
fn pause_resume_remove_are_audited_and_stop_queued_firings() {
    let (_d, mut c) = setup();
    add(&mut c, "digest", "0 9 * * *");
    let change = |c: &mut Connection, action: &str, now: i64| {
        schedule::change(c, &who(), "b", "digest", action, "why text", now).unwrap()
    };
    // Pausing drops a queued firing; the missed slots never fire.
    assert_eq!(schedule::tick(&mut c, T0 + HOUR).unwrap(), 1);
    change(&mut c, "pause", T0 + HOUR + 1);
    assert!(queued(&c).is_empty());
    assert!(!schedule::list(&c, "b").unwrap()[0].enabled);
    assert_eq!(schedule::tick(&mut c, T0 + 72 * HOUR).unwrap(), 0);
    // Resuming fires from the next occurrence, not the paused backlog.
    change(&mut c, "resume", T0 + 72 * HOUR);
    let s = &schedule::list(&c, "b").unwrap()[0];
    assert!(s.enabled);
    assert_eq!(s.next_at, "2027-01-18T09:00:00Z");
    assert_eq!(schedule::tick(&mut c, T0 + 72 * HOUR).unwrap(), 0);
    // Removal drops the row and any queued firing.
    assert_eq!(schedule::tick(&mut c, T0 + 73 * HOUR).unwrap(), 1);
    change(&mut c, "remove", T0 + 73 * HOUR + 1);
    assert!(queued(&c).is_empty());
    assert!(schedule::list(&c, "b").unwrap().is_empty());
    for kind in [
        "schedule_add",
        "schedule_pause",
        "schedule_resume",
        "schedule_remove",
    ] {
        let e = events(&c, kind);
        assert_eq!(e.len(), 1, "{kind}");
    }
    assert!(events(&c, "schedule_pause")[0].contains("why text"));
    assert!(schedule::change(&mut c, &who(), "b", "digest", "pause", "w", T0).is_err());
}

#[test]
fn invalid_cron_and_short_intervals_are_rejected() {
    let (_d, mut c) = setup();
    for (cron, expect) in [
        ("not a cron at all", "five numeric"),
        ("0 9 * *", "five numeric"),
        ("61 9 * * *", "invalid cron"),
        ("0 0 30 2 *", ""),
        ("* * * * *", "minimum interval"),
        ("*/5 * * * *", "minimum interval"),
        ("0,10 * * * *", "minimum interval"),
        // Short only across the day boundary.
        ("0,55 0,23 * * *", "minimum interval"),
    ] {
        let err = schedule::add(&mut c, &who(), "b", &new("x", cron), "w", T0)
            .expect_err(cron)
            .to_string();
        assert!(err.contains(expect), "{cron}: {err}");
    }
    let mut bad_tz = new("x", "0 9 * * *");
    bad_tz.timezone = "Mars/Base".into();
    assert!(schedule::add(&mut c, &who(), "b", &bad_tz, "w", T0).is_err());
    for name in ["", "UPPER", "-x", "has space"] {
        assert!(
            schedule::add(&mut c, &who(), "b", &new(name, "0 9 * * *"), "w", T0).is_err(),
            "{name}"
        );
    }
    assert!(schedule::add(&mut c, &who(), "b", &new("x", "0 9 * * *"), " ", T0).is_err());
    assert!(schedule::list(&c, "b").unwrap().is_empty());
    // The exact minimum is allowed; a zone's DST must not reject it.
    add(&mut c, "edge", "*/15 * * * *");
    let mut ny = new("ny", "*/15 * * * *");
    ny.timezone = "America/New_York".into();
    schedule::add(&mut c, &who(), "b", &ny, "w", T0).unwrap();
    // A name is unique per bot.
    assert!(schedule::add(&mut c, &who(), "b", &new("edge", "0 9 * * *"), "w", T0).is_err());
}
