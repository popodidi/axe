//! Controlled resource samples and a fake monotonic clock for admission tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use axe::admission::{Clock, CpuTimes, Gate, Memory, MonotonicClock, Probe};
use axe::config::Resources;

pub const GIB: u64 = 1 << 30;
/// Clock ticks per second in the fake `/proc/stat`.
const HZ: u64 = 100;

#[derive(Clone, Default)]
pub struct FakeClock(Arc<AtomicU64>);

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

pub struct Readings {
    pub disk: Result<u64, String>,
    /// MemAvailable as a percent of MemTotal (1,000,000 KiB, so percents are exact).
    pub memory: Result<f64, String>,
    pub cpu: CpuTimes,
    pub cpu_error: Option<String>,
    /// CPU busy percent applied while the clock advances.
    pub busy: f64,
    /// Calls per probe method, in order.
    pub calls: Vec<&'static str>,
    /// Runs observed by `on_sample` at each disk read (sample sequencing).
    pub observed: Vec<i64>,
    pub on_sample: Option<Box<dyn FnMut() -> i64 + Send>>,
    /// Disk reading as a function of the value `on_sample` observed.
    pub disk_by_observed: Option<Box<dyn Fn(i64) -> u64 + Send>>,
    /// Accrue one second of CPU time per read (for real-clock tests).
    pub auto_cpu: bool,
}

/// Shared handle to the fake guest: change readings, advance time.
#[derive(Clone)]
pub struct Rig {
    pub clock: FakeClock,
    pub r: Arc<Mutex<Readings>>,
    pub log: Arc<Mutex<Vec<String>>>,
}

struct FakeProbe(Arc<Mutex<Readings>>);

impl Probe for FakeProbe {
    fn disk_available(&mut self, _: &Path) -> Result<u64> {
        let mut r = self.0.lock().unwrap();
        r.calls.push("disk");
        if let Some(f) = r.on_sample.as_mut() {
            let n = f();
            r.observed.push(n);
            if let Some(d) = &r.disk_by_observed {
                return Ok(d(n));
            }
        }
        r.disk.clone().map_err(|e| anyhow!(e))
    }
    fn memory(&mut self) -> Result<Memory> {
        let mut r = self.0.lock().unwrap();
        r.calls.push("memory");
        let total = 1_000_000;
        r.memory
            .clone()
            .map(|pct| Memory {
                available_kib: (pct / 100.0 * total as f64).round() as u64,
                total_kib: total,
            })
            .map_err(|e| anyhow!(e))
    }
    fn cpu(&mut self) -> Result<CpuTimes> {
        let mut r = self.0.lock().unwrap();
        r.calls.push("cpu");
        if r.auto_cpu {
            accrue(&mut r, 1);
        }
        match &r.cpu_error {
            Some(e) => Err(anyhow!(e.clone())),
            None => Ok(r.cpu),
        }
    }
}

impl Default for Rig {
    fn default() -> Self {
        Rig {
            clock: FakeClock::default(),
            r: Arc::new(Mutex::new(Readings {
                disk: Ok(100 * GIB),
                memory: Ok(50.0),
                cpu: CpuTimes::default(),
                cpu_error: None,
                busy: 10.0,
                calls: Vec::new(),
                observed: Vec::new(),
                on_sample: None,
                disk_by_observed: None,
                auto_cpu: false,
            })),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Rig {
    pub fn gate(&self, cfg: Resources) -> Gate {
        let log = Arc::clone(&self.log);
        Gate::with(
            cfg,
            PathBuf::from("/fake/workspace"),
            Box::new(FakeProbe(Arc::clone(&self.r))),
            Box::new(self.clock.clone()),
        )
        .with_emit(Box::new(move |l| log.lock().unwrap().push(l.to_string())))
    }

    /// The same fake guest on the real monotonic clock (live dispatcher tests).
    pub fn gate_real_clock(&self, cfg: Resources) -> Gate {
        self.r.lock().unwrap().auto_cpu = true;
        let log = Arc::clone(&self.log);
        Gate::with(
            cfg,
            PathBuf::from("/fake/workspace"),
            Box::new(FakeProbe(Arc::clone(&self.r))),
            Box::new(MonotonicClock::default()),
        )
        .with_emit(Box::new(move |l| log.lock().unwrap().push(l.to_string())))
    }

    /// Advances the clock, accruing CPU time at the current busy percent.
    pub fn advance(&self, secs: u64) {
        self.clock.0.fetch_add(secs * 1000, Ordering::SeqCst);
        accrue(&mut self.r.lock().unwrap(), secs);
    }

    pub fn disk_calls(&self) -> usize {
        let r = self.r.lock().unwrap();
        r.calls.iter().filter(|c| **c == "disk").count()
    }

    pub fn set_disk(&self, v: Result<u64, &str>) {
        self.r.lock().unwrap().disk = v.map_err(String::from);
    }
    pub fn set_memory(&self, v: Result<f64, &str>) {
        self.r.lock().unwrap().memory = v.map_err(String::from);
    }
    pub fn set_busy(&self, pct: f64) {
        self.r.lock().unwrap().busy = pct;
    }
    pub fn set_cpu_error(&self, e: Option<&str>) {
        self.r.lock().unwrap().cpu_error = e.map(String::from);
    }
    pub fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Periodic samples every `step` seconds for `secs` seconds.
    pub fn run(&self, gate: &mut Gate, secs: u64, step: u64) {
        for _ in 0..secs / step {
            self.advance(step);
            gate.tick();
        }
    }
}

fn accrue(r: &mut Readings, secs: u64) {
    let total = secs * HZ;
    let b = (r.busy / 100.0 * total as f64).round() as u64;
    // Busy time split across user and system; idle time across idle and iowait.
    let t = &mut r.cpu.fields;
    t[0] += b / 2;
    t[2] += b - b / 2;
    t[3] += (total - b) / 2;
    t[4] += (total - b) - (total - b) / 2;
}

/// Approved defaults: 15/20 GiB, 15/20 %, 90/80 % over 60 s, sampled every 10 s.
pub fn defaults() -> Resources {
    Resources::default()
}
