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
use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bot::{epoch_of, ts};
use crate::db::Actor;

pub const RATINGS: [&str; 3] = ["needed", "could_decide", "should_not_exist"];
/// The event kind of a rating, in `events` and `bot_events`.
pub const EVENT: &str = "ask_rating";
const WEEK: i64 = 7 * 86_400;

/// Whether a rating says the owner should not have been asked.
pub fn wasted(rating: &str) -> bool {
    matches!(rating, "could_decide" | "should_not_exist")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ask {
    pub id: String,
    /// `escalation` or `card`.
    pub kind: String,
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
         WHERE e.kind IN ('created','state') AND (?1 IS NULL OR t.project=?1)
         ORDER BY e.task_id, e.id",
    )?;
    let mut rows = stmt.query([project])?;
    let mut out = Vec::new();
    let mut task = None;
    let mut n = 0i64;
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        if task != Some(id) {
            task = Some(id);
            n = 0;
        }
        let kind: String = r.get(2)?;
        let from: Option<String> = r.get(3)?;
        let to: Option<String> = r.get(4)?;
        let body: Option<String> = r.get(5)?;
        if !opens_episode(&kind, from.as_deref(), to.as_deref(), body.as_deref()) {
            continue;
        }
        n += 1;
        let owner: Option<String> = r.get(8)?;
        let bot = owner
            .as_deref()
            .and_then(|o| o.strip_prefix("bot:"))
            .map(str::to_string)
            .or_else(|| card_bot.get(&id).cloned());
        let rating = ratings.get(&(id, n));
        out.push(Ask {
            id: format!("t{id}.{n}"),
            kind: "escalation".into(),
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
        out.push(Ask {
            id: format!("m{id}"),
            kind: "card".into(),
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
            && !(f.unrated && a.rating.is_some())
    });
    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Records the owner's rating of one ask and returns it rated. Task sessions
/// (`agent`) may not rate: the rating is the owner's judgment of the ask.
pub fn rate(conn: &mut Connection, actor: &Actor, id: &str, rating: &str) -> Result<Ask> {
    if !RATINGS.contains(&rating) {
        bail!(
            "invalid rating {rating:?} (expected {})",
            RATINGS.join(", ")
        );
    }
    if actor.name == "agent" {
        bail!("task sessions cannot rate asks; the owner rates them");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ask_id = match AskId::parse(id)? {
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
                &tx,
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
                &tx,
                bot,
                actor,
                EVENT,
                &json!({"message": m, "rating": rating}).to_string(),
            )?;
            format!("m{m}")
        }
    };
    tx.commit()?;
    find(conn, &ask_id)
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
    };
    for a in &asks {
        let w = ((epoch_of(&a.at).unwrap_or(since) - since) / WEEK) as usize;
        let bot = a.bot.clone().unwrap_or_else(|| "-".into());
        for s in [&mut out.overall, out.bots.entry(bot).or_insert_with(series)] {
            s.total.add(a);
            s.weeks[w].tally.add(a);
        }
    }
    Ok(out)
}

/// Items on one retro card; the rest are counted in `more`.
pub const RETRO_ITEMS: usize = 10;
const RETRO_SUMMARY: usize = 40;
const RETRO_BOT: usize = 16;
/// Button codes in callback data `rate:<ask>:<code>`.
pub const CODES: [(&str, &str, &str); 3] = [
    ("n", "needed", "needed"),
    ("c", "could_decide", "decide"),
    ("x", "should_not_exist", "drop"),
];

/// The rating a button code stands for.
pub fn rating_of(code: &str) -> Option<&'static str> {
    CODES.iter().find(|c| c.0 == code).map(|c| c.1)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetroItem {
    pub ask: String,
    pub line: String,
}

/// A rendered retro card: plain text and one row of three buttons per
/// unrated item. No items means nothing to ask.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Retro {
    pub text: String,
    pub buttons: Vec<(String, String)>,
    pub rows: Vec<usize>,
    pub items: Vec<RetroItem>,
    pub more: usize,
}

/// The weekly retro card: the owner's asks of the 7 days before `now` that
/// nobody rated yet, oldest first, at most [`RETRO_ITEMS`]. An item left
/// alone stays unrated; it is never taken as `needed`.
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
        })
        .collect::<Vec<_>>();
    Ok(render_retro(&items, more, &HashMap::new()))
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

/// Renders a retro card from its items, with `rated` (ask id to rating)
/// marked and their buttons gone. Pure, so presses re-render the stored items.
pub fn render_retro(items: &[RetroItem], more: usize, rated: &HashMap<String, String>) -> Retro {
    if items.is_empty() {
        return Retro {
            text: "Weekly retro: no unrated asks last week.".into(),
            ..Default::default()
        };
    }
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
        items: items.to_vec(),
        more,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(i: usize) -> RetroItem {
        RetroItem {
            ask: format!("t{i}.1"),
            line: format!("bot task #{i}: thing {i}"),
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
        let r = render_retro(&[], 0, &HashMap::new());
        assert!(r.buttons.is_empty() && r.items.is_empty());
        assert_eq!(r.text, "Weekly retro: no unrated asks last week.");

        let r = render_retro(&[item(7)], 0, &HashMap::new());
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
        let r = render_retro(&[item(7)], 0, &rated);
        assert!(r.buttons.is_empty());
        assert!(r.text.contains("1. bot task #7: thing 7 = needed"));
        assert!(r.text.ends_with("All rated. Thanks."));
    }

    #[test]
    fn retro_many_fits_telegram() {
        let items: Vec<_> = (1..=RETRO_ITEMS).map(item).collect();
        let r = render_retro(&items, 5, &HashMap::new());
        assert_eq!(r.buttons.len(), RETRO_ITEMS * 3);
        assert_eq!(r.rows, vec![3; RETRO_ITEMS]);
        assert!(r.text.contains("(15)") && r.text.contains("+5 more"));
        for (label, data) in &r.buttons {
            assert!(data.len() <= 64 && label.len() <= 16, "{label} {data}");
        }
        assert!(crate::board::utf16(&r.text) < 4096);
    }

    #[test]
    fn retro_lines_truncate() {
        let a = Ask {
            id: "t903.2".into(),
            kind: "escalation".into(),
            bot: Some("a-very-long-bot-name-indeed".into()),
            project: "p".into(),
            task: Some(903),
            at: ts(0),
            summary: "word ".repeat(30),
            rating: None,
            rated_by: None,
            rated_at: None,
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
        let mut a = Ask {
            id: "m1".into(),
            kind: "card".into(),
            bot: None,
            project: "p".into(),
            task: None,
            at: ts(0),
            summary: String::new(),
            rating: None,
            rated_by: None,
            rated_at: None,
        };
        t.add(&a);
        assert_eq!(t.wasted_share, None);
        for r in ["needed", "could_decide", "should_not_exist", "needed"] {
            a.rating = Some(r.into());
            t.add(&a);
        }
        assert_eq!((t.asks, t.rated, t.needed), (5, 4, 2));
        assert_eq!(t.wasted_share, Some(0.5));
    }
}
