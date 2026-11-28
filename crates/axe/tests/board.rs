//! The board as a core primitive (`board.rs`): what it derives from tasks,
//! cards and goals, how the bot's items merge into those lines, the verbs
//! and their caps, per-chat views, and the v25 -> v26 migration.

use axe::board::{self, ItemEdit, Links, NewItem, Park, ViewEdit};
use axe::bot::{self, NewBot, NewMessage};
use axe::db::{self, Actor, NewTask};
use axe::goal::{self, NewGoal};
use axe::subscription::{self, Change};
use rusqlite::Connection;
use serde_json::json;

/// 2027-01-15T08:00:00Z.
const T0: i64 = 1_800_000_000;

fn human() -> Actor {
    Actor::new("human")
}

fn setup() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    for name in ["cos", "other"] {
        let b = NewBot {
            name: name.into(),
            project: "p".into(),
            agent: Some("fake".into()),
            identity: String::new(),
            conduct: String::new(),
            notes: String::new(),
            heartbeat: None,
            nudge_after: None,
        };
        bot::create(&mut c, &human(), &b, T0).unwrap();
    }
    let sub = Change {
        project: Some("p".into()),
        ..Default::default()
    };
    subscription::subscribe(&mut c, &human(), "cos", &sub, "test", T0).unwrap();
    (dir, c)
}

fn task(c: &mut Connection, title: &str) -> i64 {
    let t = NewTask {
        agent: None,
        project: "p",
        title,
        detail: "d",
        state: "pending",
    };
    db::add_task(c, &human(), &t).unwrap()
}

fn escalate(c: &mut Connection, id: i64) {
    db::escalate(
        c,
        &human(),
        id,
        "needs a call",
        "owner: decide",
        "decided",
        "decision",
    )
    .unwrap();
}

/// An outbound card (a text message with buttons) of bot `cos`.
fn card(c: &mut Connection, text: &str, task: Option<i64>) -> i64 {
    let mut flags = json!({"buttons": [["Yes", "yes"]], "html": false});
    if let Some(t) = task {
        flags["task"] = json!(t);
    }
    let m = NewMessage {
        direction: "out".into(),
        chat_id: 111,
        message_id: 9000 + text.len() as i64,
        kind: "text".into(),
        text: text.into(),
        flags,
        ..Default::default()
    };
    bot::record_message(c, "cos", &m, T0).unwrap().0
}

fn no_blob(_: &str) -> Option<String> {
    None
}

fn show(c: &Connection, done_since: i64) -> board::Board {
    let blob = |sha: &str| Some(format!("http://blob/{sha}"));
    let links = Links {
        web: Some("http://axe/"),
        blob: &blob,
    };
    board::board(c, "cos", done_since, T0 + 86_400 * 3, &links).unwrap()
}

/// `section: kind id text [note]` per line, for compact asserts.
fn lines(b: &board::Board) -> Vec<String> {
    b.sections
        .iter()
        .flat_map(|s| {
            s.lines.iter().map(move |l| {
                let note = l
                    .note
                    .as_deref()
                    .map(|n| format!(" [{n}]"))
                    .unwrap_or_default();
                format!("{}: {} {} {}{note}", s.title, l.kind, l.id, l.text)
            })
        })
        .collect()
}

fn add(c: &mut Connection, text: &str, r: Option<&str>, section: Option<&str>) -> board::Item {
    let n = NewItem {
        text: text.into(),
        reference: r.map(str::to_string),
        section: section.map(str::to_string),
        until: None,
    };
    board::add(c, &Actor::new("bot:cos"), "cos", &n, T0).unwrap()
}

#[test]
fn derives_decide_and_goals_and_merges_items_by_reference() {
    let (_d, mut c) = setup();
    let t1 = task(&mut c, "Deploy the site");
    let t2 = task(&mut c, "Pick a host");
    let t3 = task(&mut c, "Write the notes");
    escalate(&mut c, t1);
    escalate(&mut c, t2);
    // A card for t2 stands for it; an unrelated card is its own line.
    let c2 = card(&mut c, "Host A or B?", Some(t2));
    let ask = card(&mut c, "Ship it?", None);
    let g = goal::propose(
        &mut c,
        &Actor::new("bot:cos"),
        &NewGoal {
            name: "bot-cost".into(),
            project: "p".into(),
            intent: "Cut cost.".into(),
            ..Default::default()
        },
        None,
        T0,
    )
    .unwrap();
    goal::change(&mut c, &human(), "bot-cost", "activate", "go", None).unwrap();

    // Items: one names escalated t1 (a note on its Decide line), one the
    // goal (a note), one the card, one a pending task, one plain, and one in
    // a named section.
    add(&mut c, "waits on your CI host", Some("task:1"), None);
    add(&mut c, "on track", Some(&format!("goal:{}", g.id)), None);
    add(&mut c, "", Some(&format!("card:{ask}")), None);
    let notes = add(&mut c, "", Some(&format!("task:{t3}")), None);
    add(&mut c, "Log retention cap", None, None);
    add(&mut c, "Contract", None, Some("Waiting on you"));

    let b = show(&c, 0);
    assert_eq!(
        lines(&b),
        [
            format!("Decide: task {t1} Deploy the site [waits on your CI host]"),
            format!("Decide: card {c2} Host A or B?"),
            format!("Decide: card {ask} Ship it?"),
            format!("Goals: goal {} bot-cost [on track]", g.id),
            "Waiting on you: item 6 Contract".to_string(),
            format!("Items: item {} Write the notes", notes.id),
            "Items: item 5 Log retention cap".to_string(),
        ],
        "{b:#?}"
    );
    assert_eq!(b.decide, 3);
    assert_eq!(b.open_items, 6);
    let decide = &b.sections[0].lines;
    assert_eq!(
        decide[0].link.as_deref(),
        Some(&*format!("http://axe/?task={t1}"))
    );
    assert_eq!(decide[1].task, Some(t2), "the card stands for its task");
    assert_eq!(
        decide[1].link_label.as_deref(),
        Some(&*format!("task #{t2}"))
    );
    // The derived title is read at render time: a rename shows at once.
    c.execute(
        "UPDATE tasks SET title='Write the release notes' WHERE id=?1",
        [t3],
    )
    .unwrap();
    assert!(
        lines(&show(&c, 0)).contains(&format!("Items: item {} Write the release notes", notes.id))
    );
    // A task that closes closes its item: it shows as done today, then not.
    c.execute(
        "UPDATE tasks SET state='done', updated_at=?1 WHERE id=?2",
        rusqlite::params![bot::ts(T0 + 86_400), t3],
    )
    .unwrap();
    let b = show(&c, T0);
    assert!(
        lines(&b).contains(&format!(
            "Done today: item {} Write the release notes",
            notes.id
        )),
        "{:?}",
        lines(&b)
    );
    assert!(!lines(&show(&c, T0 + 2 * 86_400))
        .iter()
        .any(|l| l.contains("release notes")));
}

#[test]
fn links_are_built_at_render_time() {
    let (_d, mut c) = setup();
    let sha = "a".repeat(64);
    add(
        &mut c,
        "Core fixes",
        Some("doc:harvis/docs/x y.md"),
        Some("review"),
    );
    add(&mut c, "Report", Some(&format!("blob:{sha}")), None);
    add(&mut c, "Site", Some("https://example.com/a"), None);
    let b = show(&c, 0);
    let all: Vec<&board::BoardLine> = b.sections.iter().flat_map(|s| &s.lines).collect();
    assert_eq!(
        b.sections[0].title, "Review",
        "any case of review is Review"
    );
    assert_eq!(
        all[0].link.as_deref(),
        Some("http://axe/doc?path=harvis%2Fdocs%2Fx%20y.md")
    );
    assert_eq!(all[0].link_label.as_deref(), Some("doc"));
    assert_eq!(all[1].link.as_deref(), Some(&*format!("http://blob/{sha}")));
    assert_eq!(all[2].link.as_deref(), Some("https://example.com/a"));
    // Without a blob store or public URL there is no link, only the label.
    let links = Links {
        web: None,
        blob: &no_blob,
    };
    let b = board::board(&c, "cos", 0, T0, &links).unwrap();
    assert!(b
        .sections
        .iter()
        .flat_map(|s| &s.lines)
        .all(|l| l.link.is_none()
            || l.kind == "item" && l.reference.as_deref() == Some("https://example.com/a")));
}

#[test]
fn verbs_tick_drop_reopen_edit_and_move_with_events() {
    let (_d, mut c) = setup();
    let bot_actor = Actor::new("bot:cos");
    let a = add(&mut c, "First", None, None);
    let b = add(&mut c, "Second", None, None);
    board::edit(
        &mut c,
        &bot_actor,
        "cos",
        b.id,
        &ItemEdit {
            pos: Some(0),
            ..Default::default()
        },
        T0,
    )
    .unwrap();
    assert_eq!(
        lines(&show(&c, 0)),
        ["Items: item 2 Second", "Items: item 1 First"]
    );
    let e = board::edit(
        &mut c,
        &bot_actor,
        "cos",
        a.id,
        &ItemEdit {
            text: Some("  First,   reworded ".into()),
            until: Some("2027-01-16".into()),
            ..Default::default()
        },
        T0,
    )
    .unwrap();
    assert_eq!(e.text, "First, reworded");
    assert_eq!(e.until.as_deref(), Some("2027-01-16T23:59:59Z"));
    assert!(
        show(&c, 0).sections[0].lines[1].overdue,
        "past until: overdue, still shown"
    );
    board::set_state(&mut c, &human(), "cos", a.id, "done", "ticked", T0 + 10).unwrap();
    assert_eq!(
        lines(&show(&c, T0)),
        ["Items: item 2 Second", "Done today: item 1 First, reworded"]
    );
    board::set_state(&mut c, &bot_actor, "cos", b.id, "drop", "stale", T0 + 20).unwrap();
    board::set_state(&mut c, &bot_actor, "cos", a.id, "reopen", "", T0 + 30).unwrap();
    assert_eq!(lines(&show(&c, T0)), ["Items: item 1 First, reworded"]);
    let actions: Vec<String> = bot::events(&c, "cos", 50)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "board")
        .map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.body).unwrap()["action"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    for a in ["add", "edit", "done", "drop", "reopen"] {
        assert!(actions.contains(&a.to_string()), "{actions:?}");
    }
    // Another bot's item is not this bot's.
    let err = board::set_state(&mut c, &human(), "other", a.id, "done", "", T0).unwrap_err();
    assert!(
        format!("{err:#}").contains("not found for bot other"),
        "{err:#}"
    );
}

#[test]
fn caps_refs_and_sections_are_checked() {
    let (_d, mut c) = setup();
    let err = |c: &mut Connection, n: NewItem| {
        format!("{:#}", board::add(c, &human(), "cos", &n, T0).unwrap_err())
    };
    let text = |t: &str| NewItem {
        text: t.into(),
        ..Default::default()
    };
    assert!(err(&mut c, text("  ")).contains("needs text"));
    assert!(err(&mut c, text(&"x".repeat(201))).contains("max 200"));
    for r in [
        "task:999",
        "goal:nope",
        "card:999",
        "doc:",
        "blob:xyz",
        "ftp://x",
        "nonsense",
    ] {
        let e = err(
            &mut c,
            NewItem {
                text: "t".into(),
                reference: Some(r.into()),
                ..Default::default()
            },
        );
        assert!(!e.is_empty(), "{r}");
    }
    // Duplicate refs in one section are refused.
    let t = task(&mut c, "T");
    add(&mut c, "", Some(&format!("task:{t}")), None);
    assert!(err(
        &mut c,
        NewItem {
            reference: Some(format!("task:#{t}")),
            ..Default::default()
        }
    )
    .contains("already refers to"));
    // Three named sections at most.
    for s in ["A", "B", "C"] {
        add(&mut c, "x", None, Some(s));
    }
    assert!(err(
        &mut c,
        NewItem {
            text: "x".into(),
            section: Some("D".into()),
            ..Default::default()
        }
    )
    .contains("3 named sections"));
    add(&mut c, "x", None, Some("A"));
    // Fifty open items at most; a drop makes room.
    let open = board::items(&c, "cos").unwrap().len();
    for i in open..board::MAX_OPEN {
        add(&mut c, &format!("item {i}"), None, None);
    }
    assert!(err(&mut c, text("one more")).contains("50 open board items"));
    board::set_state(&mut c, &human(), "cos", 1, "drop", "", T0).unwrap();
    add(&mut c, "one more", None, None);
}

#[test]
fn park_moves_a_decision_to_review_and_closes_its_card() {
    let (_d, mut c) = setup();
    let t1 = task(&mut c, "Deploy");
    let t2 = task(&mut c, "Host");
    escalate(&mut c, t1);
    escalate(&mut c, t2);
    let c2 = card(&mut c, "Host A or B?", Some(t2));
    let ask = card(&mut c, "Ship it?", None);
    let owner = human();
    // A task parks with its title; parking it again returns the same item.
    let p1 = board::park(
        &mut c,
        &owner,
        "cos",
        &Park {
            task: Some(t1),
            ..Default::default()
        },
        T0,
    )
    .unwrap();
    let again = board::park(
        &mut c,
        &owner,
        "cos",
        &Park {
            task: Some(t1),
            ..Default::default()
        },
        T0,
    )
    .unwrap();
    assert_eq!(p1.id, again.id);
    // A task's card parks its task (and closes); a plain card parks itself.
    let p2 = board::park(
        &mut c,
        &owner,
        "cos",
        &Park {
            card: Some(c2),
            ..Default::default()
        },
        T0,
    )
    .unwrap();
    assert_eq!(p2.source.as_deref(), Some(&*format!("task:{t2}")));
    let p3 = board::park(
        &mut c,
        &owner,
        "cos",
        &Park {
            card: Some(ask),
            reference: Some("doc:notes/ship.md".into()),
            ..Default::default()
        },
        T0,
    )
    .unwrap();
    assert_eq!(p3.reference.as_deref(), Some("doc:notes/ship.md"));
    let b = show(&c, 0);
    assert_eq!(
        lines(&b),
        [
            format!("Review: item {} Deploy", p1.id),
            format!("Review: item {} Host A or B?", p2.id),
            format!("Review: item {} Ship it?", p3.id),
        ]
    );
    let flags = axe::channel::get_message(&c, "cos", ask).unwrap().flags;
    assert_eq!(flags["closed"], "parked for review");
    assert_eq!(flags["parked"], p3.id);
    assert!(flags["buttons"].is_null(), "the card closed");
    assert!(
        board::park_wake_note(&p1).contains(&format!("axe bot board edit {} --ref doc:", p1.id))
    );
    // Deciding the review item brings nothing back: the task is still
    // escalated, so it is on Decide again.
    board::set_state(&mut c, &owner, "cos", p1.id, "done", "", T0).unwrap();
    assert!(lines(&show(&c, T0 + 86_400 * 3)).contains(&format!("Decide: task {t1} Deploy")));
}

#[test]
fn views_are_per_chat_with_pin_buttons_and_repost() {
    let (_d, mut c) = setup();
    assert!(board::default_view(&mut c, "cos", "telegram", 111, 0, T0).unwrap());
    assert!(
        !board::default_view(&mut c, "cos", "telegram", 222, 0, T0).unwrap(),
        "only the first"
    );
    let v = board::views(&c, "cos").unwrap();
    assert_eq!(v.len(), 1);
    assert!(v[0].pin && v[0].keyboard);
    assert_eq!(v[0].repost.as_deref(), Some(board::DEFAULT_REPOST));
    let off = ViewEdit {
        pin: Some(false),
        ..Default::default()
    };
    board::set_view(&mut c, &human(), "cos", "telegram", 111, 0, &off, T0).unwrap();
    assert!(
        board::home(&c, "cos").unwrap().is_none(),
        "no pin on: no home chat"
    );
    let group = ViewEdit {
        keyboard: Some(false),
        repost: Some("off".into()),
        ..Default::default()
    };
    let g = board::set_view(&mut c, &human(), "cos", "telegram", -500, 7, &group, T0).unwrap();
    assert!(g.pin && !g.keyboard && g.repost.is_none());
    assert_eq!(board::home(&c, "cos").unwrap().unwrap().chat_id, -500);
    let bad = ViewEdit {
        repost: Some("every day".into()),
        ..Default::default()
    };
    assert!(board::set_view(&mut c, &human(), "cos", "telegram", 111, 0, &bad, T0).is_err());
    assert!(board::set_view(
        &mut c,
        &human(),
        "cos",
        "slack",
        1,
        0,
        &ViewEdit::default(),
        T0
    )
    .is_err());
    // Repost: due at the first cadence time after the post.
    let v = &board::views(&c, "cos").unwrap()[0];
    // T0 is Fri 08:00Z; the next 10:00 UTC workday slot is the same day.
    assert!(!board::repost_due(v, "UTC", T0, T0 + 3600));
    assert!(board::repost_due(v, "UTC", T0, T0 + 2 * 3600));
    assert_eq!(board::day_start("UTC", T0), T0 - 8 * 3600);
    assert_eq!(board::day_start("Asia/Taipei", T0), T0 - 16 * 3600);
}

/// v26 only adds the board tables and one Telegram view per old inbox
/// chat (the latest message with `flags.board`); a database lowered to v25
/// keeps its views and gets no duplicates.
#[test]
fn migrates_v25_inbox_chats_into_views() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v25.db");
    let (_d, c0) = setup();
    drop(c0);
    let mut c = db::open(&path).unwrap();
    let b = NewBot {
        name: "cos".into(),
        project: "p".into(),
        agent: Some("fake".into()),
        identity: String::new(),
        conduct: String::new(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    };
    bot::create(&mut c, &human(), &b, T0).unwrap();
    for (msg, chat, thread, flags) in [
        (1, 111, None, json!({"board": true, "board_retired": true})),
        (2, -500, Some(9), json!({"board": true})),
        (
            3,
            111,
            None,
            json!({"card_pin": true, "pinned": true, "buttons": [["A", "a"]]}),
        ),
    ] {
        let m = NewMessage {
            direction: "out".into(),
            chat_id: chat,
            thread_id: thread,
            message_id: msg,
            kind: "text".into(),
            text: "x".into(),
            flags,
            ..Default::default()
        };
        bot::record_message(&mut c, "cos", &m, T0).unwrap();
    }
    c.execute_batch("DROP TABLE board_items; DROP TABLE board_views; PRAGMA user_version=25;")
        .unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    let version: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 26);
    let v = board::views(&c, "cos").unwrap();
    assert_eq!(v.len(), 1, "the latest board message's chat: {v:?}");
    assert_eq!((v[0].chat_id, v[0].thread_id, v[0].pin), (-500, 9, true));
    // The old pins wait for the gateway to unpin them.
    let legacy: Vec<i64> = axe::channel::legacy_pins(&c, "cos")
        .unwrap()
        .iter()
        .map(|m| m.message_id)
        .collect();
    assert_eq!(legacy, [1, 2, 3]);
    c.pragma_update(None, "user_version", 25).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(board::views(&c, "cos").unwrap().len(), 1);
    // Once cleared, a message is no longer a legacy pin.
    axe::channel::clear_legacy_pin(&c, 3, true).unwrap();
    let f = axe::channel::get_message(&c, "cos", 3).unwrap().flags;
    assert!(
        f["card_pin"].is_null() && f["pinned"] == false && f["legacy_pin"] == true,
        "{f}"
    );
}

mod common;

/// The API and CLI are thin: `axe bot board` writes through the core verbs,
/// and a dashboard read session sees the board with cards as ids only.
#[test]
fn the_cli_and_api_drive_the_verbs_and_a_read_session_sees_no_card_text() {
    let dir = tempfile::tempdir().unwrap();
    let srv = common::Server::start(dir.path());
    let f = dir.path().join("bot.json");
    std::fs::write(&f, json!({"name": "cos", "project": "p"}).to_string()).unwrap();
    srv.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    srv.ok(&["bot", "subscribe", "cos", "--why", "test"]);
    let b = |args: &[&str]| {
        let mut a = vec!["bot", "board", "--bot", "cos"];
        a.extend_from_slice(args);
        srv.ok(&a)
    };
    assert_eq!(b(&["show"]), "Nothing on the board.\n");
    assert_eq!(
        b(&["add", "Contract", "--section", "Waiting on you"]),
        "added item 1: Contract\n"
    );
    assert_eq!(
        b(&["add", "Log cap", "--until", "3d"]),
        "added item 2: Log cap\n"
    );
    assert_eq!(b(&["move", "2", "0"]), "moved item 2: Log cap\n");
    assert_eq!(
        b(&["edit", "1", "--text", "Contract signed?"]),
        "edited item 1: Contract signed?\n"
    );
    assert_eq!(
        b(&["done", "2", "--why", "set"]),
        "ticked item 2: Log cap\n"
    );
    assert_eq!(
        b(&["park", "Pricing", "--ref", "https://example.com/p"]),
        "parked item 3: Pricing\n"
    );
    let show = b(&["show"]);
    assert_eq!(
        show,
        "Review\n  item 3  Pricing · link · https://example.com/p\n\
         Waiting on you\n  item 1  Contract signed?\n\
         Done today\n  item 2  Log cap\n",
        "{show}"
    );
    let out = srv.run(&["bot", "board", "--bot", "cos", "add", ""]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs text"),
        "{out:?}"
    );
    let out = srv.run(&["bot", "board", "--bot", "cos", "view", "--pin", "on"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--chat"),
        "{out:?}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&b(&["view", "--chat", "111", "--buttons", "off"])).unwrap();
    assert_eq!((&v["pin"], &v["keyboard"]), (&json!(true), &json!(false)));

    // A card (stored as the gateway would) shows its text with the token,
    // and only its id to a read session.
    let mut c = db::open(&srv.db).unwrap();
    card(&mut c, "Secret plan?", None);
    drop(c);
    let full = srv.get("/api/bots/cos/board");
    assert_eq!(full["sections"][0]["lines"][0]["text"], "Secret plan?");
    let session = ureq::post(&format!("{}/api/session", srv.url))
        .set("Authorization", &format!("Bearer {}", common::TOKEN))
        .call()
        .unwrap();
    let cookie = session
        .header("set-cookie")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let read: serde_json::Value = ureq::get(&format!("{}/api/bots/cos/board", srv.url))
        .set("Cookie", &cookie)
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(read["sections"][0]["lines"][0]["text"], "Card #1", "{read}");
    assert!(!read.to_string().contains("Secret plan"));

    // The web board's presses (task 2044): a read session may tick, reopen
    // and park from this server's own pages, recorded as the owner, and
    // nothing else; another site, or no Origin, is refused.
    let host = srv.url.trim_start_matches("http://").to_string();
    let press = |path: &str, origin: Option<&str>, body: serde_json::Value| {
        let mut r = ureq::post(&format!("{}{path}", srv.url)).set("Cookie", &cookie);
        if let Some(o) = origin {
            r = r.set("Origin", o);
        }
        match r.send_json(body) {
            Ok(r) => (r.status(), r.into_json::<serde_json::Value>().unwrap()),
            Err(ureq::Error::Status(s, r)) => (s, r.into_json().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    };
    let own = format!("http://{host}");
    let (status, v) = press(
        "/api/bots/cos/board/items/1/done",
        Some(&own),
        json!({"actor": "bot:cos", "why": "ticked on the web board"}),
    );
    assert_eq!((status, &v["state"]), (200, &json!("done")), "{v}");
    let actor: String = db::open(&srv.db)
        .unwrap()
        .query_row(
            "SELECT actor FROM bot_events WHERE kind='board' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, "owner", "a session cannot name another actor");
    assert_eq!(
        press("/api/bots/cos/board/items/1/reopen", Some(&own), json!({})).0,
        200
    );
    for (path, origin) in [
        ("/api/bots/cos/board/items/1/done", Some("http://evil.test")),
        ("/api/bots/cos/board/items/1/done", None),
        ("/api/bots/cos/board/items/1/drop", Some(own.as_str())),
        ("/api/bots/cos/board/items", Some(own.as_str())),
    ] {
        assert_eq!(
            press(path, origin, json!({"text": "x"})).0,
            401,
            "{path} {origin:?}"
        );
    }
    let (status, v) = press("/api/bots/cos/board/park", Some(&own), json!({"card": 1}));
    assert_eq!(status, 201, "{v}");
    let woken: i64 = db::open(&srv.db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM bot_wakes WHERE key LIKE 'wake:board-park-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(woken, 1, "a web park asks the bot for its doc");
}
