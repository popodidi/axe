//! Read-only owner commands of a bot's Telegram chat, `/tasks`, `/task N`,
//! `/cost`, `/goals`, `/services` and `/routines`: a small slice of the web
//! dashboard for when the owner has no web access. Each reply is one
//! Telegram HTML message (links to the web pages when the server has a
//! public URL), rendered from the same store queries as the dashboard: the
//! task list, a task and its events, the Cost view's aggregate
//! (`usage::cost_stats`), the goal pins' lines (`goal_board`), the bot's
//! services and the routines it owns, and `/status` (state, wakes, services
//! and schedules, times as a short Taipei clock plus how far off). Nothing
//! here writes.
use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use time::OffsetDateTime;

use chrono::TimeZone;

use crate::board::{cap, escape, short, utf16, FOOTER_ROOM, TEXT_LIMIT};
use crate::bot;
use crate::db::{self, Filter, Task};
use crate::goal_board::{self, cells, day};
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
    s.push_str(&view_link(
        web,
        &format!("view=cost&days={COST_DAYS}&cost_bot={}", encode(name)),
        "Open cost",
    ));
    Ok(s)
}

/// `s` percent-encoded for a URL query value.
fn encode(s: &str) -> String {
    let mut q = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            q.push(b as char);
        } else {
            q.push_str(&format!("%{b:02X}"));
        }
    }
    q
}

/// `\n<a href="<web>/?<query>">label</a>`, or nothing without a public URL.
fn view_link(web: Option<&str>, query: &str, label: &str) -> String {
    match web {
        Some(w) => {
            let url = format!("{}/?{query}", w.trim_end_matches('/'));
            format!(
                "\n<a href=\"{}\">{label}</a>",
                escape(&url).replace('"', "&quot;")
            )
        }
        None => String::new(),
    }
}

/// The owner's time zone: `/status` shows the bot's own times (wakes, runs)
/// as wall time here.
pub const OWNER_TZ: chrono_tz::Tz = chrono_tz::Asia::Taipei;

/// A short wall clock for epoch `at` in `tz`: `16:50` on `now`'s day,
/// `Thu 16:50` within six days, else `Nov 26 16:50`.
pub fn clock(at: i64, tz: chrono_tz::Tz, now: i64) -> String {
    let (Some(t), Some(n)) = (
        tz.timestamp_opt(at, 0).single(),
        tz.timestamp_opt(now, 0).single(),
    ) else {
        return bot::ts(at);
    };
    let fmt = if t.date_naive() == n.date_naive() {
        "%H:%M"
    } else if (at - now).abs() < 6 * DAY {
        "%a %H:%M"
    } else {
        "%b %-d %H:%M"
    };
    t.format(fmt).to_string()
}

/// `in 35m` or `2h ago`: how far epoch `at` is from `now`.
pub fn relative(at: i64, now: i64) -> String {
    if at >= now {
        format!("in {}", crate::board::age(at - now))
    } else {
        format!("{} ago", crate::board::age(now - at))
    }
}

/// `16:50 (in 35m)`: [`clock`] in the owner's zone and [`relative`].
fn when(at: i64, now: i64) -> String {
    format!("{} ({})", clock(at, OWNER_TZ, now), relative(at, now))
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// A cron day-of-week field as day numbers (0 Sunday), or `None` for what
/// it does not read: numbers 0-7, `SUN`..`SAT`, ranges and lists.
fn weekdays(field: &str) -> Option<Vec<usize>> {
    let day = |s: &str| -> Option<usize> {
        let n = match s.parse::<usize>() {
            Ok(n) if n <= 7 => n % 7,
            Ok(_) => return None,
            Err(_) => DAYS.iter().position(|d| d.eq_ignore_ascii_case(s))?,
        };
        Some(n)
    };
    let mut out = Vec::new();
    for part in field.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (day(a)?, day(b)?);
                let b = if b == 0 && a > 0 { 7 } else { b };
                if a > b {
                    return None;
                }
                out.extend((a..=b).map(|d| d % 7));
            }
            None => out.push(day(part)?),
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// `cron` (five fields) in words for the common patterns: `daily 09:00`,
/// `weekdays 09:00`, `Mon, Thu 10:00`, `every 6h`, `hourly at :15`,
/// `every 15m`, `monthly on day 1 09:00`; `None` for anything else.
pub fn cron_words(cron: &str) -> Option<String> {
    let f: Vec<&str> = cron.split_whitespace().collect();
    let [min, hour, dom, month, dow] = f[..] else {
        return None;
    };
    let num = |s: &str, max: u32| s.parse::<u32>().ok().filter(|n| *n <= max);
    let step = |s: &str| {
        s.strip_prefix("*/")
            .or_else(|| s.strip_prefix("0/"))
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| *n > 0)
    };
    if month != "*" {
        return None;
    }
    let all_days = dom == "*" && dow == "*";
    // Sub-daily: every N minutes, hourly, every N hours.
    if all_days {
        if hour == "*" {
            if let Some(n) = step(min) {
                return Some(if n == 1 {
                    "every minute".into()
                } else {
                    format!("every {n}m")
                });
            }
            return match num(min, 59)? {
                0 => Some("hourly".into()),
                m => Some(format!("hourly at :{m:02}")),
            };
        }
        if let Some(n) = step(hour) {
            let m = num(min, 59)?;
            let every = if n == 1 {
                "hourly".into()
            } else {
                format!("every {n}h")
            };
            return Some(if m == 0 {
                every
            } else {
                format!("{every} at :{m:02}")
            });
        }
    }
    // At fixed times of day.
    let m = num(min, 59)?;
    let hours: Vec<u32> = hour.split(',').map(|h| num(h, 23)).collect::<Option<_>>()?;
    let times = hours
        .iter()
        .map(|h| format!("{h:02}:{m:02}"))
        .collect::<Vec<_>>()
        .join(", ");
    let days = match (dom, dow) {
        ("*", "*") => "daily".to_string(),
        ("*", d) => {
            let d = weekdays(d)?;
            match d[..] {
                [1, 2, 3, 4, 5] => "weekdays".into(),
                [0, 6] => "weekends".into(),
                [0, 1, 2, 3, 4, 5, 6] => "daily".into(),
                _ => d.iter().map(|&i| DAYS[i]).collect::<Vec<_>>().join(", "),
            }
        }
        (d, "*") => format!("monthly on day {}", num(d, 31).filter(|n| *n > 0)?),
        _ => return None,
    };
    Some(format!("{days} {times}"))
}

/// When a schedule fires, for an owner line: its cron in words (else the
/// raw cron in `<code>`), with the time zone unless it is the owner's.
pub fn schedule_words(cron: &str, timezone: &str) -> String {
    let mut s = match cron_words(cron) {
        Some(w) => w,
        None => format!("<code>{}</code>", escape(cron)),
    };
    let tz = timezone.parse::<chrono_tz::Tz>().ok();
    if tz != Some(OWNER_TZ) {
        s.push(' ');
        s.push_str(&escape(timezone));
    }
    s
}

/// `next in 9h`, or `due now` once `at` has passed.
fn next_in(at: i64, now: i64) -> String {
    if at > now {
        format!("next in {}", crate::board::age(at - now))
    } else {
        "due now".into()
    }
}

/// Adds each block of `blocks` under `head` while the message fits
/// [`TEXT_LIMIT`], then a `+N more` line, then `tail`.
fn fit_blocks(head: String, blocks: Vec<String>, tail: String) -> String {
    let mut out = head;
    let room = TEXT_LIMIT - FOOTER_ROOM - utf16(&tail);
    let total = blocks.len();
    for (i, b) in blocks.into_iter().enumerate() {
        if utf16(&out) + utf16(&b) > room {
            out.push_str(&format!("\n\n+{} more", total - i));
            break;
        }
        out.push_str(&b);
    }
    out.push_str(&tail);
    out
}

/// The empty `/goals` reply: what a goal is, in plain words, no commands.
pub const EMPTY_GOALS: &str = "<b>No open goals.</b> A goal is an outcome you want, with a weekly \
     budget I work toward and report on. Tell me what you want to achieve and I'll propose one.";
/// The empty `/services` reply.
pub const EMPTY_SERVICES: &str = "<b>No services.</b> A service keeps a program running for this \
     bot (a watcher, a trading loop, a local server) and restarts it if it stops. \
     Ask me to set one up.";
/// The empty `/routines` reply.
pub const EMPTY_ROUTINES: &str = "<b>No routines.</b> A routine creates a task on a schedule \
     (a weekly report, a daily check) for me to carry out. Ask me to set one up.";

/// `/goals`: the open goals (proposed, active, paused) bot `name` is
/// attached to, each laid out as its goal pin: name, status line, Next and
/// Needs you, and the cost and attention bars against the weekly budgets.
pub fn goals(conn: &Connection, name: &str, web: Option<&str>) -> Result<String> {
    bot::get(conn, name)?;
    let lines = goal_board::lines(conn, name, web)?;
    let link = view_link(web, "view=goals", "Open goals");
    if lines.is_empty() {
        return Ok(format!("{EMPTY_GOALS}{link}"));
    }
    let n = lines.len() as i64;
    let head = format!("<b>bot {}: {n} {}</b>", escape(name), plural(n, "goal"));
    let tail = if link.is_empty() {
        link
    } else {
        format!("\n{link}")
    };
    Ok(fit_blocks(
        head,
        lines.iter().map(goal_board::block).collect(),
        tail,
    ))
}

/// One `/services` (and `/status`) bullet: name (bold when `bold`), state,
/// the schedule or the restart policy, then the last run or exit.
fn service_line(svc: &crate::service::Service, now: i64, bold: bool) -> String {
    let st = &svc.status;
    let ago = |at: &Option<String>| {
        at.as_deref()
            .and_then(bot::epoch_of)
            .map(|t| crate::board::age(now - t))
    };
    let state = svc.display_state();
    let name = if bold {
        format!("<b>{}</b>", escape(&svc.name))
    } else {
        escape(&svc.name)
    };
    let mut parts = vec![format!("• {name}"), escape(state)];
    if !svc.enabled && state != "stopped" {
        parts.push("turned off".into());
    }
    match &svc.spec.schedule {
        Some(s) => {
            parts.push(schedule_words(&s.cron, &s.timezone));
            parts.push(match (ago(&st.started_at), state) {
                (Some(at), "running") => format!("running {at}"),
                (Some(at), _) => format!(
                    "last run {at} ago: {}",
                    escape(st.last_exit.as_deref().unwrap_or("no exit"))
                ),
                (None, _) => "no run yet".into(),
            });
            if let Some(at) = st.next_run_at.as_deref().and_then(bot::epoch_of) {
                if svc.enabled {
                    parts.push(format!("next in {}", crate::board::age(at - now)));
                }
            }
        }
        None => {
            parts.push(format!("restart {}", escape(&svc.spec.restart)));
            if state == "running" {
                if let Some(up) = ago(&st.started_at) {
                    parts.push(format!("up {up}"));
                }
            }
            parts.push(match &st.last_exit {
                Some(e) => format!("last {}", escape(e)),
                None => "no exit yet".into(),
            });
        }
    }
    if let Some(e) = &st.error {
        parts.push(format!("error: {}", escape(&crate::board::cap(e, 200))));
    }
    parts.join(" · ")
}

/// `/services`: bot `name`'s services, one line each: name, state, schedule
/// or restart policy, and the last run or exit.
pub fn services(conn: &Connection, name: &str, web: Option<&str>, now: i64) -> Result<String> {
    bot::get(conn, name)?;
    let list = crate::service::list(conn, Some(name))?;
    let link = view_link(
        web,
        &format!("view=services&q={}", encode(name)),
        "Open services",
    );
    if list.is_empty() {
        return Ok(format!("{EMPTY_SERVICES}{link}"));
    }
    let n = list.len() as i64;
    let head = format!("<b>bot {}: {n} {}</b>", escape(name), plural(n, "service"));
    let lines = list.iter().map(|s| service_line(s, now, true)).collect();
    Ok(fit(
        head,
        lines,
        (!link.is_empty()).then(|| link[1..].to_string()),
    ))
}

/// `/routines`: the routines whose tasks bot `name` owns (`owner`
/// `bot:<name>`), archived ones left out, one bullet each: name, when it
/// fires in words, and the next fire or `paused`.
pub fn routines(conn: &Connection, name: &str, web: Option<&str>, now: i64) -> Result<String> {
    bot::get(conn, name)?;
    let owner = format!("bot:{name}");
    let mut list: Vec<crate::routine::Routine> = crate::routine::list(conn)?
        .into_iter()
        .filter(|r| !r.archived && r.template.owner.as_deref() == Some(owner.as_str()))
        .collect();
    list.sort_by(|a, b| a.template.name.cmp(&b.template.name));
    let link = view_link(web, "view=routines", "Open routines");
    if list.is_empty() {
        return Ok(format!("{EMPTY_ROUTINES}{link}"));
    }
    let n = list.len() as i64;
    let head = format!("<b>bot {}: {n} {}</b>", escape(name), plural(n, "routine"));
    let lines = list
        .iter()
        .map(|r| {
            let t = &r.template;
            let next = if r.enabled {
                next_in(r.next_at, now)
            } else {
                "<i>paused</i>".into()
            };
            format!(
                "• <b>{}</b> · {} · {next}",
                escape(&t.name),
                schedule_words(&t.cron, &t.timezone)
            )
        })
        .collect();
    Ok(fit(
        head,
        lines,
        (!link.is_empty()).then(|| link[1..].to_string()),
    ))
}

/// Characters of the live run's activity `/status` shows.
const ACTIVITY_CAP: usize = 80;

/// `/status`, HTML for a phone: a bold headline with the bot's state and
/// what it is doing, one line of wakes and the last run (Taipei clock and
/// how far off), then its services and its schedules (when each fires in
/// words, the next fire; soonest first, paused last).
pub fn status(conn: &Connection, name: &str, now: i64) -> Result<String> {
    let b = bot::get(conn, name)?;
    let doing = match b.live_run {
        Some(r) => {
            let run = bot::get_run(conn, r)?;
            let what = run
                .current_activity
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .unwrap_or("starting");
            let mut d = format!("running: {}", escape(&cap(what, ACTIVITY_CAP)));
            if let Some(at) = bot::epoch_of(&run.started_at) {
                d.push_str(&format!(", {}", crate::board::age(now - at)));
            }
            d
        }
        None => "idle".into(),
    };
    let mut s = format!(
        "<b>{} · {}</b> · {doing}",
        escape(&b.name),
        escape(&b.state)
    );
    if b.attention {
        s.push_str(&format!(
            "\nNeeds attention: {} failed {} in a row",
            b.failures,
            plural(b.failures, "run")
        ));
    }
    let mut line = Vec::new();
    match b.next_wake.as_deref().and_then(bot::epoch_of) {
        Some(at) if at > now => line.push(format!("Next wake {}", when(at, now))),
        Some(_) => line.push("Wake due now".into()),
        None => line.push("No wakes queued".into()),
    }
    if b.queued > 1 {
        line.push(format!("{} queued", b.queued));
    }
    line.push(match b.last_run_end.as_deref().and_then(bot::epoch_of) {
        Some(at) => format!("last run {}", when(at, now)),
        None => "no runs yet".into(),
    });
    s.push('\n');
    s.push_str(&line.join(" · "));

    let services = crate::service::list(conn, Some(&b.name))?;
    if !services.is_empty() {
        s.push_str("\n\n<b>Services</b>");
        for svc in &services {
            s.push('\n');
            s.push_str(&service_line(svc, now, false));
        }
    }
    let mut schedules: Vec<(Option<i64>, crate::schedule::Schedule)> =
        crate::schedule::list(conn, &b.name)?
            .into_iter()
            .map(|sc| (sc.enabled.then(|| bot::epoch_of(&sc.next_at)).flatten(), sc))
            .collect();
    // Soonest first; paused (and unreadable) last, by name.
    schedules.sort_by(|(a, x), (b, y)| (a.is_none(), a, &x.name).cmp(&(b.is_none(), b, &y.name)));
    if !schedules.is_empty() {
        s.push_str("\n\n<b>Schedules</b>");
        for (at, sc) in &schedules {
            let next = match at {
                Some(at) => next_in(*at, now),
                None if sc.enabled => "next unknown".into(),
                None => "<i>paused</i>".into(),
            };
            s.push_str(&format!(
                "\n• {} · {} · {next}",
                escape(&sc.name),
                schedule_words(&sc.cron, &sc.timezone)
            ));
        }
    }
    Ok(s)
}

/// A sample older than this reads as stale in `/node`.
const NODE_STALE_SECONDS: i64 = 120;

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

/// One `/node` run line: the kind, its total, then each name (with its count
/// when above one), largest first.
fn node_runs_line(kind: &str, by: &std::collections::BTreeMap<String, u32>) -> String {
    let total: u32 = by.values().sum();
    let mut line = format!("{kind} <code>{total}</code>");
    let mut names: Vec<(&String, &u32)> = by.iter().collect();
    names.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let names: Vec<String> = names
        .into_iter()
        .map(|(n, c)| match c {
            1 => escape(n),
            c => format!("{} <code>{c}</code>", escape(n)),
        })
        .collect();
    if !names.is_empty() {
        line.push_str(": ");
        line.push_str(&names.join(", "));
    }
    line
}

/// `/node`, HTML for a phone: the node's newest sample (`node_metrics.rs`),
/// CPU, memory, disk and its running runs by kind.
pub fn node(conn: &Connection, web: Option<&str>, now: i64) -> Result<String> {
    let link = view_link(web, "view=node", "Open node");
    let Some(s) = crate::node_metrics::latest(conn)? else {
        return Ok(format!(
            "<b>Node · no samples yet</b>\nThe node reports every <code>[node_metrics] sample_seconds</code>.{link}"
        ));
    };
    let age = now - s.at;
    let [task, bots, services, scheduled] = s.runs.totals();
    let running = task + bots + services + scheduled;
    let mut out = if age > NODE_STALE_SECONDS {
        format!(
            "<b>Node · stale</b> · last sample {} ago",
            crate::board::age(age)
        )
    } else {
        format!("<b>Node · {running} running</b>")
    };
    let mut cpu = match s.cpu_percent {
        Some(c) => format!("\nCPU <code>{c:.0}%</code>"),
        None => "\nCPU <code>?</code>".into(),
    };
    if let (Some(a), Some(b), Some(c)) = (s.load1, s.load5, s.load15) {
        cpu.push_str(&format!(" · load <code>{a:.2} {b:.2} {c:.2}</code>"));
    }
    out.push_str(&cpu);
    if let (Some(used), Some(total)) = (s.mem_used_bytes, s.mem_total_bytes) {
        let pct = 100.0 * used as f64 / total.max(1) as f64;
        out.push_str(&format!(
            "\nMemory <code>{} / {} GiB</code> · <code>{pct:.0}%</code>",
            gib(used),
            gib(total)
        ));
    }
    if let (Some(free), Some(total)) = (s.disk_free_bytes, s.disk_total_bytes) {
        out.push_str(&format!(
            "\nDisk free <code>{} GiB</code> of <code>{} GiB</code>",
            gib(free),
            gib(total)
        ));
    }
    if age <= NODE_STALE_SECONDS {
        out.push_str(&format!("\nSampled {} ago", crate::board::age(age)));
    }
    out.push_str("\n\n<b>Running</b>");
    for (kind, by) in [
        ("Tasks", &s.runs.task),
        ("Bots", &s.runs.bot),
        ("Services", &s.runs.service),
        ("Scheduled", &s.runs.scheduled),
    ] {
        out.push('\n');
        out.push_str(&node_runs_line(kind, by));
    }
    out.push_str(&link);
    Ok(out)
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
