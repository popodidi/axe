//! The owner's inbox in a bot's chat: one pinned card per item that waits on
//! the owner, and one pinned overview, the newest pin. The server renders
//! both from stored state, never a hand-kept list. What waits on the owner:
//!
//! - escalated tasks in the bot's subscribed projects (`*` is every project);
//! - the bot's open cards: its own text messages that still have inline
//!   buttons. A press (`flags.answered`), an edit that removes the keyboard,
//!   or a delete closes a card. A card sent with `--task N` (`flags.task`)
//!   stands for escalated task N, and closes when the task leaves escalated.
//!
//! An escalated task with no card since its escalation gets one: the bot is
//! woken to post it with its real options, and [`CARD_WAIT`] after that wake
//! the server posts a minimal card itself ([`fallback_card`]). The gateway pins each open
//! card silently (`flags.card_pin`) and unpins it once it is closed.
//!
//! The overview is the bot's own text message with `flags.board`; the key of
//! its last successful render is `flags.board_key`. It carries the overall
//! stats (decisions waiting, running, blocked or waiting, done today) and the
//! goal lines of the goals the bot is attached to. When the key changes, or a
//! pinned card is newer than it (Telegram orders pins by sending date), the
//! gateway re-posts it: it sends the new text silently at the bottom of the
//! chat and makes that message the overview, with `flags.board_from` naming
//! the previous one until the move is finished (new pinned, old unpinned and
//! deleted, or edited to "Moved below (#new)" when Telegram refuses the
//! delete). The first render of a message just chosen with `set_message`, and
//! every other render with `telegram.board = "edit"`, edits it in place.
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;

use crate::bot::{self, epoch_of};
use crate::channel::{bot_id, get_message, StoredMessage};
use crate::goal_board::{self, GoalLine};
use crate::subscription::ALL_PROJECTS;

/// Telegram's text limit (UTF-16 units; the overview stays well inside it).
pub(crate) const TEXT_LIMIT: usize = 4096;
/// Room kept for the "+N more" line and the footer.
pub(crate) const FOOTER_ROOM: usize = 120;
const TITLE_CAP: usize = 60;
/// Ids listed per stats line before "+N".
const IDS_CAP: usize = 12;
/// Text of the blocked and resolver lines of a fallback card.
const NOTE_CAP: usize = 300;
/// How long an escalated task waits for its bot's card before the server
/// posts a minimal one (seconds since the bot was woken to post it).
pub const CARD_WAIT: i64 = 600;
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
    /// The escalated task a card stands for (`--task`).
    pub task: Option<i64>,
}

/// Overall task stats of the bot's subscribed projects.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stats {
    /// In-progress task ids.
    pub running: Vec<i64>,
    /// Task ids whose status is blocked (stored blocked, or pending on a
    /// prerequisite or a wake time).
    pub waiting: Vec<i64>,
    /// Task ids done since 00:00 UTC today.
    pub done_today: Vec<i64>,
}

/// What `bot tg board show` returns.
#[derive(Debug, Serialize)]
pub struct Board {
    /// Local id of the overview message, if one is set.
    pub message: Option<i64>,
    pub lines: Vec<Line>,
    pub stats: Stats,
    pub goals: Vec<GoalLine>,
    /// The text the next render sends.
    pub text: String,
    /// Whether the overview message shows the current state.
    pub current: bool,
}

/// An escalated task without a card since its escalation.
#[derive(Debug, Clone, PartialEq)]
pub struct CardNeed {
    pub task: i64,
    pub project: String,
    pub title: String,
    /// When it was escalated (epoch seconds).
    pub since: i64,
    /// The `blocked:` and `resolver:` lines of its escalation note.
    pub blocked: String,
    pub resolver: String,
}

/// The bot's overview message, if one is set.
pub fn message(conn: &Connection, name: &str) -> Result<Option<StoredMessage>> {
    flagged(conn, name, "board")
}

/// Makes `msg` (a local id) the bot's overview message, or with `None`
/// unsets it. The new overview renders on the next pass.
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
                "overview"
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
                "UPDATE bot_messages SET flags=json_set(json_remove(flags,'$.buttons','$.answered','$.task'),'$.{flag}',json('true'))
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

/// Records the key of a successful render and the text the message now shows.
pub fn rendered(conn: &Connection, msg: i64, key: &str, text: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET text=?1, flags=json_remove(json_set(flags,'$.board_key',?2,'$.edited',json('true')),'$.board_buttons')
         WHERE id=?3",
        params![text, key, msg],
    )?;
    Ok(())
}

/// Makes the freshly sent message `new` the overview in place of `old`, with
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

/// The text a replaced overview is edited to when it cannot be deleted.
pub fn moved_text(new: i64) -> String {
    format!("Moved below (#{new}).")
}

/// The text a retired goal board is edited to.
pub fn merged_text() -> &'static str {
    "Goals moved into the pinned overview."
}

/// Records that the move of the overview to `msg` is finished.
pub fn moved(conn: &Connection, msg: i64) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_remove(flags,'$.board_from') WHERE id=?1",
        [msg],
    )?;
    Ok(())
}

/// The bot's subscribed projects: (every project, the named ones).
fn projects(conn: &Connection, bot: i64) -> Result<(bool, Vec<String>)> {
    let projects: Vec<String> = conn
        .prepare("SELECT project FROM bot_subscriptions WHERE bot_id=?1")?
        .query_map([bot], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((projects.iter().any(|p| p == ALL_PROJECTS), projects))
}

/// Ids of the tasks in the bot's projects matching SQL `cond` on `tasks`.
fn task_ids(conn: &Connection, bot: i64, cond: &str, arg: &str) -> Result<Vec<i64>> {
    let (all, projects) = projects(conn, bot)?;
    if !all && projects.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(i64, String)> = conn
        .prepare(&format!(
            "SELECT tasks.id, tasks.project FROM tasks WHERE {cond} ORDER BY tasks.id"
        ))?
        .query_map(
            rusqlite::params_from_iter(Some(arg).filter(|a| !a.is_empty())),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, p)| all || projects.contains(p))
        .map(|(id, _)| id)
        .collect())
}

/// Escalated tasks in the bot's projects: (id, project, title, escalated at).
fn escalated(conn: &Connection, bot: i64) -> Result<Vec<(i64, String, String, i64)>> {
    let (all, projects) = projects(conn, bot)?;
    if !all && projects.is_empty() {
        return Ok(vec![]);
    }
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
    let mut out = Vec::new();
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
        out.push((id, project, title, since));
    }
    Ok(out)
}

/// SQL condition on `bot_messages` for an open card.
const OPEN_CARD: &str = "direction='out' AND kind='text'
    AND json_type(flags,'$.buttons')='array' AND json_array_length(flags,'$.buttons')>0
    AND json_type(flags,'$.answered') IS NULL
    AND json_type(flags,'$.deleted') IS NULL
    AND json_type(flags,'$.board') IS NULL";

/// The bot's open cards, oldest first.
fn open_cards(conn: &Connection, bot: i64) -> Result<Vec<Line>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, text, flags, at FROM bot_messages WHERE bot_id=?1 AND {OPEN_CARD} ORDER BY id"
    ))?;
    let rows = stmt.query_map([bot], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, text, flags, at) = row?;
        let flags: serde_json::Value = serde_json::from_str(&flags).unwrap_or_default();
        let plain = if flags["html"] == true {
            strip_html(&text)
        } else {
            text
        };
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
            task: flags["task"].as_i64(),
        });
    }
    Ok(out)
}

/// What waits on the owner for bot `name`: escalated tasks without an open
/// card first (oldest first), then open cards (oldest first). A card for an
/// escalated task stands for it.
pub fn lines(conn: &Connection, name: &str) -> Result<Vec<Line>> {
    let b = bot::get(conn, name)?;
    let cards = open_cards(conn, b.id)?;
    let mut out = Vec::new();
    for (id, project, title, since) in escalated(conn, b.id)? {
        if cards.iter().any(|c| c.task == Some(id)) {
            continue;
        }
        out.push(Line {
            kind: "task",
            id,
            title: short(&title),
            project,
            since,
            task: None,
        });
    }
    out.extend(cards);
    Ok(out)
}

/// Escalated tasks of bot `name` with no card sent since their escalation
/// (open or already closed), oldest first.
pub fn card_needs(conn: &Connection, name: &str) -> Result<Vec<CardNeed>> {
    let b = bot::get(conn, name)?;
    let mut out = Vec::new();
    for (task, project, title, since) in escalated(conn, b.id)? {
        let sent: Vec<String> = conn
            .prepare(
                "SELECT at FROM bot_messages WHERE bot_id=?1 AND direction='out'
                   AND json_extract(flags,'$.task')=?2",
            )?
            .query_map(params![b.id, task], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if sent
            .iter()
            .any(|at| epoch_of(at).is_some_and(|t| t >= since))
        {
            continue;
        }
        let note: Option<String> = conn
            .query_row(
                "SELECT body FROM events WHERE task_id=?1 AND kind='note' AND body LIKE 'blocked:%'
                 ORDER BY id DESC LIMIT 1",
                [task],
                |r| r.get(0),
            )
            .optional()?;
        let field = |k: &str| {
            note.as_deref()
                .unwrap_or("")
                .lines()
                .find_map(|l| l.strip_prefix(k))
                .map(|v| cap(v.trim(), NOTE_CAP))
                .unwrap_or_default()
        };
        out.push(CardNeed {
            task,
            project,
            title,
            since,
            blocked: field("blocked:"),
            resolver: field("resolver:"),
        });
    }
    Ok(out)
}

/// Open cards of bot `name` that stand for a task no longer escalated:
/// (local id, task id, its state).
pub fn stale_task_cards(conn: &Connection, name: &str) -> Result<Vec<(i64, i64, String)>> {
    let b = bot_id(conn, name)?;
    Ok(conn
        .prepare(&format!(
            "SELECT m.id, t.id, t.state FROM bot_messages m
             JOIN tasks t ON t.id=json_extract(m.flags,'$.task')
             WHERE m.bot_id=?1 AND {OPEN_CARD} AND t.state!='escalated' ORDER BY m.id"
        ))?
        .query_map([b], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Cards the server pinned that are closed now and still pinned.
pub fn closed_pins(conn: &Connection, name: &str) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let ids: Vec<i64> = conn
        .prepare(&format!(
            "SELECT id FROM bot_messages WHERE bot_id=?1
               AND json_extract(flags,'$.card_pin')=1 AND json_extract(flags,'$.pinned')=1
               AND NOT COALESCE(({OPEN_CARD}), 0) ORDER BY id"
        ))?
        .query_map([b], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| get_message(conn, name, id))
        .collect()
}

/// Overall stats of bot `name`'s projects at `now`.
pub fn stats(conn: &Connection, name: &str, now: i64) -> Result<Stats> {
    let b = bot_id(conn, name)?;
    let today = format!("{}T00:00:00", &bot::ts(now)[..10]);
    Ok(Stats {
        running: task_ids(conn, b, "tasks.state='in_progress'", "")?,
        waiting: task_ids(conn, b, &crate::db::waiting_sql(), "")?,
        done_today: task_ids(
            conn,
            b,
            "tasks.state='done' AND EXISTS (SELECT 1 FROM events e WHERE e.task_id=tasks.id
               AND e.kind='state' AND e.to_value='done' AND e.at>=?1)",
            &today,
        )?,
    })
}

/// The render key: what the overview shows, without ages or the footer time,
/// so an unchanged input set never causes a render. `v` marks the layout.
pub fn key(lines: &[Line], stats: &Stats, goals: &[GoalLine]) -> String {
    let v: Vec<_> = lines
        .iter()
        .map(|l| json!([l.kind, l.id, l.task]))
        .collect();
    json!({"v": 3, "lines": v, "stats": stats, "goals": goal_board::key(goals)}).to_string()
}

/// How an item is named on the overview: `#N` for a task (or a card that
/// stands for one), `card #N` for a card.
fn item(l: &Line) -> String {
    match (l.kind, l.task) {
        ("task", _) => format!("#{}", l.id),
        (_, Some(t)) => format!("#{t}"),
        _ => format!("card #{}", l.id),
    }
}

/// `Label: N · a b c +K`, or `Label: none`.
fn count_line(label: &str, items: &[String]) -> String {
    if items.is_empty() {
        return format!("\n{label}: none");
    }
    let mut s = format!(
        "\n{label}: {} · {}",
        items.len(),
        items
            .iter()
            .take(IDS_CAP)
            .map(|i| escape(i))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if items.len() > IDS_CAP {
        s.push_str(&format!(" +{}", items.len() - IDS_CAP));
    }
    s
}

fn ids(v: &[i64]) -> Vec<String> {
    v.iter().map(|i| format!("#{i}")).collect()
}

/// The overview as Telegram HTML: the stats, the goal blocks, then "Updated
/// <time>". Goals that do not fit become "+N more".
pub fn render(lines: &[Line], stats: &Stats, goals: &[GoalLine], now: i64) -> String {
    let mut out = String::from("<b>Overview</b>\n");
    let decisions: Vec<String> = lines.iter().map(item).collect();
    out.push_str(&count_line("Decisions waiting", &decisions));
    out.push_str(&count_line("Running", &ids(&stats.running)));
    out.push_str(&count_line("Blocked or waiting", &ids(&stats.waiting)));
    out.push_str(&count_line("Done today", &ids(&stats.done_today)));
    if lines.is_empty() {
        out.push_str("\n\n");
        out.push_str(EMPTY);
    } else {
        out.push_str("\n\n<i>Each decision is a pinned card above.</i>");
    }
    out.push_str("\n\n<b>Goals</b>");
    if goals.is_empty() {
        out.push('\n');
        out.push_str(goal_board::EMPTY);
    }
    let mut shown = 0;
    for g in goals {
        let b = goal_board::block(g);
        if utf16(&out) + utf16(&b) > TEXT_LIMIT - FOOTER_ROOM {
            break;
        }
        out.push_str(&b);
        shown += 1;
    }
    if shown < goals.len() {
        out.push_str(&format!("\n\n+{} more goals", goals.len() - shown));
    }
    out.push_str(&format!(
        "\n\n<i>Updated {} UTC</i>",
        bot::ts(now)[..16].replace('T', " ")
    ));
    out
}

/// The minimal card the server posts for an escalated task whose bot did not
/// post one in time: Telegram HTML and its buttons, one row each. A press is
/// an ordinary button wake for the bot (`task #N resolved` / `task #N discuss`).
pub fn fallback_card(n: &CardNeed) -> (String, Vec<(String, String)>) {
    let mut text = format!(
        "<b>Task #{}</b> · {}\n{}",
        n.task,
        escape(&n.project),
        escape(&short(&n.title))
    );
    if !n.blocked.is_empty() {
        text.push_str(&format!("\nBlocked: {}", escape(&n.blocked)));
    }
    if !n.resolver.is_empty() {
        text.push_str(&format!("\nResolver: {}", escape(&n.resolver)));
    }
    let buttons = vec![
        ("Resolved".to_string(), format!("task #{} resolved", n.task)),
        ("Discuss".to_string(), format!("task #{} discuss", n.task)),
    ];
    (text, buttons)
}

/// The note of the wake asking the bot to post a card; the server posts a
/// minimal one at `fallback` (epoch seconds).
pub fn card_wake_note(n: &CardNeed, fallback: i64) -> String {
    format!(
        "Task #{} ({}) is escalated and has no card. Post one with its real options: \
         axe bot tg send --task {} --buttons \"<label>=<data>;...\" \"<short blocked/resolver text>\". \
         Without one the server posts a minimal card at {}.",
        n.task,
        n.project,
        n.task,
        bot::ts(fallback)
    )
}

/// The current overview of bot `name`.
pub fn show(conn: &Connection, name: &str, now: i64) -> Result<Board> {
    let lines = lines(conn, name)?;
    let stats = stats(conn, name, now)?;
    let goals = goal_board::lines(conn, name)?;
    let msg = message(conn, name)?;
    let k = key(&lines, &stats, &goals);
    Ok(Board {
        message: msg.as_ref().map(|m| m.id),
        current: msg
            .as_ref()
            .is_some_and(|m| m.flags["board_key"].as_str() == Some(k.as_str())),
        text: render(&lines, &stats, &goals, now),
        lines,
        stats,
        goals,
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
    cap(s.trim(), TITLE_CAP)
}

/// At most `limit` characters.
fn cap(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut t: String = s.chars().take(limit - 1).collect();
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
            task: None,
        }
    }

    fn goal(name: &str) -> GoalLine {
        GoalLine {
            name: name.into(),
            state: "active".into(),
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
    fn renders_stats_goals_and_footer() {
        let mut task_card = line("card", 40, "Ship?");
        task_card.task = Some(9);
        let stats = Stats {
            running: vec![3, 4],
            waiting: vec![],
            done_today: (100..115).collect(),
        };
        let t = render(
            &[
                line("task", 7, "x"),
                line("card", 12, "Pick one"),
                task_card,
            ],
            &stats,
            &[goal("grow <it>")],
            3 * 3600 + 5,
        );
        assert!(t.starts_with("<b>Overview</b>\n\nDecisions waiting: 3 · #7 card #12 #9\nRunning: 2 · #3 #4\nBlocked or waiting: none\nDone today: 15 · #100 #101 #102 #103 #104 #105 #106 #107 #108 #109 #110 #111 +3\n\n<i>Each decision is a pinned card above.</i>\n\n<b>Goals</b>\n\n<b>grow &lt;it&gt;</b> · active"), "{t}");
        assert!(
            t.ends_with("\n\n<i>Updated 1970-01-01 03:00 UTC</i>"),
            "{t}"
        );
        let empty = render(&[], &Stats::default(), &[], 0);
        assert_eq!(
            empty,
            format!("<b>Overview</b>\n\nDecisions waiting: none\nRunning: none\nBlocked or waiting: none\nDone today: none\n\n{EMPTY}\n\n<b>Goals</b>\n{}\n\n<i>Updated 1970-01-01 00:00 UTC</i>", goal_board::EMPTY)
        );
    }

    #[test]
    fn many_goals_stay_under_the_limit() {
        let goals: Vec<GoalLine> = (0..200)
            .map(|i| goal(&format!("goal-{i:03}-{}", "x".repeat(40))))
            .collect();
        let many: Vec<Line> = (0..400).map(|i| line("task", i, "x")).collect();
        let t = render(&many, &Stats::default(), &goals, 0);
        assert!(utf16(&t) <= TEXT_LIMIT, "{}", utf16(&t));
        let shown = t.matches(" · active").count();
        assert!(shown > 5 && shown < 200, "{shown}");
        assert!(t.contains(&format!("+{} more goals", 200 - shown)), "{t}");
        assert!(t.contains("Decisions waiting: 400 · #0 "), "{t}");
        assert!(t.contains(&format!(" +{}\n", 400 - IDS_CAP)), "{t}");
    }

    #[test]
    fn key_ignores_age_titles_and_time() {
        let mut a = line("card", 1, "Q");
        let s = Stats::default();
        let k = key(std::slice::from_ref(&a), &s, &[]);
        a.since = 999;
        a.title = "R".into();
        assert_eq!(k, key(std::slice::from_ref(&a), &s, &[]));
        let more = Stats {
            running: vec![5],
            ..Stats::default()
        };
        assert_ne!(k, key(&[a.clone()], &more, &[]));
        assert_ne!(k, key(&[a], &s, &[goal("g")]));
    }

    #[test]
    fn fallback_cards_fit_telegram_limits() {
        let n = CardNeed {
            task: i64::MAX,
            project: "p<".into(),
            title: "t".repeat(200),
            since: 0,
            blocked: "needs <a> call".into(),
            resolver: "owner: decide".into(),
        };
        let (text, buttons) = fallback_card(&n);
        assert!(
            text.starts_with(&format!("<b>Task #{}</b> · p&lt;\n", i64::MAX)),
            "{text}"
        );
        assert!(
            text.contains("\nBlocked: needs &lt;a&gt; call\nResolver: owner: decide"),
            "{text}"
        );
        assert!(utf16(&text) < TEXT_LIMIT);
        for (label, data) in &buttons {
            assert!(data.len() <= 64 && label.len() <= 32, "{buttons:?}");
        }
        assert_eq!(buttons[0].1, format!("task #{} resolved", i64::MAX));
        assert!(card_wake_note(&n, 0).contains(&format!("axe bot tg send --task {}", i64::MAX)));
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
