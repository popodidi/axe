//! The "Waiting on you" board: one pinned message per bot that the server
//! renders from what waits on the owner, never a hand-kept list. Its inputs:
//!
//! - escalated tasks in the bot's subscribed projects (`*` is every project);
//! - the bot's open cards: its own text messages that still have inline
//!   buttons. A press (`flags.answered`), an edit that removes the keyboard,
//!   or a delete closes a card.
//!
//! The board message is the bot's own text message with `flags.board`; the
//! key of its last successful render is `flags.board_key`. When the key
//! changes, the Telegram gateway re-posts the board: it sends the new text
//! silently at the bottom of the chat and makes that message the board, with
//! `flags.board_from` naming the previous one until the move is finished
//! (new pinned, old unpinned and edited to "Moved below (#new)"). The first
//! render of a message just chosen with `set_message`, and every render with
//! `telegram.board = "edit"`, edits the message in place instead.
//!
//! Each listed item gets one inline button on its own row (`#349`,
//! `card #1667`), kept in `flags.board_buttons` (not `flags.buttons`, so the
//! board is never a card). A press is a button wake for the bot whose board
//! it is, with data naming the item (`board task #349`); it does not answer
//! or close the board, so every item stays pressable.
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;

use crate::bot::{self, epoch_of};
use crate::channel::{bot_id, get_message, StoredMessage};
use crate::subscription::ALL_PROJECTS;

/// Telegram's text limit (UTF-16 units; the board stays well inside it).
pub(crate) const TEXT_LIMIT: usize = 4096;
/// Room kept for the "+N more" line and the footer.
pub(crate) const FOOTER_ROOM: usize = 120;
const TITLE_CAP: usize = 60;
/// At most this many items are listed (each with its button); the rest
/// become "+N more", without buttons.
pub const MAX_ITEMS: usize = 25;
/// Prefix of the callback data of a board button.
pub const BUTTON_PREFIX: &str = "board ";
pub const EMPTY: &str = "Nothing waiting on you.";

/// One thing waiting on the owner.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    /// `task` or `card`.
    pub kind: &'static str,
    /// Task id, or the card's local message id.
    pub id: i64,
    pub title: String,
    /// Project of a task; empty for a card.
    pub project: String,
    /// When it started waiting (epoch seconds).
    pub since: i64,
}

/// What `bot tg board show` returns.
#[derive(Debug, Serialize)]
pub struct Board {
    /// Local id of the board message, if one is set.
    pub message: Option<i64>,
    pub lines: Vec<Line>,
    /// The text the next render sends.
    pub text: String,
    /// The inline buttons (`[label, data]`, one row each) it sends.
    pub buttons: Vec<(String, String)>,
    /// Whether the board message shows the current lines.
    pub current: bool,
}

/// The bot's board message, if one is set.
pub fn message(conn: &Connection, name: &str) -> Result<Option<StoredMessage>> {
    flagged(conn, name, "board")
}

/// Makes `msg` (a local id) the bot's board message, or with `None` unsets
/// it. The new board renders on the next pass.
pub fn set_message(conn: &mut Connection, name: &str, msg: Option<i64>) -> Result<Option<i64>> {
    set_flagged(
        conn,
        name,
        msg,
        "board",
        &["board", "board_key", "board_from", "board_buttons"],
        "goal_board",
    )
}

/// Makes `msg` the one message of bot `name` with `flags.<flag>` (removing
/// `clear` from the previous one), or with `None` unsets it. A board is
/// never a card, and one message is never both boards (`other`).
pub(crate) fn set_flagged(
    conn: &mut Connection,
    name: &str,
    msg: Option<i64>,
    flag: &str,
    clear: &[&str],
    other: &str,
) -> Result<Option<i64>> {
    let b = bot_id(conn, name)?;
    if let Some(m) = msg {
        let stored = get_message(conn, name, m)?;
        if stored.direction != "out" || stored.kind != "text" {
            bail!("message {m} is not one of the bot's own text messages");
        }
        if stored.flags["deleted"] == true {
            bail!("message {m} was deleted");
        }
        if stored.flags[other] == true {
            let what = if other == "board" {
                "\"Waiting on you\" board"
            } else {
                "goal board"
            };
            bail!("message {m} is already the {what}");
        }
    }
    let paths = clear
        .iter()
        .map(|k| format!("'$.{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let tx = conn.transaction()?;
    tx.execute(
        &format!(
            "UPDATE bot_messages SET flags=json_remove(flags,{paths})
             WHERE bot_id=?1 AND json_extract(flags,'$.{flag}') IS NOT NULL"
        ),
        [b],
    )?;
    if let Some(m) = msg {
        tx.execute(
            &format!(
                "UPDATE bot_messages SET flags=json_set(json_remove(flags,'$.buttons','$.answered'),'$.{flag}',json('true'))
                 WHERE id=?1 AND bot_id=?2"
            ),
            params![m, b],
        )?;
    }
    tx.commit()?;
    Ok(msg)
}

/// The bot's message with `flags.<flag>`, if one is set.
pub(crate) fn flagged(conn: &Connection, name: &str, flag: &str) -> Result<Option<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let id: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT id FROM bot_messages WHERE bot_id=?1 AND json_extract(flags,'$.{flag}')=1
                 ORDER BY id DESC LIMIT 1"
            ),
            [b],
            |r| r.get(0),
        )
        .optional()?;
    id.map(|id| get_message(conn, name, id)).transpose()
}

/// Records the key of a successful render, and the text and buttons the
/// message now shows.
pub fn rendered(
    conn: &Connection,
    msg: i64,
    key: &str,
    text: &str,
    buttons: &[(String, String)],
) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.board_key',?2,'$.edited',json('true'),'$.board_buttons',json(?3))
         WHERE id=?4",
        params![text, key, json!(buttons).to_string(), msg],
    )?;
    Ok(())
}

/// Makes the freshly sent message `new` the board in place of `old`, with
/// `key` rendered; `flags.board_from` keeps `old` until [`moved`] runs.
pub fn reposted(conn: &mut Connection, old: i64, new: i64, key: &str) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE bot_messages SET flags=json_remove(flags,'$.board','$.board_key','$.board_from','$.board_buttons') WHERE id=?1",
        [old],
    )?;
    tx.execute(
        "UPDATE bot_messages SET flags=json_set(flags,'$.board',json('true'),'$.board_key',?1,'$.board_from',?2) WHERE id=?3",
        params![key, old, new],
    )?;
    tx.commit()?;
    Ok(())
}

/// The text a replaced board is edited to.
pub fn moved_text(new: i64) -> String {
    format!("Moved below (#{new}).")
}

/// Records that the move of the board to `msg` is finished.
pub fn moved(conn: &Connection, msg: i64) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_remove(flags,'$.board_from') WHERE id=?1",
        [msg],
    )?;
    Ok(())
}

/// What waits on the owner for bot `name`: escalated tasks first (oldest
/// first), then open cards (oldest first).
pub fn lines(conn: &Connection, name: &str) -> Result<Vec<Line>> {
    let b = bot::get(conn, name)?;
    let projects: Vec<String> = conn
        .prepare("SELECT project FROM bot_subscriptions WHERE bot_id=?1")?
        .query_map([b.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let all = projects.iter().any(|p| p == ALL_PROJECTS);
    let mut out = Vec::new();
    if all || !projects.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.project, t.title, t.updated_at,
                    (SELECT MAX(e.at) FROM events e WHERE e.task_id=t.id
                       AND e.kind='state' AND e.to_value='escalated')
             FROM tasks t WHERE t.state='escalated' ORDER BY t.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, project, title, updated, escalated) = row?;
            if !all && !projects.contains(&project) {
                continue;
            }
            let since = escalated
                .as_deref()
                .and_then(epoch_of)
                .or_else(|| epoch_of(&updated))
                .unwrap_or(0);
            out.push(Line {
                kind: "task",
                id,
                title: short(&title),
                project,
                since,
            });
        }
    }
    let mut stmt = conn.prepare(
        "SELECT id, text, flags, at FROM bot_messages
         WHERE bot_id=?1 AND direction='out' AND kind='text'
           AND json_type(flags,'$.buttons')='array' AND json_array_length(flags,'$.buttons')>0
           AND json_type(flags,'$.answered') IS NULL
           AND json_type(flags,'$.deleted') IS NULL
           AND json_type(flags,'$.board') IS NULL
         ORDER BY id",
    )?;
    let rows = stmt.query_map([b.id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (id, text, flags, at) = row?;
        let html = serde_json::from_str::<serde_json::Value>(&flags)
            .map(|f| f["html"] == true)
            .unwrap_or(false);
        let plain = if html { strip_html(&text) } else { text };
        let first = plain
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        out.push(Line {
            kind: "card",
            id,
            title: short(first),
            project: String::new(),
            since: epoch_of(&at).unwrap_or(0),
        });
    }
    Ok(out)
}

/// The render key: what the board shows, without ages or the footer time, so
/// an unchanged input set never causes an edit. `v` marks the layout with
/// buttons, so boards rendered before it are rendered again.
pub fn key(lines: &[Line]) -> String {
    let v: Vec<_> = lines
        .iter()
        .map(|l| json!([l.kind, l.id, l.title]))
        .collect();
    json!({"v": 2, "lines": v}).to_string()
}

/// The pick button of one item: a short label and data naming it.
pub fn button(l: &Line) -> (String, String) {
    let item = match l.kind {
        "task" => format!("task #{}", l.id),
        _ => format!("card #{}", l.id),
    };
    let label = match l.kind {
        "task" => format!("#{}", l.id),
        _ => item.clone(),
    };
    (label, format!("{BUTTON_PREFIX}{item}"))
}

/// The board as Telegram HTML (one line per item, then "Updated <time>") and
/// its buttons, one per listed item. Items past [`MAX_ITEMS`] or that do not
/// fit become "+N more", without buttons.
pub fn render(lines: &[Line], now: i64) -> (String, Vec<(String, String)>) {
    let mut out = String::from("<b>Waiting on you</b>\n");
    if lines.is_empty() {
        out.push('\n');
        out.push_str(EMPTY);
    }
    let mut shown = 0;
    for l in lines.iter().take(MAX_ITEMS) {
        let line = match l.kind {
            "task" => format!(
                "\n• task #{} {} <i>({}, {})</i>",
                l.id,
                escape(&l.title),
                escape(&l.project),
                age(now - l.since)
            ),
            _ => format!(
                "\n• card #{} {} <i>({})</i>",
                l.id,
                escape(&l.title),
                age(now - l.since)
            ),
        };
        if utf16(&out) + utf16(&line) > TEXT_LIMIT - FOOTER_ROOM {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < lines.len() {
        out.push_str(&format!("\n+{} more", lines.len() - shown));
    }
    out.push_str(&format!(
        "\n\n<i>Updated {} UTC</i>",
        bot::ts(now)[..16].replace('T', " ")
    ));
    (out, lines[..shown].iter().map(button).collect())
}

/// The current board of bot `name`.
pub fn show(conn: &Connection, name: &str, now: i64) -> Result<Board> {
    let lines = lines(conn, name)?;
    let msg = message(conn, name)?;
    let k = key(&lines);
    let (text, buttons) = render(&lines, now);
    Ok(Board {
        message: msg.as_ref().map(|m| m.id),
        current: msg
            .as_ref()
            .is_some_and(|m| m.flags["board_key"].as_str() == Some(k.as_str())),
        text,
        buttons,
        lines,
    })
}

/// `45s`, `12m`, `3h`, `2d`.
pub fn age(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

pub(crate) fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= TITLE_CAP {
        return s.to_string();
    }
    let mut t: String = s.chars().take(TITLE_CAP - 1).collect();
    t.push('…');
    t
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Text of Telegram HTML: tags dropped, the common entities decoded.
pub(crate) fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

pub(crate) fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: &'static str, id: i64, title: &str) -> Line {
        Line {
            kind,
            id,
            title: title.into(),
            project: if kind == "task" {
                "p".into()
            } else {
                "".into()
            },
            since: 0,
        }
    }

    #[test]
    fn renders_lines_ages_and_footer() {
        let (t, buttons) = render(
            &[
                line("task", 7, "Ship <it> & go"),
                line("card", 12, "Pick one"),
            ],
            3 * 3600 + 5,
        );
        assert_eq!(
            buttons,
            [
                ("#7".to_string(), "board task #7".to_string()),
                ("card #12".to_string(), "board card #12".to_string()),
            ]
        );
        assert!(t.starts_with("<b>Waiting on you</b>\n"), "{t}");
        assert!(
            t.contains("• task #7 Ship &lt;it&gt; &amp; go <i>(p, 3h)</i>"),
            "{t}"
        );
        assert!(t.contains("• card #12 Pick one <i>(3h)</i>"), "{t}");
        assert!(
            t.ends_with("\n\n<i>Updated 1970-01-01 03:00 UTC</i>"),
            "{t}"
        );
        assert_eq!(
            render(&[], 0),
            (
                format!("<b>Waiting on you</b>\n\n{EMPTY}\n\n<i>Updated 1970-01-01 00:00 UTC</i>"),
                vec![]
            )
        );
    }

    #[test]
    fn overflow_becomes_more_and_stays_under_the_limit() {
        // Past MAX_ITEMS: the first MAX_ITEMS listed, each with a button.
        let many: Vec<Line> = (0..400)
            .map(|i| line(if i % 2 == 0 { "task" } else { "card" }, i, "x"))
            .collect();
        let (t, buttons) = render(&many, 0);
        assert_eq!(t.matches("\n• ").count(), MAX_ITEMS, "{t}");
        assert!(t.contains(&format!("+{} more", 400 - MAX_ITEMS)), "{t}");
        assert_eq!(buttons.len(), MAX_ITEMS);
        assert_eq!(buttons[0], ("#0".into(), "board task #0".into()));
        assert_eq!(buttons[1], ("card #1".into(), "board card #1".into()));
        assert_eq!(buttons[24].1, "board task #24");

        // Long escaped titles hit the text limit before MAX_ITEMS: only the
        // lines that fit are listed and have buttons.
        let long: Vec<Line> = (0..400)
            .map(|i| line("task", i, &"&".repeat(TITLE_CAP)))
            .collect();
        let (t, buttons) = render(&long, 0);
        assert!(utf16(&t) <= TEXT_LIMIT, "{}", utf16(&t));
        let shown = t.matches("• task").count();
        assert!(shown > 5 && shown < MAX_ITEMS, "{shown}");
        assert_eq!(buttons.len(), shown);
        assert!(t.contains(&format!("+{} more", 400 - shown)), "{t}");
    }

    #[test]
    fn buttons_fit_telegram_limits() {
        // Callback data is at most 64 bytes, labels are short.
        let (_, buttons) = render(&[line("card", i64::MAX, "x")], 0);
        let (label, data) = &buttons[0];
        assert!(data.len() <= 64 && label.len() <= 32, "{buttons:?}");
    }

    #[test]
    fn key_ignores_age_and_time() {
        let mut a = line("card", 1, "Q");
        let k = key(std::slice::from_ref(&a));
        a.since = 999;
        assert_eq!(k, key(&[a]));
    }

    #[test]
    fn ages_and_html_titles() {
        assert_eq!(age(-3), "0s");
        assert_eq!(age(61), "1m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(200_000), "2d");
        assert_eq!(
            strip_html("<b>Merge</b> &lt;PR&gt; &amp; go"),
            "Merge <PR> & go"
        );
        assert_eq!(short(&"y".repeat(80)).chars().count(), TITLE_CAP);
    }
}
