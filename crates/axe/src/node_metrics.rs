//! Node observability: host load and running runs, sampled by the node and
//! kept by the server (`[node_metrics]`, [`crate::config::NodeMetrics`]).
//!
//! Node side: every `sample_seconds` the node builds a [`Sample`] ([`Sampler`])
//! and sends it in its admission report (`POST /api/dispatcher/admission`,
//! field `sample`), so no new channel is needed. An older server ignores the
//! field. Signals (Linux, see proc(5) and statvfs(3)):
//! - CPU: busy share of the aggregate `/proc/stat` `cpu` line since the
//!   previous sample ([`crate::admission::cpu_busy_percent`]); the first sample
//!   has none;
//! - memory: used is `MemTotal - MemAvailable`;
//! - load: the 1, 5 and 15 minute averages of `/proc/loadavg`;
//! - disk: free (`f_bavail`) and total of the filesystem holding the run
//!   workspaces (`Config::run_root`, or its nearest existing parent).
//!
//! Each sample also names the node binary's version (`axe --version` of the
//! running node) and the version of the `axe` CLI on the node host, which
//! can differ until the node restarts (`$PATH`, else `~/.local/bin/axe`; read
//! every [`CLI_VERSION_EVERY`]). The server adds its own ([`Series::server_version`]).
//!
//! Running runs ([`Runs`]) follow the node's own pools: task runs (agent
//! sessions and routine pre-checks) per project, bot runs per bot, and the
//! bot services with a live process per owning bot, long-running services
//! apart from scheduled (cron) service runs. A run waiting for resource
//! admission has no process and is not counted.
//!
//! Server side: [`record`] stores each sample in `node_samples` (the server's
//! receive time, so node clock skew cannot reorder history) and recomputes its
//! `rollup_seconds` bucket in `node_rollups` (avg/max) from the raw rows, so a
//! rollup is current as soon as its samples arrive. [`prune`] drops raw rows
//! past `raw_retention_hours` and rollups past `rollup_retention_days`; the
//! server's housekeeping loop runs it every `prune_seconds`. [`query`] serves
//! the web Node view: short ranges read raw samples, long ones rollups
//! ([`RANGES`]).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::admission::{self, CpuTimes, Memory};
use crate::config::NodeMetrics as Cfg;

/// Schema v24: additive tables only.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS node_samples (
 at INTEGER PRIMARY KEY, node TEXT NOT NULL,
 cpu REAL, mem_used INTEGER, mem_total INTEGER,
 load1 REAL, load5 REAL, load15 REAL, disk_free INTEGER, disk_total INTEGER,
 runs_task INTEGER NOT NULL, runs_bot INTEGER NOT NULL,
 runs_service INTEGER NOT NULL, runs_scheduled INTEGER NOT NULL,
 sample TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS node_rollups (
 bucket INTEGER PRIMARY KEY, n INTEGER NOT NULL,
 cpu_avg REAL, cpu_max REAL, mem_used_avg REAL, mem_used_max REAL, mem_total REAL,
 load1_avg REAL, load1_max REAL, disk_free_min REAL,
 task_avg REAL NOT NULL, task_max REAL NOT NULL, bot_avg REAL NOT NULL, bot_max REAL NOT NULL,
 service_avg REAL NOT NULL, service_max REAL NOT NULL,
 scheduled_avg REAL NOT NULL, scheduled_max REAL NOT NULL
);
";

/// Most names one run kind may list in a sample; the rest fold into `other`.
const MAX_NAMES: usize = 64;
const MAX_NAME_LEN: usize = 64;

/// Running runs by kind, each by name (project or bot). See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "NodeRuns"))]
pub struct Runs {
    /// Task runs (agent sessions and routine pre-checks) per project.
    #[serde(default)]
    pub task: BTreeMap<String, u32>,
    /// Bot runs per bot.
    #[serde(default)]
    pub bot: BTreeMap<String, u32>,
    /// Long-running services with a live process, per owning bot.
    #[serde(default)]
    pub service: BTreeMap<String, u32>,
    /// Scheduled (cron) service runs in progress, per owning bot.
    #[serde(default)]
    pub scheduled: BTreeMap<String, u32>,
}

impl Runs {
    /// Task, bot, service and scheduled totals.
    pub fn totals(&self) -> [u32; 4] {
        let sum = |m: &BTreeMap<String, u32>| m.values().sum::<u32>();
        [
            sum(&self.task),
            sum(&self.bot),
            sum(&self.service),
            sum(&self.scheduled),
        ]
    }

    /// Bounds a sample from the network: at most [`MAX_NAMES`] names of at
    /// most [`MAX_NAME_LEN`] bytes per kind, the rest summed as `other`.
    fn bounded(mut self) -> Self {
        for m in [
            &mut self.task,
            &mut self.bot,
            &mut self.service,
            &mut self.scheduled,
        ] {
            m.retain(|_, n| *n > 0);
            let long: Vec<String> = m
                .keys()
                .filter(|k| k.is_empty() || k.len() > MAX_NAME_LEN)
                .cloned()
                .collect();
            let mut other: u32 = long.iter().filter_map(|k| m.remove(k)).sum();
            while m.len() > MAX_NAMES {
                let (_, n) = m.pop_last().expect("non-empty");
                other = other.saturating_add(n);
            }
            if other > 0 {
                *m.entry("other".into()).or_default() += other;
            }
        }
        self
    }
}

/// One node sample. `at` is set by the server when it stores the sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "NodeSample"))]
pub struct Sample {
    #[serde(default)]
    pub at: i64,
    /// Busy CPU percent since the previous sample; null on the first.
    pub cpu_percent: Option<f64>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    /// Free and total bytes of the run workspaces' filesystem (`disk_path`).
    pub disk_free_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    #[serde(default)]
    pub disk_path: String,
    #[serde(default)]
    pub runs: Runs,
    /// The node binary's `axe --version` string, e.g. `0.5.4 (5437bf3602b8)`.
    #[serde(default)]
    pub node_version: Option<String>,
    /// `axe --version` of the CLI on the node host; null when not found.
    #[serde(default)]
    pub cli_version: Option<String>,
}

/// Raw host readings for one sample; `None` where the read failed.
#[derive(Debug, Clone, Default)]
pub struct Readings {
    pub cpu: Option<CpuTimes>,
    pub memory: Option<Memory>,
    pub load: Option<[f64; 3]>,
    /// Free and total bytes.
    pub disk: Option<(u64, u64)>,
}

/// How often the node re-reads the host CLI's version.
pub const CLI_VERSION_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// Builds samples on the node; keeps the previous CPU counters and the last
/// CLI version read.
#[derive(Debug, Clone)]
pub struct Sampler {
    disk_path: PathBuf,
    prev_cpu: Option<CpuTimes>,
    cli: Option<(std::time::Instant, Option<String>)>,
}

impl Sampler {
    pub fn new(disk_path: PathBuf) -> Self {
        Sampler {
            disk_path,
            prev_cpu: None,
            cli: None,
        }
    }

    pub fn disk_path(&self) -> &Path {
        &self.disk_path
    }

    /// Reads the host and builds a sample.
    pub fn sample(&mut self, runs: Runs) -> Sample {
        let r = read_host(&self.disk_path);
        if self
            .cli
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= CLI_VERSION_EVERY)
        {
            self.cli = Some((std::time::Instant::now(), cli_version()));
        }
        let mut s = self.build(r, runs);
        s.cli_version = self.cli.as_ref().and_then(|c| c.1.clone());
        s
    }

    /// A sample from `r`: CPU is the busy share since the previous readings.
    pub fn build(&mut self, r: Readings, runs: Runs) -> Sample {
        let cpu_percent = match (self.prev_cpu, r.cpu) {
            (Some(from), Some(to)) => admission::cpu_busy_percent(&from, &to),
            _ => None,
        };
        if r.cpu.is_some() {
            self.prev_cpu = r.cpu;
        }
        let kib = |v: u64| v.saturating_mul(1024);
        Sample {
            at: crate::remote::epoch(),
            cpu_percent,
            mem_used_bytes: r
                .memory
                .map(|m| kib(m.total_kib.saturating_sub(m.available_kib))),
            mem_total_bytes: r.memory.map(|m| kib(m.total_kib)),
            load1: r.load.map(|l| l[0]),
            load5: r.load.map(|l| l[1]),
            load15: r.load.map(|l| l[2]),
            disk_free_bytes: r.disk.map(|d| d.0),
            disk_total_bytes: r.disk.map(|d| d.1),
            disk_path: self.disk_path.display().to_string(),
            runs,
            node_version: Some(env!("AXE_VERSION").to_string()),
            cli_version: None,
        }
    }
}

/// The version an `axe --version` line names: `axe 0.5.4 (5437bf3602b8)`
/// gives `0.5.4 (5437bf3602b8)`.
pub fn parse_cli_version(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    let v = line.strip_prefix("axe ").unwrap_or(line).trim();
    (!v.is_empty() && v.len() <= 80).then(|| v.to_string())
}

/// `axe --version` of the CLI on this host: the first `axe` on `$PATH`, else
/// `~/.local/bin/axe` (where the installer puts it). Its update check and
/// skill refresh are off for this call.
pub fn cli_version() -> Option<String> {
    let run = |bin: &std::ffi::OsStr| {
        std::process::Command::new(bin)
            .arg("--version")
            .env("AXE_UPDATE_CHECK", "0")
            .env("AXE_SKILL_REFRESH", "0")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_cli_version(&String::from_utf8_lossy(&o.stdout)))
    };
    run("axe".as_ref()).or_else(|| {
        let local = dirs::home_dir()?.join(".local/bin/axe");
        run(local.as_os_str())
    })
}

/// Reads `/proc` and `statvfs`; a failed read is `None`.
pub fn read_host(disk_path: &Path) -> Readings {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    Readings {
        cpu: read("/proc/stat").and_then(|t| admission::parse_proc_stat(&t).ok()),
        memory: read("/proc/meminfo").and_then(|t| admission::parse_meminfo(&t).ok()),
        load: read("/proc/loadavg").and_then(|t| parse_loadavg(&t).ok()),
        disk: disk_stats(disk_path).ok(),
    }
}

/// The three load averages of `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Result<[f64; 3]> {
    let mut it = text.split_whitespace();
    let mut out = [0.0; 3];
    for v in &mut out {
        *v = it
            .next()
            .context("/proc/loadavg is short")?
            .parse()
            .context("load average is not a number")?;
    }
    Ok(out)
}

/// Free (`f_bavail`) and total bytes of the filesystem holding `path`, or of
/// its nearest existing parent (the run root may not exist yet).
pub fn disk_stats(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let at = path
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .with_context(|| format!("no existing parent of {}", path.display()))?;
    let c = std::ffi::CString::new(at.as_os_str().as_bytes())
        .with_context(|| format!("path {}", at.display()))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("statvfs {}", at.display()));
    }
    let frsize = st.f_frsize as u64;
    Ok((
        (st.f_bavail as u64).saturating_mul(frsize),
        (st.f_blocks as u64).saturating_mul(frsize),
    ))
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|v| v.is_finite() && *v >= 0.0)
}

/// Stores `s` from node `node` at server time `at` and recomputes its
/// rollup bucket.
pub fn record(conn: &Connection, node: &str, at: i64, s: Sample, cfg: &Cfg) -> Result<()> {
    let mut s = Sample {
        at,
        cpu_percent: finite(s.cpu_percent).map(|v| v.min(100.0)),
        load1: finite(s.load1),
        load5: finite(s.load5),
        load15: finite(s.load15),
        runs: s.runs.bounded(),
        ..s
    };
    for v in [&mut s.node_version, &mut s.cli_version] {
        if v.as_ref().is_some_and(|v| v.len() > 80) {
            *v = None;
        }
    }
    s.disk_path.truncate(
        (0..=s.disk_path.len().min(512))
            .rev()
            .find(|i| s.disk_path.is_char_boundary(*i))
            .unwrap_or(0),
    );
    let [task, bot, service, scheduled] = s.runs.totals();
    let i = |v: Option<u64>| v.map(|v| v.min(i64::MAX as u64) as i64);
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO node_samples (at, node, cpu, mem_used, mem_total, load1, load5,
         load15, disk_free, disk_total, runs_task, runs_bot, runs_service, runs_scheduled, sample)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            at,
            node,
            s.cpu_percent,
            i(s.mem_used_bytes),
            i(s.mem_total_bytes),
            s.load1,
            s.load5,
            s.load15,
            i(s.disk_free_bytes),
            i(s.disk_total_bytes),
            task,
            bot,
            service,
            scheduled,
            serde_json::to_string(&s)?,
        ],
    )?;
    let width = cfg.rollup_seconds.max(1) as i64;
    rollup(&tx, at.div_euclid(width) * width, width)?;
    tx.commit()?;
    Ok(())
}

/// Recomputes the rollup of bucket `[start, start + width)` from raw samples;
/// a bucket with no raw samples keeps its stored rollup.
pub fn rollup(conn: &Connection, start: i64, width: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO node_rollups
         SELECT ?1, count(*), avg(cpu), max(cpu), avg(mem_used), max(mem_used), max(mem_total),
          avg(load1), max(load1), min(disk_free),
          avg(runs_task), max(runs_task), avg(runs_bot), max(runs_bot),
          avg(runs_service), max(runs_service), avg(runs_scheduled), max(runs_scheduled)
         FROM node_samples WHERE at >= ?1 AND at < ?1 + ?2 HAVING count(*) > 0",
        params![start, width],
    )?;
    Ok(())
}

/// Drops raw samples and rollups past their retention at `now`; returns the
/// rows dropped from each.
pub fn prune(conn: &Connection, now: i64, cfg: &Cfg) -> Result<(usize, usize)> {
    let raw = conn.execute(
        "DELETE FROM node_samples WHERE at < ?1",
        [now - cfg.raw_retention_hours as i64 * 3600],
    )?;
    let rolled = conn.execute(
        "DELETE FROM node_rollups WHERE bucket < ?1",
        [now - cfg.rollup_retention_days as i64 * 86400],
    )?;
    Ok((raw, rolled))
}

/// The newest stored sample.
pub fn latest(conn: &Connection) -> Result<Option<Sample>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT sample FROM node_samples ORDER BY at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    row.map(|s| serde_json::from_str(&s).context("stored node sample"))
        .transpose()
}

/// The Node view's ranges: name, span, source and the step its points are
/// grouped to (`None`: the sample interval, raw; otherwise at least the
/// rollup width). 1h and 6h read raw samples; 24h and longer read rollups,
/// since 24h of raw samples is several times the points a chart can show.
pub const RANGES: [(&str, i64, Source, i64); 6] = [
    ("1h", 3600, Source::Raw, 0),
    ("6h", 6 * 3600, Source::Raw, 60),
    ("24h", 86400, Source::Rollup, 0),
    ("7d", 7 * 86400, Source::Rollup, 1800),
    ("14d", 14 * 86400, Source::Rollup, 3600),
    ("30d", 30 * 86400, Source::Rollup, 7200),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Raw,
    Rollup,
}

/// One chart point: the samples in `[t, t + step)`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "NodePoint"))]
pub struct Point {
    pub t: i64,
    /// Raw samples behind the point.
    pub n: i64,
    pub cpu_avg: Option<f64>,
    pub cpu_max: Option<f64>,
    pub mem_used_avg: Option<f64>,
    pub mem_used_max: Option<f64>,
    pub mem_total: Option<f64>,
    pub load1_avg: Option<f64>,
    pub load1_max: Option<f64>,
    pub disk_free_min: Option<f64>,
    pub task_avg: f64,
    pub task_max: f64,
    pub bot_avg: f64,
    pub bot_max: f64,
    pub service_avg: f64,
    pub service_max: f64,
    pub scheduled_avg: f64,
    pub scheduled_max: f64,
}

/// `GET /api/node/metrics`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(rename = "NodeSeries"))]
pub struct Series {
    pub range: String,
    /// `raw` or `rollup`.
    pub source: String,
    pub step_seconds: i64,
    /// Points cover `[from, to)`, server time.
    pub from: i64,
    pub to: i64,
    pub now: i64,
    pub sample_seconds: u64,
    pub points: Vec<Point>,
    /// The newest sample, whatever the range.
    pub current: Option<Sample>,
    /// This server's `axe --version` string.
    pub server_version: String,
}

/// The points of `range` (one of [`RANGES`]) ending at `now`.
pub fn query(conn: &Connection, range: &str, now: i64, cfg: &Cfg) -> Result<Series> {
    let Some(&(name, span, source, step)) = RANGES.iter().find(|r| r.0 == range) else {
        let names: Vec<&str> = RANGES.iter().map(|r| r.0).collect();
        bail!("range must be one of {}", names.join(", "));
    };
    let width = cfg.rollup_seconds.max(1) as i64;
    let step = match source {
        Source::Raw if step == 0 => cfg.sample_seconds.max(1) as i64,
        Source::Raw => step,
        // A whole number of rollup buckets.
        Source::Rollup => (step.max(width) + width - 1) / width * width,
    };
    let to = now.div_euclid(step) * step + step;
    let from = to - span - step;
    let sql = match source {
        Source::Raw => {
            "SELECT (at / ?3) * ?3 AS b, count(*), avg(cpu), max(cpu), avg(mem_used), max(mem_used),
              max(mem_total), avg(load1), max(load1), min(disk_free),
              avg(runs_task), max(runs_task), avg(runs_bot), max(runs_bot),
              avg(runs_service), max(runs_service), avg(runs_scheduled), max(runs_scheduled)
             FROM node_samples WHERE at >= ?1 AND at < ?2 GROUP BY b ORDER BY b"
        }
        // Averages weighted by each bucket's sample count.
        Source::Rollup => {
            "SELECT (bucket / ?3) * ?3 AS b, sum(n),
              sum(cpu_avg * n) / sum(CASE WHEN cpu_avg IS NOT NULL THEN n END), max(cpu_max),
              sum(mem_used_avg * n) / sum(CASE WHEN mem_used_avg IS NOT NULL THEN n END),
              max(mem_used_max), max(mem_total),
              sum(load1_avg * n) / sum(CASE WHEN load1_avg IS NOT NULL THEN n END), max(load1_max),
              min(disk_free_min),
              sum(task_avg * n) / sum(n), max(task_max), sum(bot_avg * n) / sum(n), max(bot_max),
              sum(service_avg * n) / sum(n), max(service_max),
              sum(scheduled_avg * n) / sum(n), max(scheduled_max)
             FROM node_rollups WHERE bucket >= ?1 AND bucket < ?2 GROUP BY b ORDER BY b"
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let points = stmt
        .query_map(params![from, to, step], |r| {
            Ok(Point {
                t: r.get(0)?,
                n: r.get(1)?,
                cpu_avg: r.get(2)?,
                cpu_max: r.get(3)?,
                mem_used_avg: r.get(4)?,
                mem_used_max: r.get(5)?,
                mem_total: r.get(6)?,
                load1_avg: r.get(7)?,
                load1_max: r.get(8)?,
                disk_free_min: r.get(9)?,
                task_avg: r.get(10)?,
                task_max: r.get(11)?,
                bot_avg: r.get(12)?,
                bot_max: r.get(13)?,
                service_avg: r.get(14)?,
                service_max: r.get(15)?,
                scheduled_avg: r.get(16)?,
                scheduled_max: r.get(17)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Series {
        range: name.into(),
        source: match source {
            Source::Raw => "raw",
            Source::Rollup => "rollup",
        }
        .into(),
        step_seconds: step,
        from,
        to,
        now,
        sample_seconds: cfg.sample_seconds,
        points,
        current: latest(conn)?,
        server_version: crate::server::version().version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Cfg {
        Cfg::default()
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c
    }

    fn runs(task: &[(&str, u32)], bot: &[(&str, u32)]) -> Runs {
        Runs {
            task: task.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            bot: bot.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Runs::default()
        }
    }

    fn cpu(busy: u64, idle: u64) -> CpuTimes {
        CpuTimes {
            fields: [busy, 0, 0, idle, 0, 0, 0, 0],
        }
    }

    #[test]
    fn cli_version_is_the_version_line() {
        assert_eq!(
            parse_cli_version("axe 0.5.4 (5437bf3602b8)\n").as_deref(),
            Some("0.5.4 (5437bf3602b8)")
        );
        assert_eq!(parse_cli_version("").as_deref(), None);
        assert_eq!(parse_cli_version(&"x".repeat(100)), None);
    }

    #[test]
    fn samples_name_the_node_binary() {
        let s = Sampler::new("/".into()).build(Readings::default(), Runs::default());
        assert_eq!(s.node_version.as_deref(), Some(env!("AXE_VERSION")));
    }

    #[test]
    fn loadavg_parses_three_averages() {
        assert_eq!(
            parse_loadavg("0.52 1.25 2.00 3/812 12345\n").unwrap(),
            [0.52, 1.25, 2.0]
        );
        assert!(parse_loadavg("0.5 1.0").is_err());
        assert!(parse_loadavg("x 1 2").is_err());
    }

    #[test]
    fn sampler_reports_cpu_since_the_previous_sample() {
        let mut s = Sampler::new("/".into());
        let mem = Memory {
            available_kib: 3 * 1024 * 1024,
            total_kib: 4 * 1024 * 1024,
        };
        let first = s.build(
            Readings {
                cpu: Some(cpu(100, 100)),
                memory: Some(mem),
                load: Some([1.0, 2.0, 3.0]),
                disk: Some((10, 20)),
            },
            runs(&[("harvis", 2)], &[]),
        );
        assert_eq!(first.cpu_percent, None, "no window on the first sample");
        assert_eq!(
            first.mem_used_bytes,
            Some(1024 * 1024 * 1024),
            "KiB to bytes"
        );
        assert_eq!(first.mem_total_bytes, Some(4 * 1024 * 1024 * 1024));
        assert_eq!((first.load1, first.load15), (Some(1.0), Some(3.0)));
        assert_eq!(
            (first.disk_free_bytes, first.disk_total_bytes),
            (Some(10), Some(20))
        );
        assert_eq!(first.runs.totals(), [2, 0, 0, 0]);
        // 30 busy of 40 ticks since.
        let second = s.build(
            Readings {
                cpu: Some(cpu(130, 110)),
                ..Readings::default()
            },
            Runs::default(),
        );
        assert_eq!(second.cpu_percent, Some(75.0));
        assert_eq!(second.mem_used_bytes, None, "a failed read is unknown");
        // A failed CPU read keeps the last counters for the next window.
        let third = s.build(Readings::default(), Runs::default());
        assert_eq!(third.cpu_percent, None);
        let fourth = s.build(
            Readings {
                cpu: Some(cpu(140, 120)),
                ..Readings::default()
            },
            Runs::default(),
        );
        assert_eq!(fourth.cpu_percent, Some(50.0));
    }

    #[test]
    fn disk_stats_walks_up_to_an_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let (free, total) = disk_stats(&dir.path().join("not/yet/made")).unwrap();
        assert!(total > 0 && free <= total);
    }

    #[test]
    fn runs_from_the_network_are_bounded() {
        let mut r = Runs::default();
        for i in 0..70 {
            r.bot.insert(format!("b{i:02}"), 1);
        }
        r.task.insert("x".repeat(100), 2);
        r.task.insert("zero".into(), 0);
        let b = r.bounded();
        assert_eq!(b.bot.len(), MAX_NAMES + 1);
        assert_eq!(b.bot["other"], 6);
        assert_eq!(b.task, BTreeMap::from([("other".to_string(), 2)]));
        assert_eq!(b.totals(), [2, 70, 0, 0]);
    }

    fn sample(cpu: f64, task: u32) -> Sample {
        Sample {
            cpu_percent: Some(cpu),
            mem_used_bytes: Some(100),
            mem_total_bytes: Some(1000),
            load1: Some(cpu / 100.0),
            disk_free_bytes: Some(500),
            runs: runs(&[("p", task)], &[("axe-os", 1)]),
            ..Sample::default()
        }
    }

    #[test]
    fn record_keeps_raw_and_a_current_rollup() {
        let c = db();
        let t0 = 1_800_000_000;
        record(&c, "n1", t0, sample(10.0, 1), &cfg()).unwrap();
        record(&c, "n1", t0 + 15, sample(30.0, 3), &cfg()).unwrap();
        // The next bucket.
        record(&c, "n1", t0 + 300, sample(50.0, 0), &cfg()).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM node_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        let row: (i64, f64, f64, f64, f64, f64) = c
            .query_row(
                "SELECT n, cpu_avg, cpu_max, task_avg, task_max, bot_max FROM node_rollups WHERE bucket=?1",
                [t0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(row, (2, 20.0, 30.0, 2.0, 3.0, 1.0));
        let next: i64 = c
            .query_row(
                "SELECT n FROM node_rollups WHERE bucket=?1",
                [t0 + 300],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, 1);
        let last = latest(&c).unwrap().unwrap();
        assert_eq!((last.at, last.cpu_percent), (t0 + 300, Some(50.0)));
    }

    #[test]
    fn record_rejects_impossible_readings() {
        let c = db();
        let mut s = sample(250.0, 1);
        s.load1 = Some(f64::NAN);
        s.load5 = Some(-1.0);
        record(&c, "n1", 1000, s, &cfg()).unwrap();
        let last = latest(&c).unwrap().unwrap();
        assert_eq!(last.cpu_percent, Some(100.0));
        assert_eq!((last.load1, last.load5), (None, None));
    }

    #[test]
    fn prune_drops_rows_past_retention() {
        let c = db();
        let cfg = cfg();
        let now = 1_800_000_000;
        let raw_keep = cfg.raw_retention_hours as i64 * 3600;
        let roll_keep = cfg.rollup_retention_days as i64 * 86400;
        for at in [
            now - roll_keep - 600,
            now - raw_keep - 60,
            now - raw_keep + 60,
            now,
        ] {
            record(&c, "n1", at, sample(1.0, 1), &cfg).unwrap();
        }
        let (raw, rolled) = prune(&c, now, &cfg).unwrap();
        assert_eq!((raw, rolled), (2, 1));
        let left: Vec<i64> = c
            .prepare("SELECT at FROM node_samples ORDER BY at")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(left, vec![now - raw_keep + 60, now]);
        // The rollup of a pruned raw sample outlives it.
        let buckets: i64 = c
            .query_row("SELECT count(*) FROM node_rollups", [], |r| r.get(0))
            .unwrap();
        assert_eq!(buckets, 3);
        assert_eq!(prune(&c, now, &cfg).unwrap(), (0, 0));
    }

    #[test]
    fn rollup_of_a_pruned_bucket_is_kept() {
        let c = db();
        record(&c, "n1", 600, sample(40.0, 2), &cfg()).unwrap();
        c.execute("DELETE FROM node_samples", []).unwrap();
        rollup(&c, 600, 300).unwrap();
        let n: i64 = c
            .query_row("SELECT n FROM node_rollups WHERE bucket=600", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn query_picks_raw_or_rollups_by_range() {
        let c = db();
        let cfg = cfg();
        let now = 1_800_000_000 + 7000;
        // Two days of samples every 15 s.
        let mut at = now - 2 * 86400;
        let mut last = at;
        while at <= now {
            last = at;
            let busy = if at >= now - 3600 { 80.0 } else { 20.0 };
            record(&c, "n1", at, sample(busy, 1), &cfg).unwrap();
            at += 15;
        }
        let h = query(&c, "1h", now, &cfg).unwrap();
        assert_eq!((h.source.as_str(), h.step_seconds), ("raw", 15));
        assert!((240..=242).contains(&h.points.len()), "{}", h.points.len());
        assert!(h.points.iter().all(|p| p.t >= h.from && p.t < h.to));
        assert_eq!(h.points.last().unwrap().cpu_avg, Some(80.0));
        let six = query(&c, "6h", now, &cfg).unwrap();
        assert_eq!((six.source.as_str(), six.step_seconds), ("raw", 60));
        assert_eq!(six.points[1].n, 4);
        let day = query(&c, "24h", now, &cfg).unwrap();
        assert_eq!((day.source.as_str(), day.step_seconds), ("rollup", 300));
        assert!((288..=290).contains(&day.points.len()));
        assert_eq!(day.points[1].n, 20);
        let week = query(&c, "7d", now, &cfg).unwrap();
        assert_eq!((week.source.as_str(), week.step_seconds), ("rollup", 1800));
        // A full 30-minute point: 120 samples, weighted mean of its buckets.
        let full = &week.points[week.points.len() - 5];
        assert_eq!(full.n, 120);
        assert_eq!(full.cpu_avg, Some(20.0));
        assert_eq!(full.task_max, 1.0);
        let fortnight = query(&c, "14d", now, &cfg).unwrap();
        assert_eq!(
            (fortnight.source.as_str(), fortnight.step_seconds),
            ("rollup", 3600)
        );
        assert_eq!(fortnight.points.last().unwrap().cpu_max, Some(80.0));
        // The last hour is busier: its max shows in the 2 h point.
        let month = query(&c, "30d", now, &cfg).unwrap();
        assert_eq!(month.step_seconds, 7200);
        assert_eq!(month.points.last().unwrap().cpu_max, Some(80.0));
        assert_eq!(month.current.unwrap().at, last);
        assert!(query(&c, "2h", now, &cfg).is_err());
    }
}
