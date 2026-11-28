//! The board as Telegram shows it: one pinned HTML message per chat with a
//! button row (`telegram.rs`, the view pass). Pure rendering of the core
//! model ([`crate::board::Board`]); which items exist, their order and their
//! state are the core's.
//!
//! The text fits Telegram's limit: when it would not, lines are left out in
//! this order until it does: Done today, then the item sections from the
//! bottom, then Review from the bottom, and Decide from the bottom only as a
//! last resort. A "+N more" line links the web board, which has no limit.
use chrono::TimeZone;
use serde_json::{json, Value};

use crate::board::{age, cap, escape, utf16, Board, BoardLine, FOOTER_ROOM, TEXT_LIMIT};

/// Decide lines that get a jump button.
pub const JUMP_BUTTONS: usize = 5;
/// Entries of a Mark done or Park menu; more items get a web board link.
pub const MENU_ENTRIES: usize = 12;
/// Characters of a menu button's label (one full-width row on a phone).
const LABEL_CAP: usize = 36;
/// The main row's button that opens the item picker.
pub const TICK_LABEL: &str = "☑ Mark done…";
/// The main row's button that opens the decision picker.
pub const PARK_LABEL: &str = "⏸ Park…";
/// The button that closes a picker.
pub const BACK_LABEL: &str = "‹ Back";
/// Callback data prefix of the board's buttons.
pub const PREFIX: &str = "brd:";
/// Text a board pin is edited to when a newer one replaces it.
pub const MOVED_TEXT: &str = "Moved to today's board.";
/// Text a board pin is edited to when its chat's pin is turned off.
pub const OFF_TEXT: &str = "The board pin is off in this chat (/board pin on).";
/// Text a retired goal pin or overview is edited to.
pub const LEGACY_TEXT: &str = "Moved to the board (the pinned message).";

/// `Sat 28 Nov` for `now` in timezone `tz`.
fn date(now: i64, tz: &str) -> String {
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    match tz.timestamp_opt(now, 0).single() {
        Some(t) => t.format("%a %-d %b").to_string(),
        None => String::new(),
    }
}

/// The web board of bot `bot`.
pub fn web_url(web: &str, bot: &str) -> String {
    format!(
        "{}/?bot={}&tab=board",
        web.trim_end_matches('/'),
        crate::channel::pct_encode(bot)
    )
}

fn link(l: &BoardLine) -> String {
    match (&l.link, &l.link_label) {
        (Some(u), Some(t)) => format!(
            "<a href=\"{}\">{}</a>",
            escape(u).replace('"', "&quot;"),
            escape(t)
        ),
        (None, Some(t)) => escape(t),
        _ => String::new(),
    }
}

/// One line: `☐ text · task #12 · 2d — note`; done lines are struck out.
fn line(l: &BoardLine, n: Option<usize>, now: i64) -> String {
    if l.state == "done" {
        return format!("☑ <s>{}</s>", escape(&l.text));
    }
    let mut out = String::new();
    if let Some(n) = n {
        out.push_str(&format!("{n}. "));
    }
    out.push_str("☐ ");
    if l.kind == "goal" {
        match &l.link {
            Some(_) => out.push_str(&format!(
                "<a href=\"{}\">{}</a>",
                escape(l.link.as_deref().unwrap_or("")).replace('"', "&quot;"),
                escape(&l.text)
            )),
            None => out.push_str(&format!("<b>{}</b>", escape(&l.text))),
        }
        if let Some(d) = &l.detail {
            out.push_str(&format!(" · {}", escape(d)));
        }
    } else {
        out.push_str(&escape(&l.text));
        let k = link(l);
        if !k.is_empty() {
            out.push_str(&format!(" · {k}"));
        }
        if l.overdue {
            out.push_str(" · <b>overdue</b>");
        }
        if l.since > 0 && now - l.since >= 86_400 {
            out.push_str(&format!(" · {}", age(now - l.since)));
        }
    }
    if let Some(note) = &l.note {
        out.push_str(&format!(" — <i>{}</i>", escape(note)));
    }
    out
}

struct Block {
    key: String,
    title: String,
    lines: Vec<String>,
}

fn assemble(head: &str, blocks: &[Block]) -> String {
    let mut out = head.to_string();
    for b in blocks.iter().filter(|b| !b.lines.is_empty()) {
        out.push_str(&format!(
            "\n\n<b>{}</b>\n{}",
            escape(&b.title),
            b.lines.join("\n")
        ));
    }
    out
}

/// The board of bot `board.bot` as Telegram HTML at `now` (dates in `tz`),
/// within [`TEXT_LIMIT`]. `web` is the public URL for the "+N more" link.
pub fn render(board: &Board, now: i64, tz: &str, web: Option<&str>) -> String {
    let decide = match board.decide {
        0 => "nothing to decide".to_string(),
        n => format!("{n} to decide"),
    };
    let head = format!(
        "<b>{}</b> · {} · {decide}",
        escape(&board.bot),
        date(now, tz)
    );
    if board.sections.is_empty() {
        return format!("{head}\n\n{}", crate::board::EMPTY);
    }
    let mut blocks: Vec<Block> = board
        .sections
        .iter()
        .map(|s| Block {
            key: s.key.clone(),
            title: s.title.clone(),
            lines: s
                .lines
                .iter()
                .enumerate()
                .map(|(i, l)| line(l, (s.key == "decide").then_some(i + 1), now))
                .collect(),
        })
        .collect();
    let budget = TEXT_LIMIT - FOOTER_ROOM;
    let mut hidden = 0;
    // Drop order: done, item sections from the bottom, review, then decide.
    let order = ["done", "items", "review", "goals", "decide"];
    'fit: for key in order {
        while utf16(&assemble(&head, &blocks)) > budget {
            let Some(b) = blocks
                .iter_mut()
                .rev()
                .find(|b| b.key == key && !b.lines.is_empty())
            else {
                continue 'fit;
            };
            b.lines.pop();
            hidden += 1;
        }
        break;
    }
    let mut out = assemble(&head, &blocks);
    if hidden > 0 {
        match web {
            Some(w) => out.push_str(&format!(
                "\n\n+{hidden} more on the <a href=\"{}\">web board</a>",
                escape(&web_url(w, &board.bot)).replace('"', "&quot;")
            )),
            None => out.push_str(&format!("\n\n+{hidden} more (axe bot board show)")),
        }
    }
    out
}

fn data(label: &str, data: String) -> Value {
    json!({"text": label, "callback_data": data})
}

/// `task:12` or `card:40` for a Decide line.
fn target(l: &BoardLine) -> String {
    format!("{}:{}", l.kind, l.id)
}

/// The pin's inline keyboard. The main view (`menu` `None`): a row of jump
/// buttons, one per numbered Decide line (up to [`JUMP_BUTTONS`]); a row
/// with Mark done… and Park… when they have entries; and a row with the web
/// board. A picker (`tick`: open Review and Items lines; `park`: Decide
/// lines) lists one entry per full-width row, labelled with the line's own
/// text so a press says what it does, then "+N more" on the web when it is
/// cut, and Back. Empty: no keyboard.
pub fn keyboard(board: &Board, menu: Option<&str>, web: Option<&str>) -> Value {
    let decide: Vec<&BoardLine> = board
        .sections
        .iter()
        .filter(|s| s.key == "decide")
        .flat_map(|s| &s.lines)
        .collect();
    let items: Vec<&BoardLine> = board
        .sections
        .iter()
        .filter(|s| matches!(s.key.as_str(), "review" | "items"))
        .flat_map(|s| &s.lines)
        .filter(|l| l.kind == "item" && l.state == "open")
        .collect();
    let back = vec![data(BACK_LABEL, format!("{PREFIX}m:"))];
    let more = |n: usize| -> Option<Vec<Value>> {
        let w = web.filter(|_| n > MENU_ENTRIES)?;
        Some(vec![json!({
            "text": format!("+{} more on the web board", n - MENU_ENTRIES),
            "url": web_url(w, &board.bot)
        })])
    };
    let rows: Vec<Vec<Value>> = match menu {
        Some("tick") if !items.is_empty() => items
            .iter()
            .take(MENU_ENTRIES)
            .map(|l| {
                vec![data(
                    &format!("☑ {}", cap(&l.text, LABEL_CAP)),
                    format!("{PREFIX}d:{}", l.id),
                )]
            })
            .chain(more(items.len()))
            .chain([back])
            .collect(),
        Some("park") if !decide.is_empty() => decide
            .iter()
            .take(MENU_ENTRIES)
            .enumerate()
            .map(|(i, l)| {
                vec![data(
                    &format!("⏸ {}. {}", i + 1, cap(&l.text, LABEL_CAP)),
                    format!("{PREFIX}p:{}", target(l)),
                )]
            })
            .chain(more(decide.len()))
            .chain([back])
            .collect(),
        _ => {
            let jumps: Vec<Value> = decide
                .iter()
                .take(JUMP_BUTTONS)
                .enumerate()
                .map(|(i, l)| data(&(i + 1).to_string(), format!("{PREFIX}j:{}", target(l))))
                .collect();
            let mut actions = Vec::new();
            if !items.is_empty() {
                actions.push(data(TICK_LABEL, format!("{PREFIX}m:tick")));
            }
            if !decide.is_empty() {
                actions.push(data(PARK_LABEL, format!("{PREFIX}m:park")));
            }
            let web_row = web.map(|w| vec![json!({"text": "Web board", "url": web_url(w, &board.bot)})]);
            [jumps, actions]
                .into_iter()
                .chain(web_row)
                .filter(|r| !r.is_empty())
                .collect()
        }
    };
    json!({ "inline_keyboard": rows })
}

/// A press on a board button: `brd:j:<kind>:<id>` jumps to a Decide line,
/// `brd:m:<menu>` opens a menu (empty: the main row), `brd:d:<item>` ticks
/// an item and `brd:p:<kind>:<id>` parks a Decide line.
#[derive(Debug, PartialEq)]
pub enum Press {
    Jump(String, i64),
    Menu(Option<String>),
    Done(i64),
    Park(String, i64),
}

impl Press {
    pub fn parse(data: &str) -> Option<Press> {
        let rest = data.strip_prefix(PREFIX)?;
        let (op, arg) = rest.split_once(':')?;
        let kind_id = |a: &str| -> Option<(String, i64)> {
            let (k, id) = a.split_once(':')?;
            matches!(k, "task" | "card").then_some(())?;
            Some((k.to_string(), id.parse().ok()?))
        };
        match op {
            "j" => kind_id(arg).map(|(k, i)| Press::Jump(k, i)),
            "p" => kind_id(arg).map(|(k, i)| Press::Park(k, i)),
            "d" => arg.parse().ok().map(Press::Done),
            "m" => Some(Press::Menu(
                matches!(arg, "tick" | "park").then(|| arg.to_string()),
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Section, View};

    fn l(kind: &str, id: i64, text: &str) -> BoardLine {
        BoardLine {
            kind: kind.into(),
            id,
            text: text.into(),
            note: None,
            item: (kind == "item").then_some(id),
            task: (kind == "task").then_some(id),
            reference: None,
            link: None,
            link_label: None,
            since: 0,
            state: "open".into(),
            overdue: false,
            detail: None,
        }
    }

    fn board(sections: Vec<(&str, &str, Vec<BoardLine>)>) -> Board {
        let decide = sections
            .iter()
            .filter(|s| s.0 == "decide")
            .map(|s| s.2.len())
            .sum();
        Board {
            bot: "cos".into(),
            sections: sections
                .into_iter()
                .map(|(k, t, lines)| Section {
                    key: k.into(),
                    title: t.into(),
                    lines,
                })
                .collect(),
            decide,
            open_items: 0,
            done_since: 0,
            views: Vec::<View>::new(),
        }
    }

    // 2026-11-30 (a Monday) 09:00 UTC.
    const NOW: i64 = 1_796_029_200;

    #[test]
    fn renders_sections_in_order_with_links_ages_and_notes() {
        let mut t = l("task", 12, "Deploy <site>");
        t.link = Some("http://axe/?task=12".into());
        t.link_label = Some("task #12".into());
        t.since = NOW - 2 * 86_400;
        t.note = Some("waits on your CI host".into());
        let mut doc = l("item", 3, "Core fixes");
        doc.link = Some("http://axe/doc?path=a&b".into());
        doc.link_label = Some("doc".into());
        let mut g = l("goal", 1, "bot-cost");
        g.detail = Some("on track · next: cache #9".into());
        let mut done = l("item", 4, "Old thing");
        done.state = "done".into();
        let mut late = l("item", 5, "Log cap");
        late.overdue = true;
        let b = board(vec![
            ("decide", "Decide", vec![t, l("card", 40, "Which one?")]),
            ("review", "Review", vec![doc]),
            ("goals", "Goals", vec![g]),
            ("items", "Waiting on you", vec![l("item", 6, "Contract")]),
            ("items", "Items", vec![late]),
            ("done", "Done today", vec![done]),
        ]);
        assert_eq!(
            render(&b, NOW, "UTC", None),
            "<b>cos</b> · Mon 30 Nov · 2 to decide\n\n\
             <b>Decide</b>\n\
             1. ☐ Deploy &lt;site&gt; · <a href=\"http://axe/?task=12\">task #12</a> · 2d — <i>waits on your CI host</i>\n\
             2. ☐ Which one?\n\n\
             <b>Review</b>\n\
             ☐ Core fixes · <a href=\"http://axe/doc?path=a&amp;b\">doc</a>\n\n\
             <b>Goals</b>\n\
             ☐ <b>bot-cost</b> · on track · next: cache #9\n\n\
             <b>Waiting on you</b>\n\
             ☐ Contract\n\n\
             <b>Items</b>\n\
             ☐ Log cap · <b>overdue</b>\n\n\
             <b>Done today</b>\n\
             ☑ <s>Old thing</s>"
        );
        assert_eq!(
            render(&board(vec![]), NOW, "Asia/Taipei", None),
            format!(
                "<b>cos</b> · Mon 30 Nov · nothing to decide\n\n{}",
                crate::board::EMPTY
            )
        );
    }

    #[test]
    fn overflow_drops_done_then_items_then_review_and_stays_under_the_limit() {
        let many = |kind: &str, n: i64, w: &str| -> Vec<BoardLine> {
            (0..n)
                .map(|i| l(kind, i, &format!("{w} {i} {}", "x".repeat(150))))
                .collect()
        };
        let mut done = many("item", 30, "done");
        for d in &mut done {
            d.state = "done".into();
        }
        let b = board(vec![
            ("decide", "Decide", many("task", 3, "decide")),
            ("review", "Review", many("item", 5, "review")),
            ("items", "Items", many("item", 50, "item")),
            ("done", "Done today", done),
        ]);
        let t = render(&b, NOW, "UTC", Some("http://axe"));
        assert!(utf16(&t) <= TEXT_LIMIT, "{}", utf16(&t));
        assert_eq!(t.matches("☑").count(), 0, "done goes first");
        assert_eq!(t.matches("review ").count(), 5, "review is kept: {t}");
        assert_eq!(t.matches("decide ").count(), 3);
        let shown_items = t.matches("☐ item ").count();
        assert!(shown_items > 0 && shown_items < 50, "{shown_items}");
        let hidden = 30 + 50 - shown_items;
        assert!(
            t.ends_with(&format!(
                "\n\n+{hidden} more on the <a href=\"http://axe/?bot=cos&amp;tab=board\">web board</a>"
            )),
            "{t}"
        );
        // Decide alone over the limit: cut from the bottom, still counted.
        let b = board(vec![("decide", "Decide", many("task", 60, "decide"))]);
        let t = render(&b, NOW, "UTC", None);
        assert!(utf16(&t) <= TEXT_LIMIT);
        let shown = t.matches("☐ decide").count();
        assert!(t.contains("1. ☐ decide 0 "), "oldest first");
        assert!(
            t.ends_with(&format!("+{} more (axe bot board show)", 60 - shown)),
            "{t}"
        );
    }

    #[test]
    fn keyboard_main_view_and_pickers() {
        let mut done = l("item", 9, "Done");
        done.state = "done".into();
        let b = board(vec![
            (
                "decide",
                "Decide",
                vec![l("task", 12, "Deploy"), l("card", 40, "Which one?")],
            ),
            ("review", "Review", vec![l("item", 3, "Core fixes")]),
            ("items", "Items", vec![l("item", 7, &"a".repeat(80)), done]),
        ]);
        let k = keyboard(&b, None, Some("http://axe/"));
        assert_eq!(
            k,
            json!({"inline_keyboard": [
                [
                    {"text": "1", "callback_data": "brd:j:task:12"},
                    {"text": "2", "callback_data": "brd:j:card:40"}
                ],
                [
                    {"text": "☑ Mark done…", "callback_data": "brd:m:tick"},
                    {"text": "⏸ Park…", "callback_data": "brd:m:park"}
                ],
                [{"text": "Web board", "url": "http://axe/?bot=cos&tab=board"}]
            ]})
        );
        // The item picker: each open Review and Items line by its own text,
        // then Back; a done item is not offered.
        let tick = keyboard(&b, Some("tick"), None);
        let rows = tick["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "two open items and Back: {tick}");
        assert_eq!(rows[0][0], json!({"text": "☑ Core fixes", "callback_data": "brd:d:3"}));
        assert_eq!(rows[1][0]["callback_data"], "brd:d:7");
        assert_eq!(
            rows[1][0]["text"].as_str().unwrap().chars().count(),
            2 + LABEL_CAP
        );
        assert_eq!(rows[2][0], json!({"text": "‹ Back", "callback_data": "brd:m:"}));
        let park = keyboard(&b, Some("park"), None);
        assert_eq!(
            park["inline_keyboard"][1][0],
            json!({"text": "⏸ 2. Which one?", "callback_data": "brd:p:card:40"})
        );
        for row in rows {
            for btn in row.as_array().unwrap() {
                assert!(btn["callback_data"].as_str().unwrap().len() <= 64);
            }
        }
        // More open items than a picker holds: the rest are on the web.
        let many: Vec<BoardLine> = (0..15).map(|i| l("item", i, &format!("item {i}"))).collect();
        let b = board(vec![("items", "Items", many)]);
        let tick = keyboard(&b, Some("tick"), Some("http://axe"));
        let rows = tick["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), MENU_ENTRIES + 2);
        assert_eq!(
            rows[MENU_ENTRIES][0],
            json!({"text": "+3 more on the web board", "url": "http://axe/?bot=cos&tab=board"})
        );
        // No Decide line: no jump row and no Park.
        assert_eq!(
            keyboard(&b, None, None),
            json!({"inline_keyboard": [[{"text": "☑ Mark done…", "callback_data": "brd:m:tick"}]]})
        );
        // A picker with nothing left falls back to the main view.
        assert_eq!(keyboard(&b, Some("park"), None), keyboard(&b, None, None));
        assert_eq!(
            keyboard(&board(vec![]), None, None),
            json!({"inline_keyboard": []})
        );
    }

    #[test]
    fn presses_parse() {
        assert_eq!(
            Press::parse("brd:j:task:12"),
            Some(Press::Jump("task".into(), 12))
        );
        assert_eq!(
            Press::parse("brd:p:card:40"),
            Some(Press::Park("card".into(), 40))
        );
        assert_eq!(Press::parse("brd:d:7"), Some(Press::Done(7)));
        assert_eq!(
            Press::parse("brd:m:tick"),
            Some(Press::Menu(Some("tick".into())))
        );
        assert_eq!(Press::parse("brd:m:"), Some(Press::Menu(None)));
        assert_eq!(Press::parse("brd:j:goal:1"), None);
        assert_eq!(Press::parse("task #1 resolved"), None);
    }
}
