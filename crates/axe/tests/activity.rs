use axe::activity::{clip, from_text, line_activity, MAX_ACTIVITY};

const CLAUDE: &str = include_str!("fixtures/claude-stream.jsonl");
const CODEX: &str = include_str!("fixtures/codex-exec.jsonl");

/// Activity after each prefix of the recorded stream, as the dispatcher sees it
/// while the log grows.
fn progression(fixture: &str) -> Vec<Option<String>> {
    let lines: Vec<&str> = fixture.lines().collect();
    (1..=lines.len())
        .map(|n| from_text(&lines[..n].join("\n")))
        .collect()
}

#[test]
fn claude_stream_json_activity() {
    let got = progression(CLAUDE);
    let s = |v: &str| Some(v.to_string());
    let says = s("says: The directory contains `claude.err`, `claude.jsonl`, and `note.txt`, and `note.txt` contains \"hello\".");
    let result = s("result: The directory contains `claude.err`, `claude.jsonl`, and `note.txt`, and `note.txt` contains \"hello\".");
    assert_eq!(
        got,
        [
            s("session started"),
            s("Bash: ls"),
            s("Bash: ls"), // tool result
            s("Read: /work/fixture/note.txt"),
            s("Read: /work/fixture/note.txt"),
            s("Read: /work/fixture/note.txt"), // thinking_tokens
            s("Read: /work/fixture/note.txt"),
            s("Read: /work/fixture/note.txt"), // empty thinking block
            says,
            result,
        ]
    );
}

#[test]
fn codex_json_activity() {
    let got = progression(CODEX);
    let s = |v: &str| Some(v.to_string());
    assert_eq!(
        got,
        [
            None,
            None,
            s("says: I’ll run `ls`, then read `note.txt`."),
            s("exec: ls"),
            s("exec: ls"),
            s("exec: cat note.txt"),
            s("exec: cat note.txt"),
            s("says: `note.txt` contains “hello”."),
            s("says: `note.txt` contains “hello”."), // turn.completed
        ]
    );
}

#[test]
fn partial_and_noise_lines_are_skipped() {
    let text = format!(
        "{}\nwarning: something on stderr\n{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_",
        CODEX.lines().nth(3).unwrap()
    );
    assert_eq!(from_text(&text).as_deref(), Some("exec: ls"));
    assert_eq!(from_text(""), None);
}

#[test]
fn other_items_and_clipping() {
    let line = |v: &str| line_activity(v);
    assert_eq!(
        line(r#"{"type":"item.completed","item":{"type":"file_change","changes":[{"path":"src/a.rs","kind":"update"},{"path":"b","kind":"add"}]}}"#).as_deref(),
        Some("edit: src/a.rs (+1 more)")
    );
    assert_eq!(
        line(r#"{"type":"item.started","item":{"type":"mcp_tool_call","server":"gh","tool":"search"}}"#).as_deref(),
        Some("mcp: gh.search")
    );
    assert_eq!(
        line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[]}}]}}"#).as_deref(),
        Some("TodoWrite")
    );
    assert_eq!(
        line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"fn  main\n"}}]}}"#).as_deref(),
        Some("Grep: fn main")
    );
    let long = clip(&"é".repeat(200));
    assert_eq!(long.chars().count(), MAX_ACTIVITY + 1);
    assert!(long.ends_with('…'));
}
