//! Services: long-running processes a bot declares and the dispatcher runs on
//! its machine (`dispatch/services.rs`), the way routines are crontab and
//! services are systemd.
//!
//! The server holds each declaration (name, command, cwd, env, restart
//! policy, enabled) and the state the dispatcher last reported for it. The
//! dispatcher syncs both ways ([`report`]) and raises the events a bot must
//! hear about ([`notify`]): a crash loop and an upcoming stop wake the owning
//! bot; a crash loop that persists alerts the owner as well. The change
//! between two reports is also a set of service events ([`events`]) for the
//! bots subscribed to them (`subscription.rs`); a new service subscribes its
//! owning bot to its failures.
use std::collections::BTreeMap;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bot;
use crate::db::{self, Actor};

/// v22: additive, so a database lowered to `user_version` 21 still opens with
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

/// Restart policies: after any exit, after a failure (non-zero exit or a
/// signal), or never.
pub const RESTARTS: [&str; 3] = ["always", "on-failure", "never"];
/// Most bytes in a command, a cwd or one env value.
pub const VALUE_MAX: usize = 4096;
/// Most env entries of one service.
pub const ENV_MAX: usize = 64;
/// Most services one bot declares.
pub const PER_BOT_MAX: usize = 16;

/// What a declaration sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewService {
    /// A shell command (`sh -c`).
    pub command: String,
    /// Where it runs: absolute, or relative to the bot's run folder (the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default = "on_failure")]
    pub restart: String,
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
                bail!("env name {k:?}: AXE_ variables are set by the dispatcher");
            }
            if v.len() > VALUE_MAX || v.contains('\0') {
                bail!("env {k}: the value must be at most {VALUE_MAX} bytes without NUL");
            }
        }
        Ok(())
    }
}

/// What the dispatcher reports for one service; stored as `services.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// `starting`, `running`, `backoff` (waiting to restart), `stopping`,
    /// `stopped` or `exited` (done, not restarted by its policy).
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// When the current (or last) process started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Restarts since the dispatcher started the service.
    #[serde(default)]
    pub restarts: u64,
    /// Failures since the dispatcher started the service: unsuccessful exits
    /// it did not ask for, and starts that failed.
    #[serde(default)]
    pub failures: u64,
    /// How the last process ended: `exit N` or `signal N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<String>,
    /// When the next restart is due, in `backoff`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_start_at: Option<String>,
    /// Since when it has been crash looping, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_loop_since: Option<String>,
    /// Why it could not start, if it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The log file on the dispatcher's machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// The dispatcher that reported it, and when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<String>,
}

/// One declared service with its last reported status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub bot: String,
    /// The bot's folder key (`Bot::home`): where its run folder and logs are.
    pub home: String,
    pub name: String,
    #[serde(flatten)]
    pub spec: NewService,
    pub enabled: bool,
    /// Empty (`state` "") until a dispatcher reported it.
    #[serde(default)]
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
}

impl Service {
    /// The key the dispatcher tracks it by: `<bot>/<name>`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.bot, self.name)
    }
}

/// `home` as [`bot::Bot::home`]: the name before the bot's first rename, else its name.
const SELECT: &str = "SELECT b.name, (SELECT json_extract(e.body,'$.from') FROM bot_events e
 WHERE e.bot_id=b.id AND e.kind='rename' ORDER BY e.id LIMIT 1), s.name, s.command, s.cwd, s.env, s.restart,
 s.enabled, s.status, s.created_at, s.updated_at FROM services s JOIN bots b ON b.id=s.bot_id";

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
    Ok(Service {
        home: home.unwrap_or_else(|| bot.clone()),
        bot,
        name: r.get(2)?,
        spec: NewService {
            command: r.get(3)?,
            cwd: r.get(4)?,
            env: serde_json::from_value(env).unwrap_or_default(),
            restart: r.get(6)?,
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
    new.validate()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let b = bot::get(&tx, bot)?;
    let now = db::now();
    let env = serde_json::to_string(&new.env)?;
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
        "INSERT INTO services (bot_id, name, command, cwd, env, restart, enabled, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,1,?7,?7)
         ON CONFLICT(bot_id, name) DO UPDATE SET command=excluded.command, cwd=excluded.cwd,
           env=excluded.env, restart=excluded.restart, updated_at=excluded.updated_at",
        params![b.id, name, new.command, new.cwd, env, new.restart, now],
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
            "env":new.env.keys().collect::<Vec<_>>(),"restart":new.restart})
        .to_string(),
    )?;
    let s = find(&tx, b.id, name)?.expect("service just written");
    tx.commit()?;
    Ok(s)
}

/// Turns a service on (`start`) or off (`stop`); the dispatcher follows.
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

/// Removes a service; the dispatcher stops it. Returns what it held.
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

/// The dispatcher `dispatcher` (generation `generation`, the registered one)
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
/// the dispatcher), `crash_loop`, `run_finished` (it ended and is not
/// restarted), `stopped` and `started` (a new process, seen by its start
/// time).
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
    if new.state == "exited" && (old.state != "exited" || new_run) {
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

/// An event the dispatcher raises for a service.
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
