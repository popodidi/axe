//! Read-only owner commands of a bot's Telegram chat, `/tasks`, `/task N`
//! and `/cost`: a small slice of the web dashboard for when the owner has no
//! web access. Each reply is one Telegram HTML message (links to the web
//! pages when the server has a public URL), rendered from the same store
//! queries as the dashboard: the task list, a task and its events, and the
//! Cost view's aggregate (`usage::cost_stats`). Nothing here writes.
use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use time::OffsetDateTime;

use crate::board::{cap, escape, short, utf16, FOOTER_ROOM, TEXT_LIMIT};
use crate::bot;
use crate::db::{self, Filter, Task};
use crate::goal_board::{cells, day};
use crate::usage::{cost_stats, CostQuery};

/// Notes `/task N` shows, newest last.
pub const NOTES: usize = 3;
/// Characters of the Outcome `/task N` shows.
const OUTCOME_CAP: usize = 600;
/// Characters of each note `/task N` shows.
const NOTE_CAP: usize = 300;
/// Days `/cost` covers: the web Cost view's default "Last 7 days".
pub const COST_DAYS: i64 = 7;
const DAY: i64 = 86_400;

/// What a task's line calls its state, the dashboard's words: `running`
/// (in progress), `waiting` (a timed wait), `blocked` (a hold or
/// prerequisites), `escalated`, `queued` (ready to claim).
pub fn status_label(t: &Task) -> &'static str {
    match t.state.as_str() {
        "in_progress" => "running",
        "escalated" => "escalated",
        "done" => "done",
        "blocked" => "blocked",
        _ if !t.waiting_on.is_empty() => "blocked",
        _ if t.sleeping() => "waiting",
        _ => "queued",
    }
}

/// Order of `/tasks`: what needs the owner first.
fn rank(label: &str) -> u8 {
    match label {
        "escalated" => 0,
        "running" => 1,
        "waiting" => 2,
        "blocked" => 3,
        _ => 4,
    }
}

fn task_url(web: &str, id: i64) -> String {
    format!("{}/?task={id}", web.trim_end_matches('/'))
}

/// `#N`, a link to the task page when the server has a public URL.
fn task_ref(web: Option<&str>, id: i64) -> String {
    match web {
        Some(w) => format!("<a href=\"{}\">#{id}</a>", escape(&task_url(w, id))),
        None => format!("#{id}"),
    }
}

/// Adds `lines` under `head` while the message fits [`TEXT_LIMIT`], then a
/// `+N more` line for the rest.
fn fit(head: String, lines: Vec<String>, tail: Option<String>) -> String {
    let mut out = head;
    let room = TEXT_LIMIT - FOOTER_ROOM - tail.as_deref().map_or(0, |t| utf16(t) + 1);
    let total = lines.len();
    for (i, line) in lines.into_iter().enumerate() {
        if utf16(&out) + 1 + utf16(&line) > room {
            out.push_str(&format!("\n+{} more", total - i));
            break;
        }
        out.push('\n');
        out.push_str(&line);
    }
    if let Some(t) = tail {
        out.push('\n');
        out.push_str(&t);
    }
    out
}

/// `/tasks`: bot `name`'s open tasks, those it owns (`bot:<name>`) or in its
/// project or subscribed projects (`*` is every project), one line each.
pub fn tasks(conn: &Connection, name: &str, web: Option<&str>) -> Result<String> {
    let b = bot::get(conn, name)?;
    let (all, projects) = crate::board::projects(conn, b.id)?;
    let owner = format!("bot:{name}");
    let mut open: Vec<(&'static str, Task)> = db::list_tasks(conn, &Filter::default())?
        .into_iter()
        .filter(|t| {
            all || t.owner.as_deref() == Some(owner.as_str())
                || t.project == b.project
                || projects.contains(&t.project)
        })
        .map(|t| (status_label(&t), t))
        .collect();
    open.sort_by_key(|(l, t)| (rank(l), t.id));
    if open.is_empty() {
        return Ok(format!("bot {}: no open tasks", escape(name)));
    }
    let n = open.len();
    let head = format!(
        "bot {}: {n} open task{}",
        escape(name),
        if n == 1 { "" } else { "s" }
    );
    let lines = open
        .iter()
        .map(|(l, t)| {
            format!(
                "{} {l} · {} · {}",
                task_ref(web, t.id),
                escape(&t.project),
                escape(&short(&t.title))
            )
        })
        .collect();
    Ok(fit(head, lines, None))
}

/// The `## Outcome` section of a task spec, or the spec's start when it has
/// none.
pub fn outcome(detail: &str) -> String {
    let mut lines = detail.lines();
    let found = lines
        .by_ref()
        .any(|l| l.trim().eq_ignore_ascii_case("## outcome"));
    let body: Vec<&str> = if found {
        lines.take_while(|l| !l.starts_with("## ")).collect()
    } else {
        detail.lines().collect()
    };
    cap(body.join("\n").trim(), OUTCOME_CAP)
}

/// `/task N`: title, state, Outcome and the latest notes of task `id`.
pub fn task(conn: &Connection, arg: &str, web: Option<&str>) -> Result<String> {
    let Some(id) = arg
        .trim()
        .trim_start_matches('#')
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
    else {
        return Ok("usage: /task N (a task id)".into());
    };
    let t = db::get_task(conn, id)?;
    let mut s = format!(
        "{} <b>{}</b>\n{} · {}",
        task_ref(web, t.id),
        escape(&cap(&t.title, 200)),
        status_label(&t),
        escape(&t.project)
    );
    if let Some(o) = &t.owner {
        s.push_str(&format!(" · owner {}", escape(o)));
    }
    let out = outcome(&t.detail);
    if !out.is_empty() {
        s.push_str(&format!("\n\nOutcome:\n{}", escape(&out)));
    }
    let notes: Vec<db::Event> = db::events(conn, id)?
        .into_iter()
        .filter(|e| e.kind == "note")
        .collect();
    if notes.is_empty() {
        s.push_str("\n\nno notes");
    } else {
        let skip = notes.len().saturating_sub(NOTES);
        s.push_str(&format!(
            "\n\nlatest notes ({} of {}):",
            notes.len() - skip,
            notes.len()
        ));
        for e in &notes[skip..] {
            s.push_str(&format!(
                "\n{} {}: {}",
                escape(e.at.get(..16).unwrap_or(&e.at)),
                escape(&e.actor),
                escape(&cap(e.body.as_deref().unwrap_or("").trim(), NOTE_CAP))
            ));
        }
    }
    Ok(s)
}

fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

fn usd(n: f64) -> String {
    if n > 0.0 && n < 0.01 {
        "<$0.01".into()
    } else {
        format!("${n:.2}")
    }
}

/// `/cost`: bot `name`'s spend over the web Cost view's "Last 7 days" (UTC
/// days, today included) at `now`, from the same aggregate, laid out like the
/// goal pin: a bold headline (total and runs), then one monospace bar per
/// day, newest first, scaled to the costliest day (a day with no spend shows
/// an empty bar), with its amount and runs.
pub fn cost(conn: &Connection, name: &str, web: Option<&str>, now: i64) -> Result<String> {
    bot::get(conn, name)?;
    let until = now.div_euclid(DAY) * DAY + DAY;
    let since = until - COST_DAYS * DAY;
    let query = CostQuery {
        bot: Some(name),
        ..Default::default()
    };
    let stats = cost_stats(conn, since, until, None, &query)?;
    let total = &stats["total"];
    let runs = total["runs"].as_i64().unwrap_or(0);
    let mut head = format!(
        "bot {} cost, last {COST_DAYS} days: {}",
        escape(name),
        escape(&usd(total["cost"].as_f64().unwrap_or(0.0)))
    );
    if runs > 0 {
        head.push_str(&format!(" over {runs} {}", plural(runs, "run")));
    }
    let mut s = format!("<b>{head}</b>");
    if runs == 0 {
        s.push_str("\nno runs");
    } else {
        let unknown = total["unknown_runs"].as_i64().unwrap_or(0);
        if unknown > 0 {
            s.push_str(&format!(
                "\n{unknown} with unknown usage: the figure is a lower bound"
            ));
        }
        let by_day: HashMap<&str, &Value> = stats["breakdowns"]["day"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| Some((d["key"].as_str()?, d)))
                    .collect()
            })
            .unwrap_or_default();
        // Every day of the range, newest first, zero where nothing ran.
        let days: Vec<(String, f64, i64)> = (0..COST_DAYS)
            .rev()
            .map(|i| {
                let key = OffsetDateTime::from_unix_timestamp(since + i * DAY)?
                    .date()
                    .to_string();
                let d = by_day.get(key.as_str());
                let cost = d.and_then(|d| d["cost"].as_f64()).unwrap_or(0.0);
                let runs = d.and_then(|d| d["runs"].as_i64()).unwrap_or(0);
                Ok((key, cost, runs))
            })
            .collect::<Result<_>>()?;
        let max = days.iter().map(|d| d.1).fold(0.0, f64::max);
        let amounts: Vec<String> = days.iter().map(|d| usd(d.1)).collect();
        let width = amounts.iter().map(|a| a.chars().count()).max().unwrap_or(0);
        for ((key, cost, runs), amount) in days.iter().zip(&amounts) {
            let ratio = if max > 0.0 { cost / max } else { 0.0 };
            let line = format!(
                "{:<6} {}  {amount:>width$}  {runs} {}",
                day(key),
                cells(ratio),
                plural(*runs, "run")
            );
            s.push_str(&format!("\n<code>{}</code>", escape(&line)));
        }
    }
    if let Some(w) = web {
        let mut q = String::new();
        for b in name.bytes() {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                q.push(b as char);
            } else {
                q.push_str(&format!("%{b:02X}"));
            }
        }
        let url = format!(
            "{}/?view=cost&days={COST_DAYS}&cost_bot={q}",
            w.trim_end_matches('/')
        );
        s.push_str(&format!("\n<a href=\"{}\">Open cost</a>", escape(&url)));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_is_the_section_or_the_start() {
        let d = "## Outcome\nShip it.\nAll of it.\n\n## Verify\n- CI";
        assert_eq!(outcome(d), "Ship it.\nAll of it.");
        assert_eq!(outcome("Just text"), "Just text");
        assert_eq!(outcome(""), "");
        assert_eq!(outcome(&"x".repeat(2000)).chars().count(), OUTCOME_CAP);
    }

    #[test]
    fn fit_truncates_with_a_more_line() {
        let lines: Vec<String> = (0..500)
            .map(|i| format!("line {i} {}", "y".repeat(40)))
            .collect();
        let out = fit("head".into(), lines, Some("tail".into()));
        assert!(utf16(&out) <= TEXT_LIMIT, "{}", utf16(&out));
        let more = out.lines().rev().nth(1).unwrap();
        assert!(more.starts_with('+') && more.ends_with(" more"), "{more}");
        let shown = out.lines().filter(|l| l.starts_with("line ")).count();
        assert_eq!(more, format!("+{} more", 500 - shown));
        assert!(out.ends_with("\ntail"));
        assert_eq!(fit("h".into(), vec!["a".into()], None), "h\na");
    }
}
