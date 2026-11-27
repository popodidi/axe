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
use serde_json::{json, Value};

use crate::ask;
use crate::board;
use crate::bot::{self, NewMessage};
use crate::config;
pub use crate::core::Axe;
use crate::db::{Actor, Forbidden};
use crate::goal_board;

pub use crate::channel::{
    check_buttons, check_upload, get_message, history, pct_encode, safe_name, SendOpts,
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
/// Wait before the first retry of a transient send failure
/// ([`ApiError::transient`]); it doubles for each later attempt.
const SEND_RETRY: Duration = Duration::from_secs(1);
/// Most messages one `send` may split into (sends are about 1 s apart).
pub const MAX_PARTS: usize = 20;
const MAX_RETRY_AFTER: u64 = 60;
/// At most one security alert to the owner per interval.
const ALERT_INTERVAL: Duration = Duration::from_secs(60);
/// Attempts at one update before it is skipped (recorded as an event).
const UPDATE_ATTEMPTS: u32 = 3;
/// The owner commands [`command`] routes, with the description each bot's
/// `/` menu shows ([`Gateway::publish_commands`]). `BotCommand`: the command
/// is 1-32 lowercase letters, digits and underscores; the description 1-256
/// characters.
pub const OWNER_COMMANDS: [(&str, &str); 14] = [
    (
        "status",
        "This bot: state, live run, wakes, services, schedules",
    ),
    (
        "node",
        "The machine: CPU, memory, disk, runs by kind, versions",
    ),
    ("tasks", "This bot's open tasks"),
    (
        "task",
        "One task: state, outcome and latest notes (/task N)",
    ),
    ("cost", "This bot's spend over the last 7 days"),
    ("goals", "This bot's goals and budgets"),
    ("services", "This bot's services and their state"),
    ("routines", "Routines this bot owns, next fire"),
    ("wake", "Queue a wake, with an optional note"),
    ("stop", "Stop the live run; queued wakes are kept"),
    ("pause", "Pause: no new wakes, stop the live run"),
    ("resume", "Resume a paused bot"),
    ("reset", "Clear the session; the next run starts fresh"),
    ("runtimes", "Agent runtimes and their logins"),
];
/// Callback data of the button on a runtime card.
const RUNTIMES_BUTTON: &str = "runtimes";
/// Callback data prefix of the runtime login buttons:
/// `rtlogin:<runtime>` opens the method menu, `rtlogin:<runtime>:<method>`
/// starts that login.
const LOGIN_BUTTON: &str = "rtlogin:";
/// `rtrm:<runtime>` asks to confirm removing a runtime; `rtrm!:<runtime>`
/// removes it and `rtrm-` cancels.
const REMOVE_BUTTON: &str = "rtrm:";
const REMOVE_CONFIRM: &str = "rtrm!:";
const REMOVE_CANCEL: &str = "rtrm-";
/// `rtadd:<harness>` starts a login of a new runtime of that harness.
const ADD_BUTTON: &str = "rtadd:";
/// `rtpri:<runtime>` makes the runtime the chat bot's primary;
/// `rtbak:<runtime>` its backup, and `rtbak-` clears its backup.
const PRIMARY_BUTTON: &str = "rtpri:";
const BACKUP_BUTTON: &str = "rtbak:";
const NO_BACKUP: &str = "rtbak-";
/// What `/runtimes` says when no runtime is logged in yet.
const NO_RUNTIME: &str = "Log in Claude or Codex to start me.";

/// A press on a `/runtimes` button, by its callback data.
#[derive(Debug, PartialEq)]
enum RuntimePress<'a> {
    /// A runtime card's button: the list.
    List,
    /// Log in or Re-login: the runtime's login methods.
    Menu(&'a str),
    Login(&'a str, &'a str),
    Remove(&'a str),
    Confirm(&'a str),
    Cancel,
    Add(&'a str),
    Primary(&'a str),
    Backup(&'a str),
    NoBackup,
}

impl<'a> RuntimePress<'a> {
    fn parse(data: &'a str) -> Option<RuntimePress<'a>> {
        if data == RUNTIMES_BUTTON {
            return Some(RuntimePress::List);
        }
        if data == REMOVE_CANCEL {
            return Some(RuntimePress::Cancel);
        }
        if data == NO_BACKUP {
            return Some(RuntimePress::NoBackup);
        }
        if let Some(n) = data.strip_prefix(PRIMARY_BUTTON) {
            return Some(RuntimePress::Primary(n));
        }
        if let Some(n) = data.strip_prefix(BACKUP_BUTTON) {
            return Some(RuntimePress::Backup(n));
        }
        if let Some(l) = data.strip_prefix(LOGIN_BUTTON) {
            return Some(match l.split_once(':') {
                Some((name, method)) => RuntimePress::Login(name, method),
                None => RuntimePress::Menu(l),
            });
        }
        if let Some(n) = data.strip_prefix(REMOVE_CONFIRM) {
            return Some(RuntimePress::Confirm(n));
        }
        if let Some(n) = data.strip_prefix(REMOVE_BUTTON) {
            return Some(RuntimePress::Remove(n));
        }
        let h = data.strip_prefix(ADD_BUTTON)?;
        crate::harness::known(h).then_some(RuntimePress::Add(h))
    }
}

/// Runtimes a [`crate::runtime::view`] shows: not those still waiting
/// for their first login (a runtime exists once its login works).
fn shown(view: &Value) -> Vec<Value> {
    view["runtimes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["pending"] != true)
        .collect()
}

/// The names of chat bot `me`'s primary and backup runtimes in `view`.
fn roles(view: &Value, me: Option<&bot::Bot>) -> (Option<String>, Option<String>) {
    let list = view["runtimes"].as_array().cloned().unwrap_or_default();
    let name = |n: Option<&str>| {
        n.and_then(|n| crate::runtime::resolve_listed(&list, n))
            .and_then(|r| r["name"].as_str().map(String::from))
    };
    match me {
        Some(b) => (name(Some(&b.agent)), name(b.backup.as_deref())),
        None => (None, None),
    }
}

/// A harness for people: `Claude`, `Codex`.
fn harness_name(h: &str) -> &str {
    crate::harness::get(h).map_or(h, |x| x.display)
}

/// A runtime's login state in plain words, with a limit's end.
fn plain_state(r: &Value) -> String {
    let until = r["until"].as_i64().map(bot::ts);
    match r["state"].as_str().unwrap_or("?") {
        crate::runtime::NEEDS_LOGIN if r["never_logged_in"] == true => "Not logged in".into(),
        crate::runtime::NEEDS_LOGIN => "Login expired".into(),
        crate::runtime::LIMITED => match until {
            Some(u) => format!("Logged in, out of quota until {u}"),
            None => "Logged in, out of quota".into(),
        },
        crate::runtime::DEGRADED => match until {
            Some(u) => format!("Logged in, failing until {u}"),
            None => "Logged in, failing".into(),
        },
        crate::runtime::DISABLED => "Disabled".into(),
        _ => "Logged in".into(),
    }
}

/// The `/runtimes` text: one line per runtime with its login state and
/// uses, or [`NO_RUNTIME`] on a node that manages its runtimes and has
/// none logged in.
fn list_text(view: &Value) -> String {
    let list = shown(view);
    if list.is_empty() && view["managed"] == true && view["report_age_seconds"].is_u64() {
        return NO_RUNTIME.into();
    }
    if list.is_empty() {
        return crate::runtime::render(view);
    }
    let mut out = vec!["Runtimes".to_string()];
    for r in &list {
        let uses: Vec<&str> = r["uses"]
            .as_array()
            .map(|u| u.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut line = format!(
            "{} ({}): {}",
            r["name"].as_str().unwrap_or("?"),
            harness_name(r["harness"].as_str().unwrap_or("?")),
            plain_state(r)
        );
        match uses.is_empty() {
            true => line.push_str(". Not used"),
            false => line.push_str(&format!(". {}", capitalize(&uses.join("; ")))),
        }
        out.push(line);
    }
    out.push(String::new());
    out.push(
        "Primary runs this bot. Backup runs it only while the primary is down, in a fresh \
         session; it goes back to the primary once that works. Notes are kept."
            .into(),
    );
    if view["stale"] == true {
        out.push(format!(
            "(stale: the node last reported {}s ago)",
            view["report_age_seconds"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(fp) = view["key"]["fingerprint"].as_str() {
        out.push(format!(
            "dispatcher key {fp} (sealed forms show it; compare once)"
        ));
    }
    out.join("\n")
}

/// The `/runtimes` list's keyboard on a node that manages its runtimes,
/// for chat bot `me`: per runtime (one row each) Log in when its login
/// expired, Primary and Backup (or No backup) for `me`, and Remove; then
/// one login button per harness for a new runtime.
fn list_buttons(view: &Value, me: Option<&bot::Bot>) -> (Vec<(String, String)>, Vec<usize>) {
    if view["managed"] != true {
        return (vec![], vec![]);
    }
    let (primary, backup) = roles(view, me);
    let list = shown(view);
    let mut buttons = Vec::new();
    let mut rows = Vec::new();
    for r in &list {
        let Some(n) = r["name"].as_str() else {
            continue;
        };
        let mut row = vec![];
        let can_login =
            !crate::runtime::login_methods(r["harness"].as_str().unwrap_or("")).is_empty();
        if can_login && r["state"] == crate::runtime::NEEDS_LOGIN {
            row.push((format!("Log in {n}"), format!("{LOGIN_BUTTON}{n}")));
        }
        if me.is_some() {
            let is_primary = primary.as_deref() == Some(n);
            if !is_primary {
                row.push((format!("Primary {n}"), format!("{PRIMARY_BUTTON}{n}")));
            }
            if backup.as_deref() == Some(n) {
                row.push(("No backup".into(), NO_BACKUP.into()));
            } else if !is_primary {
                row.push((format!("Backup {n}"), format!("{BACKUP_BUTTON}{n}")));
            }
        }
        row.push((format!("Remove {n}"), format!("{REMOVE_BUTTON}{n}")));
        row.retain(|(_, d)| d.len() <= CALLBACK_DATA_LIMIT);
        if !row.is_empty() {
            rows.push(row.len());
            buttons.extend(row);
        }
    }
    for h in crate::harness::HARNESSES {
        let label = match list.is_empty() {
            true => format!("Log in {}", h.display),
            false => format!("Add {}", h.display),
        };
        buttons.push((label, format!("{ADD_BUTTON}{}", h.name)));
    }
    rows.push(crate::harness::HARNESSES.len());
    (buttons, rows)
}

/// A runtime's login method menu: (text, one button per method). `added`:
/// the runtime was just added for that harness.
fn login_menu(name: &str, harness: &str, added: Option<&str>) -> (String, Vec<(String, String)>) {
    let buttons: Vec<(String, String)> = crate::runtime::login_methods(harness)
        .iter()
        .map(|(m, _, label)| (capitalize(label), format!("{LOGIN_BUTTON}{name}:{m}")))
        .filter(|(_, d)| d.len() <= CALLBACK_DATA_LIMIT)
        .collect();
    let head = match added {
        Some(h) => format!(
            "Log in {} for a new runtime ({name}, its own login). It is added once the login \
             works; nothing is added if it fails. ",
            harness_name(h)
        ),
        None => String::new(),
    };
    let text = if buttons.is_empty() {
        format!("{head}Runtime {name} cannot be logged in from here.")
    } else {
        format!(
            "{head}Log in runtime {name} ({harness}). A credential goes through a sealed \
             form, so only the node reads it."
        )
    };
    (text, buttons)
}
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
    /// No HTTP response arrived: the connection failed, was reset or timed
    /// out ([`Api::finish`]).
    pub network: bool,
}

impl ApiError {
    /// Worth sending again: no response arrived (a network error) or the
    /// server answered 5xx. Any other answer, including one whose body could
    /// not be read, is final, so a retried send never repeats a call Telegram
    /// acknowledged.
    pub fn transient(&self) -> bool {
        self.network || self.code.is_some_and(|c| (500..600).contains(&c))
    }
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
            network: false,
        };
        let body: Value = match r {
            Ok(resp) => resp.into_json().map_err(|e| {
                err(
                    None,
                    format!("bad response: {}", self.clean(&e.to_string())),
                    None,
                )
            })?,
            Err(ureq::Error::Status(code, resp)) => {
                let mut body = resp
                    .into_json()
                    .unwrap_or_else(|_| json!({"ok":false,"error_code":code}));
                // A 5xx is transient whatever its body says.
                if code >= 500 {
                    body["error_code"] = json!(code);
                }
                body
            }
            Err(ureq::Error::Transport(t)) => {
                use ureq::ErrorKind::{ConnectionFailed, Dns, Io};
                return Err(ApiError {
                    network: matches!(t.kind(), Dns | ConnectionFailed | Io),
                    ..err(None, self.clean(&t.to_string()), None)
                });
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
            network: false,
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

impl TgBot {
    /// The axe bot this Telegram bot talks for.
    pub fn name(&self) -> String {
        self.bound.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// One configured Telegram bot.
pub struct TgBot {
    /// Its `[[telegram.bot]] name` in the server config.
    config: String,
    /// The axe bot it talks for: the config name, or the bot's current name
    /// after a rename ([`Gateway::bind`], [`Gateway::renamed`]).
    bound: std::sync::RwLock<String>,
    pub chats: Vec<i64>,
    api: Api,
    /// Per-chat outbound pacing: the earliest next send.
    queues: Mutex<HashMap<i64, Arc<Mutex<Instant>>>>,
    alerts: Mutex<Alerts>,
    /// `getMe` username, for `/cmd@username`.
    username: Mutex<Option<String>>,
    /// `getMe` user id, to recognise the bot's own service messages.
    id: Mutex<Option<i64>>,
    /// The server's public URL, for the goal pins' links to the web goal page.
    web: Option<String>,
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
    /// The bot that sends runtime cards (`telegram.ops_bot`).
    ops: Option<String>,
}

/// How often the retro send checks whether a bot's slot came.
const RETRO_TICK: Duration = Duration::from_secs(10);

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

/// Bounds a `getUpdates` outage: one log line, plus a `poll_recovered` bot
/// event when it outlasted two long-poll windows (`min_secs`), long enough for
/// the owner to notice. Updates sent meanwhile arrive now: the offset never
/// advanced.
fn poll_recovered(b: &TgBot, store: &dyn Axe, since: i64, polls: u32, min_secs: u64) {
    let secs = now() - since;
    eprintln!(
        "telegram: bot {}: getUpdates recovered after {polls} failed polls over {secs}s (since {})",
        b.name(),
        bot::ts(since)
    );
    if secs < min_secs as i64 {
        return;
    }
    let body = json!({"failed_polls": polls, "since": bot::ts(since), "seconds": secs});
    if let Err(e) = store.bot_event(&b.name(), &Actor::new("telegram"), "poll_recovered", &body) {
        eprintln!("telegram: bot {}: poll_recovered event: {e:#}", b.name());
    }
}

fn owner_actor() -> Actor {
    Actor::new("owner")
}

impl Gateway {
    /// Reads each bot's token file; a bot whose token cannot be read is skipped
    /// with a warning, so the server still starts. `web` is the server's
    /// public URL (`[blobs] public_url`), which goal pins link to.
    pub fn new(cfg: &config::Telegram, web: Option<String>) -> Gateway {
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
                    config: b.name.clone(),
                    bound: std::sync::RwLock::new(b.name.clone()),
                    chats: b.chats.clone(),
                    api: Api::new(&cfg.api_base, &token, cfg.poll_timeout),
                    queues: Mutex::new(HashMap::new()),
                    alerts: Mutex::new(Alerts::default()),
                    username: Mutex::new(None),
                    id: Mutex::new(None),
                    web: web.clone(),
                }),
            );
        }
        for w in cfg.warnings() {
            eprintln!("telegram: {w}");
        }
        let ops = cfg
            .ops_bot
            .clone()
            .or(cfg.bots.first().map(|b| b.name.clone()))
            .filter(|n| bots.contains_key(n));
        Gateway {
            owner: cfg.owner_user_id,
            poll_timeout: cfg.poll_timeout,
            bots,
            ops,
        }
    }

    /// The Telegram bot that talks for axe bot `name`; also found by its
    /// config name, which a renamed bot's config may still hold.
    pub fn bot(&self, name: &str) -> Result<&Arc<TgBot>> {
        self.bots
            .values()
            .find(|b| b.name() == name)
            .or_else(|| self.bots.get(name))
            .with_context(|| format!("telegram is not configured for bot {name}"))
    }

    /// The axe bots the gateway talks for.
    pub fn names(&self) -> Vec<String> {
        let mut n: Vec<String> = self.bots.values().map(|b| b.name()).collect();
        n.sort();
        n
    }

    /// Binds each configured bot to the axe bot its config name stands for
    /// (`bot::bound_name`): a bot renamed since the config was written keeps
    /// its Telegram bot until the config names it anew.
    pub fn bind(&self, store: &dyn Axe) {
        for b in self.bots.values() {
            match store.bound_name(&b.config) {
                Ok(Some(name)) if name != b.config => {
                    eprintln!(
                        "telegram: bot {}: bound to axe bot {name} (renamed); the config may name it {name}",
                        b.config
                    );
                    *b.bound.write().unwrap_or_else(|e| e.into_inner()) = name;
                }
                Ok(_) => {}
                Err(e) => eprintln!("telegram: bot {}: bind: {e:#}", b.config),
            }
        }
    }

    /// Re-binds the Telegram bot of axe bot `old`, just renamed to `new`.
    /// Returns its config name, if the gateway has one for it.
    pub fn renamed(&self, old: &str, new: &str) -> Option<String> {
        let b = self.bots.values().find(|b| b.name() == old)?;
        *b.bound.write().unwrap_or_else(|e| e.into_inner()) = new.to_string();
        Some(b.config.clone())
    }

    /// Starts one poller, one typing and one board thread per bot.
    pub fn start(self: &Arc<Self>, store: Arc<dyn Axe>) {
        for b in self.bots.values() {
            let (gw, b2, store2) = (Arc::clone(self), Arc::clone(b), Arc::clone(&store));
            std::thread::spawn(move || gw.poll(&b2, store2.as_ref()));
            let (b2, store2) = (Arc::clone(b), Arc::clone(&store));
            std::thread::spawn(move || board_loop(&b2, store2.as_ref()));
            let (b, store) = (Arc::clone(b), Arc::clone(&store));
            std::thread::spawn(move || typing_loop(&b, store.as_ref()));
        }
        let (gw, store) = (Arc::clone(self), Arc::clone(&store));
        std::thread::spawn(move || gw.retro_loop(store.as_ref()));
    }

    /// Sends each bot's retro card at its own slots (`bots.retro`, read
    /// from the store on every tick), once per slot and bot: not when that
    /// bot's retro is off, not when its retro card already went out since
    /// the slot (by hand or before a restart), and not at all when it has
    /// no unrated asks from the last interval. A slot missed while the
    /// server was down is still sent up to `ask::RETRO_CATCH_UP` late.
    fn retro_loop(&self, store: &dyn Axe) {
        let mut tried: HashMap<String, i64> = HashMap::new();
        while !store.stopping() {
            for name in self.names() {
                let mut t = tried.get(&name).copied();
                if let Err(e) = self.retro_tick(store, &name, &mut t) {
                    eprintln!("telegram: bot {name}: retro: {e:#}");
                }
                if let Some(t) = t {
                    tried.insert(name, t);
                }
            }
            let until = Instant::now() + RETRO_TICK;
            while !store.stopping() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    fn retro_tick(&self, store: &dyn Axe, name: &str, tried: &mut Option<i64>) -> Result<()> {
        // The bot exists once it was created; until then, try again.
        let Some(cfg) = store.get_bot(name).ok().map(|b| b.retro) else {
            return Ok(());
        };
        let cfg = cfg.unwrap_or_default();
        if !cfg.on {
            return Ok(());
        }
        let (at, every) = (cfg.offset(), cfg.period());
        let now = now();
        let Some(slot) = ask::retro_due(now, at, every, None, *tried) else {
            return Ok(());
        };
        // The card before this slot, left open with ratings on it: its
        // bot gets the wake now, once per card. A failure here does not
        // hold up the new card.
        if let Err(e) = self.retro_wake_open(store, name, slot) {
            eprintln!("telegram: bot {name}: retro wake: {e:#}");
        }
        let last = store.last_retro(name)?;
        if ask::retro_due(now, at, every, last, *tried).is_none() {
            *tried = Some(slot);
            return Ok(());
        }
        // The owner's private chat when the bot may write there.
        let chat = self
            .bot(name)?
            .chats
            .contains(&self.owner)
            .then_some(self.owner);
        let v = self.send_retro(
            store,
            name,
            &SendOpts {
                chat,
                ..SendOpts::default()
            },
            None,
        )?;
        *tried = Some(slot);
        if v["sent"] == true {
            eprintln!(
                "telegram: bot {name}: retro sent #{} ({} asks)",
                v["id"], v["items"]
            );
        }
        Ok(())
    }

    fn poll(&self, b: &TgBot, store: &dyn Axe) {
        let mut offset: Option<i64> = None;
        let mut started = false;
        let mut last_attention = 0i64;
        let mut failures: HashMap<i64, u32> = HashMap::new();
        // (first failure, failed polls) since the last successful getUpdates.
        let mut outage: Option<(i64, u32)> = None;
        while !store.stopping() {
            let id = store.bot_id(&b.name()).ok();
            let Some(id) = id else {
                // Updates wait up to 24 h on Telegram until the bot exists.
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            if !started {
                self.startup(b);
                last_attention = store.last_bot_event(id);
                started = true;
            }
            if let Err(e) = self.attention_alerts(b, store, id, &mut last_attention) {
                eprintln!("telegram: bot {}: attention alerts: {e:#}", b.name());
            }
            let mut params =
                json!({"timeout": self.poll_timeout, "allowed_updates": ALLOWED_UPDATES});
            if let Some(o) = offset {
                params["offset"] = json!(o);
            }
            let updates = match b.api.call("getUpdates", &params) {
                Ok(u) => {
                    if let Some((since, polls)) = outage.take() {
                        poll_recovered(b, store, since, polls, 2 * self.poll_timeout);
                    }
                    u
                }
                Err(e) => {
                    eprintln!("telegram: bot {}: {e}", b.name());
                    outage.get_or_insert((now(), 0)).1 += 1;
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
                            b.name()
                        );
                        if *n < UPDATE_ATTEMPTS {
                            // Not confirmed: Telegram redelivers it on the next poll.
                            std::thread::sleep(Duration::from_secs(1));
                            break;
                        }
                        let _ = store.bot_event(
                            &b.name(),
                            &Actor::new("telegram"),
                            "update_skipped",
                            &json!({"update": uid, "error": format!("{e:#}")}),
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
                        b.name(),
                        me["username"].as_str().unwrap_or("?")
                    );
                }
            }
            Err(e) => eprintln!("telegram: bot {}: {e}", b.name()),
        }
        self.publish_commands(b);
    }

    /// Sets the `/` menu to [`OWNER_COMMANDS`] where the owner can use them:
    /// the owner's private chat (a `chat` scope; its id is the owner's user
    /// id) and, for each allowlisted group, the owner there (`chat_member`).
    /// Nobody else gets a menu. Runs at every start, so a changed command set
    /// ships with the binary; `setMyCommands` replaces the list, so it is
    /// idempotent.
    fn publish_commands(&self, b: &TgBot) {
        let commands: Vec<Value> = OWNER_COMMANDS
            .iter()
            .map(|(c, d)| json!({"command": c, "description": d}))
            .collect();
        for scope in command_scopes(self.owner, &b.chats) {
            let params = json!({"commands": commands, "scope": scope});
            if let Err(e) = b.api.call("setMyCommands", &params) {
                eprintln!("telegram: bot {}: command menu for {scope}: {e}", b.name());
            }
        }
    }

    /// Alerts the owner about new `attention` events (bot paused by failures).
    fn attention_alerts(&self, b: &TgBot, store: &dyn Axe, id: i64, last: &mut i64) -> Result<()> {
        let events = store.attention_since(id, *last)?;
        if let Some((max, _)) = events.last() {
            *last = *max;
        }
        for (_, body) in events {
            self.notify_owner(b, store, &format!("axe bot {}: {body}", b.name()));
        }
        Ok(())
    }

    /// A server-originated message to the owner's private chat.
    fn notify_owner(&self, b: &TgBot, store: &dyn Axe, text: &str) {
        let params = json!({"chat_id": self.owner, "text": text});
        match b.paced(store, self.owner, || b.api.call("sendMessage", &params)) {
            Ok(sent) => {
                if let Err(e) = record_out(
                    store,
                    &b.name(),
                    &sent,
                    "text",
                    text,
                    None,
                    json!([]),
                    json!({"system": true}),
                ) {
                    eprintln!("telegram: bot {}: record alert: {e:#}", b.name());
                }
            }
            Err(e) => eprintln!("telegram: bot {}: alert: {e}", b.name()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn security_drop(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        update: i64,
        kind: &str,
        reason: &str,
        from: Option<i64>,
        chat: Option<i64>,
    ) -> Result<()> {
        store.bot_event(
            &b.name(),
            &Actor::new("telegram"),
            "security_drop",
            &json!({"update":update,"type":kind,"reason":reason,"from_id":from,"chat_id":chat}),
        )?;
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
                    b.name()
                ),
            );
        }
        Ok(())
    }

    /// Handles one update; returns after its effects are committed.
    pub fn handle_update(&self, b: &TgBot, store: &dyn Axe, u: &Value) -> Result<()> {
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

    fn inbound(&self, b: &TgBot, store: &dyn Axe, uid: i64, m: &Value, edit: bool) -> Result<()> {
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
        // A sealed form reply (Mini App data, pasted text or a `.sealed`
        // file) goes to the node; it is not stored and wakes no bot.
        if !edit {
            if let Some(data) = m["web_app_data"]["data"].as_str() {
                return self.form_reply(b, store, chat, tg_id, Ok(data.to_string()));
            }
            if plain && crate::sealed::Sealed::looks_like(&text) {
                return self.form_reply(b, store, chat, tg_id, Ok(text));
            }
            let doc = &m["document"];
            if m["forward_origin"].is_null()
                && doc["file_name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with(".sealed"))
            {
                let data = self.sealed_file(b, doc);
                return self.form_reply(b, store, chat, tg_id, data);
            }
        }
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
        let (dup, msg) = store.record_inbound(
            &b.name(),
            edit.then_some(tg_id),
            m["reply_to_message"]["message_id"].as_i64(),
            NewMessage {
                direction: "in".into(),
                chat_id: chat,
                thread_id: m["message_thread_id"].as_i64(),
                message_id,
                from_id: m["from"]["id"].as_i64(),
                kind,
                text,
                attachments: json!(files),
                media_group_id: m["media_group_id"].as_str().map(String::from),
                flags,
                ..Default::default()
            },
        )?;
        if !dup {
            // Acknowledge: the owner sees the message arrived.
            if !edit {
                let r = b.api.call(
                    "setMessageReaction",
                    &json!({"chat_id":chat,"message_id":tg_id,"reaction":[{"type":"emoji","emoji":ACK_EMOJI}]}),
                );
                if let Err(e) = r {
                    eprintln!("telegram: bot {}: ack #{msg}: {e}", b.name());
                }
            }
            b.typing(chat, m["message_thread_id"].as_i64());
        }
        Ok(())
    }

    fn button(&self, b: &TgBot, store: &dyn Axe, uid: i64, cq: &Value) -> Result<()> {
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
        if let Some(t) = tg_msg {
            if self.retro_press(b, store, chat, t, &data, cq["id"].as_str())? {
                return Ok(());
            }
            if self.runtimes_press(b, store, chat, t, &data, cq["id"].as_str())? {
                return Ok(());
            }
        }
        let label_of = |flags: &Value| button_label(flags, &data);
        let (target, answered) = store.record_press(
            &b.name(),
            tg_msg,
            NewMessage {
                direction: "in".into(),
                chat_id: chat,
                thread_id: thread,
                message_id: uid,
                from_id: cq["from"]["id"].as_i64(),
                kind: "button".into(),
                text: data.clone(),
                ..Default::default()
            },
            &data,
            &label_of,
        )?;
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
                eprintln!("telegram: bot {}: {e}", b.name());
            }
        }
        if answered {
            return Ok(());
        }
        if let Some(t) = tg_msg {
            self.mark_selected(b, store, chat, t, target.as_ref(), &label);
        }
        b.typing(chat, thread);
        Ok(())
    }

    /// A press on a runtime card's or `/runtimes` list's button: replies
    /// in place of waking a bot. False when the message is not ours.
    fn runtimes_press(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: i64,
        tg_msg: i64,
        data: &str,
        query: Option<&str>,
    ) -> Result<bool> {
        let Some(press) = RuntimePress::parse(data) else {
            return Ok(false);
        };
        let stored = store.message_at(&b.name(), chat, tg_msg)?;
        let flags = stored.map(|m| m.flags).unwrap_or_default();
        let ours = match &press {
            RuntimePress::List => flags["runtime_alert"] == true,
            RuntimePress::Menu(_) | RuntimePress::Login(..) => {
                flags["runtimes_list"] == true || flags["runtime_menu"] == true
            }
            RuntimePress::Remove(_)
            | RuntimePress::Add(_)
            | RuntimePress::Primary(_)
            | RuntimePress::Backup(_)
            | RuntimePress::NoBackup => flags["runtimes_list"] == true,
            RuntimePress::Confirm(_) | RuntimePress::Cancel => flags["runtime_remove"] == true,
        };
        if !ours {
            return Ok(false);
        }
        if let Some(qid) = query {
            if let Err(e) = b
                .api
                .call("answerCallbackQuery", &json!({"callback_query_id": qid}))
            {
                eprintln!("telegram: bot {}: {e}", b.name());
            }
        }
        let say = |text: &str, buttons: &[(String, String)], extra: Value| {
            let rows = vec![1; buttons.len()];
            self.system_say(b, store, chat, tg_msg, text, buttons, &rows, extra);
        };
        let reported = |name: &str| {
            store.runtimes()["runtimes"]
                .as_array()
                .and_then(|l| l.iter().find(|r| r["name"] == name).cloned())
        };
        match press {
            RuntimePress::List => self.runtimes_reply(b, store, chat, Some(tg_msg), None),
            RuntimePress::Menu(name) => {
                let harness = reported(name)
                    .and_then(|r| r["harness"].as_str().map(String::from))
                    .unwrap_or_default();
                let (text, buttons) = login_menu(name, &harness, None);
                say(&text, &buttons, json!({"runtime_menu": true}));
            }
            RuntimePress::Login(name, method) => {
                let text = store
                    .runtime_login(&b.name(), name, method)
                    .unwrap_or_else(|e| format!("Login of runtime {name} not started: {e:#}"));
                say(&text, &[], json!({}));
            }
            // A bot with no runtime yet starts on its first login (the action).
            RuntimePress::Add(harness) => {
                match store.runtime_add(&owner_actor(), harness, Some(&b.name())) {
                    Ok(name) => {
                        let (text, buttons) = login_menu(&name, harness, Some(harness));
                        say(&text, &buttons, json!({"runtime_menu": true}));
                    }
                    Err(e) => say(
                        &format!("No {harness} runtime added: {e:#}"),
                        &[],
                        json!({}),
                    ),
                }
            }
            RuntimePress::Remove(name) => {
                let Some(r) = reported(name) else {
                    say(
                        &format!("The node reports no runtime {name}."),
                        &[],
                        json!({}),
                    );
                    return Ok(true);
                };
                // The node refuses a runtime with live runs (`Registry::remove`).
                let harness = r["harness"].as_str().unwrap_or("?");
                let mut text = format!(
                    "Remove runtime {name} ({harness})? No run goes to it afterwards; a \
                     session that ran on it starts fresh elsewhere. Its stored credentials \
                     are deleted, and its login directory too when it has its own; the \
                     CLI's default login is left in place."
                );
                let primaries: Vec<&str> = r["uses"]
                    .as_array()
                    .map(|u| u.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|u| u.strip_prefix("primary of "))
                    .collect();
                if !primaries.is_empty() {
                    text.push_str(&format!(
                        " It is the primary of {}: until you pick another primary, those bots \
                         run only on their backup.",
                        primaries.join(", ")
                    ));
                }
                let buttons = vec![
                    (format!("Remove {name}"), format!("{REMOVE_CONFIRM}{name}")),
                    ("Cancel".to_string(), REMOVE_CANCEL.to_string()),
                ];
                let buttons: Vec<_> = buttons
                    .into_iter()
                    .filter(|(_, d)| d.len() <= CALLBACK_DATA_LIMIT)
                    .collect();
                say(
                    &text,
                    &buttons,
                    json!({"runtime_remove": true, "runtime": name}),
                );
            }
            RuntimePress::Confirm(name) => {
                self.clear_buttons(b, chat, tg_msg);
                let text = match store.runtime_remove(name) {
                    Ok(note) if note.is_empty() => format!("Runtime {name} removed."),
                    Ok(note) => format!("Runtime {name} removed: {note}."),
                    Err(e) => format!("Runtime {name} not removed: {e:#}"),
                };
                say(&text, &[], json!({}));
            }
            RuntimePress::Cancel => {
                self.clear_buttons(b, chat, tg_msg);
                say("Nothing removed.", &[], json!({}));
            }
            RuntimePress::Primary(name) | RuntimePress::Backup(name) => {
                let primary = matches!(press, RuntimePress::Primary(_));
                let e = bot::Edit {
                    agent: primary.then(|| name.to_string()),
                    backup: (!primary).then(|| name.to_string()),
                    ..Default::default()
                };
                let me = b.name();
                let why = "owner via /runtimes";
                let text = match store.bot_edit(&owner_actor(), &me, &e, why) {
                    Ok(_) if primary => format!(
                        "{name} is now the primary of {me}. Its next run starts a fresh session \
                         there; notes are kept."
                    ),
                    Ok(_) => format!(
                        "{name} is now the backup of {me}: it runs {me} only while the primary \
                         is down."
                    ),
                    Err(e) => format!("Not changed: {e:#}"),
                };
                say(&text, &[], json!({}));
            }
            RuntimePress::NoBackup => {
                let e = bot::Edit {
                    backup: Some("none".into()),
                    ..Default::default()
                };
                let me = b.name();
                let text = match store.bot_edit(&owner_actor(), &me, &e, "owner via /runtimes") {
                    Ok(_) => format!("{me} has no backup runtime now."),
                    Err(e) => format!("Not changed: {e:#}"),
                };
                say(&text, &[], json!({}));
            }
        }
        Ok(true)
    }

    /// Removes the keyboard of Telegram message `tg_msg` (best effort).
    fn clear_buttons(&self, b: &TgBot, chat: i64, tg_msg: i64) {
        let p = json!({"chat_id": chat, "message_id": tg_msg,
                       "reply_markup": {"inline_keyboard": []}});
        if let Err(e) = b.api.call("editMessageReplyMarkup", &p) {
            eprintln!("telegram: bot {}: clear buttons: {e}", b.name());
        }
    }

    /// The runtime list, with a login button per runtime the node
    /// manages, in reply to `reply_to`.
    fn runtimes_reply(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: i64,
        reply_to: Option<i64>,
        thread: Option<i64>,
    ) {
        let view = store.runtimes();
        let text = list_text(&view);
        let me = store.get_bot(&b.name()).ok();
        let (buttons, rows) = list_buttons(&view, me.as_ref());
        let mut p = json!({"chat_id": chat, "text": text});
        if let Some(r) = reply_to {
            p["reply_parameters"] = json!({"message_id": r, "allow_sending_without_reply": true});
        }
        if let Some(t) = thread {
            p["message_thread_id"] = json!(t);
        }
        if !buttons.is_empty() {
            p["reply_markup"] = keyboard(&buttons, &rows);
        }
        match b.paced(store, chat, || b.api.call("sendMessage", &p)) {
            Ok(sent) => {
                let r = record_out(
                    store,
                    &b.name(),
                    &sent,
                    "text",
                    &text,
                    None,
                    json!([]),
                    json!({"system": true, "runtimes_list": true}),
                );
                if let Err(e) = r {
                    eprintln!("telegram: bot {}: record runtimes reply: {e:#}", b.name());
                }
            }
            Err(e) => eprintln!("telegram: bot {}: runtimes reply: {e}", b.name()),
        }
    }

    /// A system reply with optional inline buttons and extra flags.
    #[allow(clippy::too_many_arguments)]
    fn system_say(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: i64,
        reply_to: i64,
        text: &str,
        buttons: &[(String, String)],
        rows: &[usize],
        extra: Value,
    ) {
        let mut p = json!({"chat_id": chat, "text": text,
            "reply_parameters": {"message_id": reply_to, "allow_sending_without_reply": true},
            "link_preview_options": {"is_disabled": true}});
        if !buttons.is_empty() {
            p["reply_markup"] = keyboard(buttons, rows);
        }
        let mut flags = json!({"system": true});
        if let (Some(f), Some(e)) = (flags.as_object_mut(), extra.as_object()) {
            f.extend(e.clone());
        }
        match b.paced(store, chat, || b.api.call("sendMessage", &p)) {
            Ok(sent) => {
                if let Err(e) = record_out(
                    store,
                    &b.name(),
                    &sent,
                    "text",
                    text,
                    None,
                    json!([]),
                    flags,
                ) {
                    eprintln!("telegram: bot {}: record reply: {e:#}", b.name());
                }
            }
            Err(e) => eprintln!("telegram: bot {}: reply: {e}", b.name()),
        }
    }

    /// Downloads a `.sealed` document's text.
    fn sealed_file(&self, b: &TgBot, doc: &Value) -> Result<String> {
        let limit = crate::sealed::MAX_SEALED as u64;
        if doc["file_size"].as_u64().is_some_and(|s| s > limit) {
            bail!("the .sealed file is over {limit} bytes");
        }
        let id = doc["file_id"]
            .as_str()
            .context("document without file_id")?;
        let f = b.api.call("getFile", &json!({"file_id": id}))?;
        let path = f["file_path"]
            .as_str()
            .context("getFile returned no file_path")?;
        let bytes = b.api.download(path, limit)?;
        String::from_utf8(bytes).context("the .sealed file is not text")
    }

    /// Hands a sealed reply to the node and tells the owner the
    /// outcome; once the form is done, the outcome replaces the form
    /// keyboard with the buttons of the forms still open in the chat, or
    /// removes it. A form that finishes later has its prompt closed by the
    /// server (`Forms::take_closing`).
    fn form_reply(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: i64,
        tg_id: i64,
        data: Result<String>,
    ) -> Result<()> {
        let outcome = data.and_then(|d| store.form_submit(&d));
        let (text, open) = match &outcome {
            Ok(f) => (
                crate::form::outcome_text(f),
                store.form_close(&f.spec.form, &b.name(), chat),
            ),
            Err(e) => (format!("Sealed form reply refused: {e:#}."), None),
        };
        self.form_say(b, store, chat, Some(tg_id), &text, open.as_deref());
        Ok(())
    }

    /// Sends a system message about a form; `open` (the open forms'
    /// purpose and link) replaces the chat's form keyboard, and an empty
    /// `open` removes it.
    fn form_say(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: i64,
        reply_to: Option<i64>,
        text: &str,
        open: Option<&[(String, String)]>,
    ) {
        let mut p = json!({"chat_id": chat, "text": text});
        if let Some(r) = reply_to {
            p["reply_parameters"] = json!({"message_id": r, "allow_sending_without_reply": true});
        }
        if let Some(open) = open {
            p["reply_markup"] = form_keyboard(open);
        }
        match b.paced(store, chat, || b.api.call("sendMessage", &p)) {
            Ok(sent) => {
                let r = record_out(
                    store,
                    &b.name(),
                    &sent,
                    "text",
                    text,
                    None,
                    json!([]),
                    json!({"system": true}),
                );
                if let Err(e) = r {
                    eprintln!("telegram: bot {}: record form reply: {e:#}", b.name());
                }
            }
            Err(e) => eprintln!("telegram: bot {}: form reply: {e}", b.name()),
        }
    }

    /// Checks that `name` can send a form prompt: a configured bot that may
    /// write to the owner's private chat (a Mini App button needs one).
    pub fn form_sender(&self, name: &str) -> Result<&Arc<TgBot>> {
        let b = self.bot(name)?;
        if !b.chats.contains(&self.owner) {
            bail!(
                "bot {name} cannot write to the owner's private chat, which a Mini App button needs"
            );
        }
        Ok(b)
    }

    /// Sends the owner a form's prompt with a reply keyboard of Mini App
    /// buttons (`web_app`, private chats only) from bot `name`. A chat shows
    /// only its latest reply keyboard, so it holds a button for each form
    /// `open` returns: the open forms prompted in that chat (this one
    /// included), as purpose and link.
    pub fn form_prompt(
        &self,
        store: &dyn Axe,
        name: &str,
        f: &crate::form::Form,
        link: &str,
        open: impl FnOnce(&crate::form::Prompt) -> Vec<(String, String)>,
    ) -> Result<Value> {
        let b = self.form_sender(name)?;
        store.bot_id(name)?;
        let name = name.to_string();
        let chat = self.owner;
        let buttons = open(&crate::form::Prompt {
            bot: name.clone(),
            chat,
        });
        let html = crate::form::prompt_html(f, b.web.as_deref(), link, now());
        let p = json!({
            "chat_id": chat,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            "reply_markup": form_keyboard(&buttons),
        });
        let sent = b.paced(store, chat, || b.api.call("sendMessage", &p))?;
        let id = record_out(
            store,
            &name,
            &sent,
            "text",
            &html,
            None,
            json!([]),
            json!({"system": true, "html": true, "form": f.spec.form}),
        )?;
        Ok(json!({"sent": true, "bot": name, "id": id}))
    }

    /// Sends the owner a sealed message (`sealed.rs`, sealed messages) from
    /// bot `name`: `text` (the sender and purpose, never a secret) and an
    /// inline Mini App button to `link`, the form page with the ciphertext in
    /// its fragment. Only that text is recorded.
    pub fn send_sealed(
        &self,
        store: &dyn Axe,
        name: &str,
        text: &str,
        link: &str,
    ) -> Result<Value> {
        let b = self.form_sender(name)?;
        store.bot_id(name)?;
        let chat = self.owner;
        let p = json!({
            "chat_id": chat,
            "text": text,
            "link_preview_options": {"is_disabled": true},
            "reply_markup": {
                "inline_keyboard": [[{"text": "Open sealed message", "web_app": {"url": link}}]],
            },
        });
        let sent = b.paced(store, chat, || b.api.call("sendMessage", &p))?;
        let id = record_out(
            store,
            name,
            &sent,
            "text",
            text,
            None,
            json!([]),
            json!({"system": true, "sealed": true}),
        )?;
        Ok(json!({"sent": true, "bot": name, "chat": chat, "id": id,
                  "message_id": sent["message_id"]}))
    }

    /// Tells the owner a form's outcome in its prompt's chat (delivered after
    /// a wait or outside Telegram, rejected for good, or expired); `open`
    /// replaces the prompt's keyboard (as `form_say`).
    pub fn form_outcome(
        &self,
        store: &dyn Axe,
        bot: &str,
        chat: i64,
        f: &crate::form::Form,
        open: Option<&[(String, String)]>,
    ) {
        if let Ok(b) = self.bot(bot) {
            self.form_say(b, store, chat, None, &crate::form::outcome_text(f), open);
        }
    }

    /// Sends the owner one card for a runtime alert (`runtime::Alert`) from
    /// the ops bot, in the owner's private chat when the bot may write there.
    /// An alert of an ok runtime closes that runtime's open cards first
    /// ([`close_runtime_card`]) and goes out without buttons.
    pub fn runtime_alert(&self, store: &dyn Axe, a: &Value) -> Result<Value> {
        let Some(name) = self
            .ops
            .as_deref()
            .and_then(|o| self.bot(o).ok())
            .map(|b| b.name())
        else {
            return Ok(json!({"sent": false, "reason": "no telegram bot to send runtime cards"}));
        };
        // The bot exists once it was created; until then the node retries.
        store.bot_id(&name)?;
        let chat = self
            .bot(&name)?
            .chats
            .contains(&self.owner)
            .then_some(self.owner);
        let runtime = a["runtime"].as_str().unwrap_or_default();
        let ok = a["state"] == crate::runtime::OK;
        if ok {
            let b = self.bot(&name)?;
            for m in store.open_runtime_cards(&name)? {
                if card_runtime(&m).as_deref() == Some(runtime) {
                    close_runtime_card(b, store, &m, runtime)?;
                }
            }
        }
        let opts = SendOpts {
            chat,
            buttons: match ok {
                true => Vec::new(),
                false => vec![("Runtimes".into(), RUNTIMES_BUTTON.into())],
            },
            ..SendOpts::default()
        };
        let out = self.send(store, &name, &crate::runtime::alert_text(a), &opts)?;
        let id = out
            .last()
            .and_then(|m| m["id"].as_i64())
            .context("runtime card was not recorded")?;
        store.mark_runtime_alert(id, runtime)?;
        Ok(json!({"sent": true, "bot": name, "id": id}))
    }

    /// A message to the owner from bot `bot` (a service crash loop the bot
    /// could not fix): sent by that bot when the gateway talks for it, else
    /// by the ops bot.
    pub fn owner_alert(&self, store: &dyn Axe, bot: &str, text: &str) -> Result<Value> {
        let name = match self.bot(bot) {
            Ok(b) => b.name(),
            Err(_) => match self.ops.as_deref().and_then(|o| self.bot(o).ok()) {
                Some(b) => b.name(),
                None => {
                    return Ok(
                        json!({"sent": false, "reason": format!("no telegram bot to alert the owner for {bot}")}),
                    )
                }
            },
        };
        let chat = self
            .bot(&name)?
            .chats
            .contains(&self.owner)
            .then_some(self.owner);
        let opts = SendOpts {
            chat,
            ..SendOpts::default()
        };
        let out = self.send(store, &name, text, &opts)?;
        let id = out.last().and_then(|m| m["id"].as_i64());
        Ok(json!({"sent": true, "bot": name, "id": id}))
    }

    /// A press on a retro card (`flags.retro`): rates an item, accepts the
    /// recommendations of a page's unrated items, turns a page or dismisses
    /// the card, then re-renders it in place. A single rating wakes no bot;
    /// the card closing (Dismiss, or its last item rated) with ratings on it
    /// wakes its bot once ([`retro_closed`]). Returns false when the message
    /// is not a retro card.
    fn retro_press(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: i64,
        tg_msg: i64,
        data: &str,
        query: Option<&str>,
    ) -> Result<bool> {
        let Some(stored) = store.message_at(&b.name(), chat, tg_msg)? else {
            return Ok(false);
        };
        if !stored.flags["retro"].is_object() {
            return Ok(false);
        }
        let items: Vec<ask::RetroItem> =
            serde_json::from_value(stored.flags["retro"]["items"].clone()).unwrap_or_default();
        let more = stored.flags["retro"]["more"].as_u64().unwrap_or(0) as usize;
        let every = ask::card_every(&stored.flags["retro"]);
        let ids: Vec<String> = items.iter().map(|i| i.ask.clone()).collect();
        let press = ask::Press::parse(data);
        if stored.flags["dismissed"] == true {
            self.toast(b, query, "This retro was dismissed");
            return Ok(true);
        }
        if press == Some(ask::Press::Dismiss) {
            // Closes the card: no keyboard, so it is not open, and the inbox
            // pass unpins it if it was pinned. Its asks stay as they are.
            let rated = store.ratings(&ids)?;
            let text = ask::dismissed_retro(&items, &rated, Some(&b.name()), every);
            if let Err(e) = self.edit(store, &b.name(), stored.id, Some(&text), false, None, &[]) {
                eprintln!("telegram: bot {}: retro card: {e:#}", b.name());
            }
            store.set_flag(stored.id, "$.dismissed", json!(true))?;
            retro_closed(store, &b.name(), &stored, every, rated.len(), false)?;
            self.toast(b, query, "Dismissed; unrated asks stay unrated");
            return Ok(true);
        }
        let (toast, page) = match press {
            Some(ask::Press::Item(a, rating)) if ids.contains(&a) => {
                match store.rate(&owner_actor(), &a, rating) {
                    Ok(_) => (format!("Rated: {rating}"), Some(ask::page_of(&items, &a))),
                    // An ask gone since the card was sent (a card its bot
                    // settled): nothing to rate.
                    Err(e) => (format!("Not rated: {e:#}"), None),
                }
            }
            Some(ask::Press::Page(p)) => (format!("Page {}", p + 1), Some(p)),
            // Accept the shown recommendations of the page's unrated asks.
            Some(ask::Press::Accept(p)) => {
                let n = store.accept_recs(&owner_actor(), ask::page_items(&items, p))?;
                let toast = match n {
                    0 => "Nothing left to accept on this page".to_string(),
                    1 => "Accepted 1 rec".to_string(),
                    n => format!("Accepted {n} recs"),
                };
                (toast, Some(p))
            }
            // A card sent when asks were rated by kind: shown per ask now.
            Some(ask::Press::Retired) => ("Rate each ask on its own".to_string(), Some(0)),
            _ => ("Not an item on this card".to_string(), None),
        };
        if let Some(page) = page {
            let rated = store.ratings(&ids)?;
            // The last item rated closes the card: its bot reads the ratings.
            if !items.is_empty() && items.iter().all(|i| rated.contains_key(&i.ask)) {
                retro_closed(store, &b.name(), &stored, every, rated.len(), false)?;
            }
            let r = ask::render_retro(&items, more, &rated, page, Some(&b.name()), every);
            if let Err(e) = self.edit(
                store,
                &b.name(),
                stored.id,
                Some(&r.text),
                false,
                Some(&r.buttons),
                &r.rows,
            ) {
                eprintln!("telegram: bot {}: retro card: {e:#}", b.name());
            }
        }
        self.toast(b, query, &toast);
        Ok(true)
    }

    /// Answers callback query `query` (if any) with a short toast. Best
    /// effort: the press is already handled.
    fn toast(&self, b: &TgBot, query: Option<&str>, text: &str) {
        if let Some(qid) = query {
            let r = b.api.call(
                "answerCallbackQuery",
                &json!({"callback_query_id": qid, "text": truncate(text, TOAST_LIMIT)}),
            );
            if let Err(e) = r {
                eprintln!("telegram: bot {}: {e}", b.name());
            }
        }
    }

    /// At bot `name`'s retro slot `slot`: its card from before the slot,
    /// if the owner rated some of it but never closed it, wakes the bot
    /// (once per card, the same wake a close queues).
    fn retro_wake_open(&self, store: &dyn Axe, name: &str, slot: i64) -> Result<()> {
        let Some((id, _, items, dismissed)) = store.previous_retro(name, slot)? else {
            return Ok(());
        };
        if dismissed {
            return Ok(());
        }
        let ids: Vec<String> = items.iter().map(|i| i.ask.clone()).collect();
        let rated = store.ratings(&ids)?.len();
        let stored = store.message(name, id)?;
        let every = ask::card_every(&stored.flags["retro"]);
        retro_closed(store, name, &stored, every, rated, rated < items.len())
    }

    /// Bot `name`'s retro card (`ask::bot_retro`): the asks it raised in its
    /// interval, at most `limit` of them (`None`: its retro size).
    pub fn retro_of(
        &self,
        store: &dyn Axe,
        name: &str,
        limit: Option<usize>,
    ) -> Result<ask::Retro> {
        store.bot_retro(name, limit)
    }

    /// Sends bot `name`'s retro card (`retro_of`): one message, paged in
    /// place, one row of rating buttons per ask of its interval it raised
    /// that nobody rated. With nothing to rate it sends nothing.
    pub fn send_retro(
        &self,
        store: &dyn Axe,
        name: &str,
        o: &SendOpts,
        limit: Option<usize>,
    ) -> Result<Value> {
        self.bot(name)?;
        let every = store
            .get_bot(name)
            .ok()
            .and_then(|b| b.retro)
            .unwrap_or_default()
            .period();
        let r = self.retro_of(store, name, limit)?;
        if r.items.is_empty() {
            return Ok(json!({"sent": false, "text": r.text}));
        }
        let opts = SendOpts {
            buttons: r.buttons.clone(),
            rows: r.rows.clone(),
            html: false,
            task: None,
            retro: Some(json!({"items": r.items, "more": r.more, "every": every})),
            ..o.clone()
        };
        let out = self.send(store, name, &r.text, &opts)?;
        let id = out
            .first()
            .and_then(|m| m["id"].as_i64())
            .context("retro card was not recorded")?;
        Ok(json!({"sent": true, "id": id, "items": r.items.len(), "more": r.more}))
    }

    /// Replaces the pressed message's keyboard with the chosen label:
    /// `editMessageText` with a `> label (selected)` line, else (text unknown
    /// or too long, or the edit failed) `editMessageReplyMarkup` alone.
    /// Best effort: the press is already recorded.
    fn mark_selected(
        &self,
        b: &TgBot,
        store: &dyn Axe,
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
                match b.paced(store, chat, || b.api.call("editMessageText", &p)) {
                    Ok(_) => return,
                    Err(e) => eprintln!("telegram: bot {}: {e}", b.name()),
                }
            }
        }
        let p = json!({"chat_id": chat, "message_id": tg_msg, "reply_markup": none});
        if let Err(e) = b.paced(store, chat, || b.api.call("editMessageReplyMarkup", &p)) {
            eprintln!("telegram: bot {}: {e}", b.name());
        }
    }

    fn membership(&self, b: &TgBot, store: &dyn Axe, uid: i64, cm: &Value) -> Result<()> {
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
                        eprintln!("telegram: bot {}: {e}", b.name());
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

    fn reaction(&self, b: &TgBot, store: &dyn Axe, uid: i64, r: &Value) -> Result<()> {
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
        store.record_reaction(
            &b.name(),
            r["message_id"].as_i64(),
            NewMessage {
                direction: "in".into(),
                chat_id: chat,
                message_id: uid,
                from_id: r["user"]["id"].as_i64(),
                kind: "reaction".into(),
                text: emoji.join(" "),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    /// `/status`, `/node`, `/tasks`, `/task N`, `/cost`, `/goals`, `/services`,
    /// `/routines`, `/stop`, `/pause`, `/resume`, `/reset`, `/wake [note]`:
    /// handled here, never waking the bot, so they
    /// work with no agent runtime logged in. The reply is recorded.
    #[allow(clippy::too_many_arguments)]
    fn owner_command(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        uid: i64,
        chat: i64,
        tg_id: i64,
        m: &Value,
        cmd: &str,
        arg: &str,
    ) -> Result<()> {
        if cmd == "runtimes" {
            self.runtimes_reply(b, store, chat, Some(tg_id), m["message_thread_id"].as_i64());
            return Ok(());
        }
        let actor = owner_actor();
        // The read-only views (`owner_views`) reply in HTML, for their links
        // and bold headlines.
        let html = crate::core::VIEWS.contains(&cmd);
        let reply = {
            let web = b.web.as_deref();
            let r = match cmd {
                v if html => store.view(v, &b.name(), arg, web),
                "runtimes" => Ok(crate::runtime::render(&store.runtimes())),
                "stop" => store
                    .bot_change(&actor, &b.name(), "stop")
                    .map(|_| "stopping the live run; queued wakes are kept".to_string()),
                "pause" => store
                    .bot_change(&actor, &b.name(), "pause")
                    .map(|_| "paused: no new wakes; a live run is stopped".to_string()),
                "resume" => store
                    .bot_change(&actor, &b.name(), "resume")
                    .map(|_| "resumed".to_string()),
                "reset" => store
                    .bot_change(&actor, &b.name(), "reset")
                    .map(|_| "session cleared; the next run starts fresh".to_string()),
                _ => {
                    let w = bot::WakeRequest {
                        now: true,
                        note: if arg.is_empty() { "owner /wake" } else { arg }.into(),
                        key: Some(format!("tg-update-{uid}")),
                        ..Default::default()
                    };
                    store
                        .request_wake(&actor, &b.name(), &w)
                        .map(|(w, _)| format!("wake {w} queued"))
                }
            };
            r.unwrap_or_else(|e| {
                let e = format!("/{cmd}: {e:#}");
                if html {
                    html_escape(&e)
                } else {
                    e
                }
            })
        };
        let thread = m["message_thread_id"].as_i64();
        let mut params =
            json!({"chat_id":chat,"text":reply,"reply_parameters":{"message_id":tg_id}});
        if let Some(t) = thread {
            params["message_thread_id"] = json!(t);
        }
        if html {
            params["parse_mode"] = json!("HTML");
            params["link_preview_options"] = json!({"is_disabled": true});
        }
        // The command already committed: a failed reply must not make the
        // update redeliver and repeat it.
        match b.paced(store, chat, || b.api.call("sendMessage", &params)) {
            Ok(sent) => {
                if let Err(e) = record_out(
                    store,
                    &b.name(),
                    &sent,
                    "text",
                    &reply,
                    None,
                    json!([]),
                    if html {
                        json!({"system": true, "html": true})
                    } else {
                        json!({"system": true})
                    },
                ) {
                    eprintln!("telegram: bot {}: record /{cmd} reply: {e:#}", b.name());
                }
            }
            Err(e) => eprintln!("telegram: bot {}: /{cmd} reply: {e}", b.name()),
        }
        Ok(())
    }

    // Outbound, for `axe bot tg`.

    fn target(
        &self,
        b: &TgBot,
        store: &dyn Axe,
        chat: Option<i64>,
        reply_to: Option<i64>,
    ) -> Result<(i64, Option<StoredMessage>)> {
        let reply = reply_to.map(|r| store.message(&b.name(), r)).transpose()?;
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
            (None, None) => store
                .last_inbound_chat(&b.name())?
                .filter(|c| b.chats.contains(c))
                .unwrap_or(b.chats[0]),
        };
        b.allowed(chat)?;
        Ok((chat, reply))
    }

    /// Sends text, split at line boundaries; returns the stored messages.
    pub fn send(
        &self,
        store: &dyn Axe,
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
            store.task_exists(t)?;
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
            let sent = b.paced(store, chat, || b.api.call("sendMessage", &p))?;
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
            } else if let Some(r) = &o.retro {
                json!({"buttons": o.buttons, "html": o.html, "retro": r})
            } else {
                // `ask` marks an owner ask for good (`ask.rs`): it stays when
                // the card closes and its buttons go.
                json!({"buttons": o.buttons, "html": o.html, "ask": true})
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
            store.board_default_on(name, id)?;
            out.push(json!({"id": id, "message_id": sent["message_id"]}));
        }
        Ok(out)
    }

    /// Uploads a file as `photo`, `document` or `voice`.
    #[allow(clippy::too_many_arguments)]
    pub fn send_file(
        &self,
        store: &dyn Axe,
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
        let sent = b.paced(store, chat, || {
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

    fn own_message(&self, b: &TgBot, store: &dyn Axe, msg: i64) -> Result<StoredMessage> {
        let m = store.message(&b.name(), msg)?;
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
        store: &dyn Axe,
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
        let res = match b.paced(store, m.chat_id, || b.api.call(method, &p)) {
            Ok(r) => r,
            // Telegram already shows this (for example clearing a keyboard an
            // older text edit dropped): record the state all the same.
            Err(e) if e.description.contains("message is not modified") => Value::Null,
            Err(e) => return Err(e.into()),
        };
        // Without `buttons`, Telegram drops the keyboard: the card closes. A
        // new keyboard keeps the message's parse mode unless the text changed.
        let html = if text.is_some() {
            html
        } else {
            m.flags["html"] == true
        };
        store.record_edit(msg, text, buttons, html)?;
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

    /// Pins or unpins one of the bot's own messages. A manual pin or unpin
    /// takes the message over from the inbox pass: it drops `flags.card_pin`,
    /// so a closed card the bot pinned is not unpinned again.
    pub fn pin(
        &self,
        store: &dyn Axe,
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
        b.paced(store, m.chat_id, || b.api.call(method, &p))?;
        store.record_pin(msg, pin)?;
        Ok(json!({"id": msg, "pinned": pin}))
    }

    pub fn react(&self, store: &dyn Axe, name: &str, msg: i64, emoji: &str) -> Result<Value> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        b.api.call(
            "setMessageReaction",
            &json!({"chat_id": m.chat_id, "message_id": m.message_id, "reaction": [{"type":"emoji","emoji":emoji}]}),
        )?;
        Ok(json!({"id": msg}))
    }

    pub fn delete(&self, store: &dyn Axe, name: &str, msg: i64) -> Result<Value> {
        let b = self.bot(name)?;
        let m = self.own_message(b, store, msg)?;
        b.api.call(
            "deleteMessage",
            &json!({"chat_id": m.chat_id, "message_id": m.message_id}),
        )?;
        // The stored row stays (history is never deleted); it is marked.
        store.mark_deleted(msg)?;
        Ok(json!({"id": msg}))
    }

    pub fn typing(&self, store: &dyn Axe, name: &str, chat: Option<i64>) -> Result<Value> {
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
        store: &dyn Axe,
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
fn typing_loop(b: &TgBot, store: &dyn Axe) {
    let mut last: HashMap<(i64, Option<i64>), Instant> = HashMap::new();
    while !store.stopping() {
        // The bot may not exist yet.
        let chats = store.live_chats(&b.name()).unwrap_or_default();
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
fn board_loop(b: &TgBot, store: &dyn Axe) {
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
                    b.name(),
                    d.as_secs()
                );
                let body = json!({"error": format!("{e:#}"), "retry_seconds": d.as_secs()});
                let _ = store.bot_event(&b.name(), &Actor::new("telegram"), "board_error", &body);
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
fn inbox_pass(b: &TgBot, store: &dyn Axe) -> Result<()> {
    if store.bot_id(&b.name()).is_err() {
        return Ok(());
    }
    runtime_cards(b, store)?;
    let goal_board = store.goal_board_message(&b.name())?;
    let Some(m) = store.board_message(&b.name())? else {
        return Ok(());
    };
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
fn skip_refused(b: &TgBot, store: &dyn Axe, step: &str, msg: i64, e: ApiError) -> Result<()> {
    if e.code != Some(400) {
        return Err(e.into());
    }
    let body = json!({"error": e.to_string(), "step": step, "message": msg, "skipped": true});
    store.bot_event(&b.name(), &Actor::new("telegram"), "board_error", &body)
}

/// The separate goal board is retired (each active goal has its own pin):
/// unpin it, edit it to a pointer, and stop rendering it.
fn retire_goal_board(b: &TgBot, store: &dyn Axe, g: &StoredMessage) -> Result<()> {
    let live = g.flags["deleted"] != true && b.allowed(g.chat_id).is_ok();
    if live && g.flags["pinned"] != false {
        let p = json!({"chat_id": g.chat_id, "message_id": g.message_id});
        if let Err(e) = b.paced(store, g.chat_id, || b.api.call("unpinChatMessage", &p)) {
            skip_refused(b, store, "unpin goal board", g.id, e)?;
        }
        store.set_flag(g.id, "$.pinned", json!(false))?;
    }
    let text = board::merged_text();
    if live && g.text != text {
        let p = json!({"chat_id": g.chat_id, "message_id": g.message_id, "text": text});
        match b.paced(store, g.chat_id, || b.api.call("editMessageText", &p)) {
            Ok(_) => {}
            Err(e) if e.description.contains("message is not modified") => {}
            Err(e) => skip_refused(b, store, "edit goal board", g.id, e)?,
        }
        store.set_text_edited(g.id, text)?;
    }
    store.clear_goal_board(&b.name())?;
    Ok(())
}

/// Queues the one wake a retro card gives its bot (`ask::retro_wake_key`)
/// when the owner rated `rated` of its items: on close (`open` false) or at
/// the bot's next slot with the card still open. No ratings, no wake; a
/// repeat (another press, a restart, the next slot) adds none.
fn retro_closed(
    store: &dyn Axe,
    name: &str,
    card: &StoredMessage,
    every: i64,
    rated: usize,
    open: bool,
) -> Result<()> {
    if rated == 0 {
        return Ok(());
    }
    let t = now();
    let sent = bot::epoch_of(&card.at).unwrap_or(t);
    store.add_wake(
        &Actor::new("telegram"),
        name,
        t,
        &ask::retro_wake(card.id, name, sent, every, rated, open),
        Some(&ask::retro_wake_key(card.id)),
    )?;
    Ok(())
}

/// Escalated tasks without a card since their escalation: the bot gets one
/// wake (keyed by task and escalation, so a restart never repeats it) to post
/// the card itself; [`board::CARD_WAIT`] after that wake the server posts
/// [`board::fallback_card`] in the board message's chat. Open cards of tasks no
/// longer escalated lose their keyboard (closed).
fn task_cards(b: &TgBot, store: &dyn Axe, chat: i64, thread: Option<i64>) -> Result<()> {
    let needs = store.card_needs(&b.name())?;
    let t = now();
    for n in needs {
        // The wake is due at once; its due time is when the bot was asked.
        let key = format!("card-task-{}-{}", n.task, n.since);
        let (_, asked) = store.add_wake(
            &Actor::new("telegram"),
            &b.name(),
            t,
            &board::card_wake_note(&n, t + board::CARD_WAIT),
            Some(&key),
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
        let sent = b.paced(store, chat, || b.api.call("sendMessage", &p))?;
        let id = record_out(
            store,
            &b.name(),
            &sent,
            "text",
            &text,
            None,
            json!([]),
            json!({"buttons": buttons, "html": true, "task": n.task, "fallback": true}),
        )?;
        store.bot_event(
            &b.name(),
            &Actor::new("telegram"),
            "card_fallback",
            &json!({"message": id, "task": n.task}),
        )?;
    }
    let stale = store.stale_task_cards(&b.name())?;
    for (msg, task, state) in stale {
        let m = store.message(&b.name(), msg)?;
        if b.allowed(m.chat_id).is_ok() {
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id,
                "reply_markup": {"inline_keyboard": []}});
            match b.paced(store, m.chat_id, || {
                b.api.call("editMessageReplyMarkup", &p)
            }) {
                Ok(_) => {}
                Err(e) if e.description.contains("message is not modified") => {}
                Err(e) => skip_refused(b, store, "close card", msg, e)?,
            }
        }
        store.close_card(msg, &format!("task #{task} is {state}"))?;
    }
    Ok(())
}

/// Open runtime cards ([`Gateway::runtime_alert`]) of runtimes the node
/// reports ok again close ([`close_runtime_card`]); a runtime can recover
/// without a card of its own (its periodic probe passes). Nothing closes on
/// a stale report.
fn runtime_cards(b: &TgBot, store: &dyn Axe) -> Result<()> {
    let open = store.open_runtime_cards(&b.name())?;
    if open.is_empty() {
        return Ok(());
    }
    let view = store.runtimes();
    if view["stale"] != false {
        return Ok(());
    }
    let list = view["runtimes"].as_array().cloned().unwrap_or_default();
    for m in open {
        let Some(name) = card_runtime(&m) else {
            continue;
        };
        let ok = list
            .iter()
            .any(|r| r["name"] == name.as_str() && r["state"] == crate::runtime::OK);
        if ok {
            close_runtime_card(b, store, &m, &name)?;
        }
    }
    Ok(())
}

/// The runtime a card is about: `flags.runtime`, or (cards sent before it
/// was recorded) the name after the text's leading `Runtime `.
fn card_runtime(m: &StoredMessage) -> Option<String> {
    if let Some(r) = m.flags["runtime"].as_str() {
        return Some(r.to_string());
    }
    let rest = m.text.strip_prefix("Runtime ")?;
    let end = rest.find([' ', ':'])?;
    Some(rest[..end].to_string()).filter(|r| !r.is_empty())
}

/// Closes runtime card `m` as task cards close: its keyboard goes (the
/// text gains a resolved line when it fits) and `flags.closed` says why.
/// The stored card is claimed first, so a card closes once however many
/// passes see it; the Telegram edit is best effort (a refusal is logged).
fn close_runtime_card(b: &TgBot, store: &dyn Axe, m: &StoredMessage, runtime: &str) -> Result<()> {
    let reason = format!("runtime {runtime} is ok");
    let text = format!("{}\n\nResolved: {reason} again.", m.text);
    let fits = utf16_len(&text) <= TEXT_LIMIT && m.flags["html"] != true;
    if !store.claim_close(m.id, if fits { &text } else { &m.text }, &reason)? {
        return Ok(());
    }
    if b.allowed(m.chat_id).is_err() {
        return Ok(());
    }
    let none = json!({"inline_keyboard": []});
    let (method, p) = match fits {
        true => (
            "editMessageText",
            json!({"chat_id": m.chat_id, "message_id": m.message_id, "text": text, "reply_markup": none}),
        ),
        false => (
            "editMessageReplyMarkup",
            json!({"chat_id": m.chat_id, "message_id": m.message_id, "reply_markup": none}),
        ),
    };
    match b.paced(store, m.chat_id, || b.api.call(method, &p)) {
        Ok(_) => {}
        Err(e) if e.description.contains("message is not modified") => {}
        Err(e) => {
            eprintln!(
                "telegram: bot {}: close runtime card {}: {e}",
                b.name(),
                m.id
            );
            let body = json!({"error": e.to_string(), "step": "close runtime card",
                              "message": m.id, "skipped": true});
            store.bot_event(&b.name(), &Actor::new("telegram"), "board_error", &body)?;
        }
    }
    Ok(())
}

/// Pins every open card silently (oldest first, `flags.card_pin`) and unpins
/// the cards the server pinned once they are closed. A pin Telegram refuses
/// (400) is logged and not retried (`flags.pin_skipped`).
fn sync_pins(b: &TgBot, store: &dyn Axe) -> Result<()> {
    let closed = store.closed_pins(&b.name())?;
    for m in closed {
        if m.flags["deleted"] != true && b.allowed(m.chat_id).is_ok() {
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id});
            if let Err(e) = b.paced(store, m.chat_id, || b.api.call("unpinChatMessage", &p)) {
                skip_refused(b, store, "unpin card", m.id, e)?;
            }
        }
        store.set_flag(m.id, "$.pinned", json!(false))?;
    }
    let lines = store.board_lines(&b.name())?;
    for l in lines.iter().filter(|l| l.kind == "card") {
        let m = store.message(&b.name(), l.id)?;
        if m.flags["pinned"] == true || m.flags["pin_skipped"] == true {
            continue;
        }
        if b.allowed(m.chat_id).is_err() {
            continue;
        }
        let p =
            json!({"chat_id": m.chat_id, "message_id": m.message_id, "disable_notification": true});
        match b.paced(store, m.chat_id, || b.api.call("pinChatMessage", &p)) {
            Ok(_) => {
                store.mark_card_pinned(m.id)?;
            }
            Err(e) => {
                skip_refused(b, store, "pin card", m.id, e)?;
                store.set_flag(m.id, "$.pin_skipped", json!(true))?;
            }
        }
    }
    Ok(())
}

/// The pinned overview is retired: unpin board message `m` (and the
/// overview a move had not finished leaving, `flags.board_from`) and edit it
/// to [`board::RETIRED_TEXT`]. Telegram refusing a step (400) is logged as a
/// skipped `board_error`; other failures are retried.
fn retire_overview(b: &TgBot, store: &dyn Axe, m: &StoredMessage) -> Result<()> {
    let mut old = vec![m.clone()];
    if let Some(from) = m.flags["board_from"].as_i64() {
        if let Ok(f) = store.message(&b.name(), from) {
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
            if let Err(e) = b.paced(store, o.chat_id, || b.api.call("unpinChatMessage", &p)) {
                skip_refused(b, store, "unpin overview", o.id, e)?;
            }
        }
        if live && shown && o.text != board::RETIRED_TEXT {
            // An empty keyboard drops the pick buttons of older boards.
            let p = json!({"chat_id": o.chat_id, "message_id": o.message_id,
                "text": board::RETIRED_TEXT, "reply_markup": {"inline_keyboard": []}});
            match b.paced(store, o.chat_id, || b.api.call("editMessageText", &p)) {
                Ok(_) => {}
                Err(e) if e.description.contains("message is not modified") => {}
                Err(e) => skip_refused(b, store, "edit overview", o.id, e)?,
            }
            store.set_text_edited(o.id, board::RETIRED_TEXT)?;
        }
        store.clear_board_flags(o.id)?;
    }
    store.set_flag(m.id, "$.board_retired", json!(true))?;
    Ok(())
}

/// Records a `goal_pin` bot event: `action` on goal `goal`'s pin `msg`.
fn goal_pin_event(store: &dyn Axe, name: &str, msg: i64, goal: &str, action: &str) -> Result<()> {
    store.bot_event(
        name,
        &Actor::new("telegram"),
        "goal_pin",
        &json!({"message": msg, "goal": goal, "action": action}),
    )
}

/// Stops pin `m` standing for its goal: unpins it and clears `goal_pin`.
fn drop_goal_pin(b: &TgBot, store: &dyn Axe, m: &StoredMessage) -> Result<()> {
    if m.flags["pinned"] == true && m.flags["deleted"] != true && b.allowed(m.chat_id).is_ok() {
        let p = json!({"chat_id": m.chat_id, "message_id": m.message_id});
        if let Err(e) = b.paced(store, m.chat_id, || b.api.call("unpinChatMessage", &p)) {
            skip_refused(b, store, "unpin goal", m.id, e)?;
        }
    }
    store.drop_goal_pin(m.id, &m.flags["goal_pin"].to_string())
}

/// One pinned message per active goal of the bot (`goal_board::pin_text`):
/// a goal without one gets it, sent silently in the board's chat and
/// pinned silently; a pin whose text changed is edited in place; the pin of
/// a goal that is no longer active (done, dropped, paused), or no longer
/// the bot's, is unpinned. A pin Telegram refuses (400) is logged and not
/// retried (`flags.pin_skipped`); a refused edit (the message is gone)
/// drops the pin, so the next pass posts a new one.
fn goal_pins(b: &TgBot, store: &dyn Axe, chat: i64, thread: Option<i64>) -> Result<()> {
    let (goals, pins) = store.goal_pins(&b.name(), b.web.as_deref())?;
    let mut kept: HashMap<i64, StoredMessage> = HashMap::new();
    for m in pins {
        let gid = m.flags["goal_pin"].as_i64().unwrap_or(0);
        let wanted = goals.iter().find(|g| g.id == gid);
        if wanted.is_none() || m.flags["deleted"] == true || kept.contains_key(&gid) {
            drop_goal_pin(b, store, &m)?;
            let name = store.goal_name(gid)?.unwrap_or_else(|| gid.to_string());
            goal_pin_event(store, &b.name(), m.id, &name, "unpinned")?;
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
                let sent = b.paced(store, chat, || b.api.call("sendMessage", &p))?;
                let id = record_out(
                    store,
                    &b.name(),
                    &sent,
                    "text",
                    &text,
                    None,
                    json!([]),
                    json!({"html": true, "goal_pin": g.id, "goal_pin_key": text}),
                )?;
                goal_pin_event(store, &b.name(), id, &g.name, "posted")?;
                store.message(&b.name(), id)?
            }
        };
        if m.flags["goal_pin_key"].as_str() != Some(text.as_str()) {
            if b.allowed(m.chat_id).is_err() {
                continue;
            }
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id, "text": text,
                "parse_mode": "HTML"});
            match b.paced(store, m.chat_id, || b.api.call("editMessageText", &p)) {
                Ok(_) => {}
                // The text already matches (for example after a lost reply).
                Err(e) if e.description.contains("message is not modified") => {}
                Err(e) if e.code == Some(400) => {
                    skip_refused(b, store, "edit goal", m.id, e)?;
                    drop_goal_pin(b, store, &m)?;
                    goal_pin_event(store, &b.name(), m.id, &g.name, "unpinned")?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            store.set_goal_pin_text(m.id, &text)?;
            goal_pin_event(store, &b.name(), m.id, &g.name, "edited")?;
        }
        if m.flags["pinned"] != true && m.flags["pin_skipped"] != true {
            if b.allowed(m.chat_id).is_err() {
                continue;
            }
            let p = json!({"chat_id": m.chat_id, "message_id": m.message_id,
                "disable_notification": true});
            match b.paced(store, m.chat_id, || b.api.call("pinChatMessage", &p)) {
                Ok(_) => store.set_flag(m.id, "$.pinned", json!(true))?,
                Err(e) => {
                    skip_refused(b, store, "pin goal", m.id, e)?;
                    store.set_flag(m.id, "$.pin_skipped", json!(true))?;
                }
            }
        }
    }
    Ok(())
}

/// The longest form purpose a form keyboard button shows.
const FORM_LABEL_MAX: usize = 40;

/// The reply keyboard of the open forms in a chat (purpose, link), one
/// Mini App button per row: "Open sealed form" for a single form, else each
/// form's purpose. No forms removes the keyboard.
fn form_keyboard(open: &[(String, String)]) -> Value {
    if open.is_empty() {
        return json!({"remove_keyboard": true});
    }
    let rows: Vec<Value> = open
        .iter()
        .map(|(purpose, link)| {
            let text = if open.len() == 1 {
                "Open sealed form".to_string()
            } else if purpose.chars().count() > FORM_LABEL_MAX {
                let cut: String = purpose.chars().take(FORM_LABEL_MAX - 1).collect();
                format!("Open: {}…", cut.trim_end())
            } else {
                format!("Open: {purpose}")
            };
            json!([{"text": text, "web_app": {"url": link}}])
        })
        .collect();
    json!({
        "keyboard": rows,
        "resize_keyboard": true,
        "one_time_keyboard": true,
        "input_field_placeholder": "Sealed form",
    })
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
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

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
        .find(|(c, _)| *c == cmd)
        .map(|(c, _)| (*c, arg.trim()))
}

/// The `BotCommandScope`s of the owner's command menu: the owner's private
/// chat when it is allowlisted, and the owner in each allowlisted group.
fn command_scopes(owner: i64, chats: &[i64]) -> Vec<Value> {
    chats
        .iter()
        .filter_map(|&c| match c {
            c if c == owner => Some(json!({"type": "chat", "chat_id": c})),
            c if c < 0 => Some(json!({"type": "chat_member", "chat_id": c, "user_id": owner})),
            _ => None,
        })
        .collect()
}

/// Records a sent message (the `Message` the Bot API returned).
#[allow(clippy::too_many_arguments)]
fn record_out(
    store: &dyn Axe,
    name: &str,
    sent: &Value,
    kind: &str,
    text: &str,
    reply_to: Option<i64>,
    attachments: Value,
    flags: Value,
) -> Result<i64> {
    store.record_message(
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
    )
}

impl TgBot {
    /// The outbound allowlist.
    fn allowed(&self, chat: i64) -> Result<()> {
        if !self.chats.contains(&chat) {
            return Err(Forbidden(format!(
                "chat {chat} is not in bot {}'s Telegram allowlist",
                self.name()
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
            eprintln!("telegram: bot {}: {e}", self.name());
        }
    }

    /// Runs one outbound call through the chat's queue: calls are serialized
    /// per chat, spaced by [`SEND_INTERVAL`], and retried, up to
    /// [`SEND_ATTEMPTS`] calls in all, after a 429's `retry_after` or, for a
    /// transient failure ([`ApiError::transient`]), after [`SEND_RETRY`]
    /// doubling. Each retry is a `send_retry` bot event.
    fn paced<T>(
        &self,
        store: &dyn Axe,
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
                    let wait = Duration::from_secs(secs);
                    self.send_retry(store, &e, attempt, wait);
                    *next = Instant::now() + wait;
                }
                Err(e) if e.transient() && attempt < SEND_ATTEMPTS => {
                    let wait = SEND_RETRY * (1 << (attempt - 1));
                    self.send_retry(store, &e, attempt, wait);
                    *next = Instant::now() + wait;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Records a `send_retry` bot event: call `attempt` failed with `e` and
    /// is retried after `wait`. Best effort: the send goes on regardless.
    fn send_retry(&self, store: &dyn Axe, e: &ApiError, attempt: usize, wait: Duration) {
        eprintln!(
            "telegram: bot {}: {e} (attempt {attempt}/{SEND_ATTEMPTS}, retrying in {}s)",
            self.name(),
            wait.as_secs_f64()
        );
        let body = json!({
            "method": e.method,
            "attempt": attempt,
            "error": e.to_string(),
            "retry_in_ms": wait.as_millis() as u64,
        });
        if let Err(e) = store.bot_event(&self.name(), &Actor::new("telegram"), "send_retry", &body)
        {
            eprintln!("telegram: bot {}: send_retry event: {e:#}", self.name());
        }
    }
}
