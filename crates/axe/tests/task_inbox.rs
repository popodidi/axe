//! A note or spec edit made by someone else while a task runs reaches the
//! running Claude Code session through the PostToolUse hook the node installs.
mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{Server, TOKEN};
use serde_json::Value;

/// Fake Claude Code agent: notes once itself, copies the run folder's
/// settings, then runs the PostToolUse hook command twice, each after a
/// hold file is removed.
const AGENT: &str = r#"
out="$INBOXTEST_OUT"
hold() { while [ -e "$out/$1" ]; do sleep 0.05; done; }
"$AXE_BIN" task note "$AXE_TASK" "my own progress"
cp .claude/settings.json "$out/settings"
touch "$out/ready"
hold hold1
"$AXE_BIN" task inbox --hook post-tool > "$out/post1" < /dev/null
hold hold2
echo '{"tool_name":"Bash"}' | "$AXE_BIN" task inbox --hook post-tool > "$out/post2"
"$AXE_BIN" task set "$AXE_TASK" --state done
"#;

fn write_config(dir: &Path) -> std::path::PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg = format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

[worktrees]

[agents.fake]
cmd = ["sh", "-c", {agent}, "fake", "{{prompt}}"]
session = "preset"
harness = "claude-code"

[[rule]]
match = {{ project = "a" }}
agents = ["fake"]
"#,
        ws = ws.display(),
        logs = dir.join("logs").display(),
        agent = serde_json::to_string(AGENT).unwrap(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, cfg).unwrap();
    path
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_note_added_mid_run_reaches_the_running_session() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    for h in ["hold1", "hold2"] {
        std::fs::write(out.join(h), "").unwrap();
    }
    let srv = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    srv.ok(&[
        "task",
        "add",
        "--project",
        "a",
        "--title",
        "t",
        "--detail",
        "old scope",
    ]);
    // A note before the run is history the session reads itself.
    srv.ok(&["task", "note", "1", "before the run"]);
    let outs = out.display().to_string();
    let _d = srv.dispatcher(&[], &[("INBOXTEST_OUT", &outs)]);
    wait_for("run start", 30, || out.join("ready").exists());

    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("settings")).unwrap()).unwrap();
    let hook = &settings["hooks"]["PostToolUse"][0];
    assert_eq!(hook["matcher"], "*");
    let cmd = hook["hooks"][0]["command"].as_str().unwrap();
    assert!(cmd.ends_with(" task inbox --hook post-tool"), "{cmd}");

    // Someone else notes and edits the spec while the session runs.
    srv.ok(&[
        "task",
        "note",
        "1",
        "form already sent; do not send another",
    ]);
    srv.ok(&["task", "set", "1", "--detail", "new scope"]);
    std::fs::remove_file(out.join("hold1")).unwrap();
    wait_for("first hook", 20, || {
        std::fs::metadata(out.join("post1")).is_ok_and(|m| m.len() > 0)
    });
    let post: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("post1")).unwrap()).unwrap();
    assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let ctx = post["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.starts_with("axe task 1: changed by someone else"),
        "{ctx}"
    );
    assert!(
        ctx.contains("note: form already sent; do not send another"),
        "{ctx}"
    );
    assert!(
        ctx.contains("-old scope") && ctx.contains("+new scope"),
        "{ctx}"
    );
    assert!(!ctx.contains("before the run"), "{ctx}");
    assert!(!ctx.contains("my own progress"), "{ctx}");

    // Delivered once: the next tool call hears nothing new.
    std::fs::remove_file(out.join("hold2")).unwrap();
    wait_for("task done", 20, || {
        srv.ok(&["task", "show", "1"]).contains("state: done")
    });
    assert_eq!(std::fs::read_to_string(out.join("post2")).unwrap(), "");
}
