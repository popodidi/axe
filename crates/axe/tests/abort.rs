mod common;
use common::{Server, TOKEN};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn post(s: &Server, path: &str, body: Value) -> ureq::Response {
    ureq::post(&format!("{}{path}", s.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(body)
        .unwrap()
}
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .is_some_and(|s| !s.rsplit_once(") ").unwrap().1.starts_with('Z'))
}
fn setup(dir: &std::path::Path) -> Server {
    std::fs::create_dir_all(dir.join("p")).unwrap();
    let script = dir.join("worker.py");
    std::fs::write(
        &script,
        r#"import os, subprocess, time
child = subprocess.Popen(['sleep', '300'], start_new_session=True)
# The intermediate parent exits. The worker's subreaper must retain this
# detached grandchild so abort can still find it.
daemon = subprocess.check_output(['python3', '-c', "import subprocess; p=subprocess.Popen(['sleep','300'],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); print(p.pid)"]).decode().strip()
with open(os.environ['PID_DIR'] + '/' + os.environ['AXE_TASK'], 'w') as f:
    f.write(str(os.getpid()) + ' ' + str(child.pid) + ' ' + daemon)
time.sleep(300)
"#,
    )
    .unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"workspace_root={dir:?}
log_dir={logs:?}
poll_seconds=1
[pool]
total=2
[defaults]
agents=["fixture"]
[agents.fixture]
cmd=["python3",{script:?}]
"#,
            dir = dir.to_str().unwrap(),
            logs = dir.join("logs").to_str().unwrap(),
            script = script.to_str().unwrap()
        ),
    )
    .unwrap();
    Server::start_with(dir, &cfg, Some(TOKEN), &[], &[])
}
fn pids(dir: &std::path::Path, task: &str) -> Vec<i32> {
    std::fs::read_to_string(dir.join(task))
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect()
}
#[test]
fn remote_abort_kills_only_selected_tree_and_is_retry_safe() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    s.ok(&["add", "--project", "p", "--title", "target"]);
    s.ok(&["add", "--project", "p", "--title", "unrelated"]);
    let _d = s.dispatcher(&[], &[("PID_DIR", dir.path().to_str().unwrap())]);
    wait(|| dir.path().join("1").exists() && dir.path().join("2").exists());
    let target = pids(dir.path(), "1");
    let other = pids(dir.path(), "2");
    let run = s.get("/api/runs?task=1")[0]["id"].as_i64().unwrap();
    let bad = s
        .cli(&["abort", "1"])
        .env("AXE_TOKEN", "wrong")
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(target.iter().all(|p| alive(*p)));
    assert_eq!(s.get("/api/tasks/1")["state"], "in_progress");
    let out = s.ok(&["abort", "1"]);
    assert!(out.contains("abort confirmed"), "{out}");
    assert!(target.iter().all(|p| !alive(*p)));
    assert!(other.iter().all(|p| alive(*p)));
    assert_eq!(s.get("/api/tasks/1")["state"], "escalated");
    assert!(s.get("/api/runs?task=1")[0]["ended_at"].is_string());
    assert!(s
        .ok(&["abort", "1", "--run", &run.to_string()])
        .contains("confirmed"));
    let events = s.get("/api/tasks/1/events");
    for kind in ["abort_requested", "run_aborted"] {
        assert_eq!(
            events
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["kind"] == kind)
                .count(),
            1
        );
    }
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(s.get("/api/tasks/1")["runs"], 1);
    s.ok(&["set", "1", "--state", "pending"]);
    wait(|| s.get("/api/runs?task=1")[0]["id"] != run);
    wait(|| pids(dir.path(), "1") != target);
    let next = pids(dir.path(), "1");
    let historical = s.ok(&["abort", "1", "--run", &run.to_string()]);
    assert!(
        historical.contains("current task state: in_progress"),
        "{historical}"
    );
    assert!(next.iter().all(|p| alive(*p)));
    s.ok(&["abort", "1"]);
    s.ok(&["abort", "2"]);
}

#[test]
fn disconnected_request_is_durable_and_never_claims_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let s = setup(dir.path());
    s.ok(&["add", "--project", "p", "--title", "target"]);
    let mut d = s.dispatcher(&[], &[("PID_DIR", dir.path().to_str().unwrap())]);
    wait(|| dir.path().join("1").exists());
    let target = pids(dir.path(), "1");
    // Stop the dispatcher process only. Its worker stays running and its control
    // connection times out, reproducing loss of the control channel.
    unsafe {
        libc::kill(d.0.id() as i32, libc::SIGSTOP);
    }
    let out = s.run(&["abort", "1", "--timeout", "0"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("NOT confirmed"));
    assert!(target.iter().all(|p| alive(*p)));
    assert!(!s.run(&["set", "1", "--state", "pending"]).status.success());
    wait(|| s.get("/api/dispatcher")["connected"] == false);
    unsafe {
        libc::kill(d.0.id() as i32, libc::SIGCONT);
    }
    assert!(s.ok(&["abort", "1"]).contains("confirmed"));
    assert!(target.iter().all(|p| !alive(*p)));
    // Stop gracefully so its journal does not leave an orphaned test worker.
    unsafe {
        libc::kill(d.0.id() as i32, libc::SIGTERM);
    }
    d.0.wait().unwrap();
}

#[test]
fn missing_nonrunning_and_stale_targets_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path());
    assert!(!s.run(&["abort", "999"]).status.success());
    s.ok(&["add", "--project", "p", "--title", "not running"]);
    assert!(!s.run(&["abort", "1"]).status.success());
    assert!(!s.run(&["abort", "1", "--run", "999"]).status.success());
    // Exercise the same authenticated API with a dispatcher whose run completes
    // immediately before the abort request, and then with a stale generation.
    let reg: Value = post(
        &s,
        "/api/dispatcher/register",
        json!({"id":"fixture","generation":""}),
    )
    .into_json()
    .unwrap();
    let op = |name: &str, body: Value| -> Value {
        post(&s,"/api/dispatcher/operation",json!({"dispatcher":"fixture","generation":reg["generation"],"operation_id":uuid::Uuid::new_v4().to_string(),"op":name,"body":body})).into_json().unwrap()
    };
    op("claim", json!({"project":"p","claim":"dispatch:test"}));
    let run = op(
        "prepare",
        json!({"task":1,"claim":"dispatch:test","agent":"fixture","mode":"resume","log":"/unused"}),
    );
    // A stale generation cannot finish or acknowledge the active run.
    assert!(ureq::post(&format!("{}/api/dispatcher/operation",s.url)).set("Authorization",&format!("Bearer {TOKEN}")).send_json(json!({"dispatcher":"fixture","generation":"stale","operation_id":"bad","op":"finish","body":{"run":run,"aborted":true}})).is_err());
    let accepted: Value = post(&s, "/api/tasks/1/abort", json!({"run":run}))
        .into_json()
        .unwrap();
    assert_eq!(accepted["status"], "requested");
    let retry = op(
        "finish",
        json!({"run":run,"code":1,"resumed":true,"note":"normal exit","event":{}}),
    );
    assert_eq!(retry, false);
    assert!(!s.run(&["abort", "1"]).status.success());
    assert_eq!(s.get("/api/tasks/1")["state"], "escalated");
    assert!(s
        .get("/api/tasks/1/events")
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["kind"] == "abort_raced_completion"));
}

#[test]
fn stale_generation_abort_is_not_delivered() {
    use axe::{db, remote};
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("db")).unwrap();
    let actor = db::Actor::new("human");
    db::add_task(
        &mut conn,
        &actor,
        &db::NewTask {
            agent: None,
            project: "p",
            title: "t",
            detail: "",
            state: "pending",
        },
    )
    .unwrap();
    let reg = remote::register(&conn, &json!({"id":"d","generation":""})).unwrap();
    let op = |name: &str, body: Value| {
        remote::operation(
            &conn,
            &json!({"dispatcher":"d","generation":reg["generation"],"operation_id":uuid::Uuid::new_v4().to_string(),"op":name,"body":body}),
        )
    };
    op("claim", json!({"project":"p","claim":"dispatch:test"})).unwrap();
    let run = op(
        "prepare",
        json!({"task":1,"claim":"dispatch:test","agent":"a","mode":"fresh","log":"/unused"}),
    )
    .unwrap()
    .as_i64()
    .unwrap();
    // A request accepted during prepare is not erased by the started operation.
    remote::abort(&conn, 1, run, &actor).unwrap();
    op("started", json!({"run":run})).unwrap();
    assert_eq!(
        remote::pending_aborts(&conn, "d", reg["generation"].as_str().unwrap()).unwrap(),
        vec![run]
    );
    assert!(remote::pending_aborts(&conn, "d", "other-generation")
        .unwrap()
        .is_empty());
    conn.execute("UPDATE dispatcher SET generation='replacement'", [])
        .unwrap();
    assert!(remote::abort(&conn, 1, run, &actor)
        .unwrap_err()
        .to_string()
        .contains("stale"));
    assert!(op("finish", json!({"run":run,"aborted":true})).is_err());
    assert!(db::get_run(&conn, run).unwrap().ended_at.is_none());
}
