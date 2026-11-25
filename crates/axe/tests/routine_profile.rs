//! A routine's cost controls through the node: the pre-check, the
//! model and effort, and the skill profile of the run folder.
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Server, TOKEN};
use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A workspace with clone `one` (own skills `build` and `lint`) and an agent
/// of harness `claude-code` that records its arguments and run folder.
struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBuf,
    srv: Server,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let clone = base.join("ws/one");
        let origin = base.join("one.git");
        std::fs::create_dir_all(&clone).unwrap();
        git(
            &base,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        git(&clone, &["init", "-q", "-b", "main"]);
        for name in ["build", "lint"] {
            let skill = clone.join(".agents/skills").join(name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} it\n---\nbody\n"),
            )
            .unwrap();
        }
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "base"]);
        git(
            &clone,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&clone, &["push", "-q", "origin", "main"]);
        git(&clone, &["fetch", "-q", "origin"]);
        git(&clone, &["remote", "set-head", "origin", "main"]);
        std::fs::create_dir_all(base.join("ws/proj")).unwrap();
        std::fs::create_dir_all(base.join("record")).unwrap();
        let script = base.join("agent.sh");
        std::fs::write(
            &script,
            r#"set -e
{ echo "args=$*"; echo "pwd=$PWD"; echo "skills=$(ls skills | tr '\n' ' ')"; } > "$RECORD/$AXE_TASK"
"$AXE_BIN" set "$AXE_TASK" --state done
"#,
        )
        .unwrap();
        let config = base.join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[defaults]
agents = ["rec"]
[agents.rec]
cmd = ["sh", "{script}"]
session = "none"
harness = "claude-code"
[worktrees]
"#,
                ws = base.join("ws").display(),
                logs = base.join("logs").display(),
                script = script.display(),
            ),
        )
        .unwrap();
        let srv = Server::start_with(&base, &config, Some(TOKEN), &[], &[]);
        srv.ok(&["repo", "add", "one", "https://example.invalid/one.git"]);
        Fixture {
            _dir: dir,
            base,
            srv,
        }
    }

    /// Saves a routine with `extra` fields and triggers one task of it.
    fn trigger(&self, extra: Value) -> (String, String) {
        let mut t = json!({"name":"r", "project":"proj", "title":"t", "detail":"Outcome: x",
            "cron":"0 0 1 1 *", "repos":["one"]});
        for (k, v) in extra.as_object().unwrap() {
            t[k] = v.clone();
        }
        let file = self.base.join("routine.json");
        std::fs::write(&file, t.to_string()).unwrap();
        let r: Value = serde_json::from_str(&self.srv.ok(&[
            "routine",
            "save",
            "--file",
            file.to_str().unwrap(),
        ]))
        .unwrap();
        let id = r["id"].to_string();
        let out: Value =
            serde_json::from_str(&self.srv.ok(&["routine", "trigger", &id, "--key", "k"])).unwrap();
        (id, out["task_id"].to_string())
    }

    fn dispatch(&self) {
        let record = self.base.join("record");
        let env = [("RECORD", record.to_str().unwrap())];
        let out = self.srv.dispatcher(&["--once"], &env).wait();
        assert!(out.status.success(), "{out:?}");
    }

    fn record(&self, task: &str) -> Option<String> {
        std::fs::read_to_string(self.base.join("record").join(task)).ok()
    }

    fn events(&self, task: &str) -> Vec<Value> {
        serde_json::from_str(&self.srv.ok(&["log", task, "--json"])).unwrap()
    }

    fn state(&self, task: &str) -> String {
        let v: Value = serde_json::from_str(&self.srv.ok(&["show", task, "--json"])).unwrap();
        v["state"].as_str().unwrap().to_string()
    }
}

fn kinds<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["kind"] == kind).collect()
}

#[test]
fn pre_check_exit_zero_marks_the_task_done_without_an_agent() {
    let f = Fixture::new();
    let (routine, task) = f.trigger(json!({
        "pre_check": "pwd > \"$RECORD/pc-$AXE_TASK\"; echo \"quiet: nothing to do\"; exit 0"
    }));
    f.dispatch();
    assert_eq!(f.state(&task), "done");
    assert_eq!(f.record(&task), None, "the agent never ran");
    let events = f.events(&task);
    assert!(kinds(&events, "run_start").is_empty(), "{events:?}");
    let notes = kinds(&events, "note");
    assert_eq!(notes.len(), 1, "{events:?}");
    let note = notes[0]["body"].as_str().unwrap();
    assert!(
        note.starts_with("pre-check exit 0: done with no agent run"),
        "{note}"
    );
    assert!(note.contains("quiet: nothing to do"), "{note}");
    // It ran in the task's run folder.
    let pwd = f.record(&format!("pc-{task}")).unwrap();
    assert_eq!(
        pwd.trim(),
        f.base
            .join(format!("ws/.runs/task-{task}"))
            .display()
            .to_string()
    );
    // The routine records the skip.
    let revents: Vec<Value> =
        serde_json::from_str(&f.srv.ok(&["routine", "events", &routine])).unwrap();
    let skipped = revents
        .iter()
        .find(|e| e["kind"] == "pre-check-skipped")
        .expect("a pre-check-skipped routine event");
    assert!(
        skipped["body"].as_str().unwrap().contains(&task),
        "{skipped}"
    );
}

#[test]
fn pre_check_nonzero_starts_the_agent_with_the_profile() {
    let f = Fixture::new();
    let (routine, task) = f.trigger(json!({
        "pre_check": "echo \"2 new tickets\"; exit 3",
        "model": "claude-sonnet-5-5",
        "effort": "low",
        "skills": ["lint"]
    }));
    f.dispatch();
    assert_eq!(f.state(&task), "done");
    let rec = f.record(&task).expect("the agent ran");
    assert!(
        rec.contains("args=--model claude-sonnet-5-5 --effort low"),
        "{rec}"
    );
    // Only the profile's skill is in the run folder.
    assert!(rec.contains("skills=lint \n"), "{rec}");
    let run = f.base.join(format!("ws/.runs/task-{task}"));
    let agents = std::fs::read_to_string(run.join("AGENTS.md")).unwrap();
    assert!(agents.contains("skills/lint"), "{agents}");
    assert!(!agents.contains("skills/build"), "{agents}");
    let events = f.events(&task);
    let note = kinds(&events, "note")[0]["body"].as_str().unwrap();
    assert!(
        note.starts_with("pre-check exit 3: starting the agent"),
        "{note}"
    );
    assert!(note.contains("2 new tickets"), "{note}");
    // The run log names the model, effort and profile.
    let starts = kinds(&events, "run_start");
    assert_eq!(starts.len(), 1);
    let body: Value = serde_json::from_str(starts[0]["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["use"], "claude-code:claude-sonnet-5-5:low", "{body}");
    assert_eq!(body["routine"]["routine"].to_string(), routine);
    assert_eq!(body["routine"]["skills"], json!(["lint"]));
    // A later run of the same task does not run the pre-check again.
    f.srv.ok(&["set", &task, "--state", "pending"]);
    f.dispatch();
    let events = f.events(&task);
    assert_eq!(kinds(&events, "run_start").len(), 2);
    assert_eq!(kinds(&events, "note").len(), 1, "{events:?}");
}

#[test]
fn routine_without_a_profile_runs_as_before() {
    let f = Fixture::new();
    let (_, task) = f.trigger(json!({}));
    f.dispatch();
    let rec = f.record(&task).expect("the agent ran");
    assert!(rec.contains("args=\n"), "{rec}");
    assert!(rec.contains("skills=build lint \n"), "{rec}");
    let events = f.events(&task);
    let body: Value =
        serde_json::from_str(kinds(&events, "run_start")[0]["body"].as_str().unwrap()).unwrap();
    assert!(body.get("routine").is_none(), "{body}");
}

#[test]
fn stopping_the_dispatcher_during_a_pre_check_returns_the_task_to_pending() {
    let f = Fixture::new();
    let (_, task) = f.trigger(json!({
        "pre_check": "touch \"$RECORD/pc-$AXE_TASK\"; sleep 30; exit 0"
    }));
    let record = f.base.join("record");
    let d = f
        .srv
        .dispatcher(&[], &[("RECORD", record.to_str().unwrap())]);
    let started = record.join(format!("pc-{task}"));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < until,
            "the pre-check never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    unsafe { libc::kill(d.0.id() as i32, libc::SIGTERM) };
    let out = d.wait();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(f.state(&task), "pending");
    let events = f.events(&task);
    assert!(kinds(&events, "run_start").is_empty(), "{events:?}");
    assert!(
        kinds(&events, "note").iter().any(|n| n["body"]
            .as_str()
            .unwrap()
            .contains("pre-check dispatcher stopped")),
        "{events:?}"
    );
}

#[test]
fn routine_naming_an_unregistered_repo_is_refused_when_saved() {
    let f = Fixture::new();
    let t = json!({"name":"r", "project":"proj", "title":"t", "detail":"Outcome: x",
        "cron":"0 0 1 1 *", "repos":["one","ghost"]});
    let file = f.base.join("routine.json");
    std::fs::write(&file, t.to_string()).unwrap();
    let out = f
        .srv
        .run(&["routine", "save", "--file", file.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("repo ghost is not registered") && err.contains("`axe repo add ghost <url>`"),
        "{err}"
    );
    assert_eq!(f.srv.get("/api/routines"), json!([]));
}
