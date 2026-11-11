//! Delivery statistics computed from the task event log and runs. Read-only.
use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::db;

const WEEK: f64 = 604_800.0;
pub const UNSET: &str = "unset";

pub struct Query<'a> {
    pub project: &'a str,
    pub since: i64,
    pub until: i64,
    pub workload: Option<&'a str>,
    /// `task` or `routine`: only tasks that did (not) come from a routine
    /// occurrence. `None` keeps every task.
    pub origin: Option<&'a str>,
}

/// Values of [`Query::origin`].
pub const ORIGINS: [&str; 2] = ["task", "routine"];

/// A window bound: RFC3339, or an age such as `14d`, `12h`, `30m`, `90s`.
pub fn parse_bound(s: &str, now: i64) -> Result<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    if let (Ok(n), Some(secs)) = (
        n.parse::<i64>(),
        match unit {
            "s" => Some(1),
            "m" => Some(60),
            "h" => Some(3600),
            "d" => Some(86400),
            _ => None,
        },
    ) {
        if n < 0 {
            bail!("bad time bound {s:?}");
        }
        return Ok(now - n * secs);
    }
    epoch(&db::parse_wake(s).with_context(|| format!("bad time bound {s:?}"))?)
}

fn epoch(s: &str) -> Result<i64> {
    Ok(OffsetDateTime::parse(s, &Rfc3339)
        .with_context(|| format!("bad timestamp {s:?}"))?
        .unix_timestamp())
}

fn iso(t: i64) -> Result<String> {
    Ok(OffsetDateTime::from_unix_timestamp(t)?.format(&Rfc3339)?)
}

struct Ev {
    at: i64,
    actor: String,
    kind: String,
    from: Option<String>,
    to: Option<String>,
    body: Option<String>,
}

#[derive(Default)]
struct Episode {
    start: i64,
    end: Option<i64>,
    end_actor: String,
    class: Option<String>,
    resolver: Option<String>,
    approved: bool,
}

impl Episode {
    fn intervened(&self) -> bool {
        self.approved
            || self.end_actor == "human"
            || matches!(self.resolver.as_deref(), Some("owner" | "human"))
    }
}

#[derive(Default)]
struct Facts {
    workload: Option<String>,
    episodes: Vec<Episode>,
    other_interventions: u64,
    shipped: bool,
    done: u64,
    reopened: u64,
    runs: u64,
    run_seconds: u64,
    pending_since: i64,
}

fn resolver_of(body: &str) -> Option<String> {
    let line = body.lines().find_map(|l| l.strip_prefix("resolver:"))?;
    let who = line.trim().split(':').next()?.trim().to_lowercase();
    Some(who)
}

/// The 1-based episode an `escalation_class` event names (`episode=N`), if any.
fn episode_of(body: Option<&str>) -> Option<usize> {
    body?
        .strip_prefix("episode=")?
        .trim()
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

fn analyze(evs: &[Ev], since: i64, until: i64) -> Facts {
    let mut f = Facts::default();
    let in_window = |at: i64| at >= since && at < until;
    let mut open: Option<Episode> = None;
    let mut done_at = None;
    let mut artifact = false;
    let mut prev_approval = false;
    for e in evs {
        let after_approval = std::mem::replace(&mut prev_approval, false);
        // Backlog age is "now", so pending_since is tracked past the window.
        if e.kind == "created" || (e.kind == "state" && e.to.as_deref() == Some("pending")) {
            f.pending_since = e.at;
        }
        if e.at >= until {
            continue;
        }
        match e.kind.as_str() {
            "created"
                if e.body
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("state=escalated") =>
            {
                open = Some(Episode {
                    start: e.at,
                    ..Default::default()
                });
            }
            "state" if e.from != e.to => {
                let (from, to) = (e.from.as_deref(), e.to.as_deref());
                let closing = from == Some("escalated");
                if closing {
                    if let Some(mut ep) = open.take() {
                        ep.end = Some(e.at);
                        ep.end_actor = e.actor.clone();
                        f.episodes.push(ep);
                    }
                }
                if to == Some("escalated") {
                    open = Some(Episode {
                        start: e.at,
                        ..Default::default()
                    });
                }
                if e.actor == "human" && !closing && !after_approval && in_window(e.at) {
                    f.other_interventions += 1;
                }
                if to == Some("done") {
                    done_at = Some(e.at);
                    f.done += u64::from(in_window(e.at));
                } else if from == Some("done") {
                    done_at = None;
                    f.reopened += u64::from(in_window(e.at));
                }
            }
            "edit" if e.actor == "human" && in_window(e.at) => f.other_interventions += 1,
            "artifact" => artifact = true,
            "workload" => f.workload = e.to.clone(),
            "approval" => {
                prev_approval = true;
                if let Some(ep) = open.as_mut() {
                    ep.approved = true;
                } else if in_window(e.at) {
                    f.other_interventions += 1;
                }
            }
            "note" => {
                if let (Some(ep), Some(r)) =
                    (open.as_mut(), e.body.as_deref().and_then(resolver_of))
                {
                    ep.resolver = Some(r);
                }
            }
            "escalation_class" => {
                let target = match episode_of(e.body.as_deref()) {
                    Some(n) if n <= f.episodes.len() => f.episodes.get_mut(n - 1),
                    Some(n) if n == f.episodes.len() + 1 => open.as_mut(),
                    Some(_) => None,
                    None => open.as_mut().or(f.episodes.last_mut()),
                };
                if let Some(ep) = target {
                    ep.class = e.to.clone();
                }
            }
            _ => {}
        }
    }
    if let Some(ep) = open {
        f.episodes.push(ep);
    }
    f.shipped = artifact && done_at.is_some_and(in_window);
    f
}

fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.max(1) - 1])
}

fn ratio(a: u64, b: u64) -> Option<f64> {
    (b > 0).then(|| a as f64 / b as f64)
}

#[derive(Default)]
struct ClassStats {
    opened: u64,
    resolved: u64,
    autonomous: u64,
    latencies: Vec<i64>,
}

impl ClassStats {
    fn json(mut self) -> Value {
        self.latencies.sort_unstable();
        json!({
            "opened": self.opened,
            "resolved": self.resolved,
            "autonomous": self.autonomous,
            "autonomous_rate": ratio(self.autonomous, self.resolved),
            "latency_p50_seconds": percentile(&self.latencies, 0.5),
            "latency_p90_seconds": percentile(&self.latencies, 0.9),
        })
    }
}

fn metrics(tasks: &[&Facts], ready: &[(i64, i64)], q: &Query, now: i64) -> Value {
    let (since, until) = (q.since, q.until);
    let in_window = |at: i64| at >= since && at < until;
    let mut all = ClassStats::default();
    let mut classes: BTreeMap<String, ClassStats> = BTreeMap::new();
    let mut interventions = 0;
    for f in tasks {
        interventions += f.other_interventions;
        for ep in &f.episodes {
            let class = ep.class.clone().unwrap_or_else(|| "unclassified".into());
            let c = classes.entry(class).or_default();
            if in_window(ep.start) {
                all.opened += 1;
                c.opened += 1;
            }
            let Some(end) = ep.end.filter(|e| in_window(*e)) else {
                continue;
            };
            all.resolved += 1;
            c.resolved += 1;
            all.latencies.push(end - ep.start);
            c.latencies.push(end - ep.start);
            if ep.intervened() {
                interventions += 1;
            } else {
                all.autonomous += 1;
                c.autonomous += 1;
            }
        }
    }
    let shipped: Vec<_> = tasks.iter().filter(|f| f.shipped).collect();
    let n = shipped.len() as u64;
    let done: u64 = tasks.iter().map(|f| f.done).sum();
    let reopened: u64 = tasks.iter().map(|f| f.reopened).sum();
    let oldest = ready.iter().map(|(_, since)| now - since).max();
    json!({
        "tasks": tasks.len(),
        "verified_shipped": n,
        "interventions": interventions,
        "interventions_per_verified_shipped": ratio(interventions, n),
        "throughput_per_week": n as f64 / ((until - since).max(1) as f64 / WEEK),
        "done_transitions": done,
        "reopened": reopened,
        "reopen_rate": ratio(reopened, done),
        "runs_per_shipped": ratio(shipped.iter().map(|f| f.runs).sum(), n),
        "run_seconds_per_shipped": ratio(shipped.iter().map(|f| f.run_seconds).sum(), n),
        "escalations": all.json(),
        "escalations_by_class": classes.into_iter().map(|(k, v)| (k, v.json())).collect::<BTreeMap<_, _>>(),
        "backlog": { "runnable": ready.len(), "oldest_wait_seconds": oldest },
    })
}

fn load_events(conn: &Connection, project: &str) -> Result<BTreeMap<i64, Vec<Ev>>> {
    let mut stmt = conn.prepare(
        "SELECT e.task_id, e.at, e.actor, e.kind, e.from_value, e.to_value, e.body
         FROM events e JOIN tasks t ON t.id = e.task_id
         WHERE t.project = ?1 AND e.kind IN
           ('created','state','edit','artifact','workload','approval','note','escalation_class')
         ORDER BY e.task_id, e.id",
    )?;
    let mut rows = stmt.query([project])?;
    let mut out: BTreeMap<i64, Vec<Ev>> = BTreeMap::new();
    while let Some(r) = rows.next()? {
        out.entry(r.get(0)?).or_default().push(Ev {
            at: epoch(&r.get::<_, String>(1)?)?,
            actor: r.get(2)?,
            kind: r.get(3)?,
            from: r.get(4)?,
            to: r.get(5)?,
            body: r.get(6)?,
        });
    }
    Ok(out)
}

/// Ready tasks and when each became runnable: the latest of entering pending, its
/// wake time and its last prerequisite finishing, so holds are not counted.
fn ready(conn: &Connection, project: &str, facts: &HashMap<i64, Facts>) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.not_before,
           (SELECT MAX(strftime('%s', e.at)) FROM deps d JOIN events e ON e.task_id = d.prereq_id
            WHERE d.task_id = t.id AND e.kind = 'state' AND e.to_value = 'done')
         FROM tasks t
         WHERE t.project = ?1 AND t.state = 'pending'
           AND (t.not_before IS NULL OR t.not_before <= strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
           AND NOT EXISTS (SELECT 1 FROM deps d JOIN tasks p ON p.id = d.prereq_id
                           WHERE d.task_id = t.id AND p.state != 'done')",
    )?;
    let mut rows = stmt.query([project])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let id: i64 = r.get(0)?;
        let mut at = facts.get(&id).map_or(0, |f| f.pending_since);
        if let Some(t) = r.get::<_, Option<String>>(1)? {
            at = at.max(epoch(&t)?);
        }
        if let Some(t) = r.get::<_, Option<String>>(2)? {
            at = at.max(t.parse().unwrap_or(0));
        }
        out.push((id, at));
    }
    Ok(out)
}

/// Delivery statistics for one project over `[since, until)`, overall and by workload.
pub fn stats(conn: &Connection, q: &Query) -> Result<Value> {
    if q.since >= q.until {
        bail!("since must be before until");
    }
    if let Some(w) = q.workload.filter(|w| *w != UNSET) {
        db::validate_workload(w)?;
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut facts: HashMap<i64, Facts> = load_events(conn, q.project)?
        .into_iter()
        .map(|(id, evs)| (id, analyze(&evs, q.since, q.until)))
        .collect();
    if let Some(origin) = q.origin {
        if !ORIGINS.contains(&origin) {
            bail!("invalid origin {origin:?} (expected task or routine)");
        }
        let routine: HashSet<i64> = conn
            .prepare("SELECT task_id FROM routine_occurrences")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        facts.retain(|id, _| routine.contains(id) == (origin == "routine"));
    }
    let mut stmt = conn.prepare(
        "SELECT task_id, COUNT(*),
           COALESCE(SUM(MAX(0, strftime('%s', ended_at) - strftime('%s', started_at))), 0)
         FROM runs WHERE project = ?1 GROUP BY task_id",
    )?;
    let mut rows = stmt.query([q.project])?;
    while let Some(r) = rows.next()? {
        if let Some(f) = facts.get_mut(&r.get::<_, i64>(0)?) {
            f.runs = r.get(1)?;
            f.run_seconds = r.get(2)?;
        }
    }
    let ready = ready(conn, q.project, &facts)?;
    let workload_of = |f: &Facts| f.workload.clone().unwrap_or_else(|| UNSET.into());
    let mut groups: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (id, f) in &facts {
        let w = workload_of(f);
        if q.workload.is_none_or(|want| want == w) {
            groups.entry(w).or_default().push(*id);
        }
    }
    let pick = |ids: &[i64]| -> (Vec<&Facts>, Vec<(i64, i64)>) {
        (
            ids.iter().map(|i| &facts[i]).collect(),
            ready
                .iter()
                .filter(|(id, _)| ids.contains(id))
                .copied()
                .collect(),
        )
    };
    let all: Vec<i64> = groups.values().flatten().copied().collect();
    let (tasks, r) = pick(&all);
    let overall = metrics(&tasks, &r, q, now);
    let workloads: BTreeMap<&String, Value> = groups
        .iter()
        .map(|(w, ids)| {
            let (tasks, r) = pick(ids);
            (w, metrics(&tasks, &r, q, now))
        })
        .collect();
    let owner_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM bot_messages m JOIN bots b ON b.id = m.bot_id
         WHERE b.project = ?1 AND m.direction = 'in'
           AND CAST(strftime('%s', m.at) AS INTEGER) >= ?2
           AND CAST(strftime('%s', m.at) AS INTEGER) < ?3",
        rusqlite::params![q.project, q.since, q.until],
        |r| r.get(0),
    )?;
    let mut out = json!({
        "project": q.project,
        "since": iso(q.since)?,
        "until": iso(q.until)?,
        "overall": overall,
        "workloads": workloads,
        "owner_messages": owner_messages,
    });
    if let Some(o) = q.origin {
        out["origin"] = json!(o);
    }
    Ok(out)
}
