//! Owner asks and the owner's ratings of them. An ask is something a bot or a
//! task session made the owner look at, read from the logs axe already keeps:
//!
//! - an **escalation**: one escalation episode of a task (each entry into
//!   `escalated`, numbered like `axe classify --episode`), id `t<task>.<n>`.
//!   Its bot is the task's `bot:` owner, else the bot that posted its card;
//! - a **card**: a bot's own message sent with buttons (`flags.ask`, kept when
//!   the card closes), id `m<message>`. A card for an escalated task
//!   (`flags.task`) is that task's escalation, not a second ask, and the
//!   weekly retro card (`flags.retro`) is not an ask.
//!
//! A rating is `needed`, `could_decide` (the bot could have decided itself) or
//! `should_not_exist`, with who rated and when. It is an event in the ask's
//! own log (`ask_rating`: a task event with `episode=N`, or a bot event); the
//! latest one wins. Nothing new is stored, so every count is computed on read.
//!
//! Each ask also has a **category** (its kind, [`KINDS`]), derived from what
//! the logs already hold: an escalation's class and resolver line, a card's
//! button labels. The rules are fixed so a category means the same thing week
//! to week; anything they do not place is `other`.
use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::board::utf16;
use crate::bot::{epoch_of, ts};
use crate::db::Actor;

pub const RATINGS: [&str; 3] = ["needed", "could_decide", "should_not_exist"];
/// The event kind of a rating, in `events` and `bot_events`.
pub const EVENT: &str = "ask_rating";
const WEEK: i64 = 7 * 86_400;

/// Ask categories, in display order: `(id, label)`. The ids are stable API
/// values (`category`, `--kind`, `kinds` in stats); `other` is always last.
pub const KINDS: [(&str, &str); 8] = [
    ("approval", "merge/deploy approvals"),
    ("decision", "decisions"),
    ("choice", "pick an option"),
    ("confirm", "yes/no checks"),
    ("digest", "pick from a list"),
    ("access", "access and tooling"),
    ("stuck", "stuck or failed runs"),
    ("other", "other"),
];

/// The label of category `id` (the id itself when unknown).
pub fn kind_label(id: &str) -> &str {
    KINDS.iter().find(|k| k.0 == id).map_or(id, |k| k.1)
}

/// Whether text asks to approve shipping: approve, merge, deploy, release,
/// revert, roll back, restart or apply, as a word or its stem.
fn approval_words(text: &str) -> bool {
    const STEMS: [&str; 8] = [
        "approv", "merg", "deploy", "releas", "revert", "rollback", "restart", "apply",
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| STEMS.iter().any(|s| w.starts_with(s)))
}

/// The category of an escalation episode from its class and the text the
/// owner was asked (the resolver line of its `blocked:` note, else the note).
pub fn escalation_kind(class: Option<&str>, asked: &str) -> &'static str {
    match class {
        Some("capability") => "access",
        Some("coordination" | "runtime") => "stuck",
        _ if approval_words(asked) => "approval",
        _ => "decision",
    }
}

/// The category of a card from its button labels (the chosen label alone
/// once the card closed and its buttons went).
pub fn card_kind(labels: &[String]) -> &'static str {
    let task_refs = labels
        .iter()
        .filter(|l| {
            l.trim()
                .strip_prefix('#')
                .is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count();
    if labels.iter().any(|l| approval_words(l)) {
        "approval"
    } else if task_refs > 0 && task_refs * 2 >= labels.len() {
        "digest"
    } else if labels.len() >= 3 {
        "choice"
    } else if labels.len() == 2 {
        "confirm"
    } else {
        "other"
    }
}

/// What an escalation asked: the `resolver:` line of its `blocked:` note,
/// else the note's first line.
fn asked_of(note: &str) -> String {
    note.lines()
        .find_map(|l| l.strip_prefix("resolver:"))
        .or_else(|| note.lines().next())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The `resolver:` line of an escalation note, without its prefix.
fn resolver_of(note: &str) -> String {
    note.lines()
        .find_map(|l| l.strip_prefix("resolver:"))
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The `blocked:` line of an escalation note, without its prefix.
fn blocked_of(note: &str) -> String {
    note.lines()
        .next()
        .unwrap_or("")
        .trim()
        .strip_prefix("blocked:")
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Whether a rating says the owner should not have been asked.
pub fn wasted(rating: &str) -> bool {
    matches!(rating, "could_decide" | "should_not_exist")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ask {
    pub id: String,
    /// `escalation` or `card`.
    pub kind: String,
    /// The ask's category, one of [`KINDS`] (`approval`, `decision`, ...).
    #[serde(default)]
    pub category: String,
    pub bot: Option<String>,
    pub project: String,
    pub task: Option<i64>,
    pub at: String,
    pub summary: String,
    /// What the owner was asked, in their words: an escalation's `blocked:`
    /// line, a card's first line. Empty when the log holds none.
    #[serde(default)]
    pub question: String,
    /// What the owner was asked to do: an escalation's `resolver:` line.
    /// Only the retro card shows it; not part of the API.
    #[serde(skip)]
    pub asked: String,
    pub rating: Option<String>,
    pub rated_by: Option<String>,
    pub rated_at: Option<String>,
}

/// An ask id: `t<task>.<episode>` or `m<message>`. A bare `<task>` or
/// `#<task>` is the task's latest episode, resolved by [`rate`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AskId {
    Task(i64, Option<i64>),
    Message(i64),
}

impl AskId {
    pub fn parse(s: &str) -> Result<AskId> {
        let s = s.trim().trim_start_matches('#');
        let bad =
            || anyhow::anyhow!("bad ask id {s:?}: use t<task>.<episode>, m<message> or a task id");
        if let Some(m) = s.strip_prefix('m') {
            return Ok(AskId::Message(m.parse().map_err(|_| bad())?));
        }
        let t = s.strip_prefix('t').unwrap_or(s);
        match t.split_once('.') {
            Some((task, n)) => {
                let n: i64 = n.parse().map_err(|_| bad())?;
                if n < 1 {
                    return Err(bad());
                }
                Ok(AskId::Task(task.parse().map_err(|_| bad())?, Some(n)))
            }
            None => Ok(AskId::Task(t.parse().map_err(|_| bad())?, None)),
        }
    }
}

/// Which asks [`list`] returns: made in `[since, until)`.
#[derive(Debug, Default, Clone)]
pub struct Filter<'a> {
    pub since: i64,
    pub until: i64,
    pub project: Option<&'a str>,
    pub bot: Option<&'a str>,
    /// Only asks of this category.
    pub kind: Option<&'a str>,
    pub unrated: bool,
}

struct Rating {
    rating: String,
    by: String,
    at: String,
}

/// Episode ratings per `(task, episode)`, latest first wins.
fn task_ratings(conn: &Connection) -> Result<HashMap<(i64, i64), Rating>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, body, to_value, actor, at FROM events WHERE kind=?1 ORDER BY id",
    )?;
    let mut rows = stmt.query([EVENT])?;
    let mut out = HashMap::new();
    while let Some(r) = rows.next()? {
        let body: Option<String> = r.get(1)?;
        let Some(n) = body
            .as_deref()
            .and_then(|b| b.strip_prefix("episode="))
            .and_then(|n| n.trim().parse().ok())
        else {
            continue;
        };
        out.insert(
            (r.get(0)?, n),
            Rating {
                rating: r.get(2)?,
                by: r.get(3)?,
                at: r.get(4)?,
            },
        );
    }
    Ok(out)
}

/// The set of task escalation episodes rated wasted, for the goal budget.
pub fn wasted_episodes(conn: &Connection) -> Result<std::collections::HashSet<(i64, i64)>> {
    Ok(task_ratings(conn)?
        .into_iter()
        .filter(|(_, r)| wasted(&r.rating))
        .map(|(k, _)| k)
        .collect())
}

/// Whether an event opens an escalation episode (as `db::classify` counts them).
pub fn opens_episode(kind: &str, from: Option<&str>, to: Option<&str>, body: Option<&str>) -> bool {
    match kind {
        "created" => body.unwrap_or_default().starts_with("state=escalated"),
        "state" => to == Some("escalated") && from != Some("escalated"),
        _ => false,
    }
}

fn escalations(conn: &Connection, project: Option<&str>) -> Result<Vec<Ask>> {
    let ratings = task_ratings(conn)?;
    // The bot that posted a card standing for the task, when it has no bot owner.
    let mut card_bot: HashMap<i64, String> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT CAST(json_extract(m.flags,'$.task') AS INTEGER), b.name FROM bot_messages m
         JOIN bots b ON b.id=m.bot_id
         WHERE m.direction='out' AND json_type(m.flags,'$.task') IS NOT NULL ORDER BY m.id",
    )?;
    for r in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (t, b) = r?;
        card_bot.insert(t, b);
    }
    let mut stmt = conn.prepare(
        "SELECT e.task_id, e.at, e.kind, e.from_value, e.to_value, e.body, t.project, t.title, t.owner
         FROM events e JOIN tasks t ON t.id=e.task_id
         WHERE (e.kind IN ('created','state','escalation_class')
                OR (e.kind='note' AND e.body LIKE 'blocked:%'))
           AND (?1 IS NULL OR t.project=?1)
         ORDER BY e.task_id, e.id",
    )?;
    let mut rows = stmt.query([project])?;
    let mut out: Vec<Ask> = Vec::new();
    // Per episode of the current task: its index in `out`, class and asked text.
    let mut eps: Vec<(usize, Option<String>, String)> = Vec::new();
    let mut task = None;
    let mut open = false;
    let finish = |out: &mut Vec<Ask>, eps: &mut Vec<(usize, Option<String>, String)>| {
        for (i, class, asked) in eps.drain(..) {
            out[i].category = escalation_kind(class.as_deref(), &asked).into();
        }
    };
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if task != Some(id) {
            finish(&mut out, &mut eps);
            task = Some(id);
            open = false;
        }
        let kind: String = r.get(2)?;
        let from: Option<String> = r.get(3)?;
        let to: Option<String> = r.get(4)?;
        let body: Option<String> = r.get(5)?;
        match kind.as_str() {
            // As `flow::analyze`: an episode number names its episode, else
            // the open one, else the latest.
            "escalation_class" => {
                let n = body
                    .as_deref()
                    .and_then(|b| b.strip_prefix("episode="))
                    .and_then(|n| n.trim().parse::<usize>().ok());
                let target = match n {
                    Some(n) => eps.get_mut(n.wrapping_sub(1)),
                    None => eps.last_mut(),
                };
                if let Some(ep) = target {
                    ep.1 = to;
                }
                continue;
            }
            "note" => {
                if let Some(ep) = eps.last_mut().filter(|e| open && e.2.is_empty()) {
                    let note = body.as_deref().unwrap_or_default();
                    ep.2 = asked_of(note);
                    out[ep.0].question = blocked_of(note);
                    out[ep.0].asked = resolver_of(note);
                }
                continue;
            }
            "state"
                if from.as_deref() == Some("escalated") && to.as_deref() != Some("escalated") =>
            {
                open = false;
            }
            _ => {}
        }
        if !opens_episode(&kind, from.as_deref(), to.as_deref(), body.as_deref()) {
            continue;
        }
        open = true;
        let n = eps.len() as i64 + 1;
        let owner: Option<String> = r.get(8)?;
        let bot = owner
            .as_deref()
            .and_then(|o| o.strip_prefix("bot:"))
            .map(str::to_string)
            .or_else(|| card_bot.get(&id).cloned());
        let rating = ratings.get(&(id, n));
        eps.push((out.len(), None, String::new()));
        out.push(Ask {
            id: format!("t{id}.{n}"),
            kind: "escalation".into(),
            category: String::new(),
            bot,
            project: r.get(6)?,
            task: Some(id),
            at: r.get(1)?,
            summary: r.get(7)?,
            question: String::new(),
            asked: String::new(),
            rating: rating.map(|r| r.rating.clone()),
            rated_by: rating.map(|r| r.by.clone()),
            rated_at: rating.map(|r| r.at.clone()),
        });
    }
    finish(&mut out, &mut eps);
    Ok(out)
}

/// SQL condition on `bot_messages m` for a card ask. Cards sent before
/// `flags.ask` existed count while their buttons or answer are still recorded.
const CARD_ASK: &str = "m.direction='out' AND m.kind='text'
    AND (json_type(m.flags,'$.ask') IS NOT NULL
         OR (json_type(m.flags,'$.buttons')='array' AND json_array_length(m.flags,'$.buttons')>0)
         OR json_type(m.flags,'$.answered') IS NOT NULL)
    AND json_type(m.flags,'$.task') IS NULL
    AND json_type(m.flags,'$.retro') IS NULL
    AND json_type(m.flags,'$.board') IS NULL";

fn card_ratings(conn: &Connection) -> Result<HashMap<i64, Rating>> {
    let mut stmt =
        conn.prepare("SELECT body, actor, at FROM bot_events WHERE kind=?1 ORDER BY id")?;
    let mut rows = stmt.query([EVENT])?;
    let mut out = HashMap::new();
    while let Some(r) = rows.next()? {
        let body: String = r.get(0)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let (Some(m), Some(rating)) = (v["message"].as_i64(), v["rating"].as_str()) else {
            continue;
        };
        out.insert(
            m,
            Rating {
                rating: rating.into(),
                by: r.get(1)?,
                at: r.get(2)?,
            },
        );
    }
    Ok(out)
}

fn first_line(text: &str, html: bool) -> String {
    let plain = if html {
        crate::board::strip_html(text)
    } else {
        text.to_string()
    };
    plain
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn cards(conn: &Connection, project: Option<&str>, only: Option<i64>) -> Result<Vec<Ask>> {
    let ratings = card_ratings(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT m.id, m.text, m.flags, m.at, b.name, b.project FROM bot_messages m
         JOIN bots b ON b.id=m.bot_id
         WHERE {CARD_ASK} AND (?1 IS NULL OR b.project=?1) AND (?2 IS NULL OR m.id=?2)
         ORDER BY m.id"
    ))?;
    let mut rows = stmt.query(params![project, only])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let text: String = r.get(1)?;
        let flags: Value = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
        let rating = ratings.get(&id);
        let mut labels: Vec<String> = flags["buttons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b[0].as_str().map(String::from))
            .collect();
        if labels.is_empty() {
            labels.extend(flags["answered"]["label"].as_str().map(String::from));
        }
        out.push(Ask {
            id: format!("m{id}"),
            kind: "card".into(),
            category: card_kind(&labels).into(),
            bot: Some(r.get(4)?),
            project: r.get(5)?,
            task: None,
            at: r.get(3)?,
            summary: first_line(&text, flags["html"] == true),
            question: first_line(&text, flags["html"] == true),
            asked: String::new(),
            rating: rating.map(|r| r.rating.clone()),
            rated_by: rating.map(|r| r.by.clone()),
            rated_at: rating.map(|r| r.at.clone()),
        });
    }
    Ok(out)
}

/// Asks made in `[since, until)`, oldest first.
pub fn list(conn: &Connection, f: &Filter) -> Result<Vec<Ask>> {
    let mut out = escalations(conn, f.project)?;
    out.extend(cards(conn, f.project, None)?);
    out.retain(|a| {
        let t = epoch_of(&a.at).unwrap_or(0);
        t >= f.since
            && t < f.until
            && f.bot.is_none_or(|b| a.bot.as_deref() == Some(b))
            && f.kind.is_none_or(|k| a.category == k)
            && !(f.unrated && a.rating.is_some())
    });
    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Records the owner's rating of one ask and returns it rated. Task sessions
/// (`agent`) may not rate: the rating is the owner's judgment of the ask.
pub fn rate(conn: &mut Connection, actor: &Actor, id: &str, rating: &str) -> Result<Ask> {
    check_rater(actor, rating)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ask_id = write_rating(&tx, actor, id, rating)?;
    tx.commit()?;
    find(conn, &ask_id)
}

fn check_rater(actor: &Actor, rating: &str) -> Result<()> {
    if !RATINGS.contains(&rating) {
        bail!(
            "invalid rating {rating:?} (expected {})",
            RATINGS.join(", ")
        );
    }
    if actor.name == "agent" {
        bail!("task sessions cannot rate asks; the owner rates them");
    }
    Ok(())
}

/// Writes one rating event; returns the ask's canonical id.
fn write_rating(
    tx: &rusqlite::Transaction,
    actor: &Actor,
    id: &str,
    rating: &str,
) -> Result<String> {
    Ok(match AskId::parse(id)? {
        AskId::Task(task, n) => {
            let episodes: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE task_id=?1 AND (
                       (kind='state' AND to_value='escalated' AND from_value IS NOT 'escalated')
                       OR (kind='created' AND body LIKE 'state=escalated%'))",
                    [task],
                    |r| r.get(0),
                )
                .with_context(|| format!("task {task}"))?;
            if episodes == 0 {
                bail!("task {task} has never been escalated: it made no ask");
            }
            let n = n.unwrap_or(episodes);
            if n > episodes {
                bail!("task {task} has escalation episodes 1..={episodes}, not {n}");
            }
            crate::db::write_event(
                tx,
                actor,
                task,
                EVENT,
                None,
                Some(rating),
                Some(&format!("episode={n}")),
            )?;
            format!("t{task}.{n}")
        }
        AskId::Message(m) => {
            let bot: i64 = tx
                .query_row(
                    &format!("SELECT m.bot_id FROM bot_messages m WHERE m.id=?1 AND {CARD_ASK}"),
                    [m],
                    |r| r.get(0),
                )
                .map_err(|_| anyhow::anyhow!("message {m} is not a card ask"))?;
            crate::bot::event(
                tx,
                bot,
                actor,
                EVENT,
                &json!({"message": m, "rating": rating}).to_string(),
            )?;
            format!("m{m}")
        }
    })
}

/// One ask by its canonical id.
pub fn find(conn: &Connection, id: &str) -> Result<Ask> {
    let found = match AskId::parse(id)? {
        AskId::Task(t, _) => escalations(conn, None)?
            .into_iter()
            .find(|a| a.task == Some(t) && a.id == id),
        AskId::Message(m) => cards(conn, None, Some(m))?.into_iter().next(),
    };
    found.with_context(|| format!("ask {id} not found"))
}

/// The current rating of each of `ids` that has one, read in one pass.
pub fn ratings(conn: &Connection, ids: &[String]) -> Result<HashMap<String, String>> {
    let ids: HashSet<&String> = ids.iter().collect();
    let mut all = escalations(conn, None)?;
    all.extend(cards(conn, None, None)?);
    Ok(all
        .into_iter()
        .filter(|a| ids.contains(&a.id))
        .filter_map(|a| Some((a.id, a.rating?)))
        .collect())
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Tally {
    pub asks: u64,
    pub rated: u64,
    pub needed: u64,
    pub could_decide: u64,
    pub should_not_exist: u64,
    /// `(could_decide + should_not_exist) / rated`; `None` with nothing rated.
    pub wasted_share: Option<f64>,
}

impl Tally {
    fn add(&mut self, a: &Ask) {
        self.asks += 1;
        match a.rating.as_deref() {
            Some("needed") => self.needed += 1,
            Some("could_decide") => self.could_decide += 1,
            Some("should_not_exist") => self.should_not_exist += 1,
            _ => return,
        }
        self.rated += 1;
        self.wasted_share =
            Some((self.could_decide + self.should_not_exist) as f64 / self.rated as f64);
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Week {
    pub since: String,
    pub until: String,
    #[serde(flatten)]
    pub tally: Tally,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Series {
    #[serde(flatten)]
    pub total: Tally,
    /// 7-day blocks from `since`; the last one may be shorter.
    pub weeks: Vec<Week>,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
pub struct Stats {
    pub since: String,
    pub until: String,
    pub overall: Series,
    /// Per bot; asks with no bot (an escalation of a task no bot owns) are
    /// under `-`.
    pub bots: BTreeMap<String, Series>,
    /// Per category ([`KINDS`]).
    pub kinds: BTreeMap<String, Series>,
}

/// Asks, ratings and the wasted-ask share over `[since, until)`, overall and
/// per bot, in 7-day blocks.
pub fn stats(conn: &Connection, project: Option<&str>, since: i64, until: i64) -> Result<Stats> {
    if since >= until {
        bail!("since must be before until");
    }
    let asks = list(
        conn,
        &Filter {
            since,
            until,
            project,
            ..Default::default()
        },
    )?;
    let blocks: Vec<(i64, i64)> = (0..)
        .map(|i| since + i * WEEK)
        .take_while(|s| *s < until)
        .map(|s| (s, (s + WEEK).min(until)))
        .collect();
    let series = || Series {
        total: Tally::default(),
        weeks: blocks
            .iter()
            .map(|(s, u)| Week {
                since: ts(*s),
                until: ts(*u),
                tally: Tally::default(),
            })
            .collect(),
    };
    let mut out = Stats {
        since: ts(since),
        until: ts(until),
        overall: series(),
        bots: BTreeMap::new(),
        kinds: BTreeMap::new(),
    };
    for a in &asks {
        let w = ((epoch_of(&a.at).unwrap_or(since) - since) / WEEK) as usize;
        let bot = a.bot.clone().unwrap_or_else(|| "-".into());
        for s in [
            &mut out.overall,
            out.bots.entry(bot).or_insert_with(series),
            out.kinds.entry(a.category.clone()).or_insert_with(series),
        ] {
            s.total.add(a);
            s.weeks[w].tally.add(a);
        }
    }
    Ok(out)
}

/// Items one retro card holds; the rest are counted in `more`.
pub const RETRO_ITEMS: usize = 1000;
/// Asks per page of a retro card: each has its own row of rating buttons.
pub const RETRO_PAGE: usize = 10;
/// Length (UTF-16 units, as Telegram counts) of an ask's first retro line:
/// its id and what it is about.
const RETRO_TOPIC: usize = 100;
/// Length of an ask's second retro line: what the owner was asked.
const RETRO_QUESTION: usize = 160;
/// Button codes in callback data `rate:<ask>:<code>`.
pub const CODES: [(&str, &str, &str); 3] = [
    ("n", "needed", "needed"),
    ("c", "could_decide", "decide"),
    ("x", "should_not_exist", "drop"),
];
const NOTE: &str =
    "Rate each ask: needed = right to ask, decide = the bot could have decided, drop = should not exist. Skipped asks stay unrated.";

/// The rating a button code stands for.
pub fn rating_of(code: &str) -> Option<&'static str> {
    CODES.iter().find(|c| c.0 == code).map(|c| c.1)
}

/// The button label of a rating (`decide` for `could_decide`).
fn rating_label(rating: &str) -> &str {
    CODES.iter().find(|c| c.1 == rating).map_or(rating, |c| c.2)
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
/// How late a weekly send may still go out (a server down at the slot).
pub const RETRO_CATCH_UP: i64 = 86_400;

/// A weekly UTC time `<weekday> HH:MM` (`mon 01:00`) as seconds after
/// Monday 00:00Z.
pub fn parse_weekly(s: &str) -> Result<i64> {
    let bad = || anyhow::anyhow!("{s:?}: expected a UTC weekday and time such as \"mon 01:00\"");
    let (day, time) = s.trim().split_once(char::is_whitespace).ok_or_else(bad)?;
    let day = day.to_lowercase();
    let d = DAYS
        .iter()
        .position(|x| day.starts_with(x))
        .ok_or_else(bad)? as i64;
    let (h, m) = time.trim().split_once(':').ok_or_else(bad)?;
    let (h, m): (i64, i64) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return Err(bad());
    }
    Ok(d * 86_400 + h * 3600 + m * 60)
}

/// The latest weekly slot at or before `now` (`at` from [`parse_weekly`]).
pub fn last_slot(now: i64, at: i64) -> i64 {
    // 1970-01-01 was a Thursday, 3 days after a Monday.
    let days = now.div_euclid(86_400);
    let monday = (days - (days + 3).rem_euclid(7)) * 86_400;
    let slot = monday + at;
    if slot > now {
        slot - WEEK
    } else {
        slot
    }
}

/// The slot a weekly retro is due for at `now`, if any: the latest slot,
/// at most [`RETRO_CATCH_UP`] ago, with no retro card sent since it
/// (`last_sent`) and not already tried (`tried`).
pub fn retro_due(now: i64, at: i64, last_sent: Option<i64>, tried: Option<i64>) -> Option<i64> {
    let slot = last_slot(now, at);
    (now - slot <= RETRO_CATCH_UP && last_sent.is_none_or(|t| t < slot) && tried != Some(slot))
        .then_some(slot)
}

/// When bot `bot` last sent a retro card.
pub fn last_retro(conn: &Connection, bot: &str) -> Result<Option<i64>> {
    let at: Option<String> = conn
        .query_row(
            "SELECT m.at FROM bot_messages m JOIN bots b ON b.id=m.bot_id
             WHERE b.name=?1 AND m.direction='out' AND json_type(m.flags,'$.retro') IS NOT NULL
             ORDER BY m.id DESC LIMIT 1",
            [bot],
            |r| r.get(0),
        )
        .optional()?;
    Ok(at.as_deref().and_then(epoch_of))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetroItem {
    pub ask: String,
    /// The ask's id and what it is about.
    pub line: String,
    /// What the owner was asked, summarized. Empty on cards sent before it
    /// existed: they show one line.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The ask's category, for the footer totals. Empty on cards sent
    /// before kinds existed: counted as `other`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bot: String,
}

/// A rendered retro card: plain text and its buttons. No items means
/// nothing to ask.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Retro {
    pub text: String,
    pub buttons: Vec<(String, String)>,
    pub rows: Vec<usize>,
    pub items: Vec<RetroItem>,
    pub more: usize,
}

/// What a press on a retro card asks for (callback data).
#[derive(Debug, Clone, PartialEq)]
pub enum Press {
    /// `rate:<ask>:<code>`: rate one ask.
    Item(String, &'static str),
    /// `rp:<page>`: show a page.
    Page(usize),
    /// `rd` ([`DISMISS`]): close the card, leaving its asks as they are.
    Dismiss,
    /// A button of a card sent when asks were rated by kind (`rk:`, `ro:`,
    /// `rb`): kinds are no longer rated, so it shows the first page.
    Retired,
}

impl Press {
    pub fn parse(data: &str) -> Option<Press> {
        if data == "rb" {
            return Some(Press::Retired);
        }
        if data == DISMISS {
            return Some(Press::Dismiss);
        }
        let (head, rest) = data.split_once(':')?;
        match head {
            "rp" => rest.parse().ok().map(Press::Page),
            "rate" => {
                let (ask, code) = rest.rsplit_once(':')?;
                Some(Press::Item(ask.into(), rating_of(code)?))
            }
            "rk" | "ro" => rest.contains(':').then_some(Press::Retired),
            _ => None,
        }
    }
}

/// Callback data of a retro card's Dismiss button.
pub const DISMISS: &str = "rd";

/// Whose asks a retro card holds.
#[derive(Debug, Default, Clone)]
pub struct RetroScope<'a> {
    /// The bot whose retro it is; `None` holds every ask.
    pub bot: Option<&'a str>,
    /// Whether this bot's retro also holds the asks no sending bot raised:
    /// those with no bot, or whose bot is not in `senders`. The owner's
    /// default bot (`telegram.retro.bot`, else the ops bot) has them.
    pub home: bool,
    /// The bots that send retros.
    pub senders: &'a [String],
}

impl RetroScope<'_> {
    fn holds(&self, ask: Option<&str>) -> bool {
        let Some(bot) = self.bot else {
            return true;
        };
        match ask {
            Some(a) if a == bot => true,
            Some(a) => self.home && !self.senders.iter().any(|s| s == a),
            None => self.home,
        }
    }
}

/// The weekly retro card of one bot: the asks of the 7 days before `now`
/// it raised that nobody rated yet, two lines each, oldest first. An ask
/// left alone stays unrated; it is never taken as `needed`.
pub fn retro(conn: &Connection, now: i64, scope: &RetroScope) -> Result<Retro> {
    let mut asks = list(
        conn,
        &Filter {
            since: now - WEEK,
            // Through the current second.
            until: now + 1,
            unrated: true,
            ..Default::default()
        },
    )?;
    asks.retain(|a| scope.holds(a.bot.as_deref()));
    let more = asks.len().saturating_sub(RETRO_ITEMS);
    let mut items = Vec::new();
    for a in asks.iter().take(RETRO_ITEMS) {
        let card = match a.kind.as_str() {
            "card" => card_text(conn, a)?,
            _ => CardText::default(),
        };
        let (line, detail) = retro_lines(a, &card);
        items.push(RetroItem {
            ask: a.id.clone(),
            line,
            detail,
            kind: a.category.clone(),
            bot: a.bot.clone().unwrap_or_else(|| "-".into()),
        });
    }
    Ok(render_retro(&items, more, &HashMap::new(), 0, scope.bot))
}

/// What a card's retro lines are made of, read from the store.
#[derive(Debug, Default, Clone)]
struct CardText {
    /// The card's whole text, plain.
    body: String,
    /// `#<task> <title>` when the card opens with a task id.
    task: Option<String>,
    /// The first line of the message the card replies to.
    reply: Option<String>,
}

fn card_text(conn: &Connection, a: &Ask) -> Result<CardText> {
    let Ok(id) = a.id.trim_start_matches('m').parse::<i64>() else {
        return Ok(CardText::default());
    };
    let plain = |text: String, flags: String| {
        let flags: Value = serde_json::from_str(&flags).unwrap_or_default();
        if flags["html"] == true {
            crate::board::strip_html(&text)
        } else {
            text
        }
    };
    let row: Option<(String, String, Option<i64>)> = conn
        .query_row(
            "SELECT text, flags, reply_to FROM bot_messages WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((text, flags, reply_to)) = row else {
        return Ok(CardText::default());
    };
    let body = plain(text, flags);
    let task = match leading_task(&body) {
        Some(t) => conn
            .query_row("SELECT title FROM tasks WHERE id=?1", [t], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|title| format!("#{t} {title}")),
        None => None,
    };
    let reply = match reply_to {
        Some(r) => conn
            .query_row(
                "SELECT text, flags FROM bot_messages WHERE id=?1",
                [r],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .map(|(t, f)| first_line(&plain(t, f), false))
            .filter(|l| !l.is_empty()),
        None => None,
    };
    Ok(CardText { body, task, reply })
}

/// The task id a text opens with (`#458 reopened: ...`).
fn leading_task(text: &str) -> Option<i64> {
    let rest = text.trim_start().strip_prefix('#')?;
    let n: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[n.len()..].chars().next();
    if after.is_some_and(|c| c.is_alphanumeric() || c == '.') {
        return None;
    }
    n.parse().ok()
}

/// An ask's two retro lines. The first is its id (`#<task>`,
/// `#<task>.<episode>` after the first episode, or `msg <id>`) and what it
/// is about: the task's title; for a card, the task it opens with, else
/// the message it replies to, else its first sentence. The second is what
/// was asked: an escalation's `blocked:` and `resolver:` lines, a card's
/// text (its question kept). Both are summarized: whitespace folded, links
/// shortened to a label, commit ids to 7 characters, credentials masked,
/// cut at a sentence or word.
fn retro_lines(a: &Ask, card: &CardText) -> (String, String) {
    let id = match a.task {
        Some(t) if a.id.ends_with(".1") => format!("#{t}"),
        Some(_) => format!("#{}", a.id.trim_start_matches('t')),
        None => format!("msg {}", a.id.trim_start_matches('m')),
    };
    let room = RETRO_TOPIC - utf16(&id) - 2;
    let (topic, detail) = if a.kind == "escalation" {
        (shorten(&a.summary), asked_line(&a.question, &a.asked))
    } else {
        let body = if card.body.trim().is_empty() {
            &a.question
        } else {
            &card.body
        };
        let body = shorten(&flatten(body));
        if let Some(t) = &card.task {
            (shorten(t), question_line(&body, RETRO_QUESTION))
        } else if let Some(r) = &card.reply {
            let r = shorten(r);
            let first = sentences(&r).into_iter().next().unwrap_or_default();
            (format!("re: {first}"), question_line(&body, RETRO_QUESTION))
        } else {
            let mut s = sentences(&body).into_iter();
            let first = s.next().unwrap_or_default();
            let rest = s.collect::<Vec<_>>().join(" ");
            (first, question_line(&rest, RETRO_QUESTION))
        }
    };
    (format!("{id}: {}", clip(&topic, room)), detail)
}

/// An escalation's second line: the blocker, then what the owner was asked
/// to do (`Ask: ...`, the resolver without its leading `who:`).
fn asked_line(blocked: &str, resolver: &str) -> String {
    let b = shorten(blocked);
    let mut r = resolver.trim();
    if let Some((who, rest)) = r.split_once(':') {
        if who.chars().count() <= 30 && !who.contains('/') && who.split_whitespace().count() <= 3 {
            r = rest.trim();
        }
    }
    let r = shorten(r);
    if r.is_empty() || b.is_empty() {
        let only = if b.is_empty() { r } else { b };
        return question_line(&only, RETRO_QUESTION);
    }
    // The blocker gets at least 100, the ask the rest.
    let ask = utf16(&r) + 6;
    let b = clip(&b, RETRO_QUESTION - ask.min(RETRO_QUESTION - 100));
    let room = RETRO_QUESTION - utf16(&b);
    if room < ask.min(30) {
        return b;
    }
    format!("{b} Ask: {}", clip(&r, room - 6))
}

/// Text cut to `limit`, keeping its first question (`...?`) when the cut
/// would drop it.
fn question_line(text: &str, limit: usize) -> String {
    if utf16(text) <= limit {
        return text.to_string();
    }
    let head = clip(text, limit);
    let q = sentences(text).into_iter().find(|s| s.ends_with('?'));
    match q {
        Some(q) if !head.contains(&q) && utf16(&q) <= limit / 2 => {
            let mut head = clip(text, limit - utf16(&q) - 3);
            if !head.ends_with('…') {
                head.push_str(" …");
            }
            format!("{head} {q}")
        }
        _ => head,
    }
}

/// A text's lines as one: each line ends as a sentence, list markers
/// dropped.
fn flatten(text: &str) -> String {
    let mut out = String::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let l = l.trim_start_matches(['-', '*', '•', '·']).trim_start();
        if l.is_empty() {
            continue;
        }
        if !out.is_empty() {
            if !out.ends_with(['.', '?', '!', ':', ';', ',']) {
                out.push('.');
            }
            out.push(' ');
        }
        out.push_str(l);
    }
    out
}

/// A text's sentences: split after `.`, `?` or `!` and a space.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let b = text.as_bytes();
    for (i, c) in text.char_indices() {
        if matches!(c, '.' | '?' | '!') && b.get(i + 1) == Some(&b' ') {
            out.push(text[start..=i].trim().to_string());
            start = i + 1;
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last.to_string());
    }
    out
}

/// Text cut to `limit` UTF-16 units: after the last sentence that fits
/// when that keeps at least half, else at a word, marked `…`.
fn clip(text: &str, limit: usize) -> String {
    if utf16(text) <= limit {
        return text.to_string();
    }
    let mut end = 0;
    let mut units = 0;
    for (i, c) in text.char_indices() {
        units += c.len_utf16();
        if units > limit.saturating_sub(1) {
            break;
        }
        end = i + c.len_utf8();
    }
    let fits = &text[..end];
    let sentence = fits
        .char_indices()
        .rfind(|&(i, c)| {
            matches!(c, '.' | '?' | '!' | ';') && text.as_bytes().get(i + 1) == Some(&b' ')
        })
        .map(|(i, _)| i + 1);
    if let Some(i) = sentence.filter(|&i| utf16(&text[..i]) >= limit / 2) {
        return text[..i].to_string();
    }
    let cut = match fits.rfind(' ') {
        Some(i) if utf16(&fits[..i]) >= limit / 2 => &fits[..i],
        _ => fits,
    };
    format!("{}…", cut.trim_end_matches([' ', ',', ';', ':']))
}

/// Text made short for a retro line: whitespace folded, each link a short
/// label ([`link_label`]), a long commit id its first 7 characters, and
/// credentials masked ([`scrub`]).
fn shorten(text: &str) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| {
            if let Some(i) = w.find("http://").or_else(|| w.find("https://")) {
                let (pre, url) = w.split_at(i);
                let core = url.trim_end_matches(|c: char| ").,;:!?'\"]>".contains(c));
                return format!("{pre}{}{}", link_label(core), &url[core.len()..]);
            }
            let start = w.find(|c: char| c.is_ascii_hexdigit()).unwrap_or(0);
            let run: String = w[start..]
                .chars()
                .take_while(char::is_ascii_hexdigit)
                .collect();
            let hex = run.len() >= 20
                && run.chars().any(|c| c.is_ascii_digit())
                && run.chars().any(|c| c.is_ascii_alphabetic())
                && !w[start + run.len()..].starts_with(|c: char| c.is_alphanumeric());
            if hex {
                return format!("{}{}{}", &w[..start], &run[..7], &w[start + run.len()..]);
            }
            w.to_string()
        })
        .collect();
    scrub(&words.join(" "))
}

/// A short label for a link: `repo#12` for a GitHub pull request or issue,
/// `repo@abc1234` for a commit, `repo` for a repository, else a file name
/// at the end of the path, else the host.
fn link_label(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.trim_start_matches("www.");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if host == "github.com" {
        match segs.as_slice() {
            [_, repo, "pull" | "issues", n, ..] => return format!("{repo}#{n}"),
            [_, repo, "commit", sha, ..] => {
                return format!("{repo}@{}", sha.chars().take(7).collect::<String>())
            }
            [_, repo, ..] => return repo.to_string(),
            _ => {}
        }
    }
    match segs.last() {
        Some(f) if f.contains('.') && f.chars().count() <= 32 => f.to_string(),
        _ if host.is_empty() => "link".into(),
        _ => host.to_string(),
    }
}

/// Masks what looks like a credential in text shown on a retro card: the
/// value of a `token`/`secret`/`password`/`key` pair or the word after one,
/// a known token prefix, or a long run of mixed letters and digits.
pub fn scrub(text: &str) -> String {
    const KEYS: [&str; 6] = ["token", "secret", "password", "passwd", "apikey", "api_key"];
    const PREFIXES: [&str; 6] = ["ghp_", "gho_", "ghs_", "github_pat_", "sk-", "xox"];
    const MASK: &str = "[redacted]";
    let is_key = |k: &str| {
        let k = k.to_lowercase();
        KEYS.iter().any(|x| k.ends_with(x))
    };
    let looks_secret = |w: &str| {
        let core = w.trim_matches(|c: char| !c.is_alphanumeric());
        let long = core.len() >= 24
            && core.chars().any(|c| c.is_ascii_digit())
            && core.chars().any(|c| c.is_ascii_alphabetic())
            && core
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_+/=.".contains(c));
        long || (core.len() >= 12 && PREFIXES.iter().any(|p| core.starts_with(p)))
    };
    let mut out: Vec<String> = Vec::new();
    let mut after_key = false;
    for w in text.split(' ') {
        let pair = w
            .find(['=', ':'])
            .filter(|&i| i > 0 && i + 1 < w.len() && is_key(&w[..i]));
        out.push(match pair {
            Some(i) => format!("{}{MASK}", &w[..=i]),
            None if looks_secret(w)
                || (after_key && w.len() >= 8 && !w.chars().all(char::is_alphabetic)) =>
            {
                MASK.into()
            }
            None => w.into(),
        });
        after_key = is_key(w.trim_end_matches([':', '=']));
    }
    out.join(" ")
}

/// The footer: each kind's ask count, most first, `other` last.
fn kind_totals(items: &[RetroItem]) -> String {
    let mut by: HashMap<&str, usize> = HashMap::new();
    for it in items {
        let k = if it.kind.is_empty() {
            "other"
        } else {
            &it.kind
        };
        *by.entry(k).or_default() += 1;
    }
    let order = |k: &str| {
        KINDS
            .iter()
            .position(|x| x.0 == k)
            .unwrap_or(KINDS.len() - 1)
    };
    let mut by: Vec<(&str, usize)> = by.into_iter().collect();
    by.sort_by(|a, b| {
        (a.0 == "other")
            .cmp(&(b.0 == "other"))
            .then(b.1.cmp(&a.1))
            .then(order(a.0).cmp(&order(b.0)))
            .then(a.0.cmp(b.0))
    });
    by.iter()
        .map(|(k, n)| format!("{} {n}", kind_label(k)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The page of the card that holds ask `ask` (0 when it holds none).
pub fn page_of(items: &[RetroItem], ask: &str) -> usize {
    items
        .iter()
        .position(|i| i.ask == ask)
        .map_or(0, |p| p / RETRO_PAGE)
}

/// A retro card's title: whose retro it is.
fn retro_title(bot: Option<&str>) -> String {
    match bot {
        Some(b) => format!("Weekly retro, {}", crate::board::cap(b, 32)),
        None => "Weekly retro".to_string(),
    }
}

/// The text a dismissed retro card is edited to: it has no buttons, and its
/// unrated asks stay unrated.
pub fn dismissed_retro(
    items: &[RetroItem],
    rated: &HashMap<String, String>,
    bot: Option<&str>,
) -> String {
    let left = items.iter().filter(|i| !rated.contains_key(&i.ask)).count();
    format!(
        "{}: dismissed. {left} of {} asks left unrated.",
        retro_title(bot),
        items.len()
    )
}

/// Renders page `page` of a retro card from its items, with `rated` (ask id
/// to rating) marked: two lines and one row of rating buttons per ask (a
/// rated ask can be re-rated), prev / next, a Dismiss row, and the kind
/// totals as a footer. `bot` names whose retro it is. Pure, so presses re-render the
/// stored items.
pub fn render_retro(
    items: &[RetroItem],
    more: usize,
    rated: &HashMap<String, String>,
    page: usize,
    bot: Option<&str>,
) -> Retro {
    let title = retro_title(bot);
    if items.is_empty() {
        return Retro {
            text: format!("{title}: no unrated asks last week."),
            ..Default::default()
        };
    }
    let pages = items.len().div_ceil(RETRO_PAGE);
    let page = page.min(pages - 1);
    let left = items.iter().filter(|i| !rated.contains_key(&i.ask)).count();
    let mut text = format!(
        "{title}: last week's asks ({}, {left} unrated)",
        items.len() + more
    );
    if pages > 1 {
        text.push_str(&format!(", page {}/{pages}", page + 1));
    }
    text.push('\n');
    let mut buttons = Vec::new();
    let mut rows = Vec::new();
    for (p, it) in items
        .iter()
        .enumerate()
        .skip(page * RETRO_PAGE)
        .take(RETRO_PAGE)
    {
        let n = p + 1;
        let mine = rated.get(&it.ask).map(String::as_str);
        match mine {
            Some(r) => text.push_str(&format!("\n{n}. {} = {}", it.line, rating_label(r))),
            None => text.push_str(&format!("\n{n}. {}", it.line)),
        }
        if !it.detail.is_empty() {
            text.push_str(&format!("\n   {}", it.detail));
        }
        for (code, rating, label) in CODES {
            let label = if mine == Some(rating) {
                format!("{n} {label} ✓")
            } else {
                format!("{n} {label}")
            };
            buttons.push((label, format!("rate:{}:{code}", it.ask)));
        }
        rows.push(CODES.len());
    }
    let before = buttons.len();
    if page > 0 {
        buttons.push(("‹ prev".into(), format!("rp:{}", page - 1)));
    }
    if page + 1 < pages {
        buttons.push(("next ›".into(), format!("rp:{}", page + 1)));
    }
    if buttons.len() > before {
        rows.push(buttons.len() - before);
    }
    buttons.push(("Dismiss".into(), DISMISS.into()));
    rows.push(1);
    if more > 0 {
        text.push_str(&format!(
            "\n+{more} more not on this card: axe ask list --unrated"
        ));
    }
    text.push_str("\n\n");
    text.push_str(if left == 0 {
        "All rated. Thanks."
    } else {
        NOTE
    });
    text.push_str(&format!("\nBy kind: {}", kind_totals(items)));
    Retro {
        text,
        buttons,
        rows,
        items: items.to_vec(),
        more,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kitem(i: usize, kind: &str, bot: &str) -> RetroItem {
        RetroItem {
            ask: format!("t{i}.1"),
            line: format!("#{i}: thing {i}"),
            kind: kind.into(),
            bot: bot.into(),
            ..Default::default()
        }
    }

    fn ask(id: &str) -> Ask {
        Ask {
            id: id.into(),
            kind: "card".into(),
            category: String::new(),
            bot: None,
            project: "p".into(),
            task: None,
            at: ts(0),
            summary: String::new(),
            question: String::new(),
            asked: String::new(),
            rating: None,
            rated_by: None,
            rated_at: None,
        }
    }

    #[test]
    fn retro_renders_zero_one_and_marks_ratings() {
        let r = render_retro(&[], 0, &HashMap::new(), 0, Some("a"));
        assert!(r.buttons.is_empty() && r.items.is_empty());
        assert_eq!(r.text, "Weekly retro, a: no unrated asks last week.");

        let r = render_retro(
            &[kitem(7, "decision", "a")],
            0,
            &HashMap::new(),
            0,
            Some("a"),
        );
        assert_eq!(
            r.buttons,
            [
                ("1 needed".to_string(), "rate:t7.1:n".to_string()),
                ("1 decide".into(), "rate:t7.1:c".into()),
                ("1 drop".into(), "rate:t7.1:x".into()),
                ("Dismiss".into(), DISMISS.into()),
            ]
        );
        assert_eq!(r.rows, [3, 1]);
        assert_eq!(Press::parse("rd"), Some(Press::Dismiss));
        assert!(
            r.text.starts_with(
                "Weekly retro, a: last week's asks (1, 1 unrated)\n\n1. #7: thing 7\n"
            ),
            "{}",
            r.text
        );
        assert!(r.text.ends_with("\nBy kind: decisions 1"), "{}", r.text);

        // A rated ask shows its rating and can be re-rated.
        let rated = HashMap::from([("t7.1".to_string(), "could_decide".to_string())]);
        let r = render_retro(&[kitem(7, "decision", "a")], 0, &rated, 0, Some("a"));
        assert!(r.text.contains("1. #7: thing 7 = decide"), "{}", r.text);
        assert!(r
            .buttons
            .contains(&("1 decide ✓".into(), "rate:t7.1:c".into())));
        assert_eq!(r.rows, [3, 1]);
        assert!(r.text.contains("All rated. Thanks."), "{}", r.text);

        // Dismissed: no buttons, the unrated asks counted as left unrated.
        let two = [kitem(7, "decision", "a"), kitem(8, "confirm", "a")];
        assert_eq!(
            dismissed_retro(&two, &rated, Some("a")),
            "Weekly retro, a: dismissed. 1 of 2 asks left unrated."
        );
    }

    fn lines(a: &Ask) -> (String, String) {
        retro_lines(a, &CardText::default())
    }

    #[test]
    fn retro_lines_hold_the_topic_then_the_question() {
        // An escalation: its title, then the blocker and the ask.
        let a = Ask {
            kind: "escalation".into(),
            task: Some(452),
            summary: "web: render performance fixes".into(),
            question: "Merge decision for https://github.com/acme/web/pull/1814 at head \
                       d12b8497183262fe63119d6601f3804c3bd87a80 (all checks green)."
                .into(),
            asked: "owner (Sam): approve with axe approve 452 --scope pr-merge, \
                    or request changes"
                .into(),
            ..ask("t452.1")
        };
        let (l1, l2) = lines(&a);
        assert_eq!(l1, "#452: web: render performance fixes");
        assert_eq!(
            l2,
            "Merge decision for web#1814 at head d12b849 (all checks green). \
             Ask: approve with axe approve 452 --scope pr-merge, or request changes"
        );
        // A later episode is `#<task>.<n>`; a long blocker is cut at a
        // sentence and keeps room for the ask.
        let long = Ask {
            kind: "escalation".into(),
            task: Some(903),
            summary: "the task title ".repeat(20),
            question: "First the situation in brief. ".to_string() + &"more words ".repeat(40),
            asked: "Sam: reply A or B".into(),
            ..ask("t903.2")
        };
        let (l1, l2) = lines(&long);
        assert!(
            l1.starts_with("#903.2: the task title") && l1.ends_with('…'),
            "{l1}"
        );
        assert!(utf16(&l1) <= RETRO_TOPIC, "{l1}");
        assert!(
            l2.starts_with("First the situation in brief. more words"),
            "{l2}"
        );
        assert!(l2.ends_with("… Ask: reply A or B"), "{l2}");
        assert!(utf16(&l2) <= RETRO_QUESTION, "{l2}");
        // No blocked line: the resolver alone; nothing at all: one line.
        let bare = Ask {
            kind: "escalation".into(),
            task: Some(8),
            summary: "title".into(),
            asked: "owner: pick a host".into(),
            ..ask("t8.1")
        };
        assert_eq!(lines(&bare), ("#8: title".into(), "pick a host".into()));
        let none = Ask {
            asked: String::new(),
            ..bare
        };
        assert_eq!(lines(&none), ("#8: title".into(), String::new()));

        // A card: the task it opens with, else what it replies to, else
        // its first sentence; then its text, question kept.
        let card = ask("m55");
        let text = |body: &str, task: Option<&str>, reply: Option<&str>| {
            retro_lines(
                &card,
                &CardText {
                    body: body.into(),
                    task: task.map(String::from),
                    reply: reply.map(String::from),
                },
            )
        };
        assert_eq!(
            text(
                "#479: docs PR 152 ready.\nMerge it?",
                Some("#479 Sync release docs"),
                None
            ),
            (
                "msg 55: #479 Sync release docs".into(),
                "#479: docs PR 152 ready. Merge it?".into()
            )
        );
        assert_eq!(
            text(
                "Which ones? I won't guess.",
                None,
                Some("Merge the ready PRs. Thanks")
            ),
            (
                "msg 55: re: Merge the ready PRs.".into(),
                "Which ones? I won't guess.".into()
            )
        );
        assert_eq!(
            text(
                "Weekly focus draft.\n- item one\n- item two\nPost it?",
                None,
                None
            ),
            (
                "msg 55: Weekly focus draft.".into(),
                "item one. item two. Post it?".into()
            )
        );
        let (_, q) = text(&("Context. ".repeat(40) + "Ship it on Friday?"), None, None);
        assert!(q.ends_with("… Ship it on Friday?"), "{q}");
        assert!(utf16(&q) <= RETRO_QUESTION, "{q}");
        // No stored text: the question alone.
        let old = Ask {
            question: "Ship it?".into(),
            ..ask("m56")
        };
        assert_eq!(lines(&old), ("msg 56: Ship it?".into(), String::new()));
        // Credentials never reach the card.
        let leak = Ask {
            kind: "escalation".into(),
            task: Some(1),
            question: "login failed with token=abc123SECRET and ghp_0123456789abcdef".into(),
            ..ask("t1.1")
        };
        let (_, l2) = lines(&leak);
        assert!(!l2.contains("abc123") && !l2.contains("ghp_"), "{l2}");
    }

    #[test]
    fn links_become_short_labels() {
        assert_eq!(
            shorten("PR https://github.com/acme/web/pull/1815 ready"),
            "PR web#1815 ready"
        );
        assert_eq!(
            shorten("(https://github.com/acme/web/issues/9)."),
            "(web#9)."
        );
        assert_eq!(
            shorten("see https://github.com/acme/api/commit/0123456789abcdef0123"),
            "see api@0123456"
        );
        assert_eq!(shorten("repo https://github.com/acme/api"), "repo api");
        assert_eq!(
            shorten("report: http://files.lan/proj/task-7/report.md?x=1"),
            "report: report.md"
        );
        assert_eq!(
            shorten("dash https://grafana.example.com/d/abc/x?from=now"),
            "dash grafana.example.com"
        );
        // A full commit id becomes 7 characters, not a redaction.
        assert_eq!(
            shorten("at 0123456789abcdef0123456789abcdef01234567,"),
            "at 0123456,"
        );
        assert_eq!(shorten("v38.12 at 0dc72e1"), "v38.12 at 0dc72e1");
    }

    #[test]
    fn scrub_masks_credentials_only() {
        assert_eq!(scrub("merge PR 12 to main"), "merge PR 12 to main");
        assert_eq!(scrub("set API_KEY=xyz now"), "set API_KEY=[redacted] now");
        assert_eq!(scrub("password: hunter22!"), "password: [redacted]");
        assert_eq!(scrub("token expired again"), "token expired again");
        assert_eq!(
            scrub("sha 0123456789abcdef0123456789abcdef01234567"),
            "sha [redacted]"
        );
        assert_eq!(scrub("use sk-ant-0123456789xyz"), "use [redacted]");
        assert_eq!(scrub("approve #46 at 0dc72e1"), "approve #46 at 0dc72e1");
    }

    #[test]
    fn retro_pages_fit_telegram_with_kind_totals_as_footer() {
        // Long questions, every kind, many asks: every page stays one message.
        let long = "x".repeat(RETRO_QUESTION);
        let mut items = Vec::new();
        for (k, (kind, _)) in KINDS.iter().enumerate() {
            for i in 0..(k + 1) * 4 {
                items.push(RetroItem {
                    ask: format!("t{}.{}", 100_000 + k * 1000 + i, 12),
                    line: format!(
                        "#{}.12: {}",
                        100_000 + k * 1000 + i,
                        "t".repeat(RETRO_TOPIC - 12)
                    ),
                    detail: long.clone(),
                    kind: kind.to_string(),
                    bot: "a-very-long-bot-name-indeed".into(),
                });
            }
        }
        let total = items.len();
        let pages = total.div_ceil(RETRO_PAGE);
        assert!(pages > 3);
        let rated: HashMap<String, String> = items
            .iter()
            .step_by(2)
            .map(|i| (i.ask.clone(), "should_not_exist".to_string()))
            .collect();
        let bot = "b".repeat(64);
        let mut seen = 0;
        for page in 0..pages {
            let r = render_retro(&items, 40, &rated, page, Some(&bot));
            assert!(
                crate::board::utf16(&r.text) < 4096,
                "page {page}: {}",
                r.text.len()
            );
            for (label, data) in &r.buttons {
                assert!(data.len() <= 64 && label.len() <= 20, "{label} {data}");
            }
            let n = r.rows.iter().filter(|n| **n == 3).count();
            assert!(n <= RETRO_PAGE);
            seen += n;
            assert!(r.text.contains(&format!("page {}/{pages}", page + 1)));
            // Prev / next only where there is somewhere to go.
            let nav: Vec<&str> = r
                .buttons
                .iter()
                .map(|b| b.1.as_str())
                .filter(|d| d.starts_with("rp:"))
                .collect();
            let want: Vec<String> = [page.checked_sub(1), (page + 1 < pages).then_some(page + 1)]
                .into_iter()
                .flatten()
                .map(|p| format!("rp:{p}"))
                .collect();
            assert_eq!(nav, want);
            // The kind totals are the footer, and add up to every ask.
            let footer = r.text.lines().last().unwrap();
            assert!(footer.starts_with("By kind: "), "{footer}");
            let sum: usize = footer
                .trim_start_matches("By kind: ")
                .split(", ")
                .map(|p| p.rsplit(' ').next().unwrap().parse::<usize>().unwrap())
                .sum();
            assert_eq!(sum, total);
            assert!(footer.ends_with("other 32"), "{footer}");
        }
        assert_eq!(seen, total);
        // A page past the end shows the last one.
        assert_eq!(
            render_retro(&items, 0, &rated, 99, None).text,
            render_retro(&items, 0, &rated, pages - 1, None).text
        );
        assert_eq!(page_of(&items, &items[RETRO_PAGE].ask), 1);
        assert_eq!(page_of(&items, "t0.1"), 0);
        // A stored card for RETRO_ITEMS asks stays a modest flags value.
        let full: Vec<_> = (0..RETRO_ITEMS)
            .map(|i| RetroItem {
                detail: "d".repeat(RETRO_QUESTION),
                ..kitem(i, "decision", "bot")
            })
            .collect();
        assert!(serde_json::to_string(&full).unwrap().len() < 300_000);
    }

    #[test]
    fn retro_scope_holds_own_asks_and_home_holds_the_rest() {
        let senders = s(&["a", "b"]);
        let a = RetroScope {
            bot: Some("a"),
            home: false,
            senders: &senders,
        };
        assert!(a.holds(Some("a")));
        assert!(!a.holds(Some("b")) && !a.holds(None) && !a.holds(Some("gone")));
        let home = RetroScope {
            bot: Some("b"),
            home: true,
            senders: &senders,
        };
        assert!(home.holds(Some("b")) && home.holds(None) && home.holds(Some("gone")));
        assert!(!home.holds(Some("a")));
        let all = RetroScope::default();
        assert!(all.holds(Some("a")) && all.holds(None));
    }

    #[test]
    fn presses_parse() {
        assert_eq!(
            Press::parse("rate:t7.1:n"),
            Some(Press::Item("t7.1".into(), "needed"))
        );
        assert_eq!(
            Press::parse("rate:m12:x"),
            Some(Press::Item("m12".into(), "should_not_exist"))
        );
        assert_eq!(Press::parse("rp:3"), Some(Press::Page(3)));
        // Buttons of a card that rated by kind.
        assert_eq!(Press::parse("rk:approval:c"), Some(Press::Retired));
        assert_eq!(Press::parse("ro:decision:3"), Some(Press::Retired));
        assert_eq!(Press::parse("rb"), Some(Press::Retired));
        for bad in ["rate:t7.1:q", "rk:approval", "rp:x", "zz:a:b", ""] {
            assert_eq!(Press::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn ask_ids_parse() {
        assert_eq!(AskId::parse("t903.2").unwrap(), AskId::Task(903, Some(2)));
        assert_eq!(AskId::parse("#903").unwrap(), AskId::Task(903, None));
        assert_eq!(AskId::parse("m12").unwrap(), AskId::Message(12));
        for bad in ["t903.0", "mx", "t.1", "", "x9"] {
            assert!(AskId::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tally_math() {
        let mut t = Tally::default();
        let mut a = ask("m1");
        t.add(&a);
        assert_eq!(t.wasted_share, None);
        for r in ["needed", "could_decide", "should_not_exist", "needed"] {
            a.rating = Some(r.into());
            t.add(&a);
        }
        assert_eq!((t.asks, t.rated, t.needed), (5, 4, 2));
        assert_eq!(t.wasted_share, Some(0.5));
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn kinds_derive_from_class_resolver_and_buttons() {
        let e = escalation_kind;
        assert_eq!(e(Some("capability"), "approve the merge"), "access");
        assert_eq!(e(Some("runtime"), ""), "stuck");
        assert_eq!(e(Some("coordination"), ""), "stuck");
        assert_eq!(
            e(Some("decision"), "owner: approve merging #46 at 0dc72e1"),
            "approval"
        );
        assert_eq!(e(None, "owner: OK to deploy both rows"), "approval");
        assert_eq!(e(Some("decision"), "owner: reply A, B or C"), "decision");
        assert_eq!(e(None, ""), "decision");
        // Stems are word starts: "emerging" is no merge.
        assert_eq!(e(None, "an emerging pattern"), "decision");
        assert_eq!(
            asked_of("blocked: built it\nresolver: owner: approve the merge\nresume: x"),
            "owner: approve the merge"
        );
        assert_eq!(asked_of("blocked: a\nmore"), "blocked: a");

        let c = |v: &[&str]| card_kind(&s(v));
        assert_eq!(c(&["Merge", "Changes"]), "approval");
        assert_eq!(c(&["Revert", "Revert+stop", "Hold"]), "approval");
        assert_eq!(c(&["#495", "#395", "#416"]), "digest");
        assert_eq!(c(&["Later", "#531", "#544"]), "digest");
        assert_eq!(c(&["A", "B", "C"]), "choice");
        assert_eq!(c(&["Yes", "No"]), "confirm");
        assert_eq!(c(&["Reply in chat"]), "other");
        assert_eq!(c(&[]), "other");
        // Every rule lands on a listed kind.
        for k in [
            "access", "stuck", "approval", "decision", "digest", "choice", "confirm", "other",
        ] {
            assert!(KINDS.iter().any(|x| x.0 == k), "{k}");
        }
        assert_eq!(KINDS.last().unwrap().0, "other");
    }

    #[test]
    fn weekly_send_is_due_once_per_slot() {
        let mon_1am = parse_weekly("mon 01:00").unwrap();
        assert_eq!(mon_1am, 3600);
        assert_eq!(
            parse_weekly("Sunday 23:59").unwrap(),
            6 * 86_400 + 23 * 3600 + 59 * 60
        );
        for bad in ["mon", "xyz 01:00", "mon 24:00", "mon 1:60", "mon one:00"] {
            assert!(parse_weekly(bad).is_err(), "{bad}");
        }
        // 2026-11-16 is a Monday.
        let slot = epoch_of("2026-11-16T01:00:00Z").unwrap();
        assert_eq!(last_slot(slot, mon_1am), slot);
        assert_eq!(last_slot(slot - 1, mon_1am), slot - WEEK);
        assert_eq!(last_slot(slot + 3 * 86_400, mon_1am), slot);
        let wed = parse_weekly("wed 09:30").unwrap();
        assert_eq!(ts(last_slot(slot, wed)), "2026-11-11T09:30:00Z");

        // Due at the slot and up to a day late, once.
        assert_eq!(retro_due(slot, mon_1am, None, None), Some(slot));
        assert_eq!(
            retro_due(slot + RETRO_CATCH_UP, mon_1am, None, None),
            Some(slot)
        );
        assert_eq!(
            retro_due(slot + RETRO_CATCH_UP + 1, mon_1am, None, None),
            None
        );
        assert_eq!(retro_due(slot - 60, mon_1am, None, None), None);
        assert_eq!(retro_due(slot + 60, mon_1am, None, Some(slot)), None);
        // A card sent since the slot (by hand or before a restart) counts;
        // last week's does not.
        assert_eq!(retro_due(slot + 60, mon_1am, Some(slot + 10), None), None);
        assert_eq!(
            retro_due(slot + 60, mon_1am, Some(slot - WEEK), None),
            Some(slot)
        );
    }
}
