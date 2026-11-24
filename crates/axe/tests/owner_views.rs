//! The read-only owner commands `/tasks`, `/task N` and `/cost`
//! (`owner_views`): which tasks a bot lists and in what order, the empty and
//! truncated replies, one task's view, and the 7-day cost of one bot.
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
        "bot a cost, last 7 days: $0.00\nno runs\n\
         <a href=\"http://axe.example/?view=cost&amp;days=7&amp;cost_bot=a\">Open cost</a>"
    );

    let today = now.div_euclid(DAY) * DAY;
    bot_run(&c, 1, 1, now - 60, Some(1.25));
    bot_run(&c, 2, 1, now - 120, None);
    bot_run(&c, 3, 1, today - DAY + 60, Some(0.5));
    // Outside the window, or another bot's: not counted.
    bot_run(&c, 4, 1, today - COST_DAYS * DAY + 60 - DAY, Some(100.0));
    bot_run(&c, 5, 2, now - 60, Some(7.0));
    let day = |t: i64| ts(t)[..10].to_string();
    let out = owner_views::cost(&c, "a", None, now).unwrap();
    assert_eq!(
        out,
        format!(
            "bot a cost, last 7 days: $1.75 over 3 runs\n\
             1 with unknown usage: the figure is a lower bound\n\
             {}  $1.25  2 runs\n{}  $0.50  1 run",
            day(now),
            day(today - DAY + 60)
        )
    );
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
