//! Services: long-running processes a bot declares and the node runs on
//! its machine (`dispatch/services.rs`), the way routines are crontab and
//! services are systemd.
//!
//! The server holds each declaration (name, command, cwd, env, restart
//! policy, enabled) and the state the node last reported for it. The
//! dispatcher syncs both ways ([`report`]) and raises the events a bot must
//! hear about ([`notify`]): a crash loop and an upcoming stop wake the owning
//! bot; a crash loop that persists alerts the owner as well. The change
//! between two reports is also a set of service events ([`events`]) for the
//! bots subscribed to them (`subscription.rs`); a new service subscribes its
//! owning bot to its failures.
//!
//! A service with a [`Schedule`] is a scheduled service (the crontab of the
//! node, next to the long-running ones that are its systemd): the node runs
//! it at each cron time, one run at a time, kills a run past its timeout and
//! reports the last run's exit, end and the next run. Runs missed while the
//! node was down are not replayed: when the node starts and a run time passed
//! since the last run, it runs once at once (the catch-up, unless the
//! schedule turns it off), then follows the schedule.
use std::collections::BTreeMap;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bot;
use crate::db::{self, Actor};

/// v22 (v23 adds `schedule`, [`migrate`]): additive, so a database lowered to `user_version` 21 still opens with
/// that binary and upgrades again in place. `status` is the JSON object the
/// dispatcher last reported ([`Status`]).
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS services (
 id INTEGER PRIMARY KEY, bot_id INTEGER NOT NULL REFERENCES bots(id), name TEXT NOT NULL,
 command TEXT NOT NULL, cwd TEXT, env TEXT NOT NULL DEFAULT '{}',
 restart TEXT NOT NULL CHECK (restart IN ('always','on-failure','never')),
 enabled INTEGER NOT NULL, status TEXT NOT NULL DEFAULT '{}',
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
 UNIQUE (bot_id, name)
);";

/// v23: `services.schedule`, the [`Schedule`] as JSON, NULL for a
/// long-running service. Additive and idempotent, so a database lowered to
/// `user_version` 22 still opens with that binary (which runs no schedule:
/// a scheduled service is stored `restart never`, so it would run once).
pub fn migrate(tx: &rusqlite::Transaction) -> Result<()> {
    let has: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('services') WHERE name='schedule')",
        [],
        |r| r.get(0),
    )?;
    if !has {
        tx.execute_batch("ALTER TABLE services ADD COLUMN schedule TEXT")?;
    }
    Ok(())
}

/// Restart policies: after any exit, after a failure (non-zero exit or a
/// signal), or never.
pub const RESTARTS: [&str; 3] = ["always", "on-failure", "never"];
/// Most bytes in a command, a cwd or one env value.
pub const VALUE_MAX: usize = 4096;
/// Most env entries of one service.
pub const ENV_MAX: usize = 64;
/// Most services one bot declares.
pub const PER_BOT_MAX: usize = 16;
/// A scheduled run's timeout when the schedule sets none, and the longest.
pub const TIMEOUT_DEFAULT: u64 = 3600;
pub const TIMEOUT_MAX: u64 = 86_400;

/// When a scheduled service runs: a standard five-field cron in an IANA time
/// zone (as routines), with a timeout per run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "ServiceSchedule"))]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub cron: String,
    #[serde(default = "utc")]
    pub timezone: String,
    /// A run still going this long is stopped (SIGTERM, then SIGKILL after
    /// the node's stop grace) and counts as failed.
    #[serde(default = "timeout_default")]
    pub timeout_seconds: u64,
    /// One run at node start when a run time passed while it was down.
    #[serde(default = "yes")]
    pub catch_up: bool,
}

fn utc() -> String {
    "UTC".into()
}
fn timeout_default() -> u64 {
    TIMEOUT_DEFAULT
}
fn yes() -> bool {
    true
}

impl Schedule {
    pub fn validate(&self) -> Result<()> {
        crate::routine::parse_cron(&self.cron, &self.timezone)?;
        if !(1..=TIMEOUT_MAX).contains(&self.timeout_seconds) {
            bail!("timeout must be 1..={TIMEOUT_MAX} seconds");
        }
        Ok(())
    }

    /// The first run time after epoch `now`.
    pub fn next_after(&self, now: i64) -> Result<i64> {
        crate::routine::next_after(&self.cron, &self.timezone, now)
    }

    /// The last run time at or before epoch `now`.
    pub fn latest_at(&self, now: i64) -> Result<i64> {
        crate::routine::latest_at(&self.cron, &self.timezone, now)
    }
}

/// What a declaration sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct NewService {
    /// A shell command (`sh -c`).
    pub command: String,
    /// Where it runs: absolute, or relative to the bot's run folder (the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<BTreeMap<String, String>>", optional))]
    pub env: BTreeMap<String, String>,
    /// `never` for a scheduled service.
    #[serde(default = "on_failure")]
    pub restart: String,
    /// Set: a scheduled service, run at these times instead of kept running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub schedule: Option<Schedule>,
}

fn on_failure() -> String {
    "on-failure".into()
}

impl NewService {
    pub fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() || self.command.len() > VALUE_MAX {
            bail!("command must be a non-empty shell command of at most {VALUE_MAX} bytes");
        }
        if self.command.contains('\0') {
            bail!("command must not contain NUL");
        }
        if let Some(c) = &self.cwd {
            if c.trim().is_empty() || c.len() > VALUE_MAX || c.contains('\0') {
                bail!("cwd must be a non-empty path of at most {VALUE_MAX} bytes");
            }
        }
        if !RESTARTS.contains(&self.restart.as_str()) {
            bail!(
                "restart {:?} must be one of {}",
                self.restart,
                RESTARTS.join(", ")
            );
        }
        if self.env.len() > ENV_MAX {
            bail!("at most {ENV_MAX} env entries");
        }
        for (k, v) in &self.env {
            let ok = k
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && k.len() <= 128;
            if !ok {
                bail!("env name {k:?} must be letters, digits and '_', not starting with a digit");
            }
            if k.starts_with("AXE_") {
                bail!("env name {k:?}: AXE_ variables are set by the node");
            }
            if v.len() > VALUE_MAX || v.contains('\0') {
                bail!("env {k}: the value must be at most {VALUE_MAX} bytes without NUL");
            }
        }
        if let Some(s) = &self.schedule {
            s.validate()?;
            if self.restart != "never" {
                bail!("a scheduled service has no restart policy (restart never)");
            }
        }
        Ok(())
    }
}

/// What the node reports for one service; stored as `services.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "ServiceStatus"))]
pub struct Status {
    /// `starting`, `running`, `backoff` (waiting to restart), `stopping`,
    /// `stopped` or `exited` (done, not restarted by its policy); a scheduled
    /// service is `scheduled` between runs.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub pid: Option<u32>,
    /// When the current (or last) process started: a scheduled service's
    /// last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub started_at: Option<String>,
    /// Restarts since the node started the service.
    #[serde(default)]
    pub restarts: u64,
    /// Failures since the node started the service: unsuccessful exits
    /// it did not ask for, and starts that failed.
    #[serde(default)]
    pub failures: u64,
    /// How the last process ended: `exit N` or `signal N`, or for a
    /// scheduled run past its timeout `timeout after Ns (signal N)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub last_exit: Option<String>,
    /// When the next restart is due, in `backoff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_start_at: Option<String>,
    /// Since when it has been crash looping, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub crash_loop_since: Option<String>,
    /// Why it could not start, if it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub error: Option<String>,
    /// A scheduled service: when its last run ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub finished_at: Option<String>,
    /// A scheduled service: when it runs next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_run_at: Option<String>,
    /// The log file on the node's machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub log: Option<String>,
    /// The node that reported it, and when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dispatcher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reported_at: Option<String>,
}

/// One declared service with its last reported status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Service {
    pub bot: String,
    /// The bot's folder key (`Bot::home`): where its run folder and logs are.
    pub home: String,
    pub name: String,
    #[serde(flatten)]
    pub spec: NewService,
    pub enabled: bool,
    /// Empty (`state` "") until a node reported it.
    #[serde(default)]
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
}

impl Service {
    /// The key the node tracks it by: `<bot>/<name>`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.bot, self.name)
    }

    /// `long-running`, or `scheduled` with a schedule. The web Services
    /// page (`web/lib/services.ts`) reads it the same way.
    pub fn kind(&self) -> &'static str {
        if self.spec.schedule.is_some() {
            "scheduled"
        } else {
            "long-running"
        }
    }

    /// The state a person reads: `crash-looping` while in a crash loop,
    /// `stopped` when it is off and nothing runs, `pending` when it is on
    /// and no node reported it yet, else the reported state
    /// (`starting`, `running`, `backoff`, `stopping`, `exited`, `stopped`).
    pub fn display_state(&self) -> &str {
        let st = &self.status;
        if st.crash_loop_since.is_some() && self.enabled {
            return "crash-looping";
        }
        match st.state.as_str() {
            "" if self.enabled => "pending",
            "" => "stopped",
            s => s,
        }
    }

    /// One line for `/status` and `axe bot show`: name, kind, state, uptime
    /// or the next restart, restarts, the last exit, and a stale report.
    /// `now` is epoch seconds.
    pub fn summary(&self, now: i64) -> String {
        let st = &self.status;
        let ago = |at: &Option<String>| {
            at.as_deref()
                .and_then(bot::epoch_of)
                .map(|t| crate::board::age(now - t))
        };
        let state = self.display_state();
        let kind = match &self.spec.schedule {
            Some(s) => format!("scheduled {} {}", s.cron, s.timezone),
            None => self.kind().to_string(),
        };
        let mut parts = vec![format!("{} ({kind}): {state}", self.name)];
        if !self.enabled && state != "stopped" {
            parts.push("turned off".into());
        }
        if self.spec.schedule.is_some() {
            if let Some(at) = ago(&st.started_at) {
                let run = if state == "running" {
                    format!("running {at}")
                } else {
                    format!(
                        "last run {at} ago: {}",
                        st.last_exit.as_deref().unwrap_or("no exit")
                    )
                };
                parts.push(run);
            }
            if let Some(at) = st.next_run_at.as_deref().and_then(bot::epoch_of) {
                if self.enabled {
                    parts.push(format!("next run in {}", crate::board::age(at - now)));
                }
            }
            self.flags(now, &mut parts);
            return parts.join(", ");
        }
        if state == "running" {
            if let Some(up) = ago(&st.started_at) {
                parts.push(format!("up {up}"));
            }
        }
        if let Some(at) = st.next_start_at.as_deref().and_then(bot::epoch_of) {
            if state == "backoff" || state == "crash-looping" {
                parts.push(format!("restart in {}", crate::board::age(at - now)));
            }
        }
        parts.push(match st.restarts {
            1 => "1 restart".into(),
            n => format!("{n} restarts"),
        });
        if let Some(e) = &st.last_exit {
            parts.push(format!("last {e}"));
        }
        self.flags(now, &mut parts);
        parts.join(", ")
    }

    /// The error and a stale report, ending [`Self::summary`].
    fn flags(&self, now: i64, parts: &mut Vec<String>) {
        let st = &self.status;
        if let Some(e) = &st.error {
            parts.push(format!("error: {e}"));
        }
        if let Some(t) = st.reported_at.as_deref().and_then(bot::epoch_of) {
            if now - t > STALE_SECONDS {
                parts.push(format!(
                    "no report from {} for {}",
                    st.dispatcher.as_deref().unwrap_or("its node"),
                    crate::board::age(now - t)
                ));
            }
        }
    }
}

/// A node reports its services every few seconds; a report older
/// than this reads as stale (the node is down or cut off).
pub const STALE_SECONDS: i64 = 60;

/// `home` as [`bot::Bot::home`]: the name before the bot's first rename, else its name.
const SELECT: &str = "SELECT b.name, (SELECT json_extract(e.body,'$.from') FROM bot_events e
 WHERE e.bot_id=b.id AND e.kind='rename' ORDER BY e.id LIMIT 1), s.name, s.command, s.cwd, s.env, s.restart,
 s.enabled, s.status, s.created_at, s.updated_at, s.schedule FROM services s JOIN bots b ON b.id=s.bot_id";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Service> {
    let json_col = |i: usize, raw: String| -> rusqlite::Result<Value> {
        serde_json::from_str(&raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
    };
    let bot: String = r.get(0)?;
    let home: Option<String> = r.get(1)?;
    let env = json_col(5, r.get(5)?)?;
    let status = json_col(8, r.get(8)?)?;
    let schedule = match r.get::<_, Option<String>>(11)? {
        Some(raw) => serde_json::from_value(json_col(11, raw)?).ok(),
        None => None,
    };
    Ok(Service {
        home: home.unwrap_or_else(|| bot.clone()),
        bot,
        name: r.get(2)?,
        spec: NewService {
            command: r.get(3)?,
            cwd: r.get(4)?,
            env: serde_json::from_value(env).unwrap_or_default(),
            restart: r.get(6)?,
            schedule,
        },
        enabled: r.get(7)?,
        status: serde_json::from_value(status).unwrap_or_default(),
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

/// Every service, or one bot's, by bot then name.
pub fn list(conn: &Connection, bot: Option<&str>) -> Result<Vec<Service>> {
    let rows = match bot {
        Some(name) => {
            let b = bot::get(conn, name)?;
            conn.prepare(&format!("{SELECT} WHERE s.bot_id=?1 ORDER BY s.name"))?
                .query_map([b.id], row)?
                .collect::<rusqlite::Result<_>>()?
        }
        None => conn
            .prepare(&format!("{SELECT} ORDER BY b.name, s.name"))?
            .query_map([], row)?
            .collect::<rusqlite::Result<_>>()?,
    };
    Ok(rows)
}

fn find(conn: &Connection, bot_id: i64, name: &str) -> Result<Option<Service>> {
    Ok(conn
        .query_row(
            &format!("{SELECT} WHERE s.bot_id=?1 AND s.name=?2"),
            params![bot_id, name],
            row,
        )
        .optional()?)
}

/// Bot `bot`'s service `name`.
pub fn get(conn: &Connection, bot: &str, name: &str) -> Result<Service> {
    let b = bot::get(conn, bot)?;
    match find(conn, b.id, name)? {
        Some(s) => Ok(s),
        None => bail!("bot {bot} has no service {name}"),
    }
}

/// Declares bot `bot`'s service `name`, or replaces its definition. A new
/// service is enabled; a replaced one keeps its enabled switch, and the
/// dispatcher restarts it with the new definition.
pub fn put(
    conn: &mut Connection,
    actor: &Actor,
    bot: &str,
    name: &str,
    new: &NewService,
) -> Result<Service> {
    db::validate_name("service", name)?;
    let mut new = new.clone();
    if new.schedule.is_some() {
        new.restart = "never".into();
    }
    new.validate()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot)?;
    let now = db::now();
    let env = serde_json::to_string(&new.env)?;
    let schedule = new
        .schedule
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let old = find(&tx, b.id, name)?;
    if old.is_none() {
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM services WHERE bot_id=?1",
            [b.id],
            |r| r.get(0),
        )?;
        if count as usize >= PER_BOT_MAX {
            bail!("bot {bot} already has {PER_BOT_MAX} services; remove one first");
        }
    }
    tx.execute(
        "INSERT INTO services (bot_id, name, command, cwd, env, restart, enabled, created_at, updated_at, schedule)
         VALUES (?1,?2,?3,?4,?5,?6,1,?7,?7,?8)
         ON CONFLICT(bot_id, name) DO UPDATE SET command=excluded.command, cwd=excluded.cwd,
           env=excluded.env, restart=excluded.restart, updated_at=excluded.updated_at,
           schedule=excluded.schedule",
        params![b.id, name, new.command, new.cwd, env, new.restart, now, schedule],
    )?;
    if old.is_none() {
        let id: i64 = tx.query_row(
            "SELECT id FROM services WHERE bot_id=?1 AND name=?2",
            params![b.id, name],
            |r| r.get(0),
        )?;
        crate::subscription::default_service(&tx, actor, b.id, id)?;
    }
    let action = if old.is_some() { "update" } else { "add" };
    bot::event(
        &tx,
        b.id,
        actor,
        "service",
        &json!({"service":name,"action":action,"command":new.command,"cwd":new.cwd,
            "env":new.env.keys().collect::<Vec<_>>(),"restart":new.restart,
            "schedule":new.schedule})
        .to_string(),
    )?;
    let s = find(&tx, b.id, name)?.expect("service just written");
    tx.commit()?;
    Ok(s)
}

/// Turns a service on (`start`) or off (`stop`); the node follows.
pub fn set_enabled(
    conn: &mut Connection,
    actor: &Actor,
    bot: &str,
    name: &str,
    enabled: bool,
) -> Result<Service> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot)?;
    if find(&tx, b.id, name)?.is_none() {
        bail!("bot {bot} has no service {name}");
    }
    tx.execute(
        "UPDATE services SET enabled=?3, updated_at=?4 WHERE bot_id=?1 AND name=?2",
        params![b.id, name, enabled, db::now()],
    )?;
    let action = if enabled { "start" } else { "stop" };
    bot::event(
        &tx,
        b.id,
        actor,
        "service",
        &json!({"service":name,"action":action}).to_string(),
    )?;
    let s = find(&tx, b.id, name)?.expect("service exists");
    tx.commit()?;
    Ok(s)
}

/// Removes a service; the node stops it. Returns what it held.
pub fn remove(conn: &mut Connection, actor: &Actor, bot: &str, name: &str) -> Result<Service> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot)?;
    let Some(s) = find(&tx, b.id, name)? else {
        bail!("bot {bot} has no service {name}");
    };
    let id: i64 = tx.query_row(
        "SELECT id FROM services WHERE bot_id=?1 AND name=?2",
        params![b.id, name],
        |r| r.get(0),
    )?;
    crate::subscription::drop_source(
        &tx,
        actor,
        &format!("{}{id}", crate::subscription::SERVICE_PREFIX),
        "service removed",
    )?;
    tx.execute(
        "DELETE FROM services WHERE bot_id=?1 AND name=?2",
        params![b.id, name],
    )?;
    bot::event(
        &tx,
        b.id,
        actor,
        "service",
        &json!({"service":name,"action":"remove"}).to_string(),
    )?;
    tx.commit()?;
    Ok(s)
}

/// The node `dispatcher` (generation `generation`, the registered one)
/// reports the services it runs, keyed `<bot>/<name>`; returns every
/// declaration, which it then runs. Reports for services no longer declared
/// are ignored.
pub fn report(
    conn: &mut Connection,
    dispatcher: &str,
    generation: &str,
    statuses: &BTreeMap<String, Status>,
) -> Result<Vec<Service>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM dispatcher WHERE id=?1 AND generation=?2)",
        params![dispatcher, generation],
        |r| r.get(0),
    )?;
    if !current {
        bail!("stale dispatcher ownership");
    }
    let at = db::now();
    for (key, status) in statuses {
        let Some((bot, name)) = key.split_once('/') else {
            continue;
        };
        let mut status = status.clone();
        status.dispatcher = Some(dispatcher.to_string());
        status.reported_at = Some(at.clone());
        let old: Option<(i64, i64, String)> = tx
            .query_row(
                "SELECT s.id, s.bot_id, s.status FROM services s JOIN bots b ON b.id=s.bot_id
                 WHERE b.name=?1 AND s.name=?2",
                params![bot, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((id, bot_id, old)) = old else {
            continue;
        };
        tx.execute(
            "UPDATE services SET status=?2 WHERE id=?1",
            params![id, serde_json::to_string(&status)?],
        )?;
        let old: Status = serde_json::from_str(&old).unwrap_or_default();
        for (event, detail) in events(&old, &status) {
            let line = if detail.is_empty() {
                key.clone()
            } else {
                format!("{key}: {detail}")
            };
            crate::subscription::service_event(&tx, id, bot_id, event, &line)?;
        }
    }
    let all = list(&tx, None)?;
    tx.commit()?;
    Ok(all)
}

/// The service events between two reported statuses, oldest kind first, each
/// with a detail: `failed` (counted by `failures`, which restarts at 0 with
/// the node), `crash_loop`, `run_finished` (it ended and is not
/// restarted, or a scheduled run ended: a new `finished_at`), `stopped` and
/// `started` (a new process, seen by its start time).
pub fn events(old: &Status, new: &Status) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let failures = if new.failures >= old.failures {
        new.failures - old.failures
    } else {
        new.failures
    };
    if failures > 0 {
        let why = new
            .error
            .clone()
            .or_else(|| new.last_exit.clone())
            .unwrap_or_else(|| "failed".into());
        let times = if failures > 1 {
            format!(" ({failures} times)")
        } else {
            String::new()
        };
        out.push(("failed", format!("{why}{times}")));
    }
    if new.crash_loop_since.is_some() && new.crash_loop_since != old.crash_loop_since {
        out.push((
            "crash_loop",
            format!("crash looping, {} restarts", new.restarts),
        ));
    }
    let new_run = new.started_at != old.started_at;
    let scheduled_end = new.finished_at.is_some() && new.finished_at != old.finished_at;
    if (new.state == "exited" && (old.state != "exited" || new_run)) || scheduled_end {
        out.push((
            "run_finished",
            new.last_exit.clone().unwrap_or_else(|| "exited".into()),
        ));
    }
    if new.state == "stopped" && !matches!(old.state.as_str(), "stopped" | "") {
        out.push(("stopped", String::new()));
    }
    if new.state == "running" && (old.state != "running" || new_run) {
        out.push((
            "started",
            new.pid.map_or(String::new(), |p| format!("pid {p}")),
        ));
    }
    out
}

/// An event the node raises for a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub bot: String,
    /// `crash_loop` and `stopping` wake the bot; `escalate` also alerts the
    /// owner (the server sends it, [`Notified::owner_text`]).
    pub kind: String,
    /// The services it is about.
    pub services: Vec<String>,
    /// Wake text for the bot.
    pub text: String,
    /// Idempotency key: one wake per key and bot.
    pub key: String,
}

/// What [`notify`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notified {
    pub wake: Option<i64>,
    /// For `escalate`: the text for the owner, when this notice is new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_text: Option<String>,
}

pub const NOTICES: [&str; 3] = ["crash_loop", "stopping", "escalate"];

/// Records `n` as a bot event and wakes the bot now with its text. A key
/// already used returns the stored wake and no owner text, so a retried
/// notice wakes and alerts once.
pub fn notify(conn: &mut Connection, n: &Notice) -> Result<Notified> {
    if !NOTICES.contains(&n.kind.as_str()) {
        bail!(
            "notice kind {:?} must be one of {}",
            n.kind,
            NOTICES.join(", ")
        );
    }
    if n.text.trim().is_empty() || n.text.len() > VALUE_MAX {
        bail!("notice text must be 1..{VALUE_MAX} bytes");
    }
    if n.key.is_empty() || n.key.len() > 200 {
        bail!("notice key must be 1..200 bytes");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, &n.bot)?;
    let key = format!("service:{}", n.key);
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM bot_wakes WHERE bot_id=?1 AND key=?2",
            params![b.id, key],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(Notified {
            wake: Some(id),
            owner_text: None,
        });
    }
    let actor = Actor::new("dispatcher");
    let now = bot::ts(crate::remote::epoch());
    let wake = bot::insert_wake(&tx, b.id, "wake", &key, None, &n.text, &now, &now, &actor)?;
    bot::event(
        &tx,
        b.id,
        &actor,
        "service",
        &json!({"action":n.kind,"services":n.services,"wake":wake,"text":n.text}).to_string(),
    )?;
    tx.commit()?;
    let owner_text = (n.kind == "escalate").then(|| {
        format!(
            "Bot {}: service {} keeps crashing and the bot has not fixed it. {}",
            n.bot,
            n.services.join(", "),
            n.text
        )
    });
    Ok(Notified {
        wake: Some(wake),
        owner_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(enabled: bool, status: Status) -> Service {
        Service {
            bot: "b".into(),
            home: "b".into(),
            name: "feed".into(),
            spec: NewService {
                command: "run".into(),
                cwd: None,
                env: BTreeMap::new(),
                restart: "on-failure".into(),
                schedule: None,
            },
            enabled,
            status,
            created_at: bot::ts(0),
            updated_at: bot::ts(0),
        }
    }

    const NOW: i64 = 1_000_000;

    fn st(state: &str) -> Status {
        Status {
            state: state.into(),
            reported_at: Some(bot::ts(NOW - 2)),
            dispatcher: Some("vm".into()),
            ..Status::default()
        }
    }

    #[test]
    fn summary_reads_kind_state_uptime_and_restarts() {
        let mut s = st("running");
        s.started_at = Some(bot::ts(NOW - 7300));
        s.restarts = 1;
        s.last_exit = Some("exit 1".into());
        assert_eq!(
            svc(true, s).summary(NOW),
            "feed (long-running): running, up 2h, 1 restart, last exit 1"
        );
        assert_eq!(
            svc(true, Status::default()).summary(NOW),
            "feed (long-running): pending, 0 restarts"
        );
        assert_eq!(
            svc(false, Status::default()).summary(NOW),
            "feed (long-running): stopped, 0 restarts"
        );
    }

    #[test]
    fn summary_flags_crash_loops_turned_off_services_and_stale_reports() {
        let mut s = st("backoff");
        s.crash_loop_since = Some(bot::ts(NOW - 60));
        s.next_start_at = Some(bot::ts(NOW + 30));
        s.restarts = 5;
        s.last_exit = Some("exit 2".into());
        let looping = svc(true, s);
        assert_eq!(looping.display_state(), "crash-looping");
        assert_eq!(
            looping.summary(NOW),
            "feed (long-running): crash-looping, restart in 30s, 5 restarts, last exit 2"
        );
        let mut s = st("stopping");
        s.crash_loop_since = Some(bot::ts(NOW - 60));
        assert_eq!(
            svc(false, s).summary(NOW),
            "feed (long-running): stopping, turned off, 0 restarts"
        );
        let mut s = st("running");
        s.started_at = Some(bot::ts(NOW - 4000));
        s.reported_at = Some(bot::ts(NOW - 600));
        assert_eq!(
            svc(true, s).summary(NOW),
            "feed (long-running): running, up 1h, 0 restarts, no report from vm for 10m"
        );
    }

    fn sched(cron: &str) -> Option<Schedule> {
        Some(Schedule {
            cron: cron.into(),
            timezone: "Asia/Taipei".into(),
            timeout_seconds: 60,
            catch_up: true,
        })
    }

    #[test]
    fn a_scheduled_service_reads_its_last_run_exit_and_next_run() {
        let mut s = st("scheduled");
        s.started_at = Some(bot::ts(NOW - 180));
        s.finished_at = Some(bot::ts(NOW - 170));
        s.last_exit = Some("exit 0".into());
        s.next_run_at = Some(bot::ts(NOW + 120));
        let mut v = svc(true, s);
        v.spec.schedule = sched("*/5 * * * *");
        v.spec.restart = "never".into();
        assert_eq!(v.kind(), "scheduled");
        assert_eq!(
            v.summary(NOW),
            "feed (scheduled */5 * * * * Asia/Taipei): scheduled, last run 3m ago: exit 0, next run in 2m"
        );
        v.status.state = "running".into();
        v.status.started_at = Some(bot::ts(NOW - 5));
        assert!(v
            .summary(NOW)
            .contains(": running, running 5s, next run in 2m"));
        v.enabled = false;
        v.status.state = "stopped".into();
        assert_eq!(
            v.summary(NOW),
            "feed (scheduled */5 * * * * Asia/Taipei): stopped, last run 5s ago: exit 0"
        );
    }

    #[test]
    fn a_schedule_is_validated_and_needs_restart_never() {
        let mut n = NewService {
            command: "true".into(),
            cwd: None,
            env: BTreeMap::new(),
            restart: "never".into(),
            schedule: sched("0 9 * * 1-5"),
        };
        n.validate().unwrap();
        n.restart = "always".into();
        assert!(n
            .validate()
            .unwrap_err()
            .to_string()
            .contains("restart never"));
        n.restart = "never".into();
        n.schedule = sched("@daily");
        assert!(n.validate().is_err());
        n.schedule = sched("0 9 * * *");
        n.schedule.as_mut().unwrap().timezone = "Mars/Base".into();
        assert!(n.validate().is_err());
        n.schedule = sched("0 9 * * *");
        n.schedule.as_mut().unwrap().timeout_seconds = 0;
        assert!(n.validate().is_err());
        // Defaults: UTC, an hour, catch-up on.
        let d: Schedule = serde_json::from_value(json!({"cron":"* * * * *"})).unwrap();
        assert_eq!(
            (d.timezone.as_str(), d.timeout_seconds, d.catch_up),
            ("UTC", TIMEOUT_DEFAULT, true)
        );
    }

    #[test]
    fn a_scheduled_run_that_ended_is_a_run_finished_event_and_a_failure_a_failed_one() {
        let mut old = st("scheduled");
        old.started_at = Some(bot::ts(NOW - 60));
        old.finished_at = Some(bot::ts(NOW - 59));
        let mut new = old.clone();
        assert!(events(&old, &new).is_empty(), "the same run");
        new.started_at = Some(bot::ts(NOW - 2));
        new.finished_at = Some(bot::ts(NOW - 1));
        new.last_exit = Some("timeout after 60s (signal 15)".into());
        new.failures = 1;
        let e = events(&old, &new);
        assert_eq!(
            e,
            [
                ("failed", "timeout after 60s (signal 15)".to_string()),
                ("run_finished", "timeout after 60s (signal 15)".to_string())
            ]
        );
    }
}
