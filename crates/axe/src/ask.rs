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
                    ep.2 = asked_of(body.as_deref().unwrap_or_default());
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

/// Rates every ask of `ids` in one transaction (a retro kind rating);
/// returns how many were rated.
pub fn rate_many(
    conn: &mut Connection,
    actor: &Actor,
    ids: &[String],
    rating: &str,
) -> Result<usize> {
    check_rater(actor, rating)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for id in ids {
        write_rating(&tx, actor, id, rating)?;
    }
    tx.commit()?;
    Ok(ids.len())
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
/// Items per page when a kind is opened.
pub const RETRO_PAGE: usize = 8;
/// Bots named on a kind line; the rest are summed as `+N`.
const RETRO_LINE_BOTS: usize = 3;
const RETRO_SUMMARY: usize = 40;
const RETRO_BOT: usize = 16;
/// Button codes in callback data `rate:<ask>:<code>` and `rk:<kind>:<code>`.
pub const CODES: [(&str, &str, &str); 3] = [
    ("n", "needed", "needed"),
    ("c", "could_decide", "decide"),
    ("x", "should_not_exist", "drop"),
];
const NOTE: &str =
    "needed: right to ask. decide: the bot could have decided. drop: should not exist.";

/// The rating a button code stands for.
pub fn rating_of(code: &str) -> Option<&'static str> {
    CODES.iter().find(|c| c.0 == code).map(|c| c.1)
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

/// When the latest retro card was sent, by any bot.
pub fn last_retro(conn: &Connection) -> Result<Option<i64>> {
    let at: Option<String> = conn
        .query_row(
            "SELECT at FROM bot_messages WHERE direction='out' AND json_type(flags,'$.retro') IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(at.as_deref().and_then(epoch_of))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetroItem {
    pub ask: String,
    pub line: String,
    /// The ask's category. Empty on cards sent before kinds existed: those
    /// render as a flat list.
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

/// What a retro card shows: every kind, or one kind's page of items.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Kinds,
    Kind(String, usize),
}

/// What a press on a retro card asks for (callback data).
#[derive(Debug, Clone, PartialEq)]
pub enum Press {
    /// `rate:<ask>:<code>`: rate one item.
    Item(String, &'static str),
    /// `rk:<kind>:<code>`: rate a kind's unrated items.
    Kind(String, &'static str),
    /// `ro:<kind>:<page>`: open a kind's page.
    Open(String, usize),
    /// `rb`: back to every kind.
    Back,
}

impl Press {
    pub fn parse(data: &str) -> Option<Press> {
        if data == "rb" {
            return Some(Press::Back);
        }
        let (head, rest) = data.split_once(':')?;
        let (arg, last) = rest.rsplit_once(':')?;
        match head {
            "rate" => Some(Press::Item(arg.into(), rating_of(last)?)),
            "rk" => Some(Press::Kind(arg.into(), rating_of(last)?)),
            "ro" => Some(Press::Open(arg.into(), last.parse().ok()?)),
            _ => None,
        }
    }
}

/// The weekly retro card: the owner's asks of the 7 days before `now` that
/// nobody rated yet, grouped by kind. An item left alone stays unrated; it
/// is never taken as `needed`.
pub fn retro(conn: &Connection, now: i64) -> Result<Retro> {
    let asks = list(
        conn,
        &Filter {
            since: now - WEEK,
            // Through the current second.
            until: now + 1,
            unrated: true,
            ..Default::default()
        },
    )?;
    let more = asks.len().saturating_sub(RETRO_ITEMS);
    let items = asks
        .iter()
        .take(RETRO_ITEMS)
        .map(|a| RetroItem {
            ask: a.id.clone(),
            line: retro_line(a),
            kind: a.category.clone(),
            bot: a.bot.clone().unwrap_or_else(|| "-".into()),
        })
        .collect::<Vec<_>>();
    Ok(render_retro(&items, more, &HashMap::new(), &View::Kinds))
}

fn retro_line(a: &Ask) -> String {
    let bot = crate::board::cap(a.bot.as_deref().unwrap_or("-"), RETRO_BOT);
    let what = match a.task {
        Some(t) if a.id.ends_with(".1") => format!("task #{t}"),
        Some(_) => format!("task #{}", a.id.trim_start_matches('t')),
        None => format!("card #{}", a.id.trim_start_matches('m')),
    };
    let summary = a.summary.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{bot} {what}: {}",
        crate::board::cap(&summary, RETRO_SUMMARY)
    )
}

/// Kinds on a card with their items' indices, most items first, `other`
/// last.
pub fn kinds_of(items: &[RetroItem]) -> Vec<(String, Vec<usize>)> {
    let mut by: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        let k = if it.kind.is_empty() {
            "other"
        } else {
            &it.kind
        };
        by.entry(k).or_default().push(i);
    }
    let order = |k: &str| {
        KINDS
            .iter()
            .position(|x| x.0 == k)
            .unwrap_or(KINDS.len() - 1)
    };
    let mut out: Vec<(String, Vec<usize>)> =
        by.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    out.sort_by(|a, b| {
        (a.0 == "other")
            .cmp(&(b.0 == "other"))
            .then(b.1.len().cmp(&a.1.len()))
            .then(order(&a.0).cmp(&order(&b.0)))
            .then(a.0.cmp(&b.0))
    });
    out
}

/// The items of kind `kind` that `rated` has no rating for.
pub fn unrated_of(items: &[RetroItem], rated: &HashMap<String, String>, kind: &str) -> Vec<String> {
    kinds_of(items)
        .into_iter()
        .filter(|(k, _)| k == kind)
        .flat_map(|(_, ix)| ix)
        .map(|i| items[i].ask.clone())
        .filter(|a| !rated.contains_key(a))
        .collect()
}

/// The page of `kind` that holds item `ask`.
pub fn page_of(items: &[RetroItem], kind: &str, ask: &str) -> usize {
    kinds_of(items)
        .into_iter()
        .find(|(k, _)| k == kind)
        .and_then(|(_, ix)| ix.iter().position(|i| items[*i].ask == ask))
        .map_or(0, |p| p / RETRO_PAGE)
}

/// Renders a retro card from its items, with `rated` (ask id to rating)
/// marked. Pure, so presses re-render the stored items.
pub fn render_retro(
    items: &[RetroItem],
    more: usize,
    rated: &HashMap<String, String>,
    view: &View,
) -> Retro {
    if items.is_empty() {
        return Retro {
            text: "Weekly retro: no unrated asks last week.".into(),
            ..Default::default()
        };
    }
    let mut r = if items.iter().any(|i| i.kind.is_empty()) {
        render_flat(items, more, rated)
    } else {
        match view {
            View::Kinds => render_kinds(items, more, rated),
            View::Kind(k, page) => render_kind(items, rated, k, *page),
        }
    };
    r.items = items.to_vec();
    r.more = more;
    r
}

fn bots_of(items: &[RetroItem], ix: &[usize]) -> String {
    let mut n: BTreeMap<&str, usize> = BTreeMap::new();
    for i in ix {
        *n.entry(items[*i].bot.as_str()).or_default() += 1;
    }
    let mut n: Vec<_> = n.into_iter().collect();
    n.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut parts: Vec<String> = n
        .iter()
        .take(RETRO_LINE_BOTS)
        .map(|(b, c)| format!("{} {c}", crate::board::cap(b, RETRO_BOT)))
        .collect();
    let rest: usize = n.iter().skip(RETRO_LINE_BOTS).map(|x| x.1).sum();
    if rest > 0 {
        parts.push(format!("+{rest}"));
    }
    parts.join(", ")
}

/// Every kind on one line with its count per bot, three rating buttons and
/// an open button. A kind rating rates its unrated items.
fn render_kinds(items: &[RetroItem], more: usize, rated: &HashMap<String, String>) -> Retro {
    let left = items.iter().filter(|i| !rated.contains_key(&i.ask)).count();
    let mut text = format!(
        "Weekly retro: last week's asks by kind ({}, {left} unrated)\n",
        items.len() + more
    );
    let mut buttons = Vec::new();
    let mut rows = Vec::new();
    for (n, (kind, ix)) in kinds_of(items).iter().enumerate() {
        let n = n + 1;
        let ratings: Vec<Option<&String>> = ix.iter().map(|i| rated.get(&items[*i].ask)).collect();
        let unrated = ratings.iter().filter(|r| r.is_none()).count();
        let state = if unrated == 0 {
            let first = ratings[0];
            if ratings.iter().all(|r| *r == first) {
                format!(" = {}", first.map_or("", |s| s.as_str()))
            } else {
                " = rated".into()
            }
        } else if unrated < ix.len() {
            format!(", {unrated} unrated")
        } else {
            String::new()
        };
        text.push_str(&format!(
            "\n{n}. {}: {} ({}){state}",
            kind_label(kind),
            ix.len(),
            bots_of(items, ix)
        ));
        let before = buttons.len();
        if unrated > 0 {
            for (code, _, label) in CODES {
                buttons.push((format!("{n} {label}"), format!("rk:{kind}:{code}")));
            }
        }
        buttons.push((format!("{n} open"), format!("ro:{kind}:0")));
        rows.push(buttons.len() - before);
    }
    if more > 0 {
        text.push_str(&format!(
            "\n+{more} more not on this card: axe ask list --unrated"
        ));
    }
    if left == 0 {
        text.push_str("\n\nAll rated. Thanks.");
    } else {
        text.push_str(&format!(
            "\n\nRate a kind once: it applies to its unrated asks. Open a kind to see or re-rate single asks. {NOTE} Skipped asks stay unrated."
        ));
    }
    Retro {
        text,
        buttons,
        rows,
        ..Default::default()
    }
}

/// One page of a kind's items, each with its rating buttons (a rated item
/// can be re-rated), and prev / next / back.
fn render_kind(
    items: &[RetroItem],
    rated: &HashMap<String, String>,
    kind: &str,
    page: usize,
) -> Retro {
    let ix: Vec<usize> = kinds_of(items)
        .into_iter()
        .find(|(k, _)| k == kind)
        .map(|(_, ix)| ix)
        .unwrap_or_default();
    let pages = ix.len().div_ceil(RETRO_PAGE).max(1);
    let page = page.min(pages - 1);
    let unrated = ix
        .iter()
        .filter(|i| !rated.contains_key(&items[**i].ask))
        .count();
    let mut text = format!(
        "Weekly retro > {}: {}, {unrated} unrated",
        kind_label(kind),
        ix.len()
    );
    if pages > 1 {
        text.push_str(&format!(" (page {}/{pages})", page + 1));
    }
    text.push('\n');
    let mut buttons = Vec::new();
    let mut rows = Vec::new();
    for (p, i) in ix
        .iter()
        .enumerate()
        .skip(page * RETRO_PAGE)
        .take(RETRO_PAGE)
    {
        let (n, it) = (p + 1, &items[*i]);
        match rated.get(&it.ask) {
            Some(r) => text.push_str(&format!("\n{n}. {} = {r}", it.line)),
            None => text.push_str(&format!("\n{n}. {}", it.line)),
        }
        for (code, rating, label) in CODES {
            let label = if rated.get(&it.ask).map(String::as_str) == Some(rating) {
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
        buttons.push(("‹ prev".into(), format!("ro:{kind}:{}", page - 1)));
    }
    if page + 1 < pages {
        buttons.push(("next ›".into(), format!("ro:{kind}:{}", page + 1)));
    }
    buttons.push(("‹ all kinds".into(), "rb".into()));
    rows.push(buttons.len() - before);
    text.push_str(&format!("\n\n{NOTE}"));
    Retro {
        text,
        buttons,
        rows,
        ..Default::default()
    }
}

/// A card sent before kinds existed: one row of buttons per unrated item.
fn render_flat(items: &[RetroItem], more: usize, rated: &HashMap<String, String>) -> Retro {
    let mut text = format!(
        "Weekly retro: rate last week's asks ({})\n",
        items.len() + more
    );
    let mut buttons = Vec::new();
    let mut rows = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let n = i + 1;
        match rated.get(&it.ask) {
            Some(r) => text.push_str(&format!("\n{n}. {} = {r}", it.line)),
            None => {
                text.push_str(&format!("\n{n}. {}", it.line));
                for (code, _, label) in CODES {
                    buttons.push((format!("{n} {label}"), format!("rate:{}:{code}", it.ask)));
                }
                rows.push(CODES.len());
            }
        }
    }
    if more > 0 {
        text.push_str(&format!("\n+{more} more: axe ask list --unrated"));
    }
    text.push_str(if buttons.is_empty() {
        "\n\nAll rated. Thanks."
    } else {
        "\n\nneeded: right to ask. decide: the bot could have decided. drop: should not exist. Skipped items stay unrated."
    });
    Retro {
        text,
        buttons,
        rows,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An item of a card sent before kinds existed (flat layout).
    fn item(i: usize) -> RetroItem {
        RetroItem {
            ask: format!("t{i}.1"),
            line: format!("bot task #{i}: thing {i}"),
            ..Default::default()
        }
    }

    fn kitem(i: usize, kind: &str, bot: &str) -> RetroItem {
        RetroItem {
            ask: format!("t{i}.1"),
            line: format!("{bot} task #{i}: thing {i}"),
            kind: kind.into(),
            bot: bot.into(),
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
            rating: None,
            rated_by: None,
            rated_at: None,
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
    fn retro_renders_zero_one_and_marks_ratings() {
        let r = render_retro(&[], 0, &HashMap::new(), &View::Kinds);
        assert!(r.buttons.is_empty() && r.items.is_empty());
        assert_eq!(r.text, "Weekly retro: no unrated asks last week.");

        let r = render_retro(&[item(7)], 0, &HashMap::new(), &View::Kinds);
        assert_eq!(
            r.buttons,
            [
                ("1 needed".to_string(), "rate:t7.1:n".to_string()),
                ("1 decide".into(), "rate:t7.1:c".into()),
                ("1 drop".into(), "rate:t7.1:x".into()),
            ]
        );
        assert_eq!(r.rows, [3]);
        assert!(r.text.contains("\n1. bot task #7: thing 7\n"), "{}", r.text);

        let rated = HashMap::from([("t7.1".to_string(), "needed".to_string())]);
        let r = render_retro(&[item(7)], 0, &rated, &View::Kinds);
        assert!(r.buttons.is_empty());
        assert!(r.text.contains("1. bot task #7: thing 7 = needed"));
        assert!(r.text.ends_with("All rated. Thanks."));
    }

    #[test]
    fn retro_flat_many_fits_telegram() {
        let items: Vec<_> = (1..=10).map(item).collect();
        let r = render_retro(&items, 5, &HashMap::new(), &View::Kinds);
        assert_eq!(r.buttons.len(), 10 * 3);
        assert_eq!(r.rows, vec![3; 10]);
        assert!(r.text.contains("(15)") && r.text.contains("+5 more"));
        for (label, data) in &r.buttons {
            assert!(data.len() <= 64 && label.len() <= 16, "{label} {data}");
        }
        assert!(crate::board::utf16(&r.text) < 4096);
    }

    #[test]
    fn retro_lines_truncate() {
        let a = Ask {
            kind: "escalation".into(),
            bot: Some("a-very-long-bot-name-indeed".into()),
            task: Some(903),
            summary: "word ".repeat(30),
            ..ask("t903.2")
        };
        let line = retro_line(&a);
        assert!(
            line.starts_with("a-very-long-bot… task #903.2: word word"),
            "{line}"
        );
        assert!(
            line.chars().count() <= RETRO_BOT + RETRO_SUMMARY + 20,
            "{line}"
        );
        assert!(line.ends_with('…'));
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
    fn retro_groups_zero_one_and_many_kinds() {
        let r = render_retro(&[], 0, &HashMap::new(), &View::Kinds);
        assert!(r.buttons.is_empty() && r.text.contains("no unrated asks"));

        let one = [kitem(1, "approval", "a"), kitem(2, "approval", "b")];
        let r = render_retro(&one, 0, &HashMap::new(), &View::Kinds);
        assert!(
            r.text.contains("\n1. merge/deploy approvals: 2 (a 1, b 1)"),
            "{}",
            r.text
        );
        assert_eq!(
            r.buttons,
            [
                ("1 needed".to_string(), "rk:approval:n".to_string()),
                ("1 decide".into(), "rk:approval:c".into()),
                ("1 drop".into(), "rk:approval:x".into()),
                ("1 open".into(), "ro:approval:0".into()),
            ]
        );
        assert_eq!(r.rows, [4]);

        // Most items first, `other` last whatever its size; bots by count,
        // beyond three summed.
        let mut many = vec![kitem(1, "decision", "a")];
        many.extend((2..=6).map(|i| kitem(i, "other", "z")));
        many.extend((7..=9).map(|i| kitem(i, "approval", "a")));
        many.push(kitem(10, "approval", "b"));
        many.extend((11..=13).map(|i| kitem(i, "stuck", &format!("bot{i}"))));
        many.push(kitem(14, "stuck", "bot11"));
        many.push(kitem(15, "stuck", "bot99"));
        let order: Vec<String> = kinds_of(&many).into_iter().map(|k| k.0).collect();
        assert_eq!(order, ["stuck", "approval", "decision", "other"]);
        let r = render_retro(&many, 0, &HashMap::new(), &View::Kinds);
        assert!(r.text.contains("(15, 15 unrated)"), "{}", r.text);
        assert!(
            r.text
                .contains("\n1. stuck or failed runs: 5 (bot11 2, bot12 1, bot13 1, +1)"),
            "{}",
            r.text
        );
        assert!(
            r.text.contains("\n2. merge/deploy approvals: 4 (a 3, b 1)"),
            "{}",
            r.text
        );
        assert!(r.text.contains("\n4. other: 5 (z 5)"), "{}", r.text);
        assert_eq!(r.rows, [4, 4, 4, 4]);
        assert_eq!(r.items, many);
    }

    #[test]
    fn retro_all_kinds_fit_telegram() {
        // Every kind, many items each, long bot names: one message.
        let mut items = Vec::new();
        for (k, (kind, _)) in KINDS.iter().enumerate() {
            for i in 0..120 {
                items.push(kitem(
                    k * 1000 + i,
                    kind,
                    &format!("a-very-long-bot-name-{}", i % 7),
                ));
            }
        }
        let r = render_retro(&items, 40, &HashMap::new(), &View::Kinds);
        assert!(kinds_of(&items).len() <= KINDS.len());
        assert_eq!(r.rows.len(), KINDS.len());
        assert!(r.text.contains("+40 more"));
        assert!(crate::board::utf16(&r.text) < 4096, "{}", r.text.len());
        for (_, data) in &r.buttons {
            assert!(data.len() <= 64, "{data}");
        }
        // A stored card for RETRO_ITEMS asks stays a modest flags value.
        let full: Vec<_> = (0..RETRO_ITEMS)
            .map(|i| kitem(i, "decision", "bot"))
            .collect();
        assert!(serde_json::to_string(&full).unwrap().len() < 200_000);
    }

    #[test]
    fn kind_rating_applies_to_unrated_items_only() {
        let items = [
            kitem(1, "approval", "a"),
            kitem(2, "approval", "a"),
            kitem(3, "decision", "a"),
        ];
        let rated = HashMap::from([("t1.1".to_string(), "needed".to_string())]);
        assert_eq!(unrated_of(&items, &rated, "approval"), ["t2.1"]);
        assert_eq!(unrated_of(&items, &rated, "decision"), ["t3.1"]);
        assert!(unrated_of(&items, &rated, "stuck").is_empty());

        // Partly rated: count of unrated shown, buttons kept.
        let r = render_retro(&items, 0, &rated, &View::Kinds);
        assert!(
            r.text.contains("approvals: 2 (a 2), 1 unrated"),
            "{}",
            r.text
        );
        // All rated alike: the rating shown, only the open button left.
        let mut rated = rated;
        rated.insert("t2.1".into(), "needed".into());
        let r = render_retro(&items, 0, &rated, &View::Kinds);
        assert!(r.text.contains("approvals: 2 (a 2) = needed"), "{}", r.text);
        assert_eq!(r.rows, [1, 4]);
        // Mixed ratings read as rated.
        rated.insert("t2.1".into(), "should_not_exist".into());
        rated.insert("t3.1".into(), "needed".into());
        let r = render_retro(&items, 0, &rated, &View::Kinds);
        assert!(r.text.contains("approvals: 2 (a 2) = rated"), "{}", r.text);
        assert!(r.text.ends_with("All rated. Thanks."), "{}", r.text);
    }

    #[test]
    fn opening_a_kind_pages_its_items_for_re_rating() {
        let mut items: Vec<_> = (1..=RETRO_PAGE + 3)
            .map(|i| kitem(i, "decision", "a"))
            .collect();
        items.push(kitem(99, "approval", "b"));
        let rated = HashMap::from([("t2.1".to_string(), "could_decide".to_string())]);
        let view = View::Kind("decision".into(), 0);
        let r = render_retro(&items, 0, &rated, &view);
        assert!(
            r.text.starts_with(&format!(
                "Weekly retro > decisions: {}, {} unrated (page 1/2)",
                RETRO_PAGE + 3,
                RETRO_PAGE + 2
            )),
            "{}",
            r.text
        );
        assert!(
            r.text.contains("\n2. a task #2: thing 2 = could_decide"),
            "{}",
            r.text
        );
        assert!(!r.text.contains("task #99"), "{}", r.text);
        // Each item has three buttons, a rated one too (its rating ticked).
        assert_eq!(r.rows, [vec![3; RETRO_PAGE], vec![2]].concat());
        assert!(r
            .buttons
            .contains(&("2 decide ✓".into(), "rate:t2.1:c".into())));
        assert!(r
            .buttons
            .contains(&("2 needed".into(), "rate:t2.1:n".into())));
        assert_eq!(
            &r.buttons[r.buttons.len() - 2..],
            [
                ("next ›".to_string(), "ro:decision:1".to_string()),
                ("‹ all kinds".into(), "rb".into())
            ]
        );
        let r = render_retro(&items, 0, &rated, &View::Kind("decision".into(), 1));
        assert!(
            r.text.contains("(page 2/2)") && r.text.contains(&format!("\n{}. ", RETRO_PAGE + 1))
        );
        assert_eq!(r.rows, [3, 3, 3, 2]);
        assert_eq!(r.buttons[r.buttons.len() - 2].1, "ro:decision:0");
        // A page past the end shows the last one.
        let past = render_retro(&items, 0, &rated, &View::Kind("decision".into(), 9));
        assert_eq!(past.text, r.text);
        assert_eq!(
            page_of(&items, "decision", &format!("t{}.1", RETRO_PAGE + 1)),
            1
        );
        assert_eq!(page_of(&items, "decision", "t1.1"), 0);
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
        assert_eq!(
            Press::parse("rk:approval:c"),
            Some(Press::Kind("approval".into(), "could_decide"))
        );
        assert_eq!(
            Press::parse("ro:decision:3"),
            Some(Press::Open("decision".into(), 3))
        );
        assert_eq!(Press::parse("rb"), Some(Press::Back));
        for bad in ["rate:t7.1:q", "rk:approval", "ro:x:y", "zz:a:b", ""] {
            assert_eq!(Press::parse(bad), None, "{bad}");
        }
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
