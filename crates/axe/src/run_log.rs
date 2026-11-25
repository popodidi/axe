//! Bounded, stateless JSONL pages. Cursors are byte offsets in one file generation.
//!
//! Beside a run's log the dispatcher keeps (all optional, named after the log):
//! - `<log>.times`: one `<size> <unix ms>` line each time it saw the log grow,
//!   so a page can say when each line arrived ([`note_growth`]). Logs from
//!   before it have none, and their lines carry no `at`.
//! - `<log>.prompt.md`: the user prompt the run was started with.
//! - `<log>.system.md`: the system prompt or run instructions the run was
//!   started with ([`run_files`]).
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};

pub const PAGE_BYTES: u64 = 256 * 1024;
#[derive(Serialize)]
pub struct Line {
    pub start: u64,
    pub end: u64,
    pub text: String,
    pub fragment: bool,
    /// When the dispatcher first saw the whole line (unix ms, within one
    /// dispatcher tick); absent when the log has no `.times` record of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}
#[derive(Serialize)]
pub struct Page {
    pub generation: String,
    pub start: u64,
    pub end: u64,
    pub size: u64,
    pub lines: Vec<Line>,
}

pub fn read(
    path: &Path,
    after: Option<u64>,
    before: Option<u64>,
    generation: Option<&str>,
    finished: bool,
) -> Result<Page> {
    let mut f = File::open(path)?;
    let m = f.metadata()?;
    if !m.is_file() {
        bail!("log is not a regular file");
    }
    let size = m.len();
    let identity = format!("{}:{}", m.dev(), m.ino());
    if generation.is_some_and(|g| g != identity) {
        bail!("log replaced; reopen this run");
    }
    if after.is_some_and(|n| n > size) || before.is_some_and(|n| n > size) {
        bail!("log truncated or invalid cursor; reopen this run");
    }
    let backwards = after.is_none();
    let limit = before.unwrap_or(size);
    let mut start = after.unwrap_or(limit.saturating_sub(PAGE_BYTES));
    let read_end = if backwards {
        limit
    } else {
        size.min(start.saturating_add(PAGE_BYTES))
    };
    // Include the preceding byte so a page beginning at a line boundary keeps it.
    let seek = start.saturating_sub(1);
    f.seek(SeekFrom::Start(seek))?;
    let mut bytes = Vec::new();
    f.take(read_end - seek).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != read_end - seek {
        bail!("log changed while reading; reopen this run");
    }
    let mut skip = (start - seek) as usize;
    let mut fragment = start > 0 && bytes.first() != Some(&b'\n');
    if backwards && fragment {
        if let Some(n) = bytes[skip..].iter().position(|b| *b == b'\n') {
            if skip + n + 1 < bytes.len() {
                skip += n + 1;
                start = seek + skip as u64;
                fragment = false;
            }
        }
    }
    let mut lines = VecDeque::new();
    let mut pos = skip;
    let mut cursor = start;
    for (i, b) in bytes.iter().enumerate().skip(skip) {
        if *b == b'\n' {
            let end = seek + i as u64 + 1;
            lines.push_back(Line {
                start: cursor,
                end,
                text: String::from_utf8_lossy(&bytes[pos..i]).into_owned(),
                fragment,
                at: None,
            });
            cursor = end;
            pos = i + 1;
            fragment = false;
            if lines.len() > 1000 {
                lines.pop_front();
            }
            if !backwards && lines.len() == 1000 {
                break;
            }
        }
    }
    if lines.len() < 1000
        && pos < bytes.len()
        && (finished && read_end == size || read_end - cursor >= PAGE_BYTES || before.is_some())
    {
        lines.push_back(Line {
            start: cursor,
            end: read_end,
            text: String::from_utf8_lossy(&bytes[pos..]).into_owned(),
            fragment: fragment || !(finished && read_end == size),
            at: None,
        });
        cursor = read_end;
    }
    start = lines.front().map_or(start, |l| l.start);
    let times = read_times(path);
    for l in lines.iter_mut() {
        l.at = seen_at(&times, l.end);
    }
    Ok(Page {
        generation: identity,
        start,
        end: cursor,
        size,
        lines: lines.into_iter().collect(),
    })
}

/// The growth record beside `log`.
pub fn times_path(log: &Path) -> PathBuf {
    log.with_extension("times")
}

/// Records that `log` had grown to `len` bytes by now; best effort, since a
/// missing record only costs the lines their times.
pub fn note_growth(log: &Path, len: u64) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(times_path(log))
        .and_then(|mut f| writeln!(f, "{len} {ms}"));
}

/// `(size, unix ms)` records in file order; unreadable lines are skipped.
fn read_times(log: &Path) -> Vec<(u64, i64)> {
    std::fs::read_to_string(times_path(log))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (size, ms) = l.split_once(' ')?;
            Some((size.parse().ok()?, ms.parse().ok()?))
        })
        .collect()
}

/// The time of the first record that saw the log reach `end` bytes.
fn seen_at(times: &[(u64, i64)], end: u64) -> Option<i64> {
    times.iter().find(|(size, _)| *size >= end).map(|t| t.1)
}

/// Longest prompt or first log line [`run_files`] returns, in bytes.
pub const MAX_RUN_FILE: u64 = 192 * 1024;

/// The files recorded beside a run's log, for the run page: the user prompt,
/// the system prompt (or, for an older run, `fallback_system` marked as
/// current rather than per run) and the log's first line (the harness's
/// init record, which names its model and version). Each is
/// `{"text","truncated","source"}`, or `{"unavailable": why}`.
pub fn run_files(log: &Path, fallback_system: Option<&Path>) -> Value {
    let file = |path: &Path, source: &str| -> Value {
        let Ok(f) = File::open(path) else {
            return json!({"unavailable":"not recorded for this run"});
        };
        let size = f.metadata().map_or(0, |m| m.len());
        let mut bytes = Vec::new();
        if f.take(MAX_RUN_FILE).read_to_end(&mut bytes).is_err() {
            return json!({"unavailable":"unreadable"});
        }
        json!({"text":String::from_utf8_lossy(&bytes),"truncated":size > MAX_RUN_FILE,
            "bytes":size,"source":source})
    };
    let system_path = log.with_extension("system.md");
    let system = match fallback_system {
        Some(current) if !system_path.exists() && current.exists() => file(current, "current"),
        _ => file(&system_path, "run"),
    };
    let first_line = File::open(log).ok().and_then(|f| {
        let mut bytes = Vec::new();
        f.take(MAX_RUN_FILE).read_to_end(&mut bytes).ok()?;
        let end = bytes.iter().position(|b| *b == b'\n')?;
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    });
    json!({
        "prompt": file(&log.with_extension("prompt.md"), "run"),
        "system_prompt": system,
        "first_line": first_line,
        "times": times_path(log).exists(),
    })
}
