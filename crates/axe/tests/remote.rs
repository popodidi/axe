mod common;
use axe::{db, remote};
use common::{Server, TOKEN};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn wait(mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < until, "condition timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[test]
fn operations_are_atomic_replayable_and_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("db")).unwrap();
    db::add_task(
        &mut c,
        &db::Actor::new("human"),
        &db::NewTask {
            agent: None,
            project: "p",
            title: "t",
            detail: "",
            state: "pending",
        },
    )
    .unwrap();
    let reg = remote::register(&c, &json!({"id":"one","generation":""})).unwrap();
    let generation = reg["generation"].clone();
    assert_eq!(
        remote::register(&c, &json!({"id":"one","generation":""})).unwrap(),
        reg
    );
    let request = |id: &str, op: &str, body: Value| json!({"dispatcher":"one","generation":generation,"operation_id":id,"op":op,"body":body});
    assert_eq!(
        remote::operation(&c, &request("pending", "pending", json!({}))).unwrap(),
        json!(["p"])
    );
    let q = request(
        "claim",
        "claim",
        json!({"project":"p","claim":"dispatch:c","max_runs":3}),
    );
    let a = remote::operation(&c, &q).unwrap();
    assert_eq!(a, remote::operation(&c, &q).unwrap());
    assert_eq!(db::get_task(&c, 1).unwrap().runs, 1);
    assert!(remote::operation(&c, &request("claim", "claim", json!({"claim":"other"}))).is_err());
    // prepare saves the task's single session row.
    let session = json!({"agent":"a","session_id":"s1","resume_count":0});
    let prep = request(
        "prepare",
        "prepare",
        json!({"task":1,"claim":"dispatch:c","agent":"a","mode":"fresh","session_id":"s1","log":"/remote/log","session":session}),
    );
    let id = remote::operation(&c, &prep).unwrap();
    assert_eq!(id, remote::operation(&c, &prep).unwrap());
    let stored = db::session(&c, 1).unwrap().unwrap();
    assert_eq!(
        (stored.agent.as_str(), stored.session_id.as_str()),
        ("a", "s1")
    );
    assert_eq!(
        remote::operation(&c, &request("session", "session", json!({"task":1}))).unwrap()
            ["session_id"],
        "s1"
    );
    let start = request("start", "started", json!({"run":id}));
    remote::operation(&c, &start).unwrap();
    remote::operation(&c, &start).unwrap();
    let finish = request(
        "finish",
        "finish",
        json!({"run":id,"code":0,"resumed":false,"note":"test exit","event":{}}),
    );
    remote::operation(&c, &finish).unwrap();
    remote::operation(&c, &finish).unwrap();
    let events = db::events(&c, 1).unwrap();
    for kind in ["claimed", "run_start", "run_end"] {
        assert_eq!(events.iter().filter(|e| e.kind == kind).count(), 1);
    }
    let run = db::get_run(&c, id.as_i64().unwrap()).unwrap();
    assert_eq!((run.agent.as_str(), run.mode.as_str()), ("a", "fresh"));
    let mut stale = q.clone();
    stale["generation"] = json!("stale");
    assert!(remote::operation(&c, &stale).is_err());
    assert!(remote::register(&c, &json!({"id":"two","generation":""})).is_err());
}

#[test]
fn prepare_without_a_session_clears_the_stored_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("db")).unwrap();
    db::add_task(
        &mut c,
        &db::Actor::new("human"),
        &db::NewTask {
            agent: None,
            project: "p",
            title: "t",
            detail: "",
            state: "pending",
        },
    )
    .unwrap();
    db::save_session(
        &c,
        1,
        &db::Session {
            agent: "a".into(),
            session_id: "old".into(),
            resume_count: 2,
        },
    )
    .unwrap();
    let r = remote::register(&c, &json!({"id":"one","generation":""})).unwrap();
    let request = |id: &str, op: &str, body: Value| json!({"dispatcher":"one","generation":r["generation"],"operation_id":id,"op":op,"body":body});
    remote::operation(
        &c,
        &request(
            "claim",
            "claim",
            json!({"project":"p","claim":"dispatch:c"}),
        ),
    )
    .unwrap();
    // A JSONL agent's fresh run has no session id until it exits.
    remote::operation(
        &c,
        &request(
            "prepare",
            "prepare",
            json!({"task":1,"claim":"dispatch:c","agent":"a","mode":"fresh","log":"/l","session":null}),
        ),
    )
    .unwrap();
    assert!(db::session(&c, 1).unwrap().is_none());
}

#[test]
fn expired_lease_does_not_requeue_or_allow_replacement_of_live_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("db")).unwrap();
    db::add_task(
        &mut c,
        &db::Actor::new("human"),
        &db::NewTask {
            agent: None,
            project: "p",
            title: "t",
            detail: "",
            state: "pending",
        },
    )
    .unwrap();
    let r = remote::register(&c, &json!({"id":"a","generation":""})).unwrap();
    remote::operation(&c,&json!({"dispatcher":"a","generation":r["generation"],"operation_id":"q","op":"claim","body":{"claim":"dispatch:c"}})).unwrap();
    c.execute("UPDATE dispatcher SET expires=0", []).unwrap();
    assert!(remote::register(&c, &json!({"id":"b","generation":""})).is_err());
    assert_eq!(db::get_task(&c, 1).unwrap().state, "in_progress");
    remote::reconcile(
        &c,
        &json!({"id":"a","generation":r["generation"],"note":"operator stopped old worker"}),
    )
    .unwrap();
    assert_eq!(db::get_task(&c, 1).unwrap().state, "escalated");
    assert!(remote::register(&c, &json!({"id":"b","generation":""})).is_ok());
}

#[test]
fn separate_dispatcher_serves_logs_and_server_never_spawns() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    let worker = dir.path().join("worker");
    std::fs::create_dir_all(worker.join("p")).unwrap();
    std::fs::write(
        &cfg,
        format!(
            r#"workspace_root={ws:?}
log_dir={logs:?}
poll_seconds=1
[defaults]
agents=["fixture"]
[agents.fixture]
cmd=["sh","-c","echo first; sleep 2; echo second"]
"#,
            ws = worker.to_str().unwrap(),
            logs = worker.join("logs").to_str().unwrap()
        ),
    )
    .unwrap();
    let s = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    s.ok(&["add", "--project", "p", "--title", "test"]);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(s.get("/api/tasks/1")["state"], "pending");
    let mut d = s.dispatcher(&[], &[]);
    wait(|| !s.get("/api/runs").as_array().unwrap().is_empty());
    wait(|| {
        s.get("/api/runs/1/log")["page"]["lines"]
            .as_array()
            .is_some_and(|l| !l.is_empty())
    });
    wait(|| s.get("/api/runs/1/log")["log_complete"] == true);
    let log = s.get("/api/runs/1/log");
    assert!(log["page"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["text"] == "second"));
    d.0.kill().unwrap();
    d.0.wait().unwrap();
    wait(|| s.get("/api/runs/1/log")["retryable"] == true);
    let _d = s.dispatcher(&[], &[]);
    wait(|| s.get("/api/runs/1/log")["page"].is_object());
}

#[test]
fn crashed_dispatcher_requires_reconciliation_then_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    let worker = dir.path().join("worker");
    std::fs::create_dir_all(worker.join("p")).unwrap();
    std::fs::write(
        &cfg,
        format!(
            r#"workspace_root={ws:?}
log_dir={logs:?}
poll_seconds=1
[defaults]
agents=["fixture"]
[agents.fixture]
cmd=["sh","-c","sleep 3"]
"#,
            ws = worker.to_str().unwrap(),
            logs = worker.join("logs").to_str().unwrap()
        ),
    )
    .unwrap();
    let s = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    s.ok(&["add", "--project", "p", "--title", "test"]);
    let mut d = s.dispatcher(&[], &[]);
    wait(|| s.get("/api/runs")[0]["started_at"].is_string());
    // A crash leaves the claim and run unresolved on the server.
    d.0.kill().unwrap();
    d.0.wait().unwrap();
    let refused = s.dispatcher(&["--once"], &[]).wait();
    assert!(!refused.status.success(), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("reconcile"));
    assert_eq!(s.get("/api/tasks/1")["state"], "in_progress");
    let reconciled = s
        .dispatcher(&["--reconcile-note", "old worker stopped"], &[])
        .wait();
    assert!(reconciled.status.success(), "{reconciled:?}");
    let task = s.get("/api/tasks/1");
    assert_eq!(task["state"], "escalated");
    assert!(s.get("/api/runs")[0]["ended_at"].is_string());
    let restarted = s.dispatcher(&["--once"], &[]).wait();
    assert!(restarted.status.success(), "{restarted:?}");
}
