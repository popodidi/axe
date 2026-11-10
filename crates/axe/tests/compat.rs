//! The taskq compatibility shim (`src/compat.rs`). Deleted together with it.
mod common;

use std::collections::HashMap;
use std::process::Command;

use axe::compat;
use common::{Server, BIN, TOKEN};

#[test]
fn old_variables_fill_only_unset_axe_variables() {
    let env: HashMap<&str, &str> = HashMap::from([
        ("TASKQ_SERVER", "http://old"),
        ("TASKQ_TASK", "7"),
        ("AXE_TASK", "8"),
        ("TASKQ_UNRELATED", "x"),
    ]);
    let imported = compat::imported_env(|k| env.get(k).map(|v| v.to_string()));
    assert_eq!(
        imported,
        [("AXE_SERVER".to_string(), "http://old".to_string())]
    );
}

#[test]
fn children_get_both_names() {
    let env = [("AXE_TASK", "7".to_string()), ("OTHER", "x".to_string())];
    assert_eq!(
        compat::child_env(&env),
        [
            ("AXE_TASK".to_string(), "7".to_string()),
            ("OTHER".into(), "x".into()),
            ("TASKQ_TASK".into(), "7".into()),
        ]
    );
    assert_eq!(
        compat::both_names(&["AXE_WORKTREE"]),
        ["AXE_WORKTREE", "TASKQ_WORKTREE"]
    );
}

#[test]
fn old_directories_are_used_only_when_the_axe_one_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let r = |rel: &str| compat::resolve(h, rel);
    // Nothing exists: the axe path.
    assert_eq!(r(".config/axe/token"), h.join(".config/axe/token"));
    // Only the old directory exists: the old path.
    std::fs::create_dir_all(h.join(".config/taskq")).unwrap();
    std::fs::create_dir_all(h.join(".local/share/taskq")).unwrap();
    std::fs::write(h.join(".local/share/taskq/tasks.db"), "").unwrap();
    std::fs::create_dir_all(h.join(".local/state/taskq/logs")).unwrap();
    assert_eq!(r(".config/axe/token"), h.join(".config/taskq/token"));
    assert_eq!(r(".config/axe"), h.join(".config/taskq"));
    assert_eq!(
        r(".local/share/axe/axe.db"),
        h.join(".local/share/taskq/tasks.db")
    );
    assert_eq!(
        r(".local/state/axe/logs"),
        h.join(".local/state/taskq/logs")
    );
    assert_eq!(r(".config/axex/token"), h.join(".config/axex/token"));
    assert_eq!(r("workspace"), h.join("workspace"));
    // Once the axe directory exists it wins.
    std::fs::create_dir_all(h.join(".config/axe")).unwrap();
    std::fs::create_dir_all(h.join(".local/share/axe")).unwrap();
    assert_eq!(r(".config/axe/token"), h.join(".config/axe/token"));
    // The database file decides on its own: an empty axe data dir is not enough.
    assert_eq!(
        r(".local/share/axe/axe.db"),
        h.join(".local/share/taskq/tasks.db")
    );
    std::fs::write(h.join(".local/share/axe/axe.db"), "").unwrap();
    assert_eq!(
        r(".local/share/axe/axe.db"),
        h.join(".local/share/axe/axe.db")
    );
}

/// The CLI under `name` (a symlink to the binary), with only old-name
/// variables pointing at `srv` and its token in the old config directory.
fn old_cli(srv: &Server, home: &std::path::Path, name: &str, args: &[&str]) -> Command {
    let link = home.join(name);
    if !link.exists() {
        std::os::unix::fs::symlink(BIN, &link).unwrap();
    }
    let mut c = Command::new(&link);
    common::isolate(&mut c);
    for n in compat::ENV_NAMES {
        c.env_remove(format!("AXE_{n}"));
    }
    c.args(args)
        .env("HOME", home)
        .env("TASKQ_SERVER", &srv.url)
        .env("TASKQ_ACTOR", "agent")
        .env("TASKQ_RETRY_SECS", "0");
    c
}

#[test]
fn taskq_symlink_old_variables_and_old_token_directory_work() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".config/taskq")).unwrap();
    std::fs::write(home.join(".config/taskq/token"), format!("{TOKEN}\n")).unwrap();

    let out = old_cli(
        &srv,
        &home,
        "taskq",
        &["add", "--project", "p", "--title", "t"],
    )
    .output()
    .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("deprecated TASKQ_* variables"), "{stderr}");
    assert!(stderr.contains("AXE_SERVER"), "{stderr}");

    let events = srv.get("/api/tasks/1/events");
    assert_eq!(events[0]["actor"], "agent");

    // Without the old token directory the same write is refused.
    std::fs::remove_dir_all(home.join(".config/taskq")).unwrap();
    let out = old_cli(&srv, &home, "taskq", &["note", "1", "x"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
}

#[test]
fn server_accepts_the_old_request_id_header() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "t"]);
    let post = || {
        ureq::post(&format!("{}/api/tasks/1/notes", srv.url))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("X-Taskq-Request-Id", "old-id-1")
            .send_json(serde_json::json!({ "body": "once" }))
            .unwrap()
            .into_string()
            .unwrap()
    };
    assert_eq!(post(), post());
    let events = srv.get("/api/tasks/1/events");
    let notes = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "note" && e["body"] == "once")
        .count();
    assert_eq!(notes, 1);
}

#[test]
fn compat_names_are_the_old_ones() {
    assert_eq!(compat::OLD_NAME, "taskq");
    assert_eq!(compat::OLD_ENV_PREFIX, "TASKQ_");
    assert_eq!(compat::OLD_REQUEST_ID_HEADER, "x-taskq-request-id");
    assert_eq!(compat::OLD_FILE_NAME_HEADER, "X-Taskq-File-Name");
}
