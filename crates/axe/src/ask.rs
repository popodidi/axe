//! Owner asks and the owner's ratings of them. An ask is something a bot or a
//! task session made the owner look at, read from the logs axe already keeps:
//!
//! - an **escalation**: one escalation episode of a task (each entry into
//!   `escalated`, numbered like `axe task classify --episode`), id `t<task>.<n>`.
//!   Its bot is the task's `bot:` owner, else the bot that posted its card;
//! - a **card**: a bot's own message sent with buttons (`flags.ask`, kept when
//!   the card closes), id `m<message>`. A card for an escalated task
//!   (`flags.task`) is that task's escalation, not a second ask, and the
//!   retro card (`flags.retro`) is not an ask. Neither is a card its bot
//!   settled without an owner press ([`settled`]: closed with a text edit
//!   that starts `Decided:` or `Closed:`).
//!
//! A rating is `needed`, `could_decide` (the bot could have decided itself) or
//! `should_not_exist`, with who rated and when. It is an event in the ask's
//! own log (`ask_rating`: a task event with `episode=N`, or a bot event); the
//! latest one wins. Nothing new is stored, so every count is computed on read.
//!
//! An ask may also carry a **recommendation**: the rating the bot that raised
//! it suggests, with a few words why (`ask_rec`, logged the same way). It is
//! never a rating: the owner confirms it, one ask at a time or every ask on a
//! retro page at once ([`accept`], which logs ratings marked `via=rec`), or
//! overrides it. Stats count only the owner's ratings and report how often a
//! recommendation was accepted, agreed with or overridden.
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
/// The event kind of a recommendation, in `events` and `bot_events`.
pub const REC_EVENT: &str = "ask_rec";
/// The longest recommendation reason, in characters.
pub const REC_WHY: usize = 120;
const WEEK: i64 = 7 * 86_400;
const DAY: i64 = 86_400;

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
    /// Whether the rating is an accepted recommendation (the retro card's
    /// accept button) rather than one the owner picked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub accepted: bool,
    /// The rating the asking bot recommends, if any; never a rating itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rec: Option<String>,
    /// Why, in a few words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rec_why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rec_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rec_at: Option<String>,
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

/// A rating or a recommendation as logged: `via_rec` marks an accepted
/// recommendation, `why` is a recommendation's reason.
#[derive(Default)]
struct Rating {
    rating: String,
    by: String,
    at: String,
    via_rec: bool,
    why: Option<String>,
}

/// The episode a task event body names (`episode=N`, more after a space or
/// a line), and the rest of its first line and later lines.
fn episode_body(body: &str) -> Option<(i64, &str, &str)> {
    let rest = body.strip_prefix("episode=")?;
    let (first, more) = rest.split_once('\n').unwrap_or((rest, ""));
    let (n, tail) = first.trim().split_once(' ').unwrap_or((first.trim(), ""));
    Some((n.parse().ok()?, tail.trim(), more.trim()))
}

/// Episode ratings (`kind` [`EVENT`]) or recommendations ([`REC_EVENT`]) per
/// `(task, episode)`, the latest winning.
fn task_ratings_of(conn: &Connection, kind: &str) -> Result<HashMap<(i64, i64), Rating>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, body, to_value, actor, at FROM events WHERE kind=?1 ORDER BY id",
    )?;
    let mut rows = stmt.query([kind])?;
    let mut out = HashMap::new();
    while let Some(r) = rows.next()? {
        let body: Option<String> = r.get(1)?;
        let Some((n, tail, more)) = body.as_deref().and_then(episode_body) else {
            continue;
        };
        out.insert(
            (r.get(0)?, n),
            Rating {
                rating: r.get(2)?,
                by: r.get(3)?,
                at: r.get(4)?,
                via_rec: tail.split_whitespace().any(|w| w == "via=rec"),
                why: (!more.is_empty()).then(|| more.to_string()),
            },
        );
    }
    Ok(out)
}

fn task_ratings(conn: &Connection) -> Result<HashMap<(i64, i64), Rating>> {
    task_ratings_of(conn, EVENT)
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
    let recs = task_ratings_of(conn, REC_EVENT)?;
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
        let rec = recs.get(&(id, n));
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
            rating: None,
            rated_by: None,
            rated_at: None,
            accepted: false,
            rec: None,
            rec_why: None,
            rec_by: None,
            rec_at: None,
        });
        out.last_mut().unwrap().apply(rating, rec);
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

/// Card ratings ([`EVENT`]) or recommendations ([`REC_EVENT`]) per message,
/// the latest winning.
fn card_ratings_of(conn: &Connection, kind: &str) -> Result<HashMap<i64, Rating>> {
    let mut stmt =
        conn.prepare("SELECT body, actor, at FROM bot_events WHERE kind=?1 ORDER BY id")?;
    let mut rows = stmt.query([kind])?;
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
                via_rec: v["via"] == "rec",
                why: v["why"].as_str().map(String::from),
            },
        );
    }
    Ok(out)
}

impl Ask {
    /// Sets the ask's latest rating and recommendation.
    fn apply(&mut self, rating: Option<&Rating>, rec: Option<&Rating>) {
        self.rating = rating.map(|r| r.rating.clone());
        self.rated_by = rating.map(|r| r.by.clone());
        self.rated_at = rating.map(|r| r.at.clone());
        self.accepted = rating.is_some_and(|r| r.via_rec);
        self.rec = rec.map(|r| r.rating.clone());
        self.rec_why = rec.and_then(|r| r.why.clone());
        self.rec_by = rec.map(|r| r.by.clone());
        self.rec_at = rec.map(|r| r.at.clone());
    }
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

/// Whether a card was settled by its bot, not the owner: its buttons are
/// gone, the owner never pressed one, and its text now starts `Decided:` or
/// `Closed:` (`axe bot tg edit <id> --clear-buttons "Decided: X"`). Such a
/// card is not an ask. A card the owner pressed stays one, and so does a
/// card with live buttons whatever its text.
fn settled(text: &str, flags: &Value) -> bool {
    let live = flags["buttons"].as_array().is_some_and(|b| !b.is_empty());
    if live || !flags["answered"].is_null() {
        return false;
    }
    let first = first_line(text, flags["html"] == true);
    ["decided:", "closed:"].iter().any(|p| {
        first
            .get(..p.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(p))
    })
}

fn cards(conn: &Connection, project: Option<&str>, only: Option<i64>) -> Result<Vec<Ask>> {
    let ratings = card_ratings_of(conn, EVENT)?;
    let recs = card_ratings_of(conn, REC_EVENT)?;
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
        if settled(&text, &flags) {
            continue;
        }
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
            rating: None,
            rated_by: None,
            rated_at: None,
            accepted: false,
            rec: None,
            rec_why: None,
            rec_by: None,
            rec_at: None,
        });
        out.last_mut()
            .unwrap()
            .apply(ratings.get(&id), recs.get(&id));
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
    let target = resolve(&tx, id)?;
    write(&tx, actor, &target, EVENT, rating, Extra::None)?;
    tx.commit()?;
    find(conn, &target.id())
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

/// A rating as given: a rating id or its button label (`decide`, `drop`).
pub fn parse_rating(s: &str) -> Result<&'static str> {
    let s = s.trim();
    RATINGS
        .iter()
        .copied()
        .find(|r| *r == s)
        .or_else(|| CODES.iter().find(|c| c.2 == s).map(|c| c.1))
        .with_context(|| {
            format!(
                "invalid rating {s:?} (expected {} or needed, decide, drop)",
                RATINGS.join(", ")
            )
        })
}

/// One recommendation: the rating the asking bot suggests for `ask`, and
/// why in a few words.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rec {
    pub ask: String,
    pub rating: String,
    #[serde(default)]
    pub why: Option<String>,
}

/// Records recommendations, all or none, and returns the asks. Only the bot
/// that raised an ask (actor `bot:<name>`) or a person may recommend for
/// it; task sessions may not. A later recommendation replaces the earlier
/// one. A recommendation is never a rating.
pub fn recommend(conn: &mut Connection, actor: &Actor, recs: &[Rec]) -> Result<Vec<Ask>> {
    if actor.name == "agent" {
        bail!("task sessions cannot recommend ratings; the bot that raised the ask does");
    }
    if recs.is_empty() {
        bail!("no recommendations given");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let bots: HashMap<String, Option<String>> = {
        let mut all = escalations(&tx, None)?;
        all.extend(cards(&tx, None, None)?);
        all.into_iter().map(|a| (a.id, a.bot)).collect()
    };
    let mut ids = Vec::new();
    for r in recs {
        let rating = parse_rating(&r.rating).with_context(|| format!("ask {}", r.ask))?;
        let why = r.why.as_deref().map(flatten).filter(|w| !w.is_empty());
        if why.as_ref().is_some_and(|w| w.chars().count() > REC_WHY) {
            bail!(
                "ask {}: --why is over {REC_WHY} characters; a few words",
                r.ask
            );
        }
        let target = resolve(&tx, &r.ask)?;
        let id = target.id();
        if let Some(me) = actor.name.strip_prefix("bot:") {
            let asker = bots.get(&id).cloned().flatten();
            if asker.as_deref() != Some(me) {
                bail!(
                    "ask {id} was raised by {}, not bot {me}: only the bot that raised an ask recommends its rating",
                    asker.as_deref().unwrap_or("no bot")
                );
            }
        }
        write(
            &tx,
            actor,
            &target,
            REC_EVENT,
            rating,
            Extra::Why(why.as_deref()),
        )?;
        ids.push(id);
    }
    tx.commit()?;
    let mut all = escalations(conn, None)?;
    all.extend(cards(conn, None, None)?);
    let mut by: HashMap<String, Ask> = all.into_iter().map(|a| (a.id.clone(), a)).collect();
    Ok(ids.iter().filter_map(|i| by.remove(i)).collect())
}

/// Accepts the recommendation of each of `items` nobody rated yet, in one
/// transaction, as the owner's ratings marked `via=rec`. Items without a
/// recommendation, or rated meanwhile, are left alone. Returns how many
/// were rated.
pub fn accept(conn: &mut Connection, actor: &Actor, items: &[RetroItem]) -> Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids: Vec<String> = items.iter().map(|i| i.ask.clone()).collect();
    let rated = ratings(&tx, &ids)?;
    let mut n = 0;
    for it in items.iter().filter(|i| !i.rec.is_empty()) {
        if rated.contains_key(&it.ask) {
            continue;
        }
        check_rater(actor, &it.rec)?;
        let target = resolve(&tx, &it.ask)?;
        write(&tx, actor, &target, EVENT, &it.rec, Extra::ViaRec)?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// The ask an id names, checked to exist.
enum Target {
    Task(i64, i64),
    Message(i64, i64),
}

impl Target {
    fn id(&self) -> String {
        match self {
            Target::Task(t, n) => format!("t{t}.{n}"),
            Target::Message(m, _) => format!("m{m}"),
        }
    }
}

fn resolve(conn: &Connection, id: &str) -> Result<Target> {
    Ok(match AskId::parse(id)? {
        AskId::Task(task, n) => {
            let episodes: i64 = conn
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
            Target::Task(task, n)
        }
        AskId::Message(m) => {
            let (bot, text, flags): (i64, String, String) = conn
                .query_row(
                    &format!(
                        "SELECT m.bot_id, m.text, m.flags FROM bot_messages m WHERE m.id=?1 AND {CARD_ASK}"
                    ),
                    [m],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|_| anyhow::anyhow!("message {m} is not a card ask"))?;
            if settled(&text, &serde_json::from_str(&flags).unwrap_or_default()) {
                bail!("message {m} is not a card ask: its bot settled it without an owner press");
            }
            Target::Message(m, bot)
        }
    })
}

/// What a logged rating carries besides its value.
#[derive(Clone, Copy)]
enum Extra<'a> {
    None,
    /// An accepted recommendation.
    ViaRec,
    /// A recommendation's reason.
    Why(Option<&'a str>),
}

/// Logs one rating or recommendation (`kind`) of `target`.
fn write(
    tx: &rusqlite::Transaction,
    actor: &Actor,
    target: &Target,
    kind: &str,
    rating: &str,
    extra: Extra,
) -> Result<()> {
    match *target {
        Target::Task(task, n) => {
            let body = match extra {
                Extra::None | Extra::Why(None) => format!("episode={n}"),
                Extra::ViaRec => format!("episode={n} via=rec"),
                Extra::Why(Some(w)) => format!("episode={n}\n{w}"),
            };
            crate::db::write_event(tx, actor, task, kind, None, Some(rating), Some(&body))
        }
        Target::Message(m, bot) => {
            let mut body = json!({"message": m, "rating": rating});
            match extra {
                Extra::ViaRec => body["via"] = json!("rec"),
                Extra::Why(Some(w)) => body["why"] = json!(w),
                _ => {}
            }
            crate::bot::event(tx, bot, actor, kind, &body.to_string())
        }
    }
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
    /// Asks with a recommendation, rated or not. A recommendation alone is
    /// never counted as a rating: `rated` and the counts above are the
    /// owner's ratings only.
    pub recommended: u64,
    /// Rated asks whose rating is the recommendation, accepted in one tap
    /// (the retro card's accept button). The owner picked the other
    /// `rated - accepted` themselves.
    pub accepted: u64,
    /// Asks the owner rated themselves the same as recommended.
    pub agreed: u64,
    /// Asks the owner rated otherwise than recommended.
    pub overridden: u64,
}

impl Tally {
    fn add(&mut self, a: &Ask) {
        self.asks += 1;
        if a.rec.is_some() {
            self.recommended += 1;
        }
        match a.rating.as_deref() {
            Some("needed") => self.needed += 1,
            Some("could_decide") => self.could_decide += 1,
            Some("should_not_exist") => self.should_not_exist += 1,
            _ => return,
        }
        self.rated += 1;
        self.wasted_share =
            Some((self.could_decide + self.should_not_exist) as f64 / self.rated as f64);
        match &a.rec {
            Some(_) if a.accepted => self.accepted += 1,
            Some(r) if Some(r) == a.rating.as_ref() => self.agreed += 1,
            Some(_) => self.overridden += 1,
            None => {}
        }
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

/// The most items one retro card may hold (`--limit` is capped to it).
pub const RETRO_ITEMS: usize = 1000;
/// Items a retro card holds by default: the most useful asks of the week
/// ([`select`]); the rest are counted in `more` and stay unrated.
pub const RETRO_LIMIT: usize = 10;
/// Asks per page of a retro card: each has its own row of rating buttons.
pub const RETRO_PAGE: usize = 10;
/// Length (UTF-16 units, as Telegram counts) of an ask's first retro line:
/// its id and what it is about.
const RETRO_TOPIC: usize = 100;
/// Length of an ask's second retro line: what the owner was asked.
const RETRO_QUESTION: usize = 160;
/// Length of a recommendation's reason on its retro line.
const RETRO_WHY: usize = 48;
/// The label of the button that accepts a page's recommendations.
pub const ACCEPT_LABEL: &str = "Accept recs on this page";
/// Button codes in callback data `rate:<ask>:<code>`.
pub const CODES: [(&str, &str, &str); 3] = [
    ("n", "needed", "needed"),
    ("c", "could_decide", "decide"),
    ("x", "should_not_exist", "drop"),
];
const NOTE: &str =
    "Rate each ask: needed = right to ask, decide = the bot could have decided, drop = should not exist. Skipped asks stay unrated.";
const REC_NOTE: &str = "(rec) marks the bot's recommendation; accept them or pick your own.";

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

/// The latest slot at or before `now`: slots are `at` (from
/// [`parse_weekly`]) after a Monday 00:00Z and repeat every `every` seconds,
/// so with 7 days they fall on that weekday and time each week.
pub fn last_slot(now: i64, at: i64, every: i64) -> i64 {
    // 1970-01-05 was a Monday.
    let anchor = 4 * DAY + at;
    anchor + (now - anchor).div_euclid(every.max(1)) * every.max(1)
}

/// The slot a retro is due for at `now`, if any: the latest slot
/// ([`last_slot`]), at most [`RETRO_CATCH_UP`] ago, with no retro card sent
/// since it (`last_sent`) and not already tried (`tried`).
pub fn retro_due(
    now: i64,
    at: i64,
    every: i64,
    last_sent: Option<i64>,
    tried: Option<i64>,
) -> Option<i64> {
    let slot = last_slot(now, at, every);
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

/// A retro card as stored: its message id, when it went out (epoch), its
/// items, and whether it was dismissed.
pub type SentRetro = (i64, i64, Vec<RetroItem>, bool);

/// The latest retro card bot `bot` sent before `before` (epoch).
pub fn previous_retro(conn: &Connection, bot: &str, before: i64) -> Result<Option<SentRetro>> {
    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT m.id, m.at, m.flags FROM bot_messages m JOIN bots b ON b.id=m.bot_id
             WHERE b.name=?1 AND m.direction='out' AND json_type(m.flags,'$.retro') IS NOT NULL
               AND m.at < ?2
             ORDER BY m.id DESC LIMIT 1",
            params![bot, ts(before)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, at, flags)) = row else {
        return Ok(None);
    };
    let flags: Value = serde_json::from_str(&flags).unwrap_or_default();
    let items: Vec<RetroItem> =
        serde_json::from_value(flags["retro"]["items"].clone()).unwrap_or_default();
    Ok(Some((
        id,
        epoch_of(&at).unwrap_or(before),
        items,
        flags["dismissed"] == true,
    )))
}

/// A bot's retro card settings (`bots.retro`; NULL is [`Default`]): whether
/// it goes out, how often and from when, and how many asks it holds. It is
/// the owner's attention budget for that bot, set with `axe bot edit
/// --retro*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct RetroConfig {
    /// Whether the card goes out.
    pub on: bool,
    /// The interval, in whole days (`7d`, 1d-28d). The card holds the asks
    /// of the last interval.
    pub every: String,
    /// The first slot: a UTC weekday and time (`mon 01:00`); slots repeat
    /// every `every` from it.
    pub at: String,
    /// The most asks on the card (1-1000).
    pub size: u32,
}

impl Default for RetroConfig {
    fn default() -> Self {
        RetroConfig {
            on: true,
            every: "7d".into(),
            at: "mon 01:00".into(),
            size: RETRO_LIMIT as u32,
        }
    }
}

/// The shortest and longest retro interval, in days.
pub const RETRO_DAYS: (i64, i64) = (1, 28);

/// A retro interval (`7d`, or `2w`) as seconds: whole days, 1d-28d.
pub fn parse_every(s: &str) -> Result<i64> {
    let t = s.trim().to_lowercase();
    let bad = || {
        anyhow::anyhow!(
            "retro interval {s:?}: expected whole days from {}d to {}d (e.g. 7d)",
            RETRO_DAYS.0,
            RETRO_DAYS.1
        )
    };
    let (n, mult) = match t.char_indices().last() {
        Some((i, 'd')) => (&t[..i], 1),
        Some((i, 'w')) => (&t[..i], 7),
        _ => return Err(bad()),
    };
    let days = n.parse::<i64>().map_err(|_| bad())? * mult;
    if !(RETRO_DAYS.0..=RETRO_DAYS.1).contains(&days) {
        return Err(bad());
    }
    Ok(days * DAY)
}

impl RetroConfig {
    /// Checks every field and returns the canonical form (`every` in days,
    /// `at` as `mon 01:00`).
    pub fn normalized(&self) -> Result<RetroConfig> {
        let every = parse_every(&self.every)?;
        let at = parse_weekly(&self.at)?;
        if !(1..=RETRO_ITEMS as u32).contains(&self.size) {
            bail!("retro size {}: expected 1 to {RETRO_ITEMS} asks", self.size);
        }
        Ok(RetroConfig {
            on: self.on,
            every: format!("{}d", every / DAY),
            at: format!(
                "{} {:02}:{:02}",
                DAYS[(at / DAY) as usize],
                at % DAY / 3600,
                at % 3600 / 60
            ),
            size: self.size,
        })
    }

    /// The interval in seconds; a bad stored value is the default 7 days.
    pub fn period(&self) -> i64 {
        parse_every(&self.every).unwrap_or(WEEK)
    }

    /// The start time ([`parse_weekly`]); a bad stored value is `mon 01:00`.
    pub fn offset(&self) -> i64 {
        parse_weekly(&self.at).unwrap_or(3600)
    }

    /// The next slot after `now`.
    pub fn next_slot(&self, now: i64) -> i64 {
        last_slot(now, self.offset(), self.period()) + self.period()
    }

    /// One line for `axe bot show`: `on, every 7d from mon 01:00Z, 10 asks,
    /// next 2026-11-30T01:00Z`.
    pub fn line(&self, now: i64) -> String {
        if !self.on {
            return format!(
                "off (every {} from {}Z, {} asks)",
                self.every, self.at, self.size
            );
        }
        let next = ts(self.next_slot(now));
        format!(
            "on, every {} from {}Z, {} asks, next {}Z",
            self.every,
            self.at,
            self.size,
            next.get(..16).unwrap_or(&next)
        )
    }
}

/// The wake a bot gets once per retro card with new ratings: when the card
/// closes (Dismiss, or its last item rated), or at the bot's next retro slot
/// if the card was left open. Its key, so one card never wakes twice.
pub fn retro_wake_key(msg: i64) -> String {
    format!("retro-closed-{msg}")
}

/// The text of a retro wake ([`retro_wake_key`]) for card `msg` of bot
/// `bot`, sent at `sent` (epoch) with interval `every`: `rated` of its
/// items have a rating; `open` when the card was never closed.
pub fn retro_wake(msg: i64, bot: &str, sent: i64, every: i64, rated: usize, open: bool) -> String {
    let what = if open {
        "left open at your next retro slot"
    } else {
        "closed"
    };
    let n = if rated == 1 { "rating" } else { "ratings" };
    format!(
        "Retro #{msg} {what}: {rated} new {n}. Read them: axe ask list --bot {bot} --since {} --json (final ratings). Propose rule changes to the owner as a card.",
        ts(sent - every)
    )
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
    /// The rating its bot recommends, as when the card was sent; empty for
    /// none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rec: String,
    /// Why, summarized.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
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
    /// `ra:<page>`: accept the recommendations of the unrated asks on a page.
    Accept(usize),
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
            "ra" => rest.parse().ok().map(Press::Accept),
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
    /// The bot whose retro it is: the card holds only the asks it raised.
    /// `None` holds every ask.
    pub bot: Option<&'a str>,
    /// How many asks the card holds; `None` is [`RETRO_LIMIT`]. Capped to
    /// [`RETRO_ITEMS`].
    pub limit: Option<usize>,
    /// The interval in seconds: the card holds the asks of the last one.
    /// `None` is 7 days.
    pub every: Option<i64>,
}

impl RetroScope<'_> {
    fn holds(&self, ask: Option<&str>) -> bool {
        match self.bot {
            None => true,
            Some(bot) => ask == Some(bot),
        }
    }
}

/// The retro card of bot `bot` now, with its own settings (`bots.retro`):
/// its interval and, unless `limit` is given, its size. A bot not in the
/// store gets the defaults.
pub fn bot_retro(conn: &Connection, now: i64, bot: &str, limit: Option<usize>) -> Result<Retro> {
    let cfg = crate::bot::get(conn, bot)
        .ok()
        .and_then(|b| b.retro)
        .unwrap_or_default();
    retro(
        conn,
        now,
        &RetroScope {
            bot: Some(bot),
            limit: Some(limit.unwrap_or(cfg.size as usize)),
            every: Some(cfg.period()),
        },
    )
}

/// The retro card of one bot: of the asks of the interval before `now`
/// (7 days by default) it raised that nobody rated yet, the most useful few
/// ([`select`]), two lines each. An ask not shown, or shown and left alone,
/// stays unrated; it is never taken as `needed`.
pub fn retro(conn: &Connection, now: i64, scope: &RetroScope) -> Result<Retro> {
    let every = scope.every.unwrap_or(WEEK);
    let mut asks = list(
        conn,
        &Filter {
            since: now - every,
            // Through the current second.
            until: now + 1,
            unrated: true,
            ..Default::default()
        },
    )?;
    asks.retain(|a| scope.holds(a.bot.as_deref()));
    let mut cands = Vec::new();
    for a in asks {
        let card = match a.kind.as_str() {
            "card" => card_text(conn, &a)?,
            _ => CardText::default(),
        };
        cands.push((a, card));
    }
    let total = cands.len();
    let limit = scope.limit.unwrap_or(RETRO_LIMIT).clamp(1, RETRO_ITEMS);
    let mut items = Vec::new();
    for (a, card) in select(cands, limit) {
        let (line, detail) = retro_lines(&a, &card);
        items.push(RetroItem {
            ask: a.id.clone(),
            line,
            detail,
            kind: a.category.clone(),
            bot: a.bot.clone().unwrap_or_else(|| "-".into()),
            rec: a.rec.clone().unwrap_or_default(),
            why: a
                .rec_why
                .as_deref()
                .map(|w| clip(&shorten(&flatten(w)), RETRO_WHY))
                .unwrap_or_default(),
        });
    }
    let more = total - items.len();
    Ok(render_retro(
        &items,
        more,
        &HashMap::new(),
        0,
        scope.bot,
        every,
    ))
}

/// What one retro item stands for, so repeats show once: an escalation's
/// task, a card's task when it opens with one, else the card's bot and
/// first line.
fn retro_key(a: &Ask, card: &CardText) -> String {
    let task = a
        .task
        .or_else(|| card.task.as_ref().and_then(|_| leading_task(&card.body)));
    match task {
        Some(t) => format!("t{t}"),
        None => format!(
            "m{}\n{}",
            a.bot.as_deref().unwrap_or("-"),
            flatten(&a.question).to_lowercase()
        ),
    }
}

/// Whether the owner had to act on an ask: an escalation of class decision
/// (a decision or an approval), or a card the owner answered. Anything else
/// (runs that got stuck, access, cards left alone) may have cleared itself.
fn acted(a: &Ask, card: &CardText) -> bool {
    match a.kind.as_str() {
        "escalation" => matches!(a.category.as_str(), "decision" | "approval"),
        _ => card.answered,
    }
}

/// An ask's place in time: when it was made, then its numeric id.
fn recency(a: &Ask) -> (String, i64, i64) {
    let id = a.id.trim_start_matches(['t', 'm']);
    let (x, y) = id.split_once('.').unwrap_or((id, "0"));
    (a.at.clone(), x.parse().unwrap_or(0), y.parse().unwrap_or(0))
}

/// The retro's asks, most useful first, at most `limit`: one per task or
/// card ([`retro_key`]: the latest), then those the owner had to act on
/// ([`acted`]) before the rest, then the most recent. Only stored data
/// decides, so the same store gives the same card.
fn select(cands: Vec<(Ask, CardText)>, limit: usize) -> Vec<(Ask, CardText)> {
    let mut latest: HashMap<String, (Ask, CardText)> = HashMap::new();
    for (a, card) in cands {
        let key = retro_key(&a, &card);
        match latest.get(&key) {
            Some((b, _)) if recency(b) >= recency(&a) => {}
            _ => {
                latest.insert(key, (a, card));
            }
        }
    }
    let mut out: Vec<_> = latest.into_values().collect();
    out.sort_by(|(a, ac), (b, bc)| {
        acted(b, bc)
            .cmp(&acted(a, ac))
            .then_with(|| recency(b).cmp(&recency(a)))
            .then_with(|| a.id.cmp(&b.id))
    });
    out.truncate(limit);
    out
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
    /// Whether the owner answered the card (`flags.answered`).
    answered: bool,
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
    let answered = serde_json::from_str::<Value>(&flags)
        .is_ok_and(|f| f.get("answered").is_some_and(|v| !v.is_null()));
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
    Ok(CardText {
        body,
        task,
        reply,
        answered,
    })
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
        // A clause (`;`) ends the cut as a word cut does.
        return match text[..i].strip_suffix(';') {
            Some(clause) => format!("{clause}…"),
            None => text[..i].to_string(),
        };
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

/// The items on page `page` of a retro card (a page past the end is the
/// last one, as [`render_retro`] shows it).
pub fn page_items(items: &[RetroItem], page: usize) -> &[RetroItem] {
    let pages = items.len().div_ceil(RETRO_PAGE).max(1);
    let start = page.min(pages - 1) * RETRO_PAGE;
    &items[start.min(items.len())..(start + RETRO_PAGE).min(items.len())]
}

/// A retro card's title: whose retro it is, "Weekly" when its interval
/// (`every`, seconds) is 7 days.
fn retro_title(bot: Option<&str>, every: i64) -> String {
    let kind = if every == WEEK {
        "Weekly retro"
    } else {
        "Retro"
    };
    match bot {
        Some(b) => format!("{kind}, {}", crate::board::cap(b, 32)),
        None => kind.to_string(),
    }
}

/// The span a retro card covers, in words: `last week` or `the last 14 days`.
fn retro_span(every: i64) -> String {
    match every {
        WEEK => "last week".into(),
        DAY => "the last day".into(),
        e => format!("the last {} days", e / DAY),
    }
}

/// The interval stored on a retro card (`flags.retro.every`); cards sent
/// before it existed are weekly.
pub fn card_every(retro: &Value) -> i64 {
    retro["every"].as_i64().filter(|e| *e > 0).unwrap_or(WEEK)
}

/// The text a dismissed retro card is edited to: it has no buttons, and its
/// unrated asks stay unrated.
pub fn dismissed_retro(
    items: &[RetroItem],
    rated: &HashMap<String, String>,
    bot: Option<&str>,
    every: i64,
) -> String {
    let left = items.iter().filter(|i| !rated.contains_key(&i.ask)).count();
    format!(
        "{}: dismissed. {left} of {} asks left unrated.",
        retro_title(bot, every),
        items.len()
    )
}

/// Renders page `page` of a retro card from its items, with `rated` (ask id
/// to rating) marked: two lines and one row of rating buttons per ask (a
/// rated ask can be re-rated), a recommended rating marked `(rec)` on its
/// button and shown as a third line with its reason, an accept button while
/// an unrated ask on the page has a recommendation, prev / next, a Dismiss
/// row, and the kind totals as a footer. `bot` names whose retro it is and
/// `every` (seconds) the span it covers. Pure, so presses re-render the
/// stored items.
pub fn render_retro(
    items: &[RetroItem],
    more: usize,
    rated: &HashMap<String, String>,
    page: usize,
    bot: Option<&str>,
    every: i64,
) -> Retro {
    let title = retro_title(bot, every);
    let span = retro_span(every);
    if items.is_empty() {
        return Retro {
            text: format!("{title}: no unrated asks {span}."),
            ..Default::default()
        };
    }
    let asks = if every == WEEK {
        "last week's asks".to_string()
    } else {
        format!("asks of {span}")
    };
    let pages = items.len().div_ceil(RETRO_PAGE);
    let page = page.min(pages - 1);
    let left = items.iter().filter(|i| !rated.contains_key(&i.ask)).count();
    // Asks not on the card stay unrated.
    let mut text = format!(
        "{title}: {asks} ({}, {} unrated)",
        items.len() + more,
        left + more
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
        if !it.rec.is_empty() {
            text.push_str(&format!("\n   rec: {}", rating_label(&it.rec)));
            if !it.why.is_empty() {
                text.push_str(&format!(" ({})", scrub(&it.why)));
            }
        }
        for (code, rating, label) in CODES {
            let label = if mine == Some(rating) {
                format!("{n} {label} ✓")
            } else if it.rec == rating {
                format!("{n} {label} (rec)")
            } else {
                format!("{n} {label}")
            };
            buttons.push((label, format!("rate:{}:{code}", it.ask)));
        }
        rows.push(CODES.len());
    }
    let open_recs = page_items(items, page)
        .iter()
        .any(|i| !i.rec.is_empty() && !rated.contains_key(&i.ask));
    if open_recs {
        buttons.push((ACCEPT_LABEL.into(), format!("ra:{page}")));
        rows.push(1);
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
    text.push_str("\n\n");
    text.push_str(if left == 0 {
        "All rated. Thanks."
    } else {
        NOTE
    });
    if open_recs {
        text.push(' ');
        text.push_str(REC_NOTE);
    }
    text.push_str(&format!("\nBy kind: {}", kind_totals(items)));
    if more > 0 {
        text.push_str(&format!(
            "\n{} of {} asks shown, most useful first; the rest stay unrated (axe ask list --unrated).",
            items.len(),
            items.len() + more
        ));
    }
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
            accepted: false,
            rec: None,
            rec_why: None,
            rec_by: None,
            rec_at: None,
        }
    }

    #[test]
    fn recs_are_marked_on_their_button_with_an_accept_button() {
        let rec = |i: usize, rec: &str, why: &str| RetroItem {
            rec: rec.into(),
            why: why.into(),
            ..kitem(i, "decision", "a")
        };
        let items = [
            rec(1, "could_decide", "reversible, in scope"),
            kitem(2, "decision", "a"),
            rec(3, "needed", ""),
        ];
        let r = render_retro(&items, 0, &HashMap::new(), 0, Some("a"), WEEK);
        assert!(
            r.text.contains(
                "\n1. #1: thing 1\n   rec: decide (reversible, in scope)\n2. #2: thing 2\n3. #3: thing 3\n   rec: needed\n"
            ),
            "{}",
            r.text
        );
        assert!(r.text.contains(REC_NOTE), "{}", r.text);
        let labels: Vec<&str> = r.buttons.iter().map(|b| b.0.as_str()).collect();
        assert_eq!(
            labels[..9],
            [
                "1 needed",
                "1 decide (rec)",
                "1 drop",
                "2 needed",
                "2 decide",
                "2 drop",
                "3 needed (rec)",
                "3 decide",
                "3 drop"
            ]
        );
        assert_eq!(r.buttons[9], (ACCEPT_LABEL.into(), "ra:0".into()));
        assert_eq!(r.rows, [3, 3, 3, 1, 1], "accept, then Dismiss");

        // A rated ask shows its own rating; once every rec on the page is
        // rated, the accept button goes.
        let rated = HashMap::from([
            ("t1.1".to_string(), "needed".to_string()),
            ("t3.1".to_string(), "needed".to_string()),
        ]);
        let r = render_retro(&items, 0, &rated, 0, Some("a"), WEEK);
        assert!(r.buttons.iter().any(|b| b.0 == "1 needed ✓"));
        assert!(r.buttons.iter().any(|b| b.0 == "1 decide (rec)"));
        assert!(r.buttons.iter().any(|b| b.0 == "3 needed ✓"));
        assert!(r.buttons.iter().all(|b| b.0 != ACCEPT_LABEL));
        assert!(!r.text.contains(REC_NOTE));
        assert_eq!(r.rows, [3, 3, 3, 1]);

        // No recs: as before, no accept button.
        let plain = render_retro(
            &[kitem(2, "decision", "a")],
            0,
            &HashMap::new(),
            0,
            None,
            WEEK,
        );
        assert!(plain.buttons.iter().all(|b| !b.1.starts_with("ra:")));
        assert!(!plain.text.contains("rec"), "{}", plain.text);

        // The accept button covers only its page.
        let many: Vec<_> = (0..15).map(|i| rec(i, "needed", "")).collect();
        let p2 = render_retro(&many, 0, &HashMap::new(), 1, None, WEEK);
        assert!(p2.buttons.contains(&(ACCEPT_LABEL.into(), "ra:1".into())));
        assert_eq!(page_items(&many, 1).len(), 5);
        assert_eq!(page_items(&many, 1)[0].ask, "t10.1");
        assert_eq!(
            page_items(&many, 9)[0].ask,
            "t10.1",
            "past the end: the last"
        );
        assert!(page_items(&[], 0).is_empty());
    }

    #[test]
    fn rec_bodies_parse() {
        assert_eq!(episode_body("episode=2"), Some((2, "", "")));
        assert_eq!(episode_body("episode=2 via=rec"), Some((2, "via=rec", "")));
        assert_eq!(
            episode_body("episode=3\nreversible, in scope"),
            Some((3, "", "reversible, in scope"))
        );
        assert_eq!(episode_body("episode=x"), None);
        assert_eq!(episode_body("other"), None);
        assert_eq!(parse_rating("decide").unwrap(), "could_decide");
        assert_eq!(parse_rating("drop").unwrap(), "should_not_exist");
        assert_eq!(parse_rating("needed").unwrap(), "needed");
        assert!(parse_rating("maybe").is_err());
    }

    #[test]
    fn retro_renders_zero_one_and_marks_ratings() {
        let r = render_retro(&[], 0, &HashMap::new(), 0, Some("a"), WEEK);
        assert!(r.buttons.is_empty() && r.items.is_empty());
        assert_eq!(r.text, "Weekly retro, a: no unrated asks last week.");

        let r = render_retro(
            &[kitem(7, "decision", "a")],
            0,
            &HashMap::new(),
            0,
            Some("a"),
            WEEK,
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
        let r = render_retro(&[kitem(7, "decision", "a")], 0, &rated, 0, Some("a"), WEEK);
        assert!(r.text.contains("1. #7: thing 7 = decide"), "{}", r.text);
        assert!(r
            .buttons
            .contains(&("1 decide ✓".into(), "rate:t7.1:c".into())));
        assert_eq!(r.rows, [3, 1]);
        assert!(r.text.contains("All rated. Thanks."), "{}", r.text);

        // Dismissed: no buttons, the unrated asks counted as left unrated.
        let two = [kitem(7, "decision", "a"), kitem(8, "confirm", "a")];
        assert_eq!(
            dismissed_retro(&two, &rated, Some("a"), WEEK),
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
                    ..Default::default()
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
        // A cut at a clause never leaves a dangling `;`.
        let t = "Merged with owner approval; then ".to_string() + &"more words ".repeat(10);
        assert_eq!(clip(&t, 50), "Merged with owner approval…");
        assert_eq!(clip("One. Two three four five six", 12), "One. Two…");
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
                    rec: "should_not_exist".into(),
                    why: "w".repeat(RETRO_WHY),
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
            let r = render_retro(&items, 40, &rated, page, Some(&bot), WEEK);
            assert!(
                crate::board::utf16(&r.text) < 4096,
                "page {page}: {}",
                r.text.len()
            );
            for (label, data) in &r.buttons {
                assert!(
                    data.len() <= 64 && (label.len() <= 20 || label == ACCEPT_LABEL),
                    "{label} {data}"
                );
            }
            assert!(r.text.contains("\n   rec: drop (www"), "{}", r.text);
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
            // The kind totals, then how many asks are shown, are the footer.
            let mut tail = r.text.lines().rev();
            assert_eq!(
                tail.next(),
                Some(
                    format!(
                        "{total} of {} asks shown, most useful first; the rest stay unrated (axe ask list --unrated).",
                        total + 40
                    )
                    .as_str()
                )
            );
            let footer = tail.next().unwrap();
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
            render_retro(&items, 0, &rated, 99, None, WEEK).text,
            render_retro(&items, 0, &rated, pages - 1, None, WEEK).text
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
    fn retro_scope_holds_only_its_bots_asks() {
        let a = RetroScope {
            bot: Some("a"),
            ..Default::default()
        };
        assert!(a.holds(Some("a")));
        // Asks no bot raised are on no bot's card.
        assert!(!a.holds(Some("b")) && !a.holds(None) && !a.holds(Some("gone")));
        let all = RetroScope::default();
        assert!(all.holds(Some("a")) && all.holds(None));
    }

    #[test]
    fn settled_cards_are_the_ones_a_bot_closed_without_a_press() {
        let closed = json!({"ask": true});
        assert!(settled("Decided: ship it", &closed));
        assert!(settled("  closed: superseded by #12", &closed));
        assert!(settled(
            "<b>Decided:</b> later",
            &json!({"ask": true, "html": true})
        ));
        // Live buttons: still open, whatever the text says.
        assert!(!settled(
            "Decided: X",
            &json!({"ask": true, "buttons": [["Ok", "ok"]]})
        ));
        // The owner pressed it: an ask.
        assert!(!settled(
            "Decided: X",
            &json!({"ask": true, "answered": {"label": "Ok"}})
        ));
        assert!(!settled("Merge #12?", &closed));
        assert!(!settled("Undecided: X", &closed));
    }

    #[test]
    fn retro_config_validates_and_normalizes() {
        let d = RetroConfig::default();
        assert_eq!(d.normalized().unwrap(), d);
        assert_eq!(d.period(), WEEK);
        let c = RetroConfig {
            on: false,
            every: "2w".into(),
            at: "Wednesday 9:05".into(),
            size: 25,
        };
        let n = c.normalized().unwrap();
        assert_eq!((n.every.as_str(), n.at.as_str()), ("14d", "wed 09:05"));
        for every in ["0d", "29d", "5w", "7", "7h", "xd", ""] {
            let c = RetroConfig {
                every: every.into(),
                ..d.clone()
            };
            assert!(c.normalized().is_err(), "{every}");
        }
        for size in [0, RETRO_ITEMS as u32 + 1] {
            let c = RetroConfig { size, ..d.clone() };
            assert!(c.normalized().is_err(), "{size}");
        }
        let c = RetroConfig {
            at: "mon 25:00".into(),
            ..d.clone()
        };
        assert!(c.normalized().is_err());
        // 2026-11-23 is a Monday.
        let now = epoch_of("2026-11-23T12:00:00Z").unwrap();
        assert_eq!(
            d.line(now),
            "on, every 7d from mon 01:00Z, 10 asks, next 2026-11-30T01:00Z"
        );
        assert!(RetroConfig { on: false, ..d }.line(now).starts_with("off"));
    }

    #[test]
    fn slots_follow_the_interval() {
        let mon_1am = parse_weekly("mon 01:00").unwrap();
        // 2026-11-16 is a Monday.
        let slot = epoch_of("2026-11-16T01:00:00Z").unwrap();
        // 7 days: the old weekly slot, for every start time and moment.
        for at in [0, mon_1am, parse_weekly("sun 23:59").unwrap()] {
            for now in (slot - 2 * WEEK..slot + 2 * WEEK).step_by(3 * 3600 + 7) {
                let days = now.div_euclid(DAY);
                let monday = (days - (days + 3).rem_euclid(7)) * DAY;
                let old = if monday + at > now {
                    monday + at - WEEK
                } else {
                    monday + at
                };
                assert_eq!(last_slot(now, at, WEEK), old, "{now} {at}");
            }
        }
        // 1 day: every day at the start time.
        assert_eq!(
            ts(last_slot(slot + 3 * DAY + 3600, mon_1am, DAY)),
            "2026-11-19T01:00:00Z"
        );
        assert_eq!(
            ts(last_slot(slot + 3 * DAY - 1, mon_1am, DAY)),
            "2026-11-18T01:00:00Z"
        );
        // 14 days: every other Monday, so a week after a slot is not one.
        let two = 2 * WEEK;
        let s = last_slot(slot, mon_1am, two);
        assert!(s == slot || s == slot - WEEK, "{}", ts(s));
        assert_eq!(last_slot(s + WEEK, mon_1am, two), s);
        assert_eq!(last_slot(s + two, mon_1am, two), s + two);
        assert_eq!(retro_due(s + WEEK, mon_1am, two, None, None), None);
        assert_eq!(retro_due(s + two, mon_1am, two, None, None), Some(s + two));
        // A 14-day card holds the asks of 14 days.
        assert_eq!(retro_span(two), "the last 14 days");
        assert_eq!(retro_title(Some("a"), two), "Retro, a");
        assert_eq!(retro_title(Some("a"), WEEK), "Weekly retro, a");
        let r = render_retro(
            &[kitem(1, "decision", "a")],
            0,
            &HashMap::new(),
            0,
            Some("a"),
            two,
        );
        assert!(
            r.text
                .starts_with("Retro, a: asks of the last 14 days (1, 1 unrated)"),
            "{}",
            r.text
        );
    }

    #[test]
    fn retro_wake_text_names_the_card_and_its_window() {
        let sent = epoch_of("2026-11-23T01:00:00Z").unwrap();
        assert_eq!(
            retro_wake(42, "a", sent, WEEK, 3, false),
            "Retro #42 closed: 3 new ratings. Read them: axe ask list --bot a --since 2026-11-16T01:00:00Z --json (final ratings). Propose rule changes to the owner as a card."
        );
        assert!(retro_wake(42, "a", sent, WEEK, 1, true)
            .starts_with("Retro #42 left open at your next retro slot: 1 new rating."));
        assert_eq!(retro_wake_key(42), "retro-closed-42");
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
        assert_eq!(Press::parse("ra:2"), Some(Press::Accept(2)));
        // Buttons of a card that rated by kind.
        assert_eq!(Press::parse("rk:approval:c"), Some(Press::Retired));
        assert_eq!(Press::parse("ro:decision:3"), Some(Press::Retired));
        assert_eq!(Press::parse("rb"), Some(Press::Retired));
        for bad in ["rate:t7.1:q", "rk:approval", "rp:x", "ra:", "zz:a:b", ""] {
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
        assert_eq!(
            (t.recommended, t.accepted, t.agreed, t.overridden),
            (0, 0, 0, 0)
        );

        // Recommendations: unrated, accepted, agreed by hand, overridden.
        let mut t = Tally::default();
        let rec = |rating: Option<&str>, accepted: bool| Ask {
            rating: rating.map(String::from),
            accepted,
            rec: Some("needed".into()),
            ..ask("m2")
        };
        for a in [
            rec(None, false),
            rec(Some("needed"), true),
            rec(Some("needed"), false),
            rec(Some("could_decide"), false),
        ] {
            t.add(&a);
        }
        assert_eq!((t.asks, t.rated, t.recommended), (4, 3, 4));
        assert_eq!((t.accepted, t.agreed, t.overridden), (1, 1, 1));
        assert_eq!(
            (t.needed, t.could_decide),
            (2, 1),
            "only owner ratings count"
        );
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
        assert_eq!(last_slot(slot, mon_1am, WEEK), slot);
        assert_eq!(last_slot(slot - 1, mon_1am, WEEK), slot - WEEK);
        assert_eq!(last_slot(slot + 3 * 86_400, mon_1am, WEEK), slot);
        let wed = parse_weekly("wed 09:30").unwrap();
        assert_eq!(ts(last_slot(slot, wed, WEEK)), "2026-11-11T09:30:00Z");

        // Due at the slot and up to a day late, once.
        assert_eq!(retro_due(slot, mon_1am, WEEK, None, None), Some(slot));
        assert_eq!(
            retro_due(slot + RETRO_CATCH_UP, mon_1am, WEEK, None, None),
            Some(slot)
        );
        assert_eq!(
            retro_due(slot + RETRO_CATCH_UP + 1, mon_1am, WEEK, None, None),
            None
        );
        assert_eq!(retro_due(slot - 60, mon_1am, WEEK, None, None), None);
        assert_eq!(retro_due(slot + 60, mon_1am, WEEK, None, Some(slot)), None);
        // A card sent since the slot (by hand or before a restart) counts;
        // last week's does not.
        assert_eq!(
            retro_due(slot + 60, mon_1am, WEEK, Some(slot + 10), None),
            None
        );
        assert_eq!(
            retro_due(slot + 60, mon_1am, WEEK, Some(slot - WEEK), None),
            Some(slot)
        );
    }
}
