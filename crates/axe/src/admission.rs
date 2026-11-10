//! Resource admission: the dispatcher starts no new agent session while guest
//! disk, memory or CPU is tight. Running sessions are never touched.
//!
//! Signals (Linux guest, see proc(5) and statvfs(3)):
//! - disk: `f_bavail * f_frsize` of the watched filesystem (bytes available to
//!   unprivileged users, as `df` reports "Avail");
//! - memory: `MemAvailable / MemTotal` from `/proc/meminfo`, in percent;
//! - cpu: busy share of the aggregate `cpu` line of `/proc/stat` between two
//!   samples at least `cpu_window_seconds` apart on a monotonic clock. Busy is
//!   every field except `idle` and `iowait`; `guest`/`guest_nice` are already in
//!   `user`/`nice` and are not added again. Fields that move backwards (iowait
//!   can) count as zero.
//!
//! Each signal is `clear`, `constrained` or `unknown`. A clear signal becomes
//! constrained strictly past its pause threshold (disk/memory below, cpu above);
//! a constrained one clears only strictly past its resume threshold. Values equal
//! to a threshold do not cross it. Unknown (startup, CPU window warming up or
//! gapped, read failure) counts as constrained, and a signal leaves unknown as
//! if it had been constrained, so it must reach its resume threshold. Admission
//! is open only while every signal is clear.
//!
//! Bot runs are not held by that pause. They stop only at a hard floor derived
//! from the same thresholds: disk or memory below half its pause threshold, or
//! unreadable. CPU never holds bots, and the floor has no hysteresis.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::Resources;

const GIB: f64 = (1u64 << 30) as f64;

/// Cumulative aggregate CPU time from `/proc/stat` (clock ticks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// user, nice, system, idle, iowait, irq, softirq, steal.
    pub fields: [u64; 8],
}

/// `MemAvailable` and `MemTotal` in KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub available_kib: u64,
    pub total_kib: u64,
}

/// Reads raw guest counters. Tests substitute controlled samples.
pub trait Probe: Send {
    fn disk_available(&mut self, path: &Path) -> Result<u64>;
    fn memory(&mut self) -> Result<Memory>;
    fn cpu(&mut self) -> Result<CpuTimes>;
}

/// Monotonic time since an arbitrary origin. Tests substitute a fake clock.
pub trait Clock: Send {
    fn now(&self) -> Duration;
}

pub struct MonotonicClock(Instant);

impl Default for MonotonicClock {
    fn default() -> Self {
        MonotonicClock(Instant::now())
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Reads `/proc` and `statvfs` on the running Linux guest.
pub struct LinuxProbe;

impl Probe for LinuxProbe {
    fn disk_available(&mut self, path: &Path) -> Result<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("path {}", path.display()))?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("statvfs {}", path.display()));
        }
        Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }

    fn memory(&mut self) -> Result<Memory> {
        parse_meminfo(&std::fs::read_to_string("/proc/meminfo")?)
    }

    fn cpu(&mut self) -> Result<CpuTimes> {
        parse_proc_stat(&std::fs::read_to_string("/proc/stat")?)
    }
}

pub fn parse_meminfo(text: &str) -> Result<Memory> {
    let field = |name: &str| -> Result<u64> {
        let line = text
            .lines()
            .find(|l| l.split(':').next() == Some(name))
            .with_context(|| format!("/proc/meminfo has no {name}"))?;
        let mut parts = line[name.len() + 1..].split_whitespace();
        let v: u64 = parts
            .next()
            .context("empty value")?
            .parse()
            .with_context(|| format!("{name} is not a number"))?;
        match parts.next() {
            Some("kB") => Ok(v),
            unit => bail!("{name} has unexpected unit {unit:?}"),
        }
    };
    let m = Memory {
        available_kib: field("MemAvailable")?,
        total_kib: field("MemTotal")?,
    };
    if m.total_kib == 0 {
        bail!("MemTotal is 0");
    }
    Ok(m)
}

pub fn parse_proc_stat(text: &str) -> Result<CpuTimes> {
    let line = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some("cpu"))
        .context("/proc/stat has no aggregate cpu line")?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse::<u64>().context("cpu field is not a number"))
        .collect::<Result<_>>()?;
    if values.len() < 4 {
        bail!("/proc/stat cpu line has {} fields, need >= 4", values.len());
    }
    let mut fields = [0; 8];
    for (f, v) in fields.iter_mut().zip(&values) {
        *f = *v;
    }
    Ok(CpuTimes { fields })
}

/// Busy percent between two cumulative samples; `None` when no time elapsed.
pub fn cpu_busy_percent(from: &CpuTimes, to: &CpuTimes) -> Option<f64> {
    let d: Vec<u64> = (0..8)
        .map(|i| to.fields[i].saturating_sub(from.fields[i]))
        .collect();
    let total: u64 = d.iter().sum();
    if total == 0 {
        return None;
    }
    let idle = d[3] + d[4];
    Some(100.0 * total.saturating_sub(idle) as f64 / total as f64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Clear,
    Constrained,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Disk,
    Memory,
    Cpu,
}

/// One signal's current reading and decision.
#[derive(Debug, Clone, Serialize)]
pub struct SignalStatus {
    pub name: &'static str,
    pub level: Level,
    /// Bytes (disk) or percent (memory, cpu); null when unknown.
    pub value: Option<f64>,
    /// Human-readable reading, or why it is unknown.
    pub reading: String,
    pub pause: String,
    pub resume: String,
}

/// Latest decision, shared with the API.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub admitting: bool,
    /// Why new sessions are paused; null while admitting.
    pub reason: Option<String>,
    /// Whether new bot runs may start (the hard floor, see the module docs).
    pub bots_admitting: bool,
    /// Why bot runs are held at the floor; null while admitting bots.
    pub bots_reason: Option<String>,
    pub checked_at: Option<String>,
    pub signals: Vec<SignalStatus>,
    pub thresholds: Option<Resources>,
}

/// Status plus when the dispatcher published it.
pub struct Published {
    status: Status,
    at: Option<Instant>,
    stale_after: Duration,
}

pub type Handle = Arc<Mutex<Published>>;

pub fn handle(resources: Option<&Resources>) -> Handle {
    let status = match resources {
        None => Status {
            enabled: false,
            admitting: true,
            reason: None,
            bots_admitting: true,
            bots_reason: None,
            checked_at: None,
            signals: Vec::new(),
            thresholds: None,
        },
        Some(r) => Status {
            enabled: true,
            admitting: false,
            reason: Some("not sampled yet".into()),
            bots_admitting: false,
            bots_reason: Some("not sampled yet".into()),
            checked_at: None,
            signals: Vec::new(),
            thresholds: Some(r.clone()),
        },
    };
    let stale_after = Duration::from_secs(resources.map_or(0, |r| r.stale_seconds));
    Arc::new(Mutex::new(Published {
        status,
        at: None,
        stale_after,
    }))
}

/// The status as the API reports it. A status the dispatcher has not refreshed
/// within `stale_seconds` is reported as not admitting.
pub fn view(h: &Handle) -> Value {
    let p = h.lock().unwrap_or_else(|e| e.into_inner());
    let mut status = p.status.clone();
    let age = p.at.map(|at| at.elapsed());
    if status.enabled {
        if let Some(age) = age.filter(|a| *a > p.stale_after) {
            let stale = format!(
                "status stale: dispatcher last checked {}s ago",
                age.as_secs()
            );
            status.admitting = false;
            status.reason = Some(stale.clone());
            status.bots_admitting = false;
            status.bots_reason = Some(stale);
        }
    }
    let mut v = json!(status);
    v["age_seconds"] = json!(age.map(|a| a.as_secs()));
    v
}

/// How long the server trusts a dispatcher report without a newer one: the
/// dispatcher lease. Dispatchers report at least every 10 s.
pub const REPORT_STALE: Duration = Duration::from_secs(60);

/// The server's view of the last dispatcher report, `(view, received)`. A missing
/// or expired report is unknown (`admitting` null), never healthy; an enabled
/// gate's reading ages by the time since the report and goes stale under the
/// dispatcher's own `stale_seconds` rule.
pub fn reported(report: Option<&(Value, Instant)>) -> Value {
    let Some((v, at)) = report else {
        return json!({
            "enabled": null,
            "admitting": null,
            "reason": "unknown: no dispatcher has reported since the server started",
            "checked_at": null,
            "signals": [],
            "thresholds": null,
            "age_seconds": null,
            "report_age_seconds": null,
        });
    };
    let mut v = v.clone();
    let since = at.elapsed().as_secs();
    v["report_age_seconds"] = json!(since);
    let age = v["age_seconds"].as_u64().map(|a| a + since);
    v["age_seconds"] = json!(age);
    // Older dispatchers report no bot decision; their fields stay absent.
    let bots = v.get("bots_admitting").is_some();
    if at.elapsed() > REPORT_STALE {
        let why = json!(format!("unknown: dispatcher last reported {since}s ago"));
        v["admitting"] = Value::Null;
        v["reason"] = why.clone();
        if bots {
            v["bots_admitting"] = Value::Null;
            v["bots_reason"] = why;
        }
    } else if v["enabled"] == true {
        let stale = v["thresholds"]["stale_seconds"].as_u64();
        if let (Some(stale), Some(age)) = (stale, age) {
            if age > stale {
                let why = json!(format!("status stale: dispatcher last checked {age}s ago"));
                v["admitting"] = json!(false);
                v["reason"] = why.clone();
                if bots {
                    v["bots_admitting"] = json!(false);
                    v["bots_reason"] = why;
                }
            }
        }
    }
    v
}

#[derive(Debug, Clone)]
struct Signal {
    kind: Kind,
    level: Level,
    value: Option<f64>,
    reading: String,
}

pub struct Gate {
    cfg: Resources,
    path: PathBuf,
    probe: Box<dyn Probe>,
    clock: Box<dyn Clock>,
    emit: Box<dyn FnMut(&str) + Send>,
    started: Duration,
    next_sample: Duration,
    cpu: VecDeque<(Duration, CpuTimes)>,
    signals: [Signal; 3],
    admitting: bool,
    /// Why bot runs are held at the hard floor; `None` while bots may start.
    bot_floor: Option<String>,
    samples: u64,
    published: Handle,
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

impl Gate {
    pub fn new(cfg: Resources, path: PathBuf) -> Gate {
        Gate::with(
            cfg,
            path,
            Box::new(LinuxProbe),
            Box::new(MonotonicClock::default()),
        )
    }

    pub fn with(
        cfg: Resources,
        path: PathBuf,
        probe: Box<dyn Probe>,
        clock: Box<dyn Clock>,
    ) -> Gate {
        let started = clock.now();
        let unknown = |kind| Signal {
            kind,
            level: Level::Unknown,
            value: None,
            reading: "not sampled yet".into(),
        };
        let published = handle(Some(&cfg));
        Gate {
            cfg,
            path,
            probe,
            clock,
            emit: Box::new(|line| eprintln!("{line}")),
            started,
            next_sample: started,
            cpu: VecDeque::new(),
            signals: [
                unknown(Kind::Disk),
                unknown(Kind::Memory),
                unknown(Kind::Cpu),
            ],
            admitting: false,
            bot_floor: Some("not sampled yet".into()),
            samples: 0,
            published,
        }
    }

    /// Replaces the journal writer (stderr) for transition lines.
    pub fn with_emit(mut self, emit: Box<dyn FnMut(&str) + Send>) -> Gate {
        self.emit = emit;
        self
    }

    pub fn handle(&self) -> Handle {
        Arc::clone(&self.published)
    }

    /// Watched path and thresholds, for the startup journal line.
    pub fn summary(&self) -> String {
        let t = |k| {
            let (pause, resume) = thresholds(&self.cfg, k);
            format!("{} pause {pause}, resume {resume}", name(k))
        };
        format!(
            "on {}: {}; {}; {}; sample {} s, stale {} s",
            self.path.display(),
            t(Kind::Disk),
            t(Kind::Memory),
            t(Kind::Cpu),
            self.cfg.sample_seconds,
            self.cfg.stale_seconds
        )
    }

    pub fn admitting(&self) -> bool {
        self.admitting
    }

    /// Completed samples (periodic and pre-spawn).
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Time until the next periodic sample is due.
    pub fn until_due(&self) -> Duration {
        self.next_sample.saturating_sub(self.clock.now())
    }

    /// True while the CPU window cannot yet span `cpu_window_seconds` since start.
    pub fn warming_up(&self) -> bool {
        self.clock.now().saturating_sub(self.started) < self.window()
    }

    fn window(&self) -> Duration {
        Duration::from_secs(self.cfg.cpu_window_seconds)
    }

    /// Takes the periodic sample when due. Returns true when admission reopened.
    pub fn tick(&mut self) -> bool {
        if self.until_due() > Duration::ZERO {
            return false;
        }
        let was = self.admitting;
        self.sample();
        !was && self.admitting
    }

    /// Samples now and returns whether a new session may start. The dispatcher
    /// calls this immediately before every claim.
    pub fn check(&mut self) -> bool {
        self.sample();
        self.admitting
    }

    /// Whether bot runs may start at the latest sample: only the hard floor
    /// holds them, never the normal pause.
    pub fn bots_admitting(&self) -> bool {
        self.bot_floor.is_none()
    }

    /// Samples now and returns whether a new bot run may start. The dispatcher
    /// calls this immediately before every bot claim.
    pub fn check_bots(&mut self) -> bool {
        self.sample();
        self.bots_admitting()
    }

    fn sample(&mut self) {
        let now = self.clock.now();
        self.next_sample = now + Duration::from_secs(self.cfg.sample_seconds);
        self.samples += 1;
        let disk = self
            .probe
            .disk_available(&self.path)
            .map(|b| b as f64)
            .map_err(|e| format!("{e:#}"));
        let memory = self
            .probe
            .memory()
            .map(|m| 100.0 * m.available_kib as f64 / m.total_kib as f64)
            .map_err(|e| format!("{e:#}"));
        let cpu = match self.probe.cpu() {
            Ok(t) => {
                self.cpu.push_back((now, t));
                self.cpu_reading(now)
            }
            Err(e) => Err(format!("unavailable: {e:#}")),
        };
        let readings = [
            disk.map_err(|e| format!("unavailable: {e}")),
            memory.map_err(|e| format!("unavailable: {e}")),
            cpu,
        ];
        let mut changes = Vec::new();
        for (signal, reading) in self.signals.iter_mut().zip(readings) {
            let before = signal.level;
            update(&self.cfg, signal, reading);
            if signal.level != before {
                changes.push(format!(
                    "axe admission: {} {} -> {}: {}",
                    name(signal.kind),
                    level_str(before),
                    level_str(signal.level),
                    describe(&self.cfg, signal, &self.path)
                ));
            }
        }
        let was = self.admitting;
        self.admitting = self.signals.iter().all(|s| s.level == Level::Clear);
        let reason = (!self.admitting).then(|| self.reason());
        let floor_was = self.bot_floor.is_none();
        self.bot_floor = self.floor_reason();
        for line in changes {
            (self.emit)(&line);
        }
        if was != self.admitting || self.samples == 1 {
            match &reason {
                Some(r) => (self.emit)(&format!("axe admission: paused new sessions: {r}")),
                None => (self.emit)("axe admission: admitting new sessions"),
            }
        }
        if floor_was != self.bot_floor.is_none() || self.samples == 1 {
            match &self.bot_floor {
                Some(r) => (self.emit)(&format!("axe admission: bot runs held at the floor: {r}")),
                None => (self.emit)("axe admission: admitting bot runs"),
            }
        }
        self.publish(reason);
    }

    /// Why bot runs are held: disk or memory unreadable, or below half its
    /// pause threshold. CPU is ignored.
    fn floor_reason(&self) -> Option<String> {
        let held: Vec<String> = self
            .signals
            .iter()
            .filter_map(|s| {
                let floor = floor(&self.cfg, s.kind)?;
                match s.value {
                    _ if s.level == Level::Unknown => {
                        Some(format!("{} unknown: {}", name(s.kind), s.reading))
                    }
                    Some(v) if v < floor => Some(format!(
                        "{} {} (floor {})",
                        name(s.kind),
                        s.reading,
                        floor_str(&self.cfg, s.kind)
                    )),
                    _ => None,
                }
            })
            .collect();
        (!held.is_empty()).then(|| held.join("; "))
    }

    /// Busy percent over the newest sample and the newest one at least a window
    /// older, pruning samples no longer needed.
    fn cpu_reading(&mut self, now: Duration) -> std::result::Result<f64, String> {
        let window = self.window();
        let latest = self.cpu.back().copied().expect("just pushed");
        while self.cpu.len() >= 2 && latest.0.saturating_sub(self.cpu[1].0) >= window {
            self.cpu.pop_front();
        }
        let base = self.cpu[0];
        let span = latest.0.saturating_sub(base.0);
        if span < window {
            let since = now.saturating_sub(self.started).min(window);
            return Err(if since < window {
                format!(
                    "warming up ({} of {} s window)",
                    since.as_secs(),
                    window.as_secs()
                )
            } else {
                format!("no sample spanning the {} s window", window.as_secs())
            });
        }
        if span > window + Duration::from_secs(self.cfg.stale_seconds) {
            // Sampling stalled: the oldest usable sample is too old to describe the window.
            self.cpu.drain(..self.cpu.len() - 1);
            return Err(format!(
                "stale window ({} s since the previous usable sample)",
                span.as_secs()
            ));
        }
        cpu_busy_percent(&base.1, &latest.1).ok_or_else(|| "no CPU time elapsed".into())
    }

    fn reason(&self) -> String {
        self.signals
            .iter()
            .filter(|s| s.level != Level::Clear)
            .map(|s| describe(&self.cfg, s, &self.path))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn publish(&self, reason: Option<String>) {
        let status = Status {
            enabled: true,
            admitting: self.admitting,
            reason,
            bots_admitting: self.bot_floor.is_none(),
            bots_reason: self.bot_floor.clone(),
            checked_at: Some(crate::db::now()),
            signals: self
                .signals
                .iter()
                .map(|s| {
                    let (pause, resume) = thresholds(&self.cfg, s.kind);
                    SignalStatus {
                        name: name(s.kind),
                        level: s.level,
                        value: s.value,
                        reading: s.reading.clone(),
                        pause,
                        resume,
                    }
                })
                .collect(),
            thresholds: Some(self.cfg.clone()),
        };
        let mut p = self.published.lock().unwrap_or_else(|e| e.into_inner());
        p.status = status;
        p.at = Some(Instant::now());
    }
}

fn update(cfg: &Resources, s: &mut Signal, reading: std::result::Result<f64, String>) {
    let v = match reading {
        Err(why) => {
            s.level = Level::Unknown;
            s.value = None;
            s.reading = why;
            return;
        }
        Ok(v) => v,
    };
    let (pause, resume) = match s.kind {
        Kind::Disk => (
            v < cfg.disk_pause_below_gib * GIB,
            v > cfg.disk_resume_above_gib * GIB,
        ),
        Kind::Memory => (
            v < cfg.memory_pause_below_percent,
            v > cfg.memory_resume_above_percent,
        ),
        Kind::Cpu => (
            v > cfg.cpu_pause_above_percent,
            v < cfg.cpu_resume_below_percent,
        ),
    };
    s.level = match s.level {
        Level::Clear if pause => Level::Constrained,
        Level::Clear => Level::Clear,
        _ if resume => Level::Clear,
        _ => Level::Constrained,
    };
    s.value = Some(v);
    s.reading = match s.kind {
        Kind::Disk => format!("{:.1} GiB free", v / GIB),
        Kind::Memory => format!("{v:.1}% available"),
        Kind::Cpu => format!("{v:.1}% busy over {} s", cfg.cpu_window_seconds),
    };
}

/// The bot hard floor in reading units (bytes, percent); `None` for CPU,
/// which never holds bots.
fn floor(cfg: &Resources, k: Kind) -> Option<f64> {
    match k {
        Kind::Disk => Some(cfg.disk_pause_below_gib * GIB / 2.0),
        Kind::Memory => Some(cfg.memory_pause_below_percent / 2.0),
        Kind::Cpu => None,
    }
}

fn floor_str(cfg: &Resources, k: Kind) -> String {
    match k {
        Kind::Disk => format!("< {} GiB free", num(cfg.disk_pause_below_gib / 2.0)),
        Kind::Memory => format!("< {}% available", num(cfg.memory_pause_below_percent / 2.0)),
        Kind::Cpu => "none".into(),
    }
}

fn name(k: Kind) -> &'static str {
    match k {
        Kind::Disk => "disk",
        Kind::Memory => "memory",
        Kind::Cpu => "cpu",
    }
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Clear => "clear",
        Level::Constrained => "constrained",
        Level::Unknown => "unknown",
    }
}

fn thresholds(cfg: &Resources, k: Kind) -> (String, String) {
    match k {
        Kind::Disk => (
            format!("< {} GiB free", num(cfg.disk_pause_below_gib)),
            format!("> {} GiB free", num(cfg.disk_resume_above_gib)),
        ),
        Kind::Memory => (
            format!("< {}% available", num(cfg.memory_pause_below_percent)),
            format!("> {}% available", num(cfg.memory_resume_above_percent)),
        ),
        Kind::Cpu => (
            format!(
                "> {}% busy over {} s",
                num(cfg.cpu_pause_above_percent),
                cfg.cpu_window_seconds
            ),
            format!(
                "< {}% busy over {} s",
                num(cfg.cpu_resume_below_percent),
                cfg.cpu_window_seconds
            ),
        ),
    }
}

fn describe(cfg: &Resources, s: &Signal, path: &Path) -> String {
    let (pause, resume) = thresholds(cfg, s.kind);
    let what = match s.kind {
        Kind::Disk => format!("disk {}", path.display()),
        k => name(k).to_string(),
    };
    match s.level {
        Level::Clear => format!("{what} {} (pause {pause})", s.reading),
        Level::Unknown => format!("{what} unknown: {} (resume {resume})", s.reading),
        Level::Constrained => format!("{what} {} (pause {pause}, resume {resume})", s.reading),
    }
}

/// A manual, expiring hold on task admission (`axe admission hold`): while
/// it is active the dispatcher claims no task for a new session. Running
/// sessions, deferred fallbacks of already claimed tasks and bot runs are not
/// affected. The hold is a file on the dispatcher host, so it needs no server
/// change and survives restarts of either process; it ends by itself at
/// `until`, or when `axe admission release` removes the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Hold {
    /// RFC3339 instant the hold ends.
    pub until: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub set_at: Option<String>,
    #[serde(default)]
    pub set_by: Option<String>,
}

/// The hold file: `$AXE_ADMISSION_HOLD`, else
/// `~/.local/state/axe/admission-hold.json`.
pub fn hold_path() -> PathBuf {
    match std::env::var_os("AXE_ADMISSION_HOLD").filter(|v| !v.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => crate::config::expand_tilde("~/.local/state/axe/admission-hold.json"),
    }
}

/// The hold as the dispatcher sees it at `now`.
#[derive(Debug, Clone, PartialEq)]
pub enum HoldState {
    /// No hold file.
    None,
    Active(Hold),
    /// Past its `until`: admission is open again.
    Expired(Hold),
    /// The file exists but cannot be read or parsed. Fails closed: admission
    /// stays held until the file is fixed or released.
    Invalid(String),
}

impl HoldState {
    /// Why task admission is held; `None` when it is not.
    pub fn reason(&self) -> Option<String> {
        match self {
            HoldState::Active(h) => Some(match &h.reason {
                Some(r) => format!("admission hold until {} ({r})", h.until),
                None => format!("admission hold until {}", h.until),
            }),
            HoldState::Invalid(e) => Some(format!("admission hold file unreadable: {e}")),
            HoldState::None | HoldState::Expired(_) => None,
        }
    }

    /// The `hold` field of the admission report; null when not held.
    pub fn json(&self) -> Value {
        match self {
            HoldState::Active(h) => json!({
                "until": h.until, "reason": h.reason, "set_at": h.set_at,
                "set_by": h.set_by, "error": null,
            }),
            HoldState::Invalid(e) => json!({
                "until": null, "reason": null, "set_at": null, "set_by": null, "error": e,
            }),
            HoldState::None | HoldState::Expired(_) => Value::Null,
        }
    }
}

fn parse_instant(s: &str) -> Result<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .with_context(|| format!("bad hold time {s:?}"))
}

/// Reads the hold file at `path` as of `now`.
pub fn read_hold(path: &Path, now: time::OffsetDateTime) -> HoldState {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HoldState::None,
        Err(e) => return HoldState::Invalid(format!("{}: {e}", path.display())),
    };
    let hold: Hold = match serde_json::from_str(&text) {
        Ok(h) => h,
        Err(e) => return HoldState::Invalid(format!("{}: {e}", path.display())),
    };
    match parse_instant(&hold.until) {
        Ok(until) if until > now => HoldState::Active(hold),
        Ok(_) => HoldState::Expired(hold),
        Err(e) => HoldState::Invalid(format!("{}: {e:#}", path.display())),
    }
}

/// Writes `hold` to `path` atomically (temporary file, then rename).
pub fn write_hold(path: &Path, hold: &Hold) -> Result<()> {
    parse_instant(&hold.until)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(hold)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

/// Removes the hold file; false when there was none.
pub fn remove_hold(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Applies a hold to an admission view: `admitting` goes false with the hold
/// first in `reason`; bot fields are untouched. Always sets `hold`.
pub fn apply_hold(view: &mut Value, hold: &HoldState) {
    view["hold"] = hold.json();
    if let Some(why) = hold.reason() {
        let reason = match view["reason"]
            .as_str()
            .filter(|_| view["admitting"] == false)
        {
            Some(r) => format!("{why}; {r}"),
            None => why,
        };
        view["admitting"] = json!(false);
        view["reason"] = json!(reason);
    }
}
