mod common;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

use axe::db::{self, Actor, Changes, Claim, NewTask};
use axe::dispatch::{format_event, format_show};
use common::{server_cmd, Server, BIN, TOKEN};
use serde_json::Value;

/// Task fields that do not depend on wall-clock time.
fn task_key(t: &db::Task) -> (i64, String, String, String, String, Option<String>, i64) {
    (
        t.id,
        t.project.clone(),
        t.title.clone(),
        t.detail.clone(),
        t.state.clone(),
        t.claimed_by.clone(),
        t.runs,
    )
}

type EventKey = (
    i64,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn event_keys(events: &[db::Event]) -> Vec<EventKey> {
    events
        .iter()
        .map(|e| {
            (
                e.task_id,
                e.actor.clone(),
                e.session.clone(),
                e.kind.clone(),
                e.from_value.clone(),
                e.to_value.clone(),
                e.body.clone(),
            )
        })
        .collect()
}

fn json_of<T: serde::de::DeserializeOwned>(text: &str) -> T {
    serde_json::from_str(text).unwrap()
}

#[test]
fn cli_commands_match_pre_server_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let spec = dir.path().join("spec.md");
    std::fs::write(&spec, "## Outcome\nold line\n## Verify\nrun tests\n").unwrap();
    let spec2 = dir.path().join("spec2.md");
    std::fs::write(&spec2, "## Outcome\nnew line\n## Verify\nrun tests\n").unwrap();
    let note_file = dir.path().join("note.md");
    std::fs::write(&note_file, "multi\nline note\n").unwrap();

    // Same operations through the library on a reference database (pre-server behavior).
    let mut reference = db::open(&dir.path().join("ref.db")).unwrap();
    let human = Actor::new("human");
    let agent = Actor {
        name: "agent".into(),
        session: Some("s1".into()),
    };
    let ops = Actor::new("ops");

    assert_eq!(
        srv.ok(&[
            "add",
            "--project",
            "orchid",
            "--title",
            "Fix X",
            "--detail-file",
            spec.to_str().unwrap(),
        ]),
        "1\n"
    );
    db::add_task(
        &mut reference,
        &human,
        &NewTask {
            agent: None,
            project: "orchid",
            title: "Fix X",
            detail: "## Outcome\nold line\n## Verify\nrun tests\n",
            state: "pending",
        },
    )
    .unwrap();
    assert_eq!(
        srv.ok(&[
            "add",
            "--project",
            "fern",
            "--title",
            "Other",
            "--detail",
            "d"
        ]),
        "2\n"
    );
    db::add_task(
        &mut reference,
        &human,
        &NewTask {
            agent: None,
            project: "fern",
            title: "Other",
            detail: "d",
            state: "pending",
        },
    )
    .unwrap();
    let out = srv
        .cli(&["set", "1", "--state", "blocked", "--note", "waiting"])
        .env("AXE_ACTOR", "agent")
        .env("AXE_SESSION", "s1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    db::set_task(
        &mut reference,
        &agent,
        1,
        &Changes {
            state: Some("blocked".into()),
            note: Some("waiting".into()),
            ..Default::default()
        },
    )
    .unwrap();
    srv.ok(&["set", "1", "--state", "pending"]);
    db::set_task(
        &mut reference,
        &human,
        1,
        &Changes {
            state: Some("pending".into()),
            ..Default::default()
        },
    )
    .unwrap();

    srv.ok(&[
        "--actor",
        "ops",
        "set",
        "1",
        "--title",
        "Fix Y",
        "--detail-file",
        spec2.to_str().unwrap(),
    ]);
    db::set_task(
        &mut reference,
        &ops,
        1,
        &Changes {
            title: Some("Fix Y".into()),
            detail: Some("## Outcome\nnew line\n## Verify\nrun tests\n".into()),
            ..Default::default()
        },
    )
    .unwrap();

    srv.ok(&["note", "1", "a note"]);
    db::append(&mut reference, &human, 1, "note", "a note").unwrap();
    srv.ok(&["note", "1", "--file", note_file.to_str().unwrap()]);
    db::append(&mut reference, &human, 1, "note", "multi\nline note\n").unwrap();
    srv.ok(&["artifact", "1", "https://github.com/o/r/pull/1"]);
    db::append(
        &mut reference,
        &human,
        1,
        "artifact",
        "https://github.com/o/r/pull/1",
    )
    .unwrap();

    assert_eq!(
        srv.ok(&["claim", "--project", "orchid", "--by", "me"]),
        "1\n"
    );
    assert!(matches!(
        db::claim(&mut reference, &human, Some("orchid"), "me", None).unwrap(),
        Claim::Claimed(_)
    ));
    let none = srv.run(&["claim", "--project", "orchid", "--by", "me2"]);
    assert_eq!(none.status.code(), Some(1), "{none:?}");
    assert!(none.stdout.is_empty());
    assert!(matches!(
        db::claim(&mut reference, &human, Some("orchid"), "me2", None).unwrap(),
        Claim::None
    ));
    srv.ok(&["set", "2", "--state", "done"]);
    db::set_task(
        &mut reference,
        &human,
        2,
        &Changes {
            state: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // Tasks and events match the reference, field for field (timestamps aside).
    let all: Vec<db::Task> = json_of(&srv.ok(&["list", "--all", "--json"]));
    let ref_all = db::list_tasks(
        &reference,
        &db::Filter {
            include_done: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        all.iter().map(task_key).collect::<Vec<_>>(),
        ref_all.iter().map(task_key).collect::<Vec<_>>()
    );
    for id in [1, 2] {
        let events: Vec<db::Event> = json_of(&srv.ok(&["log", &id.to_string(), "--json"]));
        assert_eq!(
            event_keys(&events),
            event_keys(&db::events(&reference, id).unwrap())
        );
    }

    // List filters.
    let ids = |args: &[&str]| -> Vec<i64> {
        let mut a = vec!["list", "--json"];
        a.extend_from_slice(args);
        json_of::<Vec<db::Task>>(&srv.ok(&a))
            .iter()
            .map(|t| t.id)
            .collect()
    };
    assert_eq!(ids(&[]), [1]);
    assert_eq!(ids(&["--all"]), [1, 2]);
    assert_eq!(ids(&["--state", "done"]), [2]);
    assert_eq!(ids(&["--project", "fern", "--all"]), [2]);
    assert!(ids(&["--project", "fern"]).is_empty());
    let table = srv.ok(&["list", "--all"]);
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[0], "ID  PROJECT  STATE        RUNS  TITLE");
    assert_eq!(lines[1], "1   orchid   in_progress  1     Fix Y");
    assert_eq!(lines[2], "2   fern     done         0     Other");

    // show / log text output uses the same formatting as before.
    let t: db::Task = json_of(&srv.ok(&["show", "1", "--json"]));
    assert_eq!(srv.ok(&["show", "1"]), format!("{}\n", format_show(&t)));
    assert!(srv
        .ok(&["show", "1"])
        .ends_with("\n\n## Outcome\nnew line\n## Verify\nrun tests\n\n"));
    let events: Vec<db::Event> = json_of(&srv.ok(&["log", "1", "--json"]));
    let text: String = events
        .iter()
        .map(|e| format_event(e, Some(300)) + "\n")
        .collect();
    assert_eq!(srv.ok(&["log", "1"]), text);
    let held = &events[1];
    assert_eq!(
        (held.actor.as_str(), held.session.as_deref()),
        ("agent", Some("s1"))
    );

    // Errors surface like before: non-zero exit and the server's message.
    let missing = srv.run(&["show", "99"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("task 99 not found"));
    assert!(!srv.run(&["log", "99"]).status.success());
    assert!(!srv.run(&["note", "99", "x"]).status.success());
    assert!(!srv.run(&["set", "1", "--state", "weird"]).status.success());
}

fn post(url: &str, auth: Option<&str>, body: &str) -> u16 {
    let mut req = ureq::post(url).set("Content-Type", "application/json");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    match req.send_string(body) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn writes_require_token_and_reads_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "t"]);
    let before_tasks = srv.get("/api/tasks?all=1");
    let before_events = srv.get("/api/tasks/1/events");

    let add = r#"{"project":"p","title":"evil"}"#;
    let wrong = Some("Bearer not-the-token");
    let url = |p: &str| format!("{}{p}", srv.url);
    for auth in [None, wrong, Some(TOKEN)] {
        assert_eq!(post(&url("/api/tasks"), auth, add), 401, "{auth:?}");
        assert_eq!(
            post(&url("/api/tasks/1/notes"), auth, r#"{"body":"x"}"#),
            401
        );
        assert_eq!(
            post(&url("/api/tasks/1/artifacts"), auth, r#"{"value":"x"}"#),
            401
        );
        assert_eq!(post(&url("/api/claim"), auth, r#"{"by":"x"}"#), 401);
        let patch = ureq::request("PATCH", &url("/api/tasks/1"));
        let patch = match auth {
            Some(a) => patch.set("Authorization", a),
            None => patch,
        };
        assert!(matches!(
            patch.send_string(r#"{"state":"done"}"#),
            Err(ureq::Error::Status(401, _))
        ));
    }
    let bearer = format!("Bearer {TOKEN}");
    let wrong_cli = srv
        .cli(&["note", "1", "x"])
        .env("AXE_TOKEN", "wrong")
        .output()
        .unwrap();
    assert!(!wrong_cli.status.success());
    assert!(String::from_utf8_lossy(&wrong_cli.stderr).contains("unauthorized"));
    let no_token = srv
        .cli(&["note", "1", "x"])
        .env("AXE_TOKEN_FILE", dir.path().join("absent"))
        .output()
        .unwrap();
    assert!(!no_token.status.success());

    // Nothing changed.
    assert_eq!(srv.get("/api/tasks?all=1"), before_tasks);
    assert_eq!(srv.get("/api/tasks/1/events"), before_events);

    // Reads need no token; the right token (env or file) writes.
    for path in [
        "/healthz",
        "/api/tasks",
        "/api/tasks/1",
        "/api/tasks/1/events",
        "/api/runs",
        "/api/overview",
        "/api/config",
    ] {
        assert_eq!(
            ureq::get(&url(path)).call().unwrap().status(),
            200,
            "{path}"
        );
    }
    assert_eq!(post(&url("/api/tasks"), Some(&bearer), add), 201);
    let env_token = srv
        .cli(&["note", "1", "via env"])
        .env("AXE_TOKEN", TOKEN)
        .env("AXE_TOKEN_FILE", dir.path().join("absent"))
        .output()
        .unwrap();
    assert!(env_token.status.success(), "{env_token:?}");

    // The config endpoint never exposes the token.
    let cfg = ureq::get(&url("/api/config"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(!cfg.contains(TOKEN));
    let cfg: Value = serde_json::from_str(&cfg).unwrap();
    assert_eq!(cfg["auth"], true);
    assert_eq!(
        cfg["config"]["server"]["cors_origins"],
        serde_json::json!([])
    );
}

#[test]
fn refuses_non_loopback_listen_without_token() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "").unwrap();
    let db = dir.path().join("t.db");
    let token = dir.path().join("token");
    for content in [None, Some("  \n")] {
        if let Some(c) = content {
            std::fs::write(&token, c).unwrap();
        }
        let out = server_cmd(&db, &cfg, &token, &["--listen", "0.0.0.0:0"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("refusing to listen"),
            "{out:?}"
        );
    }
    std::fs::write(&token, TOKEN).unwrap();
    let mut child = server_cmd(&db, &cfg, &token, &["--listen", "0.0.0.0:0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        line.starts_with("axe server listening on http://0.0.0.0:"),
        "{line}"
    );
}

#[test]
fn concurrent_cli_claims_have_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "t"]);
    let children: Vec<_> = (0..12)
        .map(|i| {
            srv.cli(&["claim", "--by", &format!("c{i}")])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();
    let winners = outputs.iter().filter(|o| o.status.success()).count();
    assert_eq!(winners, 1, "{outputs:?}");
    assert!(outputs
        .iter()
        .filter(|o| !o.status.success())
        .all(|o| o.status.code() == Some(1)));
    let t = srv.get("/api/tasks/1");
    assert_eq!(t["runs"], 1);
    assert_eq!(t["state"], "in_progress");
    let kinds: Vec<Value> = srv
        .get("/api/tasks/1/events")
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].clone())
        .collect();
    assert_eq!(kinds, ["created", "claimed"]);
}

#[test]
fn read_api_overview_sessions_cors_and_stream() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[server]\ncors_origins = [\"http://dash.example\"]\n",
    )
    .unwrap();
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    srv.ok(&["add", "--project", "p", "--title", "a"]);
    srv.ok(&["add", "--project", "p", "--title", "b"]);
    srv.ok(&[
        "add",
        "--project",
        "q",
        "--title",
        "c",
        "--state",
        "blocked",
    ]);
    srv.ok(&["add", "--project", "q", "--title", "d"]);
    srv.ok(&[
        "--actor",
        "agent",
        "set",
        "4",
        "--state",
        "blocked",
        "--note",
        "blocked: PR head moved\nresolver: ops\nresume: re-verify",
    ]);
    srv.ok(&["add", "--project", "q", "--title", "e", "--state", "done"]);

    // The ops queue is every blocked task.
    let ov = srv.get("/api/overview");
    assert_eq!(ov["running"], 0);
    let ops: Vec<(i64, &str)> = ov["ops_queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["id"].as_i64().unwrap(), t["state"].as_str().unwrap()))
        .collect();
    assert_eq!(ops, [(3, "blocked"), (4, "blocked")]);
    let d = &ov["ops_queue"][1];
    assert_eq!(d["blocked_by"], "agent");
    assert_eq!(
        d["blocker"],
        "blocked: PR head moved\nresolver: ops\nresume: re-verify"
    );
    assert_eq!(ov["ops_queue"][0]["blocker"], Value::Null);
    assert_eq!(
        ov["counts"],
        serde_json::json!([
            {"project": "p", "state": "pending", "count": 2},
            {"project": "q", "state": "blocked", "count": 2},
            {"project": "q", "state": "done", "count": 1},
        ])
    );
    // Task filters select by state and project.
    let blocked: Vec<db::Task> = json_of(&srv.ok(&["list", "--state", "blocked", "--json"]));
    assert_eq!(blocked.iter().map(|t| t.id).collect::<Vec<_>>(), [3, 4]);
    assert_eq!(srv.get("/api/runs?live=1"), serde_json::json!([]));
    assert!(matches!(
        ureq::get(&format!("{}/api/sessions/nope", srv.url)).call(),
        Err(ureq::Error::Status(404, _))
    ));
    assert!(matches!(
        ureq::get(&format!("{}/api/tasks/9", srv.url)).call(),
        Err(ureq::Error::Status(404, _))
    ));
    assert!(matches!(
        ureq::get(&format!("{}/api/tasks?state=weird", srv.url)).call(),
        Err(ureq::Error::Status(400, _))
    ));

    // CORS: configured origins may read; nothing else gets the header.
    let allowed = ureq::get(&format!("{}/api/tasks", srv.url))
        .set("Origin", "http://dash.example")
        .call()
        .unwrap();
    assert_eq!(
        allowed.header("Access-Control-Allow-Origin"),
        Some("http://dash.example")
    );
    let other = ureq::get(&format!("{}/api/tasks", srv.url))
        .set("Origin", "http://evil.example")
        .call()
        .unwrap();
    assert_eq!(other.header("Access-Control-Allow-Origin"), None);
    let preflight = ureq::request("OPTIONS", &format!("{}/api/tasks", srv.url))
        .set("Origin", "http://dash.example")
        .call()
        .unwrap();
    assert_eq!(
        preflight.header("Access-Control-Allow-Methods"),
        Some("GET")
    );

    // SSE: an initial change event, then one after a write.
    let stream = ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(20))
        .build()
        .get(&format!("{}/api/stream", srv.url))
        .call()
        .unwrap();
    assert_eq!(stream.header("Content-Type"), Some("text/event-stream"));
    let mut lines = BufReader::new(stream.into_reader()).lines();
    let mut next_seq = || -> u64 {
        loop {
            let line = lines.next().unwrap().unwrap();
            if let Some(data) = line.strip_prefix("data: ") {
                return serde_json::from_str::<Value>(data).unwrap()["seq"]
                    .as_u64()
                    .unwrap();
            }
        }
    };
    let first = next_seq();
    srv.ok(&["note", "1", "ping"]);
    assert!(next_seq() > first);
}

#[test]
fn dispatcher_requires_a_server_without_creating_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, format!("log_dir={:?}", dir.path().join("logs"))).unwrap();
    let db = dir.path().join("must-not-exist.db");
    let out = std::process::Command::new(BIN)
        .args(["dispatch", "--once"])
        .env("AXE_CONFIG", cfg)
        .env("AXE_DB", &db)
        .env("AXE_SERVER", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!db.exists());
}

#[test]
fn approve_endpoint_resumes_held_tasks_and_refuses_dispatched_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    for (title, state) in [
        ("ship", "blocked"),
        ("escalated", "escalated"),
        ("guarded", "blocked"),
        ("queued", "pending"),
    ] {
        srv.ok(&["add", "--project", "p", "--title", title, "--state", state]);
    }

    let url = |id: i64| format!("{}/api/tasks/{id}/approve", srv.url);
    let auth = format!("Bearer {TOKEN}");
    let post_json = |id: i64, body: Value| -> (u16, Value) {
        let r = ureq::post(&url(id))
            .set("Authorization", &auth)
            .send_json(body);
        match r {
            Ok(r) => (r.status(), r.into_json().unwrap()),
            Err(ureq::Error::Status(code, r)) => (code, r.into_json().unwrap()),
            Err(e) => panic!("{e}"),
        }
    };

    let (code, v) = post_json(
        1,
        serde_json::json!({"sha": "abc", "scope": "prod", "note": "go", "actor": "ops"}),
    );
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["task"]["state"], "pending");
    let ev: Vec<db::Event> = json_of(&srv.ok(&["log", "1", "--json"]));
    let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["created", "approval", "state"]);
    assert_eq!(ev[1].body.as_deref(), Some("sha=abc scope=prod by=ops\ngo"));

    // An escalated task is approved through the CLI.
    srv.ok(&["approve", "2", "--sha", "abc", "--scope", "prod"]);
    assert_eq!(srv.get("/api/tasks/2")["state"], "pending");

    // Pending work, blank fields, unknown task.
    let (code, v) = post_json(4, serde_json::json!({"sha": "abc", "scope": "prod"}));
    assert_eq!(code, 400, "{v}");
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("only blocked or escalated tasks"));
    assert_eq!(srv.get("/api/tasks/4")["state"], "pending");
    for body in [
        serde_json::json!({"sha": " ", "scope": "prod"}),
        serde_json::json!({"sha": "abc", "scope": ""}),
    ] {
        assert_eq!(post_json(3, body).0, 400);
    }
    assert_eq!(
        post_json(99, serde_json::json!({"sha": "a", "scope": "b"})).0,
        404
    );

    // The dispatcher and dispatched agents are refused.
    for body in [
        serde_json::json!({"sha": "abc", "scope": "prod", "actor": "agent"}),
        serde_json::json!({"sha": "abc", "scope": "prod", "actor": "dispatcher"}),
        serde_json::json!({"sha": "abc", "scope": "prod", "session": "dispatch:x"}),
    ] {
        let (code, v) = post_json(3, body);
        assert_eq!(code, 403, "{v}");
    }
    let cli = srv
        .cli(&["approve", "3", "--sha", "abc", "--scope", "prod"])
        .env("AXE_ACTOR", "agent")
        .output()
        .unwrap();
    assert!(!cli.status.success());
    assert!(String::from_utf8_lossy(&cli.stderr).contains("dispatched agents cannot approve"));

    // A live run's session is refused; once its run ends it may approve.
    let conn = db::open(&srv.db).unwrap();
    let run = db::insert_run(
        &conn,
        &db::NewRun {
            task_id: 3,
            project: "p",
            agent: "claude",
            mode: "fresh",
            session_id: Some("sess-live"),
            claim: "dispatch:live",
            log_path: "/dev/null",
        },
    )
    .unwrap();
    let (code, v) = post_json(
        3,
        serde_json::json!({"sha": "abc", "scope": "prod", "session": "sess-live"}),
    );
    assert_eq!(code, 403, "{v}");
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("running dispatcher session"));
    assert_eq!(srv.get("/api/tasks/3")["state"], "blocked");

    db::end_run(&conn, run, Some(0), None).unwrap();
    let ok = srv
        .cli(&["approve", "3", "--sha", "abc", "--scope", "prod"])
        .env("AXE_SESSION", "sess-live")
        .output()
        .unwrap();
    assert!(ok.status.success(), "{ok:?}");
    assert_eq!(srv.get("/api/tasks/3")["state"], "pending");
}

#[test]
fn session_lookup_returns_task_agent_and_latest_run() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "t"]);
    let conn = db::open(&srv.db).unwrap();
    // A stored session with no run yet.
    db::save_session(
        &conn,
        1,
        &db::Session {
            agent: "codex".into(),
            session_id: "s-stored".into(),
            resume_count: 0,
        },
    )
    .unwrap();
    let s = srv.get("/api/sessions/s-stored");
    assert_eq!(
        s,
        serde_json::json!({"session_id": "s-stored", "task_id": 1, "project": "p", "agent": "codex", "run": null})
    );
    let run = db::insert_run(
        &conn,
        &db::NewRun {
            task_id: 1,
            project: "p",
            agent: "claude",
            mode: "resume",
            session_id: Some("s-run"),
            claim: "dispatch:c",
            log_path: "/dev/null",
        },
    )
    .unwrap();
    let s = srv.get("/api/sessions/s-run");
    assert_eq!(
        (&s["task_id"], &s["agent"], &s["run"]["id"]),
        (&1.into(), &"claude".into(), &run.into())
    );
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys(&s),
        ["agent", "project", "run", "session_id", "task_id"]
    );
    assert_eq!(
        keys(&s["run"]),
        [
            "agent",
            "claim",
            "current_activity",
            "ended_at",
            "exit_code",
            "id",
            "last_activity_at",
            "live",
            "log_path",
            "mode",
            "project",
            "session_id",
            "started_at",
            "task_id"
        ]
    );
    // The dispatcher claim maps to the same run.
    assert_eq!(srv.get("/api/sessions/dispatch:c")["run"]["id"], run);
}

#[test]
fn reset_runs_api_and_cli_preserve_continuity_and_reopen_budget() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let id: i64 = srv
        .ok(&["add", "--project", "orchid", "--title", "reset"])
        .trim()
        .parse()
        .unwrap();
    let mut conn = db::open(&srv.db).unwrap();
    let actor = Actor::new("dispatcher");
    assert!(matches!(
        db::claim(&mut conn, &actor, None, "dispatch:test", Some(1)).unwrap(),
        Claim::Claimed(_)
    ));
    let before = db::events(&conn, id).unwrap().len();
    let url = format!("{}/api/tasks/{id}", srv.url);
    let auth = format!("Bearer {TOKEN}");
    let patch = |authenticated: bool, body: Value| {
        let mut req = ureq::patch(&url);
        if authenticated {
            req = req.set("Authorization", &auth);
        }
        req.send_json(body).map_err(Box::new)
    };
    assert!(matches!(
        patch(false, serde_json::json!({"reset_runs": true})),
        Err(e) if matches!(*e, ureq::Error::Status(401, _))
    ));
    assert_eq!(db::get_task(&conn, id).unwrap().runs, 1);
    let err = patch(
        true,
        serde_json::json!({"reset_runs": true, "state": "pending", "title": "should not change"}),
    )
    .unwrap_err();
    match *err {
        ureq::Error::Status(400, response) => {
            assert!(response.into_string().unwrap().contains("dispatcher claim"))
        }
        e => panic!("{e}"),
    }
    assert_eq!(db::events(&conn, id).unwrap().len(), before);
    assert_eq!(db::get_task(&conn, id).unwrap().title, "reset");
    db::set_task(
        &mut conn,
        &actor,
        id,
        &Changes {
            state: Some("pending".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        db::claim(&mut conn, &actor, None, "dispatch:test", Some(1)).unwrap(),
        Claim::Blocked(_)
    ));
    let session = db::Session {
        agent: "codex".into(),
        session_id: "stored".into(),
        resume_count: 3,
    };
    db::save_session(&conn, id, &session).unwrap();
    let stream = ureq::AgentBuilder::new()
        .timeout_read(Duration::from_secs(5))
        .build()
        .get(&format!("{}/api/stream", srv.url))
        .call()
        .unwrap();
    let mut lines = BufReader::new(stream.into_reader()).lines();
    let mut next_seq = || -> u64 {
        loop {
            let line = lines.next().unwrap().unwrap();
            if let Some(data) = line.strip_prefix("data: ") {
                return serde_json::from_str::<Value>(data).unwrap()["seq"]
                    .as_u64()
                    .unwrap();
            }
        }
    };
    let first = next_seq();
    let output = srv
        .cli(&[
            "set",
            &id.to_string(),
            "--reset-runs",
            "--state",
            "pending",
            "--note",
            "test",
        ])
        .env("AXE_ACTOR", "ops")
        .env("AXE_SESSION", "ops-session")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(next_seq() > first);
    assert!(srv.ok(&["show", &id.to_string()]).contains("runs: 0"));
    assert!(srv.ok(&["log", &id.to_string()]).contains("runs 1 -> 0"));
    let events = db::events(&conn, id).unwrap();
    let reset = events.iter().find(|e| e.kind == "runs").unwrap();
    assert_eq!(reset.from_value.as_deref(), Some("1"));
    assert_eq!(reset.to_value.as_deref(), Some("0"));
    assert_eq!(reset.actor, "ops");
    assert_eq!(reset.session.as_deref(), Some("ops-session"));
    let retained = db::session(&conn, id).unwrap().unwrap();
    assert_eq!(retained.session_id, "stored");
    assert_eq!(retained.resume_count, 3);
    assert_eq!(retained.agent, "codex");
    assert!(matches!(
        db::claim(&mut conn, &actor, None, "manual", Some(1)).unwrap(),
        Claim::Claimed(_)
    ));
    patch(true, serde_json::json!({"reset_runs": true, "note": "API reset", "actor": "api-ops", "session": "api-session"})).unwrap();
    assert_eq!(db::get_task(&conn, id).unwrap().runs, 0);
    let reset = db::events(&conn, id)
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == "runs")
        .unwrap();
    assert_eq!(reset.actor, "api-ops");
    assert_eq!(reset.session.as_deref(), Some("api-session"));
    assert!(srv.ok(&["set", "--help"]).contains("--reset-runs"));
}

#[test]
fn run_log_api_is_record_bound_and_preserves_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "orchid", "--title", "logs"]);
    let relay = common::LogRelay::start(&srv);
    let conn = db::open(&srv.db).unwrap();
    let path = dir.path().join("first.log");
    std::fs::write(&path, "<script>bad()</script>\n{\"type\":").unwrap();
    let insert = |path: &std::path::Path| {
        db::insert_run(
            &conn,
            &db::NewRun {
                task_id: 1,
                project: "orchid",
                agent: "codex",
                mode: "fresh",
                session_id: None,
                claim: "fixture",
                log_path: path.to_str().unwrap(),
            },
        )
        .unwrap()
    };
    let first = insert(&path);
    relay.add(&conn, first, &path, false);
    let p = srv.get(&format!("/api/runs/{first}/log"));
    assert_eq!(p["run"]["id"], first);
    assert_eq!(p["page"]["lines"][0]["text"], "<script>bad()</script>");
    let offset = p["page"]["end"].as_u64().unwrap();
    let generation = p["page"]["generation"].as_str().unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "\"turn.completed\"}}").unwrap();
    db::end_run(&conn, first, Some(0), None).unwrap();
    relay.add(&conn, first, &path, true);
    let query = format!("/api/runs/{first}/log?after={offset}&generation={generation}");
    let tail = srv.get(&query);
    assert_eq!(tail["page"]["lines"].as_array().unwrap().len(), 1);
    assert_eq!(tail["run"]["exit_code"], 0);
    assert_eq!(tail, srv.get(&query));
    let second_path = dir.path().join("second.log");
    std::fs::write(&second_path, "second attempt\n").unwrap();
    let second = insert(&second_path);
    relay.add(&conn, second, &second_path, true);
    db::end_run(&conn, second, Some(7), None).unwrap();
    assert_eq!(
        srv.get(&format!("/api/runs/{second}/log"))["run"]["exit_code"],
        7
    );
    assert_eq!(
        srv.get(&format!("/api/runs/{first}/log"))["page"]["lines"][0]["text"],
        "<script>bad()</script>"
    );
    let missing = insert(&dir.path().join("missing.log"));
    assert!(srv.get(&format!("/api/runs/{missing}/log"))["unavailable"].is_string());
    for (route, status) in [
        ("/api/runs/999999/log".to_string(), 404),
        ("/api/runs/%2e%2e%2fetc%2fpasswd/log".to_string(), 400),
        (format!("/api/runs/{first}/log?path=/etc/passwd"), 400),
        (format!("/api/runs/{first}/log?after=-1"), 400),
        (format!("/api/runs/{first}/log?after=0&before=1"), 400),
    ] {
        let error = ureq::get(&format!("{}{route}", srv.url))
            .call()
            .unwrap_err();
        assert!(
            matches!(error, ureq::Error::Status(s, _) if s == status),
            "{error}"
        );
    }
    // Older records with a reused path must never expose a newer attempt.
    insert(&path);
    assert!(srv.get(&format!("/api/runs/{first}/log"))["unavailable"]
        .as_str()
        .unwrap()
        .contains("reused"));
}

#[test]
fn wal_safe_backup_supports_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("t.db");
    let srv = Server::start(dir.path());
    let axe = |args: &[&str]| srv.ok(args);
    // A running server: an open WAL connection whose commits are not checkpointed.
    let writer = db::open(&live).unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer
        .execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE);
             INSERT INTO tasks (id, project, title, state, created_at, updated_at)
               VALUES (44, 'orchid', 'held', 'pending', 'then', 'then'),
                      (38, 'orchid', 'escalated', 'escalated', 'then', 'then');
             INSERT INTO events (task_id, at, actor, kind, from_value, to_value, body) VALUES
               (44, 'then', 'ops', 'created', NULL, 'held', 'state=pending'),
               (38, 'then', 'agent', 'state', 'in_progress', 'escalated', NULL),
               (38, 'then', 'agent', 'note', NULL, NULL, 'ESCALATE tier=high');",
        )
        .unwrap();
    let wal = dir.path().join("t.db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    // The hazard: a raw file copy misses the uncheckpointed commits.
    let raw = dir.path().join("raw-copy.db");
    std::fs::copy(&live, &raw).unwrap();
    let raw_tasks: i64 = rusqlite::Connection::open(&raw)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw_tasks, 0);

    // axe backup snapshots everything committed, WAL included, and never overwrites.
    let snap = dir.path().join("tasks.db.pre-change");
    axe(&["backup", snap.to_str().unwrap()]);
    let again = srv.run(&["backup", snap.to_str().unwrap()]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
    let summary = |path: &std::path::Path| {
        let c = db::open(path).unwrap();
        let tasks: Vec<(i64, String)> = db::list_tasks(
            &c,
            &db::Filter {
                include_done: true,
                ..Default::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|t| (t.id, t.state))
        .collect();
        let kinds: Vec<String> = db::events(&c, 38)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        (tasks, kinds)
    };
    let before = summary(&snap);
    assert_eq!(
        before.0,
        [(38, "escalated".to_string()), (44, "pending".to_string())]
    );
    assert_eq!(before.1, ["state", "note"]);

    // A change after the snapshot.
    axe(&["approve", "38", "--sha", "abc", "--scope", "prod"]);
    let changed = summary(&live);
    assert_eq!(changed.0[0], (38, "pending".to_string()));
    assert!(changed.1.contains(&"approval".to_string()));

    // Rollback with writers stopped: keep the post-snapshot files aside, restore the
    // snapshot. The restored queue has the pre-change state and every
    // uncheckpointed commit; the aside copy still holds post-snapshot history.
    drop(writer);
    drop(srv);
    let aside = dir.path().join("aside");
    std::fs::create_dir(&aside).unwrap();
    for name in ["t.db", "t.db-wal", "t.db-shm"] {
        let p = dir.path().join(name);
        if p.exists() {
            std::fs::rename(&p, aside.join(name)).unwrap();
        }
    }
    std::fs::copy(&snap, &live).unwrap();
    assert_eq!(summary(&live), before);
    let kept = summary(&aside.join("t.db"));
    assert_eq!(kept, changed);
}

#[test]
fn stats_history_matches_per_task_reads_in_one_request() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "a"]);
    srv.ok(&["add", "--project", "p", "--title", "b", "--state", "done"]);
    srv.ok(&["set", "1", "--state", "escalated"]);
    srv.ok(&["set", "1", "--state", "done"]);
    // A repeated done snapshot is not a transition, so stats never read it.
    srv.ok(&["set", "1", "--state", "done"]);
    srv.ok(&["note", "1", "a note body"]);
    {
        let c = rusqlite::Connection::open(&srv.db).unwrap();
        c.execute(
            "INSERT INTO events (task_id, at, actor, kind, to_value, body)
             VALUES (1, '2026-11-01T00:00:00Z', 'dispatcher', 'run_start', 'claim', '{}')",
            [],
        )
        .unwrap();
    }
    let file = dir.path().join("template.json");
    std::fs::write(
        &file,
        r###"{"name":"R","project":"p","cron":"* * * * *","timezone":"UTC","title":"r","detail":"## Outcome\nr\n## Verify\nr"}"###,
    )
    .unwrap();
    let r: Value =
        serde_json::from_str(&srv.ok(&["routine", "save", "--file", file.to_str().unwrap()]))
            .unwrap();
    let r: Value =
        serde_json::from_str(&srv.ok(&["routine", "trigger", &r["id"].to_string(), "--key", "k"]))
            .unwrap();
    let routine_task = r["task_id"].as_i64().unwrap();

    let h = srv.get("/api/stats/history");
    let keys = ["id", "task_id", "at", "kind", "from_value", "to_value"];
    let mut expected = Vec::new();
    for t in srv.get("/api/tasks?all=1").as_array().unwrap() {
        let id = t["id"].as_i64().unwrap();
        for e in srv
            .get(&format!("/api/tasks/{id}/events"))
            .as_array()
            .unwrap()
        {
            let state = e["kind"] == "state" && e["from_value"] != e["to_value"];
            if state || e["kind"] == "run_start" {
                let mut slim = serde_json::Map::new();
                for k in keys {
                    let v = if k == "task_id" {
                        id.into()
                    } else {
                        e[k].clone()
                    };
                    slim.insert(k.into(), v);
                }
                expected.push(Value::Object(slim));
            }
        }
    }
    expected.sort_by_key(|e| e["id"].as_i64());
    assert_eq!(h["events"], Value::Array(expected.clone()));
    let kinds: Vec<_> = expected
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"run_start") && kinds.contains(&"state"));
    let task1_done = expected
        .iter()
        .filter(|e| e["task_id"] == 1 && e["to_value"] == "done")
        .count();
    assert_eq!(task1_done, 1, "repeated done snapshot excluded");
    assert_eq!(h["routines"], serde_json::json!([routine_task]));
    for b in h["events"].as_array().unwrap() {
        assert!(b.get("body").is_none());
    }

    // Every run is returned, past the 1000-row cap of /api/runs.
    {
        let c = rusqlite::Connection::open(&srv.db).unwrap();
        for i in 0..1100 {
            let ended = (i != 0).then_some("2026-11-01T00:05:00Z");
            c.execute(
                "INSERT INTO runs (task_id, project, agent, mode, claim, started_at, ended_at, log_path)
                 VALUES (1, 'p', 'codex', 'fresh', 'c', '2026-11-01T00:00:00Z', ?1, 'l')",
                [ended],
            )
            .unwrap();
        }
    }
    let h = srv.get("/api/stats/history");
    let runs = h["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1100);
    assert_eq!(
        srv.get("/api/runs?limit=1000").as_array().unwrap().len(),
        1000
    );
    assert_eq!(
        runs[0],
        serde_json::json!({"id": 1, "task_id": 1, "agent": "codex",
            "started_at": "2026-11-01T00:00:00Z", "ended_at": null})
    );
    assert_eq!(runs[1]["ended_at"], "2026-11-01T00:05:00Z");
}
