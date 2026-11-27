//! Mid-run task news: notes, spec edits and other changes someone else makes
//! to a task while its session runs. The node installs a Claude Code
//! PostToolUse hook in the run folder (`.claude/settings.json`) that runs
//! `axe task inbox --hook post-tool`, so the news reaches the session after
//! its next tool call, as owner messages reach a bot run (`bot::hook_output`).
use std::path::Path;

use serde_json::json;

use crate::bot::HOOK_CONTEXT_CAP;
use crate::db::Event;

/// Event kinds a running session is told about.
pub const KINDS: [&str; 6] = ["note", "edit", "form", "state", "approval", "repos"];

/// Where the hook keeps the last event id it has seen, in the run folder.
pub const CURSOR_FILE: &str = ".claude/axe-task-inbox";

/// Events the session `session` has not heard about: after `cursor` and after
/// this run's start (the last `run_start`; the session reads the log when it
/// starts), not its own, not the dispatcher's, of a kind in [`KINDS`].
/// Returns them with the new cursor (the last event id).
pub fn news<'a>(
    events: &'a [Event],
    session: Option<&str>,
    cursor: Option<i64>,
) -> (Vec<&'a Event>, Option<i64>) {
    let start = events
        .iter()
        .rev()
        .find(|e| e.kind == "run_start")
        .map_or(0, |e| e.id);
    let after = cursor.unwrap_or(0).max(start);
    let news = events
        .iter()
        .filter(|e| {
            e.id > after
                && e.actor != "dispatcher"
                && (session.is_none() || e.session.as_deref() != session)
                && KINDS.contains(&e.kind.as_str())
        })
        .collect();
    let last = events.iter().map(|e| e.id).max();
    (news, last.max(cursor))
}

/// PostToolUse hook stdout for `news` on task `task`, or `None` for none.
pub fn hook_output(task: i64, news: &[&Event]) -> Option<String> {
    if news.is_empty() {
        return None;
    }
    let mut text = format!(
        "axe task {task}: changed by someone else during this run; it may amend or cancel \
         what you are doing. Re-read the spec (`axe task show {task}`) before continuing:"
    );
    for e in news {
        text.push_str("\n- ");
        text.push_str(&crate::dispatch::format_event(e, None));
    }
    if text.chars().count() > HOOK_CONTEXT_CAP {
        let more = format!("\n… truncated; run `axe task log {task} --full`.");
        let keep = HOOK_CONTEXT_CAP - more.chars().count();
        let end = text.char_indices().nth(keep).map_or(text.len(), |(i, _)| i);
        text.truncate(end);
        text.push_str(&more);
    }
    Some(
        json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":text}})
            .to_string(),
    )
}

/// Claude Code project settings installing the hook; `axe` is the binary path.
pub fn settings_json(axe: &str) -> String {
    let quoted = format!("'{}'", axe.replace('\'', r"'\''"));
    let command = format!("{quoted} task inbox --hook post-tool");
    serde_json::to_string_pretty(&json!({"hooks":{"PostToolUse":[
        {"matcher":"*","hooks":[{"type":"command","command":command}]}
    ]}}))
    .expect("static JSON")
}

/// The cursor stored in run folder `dir`, if any.
pub fn read_cursor(dir: &Path) -> Option<i64> {
    std::fs::read_to_string(dir.join(CURSOR_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Stores `cursor` in run folder `dir` (atomically).
pub fn write_cursor(dir: &Path, cursor: i64) -> std::io::Result<()> {
    let path = dir.join(CURSOR_FILE);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, cursor.to_string())?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: i64, actor: &str, session: Option<&str>, kind: &str, body: &str) -> Event {
        Event {
            id,
            task_id: 7,
            at: format!("t{id}"),
            actor: actor.into(),
            session: session.map(Into::into),
            kind: kind.into(),
            from_value: None,
            to_value: None,
            body: Some(body.into()),
        }
    }

    #[test]
    fn news_is_others_changes_since_this_run_started() {
        let events = vec![
            ev(1, "bot:os", Some("b"), "note", "before the first run"),
            ev(2, "dispatcher", None, "run_start", "{}"),
            ev(3, "bot:os", Some("b"), "note", "between runs"),
            ev(4, "dispatcher", None, "run_start", "{}"),
            ev(5, "agent", Some("me"), "note", "my own note"),
            ev(6, "bot:os", Some("b"), "edit", "--- detail\n+++ detail"),
            ev(7, "dispatcher", None, "labels", ""),
            ev(8, "agent", Some("other"), "note", "another session"),
            ev(9, "bot:os", Some("b"), "workload", ""),
        ];
        let (news, cursor) = news(&events, Some("me"), None);
        let ids: Vec<i64> = news.iter().map(|e| e.id).collect();
        assert_eq!(ids, [6, 8]);
        assert_eq!(cursor, Some(9));
        // Nothing is redelivered past the cursor.
        let (again, cursor) = super::news(&events, Some("me"), cursor);
        assert!(again.is_empty());
        assert_eq!(cursor, Some(9));
    }

    #[test]
    fn hook_output_is_post_tool_context_capped() {
        assert_eq!(hook_output(7, &[]), None);
        let e = ev(
            6,
            "bot:os",
            Some("b"),
            "note",
            &"x".repeat(3 * HOOK_CONTEXT_CAP),
        );
        let out: serde_json::Value = serde_json::from_str(&hook_output(7, &[&e]).unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        let ctx = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            ctx.starts_with("axe task 7: changed by someone else"),
            "{ctx}"
        );
        assert!(ctx.contains("bot:os note: xxx"), "{ctx}");
        assert!(ctx.ends_with("run `axe task log 7 --full`."), "{ctx}");
        assert!(ctx.chars().count() <= HOOK_CONTEXT_CAP);
    }

    #[test]
    fn settings_quote_the_binary() {
        let s: serde_json::Value = serde_json::from_str(&settings_json("/opt/it's/axe")).unwrap();
        let hook = &s["hooks"]["PostToolUse"][0];
        assert_eq!(hook["matcher"], "*");
        assert_eq!(
            hook["hooks"][0]["command"],
            r"'/opt/it'\''s/axe' task inbox --hook post-tool"
        );
    }
}
