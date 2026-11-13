//! The goals of a bot as Telegram shows them: one pinned message per active
//! goal (the "goal pin", `flags.goal_pin` holds the goal id), kept current
//! by the gateway (`telegram.rs`, `goal_pins`). Design: `docs/design/goals.md`,
//! "Goal pins". `show` renders the goal lines of every open goal (proposed,
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
/// Characters of the intent line.
const INTENT_CAP: usize = 160;
/// Task states that no longer count as open.
const CLOSED_TASK_STATES: [&str; 1] = ["done"];

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

/// One goal on the board.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GoalLine {
    pub id: i64,
    pub name: String,
    pub state: String,
    /// The first non-empty line of the intent, shortened.
    pub intent: String,
    /// Other bots attached to the goal.
    pub others: Vec<String>,
    pub latest: Option<Latest>,
    /// Open linked (H) tasks, by id.
    pub open_tasks: Vec<i64>,
    pub cost_spent: f64,
    pub cost_week: Option<f64>,
    pub attention_spent: i64,
    pub attention_week: Option<i64>,
    /// The next goal-tagged wake or schedule run of any bot (RFC3339).
    pub next_wake: Option<String>,
    pub deadline: Option<String>,
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
pub fn lines(conn: &Connection, name: &str) -> Result<Vec<GoalLine>> {
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
        let open_tasks = goal::tasks(conn, &g.name)?
            .into_iter()
            .filter(|t| !CLOSED_TASK_STATES.contains(&t.state.as_str()))
            .map(|t| t.id)
            .collect();
        let next_wake = goal::next_wake(conn, &g)?;
        let b = g.budget.clone().unwrap_or_default();
        let intent = cap(first_line(&g.intent), INTENT_CAP);
        out.push(GoalLine {
            id: g.id,
            others: g.bots.iter().filter(|b| *b != name).cloned().collect(),
            name: g.name,
            state: g.state,
            intent,
            latest,
            open_tasks,
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

/// `YYYY-MM-DD HH:MM UTC` for an RFC3339 time; a bare date as it is.
fn when(s: &str) -> String {
    if s.len() >= 16 && s.as_bytes()[10] == b'T' {
        format!("{} UTC", s[..16].replace('T', " "))
    } else {
        s.to_string()
    }
}

/// One goal's block of lines.
pub(crate) fn block(g: &GoalLine) -> String {
    let mut out = format!("\n\n<b>{}</b> · {}", escape(&g.name), escape(&g.state));
    if g.state == "proposed" {
        out.push_str(" · awaiting your Go");
    }
    if !g.others.is_empty() {
        out.push_str(&format!(" · with {}", escape(&g.others.join(", "))));
    }
    if !g.intent.is_empty() {
        out.push_str(&format!("\n{}", escape(&g.intent)));
    }
    if let Some(l) = &g.latest {
        out.push_str(&format!("\n{}: {}", l.kind, escape(&l.text)));
    }
    if g.open_tasks.is_empty() {
        out.push_str("\nno open tasks");
    } else {
        let ids: Vec<String> = g
            .open_tasks
            .iter()
            .map(|id| format!("<code>#{id}</code>"))
            .collect();
        out.push_str(&format!("\nopen {}", ids.join(" ")));
    }
    let cost = match g.cost_week {
        Some(l) => format!("${:.2}/${l:.2}", g.cost_spent),
        None => format!("${:.2}/no limit", g.cost_spent),
    };
    let attention = match g.attention_week {
        Some(l) => format!("attention {}/{l}", g.attention_spent),
        None => format!("attention {}/no limit", g.attention_spent),
    };
    out.push_str(&format!("\n{cost} · {attention}"));
    let mut tail = vec![match &g.next_wake {
        Some(w) => format!("next wake {}", when(w)),
        None => "no goal wake".to_string(),
    }];
    if let Some(d) = &g.deadline {
        tail.push(format!("deadline {}", when(d)));
    }
    out.push_str(&format!("\n{}", tail.join(" · ")));
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
pub fn pinned_lines(conn: &Connection, name: &str) -> Result<Vec<GoalLine>> {
    Ok(lines(conn, name)?
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
pub fn pin_views(conn: &Connection, name: &str) -> Result<Vec<Pin>> {
    let pins = pins(conn, name)?;
    Ok(pinned_lines(conn, name)?
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
pub fn show(conn: &Connection, name: &str, now: i64) -> Result<GoalBoard> {
    let goals = lines(conn, name)?;
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
            intent: String::new(),
            others: vec![],
            latest: None,
            open_tasks: vec![],
            cost_spent: 0.0,
            cost_week: None,
            attention_spent: 0,
            attention_week: None,
            next_wake: None,
            deadline: None,
        }
    }

    #[test]
    fn renders_blocks_and_footer() {
        let mut g = goal("grow <it>", "active");
        g.others = vec!["scout".into()];
        g.intent = "Reach 10 users & keep them".into();
        g.latest = Some(Latest {
            id: 3,
            kind: "verdict".into(),
            text: "H1 failed & why".into(),
        });
        g.open_tasks = vec![12, 14];
        g.cost_spent = 3.2;
        g.cost_week = Some(10.0);
        g.attention_spent = 2;
        g.attention_week = Some(5);
        g.next_wake = Some("2026-11-12T09:00:00Z".into());
        g.deadline = Some("2026-12-01".into());
        let t = render(&[g, goal("next", "proposed")], 0);
        assert!(t.starts_with("<b>Goals</b>\n\n<b>grow &lt;it&gt;</b> · active · with scout\nReach 10 users &amp; keep them\nverdict: H1 failed &amp; why\nopen <code>#12</code> <code>#14</code>\n$3.20/$10.00 · attention 2/5\nnext wake 2026-11-12 09:00 UTC · deadline 2026-12-01"), "{t}");
        assert!(t.contains("\n\n<b>next</b> · proposed · awaiting your Go\nno open tasks\n$0.00/no limit · attention 0/no limit\nno goal wake"), "{t}");
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
        let shown = t.matches(" · active").count();
        assert!(shown > 5 && shown < 200, "{shown}");
        assert!(t.contains(&format!("+{} more", 200 - shown)), "{t}");
    }

    #[test]
    fn pin_text_is_the_block_without_a_time() {
        let mut g = goal("g", "active");
        g.intent = "Ship it".into();
        g.cost_spent = 1.0;
        g.cost_week = Some(10.0);
        let t = pin_text(&g);
        assert_eq!(t, "<b>g</b> · active\nShip it\nno open tasks\n$1.00/$10.00 · attention 0/no limit\nno goal wake");
        g.cost_spent = 1.01;
        assert_ne!(pin_text(&g), t, "a spend change edits the pin");
    }
}
