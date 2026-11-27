//! A bot's primary and backup runtimes: the backup runs it, with a fresh
//! session, only while the primary is unhealthy, and the bot goes back to
//! its primary (fresh again) once that is healthy. Changing the primary is
//! the owner's `axe bot edit --agent`.
mod common;

use std::path::Path;

use common::{Server, TOKEN};
use serde_json::{json, Value};

/// Fake CLIs: record their arguments (NUL-separated) per bot run. `codex`
/// prints a `thread.started` line like `codex exec --json`.
const FAKE_CODEX: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\0' "$a"; done > "$BOTTEST_OUT/args-$AXE_BOT_RUN"
echo '{"type":"thread.started","thread_id":"thread-'"$AXE_BOT_RUN"'"}'
"#;
const FAKE_CLAUDE: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\0' "$a"; done > "$BOTTEST_OUT/args-$AXE_BOT_RUN"
"#;

/// A node with a codex-1 and a claude-1 runtime, each on its own login and
/// ok, and bot `b` with codex-1 as its primary.
fn rig(dir: &Path) -> Server {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, script) in [("codex", FAKE_CODEX), ("claude", FAKE_CLAUDE)] {
        let p = bin.join(name);
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (name, harness, agent) in [
        ("codex-1", "codex", "codex"),
        ("claude-1", "claude-code", "claude"),
    ] {
        let rt = dir.join("runtimes").join(name);
        std::fs::create_dir_all(rt.join("home")).unwrap();
        std::fs::write(
            rt.join("runtime.toml"),
            format!(
                "harness = \"{harness}\"\nagent = \"{agent}\"\nlogin_dir = \"{}\"\nprobe = []\n",
                rt.join("home").display()
            ),
        )
        .unwrap();
        set_state(dir, name, "ok");
    }
    std::fs::create_dir_all(dir.join("ws")).unwrap();
    std::fs::create_dir_all(dir.join("out")).unwrap();
    let cfg = format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

[worktrees]

[runtimes]
dir = "{runtimes}"

[agents.codex]
cmd = ["{bin}/codex", "exec", "--json", "{{prompt}}"]
resume_cmd = ["{bin}/codex", "exec", "resume", "--json", "{{session_id}}", "{{prompt}}"]
session = "jsonl:thread_id"
harness = "codex"

[agents.claude]
cmd = ["{bin}/claude", "-p", "--session-id", "{{session_id}}", "{{prompt}}"]
resume_cmd = ["{bin}/claude", "-p", "--resume", "{{session_id}}", "{{prompt}}"]
session = "preset"
harness = "claude-code"
"#,
        ws = dir.join("ws").display(),
        logs = dir.join("logs").display(),
        runtimes = dir.join("runtimes").display(),
        bin = bin.display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, cfg).unwrap();
    let srv = Server::start_with(dir, &path, Some(TOKEN), &[], &[]);
    let f = dir.join("bot.json");
    std::fs::write(
        &f,
        json!({"name":"b","project":"a","agent":"codex-1","notes":"KEEP"}).to_string(),
    )
    .unwrap();
    srv.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    srv
}

/// Sets runtime `name`'s health: `ok`, or `limited` for a day.
fn set_state(dir: &Path, name: &str, state: &str) {
    let body = match state {
        "ok" => json!({"state": "ok"}),
        s => json!({"state": s, "until": 4_000_000_000i64}),
    };
    std::fs::write(
        dir.join("runtimes").join(name).join("state.json"),
        body.to_string(),
    )
    .unwrap();
}

fn dispatch_once(srv: &Server, dir: &Path) {
    let out = dir.join("out").display().to_string();
    let o = srv.dispatcher(&["--once"], &[("BOTTEST_OUT", &out)]).wait();
    assert!(o.status.success(), "{o:?}");
}

fn runs(srv: &Server) -> Vec<Value> {
    let mut r: Vec<Value> = serde_json::from_value(srv.get("/api/bot-runs?bot=b")).unwrap();
    r.sort_by_key(|r| r["id"].as_i64());
    r
}

/// The `bot_run_start` event of run `run`: its mode and rule.
fn start(srv: &Server, run: i64) -> Value {
    let log = srv.ok(&["bot", "log", "b", "--full", "--json"]);
    let events: Vec<Value> = serde_json::from_str(&log).unwrap();
    events
        .iter()
        .filter(|e| e["kind"] == "run_start")
        .map(|e| serde_json::from_str::<Value>(e["body"].as_str().unwrap()).unwrap())
        .find(|b| b["run"] == run)
        .unwrap_or_else(|| panic!("no run_start for run {run}: {log}"))
}

#[test]
fn a_bot_fails_over_to_its_backup_and_returns_to_its_primary() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = rig(dir);
    let out = srv.ok(&[
        "bot",
        "edit",
        "b",
        "--backup",
        "claude-1",
        "--why",
        "owner msg #5052",
    ]);
    assert!(out.contains("backup runtime claude-1"), "{out}");
    assert_eq!(srv.get("/api/bots/b")["backup"], "claude-1");

    // Primary healthy: the run is on codex-1.
    dispatch_once(&srv, dir);
    let r = runs(&srv);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0]["agent"], "codex-1");
    assert_eq!(start(&srv, 1)["rule"], "bot primary");
    assert_eq!(srv.get("/api/bots/b")["session_agent"], "codex-1");

    // Primary limited: the next wake runs on the backup, in a fresh session.
    set_state(dir, "codex-1", "limited");
    srv.ok(&["bot", "wake", "b", "--now", "--note", "while down"]);
    dispatch_once(&srv, dir);
    let r = runs(&srv);
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[1]["agent"], "claude-1");
    let s = start(&srv, 2);
    assert_eq!(s["mode"], "fresh", "{s}");
    assert_eq!(s["rule"], "bot backup (codex-1 unhealthy)", "{s}");
    let b = srv.get("/api/bots/b");
    assert_eq!(b["session_agent"], "claude-1", "{b}");
    assert_eq!(b["notes"], "KEEP", "notes carry over");

    // Still down: the backup's session is resumed.
    srv.ok(&["bot", "wake", "b", "--now", "--note", "still down"]);
    dispatch_once(&srv, dir);
    let s = start(&srv, 3);
    assert_eq!(
        (&s["agent"], &s["mode"]),
        (&json!("claude-1"), &json!("resume")),
        "{s}"
    );

    // Primary healthy again: back to codex-1, in a fresh session.
    set_state(dir, "codex-1", "ok");
    srv.ok(&["bot", "wake", "b", "--now", "--note", "back"]);
    dispatch_once(&srv, dir);
    let s = start(&srv, 4);
    assert_eq!(
        (&s["agent"], &s["mode"]),
        (&json!("codex-1"), &json!("fresh")),
        "{s}"
    );
    assert_eq!(s["rule"], "bot primary");
    assert_eq!(srv.get("/api/bots/b")["session_agent"], "codex-1");
    let log = srv.ok(&["bot", "log", "b", "--full"]);
    assert!(
        log.contains("none -> claude-1"),
        "the backup is recorded: {log}"
    );
}

#[test]
fn a_bot_with_no_healthy_runtime_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = rig(dir);
    // No backup, primary limited: no run, even though claude-1 is healthy.
    set_state(dir, "codex-1", "limited");
    dispatch_once(&srv, dir);
    assert!(runs(&srv).is_empty());
    // A backup that is down too: still none.
    srv.ok(&["bot", "edit", "b", "--backup", "claude-1", "--why", "w"]);
    set_state(dir, "claude-1", "limited");
    dispatch_once(&srv, dir);
    assert!(runs(&srv).is_empty());
    assert!(srv.get("/api/bots/b")["queued"].as_i64().unwrap() > 0);
}

#[test]
fn switching_the_primary_keeps_notes_and_clears_a_backup_it_names() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = rig(dir);
    // The primary cannot be its own backup; unknown runtimes are refused.
    let o = srv.run(&["bot", "edit", "b", "--backup", "codex-1", "--why", "w"]);
    assert!(!o.status.success(), "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("primary runtime"),
        "{o:?}"
    );
    let o = srv.run(&["bot", "edit", "b", "--backup", "ghost", "--why", "w"]);
    assert!(!o.status.success(), "{o:?}");
    srv.ok(&["bot", "edit", "b", "--backup", "claude-1", "--why", "w"]);
    dispatch_once(&srv, dir);
    assert_eq!(srv.get("/api/bots/b")["session_agent"], "codex-1");

    // The owner makes the backup primary: fresh session there, notes kept,
    // no backup left.
    srv.ok(&[
        "bot",
        "edit",
        "b",
        "--agent",
        "claude-1",
        "--why",
        "owner switch",
    ]);
    let b = srv.get("/api/bots/b");
    assert_eq!(b["agent"], "claude-1");
    assert!(b["backup"].is_null(), "{b}");
    assert!(b["session_id"].is_null(), "{b}");
    assert_eq!(b["notes"], "KEEP");
    srv.ok(&["bot", "wake", "b", "--now", "--note", "after switch"]);
    dispatch_once(&srv, dir);
    let s = start(&srv, 2);
    assert_eq!(
        (&s["agent"], &s["mode"]),
        (&json!("claude-1"), &json!("fresh")),
        "{s}"
    );

    // `none` clears a backup.
    srv.ok(&["bot", "edit", "b", "--backup", "codex-1", "--why", "w"]);
    assert_eq!(srv.get("/api/bots/b")["backup"], "codex-1");
    let out = srv.ok(&["bot", "edit", "b", "--backup", "none", "--why", "w"]);
    assert!(out.contains("no backup runtime"), "{out}");
    assert!(srv.get("/api/bots/b")["backup"].is_null());
}
