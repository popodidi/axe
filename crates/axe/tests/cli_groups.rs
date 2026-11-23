//! `axe task <verb>` and the old top-level task commands (`axe add`, ...)
//! behave the same against a real server, and the old forms stay hidden
//! from `axe --help`.
mod common;

use common::Server;
use serde_json::Value;

/// Runs `args` in the old form or under `axe task`.
fn run(srv: &Server, grouped: bool, args: &[&str]) -> String {
    let mut a = Vec::new();
    if grouped {
        a.push("task");
    }
    a.extend_from_slice(args);
    srv.ok(&a)
}

/// A task's JSON without the fields that differ between two tasks.
fn comparable(srv: &Server, id: &str) -> Value {
    let mut t: Value = serde_json::from_str(&srv.ok(&["show", id, "--json"])).unwrap();
    for k in ["id", "created_at", "updated_at"] {
        t.as_object_mut().unwrap().remove(k);
    }
    t
}

/// A task's event log as (actor, kind, body) without times and ids.
fn events(srv: &Server, id: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(&srv.ok(&["log", id, "--json"])).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let mut e = e.clone();
            let o = e.as_object_mut().unwrap();
            for k in ["id", "task_id", "at", "created_at"] {
                o.remove(k);
            }
            e
        })
        .collect()
}

#[test]
fn old_and_grouped_task_commands_do_the_same_thing() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let mut ids = Vec::new();
    for grouped in [false, true] {
        let id = run(
            &srv,
            grouped,
            &[
                "add",
                "--project",
                "p",
                "--title",
                "t",
                "--label",
                "x",
                "--owner",
                "bot:o",
                "--detail",
                "## Outcome\nd",
            ],
        )
        .trim()
        .to_string();
        let id = id.as_str();
        run(&srv, grouped, &["note", id, "a note"]);
        run(
            &srv,
            grouped,
            &["artifact", id, "https://example.invalid/pr/1"],
        );
        run(
            &srv,
            grouped,
            &["set", id, "--label", "y", "--unlabel", "x"],
        );
        run(
            &srv,
            grouped,
            &[
                "escalate",
                id,
                "--blocked",
                "b",
                "--resolver",
                "owner: act",
                "--resume",
                "r",
                "--class",
                "decision",
            ],
        );
        run(
            &srv,
            grouped,
            &["classify", id, "--escalation", "capability"],
        );
        run(
            &srv,
            grouped,
            &["approve", id, "--sha", "abc", "--scope", "prod"],
        );
        run(
            &srv,
            grouped,
            &[
                "wait",
                id,
                "--until",
                "2099-01-01T00:00:00Z",
                "--note",
                "later",
            ],
        );
        ids.push(id.to_string());
    }
    let (a, b) = (ids[0].as_str(), ids[1].as_str());
    assert_eq!(comparable(&srv, a), comparable(&srv, b));
    assert_eq!(events(&srv, a), events(&srv, b));

    // Reads print exactly the same in both forms.
    for args in [
        vec!["show", a],
        vec!["show", a, "--json"],
        vec!["log", a, "--full"],
        vec!["deps", a],
        vec!["list", "--all", "--json"],
    ] {
        assert_eq!(run(&srv, false, &args), run(&srv, true, &args), "{args:?}");
    }
}

#[test]
fn help_groups_commands_and_hides_the_old_forms() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let help = srv.ok(&["--help"]);
    for section in ["Primitives:", "Store:", "Ops and admin:"] {
        assert!(help.contains(section), "{help}");
    }
    assert!(help.contains("\n  task "), "{help}");
    assert!(!help.contains("\n  add "), "{help}");
    let task_help = srv.ok(&["task", "--help"]);
    assert!(task_help.contains("\n  add "), "{task_help}");
    // The hidden forms keep their own help.
    assert!(srv.ok(&["add", "--help"]).contains("Usage: axe add"));
}
