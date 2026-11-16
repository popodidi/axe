//! Owner asks (`ask.rs`): escalation episodes and bot cards, ratings, the
//! wasted-ask share per bot and week, and each bot's retro card.
mod common;

use axe::ask::{self, Filter, RetroScope};
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
    // The question: an escalation's blocked line, a card's first line.
    assert_eq!(asks[0].question, "x");
    assert_eq!(asks[3].question, "Deploy now?");
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

    // The retro card: last week's unrated asks only, two lines each.
    let r = ask::retro(&c, end, &RetroScope::default()).unwrap();
    let items: Vec<_> = r.items.iter().map(|i| i.ask.clone()).collect();
    assert_eq!(items, [format!("t{}.1", ids[4]), format!("t{none}.1")]);
    assert_eq!(r.items[0].line, format!("#{}: t4", ids[4]));
    assert_eq!(r.items[1].line, format!("#{none}: nobody's"));
    assert!(
        r.text.contains(&format!(
            "\n1. #{}: t4\n   x Ask: decide\n2. #{none}: nobody's\n   x Ask: decide\n",
            ids[4]
        )),
        "{}",
        r.text
    );
    assert!(r.text.ends_with("\nBy kind: decisions 2"), "{}", r.text);
    assert_eq!(r.rows, [3, 3, 1], "a row of ratings per ask, then Dismiss");
    // Bot a's retro has its own ask only; the home bot's has the bot-less one.
    let senders = vec!["a".to_string(), "b".to_string()];
    let mine = |bot: &str, home: bool| {
        ask::retro(
            &c,
            end,
            &RetroScope {
                bot: Some(bot),
                home,
                senders: &senders,
            },
        )
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.ask)
        .collect::<Vec<_>>()
    };
    assert_eq!(mine("a", false), [format!("t{}.1", ids[4])]);
    assert!(mine("b", false).is_empty());
    assert_eq!(mine("b", true), [format!("t{none}.1")]);
    // Per kind too.
    assert_eq!(s.kinds["decision"].total.asks, 6);
    assert_eq!(s.kinds["other"].total.wasted_share, Some(1.0));
}

#[test]
fn a_bots_retro_holds_only_its_asks_two_lines_each_paged() {
    let (_d, mut c) = setup();
    let mut mine = Vec::new();
    for i in 0..13 {
        let id = task(&mut c, "p", "title", Some("bot:a"));
        let ch = Changes {
            state: Some("escalated".into()),
            note: Some(format!(
                "blocked: {}\nresolver: owner: decide\nresume: y",
                format!("need a call on question {i} ").repeat(8)
            )),
            ..Default::default()
        };
        db::set_task(&mut c, &Actor::new("agent"), id, &ch).unwrap();
        mine.push(id);
    }
    // Bot b's escalation and card never reach a's retro.
    let theirs = task(&mut c, "q", "theirs", Some("bot:b"));
    set_state(&mut c, theirs, "escalated");
    message(
        &mut c,
        "b",
        40,
        "B's question?",
        json!({"buttons": [["Y","y"],["N","n"]], "ask": true}),
    );
    let card = message(
        &mut c,
        "a",
        41,
        "A's question?",
        json!({"buttons": [["Y","y"],["N","n"]], "ask": true}),
    );
    let senders = vec!["a".to_string(), "b".to_string()];
    let scope = RetroScope {
        bot: Some("a"),
        home: false,
        senders: &senders,
    };
    let r = ask::retro(&c, now() + 1, &scope).unwrap();
    assert_eq!(r.items.len(), 14);
    assert_eq!(r.more, 0);
    assert!(r.items.iter().all(|i| i.bot == "a"), "{:?}", r.items);
    assert!(!r.text.contains("B's") && !r.text.contains(&format!("#{theirs}")));
    assert!(
        r.text
            .starts_with("Weekly retro, a: last week's asks (14, 14 unrated), page 1/2\n"),
        "{}",
        r.text
    );
    // Line 1 is the task's title, line 2 the blocker (cut) and the ask.
    assert!(
        r.items[..13]
            .iter()
            .zip(&mine)
            .all(|(i, id)| i.line == format!("#{id}: title")
                && i.detail.starts_with("need a call on question")
                && i.detail.ends_with("… Ask: decide")),
        "{:?}",
        r.items
    );
    assert!(
        r.text.contains(&format!(
            "\n1. #{}: title\n   need a call on question 0 ",
            mine[0]
        )),
        "{}",
        r.text
    );
    assert_eq!(r.items[13].line, format!("msg {card}: A's question?"));
    assert_eq!(r.items[13].detail, "");
    assert_eq!(
        r.rows,
        [vec![3; ask::RETRO_PAGE], vec![1, 1]].concat(),
        "next only, then Dismiss"
    );
    assert!(
        r.text.ends_with("\nBy kind: decisions 13, yes/no checks 1"),
        "{}",
        r.text
    );
    assert!(r.text.encode_utf16().count() < 4096);
    let page2 = ask::render_retro(&r.items, r.more, &Default::default(), 1, Some("a"));
    assert!(page2.text.contains("page 2/2") && page2.text.contains(&format!("\n14. msg {card}: ")));
    assert_eq!(page2.rows, [3, 3, 3, 3, 1, 1], "prev only, then Dismiss");
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

    // Each ask is rated on its own, even within one kind; a rated ask is
    // off the next card.
    ask::rate(&mut c, &owner(), &format!("t{merge}.2"), "needed").unwrap();
    let r = ask::retro(&c, now() + 1, &RetroScope::default()).unwrap();
    assert!(
        r.items.iter().all(|i| i.ask != format!("t{merge}.2")),
        "rated is off the card"
    );
    let approvals: Vec<String> = r
        .items
        .iter()
        .filter(|i| i.kind == "approval")
        .map(|i| i.ask.clone())
        .collect();
    assert_eq!(approvals.len(), 2);
    ask::rate(&mut c, &owner(), &approvals[0], "could_decide").unwrap();
    ask::rate(&mut c, &owner(), &approvals[1], "should_not_exist").unwrap();
    let rated = ask::ratings(
        &c,
        &r.items.iter().map(|i| i.ask.clone()).collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(rated.len(), 2);
    assert_eq!(rated[&approvals[0]], "could_decide");
    assert_eq!(rated[&approvals[1]], "should_not_exist");
    assert_eq!(
        ask::find(&c, &format!("t{merge}.2"))
            .unwrap()
            .rating
            .as_deref(),
        Some("needed"),
        "each ask keeps its own rating"
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
    assert!(
        dry.contains(&format!("\n1. #{id}: decide this\n   x Ask: y\n")),
        "{dry}"
    );
    assert!(dry.contains("By kind: decisions 1"), "{dry}");
    assert!(dry.contains("[1 needed] [1 decide] [1 drop]"), "{dry}");
    // A bot's preview holds only its own asks (no gateway: no home bot).
    let other = srv.ok(&["ask", "retro", "--dry", "--bot", "someone"]);
    assert!(
        other.contains("Weekly retro, someone: no unrated asks"),
        "{other}"
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

fn rec(ask: &str, rating: &str, why: Option<&str>) -> ask::Rec {
    ask::Rec {
        ask: ask.into(),
        rating: rating.into(),
        why: why.map(String::from),
    }
}

#[test]
fn recs_are_stored_apart_accepted_per_page_and_overridden() {
    let (_d, mut c) = setup();
    let mut ids = Vec::new();
    for i in 0..12 {
        let id = task(&mut c, "p", &format!("ask {i}"), Some("bot:a"));
        set_state(&mut c, id, "escalated");
        ids.push(format!("t{id}.1"));
    }
    let card = message(
        &mut c,
        "a",
        70,
        "Rotate the key?",
        json!({"buttons": [["Yes","y"],["No","n"]], "ask": true}),
    );
    let card = format!("m{card}");
    let theirs = task(&mut c, "q", "theirs", Some("bot:b"));
    set_state(&mut c, theirs, "escalated");
    let bot_a = Actor::new("bot:a");
    let err = |r: anyhow::Result<Vec<ask::Ask>>| format!("{:#}", r.unwrap_err());

    // The asking bot recommends, one or many at once; the label `decide`
    // is `could_decide`. A rec is stored, not a rating.
    let one = ask::recommend(
        &mut c,
        &bot_a,
        &[rec(&ids[0], "decide", Some("reversible,\n in scope"))],
    )
    .unwrap();
    assert_eq!(one[0].rec.as_deref(), Some("could_decide"));
    assert_eq!(one[0].rec_why.as_deref(), Some("reversible, in scope"));
    assert_eq!(one[0].rec_by.as_deref(), Some("bot:a"));
    assert!(one[0].rating.is_none() && !one[0].accepted);
    let mut bulk: Vec<_> = ids[1..].iter().map(|i| rec(i, "needed", None)).collect();
    bulk.push(rec(&card, "drop", Some("asked twice")));
    assert_eq!(ask::recommend(&mut c, &bot_a, &bulk).unwrap().len(), 12);
    // A later rec replaces the earlier one.
    ask::recommend(&mut c, &bot_a, &[rec(&ids[2], "drop", None)]).unwrap();
    let a2 = ask::find(&c, &ids[2]).unwrap();
    assert_eq!(a2.rec.as_deref(), Some("should_not_exist"));
    assert!(a2.rec_why.is_none());
    // Every listed ask is still unrated.
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
    assert_eq!(unrated.len(), 14);

    // Refusals: another bot's ask (all or none), task sessions, bad input.
    let mixed = [
        rec(&ids[3], "drop", None),
        rec(&format!("t{theirs}.1"), "needed", None),
    ];
    assert!(err(ask::recommend(&mut c, &bot_a, &mixed)).contains("raised by b, not bot a"));
    assert_eq!(
        ask::find(&c, &ids[3]).unwrap().rec.as_deref(),
        Some("needed"),
        "nothing of a refused batch is kept"
    );
    assert!(err(ask::recommend(
        &mut c,
        &Actor::new("agent"),
        &[rec(&ids[3], "needed", None)]
    ))
    .contains("task sessions cannot recommend"));
    assert!(err(ask::recommend(
        &mut c,
        &bot_a,
        &[rec(&ids[3], "maybe", None)]
    ))
    .contains("invalid rating"));
    let long = "w".repeat(ask::REC_WHY + 1);
    assert!(err(ask::recommend(
        &mut c,
        &bot_a,
        &[rec(&ids[3], "needed", Some(&long))]
    ))
    .contains("a few words"));
    assert!(err(ask::recommend(&mut c, &bot_a, &[])).contains("no recommendations"));
    // A person may recommend for any ask.
    ask::recommend(
        &mut c,
        &Actor::new("human"),
        &[rec(&format!("t{theirs}.1"), "needed", None)],
    )
    .unwrap();

    // The retro card carries the recs and marks them.
    let senders = vec!["a".to_string(), "b".to_string()];
    let scope = RetroScope {
        bot: Some("a"),
        home: false,
        senders: &senders,
    };
    let r = ask::retro(&c, now() + 1, &scope).unwrap();
    assert_eq!(r.items.len(), 13);
    assert_eq!(r.items[0].rec, "could_decide");
    assert_eq!(r.items[0].why, "reversible, in scope");
    assert!(
        r.text.contains("\n   rec: decide (reversible, in scope)\n"),
        "{}",
        r.text
    );
    assert!(r.text.encode_utf16().count() < 4096);
    assert!(r
        .buttons
        .contains(&("1 decide (rec)".into(), format!("rate:{}:c", ids[0]))));
    assert!(r
        .buttons
        .contains(&(ask::ACCEPT_LABEL.into(), "ra:0".into())));

    // The owner rates one ask by hand first, then accepts page 1: only the
    // page's other, unrated asks take their recs; page 2 is untouched.
    ask::rate(&mut c, &owner(), &ids[1], "could_decide").unwrap();
    let n = ask::accept(&mut c, &owner(), ask::page_items(&r.items, 0)).unwrap();
    assert_eq!(n, ask::RETRO_PAGE - 1);
    let a1 = ask::find(&c, &ids[1]).unwrap();
    assert_eq!(
        a1.rating.as_deref(),
        Some("could_decide"),
        "rated by hand stays"
    );
    assert!(!a1.accepted);
    for id in &ids[..ask::RETRO_PAGE] {
        let a = ask::find(&c, id).unwrap();
        assert_eq!(a.rated_by.as_deref(), Some("owner"));
        if id != &ids[1] {
            assert!(a.accepted, "{id}");
            assert_eq!(a.rating, a.rec, "{id}");
        }
    }
    for id in &ids[ask::RETRO_PAGE..] {
        assert!(
            ask::find(&c, id).unwrap().rating.is_none(),
            "{id} on page 2"
        );
    }
    assert!(ask::find(&c, &card).unwrap().rating.is_none());
    // Accepting again rates nothing: every rec on the page is rated.
    assert_eq!(
        ask::accept(&mut c, &owner(), ask::page_items(&r.items, 0)).unwrap(),
        0
    );
    let shown = ask::render_retro(
        &r.items,
        r.more,
        &ask::ratings(&c, &ids).unwrap(),
        0,
        Some("a"),
    );
    assert!(shown.buttons.iter().all(|b| b.0 != ask::ACCEPT_LABEL));

    // A single override after accepting wins.
    ask::rate(&mut c, &owner(), &ids[0], "needed").unwrap();
    let a0 = ask::find(&c, &ids[0]).unwrap();
    assert_eq!(a0.rating.as_deref(), Some("needed"));
    assert!(!a0.accepted);
    assert_eq!(a0.rec.as_deref(), Some("could_decide"));
    // Task sessions cannot accept.
    assert!(ask::accept(&mut c, &Actor::new("agent"), ask::page_items(&r.items, 1)).is_err());

    // Stats count only the owner's ratings, and how recs fared: ids[0]
    // overridden, ids[1] overridden by hand before, ids[2..10] accepted.
    // A rec alone (page 2, the card, theirs) is no rating.
    let end = now() + 1;
    let s = ask::stats(&c, None, end - 7 * DAY, end).unwrap();
    let t = &s.overall.total;
    assert_eq!((t.asks, t.rated, t.recommended), (14, 10, 14));
    assert_eq!((t.accepted, t.agreed, t.overridden), (8, 0, 2));
    // ids[2] accepted `should_not_exist`, the rest of the page `needed`.
    assert_eq!((t.needed, t.could_decide, t.should_not_exist), (8, 1, 1));
    // By hand, the same as the rec: agreed.
    ask::rate(&mut c, &owner(), &ids[11], "needed").unwrap();
    let s = ask::stats(&c, None, end - 7 * DAY, end + 1).unwrap();
    assert_eq!(s.bots["a"].total.agreed, 1);
    assert_eq!(s.bots["b"].total.recommended, 1);
    // Wasted-ask budgets read the episode ratings as before.
    assert!(ask::wasted_episodes(&c).unwrap().len() >= 2);
}

#[test]
fn cli_recommends_one_or_many() {
    let dir = tempfile::tempdir().unwrap();
    let srv = common::Server::start(dir.path());
    let mut ids = Vec::new();
    for t in ["one", "two"] {
        let out = srv.ok(&["add", "--project", "p", "--title", t, "--detail", "x"]);
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
        ids.push(id);
    }
    let out = srv.ok(&[
        "ask",
        "rec",
        &format!("t{}.1", ids[0]),
        "decide",
        "--why",
        "in scope",
    ]);
    assert!(out.contains("unrated (rec could_decide)"), "{out}");
    let file = dir.path().join("recs.json");
    std::fs::write(
        &file,
        json!([{"ask": format!("#{}", ids[1]), "rating": "needed"}]).to_string(),
    )
    .unwrap();
    let out = srv.ok(&["ask", "rec", "--file", file.to_str().unwrap()]);
    assert!(
        out.contains(&format!("t{}.1", ids[1])) && out.contains("(rec needed)"),
        "{out}"
    );
    let bad = srv.run(&["ask", "rec", &format!("t{}.1", ids[0]), "maybe"]);
    assert!(!bad.status.success());
    // A bot may recommend only for its own asks.
    let other = srv.run(&[
        "--actor",
        "bot:x",
        "ask",
        "rec",
        &format!("t{}.1", ids[0]),
        "needed",
    ]);
    assert!(!other.status.success());
    assert!(String::from_utf8_lossy(&other.stderr).contains("not bot x"));

    let dry = srv.ok(&["ask", "retro", "--dry"]);
    assert!(dry.contains("\n   rec: decide (in scope)\n"), "{dry}");
    assert!(dry.contains("[1 decide (rec)]"), "{dry}");
    assert!(dry.contains("[2 needed (rec)]"), "{dry}");
    assert!(dry.contains(&format!("[{}]", ask::ACCEPT_LABEL)), "{dry}");
    let list: Vec<Value> = serde_json::from_str(&srv.ok(&["ask", "list", "--json"])).unwrap();
    assert_eq!(list[0]["rec"], "could_decide");
    assert_eq!(list[0]["rec_why"], "in scope");
    assert!(list[0]["rating"].is_null());
    let v: Value = serde_json::from_str(&srv.ok(&["ask", "stats", "--since", "7d"])).unwrap();
    assert_eq!(v["overall"]["recommended"], 2);
    assert_eq!(v["overall"]["rated"], 0);
}
