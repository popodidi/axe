//! Verbose output for `axe workspace` (`-v`, `-vv`): each build step with its
//! duration at level 1, and each git command and HTTP call at level 2, on
//! stderr. Level 0, the default and the only level outside `axe workspace`,
//! prints nothing.
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

/// Runs `f` as the build step `name`; at level 1 and up prints its duration
/// (and whether it failed) when it ends.
pub fn step<T, E>(name: impl Display, f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    if level() == 0 {
        return f();
    }
    let start = Instant::now();
    let r = f();
    eprintln!(
        "axe: {:>6}  {name}{}",
        secs(start.elapsed()),
        if r.is_err() { "  (failed)" } else { "" }
    );
    r
}

/// A detail line (level 2 and up), with how long it took.
pub fn detail(took: Duration, what: impl Display) {
    if level() >= 2 {
        eprintln!("axe: {:>6}    {what}", secs(took));
    }
}

fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}
