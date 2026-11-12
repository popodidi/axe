//! The goal board: a second pinned message per bot that the server renders
//! from the goals the bot is attached to, separate from the "Waiting on you"
//! board (`board.rs`). Design: `docs/design/goals.md`, "Goal board".
//!
//! The goal board message is the bot's own text message with
//! `flags.goal_board`; the key of its last successful render is
//! `flags.goal_board_key`. Unlike the "Waiting on you" board it is always
//! edited in place and never re-posted, so it does not fight that board for
//! the top pin. The key bands budget spend in 10% steps of each weekly limit,
//! so a small spend change does not edit the message.
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;

use crate::board::{escape, short, utf16, FOOTER_ROOM, TEXT_LIMIT};
use crate::bot;
use crate::channel::StoredMessage;
use crate::goal;

pub const EMPTY: &str = "No active goals.";
/// Goal states the board shows; `done` and `dropped` are omitted.
pub const SHOWN_STATES: [&str; 3] = ["proposed", "active", "paused"];
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
    pub name: String,
    pub state: String,
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
    /// The text the next render sends.
    pub text: String,
    /// Whether the goal board message shows the current goals.
    pub current: bool,
}

/// The bot's goal board message, if one is set.
pub fn message(conn: &Connection, name: &str) -> Result<Option<StoredMessage>> {
    crate::board::flagged(conn, name, "goal_board")
}

/// Makes `msg` (a local id) the bot's goal board message, or with `None`
/// unsets it. The new goal board renders on the next pass.
pub fn set_message(conn: &mut Connection, name: &str, msg: Option<i64>) -> Result<Option<i64>> {
    crate::board::set_flagged(
        conn,
        name,
        msg,
        "goal_board",
        &["goal_board", "goal_board_key"],
        "board",
    )
}

/// Records the key of a successful render, and the text the message now shows.
pub fn rendered(conn: &Connection, msg: i64, key: &str, text: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.goal_board_key',?2,'$.edited',json('true'))
         WHERE id=?3",
        params![text, key, msg],
    )?;
    Ok(())
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
                text: short(
                    body.lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or(""),
                ),
            });
        let open_tasks = goal::tasks(conn, &g.name)?
            .into_iter()
            .filter(|t| !CLOSED_TASK_STATES.contains(&t.state.as_str()))
            .map(|t| t.id)
            .collect();
        let next_wake = goal::next_wake(conn, &g)?;
        let b = g.budget.clone().unwrap_or_default();
        out.push(GoalLine {
            others: g.bots.iter().filter(|b| *b != name).cloned().collect(),
            name: g.name,
            state: g.state,
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

/// Spend in 10% steps of `limit`; `None` without a limit, so spend toward no
/// limit never edits the board on its own.
fn band(spent: f64, limit: Option<f64>) -> Option<i64> {
    let l = limit?;
    if l <= 0.0 {
        return Some(i64::from(spent > 0.0));
    }
    Some((spent / l * 10.0).floor() as i64)
}

/// The render key: what the board shows, with budget spend banded in 10%
/// steps and without the footer time, so a small spend change or an
/// unchanged input set never causes an edit.
pub fn key(goals: &[GoalLine]) -> String {
    let v: Vec<_> = goals
        .iter()
        .map(|g| {
            json!([
                g.name,
                g.state,
                g.others,
                g.latest.as_ref().map(|l| l.id),
                g.open_tasks,
                band(g.cost_spent, g.cost_week),
                band(g.attention_spent as f64, g.attention_week.map(|l| l as f64)),
                g.cost_week,
                g.attention_week,
                g.next_wake,
                g.deadline,
            ])
        })
        .collect();
    json!(v).to_string()
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
fn block(g: &GoalLine) -> String {
    let mut out = format!("\n\n<b>{}</b> · {}", escape(&g.name), escape(&g.state));
    if g.state == "proposed" {
        out.push_str(" · awaiting your Go");
    }
    if !g.others.is_empty() {
        out.push_str(&format!(" · with {}", escape(&g.others.join(", "))));
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

/// The current goal board of bot `name`.
pub fn show(conn: &Connection, name: &str, now: i64) -> Result<GoalBoard> {
    let goals = lines(conn, name)?;
    let msg = message(conn, name)?;
    let k = key(&goals);
    Ok(GoalBoard {
        message: msg.as_ref().map(|m| m.id),
        current: msg
            .as_ref()
            .is_some_and(|m| m.flags["goal_board_key"].as_str() == Some(k.as_str())),
        text: render(&goals, now),
        goals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(name: &str, state: &str) -> GoalLine {
        GoalLine {
            name: name.into(),
            state: state.into(),
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
        assert!(t.starts_with("<b>Goals</b>\n\n<b>grow &lt;it&gt;</b> · active · with scout\nverdict: H1 failed &amp; why\nopen <code>#12</code> <code>#14</code>\n$3.20/$10.00 · attention 2/5\nnext wake 2026-11-12 09:00 UTC · deadline 2026-12-01"), "{t}");
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
    fn key_bands_spend_in_tenths() {
        let mut g = goal("g", "active");
        g.cost_week = Some(10.0);
        g.cost_spent = 1.0;
        let k = key(std::slice::from_ref(&g));
        g.cost_spent = 1.99;
        assert_eq!(key(std::slice::from_ref(&g)), k, "same 10% band");
        g.cost_spent = 2.0;
        assert_ne!(key(std::slice::from_ref(&g)), k, "next band");
        // Without a limit, spend alone never changes the key.
        let mut u = goal("u", "active");
        let k = key(std::slice::from_ref(&u));
        u.cost_spent = 500.0;
        u.attention_spent = 9;
        assert_eq!(key(std::slice::from_ref(&u)), k);
        assert_eq!(band(1.0, Some(0.0)), Some(1));
        assert_eq!(band(0.0, Some(0.0)), Some(0));
        assert_eq!(band(3.0, Some(5.0)), Some(6));
    }
}
