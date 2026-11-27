//! Bot services in the node (`service.rs` holds the declarations).
//!
//! The node runs every declared, enabled service as a child process
//! (`sh -c <command>`, its own process group) with its output appended to
//! `<log_dir>/services/<bot>/<name>.log`, rotated past `[services] log_bytes`
//! with one previous file kept. A process that exits is restarted by its
//! policy after a delay that doubles with each quick crash in a row (a run
//! shorter than `stable_seconds`), up to `restart_max_seconds`.
//! `crash_loop_restarts` quick crashes in a row are a crash loop: the owning
//! bot is woken; a loop still going `escalate_after_seconds` later alerts the
//! owner too. A stop sends SIGTERM to the process group and SIGKILL after
//! `stop_grace_seconds`. On a drain (an upgrade or restart) the owning bots
//! are woken first and the services keep running for `stop_warn_seconds`.
//!
//! A scheduled service (one with a `Schedule`) is `scheduled` between runs
//! and starts at each cron time in its time zone. There is one run at a
//! time: a run time that comes while the last run is still going is skipped
//! (with a line in the log). A run still going `timeout_seconds` after its
//! start gets SIGTERM, then SIGKILL after `stop_grace_seconds`; it counts as
//! a failure, as a non-zero exit or a failed start, and none is retried
//! before the next run time. When the supervisor first sees a scheduled
//! service (the node started, or the service is new) and a run time passed
//! since its last run (the start the server last stored), or since its
//! declaration if it never ran, it runs once at once: the one catch-up, so
//! however many run times the node missed while down, none is replayed
//! beyond it. `catch_up: false` turns it off. A draining node starts no
//! scheduled run.
//!
//! [`Supervisor`] holds the processes and takes the time as an argument, so
//! tests drive it with their own clock ([`Supervisor::with_clock`] maps it
//! to wall time for schedules); the node syncs it with the
//! server ([`Supervisor::apply`], [`Supervisor::statuses`]) and sends the
//! notices it returns.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::Services as Cfg;
use crate::service::{Notice, Service, Status};

/// Variables a service never inherits from the node: they belong to
/// task and bot runs.
const ENV_REMOVE: [&str; 5] = [
    "AXE_TASK",
    "AXE_WORKTREE",
    "AXE_BOT_RUN",
    "AXE_SESSION",
    "AXE_SECRETS_DIR",
];

/// The log file of service `name` of the bot with folder key `home`.
pub fn log_path(log_root: &Path, home: &str, name: &str) -> PathBuf {
    log_root
        .join("services")
        .join(home)
        .join(format!("{name}.log"))
}

/// The last `lines` lines of a service log (and of its rotated file when the
/// current one is shorter), for `axe service logs`.
pub fn tail(log_root: &Path, home: &str, name: &str, lines: usize) -> Result<String> {
    crate::db::validate_name("bot", home)?;
    crate::db::validate_name("service", name)?;
    let path = log_path(log_root, home, name);
    let read = |p: &Path| -> Result<String> {
        let mut f = File::open(p)?;
        let len = f.metadata()?.len();
        // Enough for the lines asked for at a generous line length.
        let window = (lines as u64 * 512).max(64 * 1024);
        f.seek(SeekFrom::Start(len.saturating_sub(window)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    };
    let current = match read(&path) {
        Ok(t) => t,
        Err(_) if !path.exists() => bail!("no log yet for service {name} on this node"),
        Err(e) => return Err(e),
    };
    let mut text = current;
    if text.lines().count() < lines {
        if let Ok(old) = read(&path.with_extension("log.1")) {
            text = old + &text;
        }
    }
    let all: Vec<&str> = text.lines().collect();
    Ok(all[all.len().saturating_sub(lines)..].join("\n"))
}

fn wall() -> String {
    crate::db::now()
}

fn wall_in(d: Duration) -> String {
    crate::bot::ts(crate::remote::epoch() + d.as_secs() as i64)
}

fn exit_text(s: ExitStatus) -> String {
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(sig)) => format!("signal {sig}"),
        _ => "exited".into(),
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s >= 60 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

struct Proc {
    /// The definition the process runs (or will run).
    svc: Service,
    /// Declared and enabled.
    want: bool,
    child: Option<Child>,
    started: Option<Instant>,
    status: Status,
    /// Quick crashes in a row.
    quick: u32,
    next_start: Option<Instant>,
    /// SIGTERM sent at.
    term_at: Option<Instant>,
    /// A changed definition to run once the old process stopped.
    pending: Option<Service>,
    /// The crash loop in progress: since when, and whether the owner was
    /// alerted.
    crash_loop: Option<(Instant, i64)>,
    escalated: bool,
    /// A scheduled service: the next run time (epoch seconds), and whether
    /// the live run is past its timeout.
    next_run: Option<i64>,
    timed_out: bool,
}

impl Proc {
    /// First sight of `svc` at `now` (wall `epoch`): a long-running service
    /// starts at once, a scheduled one waits for its next run time, or runs
    /// once at once to catch up ([`catch_up`]). A scheduled service keeps
    /// its last run as the server stored it.
    fn new(svc: Service, now: Instant, epoch: i64) -> Self {
        let want = svc.enabled;
        let mut status = Status {
            state: if want { "starting" } else { "stopped" }.into(),
            ..Status::default()
        };
        if svc.spec.schedule.is_some() {
            status.started_at = svc.status.started_at.clone();
            status.finished_at = svc.status.finished_at.clone();
            status.last_exit = svc.status.last_exit.clone();
        }
        let mut p = Proc {
            want,
            child: None,
            started: None,
            status,
            quick: 0,
            next_start: None,
            term_at: None,
            pending: None,
            crash_loop: None,
            escalated: false,
            next_run: None,
            timed_out: false,
            svc,
        };
        if want {
            p.arm(now, epoch);
            if catch_up(&p.svc, epoch) {
                p.next_run = Some(epoch);
            }
        }
        p
    }

    fn scheduled(&self) -> bool {
        self.svc.spec.schedule.is_some()
    }

    /// Ready to run its definition: a long-running service starts at `now`,
    /// a scheduled one waits for its next run time after `epoch`.
    fn arm(&mut self, now: Instant, epoch: i64) {
        self.status.next_start_at = None;
        if self.scheduled() {
            self.next_start = None;
            self.status.state = "scheduled".into();
            self.plan(epoch);
        } else {
            self.next_run = None;
            self.status.next_run_at = None;
            self.next_start = Some(now);
            self.status.state = "starting".into();
        }
    }

    /// The next run time after `epoch`.
    fn plan(&mut self, epoch: i64) {
        let Some(sch) = &self.svc.spec.schedule else {
            return;
        };
        match sch.next_after(epoch) {
            Ok(t) => {
                self.next_run = Some(t);
                self.status.next_run_at = Some(crate::bot::ts(t));
            }
            Err(e) => {
                self.next_run = None;
                self.status.next_run_at = None;
                self.status.error = Some(format!("schedule: {e:#}"));
            }
        }
    }

    fn alive(&self) -> bool {
        self.child.is_some()
    }

    fn signal(&self, sig: i32) {
        if let Some(c) = &self.child {
            // Its own process group (`process_group(0)`): the group id is its pid.
            unsafe {
                libc::kill(-(c.id() as i32), sig);
            }
        }
    }

    /// Sends SIGTERM once; SIGKILL follows after the grace period ([`Supervisor::tick`]).
    fn terminate(&mut self, now: Instant) {
        self.next_start = None;
        self.next_run = None;
        self.status.next_run_at = None;
        if self.alive() && self.term_at.is_none() {
            self.signal(libc::SIGTERM);
            self.term_at = Some(now);
            self.status.state = "stopping".into();
        }
        if !self.alive() {
            self.status.state = "stopped".into();
            self.status.next_start_at = None;
        }
    }

    fn clear_loop(&mut self) {
        self.quick = 0;
        self.crash_loop = None;
        self.escalated = false;
        self.status.crash_loop_since = None;
    }
}

/// Whether a scheduled service seen for the first time at `epoch` runs once
/// at once: catch-up is on and a run time passed since its last run start
/// (or, never run, its last declaration change), so a run was missed.
fn catch_up(svc: &Service, epoch: i64) -> bool {
    let Some(sch) = svc.spec.schedule.as_ref().filter(|s| s.catch_up) else {
        return false;
    };
    let since = svc
        .status
        .started_at
        .as_deref()
        .unwrap_or(svc.updated_at.as_str());
    match (crate::bot::epoch_of(since), sch.latest_at(epoch)) {
        (Some(since), Ok(last)) => last > since,
        _ => false,
    }
}

pub struct Supervisor {
    cfg: Cfg,
    log_root: PathBuf,
    run_root: PathBuf,
    server_url: Option<String>,
    procs: BTreeMap<String, Proc>,
    /// For tests: this instant is this wall time (epoch seconds). None: the
    /// real wall clock.
    clock: Option<(Instant, i64)>,
    /// The node is draining: no scheduled run starts (it would be stopped
    /// in moments); a run time missed meanwhile is caught up at its start.
    hold: bool,
}

impl Supervisor {
    pub fn new(cfg: Cfg, log_root: PathBuf, run_root: PathBuf, server_url: Option<String>) -> Self {
        Supervisor {
            cfg,
            log_root,
            run_root,
            server_url,
            procs: BTreeMap::new(),
            clock: None,
            hold: false,
        }
    }

    /// While `hold`, no scheduled run starts (a drain).
    pub fn hold_runs(&mut self, hold: bool) {
        self.hold = hold;
    }

    /// Reads instant `at` as wall time `epoch` (and the rest relative to
    /// it), for schedules driven by a test clock.
    pub fn with_clock(mut self, at: Instant, epoch: i64) -> Self {
        self.clock = Some((at, epoch));
        self
    }

    /// The wall time (epoch seconds) at `now`.
    fn epoch(&self, now: Instant) -> i64 {
        match self.clock {
            Some((at, epoch)) if now >= at => epoch + now.duration_since(at).as_secs() as i64,
            Some((at, epoch)) => epoch - at.duration_since(now).as_secs() as i64,
            None => crate::remote::epoch(),
        }
    }

    /// Services with a live process.
    pub fn alive(&self) -> usize {
        self.procs.values().filter(|p| p.alive()).count()
    }

    /// Live processes per owning bot: long-running services, then
    /// scheduled (cron) service runs.
    pub fn alive_by_kind(&self) -> (BTreeMap<String, u32>, BTreeMap<String, u32>) {
        let (mut long, mut cron) = (BTreeMap::new(), BTreeMap::new());
        for p in self.procs.values().filter(|p| p.alive()) {
            let m = if p.scheduled() { &mut cron } else { &mut long };
            *m.entry(p.svc.bot.clone()).or_default() += 1;
        }
        (long, cron)
    }

    /// Each bot's services with a live process.
    pub fn alive_by_bot(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for p in self.procs.values().filter(|p| p.alive()) {
            out.entry(p.svc.bot.clone())
                .or_default()
                .push(p.svc.name.clone());
        }
        out
    }

    /// What each service is doing, keyed `<bot>/<name>`.
    pub fn statuses(&self) -> BTreeMap<String, Status> {
        self.procs
            .iter()
            .map(|(k, p)| (k.clone(), p.status.clone()))
            .collect()
    }

    /// Follows the declarations: starts new and enabled services, stops
    /// disabled and removed ones, and restarts a service whose definition
    /// changed.
    /// A scheduled service's changed definition waits for the end of its
    /// current run.
    pub fn apply(&mut self, decls: &[Service], now: Instant) {
        let epoch = self.epoch(now);
        let mut seen = std::collections::BTreeSet::new();
        for d in decls {
            let key = d.key();
            seen.insert(key.clone());
            let Some(p) = self.procs.get_mut(&key) else {
                self.procs.insert(key, Proc::new(d.clone(), now, epoch));
                continue;
            };
            let changed = p.svc.spec != d.spec || p.svc.home != d.home;
            if d.enabled && !p.want {
                // Started again: a fresh start, no backoff.
                p.want = true;
                p.clear_loop();
                if p.term_at.is_some() {
                    p.pending = Some(d.clone());
                } else if !p.alive() {
                    p.svc = d.clone();
                    p.arm(now, epoch);
                }
            } else if !d.enabled && p.want {
                p.want = false;
                p.pending = None;
                p.clear_loop();
                p.terminate(now);
            } else if changed && p.want {
                p.clear_loop();
                if p.alive() && p.scheduled() {
                    // Its run goes on; the definition applies after it.
                    p.pending = Some(d.clone());
                } else if p.alive() {
                    p.pending = Some(d.clone());
                    p.terminate(now);
                } else {
                    p.svc = d.clone();
                    p.arm(now, epoch);
                }
            }
            if !p.alive() && p.pending.is_none() {
                p.svc = d.clone();
            }
            p.svc.enabled = d.enabled;
        }
        for (key, p) in &mut self.procs {
            if !seen.contains(key) {
                p.want = false;
                p.pending = None;
                p.svc.enabled = false;
                p.terminate(now);
            }
        }
        // Removed and stopped: forgotten.
        self.procs.retain(|k, p| seen.contains(k) || p.alive());
    }

    /// One supervision step at `now`: reaps exits and schedules restarts,
    /// kills processes past their grace period, starts due ones, rotates
    /// logs. Returns the notices for the owning bots.
    pub fn tick(&mut self, now: Instant) -> Vec<Notice> {
        let mut notices = Vec::new();
        let cfg = self.cfg.clone();
        let grace = Duration::from_secs(cfg.stop_grace_seconds);
        let stable = Duration::from_secs(cfg.stable_seconds);
        let epoch = self.epoch(now);
        let keys: Vec<String> = self.procs.keys().cloned().collect();
        for key in keys {
            if self.procs[&key].scheduled() {
                self.tick_scheduled(&key, now, epoch);
                continue;
            }
            let p = self.procs.get_mut(&key).expect("key from map");
            // Reap.
            let exited = match p.child.as_mut().map(|c| c.try_wait()) {
                Some(Ok(Some(st))) => Some(st),
                Some(Err(e)) => {
                    eprintln!("axe node: service {key}: wait failed: {e}");
                    None
                }
                _ => None,
            };
            if let Some(st) = exited {
                p.child = None;
                p.status.pid = None;
                p.status.last_exit = Some(exit_text(st));
                let ran = p.started.map_or(Duration::ZERO, |s| now.duration_since(s));
                note_exit(&self.log_root, &p.svc, &exit_text(st));
                if p.term_at.take().is_some() {
                    // Stopped by us: a new definition starts at once.
                    match p.pending.take() {
                        Some(next) if p.want => {
                            p.svc = next;
                            p.arm(now, epoch);
                        }
                        _ => {
                            p.status.state = "stopped".into();
                            p.status.next_start_at = None;
                        }
                    }
                } else {
                    let restart = p.want
                        && match p.svc.spec.restart.as_str() {
                            "always" => true,
                            "on-failure" => !st.success(),
                            _ => false,
                        };
                    if !st.success() {
                        p.status.failures += 1;
                    }
                    if ran >= stable {
                        p.clear_loop();
                    } else if !st.success() || restart {
                        p.quick += 1;
                    }
                    if restart {
                        self_schedule(p, &cfg, now, &mut notices);
                    } else {
                        p.status.state = "exited".into();
                        p.status.next_start_at = None;
                    }
                }
            }
            // A stable run ends the backoff and any crash loop.
            if p.alive()
                && p.started.is_some_and(|s| now.duration_since(s) >= stable)
                && p.quick > 0
            {
                p.clear_loop();
            }
            // A crash loop the bot did not fix: the owner hears of it.
            if let Some((since, epoch)) = p.crash_loop {
                let after = Duration::from_secs(cfg.escalate_after_seconds);
                if !p.escalated && p.want && now.duration_since(since) >= after {
                    p.escalated = true;
                    notices.push(Notice {
                        bot: p.svc.bot.clone(),
                        kind: "escalate".into(),
                        services: vec![p.svc.name.clone()],
                        text: format!(
                            "Service {} is still crash looping {} after its wake (last: {}). The owner has been alerted. Logs: axe service logs {}",
                            p.svc.name,
                            human(after),
                            p.status.last_exit.as_deref().unwrap_or("unknown"),
                            p.svc.name
                        ),
                        key: format!("{key}:escalate:{epoch}"),
                    });
                }
            }
            // Past the grace period: SIGKILL.
            if p.alive() && p.term_at.is_some_and(|t| now.duration_since(t) >= grace) {
                p.signal(libc::SIGKILL);
            }
            // Due: start.
            if p.want && !p.alive() && p.next_start.is_some_and(|t| now >= t) {
                p.next_start = None;
                p.status.next_start_at = None;
                let first = p.started.is_none();
                match spawn(
                    &p.svc,
                    &self.log_root,
                    &self.run_root,
                    self.server_url.as_deref(),
                ) {
                    Ok((child, log)) => {
                        if !first {
                            p.status.restarts += 1;
                        }
                        p.status.pid = Some(child.id());
                        p.status.state = "running".into();
                        p.status.started_at = Some(wall());
                        p.status.error = None;
                        p.status.log = Some(log.display().to_string());
                        p.child = Some(child);
                        p.started = Some(now);
                    }
                    Err(e) => {
                        eprintln!("axe node: service {key}: start failed: {e:#}");
                        p.status.error = Some(format!("{e:#}"));
                        p.status.failures += 1;
                        p.started = Some(now);
                        p.quick += 1;
                        self_schedule(p, &cfg, now, &mut notices);
                    }
                }
            }
            if p.alive() {
                rotate(
                    &log_path(&self.log_root, &p.svc.home, &p.svc.name),
                    cfg.log_bytes,
                );
            }
        }
        notices
    }

    /// [`Self::tick`] for a scheduled service: reaps its run, stops it past
    /// its timeout or kills it past the grace period, and starts the run
    /// that is due (or skips it while the last one goes on).
    fn tick_scheduled(&mut self, key: &str, now: Instant, epoch: i64) {
        let grace = Duration::from_secs(self.cfg.stop_grace_seconds);
        let log_bytes = self.cfg.log_bytes;
        let (log_root, run_root) = (&self.log_root, &self.run_root);
        let server_url = self.server_url.as_deref();
        let hold = self.hold;
        let p = self.procs.get_mut(key).expect("key from map");
        let exited = match p.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(Some(st))) => Some(st),
            Some(Err(e)) => {
                eprintln!("axe node: service {key}: wait failed: {e}");
                None
            }
            _ => None,
        };
        if let Some(st) = exited {
            p.child = None;
            p.status.pid = None;
            let how = match &p.svc.spec.schedule {
                Some(s) if p.timed_out => {
                    format!("timeout after {}s ({})", s.timeout_seconds, exit_text(st))
                }
                _ => exit_text(st),
            };
            note_exit(log_root, &p.svc, &how);
            p.status.last_exit = Some(how);
            let stopped = p.term_at.take().is_some() && !p.timed_out;
            if !stopped {
                // A run that ended by itself or by its timeout.
                p.status.finished_at = Some(crate::bot::ts(epoch));
                if p.timed_out || !st.success() {
                    p.status.failures += 1;
                }
            }
            p.timed_out = false;
            match p.pending.take() {
                Some(next) if p.want => {
                    p.svc = next;
                    p.arm(now, epoch);
                }
                _ if p.want && !stopped => p.status.state = "scheduled".into(),
                _ => p.status.state = "stopped".into(),
            }
            if !p.scheduled() {
                // Now long-running: the general tick runs it.
                return;
            }
        }
        let timeout = p
            .svc
            .spec
            .schedule
            .as_ref()
            .map_or(u64::MAX, |s| s.timeout_seconds);
        // Past its timeout: SIGTERM (SIGKILL after the grace period, below).
        if p.alive()
            && p.term_at.is_none()
            && p.started
                .is_some_and(|s| now.duration_since(s) >= Duration::from_secs(timeout))
        {
            log_line(
                log_root,
                &p.svc,
                &format!("timeout after {timeout}s: SIGTERM"),
            );
            p.signal(libc::SIGTERM);
            p.term_at = Some(now);
            p.timed_out = true;
        }
        if p.alive() && p.term_at.is_some_and(|t| now.duration_since(t) >= grace) {
            p.signal(libc::SIGKILL);
        }
        // Due: run, or skip while the last run goes on. Either way the next
        // run time is the first one after now, so missed ones collapse.
        if let Some(due) = p.next_run.filter(|t| p.want && !hold && epoch >= *t) {
            if p.alive() {
                let what = format!(
                    "skipped the run of {}: the last run is still going",
                    crate::bot::ts(due)
                );
                log_line(log_root, &p.svc, &what);
            } else {
                match spawn(&p.svc, log_root, run_root, server_url) {
                    Ok((child, log)) => {
                        p.status.pid = Some(child.id());
                        p.status.state = "running".into();
                        p.status.started_at = Some(crate::bot::ts(epoch));
                        p.status.error = None;
                        p.status.log = Some(log.display().to_string());
                        p.child = Some(child);
                        p.started = Some(now);
                    }
                    Err(e) => {
                        eprintln!("axe node: service {key}: start failed: {e:#}");
                        p.status.error = Some(format!("{e:#}"));
                        p.status.failures += 1;
                        p.status.started_at = Some(crate::bot::ts(epoch));
                        p.status.finished_at = Some(crate::bot::ts(epoch));
                        p.status.last_exit = None;
                    }
                }
            }
            p.plan(epoch);
        }
        if p.alive() {
            rotate(&log_path(log_root, &p.svc.home, &p.svc.name), log_bytes);
        }
    }

    /// Sends SIGTERM to every service (none restarts after it).
    pub fn terminate_all(&mut self, now: Instant) {
        for p in self.procs.values_mut() {
            p.want = false;
            p.pending = None;
            p.terminate(now);
        }
    }

    /// Stops every service: SIGTERM, then SIGKILL to those still running
    /// after the grace period. Returns once none is left.
    pub fn stop_all(&mut self) {
        let start = Instant::now();
        self.terminate_all(start);
        loop {
            self.tick(Instant::now());
            if self.alive() == 0 {
                return;
            }
            // After SIGKILL the group is gone within moments; never hang on it.
            let limit = Duration::from_secs(self.cfg.stop_grace_seconds) + Duration::from_secs(5);
            if start.elapsed() >= limit {
                for p in self.procs.values_mut() {
                    if let Some(mut c) = p.child.take() {
                        let _ = c.wait();
                    }
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILL to every service, without waiting for anything else (`Drop`).
    pub fn kill_all(&mut self) {
        for p in self.procs.values_mut() {
            p.signal(libc::SIGKILL);
            if let Some(mut c) = p.child.take() {
                let _ = c.wait();
            }
        }
    }
}

/// After an unexpected exit or a failed start: waits with backoff, and starts
/// a crash loop when the quick crashes reach the threshold.
fn self_schedule(p: &mut Proc, cfg: &Cfg, now: Instant, notices: &mut Vec<Notice>) {
    let doublings = p.quick.saturating_sub(1).min(20);
    let delay = Duration::from_secs(
        cfg.restart_delay_seconds
            .saturating_mul(1u64 << doublings)
            .min(cfg.restart_max_seconds),
    );
    p.next_start = Some(now + delay);
    p.status.state = "backoff".into();
    p.status.next_start_at = Some(wall_in(delay));
    if p.quick >= cfg.crash_loop_restarts && p.crash_loop.is_none() {
        let epoch = crate::remote::epoch();
        p.crash_loop = Some((now, epoch));
        p.status.crash_loop_since = Some(wall());
        let why = p
            .status
            .error
            .clone()
            .or_else(|| p.status.last_exit.clone())
            .unwrap_or_else(|| "unknown".into());
        notices.push(Notice {
            bot: p.svc.bot.clone(),
            kind: "crash_loop".into(),
            services: vec![p.svc.name.clone()],
            text: format!(
                "Service {name} is crash looping: {n} quick crashes in a row (last: {why}). The node keeps restarting it with backoff (next in {delay}). Read `axe service logs {name}`, then fix it or `axe service stop {name}`. If it still crash loops in {esc}, the owner is alerted.",
                name = p.svc.name,
                n = p.quick,
                delay = human(delay),
                esc = human(Duration::from_secs(cfg.escalate_after_seconds)),
            ),
            key: format!("{}:crash:{epoch}", p.svc.key()),
        });
    }
}

/// Appends a line saying how the process ended to its log.
fn note_exit(log_root: &Path, svc: &Service, how: &str) {
    log_line(log_root, svc, how);
}

/// Appends `--- axe: <now> <what> ---` to the service's log, if it has one.
fn log_line(log_root: &Path, svc: &Service, what: &str) {
    let path = log_path(log_root, &svc.home, &svc.name);
    if let Ok(mut f) = OpenOptions::new().append(true).open(path) {
        let _ = writeln!(f, "--- axe: {} {what} ---", wall());
    }
}

/// Past `max` bytes: the log moves to `<name>.log.1` (replacing it) and the
/// live file starts empty. Services write with O_APPEND, so they go on
/// writing at the new end.
fn rotate(path: &Path, max: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max {
        return;
    }
    let old = path.with_extension("log.1");
    if std::fs::copy(path, &old).is_ok() {
        if let Ok(f) = OpenOptions::new().write(true).open(path) {
            let _ = f.set_len(0);
        }
    }
}

/// Starts `svc`: `sh -c <command>` in its cwd (default: the bot's run
/// folder), in its own process group, with the bot's variables and sealed
/// env values and its own env, output appended to its log. The process gets
/// SIGTERM if the node dies without stopping it.
fn spawn(
    svc: &Service,
    log_root: &Path,
    run_root: &Path,
    server_url: Option<&str>,
) -> Result<(Child, PathBuf)> {
    let bot_dir = run_root.join(format!("bot-{}", svc.home));
    let cwd = match &svc.spec.cwd {
        Some(c) if Path::new(c).is_absolute() => PathBuf::from(c),
        Some(c) => bot_dir.join(c),
        None => {
            std::fs::create_dir_all(&bot_dir)
                .with_context(|| format!("create {}", bot_dir.display()))?;
            bot_dir.clone()
        }
    };
    if !cwd.is_dir() {
        bail!("cwd {} is not a directory", cwd.display());
    }
    let log = log_path(log_root, &svc.home, &svc.name);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    writeln!(file, "--- axe: {} start: {} ---", wall(), svc.spec.command)?;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&svc.spec.command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0);
    for k in ENV_REMOVE {
        cmd.env_remove(k);
    }
    cmd.env("AXE_BOT", &svc.bot)
        .env("AXE_ACTOR", format!("bot:{}", svc.bot))
        .env("AXE_SERVICE", &svc.name);
    if let Some(url) = server_url {
        cmd.env("AXE_SERVER", url);
    }
    if let Some(secrets) = crate::sealed::secrets_dir(
        run_root,
        &crate::sealed::Target::Bot {
            bot: svc.home.clone(),
        },
    ) {
        for (k, v) in crate::sealed::launch_env(&secrets) {
            cmd.env(k, v);
        }
        cmd.env("AXE_SECRETS_DIR", secrets);
    }
    cmd.envs(&svc.spec.env);
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawn service {}", svc.key()))?;
    Ok((child, log))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::NewService;

    fn cfg() -> Cfg {
        Cfg {
            stop_grace_seconds: 10,
            restart_delay_seconds: 1,
            restart_max_seconds: 4,
            stable_seconds: 60,
            crash_loop_restarts: 3,
            escalate_after_seconds: 300,
            ..Cfg::default()
        }
    }

    fn svc(name: &str, command: &str, restart: &str) -> Service {
        Service {
            bot: "b".into(),
            home: "b".into(),
            name: name.into(),
            spec: NewService {
                command: command.into(),
                cwd: None,
                env: BTreeMap::from([("GREETING".to_string(), "hi".to_string())]),
                restart: restart.into(),
                schedule: None,
            },
            enabled: true,
            status: Status::default(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        sup: Supervisor,
        t0: Instant,
    }

    impl Rig {
        fn new(cfg: Cfg) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let sup = Supervisor::new(
                cfg,
                dir.path().join("logs"),
                dir.path().join("runs"),
                Some("http://axe.test".into()),
            );
            Rig {
                dir,
                sup,
                t0: Instant::now(),
            }
        }
        fn at(&self, secs: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(secs)
        }
        fn state(&self, key: &str) -> Status {
            self.sup.statuses()[key].clone()
        }
        /// Ticks at fake time `secs` until `f` holds (the process is real).
        fn until(&mut self, secs: f64, mut f: impl FnMut(&Status) -> bool) -> Vec<Notice> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut notices = Vec::new();
            loop {
                notices.extend(self.sup.tick(self.at(secs)));
                if f(&self.state("b/s")) {
                    return notices;
                }
                assert!(Instant::now() < deadline, "{:?}", self.state("b/s"));
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        fn log(&self) -> String {
            std::fs::read_to_string(log_path(&self.dir.path().join("logs"), "b", "s"))
                .unwrap_or_default()
        }
    }

    #[test]
    fn starts_with_the_bot_env_in_the_bot_run_folder_and_logs_output() {
        let mut r = Rig::new(cfg());
        let s = svc(
            "s",
            "echo \"$GREETING $AXE_BOT $AXE_SERVICE $AXE_SERVER $PWD\"; sleep 30",
            "always",
        );
        r.sup.apply(&[s], r.at(0.0));
        r.until(0.0, |s| s.state == "running");
        let st = r.state("b/s");
        assert!(st.pid.is_some() && st.started_at.is_some() && st.restarts == 0);
        let runs = r.dir.path().join("runs").join("bot-b");
        let want = format!("hi b s http://axe.test {}", runs.display());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !r.log().contains(&want) {
            assert!(Instant::now() < deadline, "{}", r.log());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(r.sup.alive_by_bot()["b"], ["s"]);
        r.sup.kill_all();
    }

    #[test]
    fn a_crashing_service_restarts_with_doubling_backoff_then_wakes_and_escalates() {
        let mut r = Rig::new(cfg());
        r.sup.apply(&[svc("s", "exit 3", "on-failure")], r.at(0.0));
        // First crash: back off restart_delay (1 s).
        let n = r.until(0.0, |s| s.state == "backoff");
        assert!(n.is_empty());
        assert_eq!(r.state("b/s").last_exit.as_deref(), Some("exit 3"));
        r.sup.tick(r.at(0.9));
        assert_eq!(r.state("b/s").state, "backoff", "not before the delay");
        // Second quick crash: the delay doubles to 2 s.
        r.until(1.0, |s| s.restarts == 1 && s.state == "backoff");
        r.sup.tick(r.at(2.9));
        assert_eq!(r.state("b/s").restarts, 1, "2 s delay");
        // Third quick crash: a crash loop, and the delay doubles to 4 s.
        let n = r.until(3.0, |s| s.restarts == 2 && s.state == "backoff");
        assert_eq!(n.len(), 1, "{n:?}");
        assert_eq!(
            (n[0].kind.as_str(), n[0].bot.as_str(), n[0].services.clone()),
            ("crash_loop", "b", vec!["s".to_string()])
        );
        assert!(n[0].text.contains("crash looping") && n[0].text.contains("exit 3"));
        assert!(n[0].key.starts_with("b/s:crash:"));
        assert!(r.state("b/s").crash_loop_since.is_some());
        r.sup.tick(r.at(6.9));
        assert_eq!(r.state("b/s").restarts, 2, "4 s delay");
        // Fourth: capped at restart_max (4 s); no second crash-loop notice.
        let n = r.until(7.0, |s| s.restarts == 3 && s.state == "backoff");
        assert!(n.is_empty(), "{n:?}");
        r.sup.tick(r.at(10.9));
        assert_eq!(r.state("b/s").restarts, 3, "capped at 4 s");
        let n = r.until(11.0, |s| s.restarts == 4 && s.state == "backoff");
        assert!(n.is_empty());
        assert_eq!(r.state("b/s").failures, 5, "every crash counts");
        // Still looping escalate_after (300 s) after the wake: the owner hears.
        let n = r.sup.tick(r.at(303.5));
        assert_eq!(n.len(), 1, "{n:?}");
        assert_eq!(n[0].kind, "escalate");
        assert!(r.sup.tick(r.at(400.0)).is_empty(), "escalates once");
        assert!(r.log().contains("exit 3 ---"), "{}", r.log());
    }

    #[test]
    fn a_stable_run_resets_the_backoff_and_ends_the_crash_loop() {
        let mut c = cfg();
        c.crash_loop_restarts = 2;
        let mut r = Rig::new(c);
        let flag = r.dir.path().join("stay");
        let cmd = format!(
            "if [ -e {f} ]; then sleep 30; else exit 1; fi",
            f = flag.display()
        );
        r.sup.apply(&[svc("s", &cmd, "on-failure")], r.at(0.0));
        r.until(0.0, |s| s.state == "backoff");
        let n = r.until(1.0, |s| s.restarts == 1 && s.state == "backoff");
        assert_eq!(n.len(), 1);
        std::fs::write(&flag, "").unwrap();
        r.until(3.0, |s| s.state == "running");
        r.sup.tick(r.at(62.9));
        assert!(
            r.state("b/s").crash_loop_since.is_some(),
            "59.9 s is not stable"
        );
        r.sup.tick(r.at(63.0));
        assert!(r.state("b/s").crash_loop_since.is_none());
        // No escalation for a loop that ended.
        assert!(r.sup.tick(r.at(400.0)).is_empty());
        // A crash after the stable run restarts after the base delay.
        std::fs::remove_file(&flag).unwrap();
        r.sup.procs.get_mut("b/s").unwrap().signal(libc::SIGKILL);
        r.until(400.0, |s| s.state == "backoff");
        r.sup.tick(r.at(400.9));
        assert_eq!(r.state("b/s").state, "backoff");
        r.until(401.0, |s| s.restarts == 3);
    }

    #[test]
    fn restart_policies() {
        let mut r = Rig::new(cfg());
        r.sup.apply(&[svc("s", "exit 0", "on-failure")], r.at(0.0));
        r.until(0.0, |s| s.state == "exited");
        assert_eq!(r.state("b/s").last_exit.as_deref(), Some("exit 0"));
        assert_eq!(r.state("b/s").failures, 0, "a clean exit is no failure");
        let mut r = Rig::new(cfg());
        r.sup.apply(&[svc("s", "exit 2", "never")], r.at(0.0));
        r.until(0.0, |s| s.state == "exited");
        let mut r = Rig::new(cfg());
        r.sup.apply(&[svc("s", "exit 0", "always")], r.at(0.0));
        r.until(0.0, |s| s.state == "backoff");
        r.until(1.0, |s| s.restarts == 1);
    }

    #[test]
    fn stop_sends_sigterm_and_kills_only_after_the_grace_period() {
        let mut r = Rig::new(cfg());
        // Ignores SIGTERM: only SIGKILL ends it.
        let cmd = "trap 'echo got-term' TERM; while :; do sleep 0.05; done";
        r.sup.apply(&[svc("s", cmd, "always")], r.at(0.0));
        r.until(0.0, |s| s.state == "running");
        std::thread::sleep(Duration::from_millis(100));
        r.sup.terminate_all(r.at(1.0));
        assert_eq!(r.state("b/s").state, "stopping");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !r.log().contains("got-term") {
            assert!(Instant::now() < deadline, "{}", r.log());
            std::thread::sleep(Duration::from_millis(20));
        }
        // Within the grace period it lives on.
        for t in [2.0, 5.0, 10.9] {
            r.sup.tick(r.at(t));
            std::thread::sleep(Duration::from_millis(50));
            r.sup.tick(r.at(t));
            assert_eq!(r.state("b/s").state, "stopping", "at {t}");
            assert_eq!(r.sup.alive(), 1);
        }
        // Past it: SIGKILL, and no restart.
        r.until(11.0, |s| s.state == "stopped");
        assert_eq!(r.state("b/s").last_exit.as_deref(), Some("signal 9"));
        r.sup.tick(r.at(100.0));
        assert_eq!(r.sup.alive(), 0);
    }

    #[test]
    fn a_service_that_honours_sigterm_stops_at_once() {
        let mut r = Rig::new(cfg());
        let cmd = "trap 'echo bye; exit 0' TERM; while :; do sleep 0.05; done";
        r.sup.apply(&[svc("s", cmd, "always")], r.at(0.0));
        r.until(0.0, |s| s.state == "running");
        std::thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        r.sup.stop_all();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(r.state("b/s").state, "stopped");
        assert_eq!(r.state("b/s").last_exit.as_deref(), Some("exit 0"));
        assert!(r.log().contains("bye"));
    }

    #[test]
    fn declarations_start_stop_restart_and_remove() {
        let mut r = Rig::new(cfg());
        let mut s = svc("s", "echo one; sleep 30", "always");
        r.sup.apply(&[s.clone()], r.at(0.0));
        r.until(0.0, |s| s.state == "running");
        let pid = r.state("b/s").pid;
        // Same definition: untouched.
        r.sup.apply(&[s.clone()], r.at(1.0));
        r.sup.tick(r.at(1.0));
        assert_eq!(r.state("b/s").pid, pid);
        // Disabled: stopped and not restarted.
        s.enabled = false;
        r.sup.apply(&[s.clone()], r.at(2.0));
        r.until(2.0, |s| s.state == "stopped");
        r.sup.tick(r.at(50.0));
        assert_eq!(r.sup.alive(), 0);
        // Enabled again: starts at once.
        s.enabled = true;
        r.sup.apply(&[s.clone()], r.at(51.0));
        r.until(51.0, |s| s.state == "running");
        // A changed definition: the old process stops, the new one starts.
        s.spec.command = "echo two; sleep 30".into();
        r.sup.apply(&[s.clone()], r.at(52.0));
        r.until(52.0, |st| st.state == "running" && st.pid != pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !r.log().contains("two") {
            assert!(Instant::now() < deadline, "{}", r.log());
            std::thread::sleep(Duration::from_millis(20));
        }
        // Removed: stopped, then forgotten.
        r.sup.apply(&[], r.at(53.0));
        let deadline = Instant::now() + Duration::from_secs(5);
        while r.sup.alive() > 0 {
            r.sup.tick(r.at(53.0));
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        r.sup.apply(&[], r.at(54.0));
        assert!(r.sup.statuses().is_empty());
    }

    #[test]
    fn a_missing_cwd_is_a_start_failure_with_backoff() {
        let mut c = cfg();
        c.crash_loop_restarts = 2;
        let mut r = Rig::new(c);
        let mut s = svc("s", "true", "always");
        s.spec.cwd = Some("/nonexistent/axe-test".into());
        r.sup.apply(&[s], r.at(0.0));
        let n = r.sup.tick(r.at(0.0));
        assert!(n.is_empty());
        let st = r.state("b/s");
        assert_eq!(st.state, "backoff");
        assert!(st.error.unwrap().contains("not a directory"));
        let n = r.sup.tick(r.at(1.0));
        assert_eq!(n.len(), 1);
        assert!(n[0].text.contains("not a directory"), "{}", n[0].text);
    }

    /// Wall time at fake instant 0: 30 s past a minute.
    const E0: i64 = 1_800_000_030;

    fn cron(command: &str, timeout: u64) -> Service {
        let mut s = svc("s", command, "never");
        s.spec.schedule = Some(crate::service::Schedule {
            cron: "* * * * *".into(),
            timezone: "UTC".into(),
            timeout_seconds: timeout,
            catch_up: true,
        });
        // Declared long before: catch-up reads the last run, then this.
        s.updated_at = crate::bot::ts(E0 - 15);
        s
    }

    fn timed(cfg: Cfg) -> Rig {
        let mut r = Rig::new(cfg);
        r.sup = Supervisor::new(
            r.sup.cfg.clone(),
            r.dir.path().join("logs"),
            r.dir.path().join("runs"),
            None,
        )
        .with_clock(r.t0, E0);
        r
    }

    #[test]
    fn a_scheduled_service_runs_at_each_cron_time_and_records_the_run() {
        let mut r = timed(cfg());
        r.sup
            .apply(&[cron("echo run-$AXE_SERVICE; exit 4", 60)], r.at(0.0));
        r.sup.tick(r.at(0.0));
        let st = r.state("b/s");
        assert_eq!(st.state, "scheduled");
        assert_eq!(st.next_run_at, Some(crate::bot::ts(E0 + 30)));
        r.sup.tick(r.at(29.0));
        assert_eq!(r.state("b/s").state, "scheduled", "not before its time");
        assert!(r.state("b/s").started_at.is_none());
        // Its time: a run, which exits 4.
        r.sup.tick(r.at(30.0));
        let st = r.state("b/s");
        assert_eq!(st.state, "running");
        assert_eq!(st.started_at, Some(crate::bot::ts(E0 + 30)));
        assert_eq!(st.next_run_at, Some(crate::bot::ts(E0 + 90)));
        r.until(31.0, |s| s.state == "scheduled");
        let st = r.state("b/s");
        assert_eq!(st.last_exit.as_deref(), Some("exit 4"));
        assert_eq!(st.finished_at, Some(crate::bot::ts(E0 + 31)));
        assert_eq!(
            (st.failures, st.restarts),
            (1, 0),
            "a failed run, no restart"
        );
        assert!(r.log().contains("run-s") && r.log().contains("exit 4 ---"));
        // Not retried before the next run time; then it runs again.
        r.sup.tick(r.at(89.0));
        assert_eq!(r.state("b/s").started_at, Some(crate::bot::ts(E0 + 30)));
        r.sup.tick(r.at(90.0));
        assert_eq!(r.state("b/s").started_at, Some(crate::bot::ts(E0 + 90)));
        r.until(90.0, |s| s.state == "scheduled");
        assert_eq!(r.state("b/s").failures, 2);
        assert_eq!(r.log().matches("run-s").count(), 2);
    }

    #[test]
    fn a_scheduled_run_past_its_timeout_is_stopped_and_counts_as_failed() {
        let mut r = timed(cfg());
        r.sup.apply(&[cron("echo begin; sleep 30", 2)], r.at(0.0));
        r.sup.tick(r.at(30.0));
        assert_eq!(r.state("b/s").state, "running");
        r.sup.tick(r.at(31.9));
        assert_eq!(r.state("b/s").state, "running", "within its timeout");
        r.until(32.0, |s| s.state == "scheduled");
        let st = r.state("b/s");
        assert_eq!(
            st.last_exit.as_deref(),
            Some("timeout after 2s (signal 15)")
        );
        assert_eq!(st.failures, 1);
        assert!(st.finished_at.is_some());
        assert!(r.log().contains("timeout after 2s: SIGTERM"), "{}", r.log());
        // A run ignoring SIGTERM is killed after the grace period (10 s).
        let mut r = timed(cfg());
        let cmd = "trap 'echo got-term' TERM; while :; do sleep 0.05; done";
        r.sup.apply(&[cron(cmd, 2)], r.at(0.0));
        r.sup.tick(r.at(30.0));
        std::thread::sleep(Duration::from_millis(100));
        r.sup.tick(r.at(32.0));
        for t in [33.0, 41.9] {
            std::thread::sleep(Duration::from_millis(50));
            r.sup.tick(r.at(t));
            assert_eq!(r.state("b/s").state, "running", "at {t}");
        }
        r.until(42.0, |s| s.state == "scheduled");
        assert_eq!(
            r.state("b/s").last_exit.as_deref(),
            Some("timeout after 2s (signal 9)")
        );
        assert!(r.log().contains("got-term"));
    }

    #[test]
    fn a_run_time_during_a_run_is_skipped_and_a_new_definition_waits_for_its_end() {
        let mut r = timed(cfg());
        let flag = r.dir.path().join("done");
        let cmd = format!("while [ ! -e {} ]; do sleep 0.05; done", flag.display());
        let s = cron(&cmd, 600);
        r.sup.apply(std::slice::from_ref(&s), r.at(0.0));
        r.sup.tick(r.at(30.0));
        let pid = r.state("b/s").pid;
        r.sup.tick(r.at(90.0));
        let st = r.state("b/s");
        assert_eq!((st.state.as_str(), st.pid), ("running", pid));
        assert_eq!(st.next_run_at, Some(crate::bot::ts(E0 + 150)));
        assert!(r.log().contains("skipped the run of"), "{}", r.log());
        // A changed definition: the run goes on, the new one applies after.
        let mut s2 = s.clone();
        s2.spec.command = "echo second".into();
        r.sup.apply(&[s2], r.at(91.0));
        r.sup.tick(r.at(91.0));
        assert_eq!(r.state("b/s").pid, pid);
        std::fs::write(&flag, "").unwrap();
        r.until(92.0, |s| s.state == "scheduled");
        assert_eq!(r.state("b/s").last_exit.as_deref(), Some("exit 0"));
        r.sup.tick(r.at(150.0));
        r.until(150.0, |s| s.state == "scheduled");
        assert!(r.log().contains("second"), "{}", r.log());
    }

    #[test]
    fn a_node_restart_runs_one_catch_up_and_replays_no_missed_run() {
        // The server last stored a run 10 minutes back: ten run times passed
        // while the node was down. One run at once, then the schedule.
        let mut r = timed(cfg());
        let mut s = cron("echo run", 60);
        s.status.started_at = Some(crate::bot::ts(E0 - 600));
        s.status.last_exit = Some("exit 0".into());
        r.sup.apply(&[s.clone()], r.at(0.0));
        assert_eq!(r.state("b/s").started_at, s.status.started_at, "kept");
        r.sup.tick(r.at(0.0));
        assert_eq!(r.state("b/s").started_at, Some(crate::bot::ts(E0)));
        r.until(0.0, |s| s.state == "scheduled");
        for t in [1.0, 10.0, 29.0] {
            r.sup.tick(r.at(t));
        }
        assert_eq!(r.log().matches("start: echo run").count(), 1, "{}", r.log());
        assert_eq!(r.state("b/s").next_run_at, Some(crate::bot::ts(E0 + 30)));
        r.sup.tick(r.at(30.0));
        assert_eq!(r.log().matches("start: echo run").count(), 2);
        r.sup.kill_all();
        // Its last run after the last run time: nothing was missed.
        let mut r = timed(cfg());
        s.status.started_at = Some(crate::bot::ts(E0 - 20));
        r.sup.apply(&[s.clone()], r.at(0.0));
        r.sup.tick(r.at(0.0));
        assert_eq!(r.state("b/s").state, "scheduled");
        // Catch-up off: no run before the next run time.
        let mut r = timed(cfg());
        s.status.started_at = Some(crate::bot::ts(E0 - 600));
        s.spec.schedule.as_mut().unwrap().catch_up = false;
        r.sup.apply(&[s.clone()], r.at(0.0));
        r.sup.tick(r.at(0.0));
        assert_eq!(r.state("b/s").state, "scheduled");
        assert_eq!(r.state("b/s").started_at, Some(crate::bot::ts(E0 - 600)));
        // Never run and declared after the last run time: no catch-up.
        let mut r = timed(cfg());
        let mut s = cron("echo run", 60);
        s.updated_at = crate::bot::ts(E0 - 5);
        r.sup.apply(&[s], r.at(0.0));
        r.sup.tick(r.at(0.0));
        assert_eq!(r.state("b/s").state, "scheduled");
    }

    #[test]
    fn a_draining_node_starts_no_scheduled_run() {
        let mut r = timed(cfg());
        r.sup.apply(&[cron("echo run", 60)], r.at(0.0));
        r.sup.hold_runs(true);
        r.sup.tick(r.at(30.0));
        assert_eq!(r.state("b/s").state, "scheduled");
        assert!(r.state("b/s").started_at.is_none());
        r.sup.hold_runs(false);
        r.sup.tick(r.at(31.0));
        assert_eq!(r.state("b/s").started_at, Some(crate::bot::ts(E0 + 31)));
        r.sup.kill_all();
    }

    #[test]
    fn a_turned_off_scheduled_service_stops_its_run_and_runs_no_more() {
        let mut r = timed(cfg());
        let mut s = cron("sleep 30", 600);
        r.sup.apply(&[s.clone()], r.at(0.0));
        r.sup.tick(r.at(30.0));
        assert_eq!(r.state("b/s").state, "running");
        s.enabled = false;
        r.sup.apply(&[s.clone()], r.at(31.0));
        r.until(31.0, |s| s.state == "stopped");
        let st = r.state("b/s");
        assert_eq!((st.failures, st.next_run_at.clone()), (0, None));
        r.sup.tick(r.at(200.0));
        assert_eq!(r.sup.alive(), 0);
        // On again: waits for its next run time (no catch-up).
        s.enabled = true;
        r.sup.apply(&[s], r.at(201.0));
        r.sup.tick(r.at(201.0));
        let st = r.state("b/s");
        assert_eq!(st.state, "scheduled");
        assert_eq!(st.next_run_at, Some(crate::bot::ts(E0 + 210)));
    }

    #[test]
    fn logs_rotate_past_the_cap_and_tail_reads_across_files() {
        let mut c = cfg();
        c.log_bytes = 4096;
        let mut r = Rig::new(c);
        let cmd = "i=0; while [ $i -lt 1000 ]; do echo line-$i; i=$((i+1)); done; sleep 30";
        r.sup.apply(&[svc("s", cmd, "always")], r.at(0.0));
        r.until(0.0, |s| s.state == "running");
        let logs = r.dir.path().join("logs");
        let path = log_path(&logs, "b", "s");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&path).unwrap().contains("line-999")
            && !path.with_extension("log.1").exists()
        {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        r.sup.tick(r.at(1.0));
        assert!(path.with_extension("log.1").exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= 4096);
        let t = tail(&logs, "b", "s", 5).unwrap();
        assert_eq!(t.lines().count(), 5, "{t}");
        assert!(t.ends_with("line-999"), "{t}");
        assert!(tail(&logs, "b", "../x", 5).is_err());
        assert!(tail(&logs, "b", "none", 5).is_err());
        r.sup.kill_all();
    }
}
