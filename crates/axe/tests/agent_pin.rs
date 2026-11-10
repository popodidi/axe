mod common;

use axe::db;
use common::{Server, TOKEN};
use serde_json::{json, Value};

fn config(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir(dir.join("project")).unwrap();
    let p = dir.join("config.toml");
    std::fs::write(
        &p,
        format!(
            r#"
workspace_root = {root:?}
log_dir = {logs:?}
[agents.pinned]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done"]
resume_cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done"]
session = "preset"
[agents.other]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done"]
resume_cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done"]
session = "preset"
[[rule]]
agents = ["other"]
strategy = "round_robin"
"#,
            root = dir.display().to_string(),
            logs = dir.join("logs").display().to_string()
        ),
    )
    .unwrap();
    p
}

#[test]
fn cli_pin_overrides_round_robin_and_resume_keeps_stored_agent() {
    let d = tempfile::tempdir().unwrap();
    let cfg = config(d.path());
    let s = Server::start_with(d.path(), &cfg, Some(TOKEN), &[], &[]);
    let id = s.ok(&[
        "add",
        "--project",
        "project",
        "--title",
        "pin",
        "--agent",
        "pinned",
    ]);
    let id = id.trim();
    assert_eq!(s.get(&format!("/api/tasks/{id}"))["agent"], "pinned");
    assert!(s.ok(&["show", id]).contains("agent: pinned"));
    assert!(s.dispatcher(&["--once"], &[]).wait().status.success());
    let c = db::open(&s.db).unwrap();
    assert_eq!(
        c.query_row("SELECT count(*) FROM rr", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(s.get(&format!("/api/runs?task={id}"))[0]["agent"], "pinned");
    // A pre-existing session stays with its recorded agent even if the pin differs.
    c.execute("UPDATE sessions SET agent='other' WHERE task_id=?1", [id])
        .unwrap();
    s.ok(&["set", id, "--state", "pending"]);
    assert!(s.dispatcher(&["--once"], &[]).wait().status.success());
    let runs = s.get(&format!("/api/runs?task={id}"));
    assert_eq!(runs[0]["agent"], "other");
    assert_eq!(runs[0]["mode"], "resume");
    assert_eq!(
        c.query_row("SELECT count(*) FROM rr", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    // A failed resume starts fresh with the task pin, not the stored agent.
    let text = std::fs::read_to_string(&cfg).unwrap();
    let (head, tail) = text.split_once("[agents.other]").unwrap();
    let tail = tail.replace(
        "resume_cmd = [\"sh\", \"-c\", \"\\\"$AXE_BIN\\\" set \\\"$AXE_TASK\\\" --state done\"]",
        "resume_cmd = [\"sh\", \"-c\", \"exit 3\"]",
    );
    std::fs::write(&cfg, format!("{head}[agents.other]{tail}")).unwrap();
    s.ok(&["set", id, "--state", "pending"]);
    assert!(s.dispatcher(&["--once"], &[]).wait().status.success());
    let runs = s.get(&format!("/api/runs?task={id}"));
    assert_eq!(runs[0]["agent"], "pinned");
    assert_eq!(runs[0]["mode"], "fresh");
    assert_eq!(runs[1]["agent"], "other");
    assert_eq!(runs[1]["mode"], "resume");
    let unpinned = s.ok(&["add", "--project", "project", "--title", "normal"]);
    assert!(s.dispatcher(&["--once"], &[]).wait().status.success());
    assert_eq!(
        s.get(&format!("/api/runs?task={}", unpinned.trim()))[0]["agent"],
        "other"
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM rr", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn unknown_pin_is_rejected_and_removed_agent_escalates_without_fallback() {
    let d = tempfile::tempdir().unwrap();
    let cfg = config(d.path());
    let s = Server::start_with(d.path(), &cfg, Some(TOKEN), &[], &[]);
    let out = s.run(&[
        "add",
        "--project",
        "project",
        "--title",
        "bad",
        "--agent",
        "absent",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown agent"));
    assert_eq!(s.get("/api/tasks?all=1"), json!([]));
    let id = s.ok(&[
        "add",
        "--project",
        "project",
        "--title",
        "removed",
        "--agent",
        "pinned",
    ]);
    // Simulate a later dispatcher config no longer containing this agent.
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("agents.pinned", "agents.replacement");
    std::fs::write(&cfg, text).unwrap();
    assert!(s.dispatcher(&["--once"], &[]).wait().status.success());
    let id = id.trim();
    assert_eq!(s.get(&format!("/api/tasks/{id}"))["state"], "escalated");
    assert!(s
        .ok(&["log", id, "--full"])
        .contains("unknown agent pinned"));
    assert_eq!(s.get(&format!("/api/runs?task={id}")), json!([]));
}

#[test]
fn routine_pin_is_validated_saved_and_copied_for_scheduled_and_manual_tasks() {
    let d = tempfile::tempdir().unwrap();
    let cfg = config(d.path());
    let s = Server::start_with(d.path(), &cfg, Some(TOKEN), &[], &[]);
    let file = d.path().join("routine.json");
    let mut t = json!({"name":"pin", "project":"project", "title":"pin", "detail":"test", "cron":"0 0 1 1 *", "agent":"absent"});
    std::fs::write(&file, t.to_string()).unwrap();
    assert!(!s
        .run(&["routine", "save", "--file", file.to_str().unwrap()])
        .status
        .success());
    t["agent"] = json!("pinned");
    std::fs::write(&file, t.to_string()).unwrap();
    let r: Value =
        serde_json::from_str(&s.ok(&["routine", "save", "--file", file.to_str().unwrap()]))
            .unwrap();
    let id = r["id"].to_string();
    assert_eq!(s.get(&format!("/api/routines/{id}"))["agent"], "pinned");
    let manual: Value =
        serde_json::from_str(&s.ok(&["routine", "trigger", &id, "--key", "one"])).unwrap();
    assert_eq!(
        s.get(&format!("/api/tasks/{}", manual["task_id"]))["agent"],
        "pinned"
    );
    let mut c = db::open(&s.db).unwrap();
    let next = r["next_at"].as_i64().unwrap();
    assert_eq!(axe::routine::tick(&mut c, next).unwrap(), 1);
    for task in db::list_tasks(&c, &db::Filter::default()).unwrap() {
        assert_eq!(task.agent.as_deref(), Some("pinned"));
    }
    t["agent"] = json!("absent");
    std::fs::write(&file, t.to_string()).unwrap();
    assert!(!s
        .run(&[
            "routine",
            "save",
            "--id",
            &id,
            "--file",
            file.to_str().unwrap()
        ])
        .status
        .success());
    assert_eq!(s.get(&format!("/api/routines/{id}"))["agent"], "pinned");
}
