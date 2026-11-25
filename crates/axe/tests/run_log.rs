use axe::run_log::{note_growth, read, run_files, times_path, MAX_RUN_FILE, PAGE_BYTES};
use std::io::Write;

#[test]
fn partial_reconnect_and_finished_tail() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(f, "one\n{{\"type\":").unwrap();
    let p = read(f.path(), None, None, None, false).unwrap();
    assert_eq!(p.lines.len(), 1);
    assert_eq!(p.end, 4);
    write!(f, "\"unknown\"}}\nplain\npartial").unwrap();
    let p = read(f.path(), Some(p.end), None, Some(&p.generation), false).unwrap();
    assert_eq!(p.lines.len(), 2);
    assert_eq!(p.lines[0].text, "{\"type\":\"unknown\"}");
    let retry = read(f.path(), Some(4), None, Some(&p.generation), false).unwrap();
    assert_eq!(retry.end, p.end);
    let tail = read(f.path(), Some(p.end), None, Some(&p.generation), true).unwrap();
    assert_eq!(tail.lines[0].text, "partial");
    assert_eq!(tail.end, tail.size);
    assert!(read(f.path(), Some(tail.end + 1), None, None, false).is_err());
    assert!(read(f.path(), None, None, Some("other"), false).is_err());
}

#[test]
fn recent_and_older_pages_cover_large_log_once() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    for i in 0..10000 {
        writeln!(f, "{i:05} {}", "x".repeat(100)).unwrap();
    }
    let mut p = read(f.path(), None, None, None, true).unwrap();
    assert!(p.start > 0);
    let mut text: Vec<String> = p.lines.iter().map(|l| l.text.clone()).collect();
    while p.start > 0 {
        let next = read(f.path(), None, Some(p.start), Some(&p.generation), true).unwrap();
        assert!(next.start < p.start);
        assert_eq!(next.end, p.start);
        assert!(next.lines.len() <= 1000);
        assert!(next.end - next.start <= PAGE_BYTES);
        let mut older: Vec<String> = next.lines.iter().map(|l| l.text.clone()).collect();
        older.extend(text);
        text = older;
        p = next;
    }
    assert_eq!(text.len(), 10000);
    for (i, line) in text.iter().enumerate() {
        assert!(line.starts_with(&format!("{i:05} ")));
    }
    let mut cursor = 0;
    let mut count = 0;
    while cursor < p.size {
        let next = read(f.path(), Some(cursor), None, None, true).unwrap();
        assert!(next.end > cursor);
        count += next.lines.len();
        cursor = next.end;
    }
    assert_eq!(count, 10000);
}

#[test]
fn oversized_and_empty_partial_records_are_bounded() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(f, "partial").unwrap();
    let p = read(f.path(), None, None, None, false).unwrap();
    assert_eq!(p.end, 0);
    assert!(p.lines.is_empty());
    write!(f, "{}\nnext\n", "x".repeat(PAGE_BYTES as usize * 3)).unwrap();
    let mut p = read(f.path(), None, None, None, false).unwrap();
    while p.start > 0 {
        let next = read(f.path(), None, Some(p.start), None, false).unwrap();
        assert!(next.start < p.start);
        assert_eq!(next.end, p.start);
        assert!(next.end - next.start <= PAGE_BYTES);
        p = next;
    }
    let p = read(f.path(), Some(0), None, None, false).unwrap();
    assert_eq!(p.end, PAGE_BYTES);
    assert!(p.lines[0].fragment);
}

#[test]
fn growth_records_time_lines_and_older_logs_have_none() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("7-1.log");
    std::fs::write(&log, "one\ntwo\n").unwrap();
    let p = read(&log, None, None, None, false).unwrap();
    assert!(p.lines.iter().all(|l| l.at.is_none()));
    assert!(!serde_json::to_string(&p).unwrap().contains("\"at\""));
    note_growth(&log, 4);
    let first = read(&log, None, None, None, false).unwrap();
    let at = first.lines[0].at.unwrap();
    assert!(at > 1_700_000_000_000);
    assert_eq!(first.lines[1].at, None);
    std::thread::sleep(std::time::Duration::from_millis(5));
    note_growth(&log, 8);
    let p = read(&log, None, None, None, false).unwrap();
    assert_eq!(p.lines[0].at, Some(at));
    assert!(p.lines[1].at.unwrap() > at);
    assert_eq!(times_path(&log), dir.path().join("7-1.times"));
    // A damaged record costs only its own line.
    std::fs::write(times_path(&log), "garbage\n8 42\n").unwrap();
    let p = read(&log, None, None, None, false).unwrap();
    assert_eq!(p.lines[0].at, Some(42));
}

#[test]
fn run_files_read_prompts_beside_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("9.log");
    std::fs::write(&log, "{\"type\":\"system\",\"model\":\"m\"}\nrest\n").unwrap();
    let f = run_files(&log, None);
    assert_eq!(f["prompt"]["unavailable"], "not recorded for this run");
    assert_eq!(f["system_prompt"]["unavailable"], "not recorded for this run");
    assert_eq!(f["first_line"], "{\"type\":\"system\",\"model\":\"m\"}");
    assert_eq!(f["times"], false);
    // An older bot run falls back to the bot's current system prompt.
    let current = dir.path().join("system.md");
    std::fs::write(&current, "current system").unwrap();
    let f = run_files(&log, Some(&current));
    assert_eq!(f["system_prompt"]["text"], "current system");
    assert_eq!(f["system_prompt"]["source"], "current");
    std::fs::write(dir.path().join("9.prompt.md"), "wake #1").unwrap();
    std::fs::write(dir.path().join("9.system.md"), "x".repeat(MAX_RUN_FILE as usize + 1)).unwrap();
    note_growth(&log, 3);
    let f = run_files(&log, Some(&current));
    assert_eq!(f["prompt"]["text"], "wake #1");
    assert_eq!(f["prompt"]["truncated"], false);
    assert_eq!(f["system_prompt"]["source"], "run");
    assert_eq!(f["system_prompt"]["truncated"], true);
    assert_eq!(f["system_prompt"]["text"].as_str().unwrap().len(), MAX_RUN_FILE as usize);
    assert_eq!(f["times"], true);
}
