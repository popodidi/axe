//! The read-only owner commands `/tasks`, `/task N`, `/cost`, `/goals`,
//! `/services`, `/routines`, `/status` and `/node` (`owner_views`): which tasks a bot
//! lists and in what order, the empty and truncated replies, one task's view,
//! the 7-day cost of one bot, its goals, services, routines and schedules.
use axe::bot::{self, ts, NewBot};
use axe::db::{self, Actor, Changes, NewTask};
use axe::owner_views::{self, COST_DAYS};
use axe::subscription::{self, Change};
use axe::usage::{self, Parsed, Totals};
use rusqlite::{params, Connection};

const WEB: &str = "http://axe.example/";
const DAY: i64 = 86_400;

fn human() -> Actor {
    Actor::new("human")
}

fn now() -> i64 {
    axe::remote::epoch()
}

fn new_bot(name: &str, project: &str) -> NewBot {
    NewBot {
        name: name.into(),
        project: project.into(),
        agent: Some("fake".into()),
        identity: String::new(),
        conduct: String::new(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    }
}

/// Bots `a` (project `p`) and `b` (project `q`).
fn setup() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    bot::create(&mut c, &human(), &new_bot("a", "p"), now()).unwrap();
    bot::create(&mut c, &human(), &new_bot("b", "q"), now()).unwrap();
    (dir, c)
}

fn add(c: &mut Connection, project: &str, title: &str, detail: &str) -> i64 {
    let t = NewTask {
        agent: None,
        project,
        title,
        detail,
        state: "pending",
    };
    db::add_task(c, &human(), &t).unwrap()
}

fn set(c: &mut Connection, id: i64, ch: Changes) {
    db::set_task(c, &human(), id, &ch).unwrap();
}

fn force_state(c: &Connection, id: i64, state: &str) {
    c.execute("UPDATE tasks SET state=?2 WHERE id=?1", params![id, state])
        .unwrap();
}

#[test]
fn tasks_lists_owned_and_project_tasks_needing_the_owner_first() {
    let (_d, mut c) = setup();
    assert_eq!(
        owner_views::tasks(&c, "a", Some(WEB)).unwrap(),
        "bot a: no open tasks"
    );

    let queued = add(&mut c, "p", "Queued <one>", "");
    let running = add(&mut c, "p", "Running", "");
    force_state(&c, running, "in_progress");
    let escalated = add(&mut c, "p", "Escalated", "");
    force_state(&c, escalated, "escalated");
    let waiting = add(&mut c, "p", "Waiting", "");
    set(
        &mut c,
        waiting,
        Changes {
            not_before: Some("2099-01-01T00:00:00Z".into()),
            ..Default::default()
        },
    );
    let done = add(&mut c, "p", "Done", "");
    force_state(&c, done, "done");
    let other = add(&mut c, "r", "Other project", "");
    let owned = add(&mut c, "r", "Owned elsewhere", "");
    set(
        &mut c,
        owned,
        Changes {
            owner: Some("bot:a".into()),
            ..Default::default()
        },
    );
    let theirs = add(&mut c, "q", "Bot b's", "");

    let out = owner_views::tasks(&c, "a", Some(WEB)).unwrap();
    let link = |id: i64| format!("<a href=\"http://axe.example/?task={id}\">#{id}</a>");
    assert_eq!(
        out,
        [
            "bot a: 5 open tasks".to_string(),
            format!("{} escalated · p · Escalated", link(escalated)),
            format!("{} running · p · Running", link(running)),
            format!("{} waiting · p · Waiting", link(waiting)),
            format!("{} queued · p · Queued &lt;one&gt;", link(queued)),
            format!("{} queued · r · Owned elsewhere", link(owned)),
        ]
        .join("\n")
    );
    assert!(!out.contains(&format!("#{done}<")) && !out.contains(&format!("#{other}<")));
    assert!(!out.contains(&format!("#{theirs}<")));

    // A subscribed project counts; without a public URL there is no link.
    subscription::subscribe(
        &mut c,
        &human(),
        "b",
        &Change {
            project: Some("r".into()),
            ..Default::default()
        },
        "test",
        now(),
    )
    .unwrap();
    let out = owner_views::tasks(&c, "b", None).unwrap();
    assert_eq!(
        out,
        format!(
            "bot b: 3 open tasks\n#{other} queued · r · Other project\n\
             #{owned} queued · r · Owned elsewhere\n#{theirs} queued · q · Bot b's"
        )
    );
}

#[test]
fn tasks_truncates_to_one_message() {
    let (_d, mut c) = setup();
    let title = "x".repeat(80);
    for _ in 0..200 {
        add(&mut c, "p", &title, "");
    }
    let out = owner_views::tasks(&c, "a", Some(WEB)).unwrap();
    #[cfg(feature = "telegram")]
    assert!(out.encode_utf16().count() <= axe::telegram::TEXT_LIMIT);
    let shown = out.lines().filter(|l| l.starts_with("<a ")).count();
    assert!(shown > 10 && shown < 200, "{shown}");
    assert!(out.starts_with("bot a: 200 open tasks\n"));
    assert_eq!(
        out.lines().last().unwrap(),
        format!("+{} more", 200 - shown)
    );
}

#[test]
fn task_shows_state_outcome_and_latest_notes() {
    let (_d, mut c) = setup();
    let id = add(
        &mut c,
        "p",
        "Ship <it>",
        "## Outcome\nThe thing works.\n\n## Verify\n- tests",
    );
    let out = owner_views::task(&c, &id.to_string(), Some(WEB)).unwrap();
    assert_eq!(
        out,
        format!(
            "<a href=\"http://axe.example/?task={id}\">#{id}</a> <b>Ship &lt;it&gt;</b>\n\
             queued · p\n\nOutcome:\nThe thing works.\n\nno notes"
        )
    );

    for n in 1..=5 {
        set(
            &mut c,
            id,
            Changes {
                note: Some(format!("note {n}")),
                ..Default::default()
            },
        );
    }
    set(
        &mut c,
        id,
        Changes {
            owner: Some("bot:a".into()),
            ..Default::default()
        },
    );
    force_state(&c, id, "escalated");
    let out = owner_views::task(&c, &format!("#{id}"), None).unwrap();
    let mut lines = out.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!("#{id} <b>Ship &lt;it&gt;</b>")
    );
    assert_eq!(lines.next().unwrap(), "escalated · p · owner bot:a");
    assert!(out.contains("\n\nlatest notes (3 of 5):\n"), "{out}");
    let notes: Vec<&str> = out.lines().rev().take(3).collect();
    for (line, n) in notes.iter().rev().zip(3..=5) {
        assert!(line.ends_with(&format!(" human: note {n}")), "{line}");
    }
    assert!(!out.contains("note 2"));

    assert_eq!(
        owner_views::task(&c, "", None).unwrap(),
        "usage: /task N (a task id)"
    );
    assert_eq!(
        owner_views::task(&c, "abc", None).unwrap(),
        "usage: /task N (a task id)"
    );
    let err = owner_views::task(&c, "999", None).unwrap_err();
    assert!(format!("{err:#}").contains("task 999 not found"));
}

fn known(session: String, cost: f64) -> Parsed {
    Parsed {
        unknown: None,
        session_id: Some(session),
        models: [(
            "m".to_owned(),
            Totals {
                cost,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        turns: Some(1),
        context_tokens: Some(1),
        cache_start: Some("warm".into()),
    }
}

fn bot_run(c: &Connection, id: i64, bot: i64, ended: i64, cost: Option<f64>) {
    c.execute(
        "INSERT INTO bot_runs (id, bot_id, agent, mode, session_id, claim, dispatcher, generation,
           started_at, ended_at, log_path)
         VALUES (?1, ?2, 'claude', 'resume', 'b', 'c', 'd', 'g', ?3, ?3, 'x')",
        params![id, bot, ts(ended)],
    )
    .unwrap();
    if let Some(cost) = cost {
        usage::record(c, "bot", id, &known(format!("b{id}"), cost), "test").unwrap();
    }
}

#[test]
fn cost_sums_the_bots_runs_over_the_last_seven_days() {
    let (_d, c) = setup();
    // Midday UTC, so the runs below stay on the days they are meant for.
    let now = now().div_euclid(DAY) * DAY + DAY / 2;
    let empty = owner_views::cost(&c, "a", Some(WEB), now).unwrap();
    assert_eq!(
        empty,
        "<b>bot a cost, last 7 days: $0.00</b>\nno runs\n\
         <a href=\"http://axe.example/?view=cost&amp;days=7&amp;cost_bot=a\">Open cost</a>"
    );

    let today = now.div_euclid(DAY) * DAY;
    bot_run(&c, 1, 1, now - 60, Some(1.25));
    bot_run(&c, 2, 1, now - 120, None);
    bot_run(&c, 3, 1, today - DAY + 60, Some(0.5));
    // Outside the window, or another bot's: not counted.
    bot_run(&c, 4, 1, today - COST_DAYS * DAY + 60 - DAY, Some(100.0));
    bot_run(&c, 5, 2, now - 60, Some(7.0));
    // `Nov 25` for the UTC day `n` days before today.
    let day = |n: i64| {
        let d = time::OffsetDateTime::from_unix_timestamp(today - n * DAY)
            .unwrap()
            .date();
        format!(
            "{:<6}",
            format!("{} {}", &format!("{}", d.month())[..3], d.day())
        )
    };
    let out = owner_views::cost(&c, "a", None, now).unwrap();
    // A bar per day, newest first, scaled to the costliest day: today's
    // $1.25 fills it, yesterday's $0.50 is 4 of 10 cells, a day without
    // spend is empty.
    let mut want = "<b>bot a cost, last 7 days: $1.75 over 3 runs</b>\n\
         1 with unknown usage: the figure is a lower bound"
        .to_string();
    want.push_str(&format!(
        "\n<code>{} ▓▓▓▓▓▓▓▓▓▓  $1.25  2 runs</code>",
        day(0)
    ));
    want.push_str(&format!(
        "\n<code>{} ▓▓▓▓░░░░░░  $0.50  1 run</code>",
        day(1)
    ));
    for n in 2..COST_DAYS {
        want.push_str(&format!(
            "\n<code>{} ░░░░░░░░░░  $0.00  0 runs</code>",
            day(n)
        ));
    }
    assert_eq!(out, want);
    // The same figure the Cost view shows for that bot and range.
    let web = usage::cost_stats(
        &c,
        today + DAY - COST_DAYS * DAY,
        today + DAY,
        None,
        &usage::CostQuery {
            bot: Some("a"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(web["total"]["cost"].as_f64(), Some(1.75));
    assert!(owner_views::cost(&c, "nope", None, now).is_err());
}

#[test]
fn cost_bars_align_amounts_and_stay_empty_without_spend() {
    let (_d, c) = setup();
    let now = now().div_euclid(DAY) * DAY + DAY / 2;
    let today = now.div_euclid(DAY) * DAY;
    // Only unknown usage: no day has spend to scale to, every bar is empty.
    bot_run(&c, 1, 1, now - 60, None);
    let out = owner_views::cost(&c, "a", None, now).unwrap();
    let bars: Vec<&str> = out.lines().filter(|l| l.starts_with("<code>")).collect();
    assert_eq!(bars.len(), COST_DAYS as usize, "{out}");
    assert!(
        bars[0].ends_with(" ░░░░░░░░░░  $0.00  1 run</code>"),
        "{out}"
    );
    assert!(bars.iter().all(|b| !b.contains('▓')), "{out}");

    // Amounts right-aligned to the widest; the costliest day fills its bar.
    bot_run(&c, 2, 1, today - 2 * DAY + 60, Some(12.0));
    bot_run(&c, 3, 1, today - DAY + 60, Some(3.0));
    let out = owner_views::cost(&c, "a", None, now).unwrap();
    let bars: Vec<&str> = out.lines().filter(|l| l.starts_with("<code>")).collect();
    assert!(
        bars[0].ends_with(" ░░░░░░░░░░   $0.00  1 run</code>"),
        "{out}"
    );
    assert!(
        bars[1].ends_with(" ▓▓▓░░░░░░░   $3.00  1 run</code>"),
        "{out}"
    );
    assert!(
        bars[2].ends_with(" ▓▓▓▓▓▓▓▓▓▓  $12.00  1 run</code>"),
        "{out}"
    );
    assert!(
        out.starts_with("<b>bot a cost, last 7 days: $15.00 over 3 runs</b>\n"),
        "{out}"
    );
}

/// 2026-01-05 00:00:00 UTC (a Monday).
const MON: i64 = 1_767_571_200;

#[test]
fn goals_lists_the_bots_open_goals_with_budget_bars() {
    let (_d, mut c) = setup();
    assert_eq!(
        owner_views::goals(&c, "a", Some(WEB)).unwrap(),
        format!(
            "{}\n<a href=\"http://axe.example/?view=goals\">Open goals</a>",
            owner_views::EMPTY_GOALS
        )
    );
    assert_eq!(
        owner_views::goals(&c, "a", None).unwrap(),
        owner_views::EMPTY_GOALS
    );
    let g = |name: &str, cost: Option<f64>| axe::goal::NewGoal {
        name: name.into(),
        project: "p".into(),
        intent: "Grow <signups>. More text.".into(),
        budget_cost_week: cost,
        budget_attention_week: Some(5),
        ..Default::default()
    };
    let me = Actor::new("bot:a");
    axe::goal::propose(&mut c, &me, &g("grow", Some(50.0)), None, now()).unwrap();
    axe::goal::propose(&mut c, &me, &g("done-one", None), None, now()).unwrap();
    axe::goal::change(&mut c, &human(), "done-one", "drop", "no", None).unwrap();
    // Another bot's goal is not listed.
    axe::goal::propose(
        &mut c,
        &Actor::new("bot:b"),
        &g("theirs", None),
        None,
        now(),
    )
    .unwrap();
    axe::goal::record(
        &mut c,
        &me,
        "grow",
        "observation",
        None,
        "at risk: signups flat",
    )
    .unwrap();

    let out = owner_views::goals(&c, "a", Some(WEB)).unwrap();
    assert!(
        out.starts_with("<b>bot a: 1 goal</b>\n\n<b>grow</b> · <i>at risk</i>"),
        "{out}"
    );
    assert!(out.contains("\nsignups flat\n"), "{out}");
    assert!(
        out.contains("\n<code>Cost ░░░░░░░░░░   0%  $0.00/$50 wk</code>"),
        "{out}"
    );
    assert!(
        out.contains("\n<code>Attn ░░░░░░░░░░   0%  0/5 wk</code>"),
        "{out}"
    );
    assert!(
        out.contains("<a href=\"http://axe.example/?goal=grow\">Open goal</a>"),
        "{out}"
    );
    assert!(
        out.ends_with("\n\n<a href=\"http://axe.example/?view=goals\">Open goals</a>"),
        "{out}"
    );
    assert!(
        !out.contains("theirs") && !out.contains("done-one"),
        "{out}"
    );
    assert!(owner_views::goals(&c, "nope", None).is_err());
}

fn service(c: &mut Connection, bot: &str, name: &str, schedule: Option<&str>, status: &str) {
    let new = axe::service::NewService {
        command: "true".into(),
        cwd: None,
        env: Default::default(),
        restart: if schedule.is_some() {
            "never"
        } else {
            "always"
        }
        .into(),
        schedule: schedule.map(|cron| axe::service::Schedule {
            cron: cron.into(),
            timezone: "Asia/Taipei".into(),
            timeout_seconds: 60,
            catch_up: true,
        }),
    };
    axe::service::put(c, &human(), bot, name, &new).unwrap();
    c.execute(
        "UPDATE services SET status=?1 WHERE name=?2",
        params![status, name],
    )
    .unwrap();
}

#[test]
fn services_lists_state_policy_and_last_exit() {
    let (_d, mut c) = setup();
    let now = MON;
    assert_eq!(
        owner_views::services(&c, "a", Some(WEB), now).unwrap(),
        format!(
            "{}\n<a href=\"http://axe.example/?view=services&amp;q=a\">Open services</a>",
            owner_views::EMPTY_SERVICES
        )
    );
    service(
        &mut c,
        "a",
        "feed",
        None,
        &format!(
            r#"{{"state":"running","started_at":"{}","restarts":2,"failures":0,"last_exit":"exit 1"}}"#,
            ts(now - 7200)
        ),
    );
    service(
        &mut c,
        "a",
        "sync",
        Some("0 9 * * *"),
        &format!(
            r#"{{"state":"scheduled","started_at":"{}","restarts":0,"failures":0,"last_exit":"exit 0","next_run_at":"{}"}}"#,
            ts(now - 3600),
            ts(now + 1800)
        ),
    );
    service(&mut c, "b", "other", None, "{}");
    let out = owner_views::services(&c, "a", Some(WEB), now).unwrap();
    assert_eq!(
        out,
        "<b>bot a: 2 services</b>\n\
         • <b>feed</b> · running · restart always · up 2h · last exit 1\n\
         • <b>sync</b> · scheduled · daily 09:00 · last run 1h ago: exit 0 · next in 30m\n\
         <a href=\"http://axe.example/?view=services&amp;q=a\">Open services</a>"
    );
    // Declared, never reported, no public URL.
    let out = owner_views::services(&c, "b", None, now).unwrap();
    assert_eq!(
        out,
        "<b>bot b: 1 service</b>\n• <b>other</b> · pending · restart always · no exit yet"
    );
}

fn routine(c: &mut Connection, name: &str, owner: &str, now: i64) -> i64 {
    let t: axe::routine::Template = serde_json::from_value(serde_json::json!({
        "name": name, "project": "p", "cron": "0 9 * * *", "timezone": "Asia/Taipei",
        "title": "t", "detail": "d", "owner": owner,
    }))
    .unwrap();
    axe::routine::save(c, &human(), None, &t, now).unwrap()
}

#[test]
fn routines_lists_the_bots_routines_with_next_fire() {
    let (_d, mut c) = setup();
    let now = MON; // 08:00 in Taipei
    assert_eq!(
        owner_views::routines(&c, "a", None, now).unwrap(),
        owner_views::EMPTY_ROUTINES
    );
    routine(&mut c, "morning", "bot:a", now);
    let paused = routine(&mut c, "digest", "bot:a", now);
    axe::routine::change(&mut c, &human(), paused, "pause", now).unwrap();
    let gone = routine(&mut c, "old", "bot:a", now);
    axe::routine::change(&mut c, &human(), gone, "archive", now).unwrap();
    routine(&mut c, "theirs", "bot:b", now);
    let out = owner_views::routines(&c, "a", Some(WEB), now).unwrap();
    assert_eq!(
        out,
        "<b>bot a: 2 routines</b>\n\
         • <b>digest</b> · daily 09:00 · <i>paused</i>\n\
         • <b>morning</b> · daily 09:00 · next in 1h\n\
         <a href=\"http://axe.example/?view=routines\">Open routines</a>"
    );
}

#[test]
fn empty_states_say_what_the_primitive_is_without_commands() {
    for (text, word) in [
        (owner_views::EMPTY_GOALS, "No open goals."),
        (owner_views::EMPTY_SERVICES, "No services."),
        (owner_views::EMPTY_ROUTINES, "No routines."),
    ] {
        assert!(text.starts_with(&format!("<b>{word}</b> ")), "{text}");
        assert!(!text.contains('\n'), "one line: {text}");
        for cmd in ["axe ", "<code>", " /", "--"] {
            assert!(!text.contains(cmd), "no command ({cmd}): {text}");
        }
    }
    assert!(owner_views::EMPTY_SERVICES.contains("restarts it if it stops"));
    assert!(owner_views::EMPTY_ROUTINES.contains("on a schedule"));
    assert!(owner_views::EMPTY_GOALS.contains("budget"));
}

#[test]
fn cron_in_words_for_common_patterns_else_raw() {
    let w = |c: &str| owner_views::cron_words(c);
    for (cron, words) in [
        ("0 9 * * *", "daily 09:00"),
        ("30 2 * * *", "daily 02:30"),
        ("0 9 * * 1-5", "weekdays 09:00"),
        ("0 9 * * MON-FRI", "weekdays 09:00"),
        ("0 10 * * 6,0", "weekends 10:00"),
        ("0 10 * * 1,4", "Mon, Thu 10:00"),
        ("0 10 * * thu,mon", "Mon, Thu 10:00"),
        ("0 8 * * 7", "Sun 08:00"),
        ("0 8 * * 0-6", "daily 08:00"),
        ("0 9,17 * * *", "daily 09:00, 17:00"),
        ("0 */6 * * *", "every 6h"),
        ("15 */2 * * *", "every 2h at :15"),
        ("0 * * * *", "hourly"),
        ("5 * * * *", "hourly at :05"),
        ("*/15 * * * *", "every 15m"),
        ("0 9 1 * *", "monthly on day 1 09:00"),
    ] {
        assert_eq!(w(cron).as_deref(), Some(words), "{cron}");
    }
    for raw in [
        "0 9 * 1 *",
        "0 9 1 * 1",
        "0 9-17 * * *",
        "0 0 9 * * *",
        "@daily",
        "0 25 * * *",
        "x y z w v",
    ] {
        assert_eq!(w(raw), None, "{raw}");
    }
    // The owner's zone is implied; another zone is named; raw cron in <code>.
    assert_eq!(
        owner_views::schedule_words("0 9 * * *", "Asia/Taipei"),
        "daily 09:00"
    );
    assert_eq!(
        owner_views::schedule_words("30 2 * * *", "UTC"),
        "daily 02:30 UTC"
    );
    assert_eq!(
        owner_views::schedule_words("0 9-17 * * *", "Asia/Taipei"),
        "<code>0 9-17 * * *</code>"
    );
}

#[test]
fn relative_times_and_short_clock() {
    let tz = owner_views::OWNER_TZ;
    // MON is 08:00 in Taipei.
    assert_eq!(owner_views::relative(MON + 35 * 60, MON), "in 35m");
    assert_eq!(owner_views::relative(MON - 2 * 3600, MON), "2h ago");
    assert_eq!(owner_views::relative(MON + 3 * DAY, MON), "in 3d");
    assert_eq!(owner_views::clock(MON + 3600, tz, MON), "09:00");
    assert_eq!(owner_views::clock(MON + DAY, tz, MON), "Tue 08:00");
    assert_eq!(owner_views::clock(MON - 9 * 3600, tz, MON), "Sun 23:00");
    assert_eq!(owner_views::clock(MON + 10 * DAY, tz, MON), "Jan 15 08:00");
}

#[test]
fn status_is_a_headline_wakes_and_sorted_schedules() {
    let (_d, mut c) = setup();
    let now = MON; // 08:00 in Taipei
                   // Not the creation wake (at the real time).
    c.execute("DELETE FROM bot_wakes", []).unwrap();
    let out = owner_views::status(&c, "a", now).unwrap();
    assert_eq!(
        out,
        "<b>a · active</b> · idle\nNo wakes queued · no runs yet"
    );
    let new = |name: &str, cron: &str, tz: &str| axe::schedule::NewSchedule {
        name: name.into(),
        cron: cron.into(),
        timezone: tz.into(),
        objective: "look".into(),
    };
    for (name, cron, tz) in [
        ("review", "0 10 * * 1,4", "Asia/Taipei"),
        ("health", "0 */6 * * *", "Asia/Taipei"),
        ("nightly", "30 2 * * *", "UTC"),
        ("odd", "0 9-17 * * *", "Asia/Taipei"),
    ] {
        axe::schedule::add(&mut c, &human(), "a", &new(name, cron, tz), "w", now).unwrap();
    }
    axe::schedule::change(&mut c, &human(), "a", "nightly", "pause", "w", now).unwrap();
    // Two queued wakes, the earliest at 08:35; the last run ended 07:05.
    bot::add_wake(&mut c, &human(), "a", now + 35 * 60, "x", None, now).unwrap();
    bot::add_wake(&mut c, &human(), "a", now + 3600, "y", None, now).unwrap();
    bot_run(&c, 1, 1, now - 3600, None);
    c.execute(
        "UPDATE bot_runs SET ended_at=?1",
        params![ts(now - 55 * 60)],
    )
    .unwrap();
    let out = owner_views::status(&c, "a", now).unwrap();
    assert_eq!(
        out,
        "<b>a · active</b> · idle\n\
         Next wake 08:35 (in 35m) · 2 queued · last run 07:05 (55m ago)\n\
         \n\
         <b>Schedules</b>\n\
         • odd · <code>0 9-17 * * *</code> · next in 1h\n\
         • review · Mon, Thu 10:00 · next in 2h\n\
         • health · every 6h · next in 4h\n\
         • nightly · daily 02:30 UTC · <i>paused</i>"
    );
    assert!(
        !out.contains("Z ") && !out.contains("T0"),
        "no RFC3339: {out}"
    );
    assert!(owner_views::status(&c, "b", now)
        .unwrap()
        .find("Schedules")
        .is_none());
}

#[test]
fn status_shows_the_live_run_and_attention() {
    let (_d, c) = setup();
    let now = MON;
    c.execute("DELETE FROM bot_wakes", []).unwrap();
    c.execute(
        "INSERT INTO bot_runs (id, bot_id, agent, mode, session_id, claim, dispatcher, generation,
           started_at, log_path, current_activity)
         VALUES (1, 1, 'claude', 'resume', 'b', 'c', 'd', 'g', ?1, 'x', 'reading <PR> 12')",
        params![ts(now - 4 * 60)],
    )
    .unwrap();
    let out = owner_views::status(&c, "a", now).unwrap();
    assert!(
        out.starts_with("<b>a · active</b> · running: reading &lt;PR&gt; 12, 4m\n"),
        "{out}"
    );
    c.execute("UPDATE bot_runs SET ended_at=?1", params![ts(now - 60)])
        .unwrap();
    c.execute(
        "UPDATE bots SET state='paused', failures=?1 WHERE name='a'",
        params![bot::FAILURES_PAUSE],
    )
    .unwrap();
    let out = owner_views::status(&c, "a", now).unwrap();
    assert_eq!(
        out,
        format!(
            "<b>a · paused</b> · idle\n\
             Needs attention: {} failed runs in a row\n\
             No wakes queued · last run 07:59 (1m ago)",
            bot::FAILURES_PAUSE
        )
    );
}

/// `/node` with no sample says so; an old sample is marked stale.
#[test]
fn node_view_without_a_fresh_sample() {
    let (_d, c) = setup();
    let server = axe::server::version().version;
    let empty = owner_views::node(&c, Some(WEB), now()).unwrap();
    assert!(
        empty.starts_with(&format!(
            "<b>Node · no samples yet</b>\naxe server <code>{server}</code> · node <code>?</code> · CLI <code>?</code>\n"
        )),
        "{empty}"
    );
    assert!(empty.ends_with("<a href=\"http://axe.example/?view=node\">Open node</a>"));
    let s = axe::node_metrics::Sample {
        cpu_percent: Some(5.0),
        node_version: Some("0.5.4 (5437bf3602b8)".into()),
        cli_version: Some("0.5.3 (7467135d8cb8)".into()),
        runs: axe::node_metrics::Runs {
            bot: [("a".to_string(), 1)].into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let at = now() - 3 * 3600;
    axe::node_metrics::record(&c, "n1", at, s, &Default::default()).unwrap();
    let stale = owner_views::node(&c, None, now()).unwrap();
    assert_eq!(
        stale,
        format!(
            "<b>Node · stale</b> · last sample 3h ago\n\
             axe server <code>{server}</code> · node <code>0.5.4 (5437bf3602b8)</code> · CLI <code>0.5.3 (7467135d8cb8)</code>\n\
             CPU <code>5%</code>\n\n<b>Running</b>\n\
             Tasks <code>0</code>\nBots <code>1</code>: a\nServices <code>0</code>\nScheduled <code>0</code>"
        )
    );
}
