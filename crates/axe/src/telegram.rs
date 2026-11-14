//! Telegram gateway inside `axe server`: one long-poll thread per configured
//! bot token, the inbound security filter, owner commands, and the outbound
//! Bot API calls behind `axe bot tg`. Tokens stay in this process; agents
//! talk to Telegram only through the server. Design:
//! `docs/design/bots.md` ("Telegram and security", "Wake delivery").
//!
//! Every method and field used here was checked against
//! <https://core.telegram.org/bots/api> (Bot API 10.3).
//!
//! Stored inbound rows use the Telegram `message_id` for messages. Rows that
//! are not a Telegram message of their own (kinds `edit`, `button`,
//! `reaction`) store the `update_id` instead, so each one is unique and a
//! redelivered update dedupes; they point at their target through `edit_of` or
//! `reply_to` (local ids).
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::board;
use crate::bot::{self, NewMessage};
use crate::config;
use crate::db::{Actor, Forbidden};
use crate::goal_board;

use crate::channel::bot_id;
pub use crate::channel::{
    check_buttons, check_upload, get_message, history, pct_encode, safe_name, SendOpts, Store,
    StoredMessage, CALLBACK_DATA_LIMIT, PHOTO_UPLOAD_LIMIT, UPLOAD_LIMIT,
};

/// Update types requested explicitly: the default excludes `message_reaction`.
pub const ALLOWED_UPDATES: [&str; 5] = [
    "message",
    "edited_message",
    "callback_query",
    "my_chat_member",
    "message_reaction",
];
/// `sendMessage` text limit, counted in UTF-16 units (never fewer than characters).
pub const TEXT_LIMIT: usize = 4096;
/// `getFile` downloads files up to 20 MB.
pub const DOWNLOAD_LIMIT: u64 = 20 * 1024 * 1024;
/// `answerCallbackQuery` text is 0-200 characters.
const TOAST_LIMIT: usize = 200;
/// `typing` lasts 5 s or less; a live run's chats get it again this often.
const TYPING_REFRESH: Duration = Duration::from_secs(4);
/// Reaction set on every accepted owner message.
pub const ACK_EMOJI: &str = "👀";
/// About one message per second per chat.
const SEND_INTERVAL: Duration = Duration::from_secs(1);
const SEND_ATTEMPTS: usize = 3;
/// Most messages one `send` may split into (sends are about 1 s apart).
pub const MAX_PARTS: usize = 20;
const MAX_RETRY_AFTER: u64 = 60;
/// At most one security alert to the owner per interval.
const ALERT_INTERVAL: Duration = Duration::from_secs(60);
/// Attempts at one update before it is skipped (recorded as an event).
const UPDATE_ATTEMPTS: u32 = 3;
const OWNER_COMMANDS: [&str; 6] = ["status", "stop", "pause", "resume", "reset", "wake"];
/// First wait after a failed board edit; it doubles up to [`BOARD_RETRY_MAX`].
const BOARD_RETRY: Duration = Duration::from_secs(5);
const BOARD_RETRY_MAX: Duration = Duration::from_secs(300);
/// A board pass also runs this often with no change signal, for changes that
/// did not come through the API.
const BOARD_RESYNC: Duration = Duration::from_secs(60);
/// At most one board pass this often, however busy the database is.
const BOARD_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// A failed Bot API call. Never contains the token.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub method: String,
    pub code: Option<i64>,
    pub description: String,
    pub retry_after: Option<u64>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "telegram {}: {c} {}", self.method, self.description),
            None => write!(f, "telegram {}: {}", self.method, self.description),
        }
    }
}

impl std::error::Error for ApiError {}

/// Bot API client for one token.
pub struct Api {
    base: String,
    token: String,
    agent: ureq::Agent,
    poll_agent: ureq::Agent,
}

impl Api {
    pub fn new(base: &str, token: &str, poll_timeout: u64) -> Api {
        let agent = |secs| {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout(Duration::from_secs(secs))
                .build()
        };
        Api {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            agent: agent(120),
            poll_agent: agent(poll_timeout + 15),
        }
    }

    fn clean(&self, s: &str) -> String {
        s.replace(&self.token, "<token>")
    }

    fn finish(
        &self,
        method: &str,
        r: std::result::Result<ureq::Response, ureq::Error>,
    ) -> Result<Value, ApiError> {
        let err = |code, description: String, retry_after| ApiError {
            method: method.to_string(),
            code,
            description,
            retry_after,
        };
        let body: Value = match r {
            Ok(resp) => resp.into_json().map_err(|e| {
                err(
                    None,
                    format!("bad response: {}", self.clean(&e.to_string())),
                    None,
                )
            })?,
            Err(ureq::Error::Status(code, resp)) => resp
                .into_json()
                .unwrap_or_else(|_| json!({"ok":false,"error_code":code})),
            Err(ureq::Error::Transport(t)) => {
                return Err(err(None, self.clean(&t.to_string()), None));
            }
        };
        if body["ok"] == true {
            return Ok(body["result"].clone());
        }
        Err(err(
            body["error_code"].as_i64(),
            self.clean(body["description"].as_str().unwrap_or("request failed")),
            body["parameters"]["retry_after"].as_u64(),
        ))
    }

    /// Calls a method with a JSON body.
    pub fn call(&self, method: &str, params: &Value) -> Result<Value, ApiError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        let agent = if method == "getUpdates" {
            &self.poll_agent
        } else {
            &self.agent
        };
        self.finish(method, agent.post(&url).send_json(params.clone()))
    }

    /// Uploads one file with multipart/form-data.
    pub fn upload(
        &self,
        method: &str,
        fields: &[(&str, String)],
        file_field: &str,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<Value, ApiError> {
        let boundary = format!("axe-{}", uuid::Uuid::new_v4().simple());
        let mut body = Vec::with_capacity(bytes.len() + 1024);
        for (k, v) in fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
                )
                .as_bytes(),
            );
        }
        let safe: String = file_name
            .chars()
            .map(|c| if c == '"' || c.is_control() { '_' } else { c })
            .collect();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{safe}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        self.finish(
            method,
            self.agent
                .post(&url)
                .set(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={boundary}"),
                )
                .send_bytes(&body),
        )
    }

    /// Downloads `file_path` from `getFile`, refusing more than `limit` bytes.
    pub fn download(&self, file_path: &str, limit: u64) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}/file/bot{}/{file_path}", self.base, self.token);
        let err = |d: String| ApiError {
            method: "download".into(),
            code: None,
            description: d,
            retry_after: None,
        };
        let resp = match self.agent.get(&url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(err(format!("HTTP {code}"))),
            Err(e) => return Err(err(self.clean(&e.to_string()))),
        };
        let mut out = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(resp.into_reader(), limit + 1),
            &mut out,
        )
        .map_err(|e| err(self.clean(&e.to_string())))?;
        if out.len() as u64 > limit {
            return Err(err(format!("file is larger than {limit} bytes")));
        }
        Ok(out)
    }
}

/// One configured Telegram bot.
pub struct TgBot {
    pub name: String,
    pub chats: Vec<i64>,
    api: Api,
    /// Per-chat outbound pacing: the earliest next send.
    queues: Mutex<HashMap<i64, Arc<Mutex<Instant>>>>,
    alerts: Mutex<Alerts>,
    /// `getMe` username, for `/cmd@username`.
    username: Mutex<Option<String>>,
    /// `getMe` user id, to recognise the bot's own service messages.
    id: Mutex<Option<i64>>,
}

#[derive(Default)]
struct Alerts {
    sources: HashSet<String>,
    last: Option<Instant>,
}

/// The gateway: all configured bots.
pub struct Gateway {
    owner: i64,
    poll_timeout: u64,
    bots: HashMap<String, Arc<TgBot>>,
}

/// Kinds whose `message_id` is a Telegram update id, not a message.
const SYNTHETIC: &str = "('edit','button','reaction')";

/// Local id of Telegram message `message_id` in `chat`.
fn local_id(conn: &Connection, bot: i64, chat: i64, message_id: i64) -> Result<Option<i64>> {
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

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Splits text into parts of at most `limit` UTF-16 units, at line boundaries
/// where possible (a longer line is split inside). Blank parts are dropped.
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut len = 0;
    let mut flush = |cur: &mut String, len: &mut usize| {
        let part = cur.trim_end_matches('\n');
        if !part.trim().is_empty() {
            parts.push(part.to_string());
        }
        cur.clear();
        *len = 0;
    };
    for line in text.split_inclusive('\n') {
        let l = utf16_len(line);
        if len + l > limit && !cur.is_empty() {
            flush(&mut cur, &mut len);
        }
        if l > limit {
            for ch in line.chars() {
                if len + ch.len_utf16() > limit {
                    flush(&mut cur, &mut len);
                }
                cur.push(ch);
                len += ch.len_utf16();
            }
        } else {
            cur.push_str(line);
            len += l;
        }
    }
    flush(&mut cur, &mut len);
    parts
}

/// Attachments of an inbound message: `[{type,file_id,file_unique_id,name,mime,size}]`.
fn attachments(m: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut push = |kind: &str, f: &Value, name: Option<&str>| {
        if f["file_id"].is_string() {
            out.push(json!({
                "type": kind, "file_id": f["file_id"], "file_unique_id": f["file_unique_id"],
                "name": name.or(f["file_name"].as_str()), "mime": f["mime_type"],
                "size": f["file_size"],
            }));
        }
    };
    // `photo` is an array of sizes; keep the largest (the last).
    if let Some(p) = m["photo"].as_array().and_then(|a| a.last()) {
        push("photo", p, Some("photo.jpg"));
    }
    for kind in ["document", "audio", "voice", "video"] {
        if m[kind].is_object() {
            let default = match kind {
                "voice" => Some("voice.ogg"),
                _ => None,
            };
            push(kind, &m[kind], default);
        }
    }
    out
}

/// Whether `m` is the "pinned a message" service message Telegram creates
/// when this bot (user id `me`) pins a message itself: nothing to store or
/// alert. Unknown `me` (failed `getMe`) never matches, so it is still dropped.
fn own_pin(me: Option<i64>, m: &Value) -> bool {
    me.is_some()
        && m["pinned_message"].is_object()
        && m["from"]["id"].as_i64() == me
        && m["from"]["is_bot"] == true
}

/// Why an inbound sender is refused, or `None` when it is the owner in an
/// allowlisted chat.
fn refuse(
    owner: i64,
    chats: &[i64],
    from: &Value,
    m: &Value,
    chat: Option<i64>,
) -> Option<&'static str> {
    if from["id"].as_i64() != Some(owner) {
        return Some("sender is not the owner");
    }
    if from["is_bot"] != false {
        return Some("sender is a bot");
    }
    if !m["sender_chat"].is_null() || !m["actor_chat"].is_null() {
        return Some("sent on behalf of a chat");
    }
    if !m["via_bot"].is_null() {
        return Some("sent via a bot");
    }
    match chat {
        Some(c) if chats.contains(&c) => None,
        _ => Some("chat is not allowlisted"),
    }
}

fn now() -> i64 {
    crate::remote::epoch()
}

fn owner_actor() -> Actor {
    Actor::new("owner")
}

impl Gateway {
    /// Reads each bot's token file; a bot whose token cannot be read is skipped
    /// with a warning, so the server still starts.
    pub fn new(cfg: &config::Telegram) -> Gateway {
        let mut bots = HashMap::new();
        for b in &cfg.bots {
            let token = match config::read_token(&config::expand_tilde(&b.token_file)) {
                Ok(Some(t)) => t,
                Ok(None) => {
                    eprintln!(
                        "telegram: bot {}: no token at {}; gateway disabled for it",
                        b.name, b.token_file
                    );
                    continue;
                }
                Err(e) => {
                    eprintln!("telegram: bot {}: {e:#}; gateway disabled for it", b.name);
                    continue;
                }
            };
            bots.insert(
                b.name.clone(),
                Arc::new(TgBot {
                    name: b.name.clone(),
                    chats: b.chats.clone(),
                    api: Api::new(&cfg.api_base, &token, cfg.poll_timeout),
                    queues: Mutex::new(HashMap::new()),
                    alerts: Mutex::new(Alerts::default()),
                    username: Mutex::new(None),
                    id: Mutex::new(None),
                }),
            );
        }
        Gateway {
            owner: cfg.owner_user_id,
            poll_timeout: cfg.poll_timeout,
            bots,
        }
    }

    pub fn bot(&self, name: &str) -> Result<&Arc<TgBot>> {
        self.bots
            .get(name)
            .with_context(|| format!("telegram is not configured for bot {name}"))
    }

    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.bots.keys().cloned().collect();
        n.sort();
        n
    }

    /// Starts one poller, one typing and one board thread per bot.
    pub fn start(self: &Arc<Self>, store: Arc<dyn Store>) {
        for b in self.bots.values() {
            let (gw, b2, store2) = (Arc::clone(self), Arc::clone(b), Arc::clone(&store));
            std::thread::spawn(move || gw.poll(&b2, store2.as_ref()));
            let (b2, store2) = (Arc::clone(b), Arc::clone(&store));
            std::thread::spawn(move || board_loop(&b2, store2.as_ref()));
            let (b, store) = (Arc::clone(b), Arc::clone(&store));
            std::thread::spawn(move || typing_loop(&b, store.as_ref()));
        }
    }

    fn poll(&self, b: &TgBot, store: &dyn Store) {
        let mut offset: Option<i64> = None;
        let mut started = false;
        let mut last_attention = 0i64;
        let mut failures: HashMap<i64, u32> = HashMap::new();
        while !store.stopping() {
            let id = bot_id(&store.conn(), &b.name).ok();
            let Some(id) = id else {
                // Updates wait up to 24 h on Telegram until the bot exists.
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            if !started {
                self.startup(b);
                last_attention = store
                    .conn()
                    .query_row(
                        "SELECT COALESCE(MAX(id),0) FROM bot_events WHERE bot_id=?1",
                        [id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                started = true;
            }
            if let Err(e) = self.attention_alerts(b, store, id, &mut last_attention) {
                eprintln!("telegram: bot {}: attention alerts: {e:#}", b.name);
            }
            let mut params =
                json!({"timeout": self.poll_timeout, "allowed_updates": ALLOWED_UPDATES});
            if let Some(o) = offset {
                params["offset"] = json!(o);
            }
            let updates = match b.api.call("getUpdates", &params) {
                Ok(u) => u,
                Err(e) => {
                    eprintln!("telegram: bot {}: {e}", b.name);
                    std::thread::sleep(Duration::from_secs(
                        e.retry_after.unwrap_or(3).min(MAX_RETRY_AFTER),
                    ));
                    continue;
                }
            };
            for u in updates.as_array().into_iter().flatten() {
                let Some(uid) = u["update_id"].as_i64() else {
                    continue;
                };
                match self.handle_update(b, store, u) {
                    Ok(()) => {
                        failures.remove(&uid);
                    }
                    Err(e) => {
                        let n = failures.entry(uid).or_default();
                        *n += 1;
                        eprintln!(
                            "telegram: bot {}: update {uid} (attempt {n}): {e:#}",
                            b.name
                        );
                        if *n < UPDATE_ATTEMPTS {
                            // Not confirmed: Telegram redelivers it on the next poll.
                            std::thread::sleep(Duration::from_secs(1));
                            break;
                        }
                        let _ = bot::event(
                            &store.conn(),
                            id,
                            &Actor::new("telegram"),
                            "update_skipped",
                            &json!({"update": uid, "error": format!("{e:#}")}).to_string(),
                        );
                        failures.remove(&uid);
                    }
                }
                // Confirmed only on the next getUpdates, after this commit.
                offset = Some(uid + 1);
            }
        }
    }

    /// `getMe` warning about group joins.
    fn startup(&self, b: &TgBot) {
        match b.api.call("getMe", &json!({})) {
            Ok(me) => {
                *b.username.lock().unwrap_or_else(|e| e.into_inner()) =
                    me["username"].as_str().map(String::from);
                *b.id.lock().unwrap_or_else(|e| e.into_inner()) = me["id"].as_i64();
                let groups = b.chats.iter().any(|c| *c < 0);
                if me["can_join_groups"] == true && !groups {
                    eprintln!(
                        "telegram: bot {} (@{}) can join groups but no group is allowlisted; \
                         disable it with BotFather /setjoingroups",
                        b.name,
                        me["username"].as_str().unwrap_or("?")
                    );
                }
            }
            Err(e) => eprintln!("telegram: bot {}: {e}", b.name),
        }
    }

    /// Alerts the owner about new `attention` events (bot paused by failures).
    fn attention_alerts(
        &self,
        b: &TgBot,
        store: &dyn Store,
        id: i64,
        last: &mut i64,
    ) -> Result<()> {
        let events: Vec<(i64, String)> = store
            .conn()
            .prepare("SELECT id, body FROM bot_events WHERE bot_id=?1 AND id>?2 AND kind='attention' ORDER BY id")?
            .query_map(params![id, *last], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if let Some((max, _)) = events.last() {
            *last = *max;
        }
        for (_, body) in events {
            self.notify_owner(b, store, &format!("axe bot {}: {body}", b.name));
        }
        Ok(())
    }

    /// A server-originated message to the owner's private chat.
    fn notify_owner(&self, b: &TgBot, store: &dyn Store, text: &str) {
        let params = json!({"chat_id": self.owner, "text": text});
        match b.paced(self.owner, || b.api.call("sendMessage", &params)) {
            Ok(sent) => {
                if let Err(e) = record_out(
                    store,
                    &b.name,
                    &sent,
                    "text",
                    text,
                    None,
                    json!([]),
                    json!({"system": true}),
                ) {
                    eprintln!("telegram: bot {}: record alert: {e:#}", b.name);
                }
            }
            Err(e) => eprintln!("telegram: bot {}: alert: {e}", b.name),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn security_drop(
        &self,
        b: &TgBot,
        store: &dyn Store,
        update: i64,
        kind: &str,
        reason: &str,
        from: Option<i64>,
        chat: Option<i64>,
    ) -> Result<()> {
        {
            let conn = store.conn();
            let id = bot_id(&conn, &b.name)?;
            bot::event(
                &conn,
                id,
                &Actor::new("telegram"),
                "security_drop",
                &json!({"update":update,"type":kind,"reason":reason,"from_id":from,"chat_id":chat})
                    .to_string(),
            )?;
        }
        store.changed();
        let source = format!("{from:?}/{chat:?}");
        let alert = {
            let mut a = b.alerts.lock().unwrap_or_else(|e| e.into_inner());
            let due = a.last.is_none_or(|t| t.elapsed() >= ALERT_INTERVAL);
            if due && !a.sources.contains(&source) {
                a.sources.insert(source);
                a.last = Some(Instant::now());
                true
            } else {
                false
            }
        };
        if alert {
            let who = from.map_or("an unknown sender".into(), |f| format!("user {f}"));
            let chat = chat.map_or("an unknown chat".into(), |c| format!("chat {c}"));
            self.notify_owner(
                b,
                store,
                &format!(
                    "axe bot {}: dropped a Telegram {kind} from {who} in {chat} ({reason}). \
                     Later drops from this source are only logged (`axe bot log`).",
                    b.name
                ),
            );
        }
        Ok(())
    }

    /// Handles one update; returns after its effects are committed.
    pub fn handle_update(&self, b: &TgBot, store: &dyn Store, u: &Value) -> Result<()> {
        let uid = u["update_id"].as_i64().unwrap_or_default();
        if u["message"].is_object() {
            self.inbound(b, store, uid, &u["message"], false)
        } else if u["edited_message"].is_object() {
            self.inbound(b, store, uid, &u["edited_message"], true)
        } else if u["callback_query"].is_object() {
            self.button(b, store, uid, &u["callback_query"])
        } else if u["my_chat_member"].is_object() {
            self.membership(b, store, uid, &u["my_chat_member"])
        } else if u["message_reaction"].is_object() {
            self.reaction(b, store, uid, &u["message_reaction"])
        } else {
            Ok(())
        }
    }

    fn inbound(&self, b: &TgBot, store: &dyn Store, uid: i64, m: &Value, edit: bool) -> Result<()> {
        let chat = m["chat"]["id"].as_i64();
        let kind = if edit { "edited_message" } else { "message" };
        if own_pin(*b.id.lock().unwrap_or_else(|e| e.into_inner()), m) {
            return Ok(());
        }
        if let Some(reason) = refuse(self.owner, &b.chats, &m["from"], m, chat) {
            return self.security_drop(b, store, uid, kind, reason, m["from"]["id"].as_i64(), chat);
        }
        let chat = chat.expect("refuse() checks the chat");
        let tg_id = m["message_id"]
            .as_i64()
            .context("message without message_id")?;
        let text = m["text"]
            .as_str()
            .or(m["caption"].as_str())
            .unwrap_or("")
            .to_string();
        // Commands only from the owner's own plain text, never forwarded
        // content or captions.
        let plain = m["text"].is_string()
            && m["forward_origin"].is_null()
            && m["is_automatic_forward"].is_null();
        if !edit && plain {
            let me = b.username.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some((cmd, arg)) = command(&text, me.as_deref()) {
                return self.owner_command(b, store, uid, chat, tg_id, m, cmd, arg);
            }
        }
        let files = attachments(m);
        let mut flags = json!({});
        if !m["forward_origin"].is_null() {
            flags["forwarded"] = json!(true);
        }
        if m["is_automatic_forward"] == true {
            flags["automatic_forward"] = json!(true);
        }
        let (dup, msg) = {
            let mut conn = store.conn();
            let id = bot_id(&conn, &b.name)?;
            let edit_of = if edit {
                local_id(&conn, id, chat, tg_id)?
            } else {
                None
            };
            let reply_to = match m["reply_to_message"]["message_id"].as_i64() {
                Some(r) => local_id(&conn, id, chat, r)?,
                None => None,
            };
            let message_id = if edit { uid } else { tg_id };
            let kind = if edit {
                "edit".to_string()
            } else {
                files
                    .first()
                    .and_then(|a| a["type"].as_str())
                    .unwrap_or("text")
                    .to_string()
            };
            let dup = conn
                .query_row(
                    "SELECT 1 FROM bot_messages WHERE bot_id=?1 AND channel='telegram' AND chat_id=?2
                       AND message_id=?3 AND kind=?4",
                    params![id, chat, message_id, kind],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let (msg, _) = bot::record_message(
                &mut conn,
                &b.name,
                &NewMessage {
                    direction: "in".into(),
                    chat_id: chat,
                    thread_id: m["message_thread_id"].as_i64(),
                    message_id,
                    reply_to,
                    edit_of,
                    from_id: m["from"]["id"].as_i64(),
                    kind,
                    text,
                    attachments: json!(files),
                    media_group_id: m["media_group_id"].as_str().map(String::from),
                    flags,
                },
                now(),
            )?;
            (dup, msg)
        };
        store.changed();
        if !dup {
            // Acknowledge: the owner sees the message arrived.
            if !edit {
                let r = b.api.call(
                    "setMessageReaction",
                    &json!({"chat_id":chat,"message_id":tg_id,"reaction":[{"type":"emoji","emoji":ACK_EMOJI}]}),
                );
                if let Err(e) = r {
                    eprintln!("telegram: bot {}: ack #{msg}: {e}", b.name);
                }
            }
            b.typing(chat, m["message_thread_id"].as_i64());
        }
        Ok(())
    }

    fn button(&self, b: &TgBot, store: &dyn Store, uid: i64, cq: &Value) -> Result<()> {
        let chat = cq["message"]["chat"]["id"].as_i64();
        if let Some(reason) = refuse(self.owner, &b.chats, &cq["from"], &json!({}), chat) {
            return self.security_drop(
                b,
                store,
                uid,
                "callback_query",
                reason,
                cq["from"]["id"].as_i64(),
                chat,
            );
        }
        let chat = chat.expect("refuse() checks the chat");
        let data = cq["data"].as_str().unwrap_or("").to_string();
        let thread = cq["message"]["message_thread_id"].as_i64();
        let tg_msg = cq["message"]["message_id"].as_i64();
        let (target, answered) = {
            let mut conn = store.conn();
            let id = bot_id(&conn, &b.name)?;
            let target = match tg_msg {
                Some(t) => local_id(&conn, id, chat, t)?,
                None => None,
            };
            let stored = target.map(|t| get_message(&conn, &b.name, t)).transpose()?;
            // A later press on an answered keyboard is not a new wake.
            let answered = stored.as_ref().is_some_and(|m| {
                !m.flags["answered"].is_null() && m.flags["answered"]["update"] != uid
            });
            if !answered {
                bot::record_message(
                    &mut conn,
                    &b.name,
                    &NewMessage {
                        direction: "in".into(),
                        chat_id: chat,
                        thread_id: thread,
                        message_id: uid,
                        reply_to: target,
                        from_id: cq["from"]["id"].as_i64(),
                        kind: "button".into(),
                        text: data.clone(),
                        ..Default::default()
                    },
                    now(),
                )?;
                if let Some(m) = stored.as_ref() {
                    let label = button_label(&m.flags, &data);
                    conn.execute(
                        "UPDATE bot_messages SET flags=json_set(flags,'$.answered',json(?1)) WHERE id=?2",
                        params![
                            json!({"label": label, "data": data, "update": uid}).to_string(),
                            m.id
                        ],
                    )?;
                }
            }
            (stored, answered)
        };
        store.changed();
        let label = target
            .as_ref()
            .map_or(data.clone(), |m| button_label(&m.flags, &data));
        // The client shows the toast and stops its progress indicator.
        if let Some(qid) = cq["id"].as_str() {
            let text = if answered {
                "Already received".to_string()
            } else {
                truncate(&format!("Got it: {label}"), TOAST_LIMIT)
            };
            let r = b.api.call(
                "answerCallbackQuery",
                &json!({"callback_query_id": qid, "text": text}),
            );
            if let Err(e) = r {
                eprintln!("telegram: bot {}: {e}", b.name);
            }
        }
        if answered {
            return Ok(());
        }
        if let Some(t) = tg_msg {
            self.mark_selected(b, chat, t, target.as_ref(), &label);
        }
        b.typing(chat, thread);
        Ok(())
    }

    /// Replaces the pressed message's keyboard with the chosen label:
    /// `editMessageText` with a `> label (selected)` line, else (text unknown
    /// or too long, or the edit failed) `editMessageReplyMarkup` alone.
    /// Best effort: the press is already recorded.
    fn mark_selected(
        &self,
        b: &TgBot,
        chat: i64,
        tg_msg: i64,
        stored: Option<&StoredMessage>,
        label: &str,
    ) {
        let none = json!({"inline_keyboard": []});
        if let Some(m) = stored.filter(|m| m.kind == "text" && !m.text.is_empty()) {
            let html = m.flags["html"] == true;
            let line = if html {
                format!("&gt; {} (selected)", html_escape(label))
            } else {
                format!("> {label} (selected)")
            };
            let text = format!("{}\n\n{line}", m.text);
            if utf16_len(&text) <= TEXT_LIMIT {
                let mut p = json!({"chat_id": chat, "message_id": tg_msg, "text": text, "reply_markup": none});
                if html {
                    p["parse_mode"] = json!("HTML");
                }
                match b.api.call("editMessageText", &p) {
                    Ok(_) => return,
                    Err(e) => eprintln!("telegram: bot {}: {e}", b.name),
                }
            }
        }
        let p = json!({"chat_id": chat, "message_id": tg_msg, "reply_markup": none});
        if let Err(e) = b.api.call("editMessageReplyMarkup", &p) {
            eprintln!("telegram: bot {}: {e}", b.name);
        }
    }

    fn membership(&self, b: &TgBot, store: &dyn Store, uid: i64, cm: &Value) -> Result<()> {
        let chat = cm["chat"]["id"].as_i64();
        let status = cm["new_chat_member"]["status"].as_str().unwrap_or("");
        let joined = matches!(
            status,
            "member" | "administrator" | "restricted" | "creator"
        );
        let allowed = chat.is_some_and(|c| b.chats.contains(&c));
        if joined && !allowed {
            let left = match chat {
                Some(c) => match b.api.call("leaveChat", &json!({"chat_id": c})) {
                    Ok(_) => "added to a chat that is not allowlisted; left it",
                    Err(e) => {
                        eprintln!("telegram: bot {}: {e}", b.name);
                        "added to a chat that is not allowlisted; leaveChat failed"
                    }
                },
                None => "added to an unknown chat",
            };
            return self.security_drop(
                b,
                store,
                uid,
                "my_chat_member",
                left,
                cm["from"]["id"].as_i64(),
                chat,
            );
        }
        Ok(())
    }

    fn reaction(&self, b: &TgBot, store: &dyn Store, uid: i64, r: &Value) -> Result<()> {
        let chat = r["chat"]["id"].as_i64();
        if let Some(reason) = refuse(self.owner, &b.chats, &r["user"], r, chat) {
            return self.security_drop(
                b,
                store,
                uid,
                "message_reaction",
                reason,
                r["user"]["id"].as_i64(),
                chat,
            );
        }
        let chat = chat.expect("refuse() checks the chat");
        let emoji: Vec<&str> = r["new_reaction"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| x["emoji"].as_str().unwrap_or("(custom)"))
            .collect();
        if emoji.is_empty() {
            return Ok(()); // A reaction was removed.
        }
        {
            let mut conn = store.conn();
            let id = bot_id(&conn, &b.name)?;
            let target = match r["message_id"].as_i64() {
                Some(t) => local_id(&conn, id, chat, t)?,
                None => None,
            };
            bot::record_message(
                &mut conn,
                &b.name,
                &NewMessage {
                    direction: "in".into(),
                    chat_id: chat,
                    message_id: uid,
                    reply_to: target,
                    from_id: r["user"]["id"].as_i64(),
                    kind: "reaction".into(),
                    text: emoji.join(" "),
                    ..Default::default()
                },
                now(),
            )?;
        }
        store.changed();
        Ok(())
    }

    /// `/status`, `/stop`, `/pause`, `/resume`, `/reset`, `/wake [note]`:
    /// handled here, never waking the bot. The reply is recorded.
    #[allow(clippy::too_many_arguments)]
    fn owner_command(
        &self,
        b: &TgBot,
        store: &dyn Store,
        uid: i64,
        chat: i64,
        tg_id: i64,
        m: &Value,
        cmd: &str,
        arg: &str,
    ) -> Result<()> {
        let actor = owner_actor();
        let reply = {
            let mut conn = store.conn();
            let r = match cmd {
                "status" => status(&conn, &b.name),
                "stop" => bot::change(&mut conn, &actor, &b.name, "stop")
                    .map(|_| "stopping the live run; queued wakes are kept".to_string()),
                "pause" => bot::change(&mut conn, &actor, &b.name, "pause")
                    .map(|_| "paused: no new wakes; a live run is stopped".to_string()),
                "resume" => {
                    bot::change(&mut conn, &actor, &b.name, "resume").map(|_| "resumed".to_string())
                }
                "reset" => bot::change(&mut conn, &actor, &b.name, "reset")
                    .map(|_| "session cleared; the next run starts fresh".to_string()),
                _ => {
                    let note = if arg.is_empty() { "owner /wake" } else { arg };
                    let key = format!("tg-update-{uid}");
                    bot::add_wake(&mut conn, &actor, &b.name, now(), note, Some(&key), now())
                        .map(|(w, _)| format!("wake {w} queued"))
                }
            };
            r.unwrap_or_else(|e| format!("/{cmd}: {e:#}"))
        };
        store.changed();
        let thread = m["message_thread_id"].as_i64();
        let mut params =
            json!({"chat_id":chat,"text":reply,"reply_parameters":{"message_id":tg_id}});
        if let Some(t) = thread {
            params["message_thread_id"] = json!(t);
        }
        // The command already committed: a failed reply must not make the
        // update redeliver and repeat it.
        match b.paced(chat, || b.api.call("sendMessage", &params)) {
            Ok(sent) => {
                if let Err(e) = record_out(
                    store,
                    &b.name,
                    &sent,
                    "text",
                    &reply,
                    None,
                    json!([]),
                    json!({"system": true}),
                ) {
                    eprintln!("telegram: bot {}: record /{cmd} reply: {e:#}", b.name);
                }
            }
            Err(e) => eprintln!("telegram: bot {}: /{cmd} reply: {e}", b.name),
        }
        Ok(())
    }

    // Outbound, for `axe bot tg`.

    fn target(
        &self,
        b: &TgBot,
        store: &dyn Store,
        chat: Option<i64>,
        reply_to: Option<i64>,
    ) -> Result<(i64, Option<StoredMessage>)> {
        let conn = store.conn();
        let reply = reply_to
            .map(|r| get_message(&conn, &b.name, r))
            .transpose()?;
        if let Some(r) = &reply {
            if r.kind == "edit" || r.kind == "button" || r.kind == "reaction" {
                bail!(
                    "message {} is a {}, not a Telegram message; reply to #{}",
                    r.id,
                    r.kind,
                    r.edit_of
                        .or(r.reply_to)
                        .map_or("?".into(), |i| i.to_string())
                );
            }
        }
        let chat = match (chat, &reply) {
            (Some(c), Some(r)) if c != r.chat_id => {
                bail!("--chat {c} differs from the chat of message {}", r.id)
            }
            (Some(c), _) => c,
            (None, Some(r)) => r.chat_id,
            (None, None) => {
                let id = bot_id(&conn, &b.name)?;
                conn.query_row(
                    "SELECT chat_id FROM bot_messages WHERE bot_id=?1 AND direction='in' ORDER BY id DESC LIMIT 1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .filter(|c| b.chats.contains(c))
                .unwrap_or(b.chats[0])
            }
        };
        b.allowed(chat)?;
        Ok((chat, reply))
    }

    /// Sends text, split at line boundaries; returns the stored messages.
    pub fn send(
        &self,
        store: &dyn Store,
        name: &str,
        text: &str,
        o: &SendOpts,
    ) -> Result<Vec<Value>> {
        let b = self.bot(name)?;
        if text.trim().is_empty() {
            bail!("text must not be empty");
        }
        if o.html && utf16_len(text) > TEXT_LIMIT {
            bail!("--html text is over {TEXT_LIMIT} characters; split it yourself so markup stays balanced");
        }
        check_buttons(&o.buttons, &o.rows)?;
        if let Some(t) = o.task {
            if o.buttons.is_empty() {
                bail!("--task needs --buttons: a card is a message with buttons");
            }
            store
                .conn()
                .query_row("SELECT 1 FROM tasks WHERE id=?1", [t], |_| Ok(()))
                .optional()?
                .with_context(|| format!("task {t} not found"))?;
        }
        let (chat, reply) = self.target(b, store, o.chat, o.reply_to)?;
        let parts = split_text(text, TEXT_LIMIT);
        if parts.len() > MAX_PARTS {
            bail!(
                "text needs {} messages (max {MAX_PARTS}); send it as a file",
                parts.len()
            );
        }
        let mut out = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let mut p = json!({"chat_id": chat, "text": part});
            if let Some(t) = o.thread {
                p["message_thread_id"] = json!(t);
            }
            if o.html {
                p["parse_mode"] = json!("HTML");
            }
            if o.silent {
                p["disable_notification"] = json!(true);
            }
            if let (0, Some(r)) = (i, &reply) {
                p["reply_parameters"] = json!({"message_id": r.message_id});
            }
            if i + 1 == parts.len() && !o.buttons.is_empty() {
                p["reply_markup"] = keyboard(&o.buttons, &o.rows);
            }
            let sent = b.paced(chat, || b.api.call("sendMessage", &p))?;
            let reply_local = if i == 0 {
                reply.as_ref().map(|r| r.id)
            } else {
                None
            };
            // The keyboard's buttons and parse mode, for marking a press.
            let flags = if p["reply_markup"].is_null() {
                json!({})
            } else if let Some(t) = o.task {
                json!({"buttons": o.buttons, "html": o.html, "task": t})
            } else {
                json!({"buttons": o.buttons, "html": o.html})
            };
            let id = record_out(
                store,
                name,
                &sent,
                "text",
                part,
                reply_local,
                json!([]),
                flags,
            )?;
            if board::default_on(&mut store.conn(), name, id)? {
                store.changed();
            }
            out.push(json!({"id": id, "message_id": sent["message_id"]}));
        }
        Ok(out)
    }

    /// Uploads a file as `photo`, `document` or `voice`.
    #[allow(clippy::too_many_arguments)]
    pub fn send_file(
        &self,
        store: &dyn Store,
        name: &str,
        kind: &str,
        file_name: &str,
        bytes: &[u8],
        caption: Option<&str>,
        o: &SendOpts,
    ) -> Result<Value> {
        let b = self.bot(name)?;
        check_upload(kind, bytes.len() as u64)?;
        let method = match kind {
            "photo" => "sendPhoto",
            "voice" => "sendVoice",
            _ => "sendDocument",
        };
        let (chat, reply) = self.target(b, store, o.chat, o.reply_to)?;
        let mut fields = vec![("chat_id", chat.to_string())];
        if let Some(c) = caption.filter(|c| !c.is_empty()) {
            fields.push(("caption", c.to_string()));
        }
        if let Some(t) = o.thread {
            fields.push(("message_thread_id", t.to_string()));
        }
        if let Some(r) = &reply {
            fields.push((
                "reply_parameters",
                json!({"message_id": r.message_id}).to_string(),
            ));
        }
        let sent = b.paced(chat, || {
            b.api.upload(method, &fields, kind, file_name, bytes)
        })?;
        let files = attachments(&sent);
        let id = record_out(
            store,
            name,
            &sent,
            kind,
            caption.unwrap_or(""),
            reply.map(|r| r.id),
            json!(files),
            json!({}),
        )?;
        Ok(json!({"id": id, "message_id": sent["message_id"]}))
    }

    fn own_message(&self, b: &TgBot, store: &dyn Store, msg: i64) -> Result<StoredMessage> {
        let m = get_message(&store.conn(), &b.name, msg)?;
        if m.kind == "edit" || m.kind == "button" || m.kind == "reaction" {
            bail!("message {msg} is a {}, not a Telegram message", m.kind);
        }
        b.allowed(m.chat_id)?;
        Ok(m)
    }

    /// Edits one of the bot's text messages. `text` replaces the text
    /// (`editMessageText`); `buttons` replaces the inline keyboard, and
    /// `Some(&[])` removes it. With only `buttons`, the text is kept
    /// (`editMessageReplyMarkup`). A text edit without `buttons` sends no
    /// `reply_markup`, as before; Telegram then drops an existing keyboard.
    /// `rows` lays the buttons out as in [`SendOpts::rows`].
    #[allow(clippy::too_many_arguments)]
    pub fn edit(
        &self,
        store: &dyn Store,
        name: &str,
        msg: i64,
        text: Option<&str>,
        html: bool,
        buttons: Option<&[(String, String)]>,
        rows: &[usize],
    ) -> Result<Value> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        if m.direction != "out" || m.kind != "text" {
            bail!("only the bot's own text messages can be edited");
        }
        if let Some(t) = text {
            if t.trim().is_empty() || utf16_len(t) > TEXT_LIMIT {
                bail!("edited text must be 1-{TEXT_LIMIT} characters");
            }
        }
        match buttons {
            Some(bs) => check_buttons(bs, rows)?,
            None if !rows.is_empty() => bail!("button rows need buttons"),
            None => {}
        }
        let mut p = json!({"chat_id": m.chat_id, "message_id": m.message_id});
        if let Some(bs) = buttons {
            p["reply_markup"] = keyboard(bs, rows);
        }
        let method = match text {
            Some(t) => {
                p["text"] = json!(t);
                if html {
                    p["parse_mode"] = json!("HTML");
                }
                "editMessageText"
            }
            None if buttons.is_some() => "editMessageReplyMarkup",
            None => bail!("nothing to edit: give text, buttons or clear-buttons"),
        };
        let res = match b.paced(m.chat_id, || b.api.call(method, &p)) {
            Ok(r) => r,
            // Telegram already shows this (for example clearing a keyboard an
            // older text edit dropped): record the state all the same.
            Err(e) if e.description.contains("message is not modified") => Value::Null,
            Err(e) => return Err(e.into()),
        };
        let conn = store.conn();
        if let Some(t) = text {
            // Without `buttons`, Telegram drops the keyboard: the card closes.
            conn.execute(
                // A hand edit of a goal pin is rendered over on the next pass.
                "UPDATE bot_messages SET text=?1, flags=json_remove(json_set(flags,'$.edited',json('true')),'$.board_key','$.goal_board_key','$.goal_pin_key') WHERE id=?2",
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
            // A new keyboard takes fresh presses: the old answer no longer
            // applies, and labels come from the new buttons.
            let html = if text.is_some() {
                html
            } else {
                m.flags["html"] == true
            };
            conn.execute(
                "UPDATE bot_messages SET flags=json_remove(json_set(flags,'$.buttons',json(?1),'$.html',json(?2)),'$.answered') WHERE id=?3",
                params![json!(bs).to_string(), json!(html).to_string(), msg],
            )?;
        }
        drop(conn);
        store.changed();
        // The edited Message Telegram returned says which buttons remain.
        let left = res["reply_markup"]["inline_keyboard"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.as_array())
                    .map(Vec::len)
                    .sum::<usize>()
            })
            .or_else(|| res.is_object().then_some(0));
        Ok(json!({"id": msg, "buttons": left}))
    }

    /// Pins or unpins one of the bot's own messages.
    pub fn pin(
        &self,
        store: &dyn Store,
        name: &str,
        msg: i64,
        pin: bool,
        silent: bool,
    ) -> Result<Value> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        if m.direction != "out" {
            bail!("only the bot's own messages can be pinned or unpinned");
        }
        let mut p = json!({"chat_id": m.chat_id, "message_id": m.message_id});
        let method = if pin {
            if silent {
                p["disable_notification"] = json!(true);
            }
            "pinChatMessage"
        } else {
            "unpinChatMessage"
        };
        b.paced(m.chat_id, || b.api.call(method, &p))?;
        store.conn().execute(
            "UPDATE bot_messages SET flags=json_set(flags,'$.pinned',json(?1)) WHERE id=?2",
            params![json!(pin).to_string(), msg],
        )?;
        store.changed();
        Ok(json!({"id": msg, "pinned": pin}))
    }

    pub fn react(&self, store: &dyn Store, name: &str, msg: i64, emoji: &str) -> Result<Value> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        b.api.call(
            "setMessageReaction",
            &json!({"chat_id": m.chat_id, "message_id": m.message_id, "reaction": [{"type":"emoji","emoji":emoji}]}),
        )?;
        Ok(json!({"id": msg}))
    }

    pub fn delete(&self, store: &dyn Store, name: &str, msg: i64) -> Result<Value> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        b.api.call(
            "deleteMessage",
            &json!({"chat_id": m.chat_id, "message_id": m.message_id}),
        )?;
        // The stored row stays (history is never deleted); it is marked.
        store.conn().execute(
            "UPDATE bot_messages SET flags=json_set(flags,'$.deleted',json('true')) WHERE id=?1",
            [msg],
        )?;
        store.changed();
        Ok(json!({"id": msg}))
    }

    pub fn typing(&self, store: &dyn Store, name: &str, chat: Option<i64>) -> Result<Value> {
        let b = self.bot(name)?;
        let (chat, _) = self.target(b, store, chat, None)?;
        b.api.call(
            "sendChatAction",
            &json!({"chat_id": chat, "action": "typing"}),
        )?;
        Ok(json!({"chat": chat}))
    }

    /// Downloads attachment `n` (1-based) of a stored message:
    /// (file name, MIME type, bytes).
    pub fn file(
        &self,
        store: &dyn Store,
        name: &str,
        msg: i64,
        n: Option<usize>,
    ) -> Result<(String, String, Vec<u8>)> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        let list = m.attachments.as_array().cloned().unwrap_or_default();
        let a = match (n, list.len()) {
            (_, 0) => bail!("message {msg} has no attachments"),
            (None, 1) => &list[0],
            (None, k) => bail!("message {msg} has {k} attachments; use {msg}/1..{msg}/{k}"),
            (Some(i), k) if i >= 1 && i <= k => &list[i - 1],
            (Some(i), k) => bail!("message {msg} has {k} attachments, not {i}"),
        };
        let too_big = |size: u64| {
            anyhow::anyhow!(
                "the attachment is {:.1} MB; Telegram bots can download at most 20 MB",
                size as f64 / 1_048_576.0
            )
        };
        if let Some(size) = a["size"].as_u64().filter(|s| *s > DOWNLOAD_LIMIT) {
            return Err(too_big(size));
        }
        let file_id = a["file_id"]
            .as_str()
            .context("attachment without file_id")?;
        let f = b.api.call("getFile", &json!({"file_id": file_id}))?;
        if let Some(size) = f["file_size"].as_u64().filter(|s| *s > DOWNLOAD_LIMIT) {
            return Err(too_big(size));
        }
        let path = f["file_path"]
            .as_str()
            .context("getFile returned no file_path")?;
        let bytes = b.api.download(path, DOWNLOAD_LIMIT)?;
        let fallback = format!("{msg}-{}", path.rsplit('/').next().unwrap_or("file"));
        let name = safe_name(a["name"].as_str().unwrap_or(&fallback));
        let mime = a["mime"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string();
        Ok((name, mime, bytes))
    }
}

/// Keeps `typing` showing while the bot has a live run: about every
/// [`TYPING_REFRESH`] in each chat whose message or button woke that run
/// (wakes claimed at start or delivered through the hook). It stops when the
/// run ends; runs without a chat wake (schedules, self-wakes) show nothing.
fn typing_loop(b: &TgBot, store: &dyn Store) {
    let mut last: HashMap<(i64, Option<i64>), Instant> = HashMap::new();
    while !store.stopping() {
        // The bot may not exist yet.
        let chats = live_chats(&store.conn(), &b.name).unwrap_or_default();
        last.retain(|k, _| chats.contains(k));
        for (chat, thread) in chats {
            if !b.chats.contains(&chat) {
                continue;
            }
            let due = last
                .get(&(chat, thread))
                .is_none_or(|t| t.elapsed() >= TYPING_REFRESH);
            if due {
                b.typing(chat, thread);
                last.insert((chat, thread), Instant::now());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Keeps the bot's inbox in step with stored state ([`inbox_pass`]): a pass
/// after every committed change (at most one per [`BOARD_MIN_INTERVAL`]) and
/// at least every [`BOARD_RESYNC`]. A failed pass is logged (stderr and a
/// `board_error` bot event) and retried with backoff.
fn board_loop(b: &TgBot, store: &dyn Store) {
    let mut backoff: Option<Duration> = None;
    while !store.stopping() {
        let seen = store.seq();
        match inbox_pass(b, store) {
            Ok(_) => backoff = None,
            Err(e) => {
                let mut d = backoff.map_or(BOARD_RETRY, |d| (d * 2).min(BOARD_RETRY_MAX));
                if let Some(s) = e.downcast_ref::<ApiError>().and_then(|a| a.retry_after) {
                    d = d.max(Duration::from_secs(s.min(MAX_RETRY_AFTER)));
                }
                backoff = Some(d);
                eprintln!(
                    "telegram: bot {}: board: {e:#}; retrying in {}s",
                    b.name,
                    d.as_secs()
                );
                let conn = store.conn();
                if let Ok(id) = bot_id(&conn, &b.name) {
                    let body = json!({"error": format!("{e:#}"), "retry_seconds": d.as_secs()});
                    let _ = bot::event(
                        &conn,
                        id,
                        &Actor::new("telegram"),
                        "board_error",
                        &body.to_string(),
                    );
                }
                drop(conn);
                store.changed();
            }
        }
        match backoff {
            Some(d) => {
                let end = Instant::now() + d;
                while !store.stopping() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(200).min(end - Instant::now()));
                }
            }
            None => {
                store.wait_changed(seen, BOARD_RESYNC);
                std::thread::sleep(BOARD_MIN_INTERVAL);
            }
        }
    }
}

/// One inbox pass, for a bot with a board message: retires an old goal
/// board and the old overview, asks for or posts the cards of escalated
/// tasks, closes cards of tasks no longer escalated, pins open cards and
/// unpins closed ones, then keeps one pinned message per active goal. Each
/// step is recorded in message flags, so a retry or a restart repeats only
/// what is left.
fn inbox_pass(b: &TgBot, store: &dyn Store) -> Result<()> {
    let conn = store.conn();
    if bot_id(&conn, &b.name).is_err() {
        return Ok(());
    }
    let goal_board = goal_board::message(&conn, &b.name)?;
    let Some(m) = board::message(&conn, &b.name)? else {
        return Ok(());
    };
    drop(conn);
    if m.flags["board_retired"] != true {
        retire_overview(b, store, &m)?;
    }
    if let Some(g) = goal_board {
        retire_goal_board(b, store, &g)?;
    }
    task_cards(b, store, m.chat_id, m.thread_id)?;
    sync_pins(b, store)?;
    goal_pins(b, store, m.chat_id, m.thread_id)?;
    Ok(())
}

/// Logs a Telegram refusal (400) of `step` on message `msg` as a skipped
/// `board_error`, so one bad message never holds up the inbox; other errors
/// are returned and retried.
fn skip_refused(b: &TgBot, store: &dyn Store, step: &str, msg: i64, e: ApiError) -> Result<()> {
    if e.code != Some(400) {
        return Err(e.into());
    }
    let conn = store.conn();
    let body = json!({"error": e.to_string(), "step": step, "message": msg, "skipped": true});
    bot::event(
        &conn,
        bot_id(&conn, &b.name)?,
        &Actor::new("telegram"),
        "board_error",
        &body.to_string(),
    )?;
    Ok(())
}

/// The separate goal board is retired (each active goal has its own pin):
/// unpin it, edit it to a pointer, and stop rendering it.
fn retire_goal_board(b: &TgBot, store: &dyn Store, g: &StoredMessage) -> Result<()> {
    let live = g.flags["deleted"] != true && b.allowed(g.chat_id).is_ok();
    if live && g.flags["pinned"] != false {
        let p = json!({"chat_id": g.chat_id, "message_id": g.message_id});
        if let Err(e) = b.paced(g.chat_id, || b.api.call("unpinChatMessage", &p)) {
            skip_refused(b, store, "unpin goal board", g.id, e)?;
        }
        set_flag(store, g.id, "$.pinned", json!(false))?;
    }
    let text = board::merged_text();
    if live && g.text != text {
        let p = json!({"chat_id": g.chat_id, "message_id": g.message_id, "text": text});
        match b.paced(g.chat_id, || b.api.call("editMessageText", &p)) {
            Ok(_) => {}
            Err(e) if e.description.contains("message is not modified") => {}
            Err(e) => skip_refused(b, store, "edit goal board", g.id, e)?,
        }
        store.conn().execute(
            "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.edited',json('true')) WHERE id=?2",
            params![text, g.id],
        )?;
    }
    goal_board::set_message(&mut store.conn(), &b.name, None)?;
    store.changed();
    Ok(())
}

/// Escalated tasks without a card since their escalation: the bot gets one
/// wake (keyed by task and escalation, so a restart never repeats it) to post
/// the card itself; [`board::CARD_WAIT`] after that wake the server posts
/// [`board::fallback_card`] in the board message's chat. Open cards of tasks no
/// longer escalated lose their keyboard (closed).
fn task_cards(b: &TgBot, store: &dyn Store, chat: i64, thread: Option<i64>) -> Result<()> {
    let needs = board::card_needs(&store.conn(), &b.name)?;
    let t = now();
    for n in needs {
        // The wake is due at once; its due time is when the bot was asked.
        let key = format!("card-task-{}-{}", n.task, n.since);
        let (_, asked) = bot::add_wake(
            &mut store.conn(),
            &Actor::new("telegram"),
            &b.name,
            t,
            &board::card_wake_note(&n, t + board::CARD_WAIT),
            Some(&key),
            t,
        )?;
        if t < bot::epoch_of(&asked).unwrap_or(t) + board::CARD_WAIT {
            continue;
        }
        b.allowed(chat)?;
        let (text, buttons) = board::fallback_card(&n);
        let rows = vec![1; buttons.len()];
        let mut p = json!({"chat_id": chat, "text": text, "parse_mode": "HTML",
            "disable_notification": true, "reply_markup": keyboard(&buttons, &rows)});
        if let Some(th) = thread {
            p["message_thread_id"] = json!(th);
        }
        let sent = b.paced(chat, || b.api.call("sendMessage", &p))?;
        let id = record_out(
            store,
            &b.name,
            &sent,
            "text",
            &text,
            None,
            json!([]),
            json!({"buttons": buttons, "html": true, "task": n.task, "fallback": true}),
        )?;
        let conn = store.conn();
        bot::event(
            &conn,
            bot_id(&conn, &b.name)?,
            &Actor::new("telegram"),
            "card_fallback",
            &json!({"message": id, "task": n.task}).to_string(),
        )?;
    }
    let stale = board::stale_task_cards(&store.conn(), &b.name)?;
    for (msg, task, state) in stale {
        let m = get_message(&store.conn(), &b.name, msg)?;
        if b.allowed(m.chat_id).is_ok() {
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id,
                "reply_markup": {"inline_keyboard": []}});
            match b.paced(m.chat_id, || b.api.call("editMessageReplyMarkup", &p)) {
                Ok(_) => {}
                Err(e) if e.description.contains("message is not modified") => {}
                Err(e) => skip_refused(b, store, "close card", msg, e)?,
            }
        }
        store.conn().execute(
            "UPDATE bot_messages SET flags=json_set(json_remove(flags,'$.buttons'),'$.closed',?1) WHERE id=?2",
            params![format!("task #{task} is {state}"), msg],
        )?;
        store.changed();
    }
    Ok(())
}

/// Pins every open card silently (oldest first, `flags.card_pin`) and unpins
/// the cards the server pinned once they are closed. A pin Telegram refuses
/// (400) is logged and not retried (`flags.pin_skipped`).
fn sync_pins(b: &TgBot, store: &dyn Store) -> Result<()> {
    let closed = board::closed_pins(&store.conn(), &b.name)?;
    for m in closed {
        if m.flags["deleted"] != true && b.allowed(m.chat_id).is_ok() {
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id});
            if let Err(e) = b.paced(m.chat_id, || b.api.call("unpinChatMessage", &p)) {
                skip_refused(b, store, "unpin card", m.id, e)?;
            }
        }
        set_flag(store, m.id, "$.pinned", json!(false))?;
        store.changed();
    }
    let lines = board::lines(&store.conn(), &b.name)?;
    for l in lines.iter().filter(|l| l.kind == "card") {
        let m = get_message(&store.conn(), &b.name, l.id)?;
        if m.flags["pinned"] == true || m.flags["pin_skipped"] == true {
            continue;
        }
        if b.allowed(m.chat_id).is_err() {
            continue;
        }
        let p =
            json!({"chat_id": m.chat_id, "message_id": m.message_id, "disable_notification": true});
        match b.paced(m.chat_id, || b.api.call("pinChatMessage", &p)) {
            Ok(_) => {
                // One write, so a reader never sees a pin without its kind.
                store.conn().execute(
                    "UPDATE bot_messages SET flags=json_set(flags,'$.pinned',json('true'),'$.card_pin',json('true')) WHERE id=?1",
                    [m.id],
                )?;
            }
            Err(e) => {
                skip_refused(b, store, "pin card", m.id, e)?;
                set_flag(store, m.id, "$.pin_skipped", json!(true))?;
            }
        }
        store.changed();
    }
    Ok(())
}

/// The pinned overview is retired: unpin board message `m` (and the
/// overview a move had not finished leaving, `flags.board_from`) and edit it
/// to [`board::RETIRED_TEXT`]. Telegram refusing a step (400) is logged as a
/// skipped `board_error`; other failures are retried.
fn retire_overview(b: &TgBot, store: &dyn Store, m: &StoredMessage) -> Result<()> {
    let mut old = vec![m.clone()];
    if let Some(from) = m.flags["board_from"].as_i64() {
        if let Ok(f) = get_message(&store.conn(), &b.name, from) {
            old.push(f);
        }
    }
    for o in &old {
        let live = o.flags["deleted"] != true && b.allowed(o.chat_id).is_ok();
        // A board message that never was an overview keeps its text.
        let shown = o.id != m.id || o.flags["board_key"].is_string();
        // A rendered overview was pinned unless recorded otherwise.
        let pinned = o.flags["pinned"] == true || (shown && o.flags["pinned"] != false);
        if live && pinned {
            let p = json!({"chat_id": o.chat_id, "message_id": o.message_id});
            if let Err(e) = b.paced(o.chat_id, || b.api.call("unpinChatMessage", &p)) {
                skip_refused(b, store, "unpin overview", o.id, e)?;
            }
        }
        if live && shown && o.text != board::RETIRED_TEXT {
            // An empty keyboard drops the pick buttons of older boards.
            let p = json!({"chat_id": o.chat_id, "message_id": o.message_id,
                "text": board::RETIRED_TEXT, "reply_markup": {"inline_keyboard": []}});
            match b.paced(o.chat_id, || b.api.call("editMessageText", &p)) {
                Ok(_) => {}
                Err(e) if e.description.contains("message is not modified") => {}
                Err(e) => skip_refused(b, store, "edit overview", o.id, e)?,
            }
            store.conn().execute(
                "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.edited',json('true')) WHERE id=?2",
                params![board::RETIRED_TEXT, o.id],
            )?;
        }
        store.conn().execute(
            "UPDATE bot_messages SET flags=json_set(json_remove(flags,'$.board_key','$.board_from','$.board_buttons'),'$.pinned',json('false')) WHERE id=?1",
            [o.id],
        )?;
    }
    set_flag(store, m.id, "$.board_retired", json!(true))?;
    store.changed();
    Ok(())
}

/// Records a `goal_pin` bot event: `action` on goal `goal`'s pin `msg`.
fn goal_pin_event(store: &dyn Store, name: &str, msg: i64, goal: &str, action: &str) -> Result<()> {
    let conn = store.conn();
    bot::event(
        &conn,
        bot_id(&conn, name)?,
        &Actor::new("telegram"),
        "goal_pin",
        &json!({"message": msg, "goal": goal, "action": action}).to_string(),
    )?;
    drop(conn);
    store.changed();
    Ok(())
}

/// Stops pin `m` standing for its goal: unpins it and clears `goal_pin`.
fn drop_goal_pin(b: &TgBot, store: &dyn Store, m: &StoredMessage) -> Result<()> {
    if m.flags["pinned"] == true && m.flags["deleted"] != true && b.allowed(m.chat_id).is_ok() {
        let p = json!({"chat_id": m.chat_id, "message_id": m.message_id});
        if let Err(e) = b.paced(m.chat_id, || b.api.call("unpinChatMessage", &p)) {
            skip_refused(b, store, "unpin goal", m.id, e)?;
        }
    }
    store.conn().execute(
        "UPDATE bot_messages SET flags=json_set(json_remove(flags,'$.goal_pin','$.goal_pin_key'),'$.pinned',json('false'),'$.goal_unpinned',json(?1)) WHERE id=?2",
        params![m.flags["goal_pin"].to_string(), m.id],
    )?;
    Ok(())
}

/// One pinned message per active goal of the bot (`goal_board::pin_text`):
/// a goal without one gets it, sent silently in the board's chat and
/// pinned silently; a pin whose text changed is edited in place; the pin of
/// a goal that is no longer active (done, dropped, paused), or no longer
/// the bot's, is unpinned. A pin Telegram refuses (400) is logged and not
/// retried (`flags.pin_skipped`); a refused edit (the message is gone)
/// drops the pin, so the next pass posts a new one.
fn goal_pins(b: &TgBot, store: &dyn Store, chat: i64, thread: Option<i64>) -> Result<()> {
    let conn = store.conn();
    let goals = goal_board::pinned_lines(&conn, &b.name)?;
    let pins = goal_board::pins(&conn, &b.name)?;
    drop(conn);
    let mut kept: HashMap<i64, StoredMessage> = HashMap::new();
    for m in pins {
        let gid = m.flags["goal_pin"].as_i64().unwrap_or(0);
        let wanted = goals.iter().find(|g| g.id == gid);
        if wanted.is_none() || m.flags["deleted"] == true || kept.contains_key(&gid) {
            drop_goal_pin(b, store, &m)?;
            let name: String = store
                .conn()
                .query_row("SELECT name FROM goals WHERE id=?1", [gid], |r| r.get(0))
                .optional()?
                .unwrap_or_else(|| gid.to_string());
            goal_pin_event(store, &b.name, m.id, &name, "unpinned")?;
            continue;
        }
        kept.insert(gid, m);
    }
    for g in &goals {
        let text = goal_board::pin_text(g);
        let m = match kept.remove(&g.id) {
            Some(m) => m,
            None => {
                b.allowed(chat)?;
                let mut p = json!({"chat_id": chat, "text": text, "parse_mode": "HTML",
                    "disable_notification": true});
                if let Some(th) = thread {
                    p["message_thread_id"] = json!(th);
                }
                let sent = b.paced(chat, || b.api.call("sendMessage", &p))?;
                let id = record_out(
                    store,
                    &b.name,
                    &sent,
                    "text",
                    &text,
                    None,
                    json!([]),
                    json!({"html": true, "goal_pin": g.id, "goal_pin_key": text}),
                )?;
                goal_pin_event(store, &b.name, id, &g.name, "posted")?;
                get_message(&store.conn(), &b.name, id)?
            }
        };
        if m.flags["goal_pin_key"].as_str() != Some(text.as_str()) {
            if b.allowed(m.chat_id).is_err() {
                continue;
            }
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id, "text": text,
                "parse_mode": "HTML"});
            match b.paced(m.chat_id, || b.api.call("editMessageText", &p)) {
                Ok(_) => {}
                // The text already matches (for example after a lost reply).
                Err(e) if e.description.contains("message is not modified") => {}
                Err(e) if e.code == Some(400) => {
                    skip_refused(b, store, "edit goal", m.id, e)?;
                    drop_goal_pin(b, store, &m)?;
                    goal_pin_event(store, &b.name, m.id, &g.name, "unpinned")?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            store.conn().execute(
                "UPDATE bot_messages SET text=?1, flags=json_set(flags,'$.goal_pin_key',?1,'$.edited',json('true')) WHERE id=?2",
                params![text, m.id],
            )?;
            goal_pin_event(store, &b.name, m.id, &g.name, "edited")?;
        }
        if m.flags["pinned"] != true && m.flags["pin_skipped"] != true {
            if b.allowed(m.chat_id).is_err() {
                continue;
            }
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id,
                "disable_notification": true});
            match b.paced(m.chat_id, || b.api.call("pinChatMessage", &p)) {
                Ok(_) => set_flag(store, m.id, "$.pinned", json!(true))?,
                Err(e) => {
                    skip_refused(b, store, "pin goal", m.id, e)?;
                    set_flag(store, m.id, "$.pin_skipped", json!(true))?;
                }
            }
            store.changed();
        }
    }
    Ok(())
}

fn set_flag(store: &dyn Store, msg: i64, path: &str, v: Value) -> Result<()> {
    store.conn().execute(
        "UPDATE bot_messages SET flags=json_set(flags,?1,json(?2)) WHERE id=?3",
        params![path, v.to_string(), msg],
    )?;
    Ok(())
}

/// (chat, thread) of the messages whose wakes the bot's live run took.
fn live_chats(conn: &Connection, name: &str) -> Result<Vec<(i64, Option<i64>)>> {
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

/// The inline keyboard: `rows` buttons per row (checked by
/// [`check_buttons`]), or one row when `rows` is empty. No buttons is an
/// empty keyboard, which removes an existing one.
fn keyboard(buttons: &[(String, String)], rows: &[usize]) -> Value {
    let all = [buttons.len()];
    let sizes = if rows.is_empty() { &all[..] } else { rows };
    let mut rest = buttons;
    let mut keyboard = Vec::new();
    for &n in sizes {
        let (row, tail) = rest.split_at(n.min(rest.len()));
        rest = tail;
        if !row.is_empty() {
            let row: Vec<Value> = row
                .iter()
                .map(|(l, d)| json!({"text":l,"callback_data":d}))
                .collect();
            keyboard.push(row);
        }
    }
    json!({ "inline_keyboard": keyboard })
}

fn button_label(flags: &Value, data: &str) -> String {
    flags["buttons"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b[1] == data)
        .and_then(|b| b[0].as_str())
        .unwrap_or(data)
        .to_string()
}

/// At most `limit` characters.
fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut t: String = s.chars().take(limit - 1).collect();
    t.push('…');
    t
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `/cmd[@bot] [arg]` when it is an owner command for this bot (`me` is its
/// username from `getMe`; a command addressed to another bot is not ours).
fn command<'a>(text: &'a str, me: Option<&str>) -> Option<(&'static str, &'a str)> {
    let rest = text.strip_prefix('/')?;
    let (head, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (cmd, to) = match head.split_once('@') {
        Some((c, to)) => (c, Some(to)),
        None => (head, None),
    };
    if to.is_some_and(|to| me.is_none_or(|me| !me.eq_ignore_ascii_case(to))) {
        return None;
    }
    OWNER_COMMANDS
        .iter()
        .find(|c| **c == cmd)
        .map(|c| (*c, arg.trim()))
}

fn status(conn: &Connection, name: &str) -> Result<String> {
    let b = bot::get(conn, name)?;
    let mut s = format!("bot {} ({}): {}", b.name, b.project, b.state);
    if b.attention {
        s.push_str(&format!(
            " — needs attention after {} failed runs",
            b.failures
        ));
    }
    match b.live_run {
        Some(r) => {
            let run = bot::get_run(conn, r)?;
            s.push_str(&format!(
                "\nlive run {r} since {}: {}",
                run.started_at,
                run.current_activity.as_deref().unwrap_or("starting")
            ));
        }
        None => s.push_str("\nno live run"),
    }
    s.push_str(&format!(
        "\nnext wake: {}\nqueued wakes: {}\nlast run ended: {}",
        b.next_wake.as_deref().unwrap_or("-"),
        b.queued,
        b.last_run_end.as_deref().unwrap_or("-")
    ));
    Ok(s)
}

/// Records a sent message (the `Message` the Bot API returned).
#[allow(clippy::too_many_arguments)]
fn record_out(
    store: &dyn Store,
    name: &str,
    sent: &Value,
    kind: &str,
    text: &str,
    reply_to: Option<i64>,
    attachments: Value,
    flags: Value,
) -> Result<i64> {
    let (id, _) = bot::record_message(
        &mut store.conn(),
        name,
        &NewMessage {
            direction: "out".into(),
            chat_id: sent["chat"]["id"]
                .as_i64()
                .context("sent message without chat")?,
            thread_id: sent["message_thread_id"].as_i64(),
            message_id: sent["message_id"]
                .as_i64()
                .context("sent message without message_id")?,
            reply_to,
            from_id: sent["from"]["id"].as_i64(),
            kind: kind.into(),
            text: text.into(),
            attachments,
            flags,
            ..Default::default()
        },
        now(),
    )?;
    store.changed();
    Ok(id)
}

impl TgBot {
    /// The outbound allowlist.
    fn allowed(&self, chat: i64) -> Result<()> {
        if !self.chats.contains(&chat) {
            return Err(Forbidden(format!(
                "chat {chat} is not in bot {}'s Telegram allowlist",
                self.name
            ))
            .into());
        }
        Ok(())
    }

    /// `sendChatAction typing`, best effort.
    fn typing(&self, chat: i64, thread: Option<i64>) {
        let mut p = json!({"chat_id": chat, "action": "typing"});
        if let Some(t) = thread {
            p["message_thread_id"] = json!(t);
        }
        if let Err(e) = self.api.call("sendChatAction", &p) {
            eprintln!("telegram: bot {}: {e}", self.name);
        }
    }

    /// Runs one outbound call through the chat's queue: calls are serialized
    /// per chat, spaced by [`SEND_INTERVAL`], and retried after a 429's
    /// `retry_after`.
    fn paced<T>(
        &self,
        chat: i64,
        mut f: impl FnMut() -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let slot = Arc::clone(
            self.queues
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(chat)
                .or_insert_with(|| Arc::new(Mutex::new(Instant::now()))),
        );
        let mut next = slot.lock().unwrap_or_else(|e| e.into_inner());
        let mut attempt = 0;
        loop {
            let wait = next.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            attempt += 1;
            match f() {
                Ok(v) => {
                    *next = Instant::now() + SEND_INTERVAL;
                    return Ok(v);
                }
                Err(e) if e.code == Some(429) && attempt < SEND_ATTEMPTS => {
                    let secs = e.retry_after.unwrap_or(1).clamp(1, MAX_RETRY_AFTER);
                    *next = Instant::now() + Duration::from_secs(secs);
                }
                Err(e) => return Err(e),
            }
        }
    }
}
