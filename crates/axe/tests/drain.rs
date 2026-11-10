//! Bounded dispatcher drain (SIGUSR1): abort, protection, resume, budget.
mod common;
use common::{Server, Worker, TOKEN};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

const NOTICE: &str = "interrupted by an axe rollout";

fn wait(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(40);
    while !f() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A worker that records its prompt, then waits for `<dir>/go-<task>` and marks
/// its task done. The prompt arrives as the last argument.
fn setup(dir: &Path, drain_seconds: u64, extra: &str) -> Server {
    std::fs::create_dir_all(dir.join("p")).unwrap();
    let script = dir.join("worker.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
d={dir}
for a; do p=$a; done
echo "$p" >> "$d/prompts-$AXE_TASK"
echo "$AXE_SESSION" >> "$d/sessions-$AXE_TASK"
if [ -e "$d/protect-$AXE_TASK" ]; then "$AXE_BIN" protect "$AXE_TASK" --note deploy; fi
touch "$d/started-$AXE_TASK"
while [ ! -e "$d/go-$AXE_TASK" ]; do sleep 0.05; done
"$AXE_BIN" set "$AXE_TASK" --state done --note finished
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"workspace_root={dir:?}
log_dir={logs:?}
poll_seconds=1
drain_abort_seconds={drain_seconds}
[pool]
total=3
[defaults]
agents=["fixture"]
[agents.fixture]
session="preset"
cmd=["sh",{script:?},"--session-id","{{session_id}}","{{prompt}}"]
resume_cmd=["sh",{script:?},"--resume","{{session_id}}","{{prompt}}"]
{extra}
"#,
            dir = dir.to_str().unwrap(),
            logs = dir.join("logs").to_str().unwrap(),
            script = script.to_str().unwrap()
        ),
    )
    .unwrap();
    Server::start_with(dir, &cfg, Some(TOKEN), &[], &[])
}

fn start(s: &Server, dir: &Path, n: usize) -> Worker {
    let bin = common::BIN;
    let token = s.token_file.to_str().unwrap().to_string();
    let d = s.dispatcher(
        &[],
        &[
            ("AXE_BIN", bin),
            ("AXE_TOKEN_FILE", &token),
            ("AXE_ACTOR", "agent"),
        ],
    );
    for i in 1..=n {
        let started = dir.join(format!("started-{i}"));
        wait(|| started.exists());
    }
    d
}

fn signal(d: &Worker, sig: i32) {
    unsafe {
        libc::kill(d.0.id() as i32, sig);
    }
}

fn state(s: &Server, id: i64) -> Value {
    s.get(&format!("/api/tasks/{id}"))
}

fn kinds(s: &Server, id: i64, kind: &str) -> usize {
    s.get(&format!("/api/tasks/{id}/events"))
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .count()
}

fn lines(dir: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(name))
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

fn add(s: &Server, title: &str) {
    s.ok(&["add", "--project", "p", "--title", title]);
}

#[test]
fn past_deadline_run_returns_pending_with_session_and_budget_and_resume_is_told() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let s = setup(dir, 1, "");
    add(&s, "long");
    let mut d = start(&s, dir, 1);
    assert_eq!(state(&s, 1)["runs"], 1);
    signal(&d, libc::SIGUSR1);
    let status = d.0.wait().unwrap();
    assert!(status.success(), "{status:?}");

    let task = state(&s, 1);
    assert_eq!(task["state"], "pending");
    assert!(task["claimed_by"].is_null());
    assert_eq!(task["runs"], 0, "a rollout abort spends no run budget");
    assert_eq!(kinds(&s, 1, "run_interrupted"), 1);
    assert_eq!(s.get("/api/runs?live=1&limit=10"), serde_json::json!([]));

    // The next dispatcher resumes the same session and says why.
    std::fs::remove_file(dir.join("started-1")).unwrap();
    let mut d = start(&s, dir, 1);
    let sessions = lines(dir, "sessions-1");
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0], sessions[1]);
    let prompts = lines(dir, "prompts-1");
    assert!(!prompts[0].contains(NOTICE));
    assert!(prompts[1].contains(NOTICE), "{}", prompts[1]);
    assert_eq!(state(&s, 1)["runs"], 1);
    assert_eq!(
        s.get("/api/runs?task=1&limit=10").as_array().unwrap().len(),
        2
    );

    std::fs::write(dir.join("go-1"), "").unwrap();
    wait(|| state(&s, 1)["state"] == "done");
    signal(&d, libc::SIGTERM);
    d.0.wait().unwrap();
}

#[test]
fn protected_run_is_waited_for_and_can_be_cleared() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let s = setup(dir, 0, "");
    std::fs::write(dir.join("protect-1"), "").unwrap();
    add(&s, "deploy");
    add(&s, "plain");
    let mut d = start(&s, dir, 2);
    assert_eq!(kinds(&s, 1, "protected"), 1);
    signal(&d, libc::SIGUSR1);
    wait(|| state(&s, 2)["state"] == "pending");
    // Well past the deadline the protected run still lives.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(state(&s, 1)["state"], "in_progress");
    assert_eq!(kinds(&s, 1, "run_interrupted"), 0);
    assert!(d.0.try_wait().unwrap().is_none());
    // New work is not admitted while draining.
    assert!(lines(dir, "prompts-2").len() == 1);

    std::fs::write(dir.join("go-1"), "").unwrap();
    assert!(d.0.wait().unwrap().success());
    assert_eq!(state(&s, 1)["state"], "done");
    assert_eq!(state(&s, 1)["runs"], 1);
}

#[test]
fn clearing_protection_lets_the_drain_abort_it() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let s = setup(dir, 0, "");
    std::fs::write(dir.join("protect-1"), "").unwrap();
    add(&s, "deploy");
    let mut d = start(&s, dir, 1);
    signal(&d, libc::SIGUSR1);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(state(&s, 1)["state"], "in_progress");
    s.ok(&["protect", "1", "--clear"]);
    assert!(d.0.wait().unwrap().success());
    assert_eq!(state(&s, 1)["state"], "pending");
    assert_eq!(state(&s, 1)["runs"], 0);
}

#[test]
fn run_finishing_inside_the_window_is_untouched() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let s = setup(dir, 300, "");
    add(&s, "short");
    let mut d = start(&s, dir, 1);
    signal(&d, libc::SIGUSR1);
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(dir.join("go-1"), "").unwrap();
    assert!(d.0.wait().unwrap().success());
    assert_eq!(state(&s, 1)["state"], "done");
    assert_eq!(state(&s, 1)["runs"], 1);
    assert_eq!(kinds(&s, 1, "run_interrupted"), 0);
}

#[test]
fn sigterm_still_kills_and_escalates() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let s = setup(dir, 300, "");
    add(&s, "stopped");
    let mut d = start(&s, dir, 1);
    signal(&d, libc::SIGTERM);
    d.0.wait().unwrap();
    assert_eq!(state(&s, 1)["state"], "escalated");
    assert_eq!(state(&s, 1)["runs"], 1);
    assert_eq!(kinds(&s, 1, "run_interrupted"), 0);
}

#[test]
fn protect_needs_a_live_run_and_is_refused_while_aborting() {
    let t = tempfile::tempdir().unwrap();
    let s = setup(t.path(), 300, "");
    add(&s, "idle");
    let refused = s.run(&["protect", "1"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no live run"));

    // A fixture dispatcher holds run state still between decision and kill.
    let post = |path: &str, body: Value| -> Value {
        ureq::post(&format!("{}{path}", s.url))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(body)
            .unwrap()
            .into_json()
            .unwrap()
    };
    let reg = post(
        "/api/dispatcher/register",
        serde_json::json!({"id":"fixture","generation":""}),
    );
    let op = |name: &str, body: Value| -> Value {
        post(
            "/api/dispatcher/operation",
            serde_json::json!({"dispatcher":"fixture","generation":reg["generation"],
                "operation_id":uuid::Uuid::new_v4().to_string(),"op":name,"body":body}),
        )
    };
    op("claim", serde_json::json!({"project":"p","claim":"c"}));
    let run = op(
        "prepare",
        serde_json::json!({"task":1,"claim":"c","agent":"fixture","mode":"fresh",
            "session_id":null,"log":"/tmp/x","session":null,"event":{}}),
    );
    let run = run.as_i64().or(run["result"].as_i64()).unwrap();
    op("started", serde_json::json!({"run":run}));
    s.ok(&["protect", "1", "--note", "step"]);
    let held = op("drain_abort", serde_json::json!({"run":run}));
    assert_eq!(held.as_bool().or(held["result"].as_bool()), Some(false));
    s.ok(&["protect", "1", "--clear"]);
    let go = op("drain_abort", serde_json::json!({"run":run}));
    assert_eq!(go.as_bool().or(go["result"].as_bool()), Some(true));
    let refused = s.run(&["protect", "1"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("being aborted"));
}

#[test]
fn bot_is_admitted_during_a_drain_held_by_a_task_run() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let bot_script = dir.join("bot.sh");
    std::fs::write(
        &bot_script,
        format!("#!/bin/sh\necho x >> {}/bot-runs\n", dir.to_str().unwrap()),
    )
    .unwrap();
    let extra = format!(
        "[agents.botrun]\ncmd=[\"sh\",{:?},\"{{prompt}}\"]\nbot_args=[\"--bot\"]\n",
        bot_script.to_str().unwrap()
    );
    let s = setup(dir, 300, &extra);
    add(&s, "long");
    let mut d = start(&s, dir, 1);
    signal(&d, libc::SIGUSR1);
    std::thread::sleep(Duration::from_millis(500));
    // Creating a bot queues its first wake.
    let spec = dir.join("bot.json");
    std::fs::write(
        &spec,
        serde_json::json!({"name":"b1","project":"p","agent":"botrun","identity":"I",
            "conduct":"C","notes":"","heartbeat":null,"nudge_after":null})
        .to_string(),
    )
    .unwrap();
    s.ok(&["bot", "create", "--file", spec.to_str().unwrap()]);
    let ran = dir.join("bot-runs");
    wait(|| ran.exists());
    std::fs::write(dir.join("go-1"), "").unwrap();
    assert!(d.0.wait().unwrap().success());
}
