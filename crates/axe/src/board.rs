//! The board: one core primitive per bot, always on, that lists what the
//! owner should see about the bot (design: `docs/design/bots.md`, "Board").
//! It is a query plus one small table. The query derives, by reference, what
//! axe already stores; the table (`board_items`) holds only the bot's own
//! small items, written through typed verbs ([`add`], [`edit`],
//! [`set_state`], [`park`]). Nothing is copied: an item that names a task,
//! goal or card reads its title and state from the source at render time,
//! and an item whose ref is already a derived line merges into that line.
//!
//! Sections, in this fixed order ([`board`]):
//!
//! - **Decide**: escalated tasks in the bot's subscribed projects (`*` is
//!   every project) with no open card, then the bot's open cards. A card
//!   is one of its own text messages that still has inline buttons; a press
//!   (`flags.answered`), an edit that removes the keyboard or a delete
//!   closes it. A card sent with `--task N` (`flags.task`) stands for
//!   escalated task N and closes when the task leaves escalated. A weekly
//!   retro card (`flags.retro`) is never one.
//! - **Review**: parked decisions, items in the `Review` section ([`park`]);
//!   a parked task or card leaves Decide.
//! - **Goals**: the bot's active goals, one line each.
//! - The bot's named item sections (up to [`MAX_SECTIONS`]), then **Items**.
//! - **Done today**: items ticked since `done_since` (the view's choice).
//!
//! Views are thin adapters: the web shows the board as a tab on the bot
//! page, and a channel renders it (Telegram: one pinned message per chat,
//! `tg_board.rs`). A view's per-chat settings are rows of `board_views`
//! ([`set_view`]); the pinned message itself is channel state.
//!
//! An escalated task with no card since its escalation gets one, from one
//! bot: its owner bot, else its project's bot, else (neither exists) each bot
//! subscribed to its project ([`card_needs`]). That bot is woken to post it
//! with its real options, and [`CARD_WAIT`] after that wake the server posts a
//! minimal card itself ([`fallback_card`]) in the bot's home chat ([`home`]).
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::bot::{self, epoch_of};
use crate::channel::{bot_id, get_message, StoredMessage};
use crate::db::Actor;
use crate::goal_board::{self, GoalLine};
use crate::subscription::ALL_PROJECTS;

/// The board tables (schema v26, `db::open`).
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS board_items (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id),
 text TEXT NOT NULL DEFAULT '', ref TEXT, section TEXT NOT NULL DEFAULT '',
 state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','done','dropped')),
 source TEXT, pos INTEGER, until TEXT,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, closed_at TEXT
);
CREATE INDEX IF NOT EXISTS board_items_bot ON board_items (bot_id, state);
CREATE TABLE IF NOT EXISTS board_views (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id), channel TEXT NOT NULL,
 chat_id INTEGER NOT NULL, thread_id INTEGER NOT NULL DEFAULT 0,
 pin INTEGER NOT NULL DEFAULT 1, keyboard INTEGER NOT NULL DEFAULT 1, repost TEXT,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE (bot_id, channel, chat_id, thread_id)
);
";

/// v26: the board tables, and a Telegram view (pin on) for each bot's old
/// inbox chat (the chat of its message with `flags.board`). The old pins
/// (`flags.card_pin`, `flags.goal_pin`, the overview) are unpinned by the
/// gateway's first pass ([`legacy_pins`]). Idempotent.
pub(crate) fn migrate(tx: &rusqlite::Transaction) -> Result<()> {
    tx.execute_batch(SCHEMA)?;
    let now = bot::ts(crate::remote::epoch());
    tx.execute(
        "INSERT OR IGNORE INTO board_views (bot_id, channel, chat_id, thread_id, repost, created_at, updated_at)
         SELECT m.bot_id, 'telegram', m.chat_id, COALESCE(m.thread_id, 0), ?1, ?2, ?2
         FROM bot_messages m WHERE json_extract(m.flags,'$.board')=1
           AND m.id = (SELECT MAX(id) FROM bot_messages x WHERE x.bot_id=m.bot_id
                       AND json_extract(x.flags,'$.board')=1)",
        params![DEFAULT_REPOST, now],
    )?;
    Ok(())
}

/// Telegram's text limit (UTF-16 units).
pub(crate) const TEXT_LIMIT: usize = 4096;
/// Room kept for the "+N more" line and the footer.
pub(crate) const FOOTER_ROOM: usize = 120;
const TITLE_CAP: usize = 60;
/// Text of the blocked and resolver lines of a fallback card.
const NOTE_CAP: usize = 300;
/// How long an escalated task waits for its bot's card before the server
/// posts a minimal one (seconds since the bot was woken to post it).
pub const CARD_WAIT: i64 = 600;
pub const EMPTY: &str = "Nothing on the board.";
/// Open items per bot.
pub const MAX_OPEN: usize = 50;
/// Characters of an item's text.
pub const ITEM_CAP: usize = 200;
/// Named item sections per bot, besides Items and Review.
pub const MAX_SECTIONS: usize = 3;
/// Characters of a section name.
pub const SECTION_CAP: usize = 24;
/// The section of parked decisions.
pub const REVIEW: &str = "Review";
/// The default repost cadence of a new view: workdays at 10:00 in the
/// bot's timezone ([`timezone`]).
pub const DEFAULT_REPOST: &str = "0 10 * * 1-5";
/// Channels a view can be on.
pub const CHANNELS: [&str; 1] = ["telegram"];

/// One thing waiting on the owner (a Decide line before merging).
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

/// One item the bot keeps on its board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "BoardItem"))]
pub struct Item {
    pub id: i64,
    /// Its own words; may be empty when `ref` names a task, goal or card.
    pub text: String,
    /// `task:N`, `goal:N`, `card:N` (a local message id), `doc:<path>`,
    /// `blob:<sha256>` or an `http(s)://` URL.
    #[serde(rename = "ref")]
    #[cfg_attr(feature = "ts", ts(rename = "ref"))]
    pub reference: Option<String>,
    /// Empty: Items; `Review`: a parked decision; else a named section.
    pub section: String,
    /// What a parked decision came from: `card:N` or `task:N` ([`park`]).
    pub source: Option<String>,
    /// `open`, `done` or `dropped`.
    pub state: String,
    pub pos: Option<i64>,
    /// Shown as overdue after this time (RFC3339); never removed by it.
    pub until: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

/// One line of the board, as every view renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "BoardLine"))]
pub struct BoardLine {
    /// `task`, `card`, `goal` or `item`.
    pub kind: String,
    /// Task id, card message id, goal id or item id.
    pub id: i64,
    /// What the line says (plain text).
    pub text: String,
    /// The bot's words on a derived line (a merged item).
    pub note: Option<String>,
    /// The item behind the line (an item, or the one merged into it).
    pub item: Option<i64>,
    /// The task a card stands for, or the task an item names.
    pub task: Option<i64>,
    /// The item's ref, as written.
    #[serde(rename = "ref")]
    #[cfg_attr(feature = "ts", ts(rename = "ref"))]
    pub reference: Option<String>,
    /// Where the line links to, signed or built at render time.
    pub link: Option<String>,
    /// What the link is: `task #12`, `goal`, `doc`, `file`, `link`.
    pub link_label: Option<String>,
    /// When it started (epoch seconds).
    pub since: i64,
    /// `open` or `done`.
    pub state: String,
    /// Past its `until`.
    pub overdue: bool,
    /// A goal's status and budget, one line.
    pub detail: Option<String>,
}

/// One section of the board.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "BoardSection"))]
pub struct Section {
    /// `decide`, `review`, `goals`, `items` (Items and named item sections)
    /// or `done`.
    pub key: String,
    pub title: String,
    pub lines: Vec<BoardLine>,
}

/// A bot's board: its non-empty sections in order, and its views.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Board {
    pub bot: String,
    pub sections: Vec<Section>,
    /// Decide lines.
    pub decide: usize,
    /// Open items.
    pub open_items: usize,
    /// Done today counts from this time (epoch seconds).
    pub done_since: i64,
    pub views: Vec<View>,
}

/// Where a board is shown in a channel, with its settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "BoardView"))]
pub struct View {
    pub id: i64,
    pub channel: String,
    pub chat_id: i64,
    /// 0: the chat itself, not a topic.
    pub thread_id: i64,
    /// The board is pinned in this chat; off, it does not appear there.
    pub pin: bool,
    /// The pin's button row.
    pub keyboard: bool,
    /// When a changed board is posted again as a new pin (cron, in the bot's
    /// timezone); `None` edits in place only.
    pub repost: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A new item ([`add`]).
#[derive(Debug, Default, Clone, Deserialize)]
pub struct NewItem {
    #[serde(default)]
    pub text: String,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
}

/// An item change ([`edit`]); absent fields stay, an empty string clears
/// `ref`, `section` or `until`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ItemEdit {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub pos: Option<i64>,
}

/// What to park for review ([`park`]): a card, a task, or text alone, with
/// an optional doc ref.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Park {
    #[serde(default)]
    pub card: Option<i64>,
    #[serde(default)]
    pub task: Option<i64>,
    #[serde(default)]
    pub text: String,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
}

/// A view change ([`set_view`]); absent fields stay. `repost` `off` turns
/// reposts off.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ViewEdit {
    #[serde(default)]
    pub pin: Option<bool>,
    #[serde(default)]
    pub keyboard: Option<bool>,
    #[serde(default)]
    pub repost: Option<String>,
}

/// Builds the links of a board.
pub struct Links<'a> {
    /// The server's public URL (`[blobs] public_url`).
    pub web: Option<&'a str>,
    /// A fresh signed link to a blob, when the server has a blob store.
    pub blob: &'a dyn Fn(&str) -> Option<String>,
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

/// The bot's subscribed projects: (every project, the named ones).
pub(crate) fn projects(conn: &Connection, bot: i64) -> Result<(bool, Vec<String>)> {
    let projects: Vec<String> = conn
        .prepare(
            "SELECT project FROM bot_subscriptions WHERE bot_id=?1 AND project NOT LIKE '#%'
          AND project NOT LIKE 'service:%' AND project NOT LIKE 'routine:%'",
        )?
        .query_map([bot], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((projects.iter().any(|p| p == ALL_PROJECTS), projects))
}

/// Escalated tasks in the bot's projects: (id, project, title, escalated at).
fn escalated(conn: &Connection, bot: i64) -> Result<Vec<(i64, String, String, i64)>> {
    let (all, projects) = projects(conn, bot)?;
    if !all && projects.is_empty() {
        return Ok(vec![]);
    }
    Ok(escalated_tasks(conn)?
        .into_iter()
        .filter(|t| all || projects.contains(&t.project))
        .map(|t| (t.id, t.project, t.title, t.since))
        .collect())
}

/// An escalated task, in any project.
struct Escalated {
    id: i64,
    project: String,
    title: String,
    /// When it was escalated (epoch seconds).
    since: i64,
    owner: Option<String>,
}

/// Every escalated task, by id.
fn escalated_tasks(conn: &Connection) -> Result<Vec<Escalated>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.project, t.title, t.updated_at,
                (SELECT MAX(e.at) FROM events e WHERE e.task_id=t.id
                   AND e.kind='state' AND e.to_value='escalated'),
                t.owner
         FROM tasks t WHERE t.state='escalated' ORDER BY t.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, project, title, updated, escalated, owner) = row?;
        let since = escalated
            .as_deref()
            .and_then(epoch_of)
            .or_else(|| epoch_of(&updated))
            .unwrap_or(0);
        out.push(Escalated {
            id,
            project,
            title,
            since,
            owner,
        });
    }
    Ok(out)
}

/// The one bot that posts the card of an escalated task (its card wake and
/// fallback): its owner bot (`owner` is `bot:<name>`, renames followed), else
/// the bot whose home project is the task's (an active one first, then the
/// oldest). `None`: neither exists, and every bot subscribed to the project
/// gets it.
fn card_bot(conn: &Connection, owner: Option<&str>, project: &str) -> Result<Option<i64>> {
    if let Some(name) = owner.and_then(|o| o.strip_prefix("bot:")) {
        if let Some(name) = bot::bound_name(conn, name)? {
            return Ok(Some(bot_id(conn, &name)?));
        }
    }
    Ok(conn
        .query_row(
            "SELECT id FROM bots WHERE project=?1 ORDER BY state!='active', id LIMIT 1",
            [project],
            |r| r.get(0),
        )
        .optional()?)
}

/// SQL condition on `bot_messages` for an open card.
const OPEN_CARD: &str = "direction='out' AND kind='text'
    AND json_type(flags,'$.buttons')='array' AND json_array_length(flags,'$.buttons')>0
    AND json_type(flags,'$.answered') IS NULL
    AND json_type(flags,'$.deleted') IS NULL
    AND json_type(flags,'$.board_view') IS NULL
    AND json_type(flags,'$.retro') IS NULL";

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

/// The bot's open cards for an owner-message wake, oldest first
/// (`bot::render_prompt`, `bot::hook_output`).
pub fn open_card_list(conn: &Connection, bot: i64) -> Result<Vec<bot::OpenCard>> {
    Ok(open_cards(conn, bot)?
        .into_iter()
        .map(|l| bot::OpenCard {
            id: l.id,
            text: l.title,
            at: bot::ts(l.since),
        })
        .collect())
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

/// Escalated tasks whose card bot ([`card_bot`]) is bot `name`, with no card
/// sent for them by any bot since their escalation (open or already closed),
/// oldest first.
pub fn card_needs(conn: &Connection, name: &str) -> Result<Vec<CardNeed>> {
    let b = bot::get(conn, name)?;
    let (all, projects) = projects(conn, b.id)?;
    let mut out = Vec::new();
    for e in escalated_tasks(conn)? {
        let mine = match card_bot(conn, e.owner.as_deref(), &e.project)? {
            Some(id) => id == b.id,
            None => all || projects.contains(&e.project),
        };
        if !mine {
            continue;
        }
        let Escalated {
            id: task,
            project,
            title,
            since,
            ..
        } = e;
        let sent: Vec<String> = conn
            .prepare(
                "SELECT at FROM bot_messages WHERE direction='out'
                   AND json_extract(flags,'$.task')=?1",
            )?
            .query_map([task], |r| r.get(0))?
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

/// The minimal card the server posts for an escalated task whose bot did not
/// post one in time: Telegram HTML and its buttons, one row each. Resolved is
/// an ordinary button wake for the bot (`task #N resolved`); Park parks the
/// task for review with no wake but the one asking for its doc.
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
        ("Park".to_string(), format!("brd:p:task:{}", n.task)),
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

const ITEM_COLS: &str =
    "id, text, ref, section, source, state, pos, until, created_at, updated_at, closed_at";

fn item_row(r: &rusqlite::Row) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        text: r.get(1)?,
        reference: r.get(2)?,
        section: r.get(3)?,
        source: r.get(4)?,
        state: r.get(5)?,
        pos: r.get(6)?,
        until: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
        closed_at: r.get(10)?,
    })
}

/// Bot `bot`'s items: the open ones, and those closed at or after `since`
/// (epoch seconds; `None`: none closed). Ordered by `pos`, then age.
fn items_of(conn: &Connection, bot: i64, since: Option<i64>) -> Result<Vec<Item>> {
    let since = since.map(bot::ts);
    Ok(conn
        .prepare(&format!(
            "SELECT {ITEM_COLS} FROM board_items WHERE bot_id=?1
               AND (state='open' OR (?2 IS NOT NULL AND closed_at>=?2))
             ORDER BY pos IS NULL, pos, id"
        ))?
        .query_map(params![bot, since], item_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Bot `name`'s open items, for `board show` and the API.
pub fn items(conn: &Connection, name: &str) -> Result<Vec<Item>> {
    items_of(conn, bot_id(conn, name)?, None)
}

fn get_item(conn: &Connection, bot: i64, name: &str, id: i64) -> Result<Item> {
    conn.query_row(
        &format!("SELECT {ITEM_COLS} FROM board_items WHERE id=?1 AND bot_id=?2"),
        params![id, bot],
        item_row,
    )
    .optional()?
    .with_context(|| format!("board item {id} not found for bot {name}"))
}

fn open_count(conn: &Connection, bot: i64) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM board_items WHERE bot_id=?1 AND state='open'",
        [bot],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// A ref, checked and in its stored form: `task:N`, `goal:N` (a goal name
/// becomes its id), `card:N` (one of bot `name`'s own messages), `doc:<path>`,
/// `blob:<sha256>` or an `http(s)://` URL.
fn check_ref(conn: &Connection, name: &str, r: &str) -> Result<String> {
    let r = r.trim();
    let (kind, rest) = r.split_once(':').unwrap_or((r, ""));
    let rest = rest.trim();
    let id = || -> Result<i64> {
        rest.trim_start_matches('#')
            .parse()
            .with_context(|| format!("ref {r:?}: expected {kind}:<id>"))
    };
    Ok(match kind {
        "task" => {
            let n = id()?;
            crate::db::get_task(conn, n)?;
            format!("task:{n}")
        }
        "goal" => {
            let n = match rest.parse::<i64>() {
                Ok(n) => {
                    crate::goal::name_of(conn, n)?.with_context(|| format!("goal {n} not found"))?;
                    n
                }
                Err(_) => crate::goal::get(conn, rest)?.id,
            };
            format!("goal:{n}")
        }
        "card" => {
            let n = id()?;
            let m = get_message(conn, name, n)?;
            if m.direction != "out" {
                bail!("ref {r:?}: message {n} is not one of the bot's own");
            }
            format!("card:{n}")
        }
        "doc" if !rest.is_empty() => format!("doc:{rest}"),
        "blob" => {
            crate::blob::check_sha(rest)?;
            format!("blob:{rest}")
        }
        "http" | "https" if rest.starts_with("//") && r.len() <= 1000 => r.to_string(),
        _ => bail!(
            "ref {r:?}: expected task:N, goal:N, card:N, doc:<path>, blob:<sha256> or an http(s) URL"
        ),
    })
}

/// `task:N`, `goal:N` and `card:N` refs give a line its title.
fn titled(r: Option<&str>) -> bool {
    r.is_some_and(|r| ["task:", "goal:", "card:"].iter().any(|p| r.starts_with(p)))
}

fn check_text(text: &str, r: Option<&str>) -> Result<String> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() && !titled(r) {
        bail!("an item needs text, or a task:, goal: or card: ref to take its title from");
    }
    let n = t.chars().count();
    if n > ITEM_CAP {
        bail!("item text is {n} characters (max {ITEM_CAP}): shorten it, and link the detail with --ref");
    }
    Ok(t)
}

/// A section name in its stored form: empty (or `Items`) is Items, any
/// case of `review` is [`REVIEW`].
fn check_section(s: &str) -> Result<String> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() || s.eq_ignore_ascii_case("items") {
        return Ok(String::new());
    }
    if s.eq_ignore_ascii_case(REVIEW) {
        return Ok(REVIEW.into());
    }
    if s.chars().count() > SECTION_CAP {
        bail!("section name {s:?} is over {SECTION_CAP} characters");
    }
    Ok(s)
}

/// A new named section must leave at most [`MAX_SECTIONS`] in use.
fn check_new_section(conn: &Connection, bot: i64, section: &str) -> Result<()> {
    if section.is_empty() || section == REVIEW {
        return Ok(());
    }
    let used: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT section FROM board_items WHERE bot_id=?1 AND state='open'
               AND section NOT IN ('', 'Review')",
        )?
        .query_map([bot], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !used.iter().any(|u| u == section) && used.len() >= MAX_SECTIONS {
        bail!(
            "the board has {MAX_SECTIONS} named sections already ({}): use one of them or Items",
            used.join(", ")
        );
    }
    Ok(())
}

/// `until` as RFC3339: a time, a date (end of that day, UTC) or a
/// duration from now (`3d`).
fn check_until(s: &str, now: i64) -> Result<Option<String>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Some(t) = epoch_of(s) {
        return Ok(Some(bot::ts(t)));
    }
    if let Some(t) = epoch_of(&format!("{s}T23:59:59Z")) {
        return Ok(Some(bot::ts(t)));
    }
    let d = bot::parse_duration(s)
        .with_context(|| format!("--until {s:?}: expected a time, a date or a duration"))?;
    Ok(Some(bot::ts(now + d)))
}

fn item_event(
    conn: &Connection,
    bot: i64,
    actor: &Actor,
    action: &str,
    item: &Item,
    why: &str,
) -> Result<()> {
    let mut body = serde_json::json!({"action": action, "item": item.id, "text": item.text,
        "ref": item.reference, "section": item.section, "state": item.state});
    if !why.is_empty() {
        body["why"] = why.into();
    }
    bot::event(conn, bot, actor, "board", &body.to_string())
}

/// Adds an item to bot `name`'s board. At most [`MAX_OPEN`] are open; an
/// open item with the same ref in the same section is refused (edit it).
pub fn add(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    n: &NewItem,
    now: i64,
) -> Result<Item> {
    let b = bot_id(conn, name)?;
    let reference = n
        .reference
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .map(|r| check_ref(conn, name, r))
        .transpose()?;
    let text = check_text(&n.text, reference.as_deref())?;
    let section = check_section(n.section.as_deref().unwrap_or(""))?;
    let until = check_until(n.until.as_deref().unwrap_or(""), now)?;
    insert(
        conn, actor, name, b, text, reference, section, None, until, now, "add",
    )
}

#[allow(clippy::too_many_arguments)]
fn insert(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    b: i64,
    text: String,
    reference: Option<String>,
    section: String,
    source: Option<String>,
    until: Option<String>,
    now: i64,
    action: &str,
) -> Result<Item> {
    let tx = conn.transaction()?;
    if open_count(&tx, b)? >= MAX_OPEN {
        bail!("bot {name} has {MAX_OPEN} open board items (the most): drop or merge some first (axe bot board drop ID)");
    }
    check_new_section(&tx, b, &section)?;
    for (col, v) in [("ref", &reference), ("source", &source)] {
        if let Some(r) = v {
            let dup: Option<i64> = tx
                .query_row(
                    &format!(
                        "SELECT id FROM board_items WHERE bot_id=?1 AND state='open' AND section=?2 AND {col}=?3"
                    ),
                    params![b, section, r],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(d) = dup {
                bail!("item {d} already refers to {r}: edit it (axe bot board edit {d})");
            }
        }
    }
    let at = bot::ts(now);
    tx.execute(
        "INSERT INTO board_items (bot_id, text, ref, section, source, until, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
        params![b, text, reference, section, source, until, at],
    )?;
    let item = get_item(&tx, b, name, tx.last_insert_rowid())?;
    item_event(&tx, b, actor, action, &item, "")?;
    tx.commit()?;
    Ok(item)
}

/// Changes item `id` of bot `name`: its text, ref, section, until or
/// position (`pos`, lower first).
pub fn edit(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    id: i64,
    e: &ItemEdit,
    now: i64,
) -> Result<Item> {
    let b = bot_id(conn, name)?;
    let old = get_item(conn, b, name, id)?;
    let reference = match e.reference.as_deref() {
        None => old.reference.clone(),
        Some(r) if r.trim().is_empty() => None,
        Some(r) => Some(check_ref(conn, name, r)?),
    };
    let text = check_text(e.text.as_deref().unwrap_or(&old.text), reference.as_deref())?;
    let section = match e.section.as_deref() {
        None => old.section.clone(),
        Some(s) => check_section(s)?,
    };
    let until = match e.until.as_deref() {
        None => old.until.clone(),
        Some(u) => check_until(u, now)?,
    };
    let pos = e.pos.or(old.pos);
    let tx = conn.transaction()?;
    if section != old.section && old.state == "open" {
        check_new_section(&tx, b, &section)?;
    }
    tx.execute(
        "UPDATE board_items SET text=?1, ref=?2, section=?3, until=?4, pos=?5, updated_at=?6 WHERE id=?7",
        params![text, reference, section, until, pos, bot::ts(now), id],
    )?;
    let item = get_item(&tx, b, name, id)?;
    item_event(&tx, b, actor, "edit", &item, "")?;
    tx.commit()?;
    Ok(item)
}

/// `done`, `drop` or `reopen` item `id` of bot `name`.
pub fn set_state(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    id: i64,
    action: &str,
    why: &str,
    now: i64,
) -> Result<Item> {
    let b = bot_id(conn, name)?;
    let to = match action {
        "done" => "done",
        "drop" => "dropped",
        "reopen" => "open",
        a => bail!("unknown board action {a:?} (expected done, drop or reopen)"),
    };
    let tx = conn.transaction()?;
    let old = get_item(&tx, b, name, id)?;
    if old.state == to {
        return Ok(old);
    }
    if to == "open" && open_count(&tx, b)? >= MAX_OPEN {
        bail!("bot {name} has {MAX_OPEN} open board items (the most): drop or merge some first");
    }
    let at = bot::ts(now);
    let closed = (to != "open").then_some(at.clone());
    tx.execute(
        "UPDATE board_items SET state=?1, closed_at=?2, updated_at=?3 WHERE id=?4",
        params![to, closed, at, id],
    )?;
    let item = get_item(&tx, b, name, id)?;
    item_event(&tx, b, actor, action, &item, why)?;
    tx.commit()?;
    Ok(item)
}

/// Parks a decision for review: a Review item for card `card` (which
/// closes, `flags.closed`; the channel drops its keyboard, `flags.parked`),
/// task `task` (which leaves Decide while the item is open), or `text`
/// alone; `ref` is its doc. Parking the same card or task again returns
/// the open item.
pub fn park(conn: &mut Connection, actor: &Actor, name: &str, p: &Park, now: i64) -> Result<Item> {
    let b = bot_id(conn, name)?;
    let source = match (p.card, p.task) {
        (Some(_), Some(_)) => bail!("park a card or a task, not both"),
        (Some(c), None) => {
            let m = get_message(conn, name, c)?;
            if m.direction != "out" || m.kind != "text" {
                bail!("message {c} is not one of the bot's own cards");
            }
            // A card for a task parks the task: it leaves Decide too.
            Some(match m.flags["task"].as_i64() {
                Some(t) => format!("task:{t}"),
                None => format!("card:{c}"),
            })
        }
        (None, Some(t)) => {
            crate::db::get_task(conn, t)?;
            Some(format!("task:{t}"))
        }
        (None, None) => None,
    };
    if let Some(s) = &source {
        let open: Option<i64> = conn
            .query_row(
                "SELECT id FROM board_items WHERE bot_id=?1 AND state='open' AND source=?2",
                params![b, s],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = open {
            return get_item(conn, b, name, id);
        }
    }
    let reference = p
        .reference
        .as_deref()
        .filter(|r| !r.trim().is_empty())
        .map(|r| check_ref(conn, name, r))
        .transpose()?;
    let mut text = p.text.clone();
    if text.trim().is_empty() {
        text = match (p.card, p.task) {
            (Some(c), _) => card_title(&get_message(conn, name, c)?),
            (_, Some(t)) => crate::db::get_task(conn, t)?.title,
            _ => String::new(),
        };
    }
    let text = check_text(
        &cap(text.trim(), ITEM_CAP),
        reference.as_deref().or(source.as_deref()),
    )?;
    let item = insert(
        conn,
        actor,
        name,
        b,
        text,
        reference,
        REVIEW.into(),
        source,
        None,
        now,
        "park",
    )?;
    if let Some(c) = p.card {
        crate::channel::close_card(conn, c, "parked for review")?;
        crate::channel::set_flag(conn, c, "$.parked", &serde_json::json!(item.id))?;
    }
    Ok(item)
}

/// The first line of a card's text, plain.
fn card_title(m: &StoredMessage) -> String {
    let plain = if m.flags["html"] == true {
        strip_html(&m.text)
    } else {
        m.text.clone()
    };
    plain
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// The note of the wake that asks bot `name` for the doc of parked item `i`.
pub fn park_wake_note(i: &Item) -> String {
    format!(
        "The owner parked \"{}\" for review (board item {}). Write a short decision doc \
         (options, a recommendation) and attach it: axe bot board edit {} --ref doc:<path>.",
        i.text, i.id, i.id
    )
}

/// The bot's timezone: that of its first schedule, else UTC.
pub fn timezone(conn: &Connection, bot: i64) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT timezone FROM bot_schedules WHERE bot_id=?1 ORDER BY id LIMIT 1",
            [bot],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "UTC".into()))
}

/// The start of the day of `now` in `tz` (epoch seconds).
pub fn day_start(tz: &str, now: i64) -> i64 {
    crate::routine::latest_at("0 0 * * *", tz, now).unwrap_or(now - now.rem_euclid(86_400))
}

const VIEW_COLS: &str =
    "id, channel, chat_id, thread_id, pin, keyboard, repost, created_at, updated_at";

fn view_row(r: &rusqlite::Row) -> rusqlite::Result<View> {
    Ok(View {
        id: r.get(0)?,
        channel: r.get(1)?,
        chat_id: r.get(2)?,
        thread_id: r.get(3)?,
        pin: r.get(4)?,
        keyboard: r.get(5)?,
        repost: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

/// Bot `name`'s views, oldest first.
pub fn views(conn: &Connection, name: &str) -> Result<Vec<View>> {
    let b = bot_id(conn, name)?;
    Ok(conn
        .prepare(&format!(
            "SELECT {VIEW_COLS} FROM board_views WHERE bot_id=?1 ORDER BY id"
        ))?
        .query_map([b], view_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// The bot's home chat: its first view with the pin on. Fallback cards are
/// posted there, and the server asks for task cards only with one.
pub fn home(conn: &Connection, name: &str) -> Result<Option<View>> {
    Ok(views(conn, name)?.into_iter().find(|v| v.pin))
}

/// Creates or changes bot `name`'s view in `chat` (and `thread`, 0: none)
/// of `channel`. A new view has the pin and buttons on and reposts at
/// [`DEFAULT_REPOST`].
#[allow(clippy::too_many_arguments)]
pub fn set_view(
    conn: &mut Connection,
    actor: &Actor,
    name: &str,
    channel: &str,
    chat: i64,
    thread: i64,
    e: &ViewEdit,
    now: i64,
) -> Result<View> {
    if !CHANNELS.contains(&channel) {
        bail!(
            "unknown channel {channel:?} (expected {})",
            CHANNELS.join(", ")
        );
    }
    let b = bot_id(conn, name)?;
    let repost = match e.repost.as_deref().map(str::trim) {
        None => None,
        Some("off") | Some("") => Some(None),
        Some(c) => {
            crate::routine::parse_cron(c, &timezone(conn, b)?)?;
            Some(Some(c.to_string()))
        }
    };
    let at = bot::ts(now);
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO board_views (bot_id, channel, chat_id, thread_id, repost, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?6)",
        params![b, channel, chat, thread, DEFAULT_REPOST, at],
    )?;
    let id: i64 = tx.query_row(
        "SELECT id FROM board_views WHERE bot_id=?1 AND channel=?2 AND chat_id=?3 AND thread_id=?4",
        params![b, channel, chat, thread],
        |r| r.get(0),
    )?;
    if let Some(p) = e.pin {
        tx.execute("UPDATE board_views SET pin=?1 WHERE id=?2", params![p, id])?;
    }
    if let Some(k) = e.keyboard {
        tx.execute(
            "UPDATE board_views SET keyboard=?1 WHERE id=?2",
            params![k, id],
        )?;
    }
    if let Some(r) = repost {
        tx.execute(
            "UPDATE board_views SET repost=?1 WHERE id=?2",
            params![r, id],
        )?;
    }
    tx.execute(
        "UPDATE board_views SET updated_at=?1 WHERE id=?2",
        params![at, id],
    )?;
    let v = tx.query_row(
        &format!("SELECT {VIEW_COLS} FROM board_views WHERE id=?1"),
        [id],
        view_row,
    )?;
    bot::event(&tx, b, actor, "board_view", &serde_json::to_string(&v)?)?;
    tx.commit()?;
    Ok(v)
}

/// A bot with no view yet gets one (pin on) in the chat it first sends a
/// message to. Returns whether one was made.
pub fn default_view(
    conn: &mut Connection,
    name: &str,
    channel: &str,
    chat: i64,
    thread: i64,
    now: i64,
) -> Result<bool> {
    let b = bot_id(conn, name)?;
    let any: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM board_views WHERE bot_id=?1)",
        [b],
        |r| r.get(0),
    )?;
    if any {
        return Ok(false);
    }
    set_view(
        conn,
        &Actor::new("server"),
        name,
        channel,
        chat,
        thread,
        &ViewEdit::default(),
        now,
    )?;
    Ok(true)
}

/// Whether a board posted at `posted` is due its repost at `now` under
/// `view`'s cadence in timezone `tz`.
pub fn repost_due(view: &View, tz: &str, posted: i64, now: i64) -> bool {
    view.repost
        .as_deref()
        .and_then(|c| crate::routine::next_after(c, tz, posted).ok())
        .is_some_and(|t| t <= now)
}

/// A link for ref `r` and what it is (`task #12`, `goal`, `doc`, `file`,
/// `link`); a card has none.
fn link_of(conn: &Connection, r: &str, links: &Links) -> Result<(Option<String>, Option<String>)> {
    let web = links.web.map(|w| w.trim_end_matches('/'));
    let (kind, rest) = r.split_once(':').unwrap_or((r, ""));
    Ok(match kind {
        "task" => (
            web.map(|w| format!("{w}/?task={rest}")),
            Some(format!("task #{rest}")),
        ),
        "goal" => {
            let name = rest
                .parse()
                .ok()
                .map(|n| crate::goal::name_of(conn, n))
                .transpose()?
                .flatten();
            (
                web.zip(name).map(|(w, n)| goal_board::goal_url(w, &n)),
                Some("goal".into()),
            )
        }
        "doc" => (
            web.map(|w| format!("{w}/doc?path={}", crate::channel::pct_encode(rest))),
            Some("doc".into()),
        ),
        "blob" => ((links.blob)(rest), Some("file".into())),
        "http" | "https" => (Some(r.to_string()), Some("link".into())),
        _ => (None, None),
    })
}

/// One goal's line detail: status, cost and the next task.
fn goal_detail(g: &GoalLine) -> String {
    let mut parts = vec![g.status.clone().unwrap_or_else(|| g.state.clone())];
    match g.cost_week {
        Some(l) if l > 0.0 => parts.push(format!(
            "{} {:.0}% ${:.0}/${:.0} wk",
            goal_board::cells(g.cost_spent / l),
            100.0 * g.cost_spent / l,
            g.cost_spent,
            l
        )),
        _ => parts.push(format!("${:.2} spent", g.cost_spent)),
    }
    if let Some(n) = &g.next {
        parts.push(format!("next: {} #{}", n.title, n.id));
    }
    if g.needs_you > 0 {
        parts.push(format!("{} need you", g.needs_you));
    }
    parts.join(" · ")
}

/// A task's state, title and when it entered that state (epoch seconds).
fn task_facts(conn: &Connection, id: i64) -> Result<Option<(String, String, i64)>> {
    Ok(conn
        .query_row(
            "SELECT state, title, updated_at FROM tasks WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(s, t, u)| (s, t, epoch_of(&u).unwrap_or(0))))
}

/// Bot `name`'s board at `now`: Decide, Review, Goals, the named item
/// sections, Items, then items done since `done_since`. Empty sections are
/// left out. See the module doc for what each holds and how items merge.
pub fn board(
    conn: &Connection,
    name: &str,
    done_since: i64,
    now: i64,
    links: &Links,
) -> Result<Board> {
    let b = bot::get(conn, name)?;
    let mut decide = lines(conn, name)?;
    let goals = goal_board::pinned_lines(conn, name, links.web)?;
    let items = items_of(conn, b.id, Some(done_since))?;
    let open_items = items.iter().filter(|i| i.state == "open").count();
    let open = |i: &&Item| i.state == "open";
    // Parked tasks and cards leave Decide.
    let parked: Vec<&str> = items
        .iter()
        .filter(open)
        .filter(|i| i.section == REVIEW)
        .filter_map(|i| i.source.as_deref())
        .collect();
    decide.retain(|l| {
        let me = format!("{}:{}", l.kind, l.id);
        let task = l.task.map(|t| format!("task:{t}"));
        !parked
            .iter()
            .any(|p| *p == me || Some(*p) == task.as_deref())
    });
    let mut decide: Vec<BoardLine> = decide
        .into_iter()
        .map(|l| {
            let task = if l.kind == "task" { Some(l.id) } else { l.task };
            let (link, label) = match task {
                Some(t) => link_of(conn, &format!("task:{t}"), links).unwrap_or((None, None)),
                None => (None, None),
            };
            BoardLine {
                kind: l.kind.into(),
                id: l.id,
                text: l.title,
                note: None,
                item: None,
                task,
                reference: None,
                link,
                link_label: label,
                since: l.since,
                state: "open".into(),
                overdue: false,
                detail: None,
            }
        })
        .collect();
    let mut goal_lines: Vec<BoardLine> = goals
        .iter()
        .map(|g| BoardLine {
            kind: "goal".into(),
            id: g.id,
            text: g.name.clone(),
            note: None,
            item: None,
            task: None,
            reference: None,
            link: g.url.clone(),
            link_label: Some("goal".into()),
            since: 0,
            state: "open".into(),
            overdue: false,
            detail: Some(goal_detail(g)),
        })
        .collect();
    let mut review = Vec::new();
    let mut named: Vec<(String, Vec<BoardLine>)> = Vec::new();
    let mut plain = Vec::new();
    let mut done = Vec::new();
    for i in &items {
        let r = i.reference.as_deref();
        let (kind, rest) = r.and_then(|r| r.split_once(':')).unwrap_or(("", ""));
        let rid: Option<i64> = rest.parse().ok();
        // An open item on a derived line is that line's note.
        if i.state == "open" && i.section != REVIEW {
            let target = match kind {
                "task" => decide.iter_mut().find(|l| l.task == rid),
                "card" => decide
                    .iter_mut()
                    .find(|l| l.kind == "card" && Some(l.id) == rid),
                "goal" => goal_lines.iter_mut().find(|l| Some(l.id) == rid),
                _ => None,
            };
            if let Some(l) = target {
                if !i.text.is_empty() {
                    l.note = Some(i.text.clone());
                }
                l.item = Some(i.id);
                continue;
            }
        }
        // Its title and state from the source; a closed ref closes it.
        let mut text = i.text.clone();
        let mut state = i.state.clone();
        let mut closed_at = i.closed_at.as_deref().and_then(epoch_of);
        let mut task = None;
        match (kind, rid) {
            ("task", Some(t)) => {
                task = Some(t);
                if let Some((st, title, at)) = task_facts(conn, t)? {
                    if text.is_empty() {
                        text = short(&title);
                    }
                    if st == "done" && state == "open" {
                        state = "done".into();
                        closed_at = Some(at);
                    }
                }
            }
            ("goal", Some(g)) => {
                if let Ok(Some(n)) = crate::goal::name_of(conn, g) {
                    let gl = crate::goal::get(conn, &n)?;
                    if text.is_empty() {
                        text = n;
                    }
                    if matches!(gl.state.as_str(), "done" | "dropped") && state == "open" {
                        state = "done".into();
                        closed_at = epoch_of(&gl.updated_at);
                    }
                }
            }
            ("card", Some(c)) if text.is_empty() => {
                text = get_message(conn, name, c).map(|m| short(&card_title(&m)))?;
            }
            _ => {}
        }
        if state == "dropped" {
            continue;
        }
        if state == "done" && closed_at.is_none_or(|t| t < done_since) {
            continue;
        }
        let (link, link_label) = match r {
            Some(r) => link_of(conn, r, links)?,
            None => (None, None),
        };
        let line = BoardLine {
            kind: "item".into(),
            id: i.id,
            text,
            note: None,
            item: Some(i.id),
            task,
            reference: i.reference.clone(),
            link,
            link_label,
            since: epoch_of(&i.created_at).unwrap_or(0),
            state: state.clone(),
            overdue: state == "open"
                && i.until
                    .as_deref()
                    .and_then(epoch_of)
                    .is_some_and(|u| u < now),
            detail: None,
        };
        if state == "done" {
            done.push(line);
        } else if i.section == REVIEW {
            review.push(line);
        } else if i.section.is_empty() {
            plain.push(line);
        } else {
            match named.iter_mut().find(|(s, _)| *s == i.section) {
                Some((_, ls)) => ls.push(line),
                None => named.push((i.section.clone(), vec![line])),
            }
        }
    }
    let mut sections = Vec::new();
    let mut push = |key: &str, title: &str, lines: Vec<BoardLine>| {
        if !lines.is_empty() {
            sections.push(Section {
                key: key.into(),
                title: title.into(),
                lines,
            });
        }
    };
    let n_decide = decide.len();
    push("decide", "Decide", decide);
    push("review", REVIEW, review);
    push("goals", "Goals", goal_lines);
    for (title, lines) in named {
        push("items", &title, lines);
    }
    push("items", "Items", plain);
    push("done", "Done today", done);
    Ok(Board {
        bot: b.name,
        sections,
        decide: n_decide,
        open_items,
        done_since,
        views: views(conn, name)?,
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
pub(crate) fn cap(s: &str, limit: usize) -> String {
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
