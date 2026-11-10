use axe::run_log::{read, PAGE_BYTES};
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
