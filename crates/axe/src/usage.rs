//! Per-run token usage and estimated cost, read from `claude -p` stream-json logs.
//!
//! `modelUsage` in a `result` event is a running total for the whole session, across
//! resumes; the top-level `usage` covers only that one result and is not used. Each
//! run keeps its totals, and its delta from the session's previous known run is
//! recomputed on every insert, so ingest order does not matter.
//!
//! A run with no usable result event is `unknown`, never zero. A total that goes
//! down (observed when a resume rewinds to an earlier snapshot) makes that run's own
//! spend unrecoverable: it is `reset`, excluded from cost, and rebases the session.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, Weekday};

/// v14: additive. Only these tables are written by ingest and backfill.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS run_usage (
 kind TEXT NOT NULL CHECK (kind IN ('task','bot')), run_id INTEGER NOT NULL,
 status TEXT NOT NULL CHECK (status IN ('known','unknown','reset')), reason TEXT,
 session_id TEXT, turns INTEGER, context_tokens INTEGER,
 cache_start TEXT CHECK (cache_start IN ('cold','warm')),
 source TEXT NOT NULL, recorded_at TEXT NOT NULL,
 PRIMARY KEY (kind, run_id)
);
CREATE INDEX IF NOT EXISTS run_usage_session ON run_usage(session_id);
CREATE TABLE IF NOT EXISTS run_usage_models (
 kind TEXT NOT NULL, run_id INTEGER NOT NULL, model TEXT NOT NULL,
 total_input INTEGER NOT NULL, total_output INTEGER NOT NULL, total_thinking INTEGER NOT NULL,
 total_cache_read INTEGER NOT NULL, total_cache_write INTEGER NOT NULL, total_cost REAL NOT NULL,
 input INTEGER, output INTEGER, thinking INTEGER, cache_read INTEGER, cache_write INTEGER,
 cost REAL,
 PRIMARY KEY (kind, run_id, model),
 FOREIGN KEY (kind, run_id) REFERENCES run_usage(kind, run_id)
);
";

/// Cost totals can differ by float noise between reports of the same spend.
const COST_EPSILON: f64 = 1e-6;
pub const NO_RESULT: &str = "no_result";
pub const NO_MODEL_USAGE: &str = "no_model_usage";
pub const LOG_MISSING: &str = "log_missing";
pub const LOG_REUSED: &str = "log_reused";
pub const UNREADABLE: &str = "unreadable";
/// Reported, not stored: an ended run with no `run_usage` row yet.
pub const NOT_RECORDED: &str = "not_recorded";
pub const COUNTER_RESET: &str = "counter_reset";

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub input: i64,
    pub output: i64,
    /// Included in `output`; never add the two.
    pub thinking: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Parsed {
    /// Set when the log held no usable result; `models` is then empty.
    pub unknown: Option<String>,
    pub session_id: Option<String>,
    pub models: BTreeMap<String, Totals>,
    pub turns: Option<i64>,
    pub context_tokens: Option<i64>,
    pub cache_start: Option<String>,
}

impl Parsed {
    pub fn unknown(reason: &str) -> Parsed {
        Parsed {
            unknown: Some(reason.into()),
            ..Default::default()
        }
    }
}

fn count(v: &Value, key: &str) -> i64 {
    v[key].as_i64().unwrap_or(0)
}

/// Reads one finished log. Never fails: an unreadable log is an `unknown` result.
pub fn parse_log(path: &Path) -> Parsed {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Parsed::unknown(LOG_MISSING),
        Err(_) => return Parsed::unknown(UNREADABLE),
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut parsed = Parsed::default();
    let mut results = 0;
    let mut turns = 0;
    let mut first: Option<Value> = None;
    let mut last: Option<Value> = None;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) if results > 0 => break,
            Err(_) => return Parsed::unknown(UNREADABLE),
        }
        // Most lines are neither kind; skip the JSON parse for them.
        let text = String::from_utf8_lossy(&line);
        if !text.contains("\"result\"") && !text.contains("\"assistant\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        match v["type"].as_str() {
            Some("assistant") if v["parent_tool_use_id"].is_null() => {
                let usage = &v["message"]["usage"];
                if usage.is_object() {
                    if first.is_none() {
                        first = Some(usage.clone());
                    }
                    last = Some(usage.clone());
                }
            }
            Some("result") => {
                results += 1;
                turns += count(&v, "num_turns");
                let Some(models) = v["modelUsage"].as_object().filter(|m| !m.is_empty()) else {
                    continue;
                };
                parsed.session_id = v["session_id"].as_str().map(str::to_owned);
                parsed.models = models
                    .iter()
                    .map(|(name, m)| {
                        let totals = Totals {
                            input: count(m, "inputTokens"),
                            output: count(m, "outputTokens"),
                            thinking: count(m, "thinkingTokens"),
                            cache_read: count(m, "cacheReadInputTokens"),
                            cache_write: count(m, "cacheCreationInputTokens"),
                            cost: m["costUSD"].as_f64().unwrap_or(0.0),
                        };
                        (name.clone(), totals)
                    })
                    .collect();
            }
            _ => {}
        }
    }
    if parsed.models.is_empty() || parsed.session_id.is_none() {
        return Parsed::unknown(if results == 0 {
            NO_RESULT
        } else {
            NO_MODEL_USAGE
        });
    }
    parsed.turns = Some(turns);
    parsed.context_tokens = last.map(|u| {
        count(&u, "input_tokens")
            + count(&u, "cache_read_input_tokens")
            + count(&u, "cache_creation_input_tokens")
    });
    parsed.cache_start = first.map(|u| {
        if count(&u, "cache_read_input_tokens") < count(&u, "cache_creation_input_tokens") {
            "cold".to_owned()
        } else {
            "warm".to_owned()
        }
    });
    parsed
}

/// Stores one run's usage; returns whether anything changed. Known data is never
/// replaced or downgraded, so repeating an ingest or backfill is a no-op.
pub fn record(
    conn: &Connection,
    kind: &str,
    run: i64,
    parsed: &Parsed,
    source: &str,
) -> Result<bool> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT status FROM run_usage WHERE kind=?1 AND run_id=?2",
            params![kind, run],
            |r| r.get(0),
        )
        .optional()?;
    match (existing.as_deref(), &parsed.unknown) {
        (Some("known" | "reset"), _) | (Some("unknown"), Some(_)) => return Ok(false),
        _ => {}
    }
    conn.execute(
        "INSERT INTO run_usage (kind, run_id, status, reason, session_id, turns, context_tokens,
           cache_start, source, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (kind, run_id) DO UPDATE SET status=excluded.status, reason=excluded.reason,
           session_id=excluded.session_id, turns=excluded.turns,
           context_tokens=excluded.context_tokens, cache_start=excluded.cache_start,
           source=excluded.source, recorded_at=excluded.recorded_at",
        params![
            kind,
            run,
            if parsed.unknown.is_some() {
                "unknown"
            } else {
                "known"
            },
            parsed.unknown,
            parsed.session_id,
            parsed.turns,
            parsed.context_tokens,
            parsed.cache_start,
            source,
            crate::db::now(),
        ],
    )?;
    for (model, t) in &parsed.models {
        conn.execute(
            "INSERT INTO run_usage_models (kind, run_id, model, total_input, total_output,
               total_thinking, total_cache_read, total_cache_write, total_cost)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                kind,
                run,
                model,
                t.input,
                t.output,
                t.thinking,
                t.cache_read,
                t.cache_write,
                t.cost
            ],
        )?;
    }
    if let Some(session) = parsed.session_id.as_deref() {
        recompute_session(conn, session)?;
    }
    Ok(true)
}

/// Stores usage from a dispatcher finish. A bad payload or a storage error must not
/// fail the finish itself, so it is contained in a savepoint and only logged.
pub fn record_best_effort(conn: &Connection, kind: &str, run: i64, usage: &Value) {
    if usage.is_null() {
        return;
    }
    let result = (|| -> Result<()> {
        let parsed: Parsed = serde_json::from_value(usage.clone())?;
        conn.execute_batch("SAVEPOINT run_usage")?;
        if let Err(e) = record(conn, kind, run, &parsed, "finish") {
            conn.execute_batch("ROLLBACK TO run_usage; RELEASE run_usage")?;
            return Err(e);
        }
        conn.execute_batch("RELEASE run_usage")?;
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("axe: run usage not recorded for {kind} run {run}: {e:#}");
    }
}

fn went_down(cur: &Totals, prev: &Totals) -> bool {
    cur.input < prev.input
        || cur.output < prev.output
        || cur.thinking < prev.thinking
        || cur.cache_read < prev.cache_read
        || cur.cache_write < prev.cache_write
        || cur.cost < prev.cost - COST_EPSILON
}

type SessionRun = (String, i64, BTreeMap<String, Totals>);

fn session_runs(conn: &Connection, session: &str) -> Result<Vec<SessionRun>> {
    let mut stmt = conn.prepare(
        "SELECT u.kind, u.run_id FROM run_usage u
         LEFT JOIN runs t ON u.kind='task' AND t.id=u.run_id
         LEFT JOIN bot_runs b ON u.kind='bot' AND b.id=u.run_id
         WHERE u.session_id=?1 AND u.status IN ('known','reset')
         ORDER BY COALESCE(t.started_at, b.started_at), u.run_id",
    )?;
    let ids = stmt
        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(String, i64)>>>()?;
    let mut models = conn.prepare(
        "SELECT model, total_input, total_output, total_thinking, total_cache_read,
           total_cache_write, total_cost FROM run_usage_models WHERE kind=?1 AND run_id=?2",
    )?;
    let mut out = Vec::with_capacity(ids.len());
    for (kind, run) in ids {
        let totals = models
            .query_map(params![kind, run], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Totals {
                        input: r.get(1)?,
                        output: r.get(2)?,
                        thinking: r.get(3)?,
                        cache_read: r.get(4)?,
                        cache_write: r.get(5)?,
                        cost: r.get(6)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        out.push((kind, run, totals));
    }
    Ok(out)
}

/// Rederives every delta in a session from its stored running totals.
fn recompute_session(conn: &Connection, session: &str) -> Result<()> {
    let mut prev: BTreeMap<String, Totals> = BTreeMap::new();
    for (kind, run, cur) in session_runs(conn, session)? {
        let zero = Totals::default();
        let reset = prev
            .iter()
            .any(|(m, p)| went_down(cur.get(m).unwrap_or(&zero), p));
        conn.execute(
            "UPDATE run_usage SET status=?3, reason=?4 WHERE kind=?1 AND run_id=?2",
            params![
                kind,
                run,
                if reset { "reset" } else { "known" },
                reset.then_some(COUNTER_RESET),
            ],
        )?;
        for (model, t) in &cur {
            let p = prev.get(model).unwrap_or(&zero);
            let delta = (!reset).then_some(Totals {
                input: t.input - p.input,
                output: t.output - p.output,
                thinking: t.thinking - p.thinking,
                cache_read: t.cache_read - p.cache_read,
                cache_write: t.cache_write - p.cache_write,
                cost: (t.cost - p.cost).max(0.0),
            });
            conn.execute(
                "UPDATE run_usage_models SET input=?4, output=?5, thinking=?6, cache_read=?7,
                   cache_write=?8, cost=?9 WHERE kind=?1 AND run_id=?2 AND model=?3",
                params![
                    kind,
                    run,
                    model,
                    delta.map(|d| d.input),
                    delta.map(|d| d.output),
                    delta.map(|d| d.thinking),
                    delta.map(|d| d.cache_read),
                    delta.map(|d| d.cache_write),
                    delta.map(|d| d.cost),
                ],
            )?;
        }
        prev = cur;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CatalogRun {
    pub kind: String,
    pub run: i64,
    pub log_path: String,
    /// A later run wrote to the same file, so it no longer describes this run.
    pub reused: bool,
}

/// Ended runs that have no known usage yet: what a backfill still has to read.
pub fn catalog(conn: &Connection) -> Result<Vec<CatalogRun>> {
    let mut stmt = conn.prepare(
        "SELECT 'task', r.id, r.log_path,
           EXISTS(SELECT 1 FROM runs n WHERE n.log_path=r.log_path AND n.id>r.id)
         FROM runs r LEFT JOIN run_usage u ON u.kind='task' AND u.run_id=r.id
         WHERE r.ended_at IS NOT NULL AND (u.status IS NULL OR u.status='unknown')
         UNION ALL
         SELECT 'bot', r.id, r.log_path, 0
         FROM bot_runs r LEFT JOIN run_usage u ON u.kind='bot' AND u.run_id=r.id
         WHERE r.ended_at IS NOT NULL AND (u.status IS NULL OR u.status='unknown')
         ORDER BY 1, 2",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(CatalogRun {
            kind: r.get(0)?,
            run: r.get(1)?,
            log_path: r.get(2)?,
            reused: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IngestItem {
    pub kind: String,
    pub run: i64,
    pub usage: Parsed,
}

/// Applies a backfill batch atomically; returns how many runs changed.
pub fn ingest(conn: &mut Connection, items: &[IngestItem]) -> Result<usize> {
    let tx = conn.transaction()?;
    let mut changed = 0;
    for item in items {
        let table = match item.kind.as_str() {
            "task" => "runs",
            "bot" => "bot_runs",
            other => bail!("unknown run kind {other:?}"),
        };
        let ended: Option<Option<String>> = tx
            .query_row(
                &format!("SELECT ended_at FROM {table} WHERE id=?1"),
                [item.run],
                |r| r.get(0),
            )
            .optional()?;
        match ended {
            None => bail!("no {} run {}", item.kind, item.run),
            Some(None) => bail!("{} run {} has not ended", item.kind, item.run),
            Some(Some(_)) => {}
        }
        if record(&tx, &item.kind, item.run, &item.usage, "backfill")? {
            changed += 1;
        }
    }
    tx.commit()?;
    Ok(changed)
}

fn epoch(s: &str) -> Result<i64> {
    Ok(OffsetDateTime::parse(s, &Rfc3339)
        .with_context(|| format!("bad timestamp {s:?}"))?
        .unix_timestamp())
}

#[derive(Default, Clone, Serialize)]
struct Bucket {
    runs: i64,
    known_runs: i64,
    unknown_runs: i64,
    cost: f64,
    input: i64,
    output: i64,
    thinking: i64,
    cache_read: i64,
    cache_write: i64,
}

impl Bucket {
    fn count(&mut self, known: bool) {
        self.runs += 1;
        if known {
            self.known_runs += 1;
        } else {
            self.unknown_runs += 1;
        }
    }
    fn spend(&mut self, d: &Totals) {
        self.cost += d.cost;
        self.input += d.input;
        self.output += d.output;
        self.thinking += d.thinking;
        self.cache_read += d.cache_read;
        self.cache_write += d.cache_write;
    }
}

/// One period of the Cost view's time series; `splits` holds cost per key.
#[derive(Default, Serialize)]
struct SeriesBucket {
    start: i64,
    end: i64,
    #[serde(flatten)]
    totals: Bucket,
    splits: BTreeMap<&'static str, BTreeMap<String, f64>>,
}

/// Longest allowed series, so a bad `step_days` cannot build a huge reply.
pub const MAX_SERIES_BUCKETS: i64 = 400;

fn series_buckets(since: i64, until: i64, step: i64) -> Vec<SeriesBucket> {
    (0..((until - since).max(0) + step - 1) / step)
        .map(|i| SeriesBucket {
            start: since + i * step,
            end: (since + (i + 1) * step).min(until),
            ..Default::default()
        })
        .collect()
}

struct RunFacts {
    kind: &'static str,
    id: i64,
    project: String,
    agent: String,
    mode: String,
    ended: i64,
    task: Option<i64>,
    bot: Option<String>,
}

fn load_runs(conn: &Connection, since: i64, until: i64) -> Result<Vec<RunFacts>> {
    let mut out = Vec::new();
    let mut tasks = conn.prepare(
        "SELECT id, project, agent, mode, ended_at, task_id FROM runs WHERE ended_at IS NOT NULL",
    )?;
    let mut rows = tasks.query([])?;
    while let Some(r) = rows.next()? {
        let ended = epoch(&r.get::<_, String>(4)?)?;
        if ended >= since && ended < until {
            out.push(RunFacts {
                kind: "task",
                id: r.get(0)?,
                project: r.get(1)?,
                agent: r.get(2)?,
                mode: r.get(3)?,
                ended,
                task: Some(r.get(5)?),
                bot: None,
            });
        }
    }
    let mut bots = conn.prepare(
        "SELECT r.id, b.project, r.agent, r.mode, r.ended_at, b.name
         FROM bot_runs r JOIN bots b ON b.id=r.bot_id WHERE r.ended_at IS NOT NULL",
    )?;
    let mut rows = bots.query([])?;
    while let Some(r) = rows.next()? {
        let ended = epoch(&r.get::<_, String>(4)?)?;
        if ended >= since && ended < until {
            out.push(RunFacts {
                kind: "bot",
                id: r.get(0)?,
                project: r.get(1)?,
                agent: r.get(2)?,
                mode: r.get(3)?,
                ended,
                task: None,
                bot: Some(r.get(5)?),
            });
        }
    }
    Ok(out)
}

fn context_bucket(tokens: Option<i64>) -> &'static str {
    match tokens {
        None => "unknown",
        Some(n) if n < 50_000 => "under 50k",
        Some(n) if n < 100_000 => "50k-100k",
        Some(n) if n < 200_000 => "100k-200k",
        Some(n) if n < 500_000 => "200k-500k",
        Some(_) => "500k and over",
    }
}

fn day_key(t: i64) -> Result<(String, String)> {
    let at = OffsetDateTime::from_unix_timestamp(t)?;
    let date = at.date();
    let monday = date - Duration::days(i64::from(date.weekday().number_days_from_monday()));
    debug_assert_eq!(monday.weekday(), Weekday::Monday);
    Ok((date.to_string(), monday.to_string()))
}

/// The strongest reason a bot run was woken: what the owner did outranks automation.
fn wake_reasons(conn: &Connection) -> Result<HashMap<i64, String>> {
    let rank = |r: &str| match r {
        "owner message" => 3,
        "button" => 2,
        "event" => 1,
        _ => 0,
    };
    let mut out: HashMap<i64, String> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT run_id, kind, key, actor FROM bot_wakes WHERE run_id IS NOT NULL")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let run: i64 = r.get(0)?;
        let kind: String = r.get(1)?;
        let key: String = r.get(2)?;
        let actor: String = r.get(3)?;
        // A ride-along digest is attached to a run; it never caused one.
        let reason = match kind.as_str() {
            "message" => "owner message".to_owned(),
            "button" => "button".to_owned(),
            _ if key.starts_with(crate::subscription::RIDE_PREFIX) => continue,
            _ if key.starts_with(crate::subscription::BATCH_PREFIX) => "event".to_owned(),
            _ if key.starts_with(crate::schedule::KEY_PREFIX) => format!(
                "schedule: {}",
                actor
                    .strip_prefix(crate::schedule::ACTOR_PREFIX)
                    .unwrap_or("?")
            ),
            _ => "scheduled".to_owned(),
        };
        let slot = out.entry(run).or_insert_with(|| reason.clone());
        if rank(&reason) > rank(slot) {
            *slot = reason;
        }
    }
    Ok(out)
}

type UsageRows = HashMap<(String, i64), (String, Option<String>, Option<i64>, Option<String>)>;

fn load_usage(conn: &Connection) -> Result<UsageRows> {
    let mut stmt = conn.prepare(
        "SELECT kind, run_id, status, reason, context_tokens, cache_start FROM run_usage",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, i64>(1)?),
            (r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

type ModelRows = HashMap<(String, i64), Vec<(String, Totals)>>;

fn load_models(conn: &Connection) -> Result<ModelRows> {
    let mut out: ModelRows = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT kind, run_id, model, input, output, thinking, cache_read, cache_write, cost
         FROM run_usage_models WHERE cost IS NOT NULL",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        out.entry((r.get(0)?, r.get(1)?)).or_default().push((
            r.get(2)?,
            Totals {
                input: r.get(3)?,
                output: r.get(4)?,
                thinking: r.get(5)?,
                cache_read: r.get(6)?,
                cache_write: r.get(7)?,
                cost: r.get(8)?,
            },
        ));
    }
    Ok(out)
}

fn id_labels(conn: &Connection, sql: &str) -> Result<HashMap<i64, String>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn routine_names(conn: &Connection) -> Result<HashMap<i64, String>> {
    let tasks = id_labels(
        conn,
        "SELECT o.task_id, r.config FROM routine_occurrences o JOIN routines r ON r.id=o.routine_id",
    )?;
    Ok(tasks
        .into_iter()
        .map(|(task, config)| {
            let name = serde_json::from_str::<Value>(&config)
                .ok()
                .and_then(|c| c["name"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "unnamed routine".into());
            (task, name)
        })
        .collect())
}

/// Every dimension the Cost view breaks down by; each is a valid `breakdown`.
pub const BREAKDOWNS: [&str; 15] = [
    "project", "source", "bot", "routine", "workload", "wake", "agent", "model", "mode", "cache",
    "context", "outcome", "day", "week", "kind",
];

/// Splits every series bucket always carries; `breakdown` adds one more.
const SERIES_SPLITS: [&str; 3] = ["project", "source", "model"];
const NOT_A_BOT: &str = "not a bot";
const NOT_A_ROUTINE: &str = "not a routine";

/// Narrows and shapes a Cost report. The default is the unfiltered report.
#[derive(Default, Clone, Copy)]
pub struct CostQuery<'a> {
    pub project: Option<&'a str>,
    pub source: Option<&'a str>,
    pub bot: Option<&'a str>,
    pub routine: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub workload: Option<&'a str>,
    /// Keeps only this model's spend. A run with unknown usage stays, as unknown:
    /// nothing says it did not use this model.
    pub model: Option<&'a str>,
    /// One of `BREAKDOWNS`: adds that split to every series bucket.
    pub breakdown: Option<&'a str>,
    /// Adds `filter_options`: every value in the period, ignoring the filters.
    pub options: bool,
}

impl<'a> CostQuery<'a> {
    /// The filters that compare against one dimension of a run.
    fn dimensions(&self) -> [(&'static str, Option<&'a str>); 6] {
        [
            ("project", self.project),
            ("source", self.source),
            ("bot", self.bot),
            ("routine", self.routine),
            ("agent", self.agent),
            ("workload", self.workload),
        ]
    }
}

/// One JSON document with every Cost view breakdown over ended runs in
/// `[since, until)` that `query` keeps. Every group covers every kept run, so all
/// totals agree. `step_days` adds a time series of that bucket width, anchored on
/// `since` (the last bucket ends at `until` and may be shorter).
pub fn cost_stats(
    conn: &Connection,
    since: i64,
    until: i64,
    step_days: Option<i64>,
    query: &CostQuery,
) -> Result<Value> {
    let usage = load_usage(conn)?;
    let models = load_models(conn)?;
    let wakes = wake_reasons(conn)?;
    let routines = routine_names(conn)?;
    let states = id_labels(conn, "SELECT id, state FROM tasks")?;
    let workloads = id_labels(
        conn,
        "SELECT task_id, to_value FROM events
         WHERE kind='workload' AND to_value IS NOT NULL ORDER BY id",
    )?;
    let mut total = Bucket::default();
    let mut groups: BTreeMap<&'static str, BTreeMap<String, Bucket>> = BTreeMap::new();
    let mut reasons: BTreeMap<String, i64> = BTreeMap::new();
    let mut coverage_by_agent: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut finished: HashSet<i64> = HashSet::new();
    let mut finished_cost = 0.0;
    let step = step_days.map(|d| d * 86_400);
    let mut series = step.map_or_else(Vec::new, |s| series_buckets(since, until, s));
    let mut options: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    for run in load_runs(conn, since, until)? {
        let key = (run.kind.to_owned(), run.id);
        let row = usage.get(&key);
        let known = row.is_some_and(|r| r.0 == "known");
        let all_spend: &[(String, Totals)] = if known {
            models.get(&key).map_or(&[], Vec::as_slice)
        } else {
            &[]
        };
        let (day, week) = day_key(run.ended)?;
        let routine = run.task.and_then(|t| routines.get(&t).cloned());
        let state = run.task.and_then(|t| states.get(&t).cloned());
        let source = match (&run.bot, &routine) {
            (Some(_), _) => "bot",
            (None, Some(_)) => "routine",
            (None, None) => "one-off task",
        };
        let outcome = match (run.kind, state.as_deref()) {
            ("bot", _) => "bot",
            (_, Some("done")) => "done",
            (_, Some("escalated")) => "escalated",
            _ => "other",
        };
        let wake = match &run.bot {
            None => "task run",
            Some(_) => wakes.get(&run.id).map_or("heartbeat", String::as_str),
        };
        let workload = match run.task {
            None => "bot".to_owned(),
            Some(t) => workloads
                .get(&t)
                .cloned()
                .unwrap_or_else(|| crate::flow::UNSET.to_owned()),
        };
        let cache = row
            .filter(|_| known)
            .and_then(|r| r.3.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let context = context_bucket(row.filter(|_| known).and_then(|r| r.2));
        let dims: [(&'static str, String); 14] = [
            ("project", run.project.clone()),
            ("source", source.to_owned()),
            (
                "bot",
                run.bot.clone().unwrap_or_else(|| NOT_A_BOT.to_owned()),
            ),
            (
                "routine",
                routine.unwrap_or_else(|| NOT_A_ROUTINE.to_owned()),
            ),
            ("workload", workload),
            ("wake", wake.to_owned()),
            ("agent", run.agent.clone()),
            ("mode", run.mode.clone()),
            ("cache", cache),
            ("context", context.to_owned()),
            ("outcome", outcome.to_owned()),
            ("day", day),
            ("week", week),
            ("kind", run.kind.to_owned()),
        ];
        let dim = |name: &str| {
            dims.iter()
                .find(|(group, _)| *group == name)
                .map(|(_, value)| value.as_str())
        };
        if query.options {
            for (name, _) in query.dimensions() {
                if let Some(value) = dim(name).filter(|v| ![NOT_A_BOT, NOT_A_ROUTINE].contains(v)) {
                    options.entry(name).or_default().insert(value.to_owned());
                }
            }
            let names = options.entry("model").or_default();
            names.extend(all_spend.iter().map(|(m, _)| m.clone()));
            if all_spend.is_empty() {
                names.insert("unknown".to_owned());
            }
        }
        if query
            .dimensions()
            .iter()
            .any(|(name, want)| want.is_some() && dim(name) != *want)
        {
            continue;
        }
        let spend: Vec<&(String, Totals)> = all_spend
            .iter()
            .filter(|(m, _)| query.model.is_none_or(|want| m == want))
            .collect();
        // Known usage says whether the run used the model; unknown usage cannot.
        let on_unknown_row = all_spend.is_empty() && query.model == Some("unknown");
        if known && query.model.is_some() && spend.is_empty() && !on_unknown_row {
            continue;
        }
        if !known {
            let reason = row
                .and_then(|r| r.1.clone())
                .unwrap_or_else(|| NOT_RECORDED.to_owned());
            *reasons.entry(reason).or_default() += 1;
        }
        let mut sum = Bucket::default();
        sum.count(known);
        for (_, d) in &spend {
            sum.spend(d);
        }
        if known && outcome == "done" {
            finished.insert(run.task.unwrap_or_default());
            finished_cost += sum.cost;
        }
        for (group, value) in &dims {
            let b = groups
                .entry(*group)
                .or_default()
                .entry(value.clone())
                .or_default();
            b.count(known);
            for (_, d) in &spend {
                b.spend(d);
            }
        }
        // A run spans one row per model; an unknown run still has to appear once.
        let by_model = groups.entry("model").or_default();
        if spend.is_empty() {
            by_model
                .entry("unknown".to_owned())
                .or_default()
                .count(known);
        }
        // The run itself counts once, for the model that cost the most.
        let main = spend
            .iter()
            .enumerate()
            .max_by(|a, b| a.1 .1.cost.total_cmp(&b.1 .1.cost))
            .map(|(i, _)| i);
        for (i, (model, d)) in spend.iter().map(|m| (&m.0, &m.1)).enumerate() {
            let b = by_model.entry(model.clone()).or_default();
            if Some(i) == main {
                b.count(known);
            }
            b.spend(d);
        }
        if let Some(step) = step {
            let bucket = &mut series[((run.ended - since) / step) as usize];
            bucket.totals.count(known);
            // Fixed splits come first, so a repeated choice cannot count twice.
            let chosen = query
                .breakdown
                .filter(|b| !SERIES_SPLITS.contains(b))
                .and_then(|b| dims.iter().find(|(group, _)| *group == b))
                .map(|(group, value)| (*group, value.as_str()));
            for (model, d) in &spend {
                bucket.totals.spend(d);
                for (dim, key) in [
                    ("project", run.project.as_str()),
                    ("source", source),
                    ("model", model.as_str()),
                ]
                .into_iter()
                .chain(chosen)
                {
                    *bucket
                        .splits
                        .entry(dim)
                        .or_default()
                        .entry(key.to_owned())
                        .or_default() += d.cost;
                }
            }
        }
        let agent = coverage_by_agent.entry(run.agent).or_default();
        agent.count(known);
        total.count(known);
        for (_, d) in &spend {
            total.spend(d);
        }
    }
    let token_types = json!([
        {"key": "input", "tokens": total.input},
        {"key": "output (excluding thinking)", "tokens": total.output - total.thinking},
        {"key": "thinking", "tokens": total.thinking},
        {"key": "cache read", "tokens": total.cache_read},
        {"key": "cache write", "tokens": total.cache_write},
    ]);
    let list = |m: &BTreeMap<String, Bucket>| -> Vec<Value> {
        m.iter()
            .map(|(key, b)| {
                let mut v = serde_json::to_value(b).unwrap_or(Value::Null);
                v["key"] = json!(key);
                v
            })
            .collect()
    };
    let breakdowns: BTreeMap<&str, Vec<Value>> =
        groups.iter().map(|(k, m)| (*k, list(m))).collect();
    let finished_tasks = finished.len() as i64;
    // The earliest run with usage, in any period: older periods have no data. Filters
    // do not move it: a project's quiet early days are not missing data.
    let first_known = match step {
        None => None,
        Some(_) => load_runs(conn, i64::MIN, i64::MAX)?
            .iter()
            .filter(|r| {
                usage
                    .get(&(r.kind.to_owned(), r.id))
                    .is_some_and(|u| u.0 == "known")
            })
            .map(|r| r.ended)
            .min(),
    };
    let mut out = json!({
        "since": since,
        "until": until,
        "total": total,
        "token_types": token_types,
        "breakdowns": breakdowns,
        "coverage": {
            "runs": total.runs,
            "known": total.known_runs,
            "unknown": total.unknown_runs,
            "unknown_reasons": reasons,
            "by_agent": list(&coverage_by_agent),
        },
        "finished": {
            "tasks": finished_tasks,
            "cost": finished_cost,
            "cost_per_task": (finished_tasks > 0).then(|| finished_cost / finished_tasks as f64),
        },
    });
    if query.options {
        out["filter_options"] = json!(options);
    }
    if let Some(days) = step_days {
        out["series"] = json!({
            "step_days": days,
            "first_known": first_known,
            "buckets": series,
        });
    }
    Ok(out)
}
