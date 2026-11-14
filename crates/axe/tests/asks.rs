//! Owner asks (`ask.rs`): escalation episodes and bot cards, ratings, the
//! wasted-ask share per bot and week, and the retro card's content.
mod common;

use axe::ask::{self, Filter};
use axe::bot::{self, ts, NewBot, NewMessage};
use axe::db::{self, Actor, Changes, NewTask};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const DAY: i64 = 86_400;
const NOTE: &str = "blocked: x\nresolver: owner: decide\nresume: y";

fn now() -> i64 {
    axe::remote::epoch()
}

fn owner() -> Actor {
    Actor::new("owner")
}

fn setup() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("t.db")).unwrap();
    for (name, project) in [("a", "p"), ("b", "q")] {
        let b = NewBot {
            name: name.into(),
            project: project.into(),
            agent: Some("fake".into()),
            identity: String::new(),
            conduct: String::new(),
            notes: String::new(),
            heartbeat: None,
            nudge_after: None,
        };
        bot::create(&mut c, &Actor::new("human"), &b, now()).unwrap();
    }
    (dir, c)
}

fn task(c: &mut Connection, project: &str, title: &str, owner: Option<&str>) -> i64 {
    let t = NewTask {
        agent: None,
        project,
        title,
        detail: "",
        state: "pending",
    };
    let id = db::add_task(c, &Actor::new("human"), &t).unwrap();
    if let Some(o) = owner {
        let ch = Changes {
            owner: Some(o.into()),
            ..Default::default()
        };
        db::set_task(c, &Actor::new("human"), id, &ch).unwrap();
    }
    id
}

fn set_state(c: &mut Connection, id: i64, state: &str) {
    let ch = Changes {
        state: Some(state.into()),
        note: (state == "escalated").then(|| NOTE.into()),
        ..Default::default()
    };
    db::set_task(c, &Actor::new("agent"), id, &ch).unwrap();
}

/// An outbound message of bot `name` with `flags`; returns its local id.
fn message(c: &mut Connection, name: &str, mid: i64, text: &str, flags: Value) -> i64 {
    let m = NewMessage {
        direction: "out".into(),
        chat_id: 1,
        message_id: mid,
        kind: "text".into(),
        text: text.into(),
        attachments: json!([]),
        flags,
        ..Default::default()
    };
    bot::record_message(c, name, &m, now()).unwrap().0
}

fn all(c: &Connection) -> Vec<ask::Ask> {
    ask::list(
        c,
        &Filter {
            since: 0,
            until: now() + 1,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn asks_are_escalation_episodes_and_cards_and_take_ratings() {
    let (_d, mut c) = setup();
    let owned = task(&mut c, "p", "ship it", Some("bot:a"));
    set_state(&mut c, owned, "escalated");
    set_state(&mut c, owned, "pending");
    set_state(&mut c, owned, "escalated");
    // No bot owner: the bot that posted its card is the asker.
    let carded = task(&mut c, "q", "carded", None);
    set_state(&mut c, carded, "escalated");
    message(
        &mut c,
        "b",
        10,
        "task card",
        json!({"buttons": [["Go","go"]], "task": carded}),
    );
    // A card ask (flags.ask survives closing), a legacy card with buttons,
    // plain text, the retro card and the board are not asks or are.
    let card = message(
        &mut c,
        "a",
        11,
        "<b>Deploy</b> now?\nmore",
        json!({"buttons": [], "html": true, "ask": true}),
    );
    let legacy = message(&mut c, "b", 12, "Old?", json!({"buttons": [["Y","y"]]}));
    message(&mut c, "a", 13, "just text", json!({}));
    message(
        &mut c,
        "a",
        14,
        "retro",
        json!({"buttons": [["1","r"]], "retro": {"items": []}}),
    );
    message(
        &mut c,
        "a",
        15,
        "board",
        json!({"buttons": [["1","r"]], "board": true}),
    );

    let asks = all(&c);
    let ids: Vec<_> = asks.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            format!("t{owned}.1"),
            format!("t{owned}.2"),
            format!("t{carded}.1"),
            format!("m{card}"),
            format!("m{legacy}"),
        ]
    );
    assert_eq!(asks[0].bot.as_deref(), Some("a"));
    assert_eq!(asks[0].summary, "ship it");
    assert_eq!(asks[2].bot.as_deref(), Some("b"));
    assert_eq!(asks[3].summary, "Deploy now?");
    assert_eq!(asks[3].kind, "card");
    assert!(asks.iter().all(|a| a.rating.is_none()));

    // Rating: by id, by a bare task id (its latest episode), latest wins.
    let r = ask::rate(&mut c, &owner(), &format!("t{owned}.1"), "needed").unwrap();
    assert_eq!(r.rating.as_deref(), Some("needed"));
    assert_eq!(r.rated_by.as_deref(), Some("owner"));
    assert!(r.rated_at.is_some());
    let r = ask::rate(&mut c, &owner(), &format!("#{owned}"), "could_decide").unwrap();
    assert_eq!(r.id, format!("t{owned}.2"));
    let r = ask::rate(&mut c, &owner(), &format!("m{card}"), "needed").unwrap();
    assert_eq!(r.rating.as_deref(), Some("needed"));
    let r = ask::rate(&mut c, &owner(), &format!("m{card}"), "should_not_exist").unwrap();
    assert_eq!(r.rating.as_deref(), Some("should_not_exist"));

    let unrated = ask::list(
        &c,
        &Filter {
            since: 0,
            until: now() + 1,
            unrated: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(unrated.len(), 2);
    let by_b = ask::list(
        &c,
        &Filter {
            since: 0,
            until: now() + 1,
            bot: Some("b"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_b.len(), 2);
    let in_p = ask::list(
        &c,
        &Filter {
            since: 0,
            until: now() + 1,
            project: Some("p"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(in_p.len(), 3, "two episodes of p's task and bot a's card");

    // Refusals.
    let err = |r: anyhow::Result<ask::Ask>| format!("{:#}", r.unwrap_err());
    assert!(err(ask::rate(
        &mut c,
        &owner(),
        &format!("t{owned}.3"),
        "needed"
    ))
    .contains("1..=2"));
    assert!(
        err(ask::rate(&mut c, &owner(), &format!("t{owned}.1"), "meh")).contains("invalid rating")
    );
    assert!(err(ask::rate(
        &mut c,
        &Actor::new("agent"),
        &format!("t{owned}.1"),
        "needed"
    ))
    .contains("cannot rate"));
    let calm = task(&mut c, "p", "calm", None);
    assert!(
        err(ask::rate(&mut c, &owner(), &format!("t{calm}.1"), "needed"))
            .contains("never been escalated")
    );
    assert!(err(ask::rate(&mut c, &owner(), "m13", "needed")).contains("not a card ask"));
    assert!(err(ask::rate(&mut c, &owner(), "m14", "needed")).contains("not a card ask"));
}

#[test]
fn stats_give_wasted_share_per_bot_and_week() {
    let (_d, mut c) = setup();
    let t = now();
    let mut ids = Vec::new();
    for i in 0..5 {
        let id = task(&mut c, "p", &format!("t{i}"), Some("bot:a"));
        set_state(&mut c, id, "escalated");
        ids.push(id);
    }
    let none = task(&mut c, "p", "nobody's", None);
    set_state(&mut c, none, "escalated");
    // Two asks fall in the older week of a 14-day window.
    for id in &ids[..2] {
        c.execute(
            "UPDATE events SET at=?1 WHERE task_id=?2",
            params![ts(t - 10 * DAY), id],
        )
        .unwrap();
    }
    for (id, r) in ids
        .iter()
        .zip(["needed", "could_decide", "should_not_exist", "needed"])
    {
        ask::rate(&mut c, &owner(), &format!("t{id}.1"), r).unwrap();
    }
    let card = message(
        &mut c,
        "b",
        20,
        "q?",
        json!({"buttons": [["Y","y"]], "ask": true}),
    );
    ask::rate(&mut c, &owner(), &format!("m{card}"), "could_decide").unwrap();

    // The window ends after every ask was made (asks made in a later second
    // than `t` would fall outside an end of `t + 1`).
    let end = now() + 1;
    let s = ask::stats(&c, None, end - 14 * DAY, end).unwrap();
    assert_eq!(s.overall.total.asks, 7);
    assert_eq!(s.overall.total.rated, 5);
    assert_eq!(s.overall.total.wasted_share, Some(3.0 / 5.0));
    assert_eq!(s.overall.weeks.len(), 2);
    let a = &s.bots["a"];
    assert_eq!((a.total.asks, a.total.rated, a.total.needed), (5, 4, 2));
    assert_eq!(a.total.wasted_share, Some(0.5));
    // Older week: needed + could_decide; newer: should_not_exist, needed, unrated.
    assert_eq!((a.weeks[0].tally.asks, a.weeks[0].tally.rated), (2, 2));
    assert_eq!(a.weeks[0].tally.wasted_share, Some(0.5));
    assert_eq!((a.weeks[1].tally.asks, a.weeks[1].tally.rated), (3, 2));
    assert_eq!(a.weeks[1].tally.wasted_share, Some(0.5));
    assert_eq!(s.bots["b"].total.wasted_share, Some(1.0));
    assert_eq!(s.bots["-"].total.asks, 1);
    assert_eq!(s.bots["-"].total.wasted_share, None, "nothing rated");
    // A project filter keeps the project's tasks and bots.
    let q = ask::stats(&c, Some("q"), end - 14 * DAY, end).unwrap();
    assert_eq!(q.overall.total.asks, 1);

    // The retro card: last week's unrated asks only.
    let r = ask::retro(&c, end).unwrap();
    let items: Vec<_> = r.items.iter().map(|i| i.ask.clone()).collect();
    assert_eq!(items, [format!("t{}.1", ids[4]), format!("t{none}.1")]);
    assert_eq!(r.items[0].line, format!("a task #{}: t4", ids[4]));
    assert_eq!(r.items[1].line, format!("- task #{none}: nobody's"));
    assert!(
        r.text.contains("\n1. decisions: 2 (- 1, a 1)"),
        "{}",
        r.text
    );
    // Per kind too.
    assert_eq!(s.kinds["decision"].total.asks, 6);
    assert_eq!(s.kinds["other"].total.wasted_share, Some(1.0));
    assert_eq!(r.buttons.len(), 4, "one kind: three ratings and open");
}

#[test]
fn retro_holds_every_unrated_ask_grouped_by_kind() {
    let (_d, mut c) = setup();
    for i in 0..13 {
        let id = task(
            &mut c,
            "p",
            &format!("a long title number {i} ").repeat(5),
            Some("bot:a"),
        );
        set_state(&mut c, id, "escalated");
    }
    let r = ask::retro(&c, now() + 1).unwrap();
    assert_eq!(r.items.len(), 13);
    assert_eq!(r.more, 0);
    assert!(r.text.contains("(13, 13 unrated)") && r.text.contains("decisions: 13 (a 13)"));
    assert_eq!(r.rows, [4]);
    assert!(
        r.items
            .iter()
            .all(|i| i.line.ends_with('…') && i.kind == "decision"),
        "{:?}",
        r.items
    );
}

/// Escalates `id` with a class and resolver, as `axe escalate` does.
fn escalate(c: &mut Connection, id: i64, class: Option<&str>, resolver: &str) {
    let ch = Changes {
        state: Some("escalated".into()),
        escalation_class: class.map(String::from),
        note: Some(format!("blocked: x\nresolver: {resolver}\nresume: y")),
        ..Default::default()
    };
    db::set_task(c, &Actor::new("agent"), id, &ch).unwrap();
}

#[test]
fn asks_carry_a_stable_kind() {
    let (_d, mut c) = setup();
    let merge = task(&mut c, "p", "merge", Some("bot:a"));
    escalate(
        &mut c,
        merge,
        Some("decision"),
        "owner: approve merging #1 at abc",
    );
    // A later class correction (episode=1) wins; a second episode is its own.
    db::classify(
        &mut c,
        &Actor::new("human"),
        merge,
        Some("capability"),
        None,
        Some(1),
    )
    .unwrap();
    set_state(&mut c, merge, "pending");
    escalate(&mut c, merge, None, "owner: deploy it");
    let pick = task(&mut c, "p", "pick", None);
    escalate(&mut c, pick, Some("decision"), "owner: reply A or B");
    let stuck = task(&mut c, "p", "stuck", None);
    escalate(&mut c, stuck, Some("runtime"), "owner: look");
    let cards = [
        (
            json!({"buttons": [["Merge","m"],["Hold","h"]], "ask": true}),
            "approval",
        ),
        (
            json!({"buttons": [["Yes","y"],["No","n"]], "ask": true}),
            "confirm",
        ),
        (
            json!({"buttons": [["A","a"],["B","b"],["C","c"]], "ask": true}),
            "choice",
        ),
        (
            json!({"buttons": [["#12","a"],["#13","b"]], "ask": true}),
            "digest",
        ),
        (
            json!({"buttons": [], "ask": true, "answered": {"label": "Approve"}}),
            "approval",
        ),
        (json!({"buttons": [], "ask": true}), "other"),
    ];
    for (i, (flags, _)) in cards.iter().enumerate() {
        message(&mut c, "a", 30 + i as i64, "card", flags.clone());
    }
    let kinds: Vec<_> = all(&c).into_iter().map(|a| (a.id, a.category)).collect();
    let mut want = vec![
        (format!("t{merge}.1"), "access".to_string()),
        (format!("t{merge}.2"), "approval".into()),
        (format!("t{pick}.1"), "decision".into()),
        (format!("t{stuck}.1"), "stuck".into()),
    ];
    let first_card: i64 = c
        .query_row(
            "SELECT MIN(id) FROM bot_messages WHERE message_id>=30",
            [],
            |r| r.get(0),
        )
        .unwrap();
    for (i, (_, k)) in cards.iter().enumerate() {
        want.push((format!("m{}", first_card + i as i64), k.to_string()));
    }
    assert_eq!(kinds, want);
    // A kind filter, and every kind a listed one.
    let only = ask::list(
        &c,
        &Filter {
            since: 0,
            until: now() + 1,
            kind: Some("approval"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(only.len(), 3);
    assert!(kinds
        .iter()
        .all(|(_, k)| ask::KINDS.iter().any(|x| x.0 == k)));

    // A kind rating rates only that kind's unrated items.
    ask::rate(&mut c, &owner(), &format!("t{merge}.2"), "needed").unwrap();
    let r = ask::retro(&c, now() + 1).unwrap();
    assert!(
        r.items.iter().all(|i| i.ask != format!("t{merge}.2")),
        "rated is off the card"
    );
    let todo = ask::unrated_of(&r.items, &Default::default(), "approval");
    assert_eq!(todo.len(), 2);
    assert_eq!(
        ask::rate_many(&mut c, &owner(), &todo, "could_decide").unwrap(),
        2
    );
    let rated = ask::ratings(
        &c,
        &r.items.iter().map(|i| i.ask.clone()).collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(rated.len(), 2);
    assert!(rated.values().all(|v| v == "could_decide"));
    assert_eq!(
        ask::rate(&mut c, &owner(), &format!("t{merge}.2"), "needed")
            .unwrap()
            .rating
            .as_deref(),
        Some("needed"),
        "the item rated alone kept its rating"
    );
}

#[test]
fn cli_lists_rates_and_reports_asks() {
    let dir = tempfile::tempdir().unwrap();
    let srv = common::Server::start(dir.path());
    let out = srv.ok(&[
        "add",
        "--project",
        "p",
        "--title",
        "decide this",
        "--detail",
        "x",
    ]);
    let id: i64 = out
        .split_whitespace()
        .find_map(|w| w.trim_start_matches('#').parse().ok())
        .unwrap();
    srv.ok(&[
        "escalate",
        &id.to_string(),
        "--blocked",
        "x",
        "--resolver",
        "owner: y",
        "--resume",
        "z",
        "--class",
        "decision",
    ]);

    let list = srv.ok(&["ask", "list", "--unrated"]);
    assert!(
        list.contains(&format!("t{id}.1"))
            && list.contains("unrated")
            && list.contains("decide this"),
        "{list:?}"
    );
    let dry = srv.ok(&["ask", "retro", "--dry"]);
    assert!(dry.contains("\n1. decisions: 1 (- 1)"), "{dry}");
    assert!(
        dry.contains("[1 needed] [1 decide] [1 drop] [1 open]"),
        "{dry}"
    );
    assert!(list.contains("decision"), "{list:?}");
    let by: Value =
        serde_json::from_str(&srv.ok(&["ask", "list", "--by", "kind", "--json"])).unwrap();
    assert_eq!(by["decision"]["asks"], 1);
    assert_eq!(by["decision"]["unrated"], 1);
    let none = srv.ok(&["ask", "list", "--kind", "approval"]);
    assert!(none.trim().is_empty(), "{none}");

    let rated = srv.ok(&["ask", "rate", &format!("t{id}.1"), "could_decide"]);
    assert!(rated.contains("could_decide by human"), "{rated}");
    let bad = srv.run(&["ask", "rate", &format!("t{id}.1"), "maybe"]);
    assert!(!bad.status.success());

    let v: Value = serde_json::from_str(&srv.ok(&["ask", "stats", "--since", "7d"])).unwrap();
    assert_eq!(v["overall"]["asks"], 1);
    assert_eq!(v["overall"]["wasted_share"], 1.0);
    assert_eq!(v["kinds"]["decision"]["could_decide"], 1);
    let k: Value =
        serde_json::from_str(&srv.ok(&["ask", "stats", "--since", "7d", "--by", "kind"])).unwrap();
    assert!(
        k["bots"].is_null() && k["kinds"]["decision"]["asks"] == 1,
        "{k}"
    );
    let flow: Value = serde_json::from_str(&srv.ok(&[
        "stats",
        "--project",
        "p",
        "--until",
        "2099-01-01T00:00:00Z",
    ]))
    .unwrap();
    assert_eq!(flow["asks"]["overall"]["rated"], 1);
    assert_eq!(flow["asks"]["bots"]["-"]["could_decide"], 1);

    let dry = srv.ok(&["ask", "retro", "--dry"]);
    assert!(dry.contains("no unrated asks"), "{dry}");
    // Asks quote cards: reads need the token.
    let r = ureq::get(&format!("{}/api/asks", srv.url)).call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))));
}
