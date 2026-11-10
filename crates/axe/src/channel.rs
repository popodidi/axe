//! The bot message channel: stored messages and the limits and helpers that
//! the CLI and the server use with or without a channel gateway. Today's one
//! gateway is Telegram (`telegram.rs`, cargo feature `telegram`); messages are
//! stored with `channel = 'telegram'`.

use std::sync::MutexGuard;

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

/// What a channel gateway needs from the server.
pub trait Store: Send + Sync + 'static {
    fn conn(&self) -> MutexGuard<'_, Connection>;
    /// A database change was committed (wakes SSE clients).
    fn changed(&self);
    fn stopping(&self) -> bool;
}

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
    #[serde(default)]
    pub silent: bool,
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

/// Validates inline buttons: (label, callback data).
pub fn check_buttons(buttons: &[(String, String)]) -> Result<()> {
    for (label, data) in buttons {
        if label.trim().is_empty() || data.is_empty() || data.len() > CALLBACK_DATA_LIMIT {
            bail!(
                "button {label:?}: label must be non-empty and data 1-{CALLBACK_DATA_LIMIT} bytes"
            );
        }
    }
    Ok(())
}
