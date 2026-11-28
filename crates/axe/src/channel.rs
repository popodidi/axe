//! The bot message channel: stored messages and the limits and helpers that
//! the CLI and the server use with or without a channel gateway. Today's one
//! gateway is Telegram (`telegram.rs`, cargo feature `telegram`); messages are
//! stored with `channel = 'telegram'`.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bot;

/// Multipart uploads: 10 MB for photos, 50 MB for other files.
pub const PHOTO_UPLOAD_LIMIT: u64 = 10 * 1024 * 1024;
pub const UPLOAD_LIMIT: u64 = 50 * 1024 * 1024;
/// `callback_data` is 1-64 bytes.
pub const CALLBACK_DATA_LIMIT: usize = 64;

/// Options of `axe bot tg send`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SendOpts {
    /// Target chat; default: the chat of `reply_to`, else the latest inbound
    /// chat, else the first allowlisted chat.
    pub chat: Option<i64>,
    /// Local message id to reply to.
    pub reply_to: Option<i64>,
    pub thread: Option<i64>,
    #[serde(default)]
    pub html: bool,
    /// Inline keyboard buttons: (label, callback data).
    #[serde(default)]
    pub buttons: Vec<(String, String)>,
    /// Buttons per keyboard row, in order; empty puts every button on one row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<usize>,
    #[serde(default)]
    pub silent: bool,
    /// The escalated task this card stands for: the card is its pinned
    /// inbox item and closes when the task leaves escalated. Needs buttons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<i64>,
    /// Server side only: a weekly retro card's `flags.retro`, recorded with
    /// the message so it is never taken for an open card (`board.rs`).
    #[serde(skip)]
    pub retro: Option<Value>,
}

/// One stored message, as `bot tg history` returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub direction: String,
    pub chat_id: i64,
    pub thread_id: Option<i64>,
    pub message_id: i64,
    pub reply_to: Option<i64>,
    pub edit_of: Option<i64>,
    pub from_id: Option<i64>,
    pub kind: String,
    pub text: String,
    pub attachments: Value,
    pub media_group_id: Option<String>,
    pub flags: Value,
    pub at: String,
}

const MSG_COLS: &str = "m.id, m.direction, m.chat_id, m.thread_id, m.message_id, m.reply_to,
 m.edit_of, m.from_id, m.kind, m.text, m.attachments, m.media_group_id, m.flags, m.at";

fn msg_row(r: &rusqlite::Row) -> rusqlite::Result<StoredMessage> {
    let parse = |s: String| serde_json::from_str(&s).unwrap_or(Value::Null);
    Ok(StoredMessage {
        id: r.get(0)?,
        direction: r.get(1)?,
        chat_id: r.get(2)?,
        thread_id: r.get(3)?,
        message_id: r.get(4)?,
        reply_to: r.get(5)?,
        edit_of: r.get(6)?,
        from_id: r.get(7)?,
        kind: r.get(8)?,
        text: r.get(9)?,
        attachments: parse(r.get(10)?),
        media_group_id: r.get(11)?,
        flags: parse(r.get(12)?),
        at: r.get(13)?,
    })
}

/// Kinds whose `message_id` is a Telegram update id, not a message.
const SYNTHETIC: &str = "('edit','button','reaction')";

/// Local id of Telegram message `message_id` in `chat`.
pub fn local_id(conn: &Connection, bot: i64, chat: i64, message_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT id FROM bot_messages WHERE bot_id=?1 AND chat_id=?2 AND message_id=?3
                 AND kind NOT IN {SYNTHETIC} ORDER BY id LIMIT 1"
            ),
            params![bot, chat, message_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Whether a Telegram row (`message_id`, `kind`) of `chat` is stored: a
/// redelivered update.
pub fn is_recorded(
    conn: &Connection,
    bot: i64,
    chat: i64,
    message_id: i64,
    kind: &str,
) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM bot_messages WHERE bot_id=?1 AND channel='telegram' AND chat_id=?2
               AND message_id=?3 AND kind=?4",
            params![bot, chat, message_id, kind],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The chat of bot `bot`'s latest inbound message.
pub fn last_inbound_chat(conn: &Connection, bot: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT chat_id FROM bot_messages WHERE bot_id=?1 AND direction='in' ORDER BY id DESC LIMIT 1",
            [bot],
            |r| r.get(0),
        )
        .optional()?)
}

/// Sets flag `path` of message `msg` to `v`.
pub fn set_flag(conn: &Connection, msg: i64, path: &str, v: &Value) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_set(flags,?1,json(?2)) WHERE id=?3",
        params![path, v.to_string(), msg],
    )?;
    Ok(())
}

/// Records the press that answered message `msg`'s keyboard.
pub fn mark_answered(conn: &Connection, msg: i64, answer: &Value) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_set(flags,'$.answered',json(?1)) WHERE id=?2",
        params![answer.to_string(), msg],
    )?;
    Ok(())
}

/// Marks message `msg` as the runtime card of `runtime`.
pub fn mark_runtime_alert(conn: &Connection, msg: i64, runtime: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_set(flags,'$.runtime_alert',json('true'),'$.runtime',?1) WHERE id=?2",
        params![runtime, msg],
    )?;
    Ok(())
}

/// Records an edit of message `msg`: its new text (without `buttons`,
/// Telegram drops the keyboard, so the card closes) and its new keyboard (a
/// new keyboard takes fresh presses; `html` is its parse mode).
pub fn record_edit(
    conn: &Connection,
    msg: i64,
    text: Option<&str>,
    buttons: Option<&[(String, String)]>,
    html: bool,
) -> Result<()> {
    if let Some(t) = text {
        conn.execute(
            // A hand edit of the board pin is rendered over on the next pass.
            "UPDATE bot_messages SET text=?1, flags=json_remove(json_set(flags,'$.edited',json('true')),'$.board_key') WHERE id=?2",
            params![t, msg],
        )?;
        if buttons.is_none() {
            conn.execute(
                "UPDATE bot_messages SET flags=json_remove(flags,'$.buttons') WHERE id=?1",
                [msg],
            )?;
        }
    }
    if let Some(bs) = buttons {
        conn.execute(
            "UPDATE bot_messages SET flags=json_remove(json_set(flags,'$.buttons',json(?1),'$.html',json(?2)),'$.answered') WHERE id=?3",
            params![serde_json::json!(bs).to_string(), serde_json::json!(html).to_string(), msg],
        )?;
        if !bs.is_empty() {
            conn.execute(
                "UPDATE bot_messages SET flags=json_set(flags,'$.ask',json('true')) WHERE id=?1",
                [msg],
            )?;
        }
    }
    Ok(())
}

/// Records a pin or unpin of message `msg`.
pub fn record_pin(conn: &Connection, msg: i64, pin: bool) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_set(flags,'$.pinned',json(?1)) WHERE id=?2",
        params![serde_json::json!(pin).to_string(), msg],
    )?;
    Ok(())
}

/// Marks message `msg` deleted; the row stays (history is never deleted).
pub fn mark_deleted(conn: &Connection, msg: i64) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_set(flags,'$.deleted',json('true')) WHERE id=?1",
        [msg],
    )?;
    Ok(())
}

/// Records the server's edit of message `msg` to `text`.
pub fn set_text_edited(conn: &Connection, msg: i64, text: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.edited',json('true')) WHERE id=?2",
        params![text, msg],
    )?;
    Ok(())
}

/// Closes card `msg`: no keyboard, `flags.closed` says why.
pub fn close_card(conn: &Connection, msg: i64, reason: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET flags=json_set(json_remove(flags,'$.buttons'),'$.closed',?1) WHERE id=?2",
        params![reason, msg],
    )?;
    Ok(())
}

/// Closes card `msg` with `text` if it still has a keyboard; 0 when another
/// pass closed it first.
pub fn claim_close(conn: &Connection, msg: i64, text: &str, reason: &str) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE bot_messages SET text=?1, flags=json_set(json_remove(flags,'$.buttons'),'$.closed',?2)
         WHERE id=?3 AND json_type(flags,'$.buttons')='array'",
        params![text, reason, msg],
    )?)
}

/// Keys of the retired pins: card pins, goal pins, the overview and the
/// goal board (the board replaced them, `board.rs`).
const LEGACY_PIN_KEYS: [&str; 9] = [
    "card_pin",
    "goal_pin",
    "goal_pin_key",
    "board",
    "board_from",
    "board_buttons",
    "board_retired",
    "goal_board",
    "goal_board_key",
];

/// Bot `name`'s messages that still carry a retired pin's flags, oldest
/// first: the gateway unpins each once and clears them ([`clear_legacy_pin`]).
pub fn legacy_pins(conn: &Connection, name: &str) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let cond = LEGACY_PIN_KEYS
        .iter()
        .map(|k| format!("json_type(flags,'$.{k}') IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let ids: Vec<i64> = conn
        .prepare(&format!(
            "SELECT id FROM bot_messages WHERE bot_id=?1 AND ({cond}) ORDER BY id"
        ))?
        .query_map([b], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| get_message(conn, name, id))
        .collect()
}

/// Message `msg` no longer carries a retired pin's flags; `unpinned`
/// records that the gateway unpinned it.
pub fn clear_legacy_pin(conn: &Connection, msg: i64, unpinned: bool) -> Result<()> {
    let paths = LEGACY_PIN_KEYS
        .iter()
        .map(|k| format!("'$.{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let pinned = if unpinned {
        ",'$.pinned',json('false')"
    } else {
        ""
    };
    conn.execute(
        &format!(
            "UPDATE bot_messages SET flags=json_set(json_remove(flags,{paths},'$.board_key'),'$.legacy_pin',json('true'){pinned}) WHERE id=?1"
        ),
        [msg],
    )?;
    Ok(())
}

/// Bot `name`'s board pins (`flags.board_view`, the view's id), oldest
/// first.
pub fn board_pins(conn: &Connection, name: &str) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM bot_messages WHERE bot_id=?1
               AND json_type(flags,'$.board_view')='integer' ORDER BY id",
        )?
        .query_map([b], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| get_message(conn, name, id))
        .collect()
}

/// Board pin `msg` shows `text` now (`key` is its text and keyboard).
pub fn set_board_text(conn: &Connection, msg: i64, text: &str, key: &str) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.board_key',?2,'$.edited',json('true')) WHERE id=?3",
        params![text, key, msg],
    )?;
    Ok(())
}

/// Message `msg` is no longer a board pin (unpinned, or moved to a newer
/// one); `text` is what it was edited to, if anything.
pub fn drop_board_pin(conn: &Connection, msg: i64, text: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE bot_messages SET text=COALESCE(?1, text),
           flags=json_set(json_remove(flags,'$.board_view','$.board_key','$.board_menu','$.board_posted'),'$.pinned',json('false'))
         WHERE id=?2",
        params![text, msg],
    )?;
    Ok(())
}

/// Cards of bot `name` parked for review whose Telegram keyboard is still
/// shown (`flags.parked` without `flags.parked_cleared`).
pub fn parked_cards(conn: &Connection, name: &str) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM bot_messages WHERE bot_id=?1 AND json_type(flags,'$.parked') IS NOT NULL
               AND json_type(flags,'$.parked_cleared') IS NULL ORDER BY id",
        )?
        .query_map([b], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| get_message(conn, name, id))
        .collect()
}

/// Bot `name`'s runtime cards that still have buttons, oldest first.
pub fn open_runtime_cards(conn: &Connection, name: &str) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM bot_messages WHERE bot_id=?1 AND direction='out'
               AND json_extract(flags,'$.runtime_alert')=1
               AND json_type(flags,'$.buttons')='array' AND json_array_length(flags,'$.buttons')>0
               AND json_type(flags,'$.deleted') IS NULL ORDER BY id",
        )?
        .query_map([b], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| get_message(conn, name, id))
        .collect()
}

/// Bot `name`'s voice acks (`flags.voice_ack`, the voice message's id) that
/// are due their edit: the voice message has a transcript, or a run it woke
/// started without one. Each comes with its voice message, oldest first.
pub fn due_voice_acks(
    conn: &Connection,
    name: &str,
) -> Result<Vec<(StoredMessage, StoredMessage)>> {
    let b = bot_id(conn, name)?;
    let ids: Vec<(i64, i64)> = conn
        .prepare(
            "SELECT a.id, v.id FROM bot_messages a
             JOIN bot_messages v ON v.id=json_extract(a.flags,'$.voice_ack') AND v.bot_id=a.bot_id
             WHERE a.bot_id=?1 AND a.direction='out'
               AND json_type(a.flags,'$.voice_ack_done') IS NULL
               AND json_type(a.flags,'$.deleted') IS NULL
               AND (json_type(v.flags,'$.transcript') IS NOT NULL
                 OR EXISTS (SELECT 1 FROM bot_wakes w JOIN bot_runs r ON r.id=w.run_id
                            WHERE w.message_ref=v.id AND r.log_path<>''))
             ORDER BY a.id",
        )?
        .query_map([b], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|(a, v)| Ok((get_message(conn, name, a)?, get_message(conn, name, v)?)))
        .collect()
}

/// Voice ack `msg` is settled: edited to `text` when given (else Telegram
/// refused the edit), and never edited again.
pub fn settle_voice_ack(conn: &Connection, msg: i64, text: Option<&str>) -> Result<()> {
    if let Some(t) = text {
        set_text_edited(conn, msg, t)?;
    }
    set_flag(conn, msg, "$.voice_ack_done", &Value::Bool(true))
}

/// (chat, thread) of the messages whose wakes the bot's live run took.
pub fn live_chats(conn: &Connection, name: &str) -> Result<Vec<(i64, Option<i64>)>> {
    Ok(conn
        .prepare(
            "SELECT DISTINCT m.chat_id, m.thread_id FROM bots b
             JOIN bot_runs r ON r.bot_id=b.id AND r.ended_at IS NULL
             JOIN bot_wakes w ON w.run_id=r.id
             JOIN bot_messages m ON m.id=w.message_ref
             WHERE b.name=?1 AND m.direction='in'",
        )?
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn bot_id(conn: &Connection, name: &str) -> Result<i64> {
    Ok(bot::get(conn, name)?.id)
}

pub fn get_message(conn: &Connection, name: &str, id: i64) -> Result<StoredMessage> {
    conn.query_row(
        &format!(
            "SELECT {MSG_COLS} FROM bot_messages m JOIN bots b ON b.id=m.bot_id
             WHERE b.name=?1 AND m.id=?2"
        ),
        params![name, id],
        msg_row,
    )
    .optional()?
    .with_context(|| format!("message {id} of bot {name} not found"))
}

/// `bot tg history`: the latest `limit` messages (both directions), oldest first.
pub fn history(
    conn: &Connection,
    name: &str,
    limit: i64,
    before: Option<i64>,
    search: Option<&str>,
) -> Result<Vec<StoredMessage>> {
    let b = bot_id(conn, name)?;
    let mut rows: Vec<StoredMessage> = conn
        .prepare(&format!(
            "SELECT {MSG_COLS} FROM bot_messages m WHERE m.bot_id=?1
               AND (?2 IS NULL OR m.id < ?2)
               AND (?3 IS NULL OR instr(lower(m.text), lower(?3)) > 0)
             ORDER BY m.id DESC LIMIT ?4"
        ))?
        .query_map(params![b, before, search, limit], msg_row)?
        .collect::<rusqlite::Result<_>>()?;
    rows.reverse();
    Ok(rows)
}

/// Refuses uploads over the channel's multipart limits (Telegram's).
pub fn check_upload(kind: &str, size: u64) -> Result<()> {
    let limit = match kind {
        "photo" => PHOTO_UPLOAD_LIMIT,
        "document" | "voice" => UPLOAD_LIMIT,
        _ => bail!("--as must be photo, document or voice"),
    };
    if size > limit {
        bail!(
            "{kind} is {:.1} MB; Telegram bots can upload at most {} MB as a {kind}",
            size as f64 / 1_048_576.0,
            limit / 1_048_576
        );
    }
    if size == 0 {
        bail!("file is empty");
    }
    Ok(())
}

/// A file name safe to create in a directory: no separators, no leading dot.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base
        .chars()
        .map(|c| if c.is_control() || c == '"' { '_' } else { c })
        .collect();
    let s = s.trim_start_matches('.').trim().to_string();
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

/// Percent-encodes everything but unreserved ASCII (for a header value).
pub fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Validates inline buttons: (label, callback data), and `rows` (buttons
/// per row; empty is one row), which must be non-empty and cover every button.
pub fn check_buttons(buttons: &[(String, String)], rows: &[usize]) -> Result<()> {
    if !rows.is_empty() {
        if let Some(i) = rows.iter().position(|&n| n == 0) {
            bail!("button row {} is empty", i + 1);
        }
        let total: usize = rows.iter().sum();
        if total != buttons.len() {
            bail!(
                "button rows cover {total} buttons but {} were given",
                buttons.len()
            );
        }
    }
    for (label, data) in buttons {
        if label.trim().is_empty() || data.is_empty() || data.len() > CALLBACK_DATA_LIMIT {
            bail!(
                "button {label:?}: label must be non-empty and data 1-{CALLBACK_DATA_LIMIT} bytes"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod button_tests {
    use super::check_buttons;

    #[test]
    fn check_buttons_requires_rows_to_cover_every_button() {
        let bs = vec![("A".to_string(), "a".to_string()), ("B".into(), "b".into())];
        check_buttons(&bs, &[]).unwrap();
        check_buttons(&bs, &[2]).unwrap();
        check_buttons(&bs, &[1, 1]).unwrap();
        for (rows, err) in [
            (&[1, 0, 1][..], "row 2 is empty"),
            (&[1], "cover 1 buttons but 2"),
            (&[2, 1], "cover 3 buttons but 2"),
        ] {
            let e = check_buttons(&bs, rows).unwrap_err().to_string();
            assert!(e.contains(err), "{rows:?}: {e}");
        }
        assert!(check_buttons(&[("A".into(), String::new())], &[]).is_err());
    }
}
