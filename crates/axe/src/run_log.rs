//! Bounded, stateless JSONL pages. Cursors are byte offsets in one file generation.
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

pub const PAGE_BYTES: u64 = 256 * 1024;
#[derive(Serialize)]
pub struct Line {
    pub start: u64,
    pub end: u64,
    pub text: String,
    pub fragment: bool,
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
        });
        cursor = read_end;
    }
    start = lines.front().map_or(start, |l| l.start);
    Ok(Page {
        generation: identity,
        start,
        end: cursor,
        size,
        lines: lines.into_iter().collect(),
    })
}
