//! Short "what is the agent doing" text from agent JSONL output
//! (claude `--output-format stream-json`, codex `exec --json`).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// Longest activity text stored on a run.
pub const MAX_ACTIVITY: usize = 120;
/// Bytes read from the end of a run log per tick.
const TAIL_BYTES: u64 = 64 * 1024;

/// Activity of the latest recognized JSONL item in `text`. Partial or non-JSON
/// lines (a line still being written, stderr noise) are skipped.
pub fn from_text(text: &str) -> Option<String> {
    text.lines().rev().find_map(line_activity)
}

/// Reads the tail of a log file and returns its latest activity.
pub fn from_log(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    from_text(&String::from_utf8_lossy(&buf))
}

/// Activity for one JSONL line, if it is a recognized item.
pub fn line_activity(line: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    let text = match v.get("type")?.as_str()? {
        // claude stream-json
        "assistant" => claude_message(&v["message"])?,
        "result" => match v["result"].as_str().filter(|r| !r.trim().is_empty()) {
            Some(r) => format!("result: {r}"),
            None => format!("result: {}", v["subtype"].as_str().unwrap_or("done")),
        },
        "system" if v["subtype"] == "init" => "session started".into(),
        // codex exec --json
        "item.started" | "item.updated" | "item.completed" => codex_item(&v["item"])?,
        "turn.failed" => format!(
            "turn failed: {}",
            v["error"]["message"].as_str().unwrap_or("")
        ),
        "error" => format!("error: {}", v["message"].as_str().unwrap_or("")),
        _ => return None,
    };
    Some(clip(&text))
}

/// The last tool call or non-empty text block of a claude assistant message.
fn claude_message(msg: &Value) -> Option<String> {
    msg["content"]
        .as_array()?
        .iter()
        .rev()
        .find_map(|c| match c["type"].as_str()? {
            "tool_use" => {
                let name = c["name"].as_str()?;
                Some(match tool_arg(&c["input"]) {
                    Some(arg) => format!("{name}: {arg}"),
                    None => name.to_string(),
                })
            }
            "text" => nonempty(&c["text"]).map(|t| format!("says: {t}")),
            "thinking" => nonempty(&c["thinking"]).map(|t| format!("thinking: {t}")),
            _ => None,
        })
}

/// The most descriptive string argument of a tool call.
fn tool_arg(input: &Value) -> Option<String> {
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "skill",
        "description",
        "prompt",
    ];
    KEYS.iter()
        .find_map(|k| nonempty(&input[*k]))
        .or_else(|| input.as_object()?.values().find_map(nonempty))
}

fn codex_item(item: &Value) -> Option<String> {
    Some(match item["type"].as_str()? {
        "command_execution" => format!("exec: {}", strip_shell(item["command"].as_str()?)),
        "agent_message" => format!("says: {}", nonempty(&item["text"])?),
        "reasoning" => format!("thinking: {}", nonempty(&item["text"])?),
        "file_change" => {
            let changes = item["changes"].as_array()?;
            let first = changes.first()?["path"].as_str()?;
            match changes.len() {
                1 => format!("edit: {first}"),
                n => format!("edit: {first} (+{} more)", n - 1),
            }
        }
        "mcp_tool_call" => format!(
            "mcp: {}.{}",
            item["server"].as_str().unwrap_or("?"),
            item["tool"].as_str().unwrap_or("?")
        ),
        "web_search" => format!("search: {}", item["query"].as_str().unwrap_or("")),
        "todo_list" => format!(
            "todo: {} items",
            item["items"].as_array().map_or(0, Vec::len)
        ),
        "error" => format!("error: {}", item["message"].as_str().unwrap_or("")),
        _ => return None,
    })
}

/// `/bin/bash -lc 'cat x'` -> `cat x`.
fn strip_shell(cmd: &str) -> String {
    for prefix in ["/bin/bash -lc ", "bash -lc ", "/bin/sh -c ", "sh -c "] {
        if let Some(rest) = cmd.strip_prefix(prefix) {
            let unquoted = rest
                .strip_prefix('\'')
                .and_then(|r| r.strip_suffix('\''))
                .or_else(|| rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')));
            return unquoted.unwrap_or(rest).to_string();
        }
    }
    cmd.to_string()
}

fn nonempty(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Collapses whitespace to single spaces and cuts to [`MAX_ACTIVITY`] chars.
pub fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(MAX_ACTIVITY) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}
