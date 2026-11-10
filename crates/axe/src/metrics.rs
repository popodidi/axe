//! Bounded operational telemetry. No user-provided strings enter metric labels.
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use rusqlite::Connection;

const BUCKETS: [f64; 8] = [0.005, 0.01, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0];
type Key = (&'static str, &'static str, u16);

#[derive(Default)]
struct Histogram {
    count: u64,
    sum: f64,
    buckets: [u64; 8],
}

/// Labels every project falls back to (`other`) or is summed into (`all`).
pub const RESERVED_PROJECTS: [&str; 2] = ["other", "all"];

#[derive(Default)]
pub struct Metrics {
    /// Projects with their own label (`[metrics] projects`).
    projects: Vec<String>,
    http: Mutex<BTreeMap<Key, Histogram>>,
    snapshot: Mutex<(String, f64)>,
}

pub fn route(path: &[String]) -> &'static str {
    let p: Vec<_> = path.iter().map(String::as_str).collect();
    match p.as_slice() {
        ["healthz"] => "/healthz",
        // The embedded dashboard (`embed-ui`).
        [] | ["_next", ..] => "/ui",
        [file] if file.ends_with(".html") || file.ends_with(".txt") => "/ui",
        ["metrics"] => "/metrics",
        ["api", "tasks"] => "/api/tasks",
        ["api", "tasks", _] => "/api/tasks/:id",
        ["api", "tasks", _, "events" | "notes" | "artifacts" | "deps" | "approve" | "abort" | "protect" | "routine"
        | "classify"] => "/api/tasks/:id/action",
        ["api", "routines"] => "/api/routines",
        ["api", "routines", _] => "/api/routines/:id",
        ["api", "routines", _, "history" | "events" | "pause" | "resume" | "archive" | "trigger"] => {
            "/api/routines/:id/action"
        }
        ["api", "bots"] => "/api/bots",
        ["api", "bots", _] => "/api/bots/:name",
        ["api", "bots", _, "wakes" | "events" | "activity" | "notes" | "inbox" | "pause" | "resume" | "stop"
        | "reset"] => "/api/bots/:name/action",
        ["api", "bot-runs"] => "/api/bot-runs",
        ["api", "bot-runs", _] => "/api/bot-runs/:id",
        ["api", "bot-runs", _, "log"] => "/api/bot-runs/:id/log",
        ["api", "stats", "history"] => "/api/stats/history",
        ["api", "stats", "flow"] => "/api/stats/flow",
        ["api", "runs"] => "/api/runs",
        ["api", "runs", _, "log"] => "/api/runs/:id/log",
        ["api", "sessions", _] => "/api/sessions/:id",
        ["api", "dispatcher"] => "/api/dispatcher",
        ["api", "dispatcher", "connect" | "recovery" | "log-catalog" | "admission" | "adopt-logs" | "reconcile"
        | "register" | "operation"] => "/api/dispatcher/action",
        ["api", "config"] => "/api/config",
        ["api", "overview"] => "/api/overview",
        ["api", "admission"] => "/api/admission",
        ["api", "claim"] => "/api/claim",
        ["api", "stream"] => "/api/stream",
        ["api", "backup"] => "/api/backup",
        _ => "unknown",
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    writeln!(out, "# HELP axe_{name} {help}\n# TYPE axe_{name} {kind}").unwrap();
}

impl Metrics {
    /// Metrics that label `projects` by name and every other project `other`.
    pub fn new(projects: &[String]) -> Metrics {
        Metrics {
            projects: projects.to_vec(),
            ..Default::default()
        }
    }

    fn project<'a>(&'a self, raw: &str) -> &'a str {
        self.projects
            .iter()
            .find(|p| *p == raw)
            .map_or("other", String::as_str)
    }

    pub fn observe(&self, method: &str, route: &'static str, status: u16, start: Instant) {
        let method = match method {
            "GET" => "GET",
            "POST" => "POST",
            "PATCH" => "PATCH",
            "PUT" => "PUT",
            "DELETE" => "DELETE",
            "HEAD" => "HEAD",
            "OPTIONS" => "OPTIONS",
            _ => "OTHER",
        };
        let seconds = start.elapsed().as_secs_f64();
        let mut http = self.http.lock().unwrap_or_else(|e| e.into_inner());
        let h = http.entry((method, route, status)).or_default();
        h.count += 1;
        h.sum += seconds;
        for (i, bound) in BUCKETS.iter().enumerate() {
            h.buckets[i] += u64::from(seconds <= *bound);
        }
    }

    /// Called by the scheduler at most once per 15 seconds, never by a scrape.
    /// Aggregate only narrow columns; never read task content or event bodies.
    pub fn refresh(&self, conn: &Connection) -> Result<()> {
        let mut counts = BTreeMap::new();
        let mut pending = BTreeMap::new();
        let mut active = BTreeMap::new();
        let mut completed = BTreeMap::new();
        let labels = self
            .projects
            .iter()
            .map(String::as_str)
            .chain(RESERVED_PROJECTS);
        for p in labels {
            for s in crate::db::STATES {
                counts.insert((p.to_string(), s.to_string()), 0i64);
            }
            pending.insert(p.to_string(), 0f64);
            active.insert(p.to_string(), 0i64);
            completed.insert(p.to_string(), (0i64, 0f64));
        }
        let mut tasks = conn.prepare("SELECT project, state, count(*), COALESCE(MAX(MAX(0, (julianday('now') - julianday(updated_at))*86400)),0) FROM tasks GROUP BY project,state")?;
        for row in tasks.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })? {
            let (p, s, n, age) = row?;
            for label in [self.project(&p), "all"] {
                *counts.get_mut(&(label.to_string(), s.clone())).unwrap() += n;
                if s == "pending" {
                    let v = pending.get_mut(label).unwrap();
                    *v = v.max(age);
                }
            }
        }
        let mut runs = conn.prepare("SELECT project, ended_at IS NULL, count(*), COALESCE(SUM(MAX(0, (julianday(ended_at)-julianday(started_at))*86400)),0) FROM runs GROUP BY project, ended_at IS NULL")?;
        for row in runs.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })? {
            let (p, running, n, seconds) = row?;
            for label in [self.project(&p), "all"] {
                if running {
                    *active.get_mut(label).unwrap() += n;
                } else {
                    let v = completed.get_mut(label).unwrap();
                    v.0 += n;
                    v.1 += seconds;
                }
            }
        }
        let mut out = String::new();
        header(
            &mut out,
            "tasks",
            "gauge",
            "Stored tasks by state; waiting tasks retain their database state.",
        );
        for ((p, s), n) in counts {
            writeln!(out, "axe_tasks{{project=\"{p}\",state=\"{s}\"}} {n}").unwrap();
        }
        header(
            &mut out,
            "pending_oldest_age_seconds",
            "gauge",
            "Oldest pending task time since last update; includes held tasks.",
        );
        for (p, n) in pending {
            writeln!(out, "axe_pending_oldest_age_seconds{{project=\"{p}\"}} {n}").unwrap();
        }
        header(
            &mut out,
            "active_runs",
            "gauge",
            "Stored runs with no end timestamp; includes disconnected workers.",
        );
        for (p, n) in active {
            writeln!(out, "axe_active_runs{{project=\"{p}\"}} {n}").unwrap();
        }
        header(
            &mut out,
            "completed_runs",
            "gauge",
            "Retained ended runs; database-derived, not a process counter.",
        );
        header(
            &mut out,
            "completed_run_duration_seconds",
            "gauge",
            "Sum of retained ended run durations; database-derived.",
        );
        for (p, (n, seconds)) in completed {
            writeln!(out, "axe_completed_runs{{project=\"{p}\"}} {n}\naxe_completed_run_duration_seconds{{project=\"{p}\"}} {seconds}").unwrap();
        }
        let now = time::OffsetDateTime::now_utc().unix_timestamp() as f64;
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = (out, now);
        Ok(())
    }

    pub fn render(&self, connections: usize) -> String {
        let (mut out, timestamp) = self
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        header(
            &mut out,
            "snapshot_timestamp_seconds",
            "gauge",
            "Unix timestamp of last successful queue snapshot.",
        );
        writeln!(out, "axe_snapshot_timestamp_seconds {timestamp}").unwrap();
        header(
            &mut out,
            "connections",
            "gauge",
            "Accepted open connections including streams and this scrape.",
        );
        writeln!(out, "axe_connections {connections}").unwrap();
        header(
            &mut out,
            "http_requests_total",
            "counter",
            "HTTP responses selected; streams counted at response headers.",
        );
        header(&mut out, "http_request_duration_seconds", "histogram", "Time to response selection including request read; excludes stream lifetime and response transfer.");
        let http = self.http.lock().unwrap_or_else(|e| e.into_inner());
        for ((method, route, status), h) in http.iter() {
            let labels = format!("method=\"{method}\",route=\"{route}\",status=\"{status}\"");
            writeln!(out, "axe_http_requests_total{{{labels}}} {}", h.count).unwrap();
            for (i, bound) in BUCKETS.iter().enumerate() {
                writeln!(
                    out,
                    "axe_http_request_duration_seconds_bucket{{{labels},le=\"{bound}\"}} {}",
                    h.buckets[i]
                )
                .unwrap();
            }
            writeln!(out, "axe_http_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\naxe_http_request_duration_seconds_count{{{labels}}} {}\naxe_http_request_duration_seconds_sum{{{labels}}} {}",h.count,h.count,h.sum).unwrap();
        }
        out
    }
}
