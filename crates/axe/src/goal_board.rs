//! The goals of a bot as Telegram shows them: one pinned message per active
//! goal (the "goal pin", `flags.goal_pin` holds the goal id), kept current
//! by the gateway (`telegram.rs`, `goal_pins`). Design: `docs/design/goals.md`,
//! "Budget". `show` renders the goal lines of every open goal (proposed,
//! active, paused) as one text. The separate goal board message
//! (`flags.goal_board`) is retired: the gateway unpins it, edits it to a
//! pointer and clears the flag; `set_message` only unsets it.
use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::board::{cap, escape, short, utf16, FOOTER_ROOM, TEXT_LIMIT};
use crate::bot;
use crate::channel::{bot_id, get_message, StoredMessage};
use crate::goal;

pub const EMPTY: &str = "No active goals.";
/// Goal states the board shows; `done` and `dropped` are omitted.
pub const SHOWN_STATES: [&str; 3] = ["proposed", "active", "paused"];
/// The goal state that has a pinned message.
pub const PINNED_STATE: &str = "active";
/// Characters of the summary line.
const SUMMARY_CAP: usize = 120;
/// Characters of the next task's title.
const NEXT_CAP: usize = 40;
/// Cells of a budget bar.
const BAR_CELLS: usize = 10;
/// Status observations searched for the latest one.
const STATUS_SCAN: i64 = 50;
/// Task states that no longer count as open.
const CLOSED_TASK_STATES: [&str; 1] = ["done"];
/// The task state that waits on the owner: each such task is a card in the
/// pinned list.
const NEEDS_YOU_STATE: &str = "escalated";
/// Open task states in the order the "Next" task is picked from.
const NEXT_ORDER: [&str; 2] = ["in_progress", "pending"];

/// The latest verdict or observation of a goal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Latest {
    /// Goal event id.
    pub id: i64,
    /// `verdict` or `observation`.
    pub kind: String,
    /// Its first non-empty line, shortened.
    pub text: String,
}

/// The linked task the goal's work goes on with.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NextTask {
    pub id: i64,
    /// Its title, shortened.
    pub title: String,
}

/// One goal on the board.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoalLine {
    pub id: i64,
    pub name: String,
    pub state: String,
    /// `on track`, `at risk` or `off track`, from the latest status
    /// observation ([`goal::status_of`]); `None` without one.
    pub status: Option<String>,
    /// One line: the status observation's summary, else the first sentence
    /// of the intent.
    pub summary: String,
    /// `<name> <now> → <target>`, from the status observation.
    pub metric: Option<String>,
    /// Other bots attached to the goal.
    pub others: Vec<String>,
    pub latest: Option<Latest>,
    /// Open linked (H) tasks, by id.
    pub open_tasks: Vec<i64>,
    /// The open linked task to work next: in progress first, then pending.
    pub next: Option<NextTask>,
    /// Escalated linked tasks: what the goal waits on the owner for.
    pub needs_you: usize,
    pub cost_spent: f64,
    pub cost_week: Option<f64>,
    pub attention_spent: i64,
    pub attention_week: Option<i64>,
    /// The next goal-tagged wake or schedule run of any bot (RFC3339).
    pub next_wake: Option<String>,
    pub deadline: Option<String>,
    /// The goal's page in the web dashboard, when the server's public URL
    /// is set (`[blobs] public_url`).
    pub url: Option<String>,
}

/// What `bot tg goal-board show` returns.
#[derive(Debug, Serialize)]
pub struct GoalBoard {
    /// Local id of the goal board message, if one is set.
    pub message: Option<i64>,
    pub goals: Vec<GoalLine>,
    /// The goal lines as one text.
    pub text: String,
}

/// One goal pin, as `bot tg board show` lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pin {
    pub goal: String,
    /// Local id of its pinned message; `None` until the gateway posts it.
    pub message: Option<i64>,
    /// Whether that message shows the current text.
    pub current: bool,
}

/// The bot's goal board message, if one is set.
pub fn message(conn: &Connection, name: &str) -> Result<Option<StoredMessage>> {
    crate::board::flagged(conn, name, "goal_board")
}

/// With `None`, stops rendering the bot's old goal board message. A new one
/// is refused: each active goal has its own pinned message.
pub fn set_message(conn: &mut Connection, name: &str, msg: Option<i64>) -> Result<Option<i64>> {
    if msg.is_some() {
        bail!("each active goal has its own pinned message now (`axe bot tg board show`); there is no goal board message");
    }
    crate::board::set_flagged(
        conn,
        name,
        None,
        "goal_board",
        &["goal_board", "goal_board_key"],
        "board",
    )
}

/// The goals bot `name` is attached to that are still open, oldest first.
/// `web` is the server's public URL, for the link to each goal's page.
pub fn lines(conn: &Connection, name: &str, web: Option<&str>) -> Result<Vec<GoalLine>> {
    let goals: Vec<goal::Goal> = goal::list(conn, Some(name), None)?
        .into_iter()
        .filter(|g| SHOWN_STATES.contains(&g.state.as_str()))
        .collect();
    let mut out = Vec::with_capacity(goals.len());
    for g in goals {
        let latest = conn
            .query_row(
                "SELECT id, kind, body FROM goal_events
                 WHERE goal_id=?1 AND kind IN ('verdict','observation')
                 ORDER BY id DESC LIMIT 1",
                [g.id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .map(|(id, kind, body)| Latest {
                id,
                kind,
                text: short(first_line(&body)),
            });
        let status = latest_status(conn, g.id)?;
        let open: Vec<goal::LinkedTask> = goal::tasks(conn, &g.name)?
            .into_iter()
            .filter(|t| !CLOSED_TASK_STATES.contains(&t.state.as_str()))
            .collect();
        let next = NEXT_ORDER
            .iter()
            .find_map(|s| open.iter().find(|t| t.state == *s))
            .map(|t| NextTask {
                id: t.id,
                title: clip(&t.title, NEXT_CAP),
            });
        let needs_you = open.iter().filter(|t| t.state == NEEDS_YOU_STATE).count();
        let next_wake = goal::next_wake(conn, &g)?;
        let b = g.budget.clone().unwrap_or_default();
        let summary = match &status {
            Some(s) => clip(&s.summary, SUMMARY_CAP),
            None => clip(first_sentence(intent_line(&g.intent)), SUMMARY_CAP),
        };
        out.push(GoalLine {
            id: g.id,
            others: g.bots.iter().filter(|b| *b != name).cloned().collect(),
            url: web.map(|w| goal_url(w, &g.name)),
            name: g.name,
            state: g.state,
            status: status.as_ref().map(|s| s.status.clone()),
            summary,
            metric: status.and_then(|s| s.metric),
            latest,
            open_tasks: open.iter().map(|t| t.id).collect(),
            next,
            needs_you,
            cost_spent: b.cost_spent,
            cost_week: g.budget_cost_week,
            attention_spent: b.attention_spent,
            attention_week: g.budget_attention_week,
            next_wake,
            deadline: g.deadline,
        });
    }
    Ok(out)
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// The status of the goal's latest status observation.
fn latest_status(conn: &Connection, goal: i64) -> Result<Option<goal::Status>> {
    let bodies: Vec<String> = conn
        .prepare(
            "SELECT body FROM goal_events WHERE goal_id=?1 AND kind='observation'
             ORDER BY id DESC LIMIT ?2",
        )?
        .query_map(rusqlite::params![goal, STATUS_SCAN], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(bodies.iter().find_map(|b| goal::status_of(b)))
}

/// Up to and including the first `. `, `! ` or `? ` of `s`.
fn first_sentence(s: &str) -> &str {
    s.char_indices()
        .find(|&(i, c)| matches!(c, '.' | '!' | '?') && s[i + 1..].starts_with(' '))
        .map_or(s, |(i, _)| &s[..=i])
}

/// At most `limit` characters, cut at a word boundary with `…`.
fn clip(s: &str, limit: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let cut = cap(s, limit);
    let body = cut.trim_end_matches('…');
    match body.rfind(' ') {
        Some(i) if i > body.len() / 2 => {
            format!("{}…", body[..i].trim_end_matches([',', ';', ':', ' ']))
        }
        _ => cut,
    }
}

/// `<web>/?goal=<name>`, the goal's page in the web dashboard.
fn goal_url(web: &str, name: &str) -> String {
    let mut q = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            q.push(b as char);
        } else {
            q.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{}/?goal={q}", web.trim_end_matches('/'))
}

/// The first prose line of a markdown intent: headings (`## Outcome`) are
/// skipped, and a list marker is dropped.
fn intent_line(s: &str) -> &str {
    let l = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");
    l.strip_prefix("- ")
        .or_else(|| l.strip_prefix("* "))
        .unwrap_or(l)
}

/// `Jan 31` for a date or RFC3339 time; anything else as it is.
fn day(s: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts = (
        s.get(5..7),
        s.get(8..10),
        s.as_bytes().get(4),
        s.as_bytes().get(7),
    );
    if let (Some(m), Some(d), Some(b'-'), Some(b'-')) = parts {
        if let (Ok(m), Ok(d)) = (m.parse::<usize>(), d.parse::<u32>()) {
            if (1..=12).contains(&m) {
                return format!("{} {d}", MONTHS[m - 1]);
            }
        }
    }
    s.to_string()
}

/// `$58` from $10 up, `$3.20` below.
fn money(x: f64) -> String {
    if x.abs() >= 10.0 {
        format!("${x:.0}")
    } else {
        format!("${x:.2}")
    }
}

/// One budget line in monospace: `Cost ▓░░░░░░░░░  12%  $58/$500 wk`. A
/// spend over its limit fills the bar and says `over`; no limit shows an
/// empty bar and `no limit`.
fn bar(label: &str, spent: f64, limit: Option<f64>, amount: &str) -> String {
    let line = match limit {
        None => format!(
            "{label} {}    -  {amount} wk, no limit",
            "░".repeat(BAR_CELLS)
        ),
        Some(l) => {
            let ratio = if l > 0.0 {
                spent / l
            } else if spent > 0.0 {
                f64::INFINITY
            } else {
                0.0
            };
            let filled = ((ratio * BAR_CELLS as f64).round().max(0.0) as usize).min(BAR_CELLS);
            let pct = if ratio.is_finite() {
                format!("{:.0}%", ratio * 100.0)
            } else {
                "max".to_string()
            };
            let over = if ratio > 1.0 { " over" } else { "" };
            format!(
                "{label} {}{} {pct:>4}  {amount} wk{over}",
                "▓".repeat(filled),
                "░".repeat(BAR_CELLS - filled)
            )
        }
    };
    format!("<code>{}</code>", escape(&line))
}

/// One goal's block of lines, the scoreboard layout:
///
/// ```text
/// <b>NAME</b> · <i>STATUS</i> · Jan 31
/// One-line summary.
/// metric now → target             (with a metric)
/// Next: short task title #ID · Needs you: 1 open card
/// Cost ▓░░░░░░░░░  12%  $58/$500 wk
/// Attn ▓▓▓▓▓▓▓▓░░  80%  16/20 wk
/// Open goal                       (a link, with a public URL)
/// ```
pub(crate) fn block(g: &GoalLine) -> String {
    let status = g.status.as_deref().unwrap_or(&g.state);
    let mut out = format!("\n\n<b>{}</b> · <i>{}</i>", escape(&g.name), escape(status));
    if g.state == "proposed" {
        out.push_str(" · awaiting your Go");
    }
    out.push_str(&format!(
        " · {}",
        g.deadline.as_deref().map_or("no deadline".into(), day)
    ));
    if !g.summary.is_empty() {
        out.push_str(&format!("\n{}", escape(&g.summary)));
    }
    if let Some(m) = &g.metric {
        out.push_str(&format!("\n{}", escape(m)));
    }
    let next = match &g.next {
        Some(t) => format!("{} #{}", escape(&t.title), t.id),
        None => "nothing open".to_string(),
    };
    let needs = match g.needs_you {
        0 => "nothing".to_string(),
        1 => "1 open card".to_string(),
        n => format!("{n} open cards"),
    };
    out.push_str(&format!("\nNext: {next} · Needs you: {needs}"));
    let cost = match g.cost_week {
        Some(l) => format!("{}/{}", money(g.cost_spent), money(l)),
        None => money(g.cost_spent),
    };
    out.push_str(&format!(
        "\n{}",
        bar("Cost", g.cost_spent, g.cost_week, &cost)
    ));
    let attention = match g.attention_week {
        Some(l) => format!("{}/{l}", g.attention_spent),
        None => g.attention_spent.to_string(),
    };
    out.push_str(&format!(
        "\n{}",
        bar(
            "Attn",
            g.attention_spent as f64,
            g.attention_week.map(|l| l as f64),
            &attention
        )
    ));
    if let Some(u) = &g.url {
        out.push_str(&format!(
            "\n<a href=\"{}\">Open goal</a>",
            escape(u).replace('"', "&quot;")
        ));
    }
    out
}

/// The goal board as Telegram HTML: one block per goal, then "Updated
/// <time>". Goals that do not fit become "+N more".
pub fn render(goals: &[GoalLine], now: i64) -> String {
    let mut out = String::from("<b>Goals</b>");
    if goals.is_empty() {
        out.push_str("\n\n");
        out.push_str(EMPTY);
    }
    let mut shown = 0;
    for g in goals {
        let b = block(g);
        if utf16(&out) + utf16(&b) > TEXT_LIMIT - FOOTER_ROOM {
            break;
        }
        out.push_str(&b);
        shown += 1;
    }
    if shown < goals.len() {
        out.push_str(&format!("\n\n+{} more", goals.len() - shown));
    }
    out.push_str(&format!(
        "\n\n<i>Updated {} UTC</i>",
        bot::ts(now)[..16].replace('T', " ")
    ));
    out
}

/// The text of goal `g`'s pinned message. It has no time in it, so it is
/// also the render key: the gateway edits the pin when the text changes.
pub fn pin_text(g: &GoalLine) -> String {
    block(g).trim_start().to_string()
}

/// The goal lines of bot `name` that have a pinned message.
pub fn pinned_lines(conn: &Connection, name: &str, web: Option<&str>) -> Result<Vec<GoalLine>> {
    Ok(lines(conn, name, web)?
        .into_iter()
        .filter(|g| g.state == PINNED_STATE)
        .collect())
}

/// The bot's goal pins (messages with `flags.goal_pin`), oldest first.
pub fn pins(conn: &Connection, name: &str) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM bot_messages WHERE bot_id=?1
               AND json_type(flags,'$.goal_pin')='integer' ORDER BY id",
        )?
        .query_map([b], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| get_message(conn, name, id))
        .collect()
}

/// Each goal of bot `name` that has a pinned message, with that message.
pub fn pin_views(conn: &Connection, name: &str, web: Option<&str>) -> Result<Vec<Pin>> {
    let pins = pins(conn, name)?;
    Ok(pinned_lines(conn, name, web)?
        .iter()
        .map(|g| {
            let m = pins
                .iter()
                .find(|m| m.flags["goal_pin"].as_i64() == Some(g.id));
            Pin {
                goal: g.name.clone(),
                message: m.map(|m| m.id),
                current: m.is_some_and(|m| {
                    m.flags["goal_pin_key"].as_str() == Some(pin_text(g).as_str())
                }),
            }
        })
        .collect())
}

/// The goal lines of bot `name`, and its old goal board message if one is
/// still set.
pub fn show(conn: &Connection, name: &str, now: i64, web: Option<&str>) -> Result<GoalBoard> {
    let goals = lines(conn, name, web)?;
    let msg = message(conn, name)?;
    Ok(GoalBoard {
        message: msg.as_ref().map(|m| m.id),
        text: render(&goals, now),
        goals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(name: &str, state: &str) -> GoalLine {
        GoalLine {
            id: 1,
            name: name.into(),
            state: state.into(),
            status: None,
            summary: String::new(),
            metric: None,
            others: vec![],
            latest: None,
            open_tasks: vec![],
            next: None,
            needs_you: 0,
            cost_spent: 0.0,
            cost_week: None,
            attention_spent: 0,
            attention_week: None,
            next_wake: None,
            deadline: None,
            url: None,
        }
    }

    /// A goal with a metric, read from its status observation.
    fn metric_goal() -> GoalLine {
        let s = goal::status_of(
            "at risk: Cost is trending down & under target.\nmetric: cost/day $59 -> $125\ndetail",
        )
        .unwrap();
        let mut g = goal("cut-cost", "active");
        g.status = Some(s.status);
        g.summary = s.summary;
        g.metric = s.metric;
        g.open_tasks = vec![535];
        g.next = Some(NextTask {
            id: 535,
            title: clip(
                "H1: shorter sessions cut cold cache reads by half or more",
                NEXT_CAP,
            ),
        });
        g.cost_spent = 0.33;
        g.cost_week = Some(35.0);
        g.attention_week = Some(3);
        g.deadline = Some("2026-12-08".into());
        g.url = Some(goal_url("http://axe.example/", "cut-cost"));
        g
    }

    #[test]
    fn metric_goal_renders_the_scoreboard() {
        assert_eq!(
            pin_text(&metric_goal()),
            "<b>cut-cost</b> · <i>at risk</i> · Dec 8\n\
             Cost is trending down &amp; under target.\n\
             cost/day $59 → $125\n\
             Next: H1: shorter sessions cut cold cache… #535 · Needs you: nothing\n\
             <code>Cost ░░░░░░░░░░   1%  $0.33/$35 wk</code>\n\
             <code>Attn ░░░░░░░░░░   0%  0/3 wk</code>\n\
             <a href=\"http://axe.example/?goal=cut-cost\">Open goal</a>"
        );
    }

    #[test]
    fn build_goal_renders_without_a_metric() {
        // No status observation: the state and the intent's first sentence.
        let mut g = goal("ship-v1", "active");
        g.summary = clip(
            first_sentence(intent_line("## Outcome\nShip version one. Then grow it.")),
            SUMMARY_CAP,
        );
        g.open_tasks = vec![880, 894];
        g.next = Some(NextTask {
            id: 894,
            title: clip("Goal pin: scoreboard layout", NEXT_CAP),
        });
        g.needs_you = 2;
        g.cost_spent = 58.12;
        g.cost_week = Some(500.0);
        g.attention_spent = 16;
        g.attention_week = Some(20);
        g.deadline = Some("2027-01-31".into());
        assert_eq!(
            pin_text(&g),
            "<b>ship-v1</b> · <i>active</i> · Jan 31\n\
             Ship version one.\n\
             Next: Goal pin: scoreboard layout #894 · Needs you: 2 open cards\n\
             <code>Cost ▓░░░░░░░░░  12%  $58/$500 wk</code>\n\
             <code>Attn ▓▓▓▓▓▓▓▓░░  80%  16/20 wk</code>"
        );
        g.needs_you = 1;
        g.next = None;
        assert!(
            pin_text(&g).contains("\nNext: nothing open · Needs you: 1 open card\n"),
            "{}",
            pin_text(&g)
        );
    }

    #[test]
    fn over_budget_fills_the_bar_and_says_over() {
        assert_eq!(
            bar("Cost", 650.0, Some(500.0), "$650/$500"),
            "<code>Cost ▓▓▓▓▓▓▓▓▓▓ 130%  $650/$500 wk over</code>"
        );
        assert_eq!(
            bar("Attn", 20.0, Some(20.0), "20/20"),
            "<code>Attn ▓▓▓▓▓▓▓▓▓▓ 100%  20/20 wk</code>"
        );
        assert_eq!(
            bar("Attn", 1.0, Some(0.0), "1/0"),
            "<code>Attn ▓▓▓▓▓▓▓▓▓▓  max  1/0 wk over</code>"
        );
        // No limit: always shown, empty.
        let g = goal("free", "active");
        let t = pin_text(&g);
        assert!(
            t.contains("\n<code>Cost ░░░░░░░░░░    -  $0.00 wk, no limit</code>\n<code>Attn ░░░░░░░░░░    -  0 wk, no limit</code>"),
            "{t}"
        );
    }

    #[test]
    fn status_observations_are_recognised() {
        let s = goal::status_of("On-Track: fine").unwrap();
        assert_eq!(
            (s.status.as_str(), s.summary.as_str(), s.metric),
            ("on track", "fine", None)
        );
        assert_eq!(goal::status_of("off track:  "), None);
        assert_eq!(goal::status_of("Two users signed up"), None);
        assert_eq!(goal::status_of("note: on track: x"), None);
        assert_eq!(
            goal::status_of("\nat risk: slow\n\nMetric: users 2 -> 10")
                .unwrap()
                .metric
                .as_deref(),
            Some("users 2 → 10")
        );
    }

    #[test]
    fn summaries_and_titles_are_clipped_at_words() {
        assert_eq!(first_sentence("Cut cost. Then more."), "Cut cost.");
        assert_eq!(first_sentence("v0.2 ships"), "v0.2 ships");
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("one two three four", 12), "one two…");
        assert_eq!(clip("abcdefghijklmnop", 8), "abcdefg…");
        assert_eq!(day("2027-01-31T00:00:00Z"), "Jan 31");
        assert_eq!(day("soon"), "soon");
        assert_eq!(money(9.5), "$9.50");
        assert_eq!(money(58.6), "$59");
        assert_eq!(goal_url("http://h", "a b&c"), "http://h/?goal=a%20b%26c");
    }

    #[test]
    fn renders_blocks_and_footer() {
        let mut later = goal("next", "proposed");
        later.summary = "Grow <it>".into();
        let t = render(&[metric_goal(), later], 0);
        assert!(
            t.starts_with("<b>Goals</b>\n\n<b>cut-cost</b> · <i>at risk</i> · Dec 8\n"),
            "{t}"
        );
        assert!(
            t.contains("\n\n<b>next</b> · <i>proposed</i> · awaiting your Go · no deadline\nGrow &lt;it&gt;\nNext: nothing open"),
            "{t}"
        );
        assert!(
            t.ends_with("\n\n<i>Updated 1970-01-01 00:00 UTC</i>"),
            "{t}"
        );
        assert_eq!(
            render(&[], 0),
            format!("<b>Goals</b>\n\n{EMPTY}\n\n<i>Updated 1970-01-01 00:00 UTC</i>")
        );
    }

    #[test]
    fn overflow_becomes_more_and_stays_under_the_limit() {
        let many: Vec<GoalLine> = (0..200)
            .map(|i| {
                let mut g = goal(&format!("goal-{i:03}-{}", "x".repeat(40)), "active");
                g.open_tasks = (0..5).collect();
                g
            })
            .collect();
        let t = render(&many, 0);
        assert!(utf16(&t) <= TEXT_LIMIT, "{}", utf16(&t));
        let shown = t.matches("<i>active</i>").count();
        assert!(shown > 5 && shown < 200, "{shown}");
        assert!(t.contains(&format!("+{} more", 200 - shown)), "{t}");
    }

    #[test]
    fn intent_line_skips_markdown_headings() {
        assert_eq!(
            intent_line("# Intent\n\n## Outcome\nCut bot run cost.\n\n## Done when\nx"),
            "Cut bot run cost."
        );
        assert_eq!(intent_line("## Outcome\n- Ship it"), "Ship it");
        assert_eq!(intent_line("Plain first line\nmore"), "Plain first line");
        assert_eq!(intent_line("## Only headings"), "");
    }

    #[test]
    fn pin_text_has_no_time_and_tracks_spend() {
        let mut g = goal("g", "active");
        g.cost_spent = 1.0;
        g.cost_week = Some(10.0);
        let t = pin_text(&g);
        assert!(!t.contains("Updated"), "{t}");
        g.cost_spent = 1.01;
        assert_ne!(pin_text(&g), t, "a spend change edits the pin");
    }
}
