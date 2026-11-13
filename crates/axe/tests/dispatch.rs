mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use axe::db::{self, Actor, NewTask};
use axe::dispatch::{
    fresh_prompt, otel_resource_attributes, parse_jsonl_session, render, resume_prompt,
};
use common::{Server, TOKEN};
use serde_json::Value;

fn write_config(dir: &Path) -> std::path::PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join("a")).unwrap();
    std::fs::create_dir_all(ws.join("b")).unwrap();
    std::fs::create_dir_all(ws.join("c")).unwrap();
    let cfg = format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

# One pass admits every project.
[pool]
total = 4

[agents.good]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done --note \"built in $PWD\"", "{{prompt}}"]

[agents.bad]
cmd = ["sh", "-c", "exit 3"]

[agents.codexish]
cmd = ["sh", "-c", "echo 'noise'; echo '{{\"type\":\"thread.started\",\"thread_id\":\"th-1\"}}'"]
session = "jsonl:thread_id"

[[rule]]
match = {{ project = "a" }}
agents = ["good"]

[[rule]]
match = {{ project = "b" }}
agents = ["bad"]

[[rule]]
match = {{ project = "c" }}
agents = ["codexish"]

[[rule]]
match = {{ project = "missing" }}
agents = ["good"]
"#,
        ws = ws.display(),
        logs = dir.join("logs").display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, cfg).unwrap();
    path
}

/// One dispatcher pass inside `axe server --once`; agents reach the server
/// through the inherited `AXE_SERVER` and token file.
fn run_dispatch_once(dir: &Path, db_path: &Path, cfg: &Path) {
    assert_eq!(db_path, dir.join("t.db"));
    let srv = Server::start_with(dir, cfg, Some(TOKEN), &[], &[]);
    let out = srv.dispatcher(&["--once"], &[]).wait();
    assert!(out.status.success(), "{out:?}");
}

fn add(conn: &mut rusqlite::Connection, project: &str) -> i64 {
    db::add_task(
        conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project,
            title: "do it",
            detail: "Outcome: x",
            state: "pending",
        },
    )
    .unwrap()
}

#[test]
fn dispatch_once_runs_agents_and_blocks_silent_exits() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let db_path = dir.path().join("t.db");
    let mut conn = db::open(&db_path).unwrap();
    let good = add(&mut conn, "a");
    let bad = add(&mut conn, "b");
    let codex = add(&mut conn, "c");
    let missing = add(&mut conn, "missing");
    drop(conn);

    run_dispatch_once(dir.path(), &db_path, &cfg);
    let conn = db::open(&db_path).unwrap();

    let t = db::get_task(&conn, good).unwrap();
    assert_eq!((t.state.as_str(), t.runs), ("done", 1));
    assert_eq!(t.claimed_by, None);
    let ev = db::events(&conn, good).unwrap();
    let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "created",
            "claimed",
            "run_start",
            "state",
            "note",
            "run_end"
        ]
    );
    let state_ev = &ev[3];
    assert_eq!(state_ev.actor, "agent");
    assert!(state_ev.session.is_some());
    let note = ev[4].body.as_deref().unwrap();
    assert!(note.ends_with("/ws/a"), "{note}");
    let start: serde_json::Value = serde_json::from_str(ev[2].body.as_deref().unwrap()).unwrap();
    assert_eq!(start["agent"], "good");
    assert_eq!(start["rule"], "project=a");
    let prompt_file = start["prompt_file"].as_str().unwrap();
    assert!(Path::new(prompt_file).ends_with(format!("logs/{good}-1.prompt.md")));
    let prompt = std::fs::read_to_string(prompt_file).unwrap();
    assert_eq!(prompt, fresh_prompt(good, "a"));
    let cmd: Vec<&str> = start["cmd"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(cmd.last().copied(), Some(prompt.as_str()));
    assert_eq!(&cmd[..2], ["sh", "-c"]);
    let end: serde_json::Value = serde_json::from_str(ev[5].body.as_deref().unwrap()).unwrap();
    assert_eq!(end["exit_code"], 0);
    assert!(Path::new(end["log"].as_str().unwrap()).ends_with(format!("{good}-1.log")));

    let t = db::get_task(&conn, bad).unwrap();
    assert_eq!(t.state, "escalated");
    let ev = db::events(&conn, bad).unwrap();
    let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "created",
            "claimed",
            "run_start",
            "run_end",
            "state",
            "escalation_class",
            "note"
        ]
    );
    assert_eq!(
        ev[6].body.as_deref(),
        Some(&*format!("blocked: session exited without updating task (exit 3)\nresolver: ops: inspect the run log and fix the cause\nresume: axe set {bad} --state pending"))
    );

    let ev = db::events(&conn, codex).unwrap();
    let end = ev.iter().find(|e| e.kind == "run_end").unwrap();
    let end: serde_json::Value = serde_json::from_str(end.body.as_deref().unwrap()).unwrap();
    assert_eq!(end["session_id"], "th-1");

    let t = db::get_task(&conn, missing).unwrap();
    assert_eq!(t.state, "escalated");
    let last = db::events(&conn, missing).unwrap().pop().unwrap();
    let body = last.body.unwrap();
    assert!(body.starts_with("blocked: project directory"), "{body}");
    assert!(body.contains("does not exist\nresolver: ops:"), "{body}");
}

#[test]
fn server_restart_preserves_unresolved_claims() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    db::claim(
        &mut conn,
        &Actor::new("dispatcher"),
        None,
        "dispatch:old",
        None,
    )
    .unwrap();
    let _srv = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    assert_eq!(db::get_task(&conn, id).unwrap().state, "in_progress");
}

#[test]
fn parses_codex_jsonl_session() {
    let fixture = concat!(
        "Reading prompt from stdin...\n",
        "{\"type\":\"thread.started\",\"thread_id\":\"0199a213-81c0-7800-8aa1-bbab2a035a53\"}\n",
        "{\"type\":\"turn.started\"}\n",
    );
    assert_eq!(
        parse_jsonl_session(fixture, "thread_id").as_deref(),
        Some("0199a213-81c0-7800-8aa1-bbab2a035a53")
    );
    let fallback = "{\"type\":\"x\"}\n{\"session_id\":\"s-9\"}\n";
    assert_eq!(
        parse_jsonl_session(fallback, "thread_id").as_deref(),
        Some("s-9")
    );
    assert_eq!(parse_jsonl_session("plain text\n", "thread_id"), None);
}

#[test]
fn render_substitutes_once() {
    let vars = BTreeMap::from([
        ("prompt", "use {cwd} literally".to_string()),
        ("cwd", "/w".to_string()),
    ]);
    assert_eq!(
        render("{cwd}: {prompt} {unknown} {", &vars),
        "/w: use {cwd} literally {unknown} {"
    );
}

fn axe_log(srv: &Server, id: i64, extra: &[&str]) -> String {
    let id = id.to_string();
    let mut args = vec!["log", id.as_str()];
    args.extend_from_slice(extra);
    srv.ok(&args)
}

#[test]
fn log_truncates_long_body_lines_unless_full() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let mut conn = db::open(&db_path).unwrap();
    let id = add(&mut conn, "a");
    let long = "é".repeat(350);
    let body = format!("short\n{long}");
    db::append(&mut conn, &Actor::new("human"), id, "note", &body).unwrap();
    drop(conn);

    let srv = Server::start(dir.path());
    assert_eq!(srv.db, db_path);
    let text = axe_log(&srv, id, &[]);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[1].ends_with(": short"), "{text}");
    assert_eq!(lines[2], format!("{}…", "é".repeat(300)));
    assert!(!text.contains(&long));

    let full = axe_log(&srv, id, &["--full"]);
    assert!(full.lines().any(|l| l == long), "{full}");

    let json: serde_json::Value = serde_json::from_str(&axe_log(&srv, id, &["--json"])).unwrap();
    assert_eq!(json[1]["body"], body);
}

#[test]
fn prompts_are_exact() {
    assert_eq!(
        fresh_prompt(7, "orchid"),
        "Work axe task 7 in project orchid: run axe show 7 and axe log 7, then carry it to done. Around a merge or deploy: axe protect 7, then --clear."
    );
    assert_eq!(
        resume_prompt(7),
        "Task 7 resumed; run axe log 7 for what changed."
    );
}

#[test]
fn removed_template_variables_are_rejected() {
    for var in ["persona_file", "prompt_with_persona", "persona"] {
        let err =
            axe::config::Config::parse(&format!("[agents.x]\ncmd = [\"sh\", \"{{{var}}}\"]\n"))
                .unwrap_err();
        assert!(format!("{err:#}").contains(var), "{var}: {err:#}");
    }
}

fn session_config(dir: &Path, extra: &str, fail: bool) -> std::path::PathBuf {
    let path = write_config(dir);
    let fresh = r#"["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state blocked --note paused", "{session_id}", "{prompt}"]"#;
    let resume = if fail {
        r#"["sh", "-c", "exit 9", "{session_id}", "{prompt}"]"#
    } else {
        fresh
    };
    std::fs::write(
        &path,
        format!(
            r#"
workspace_root = "{}/ws"
log_dir = "{}/logs"
[defaults]
agents = ["first", "second"]
strategy = "round_robin"
max_runs = 20
{extra}
[agents.first]
cmd = {fresh}
resume_cmd = {resume}
session = "preset"
[agents.second]
cmd = {fresh}
resume_cmd = {resume}
session = "preset"
"#,
            dir.display(),
            dir.display()
        ),
    )
    .unwrap();
    path
}

/// Sets a held task pending again, as a human resolving its hold would.
fn bounce(conn: &mut rusqlite::Connection, id: i64) {
    db::set_task(
        conn,
        &Actor::new("human"),
        id,
        &db::Changes {
            state: Some("pending".into()),
            note: Some("resolved: carry on".into()),
            ..Default::default()
        },
    )
    .unwrap();
}

fn starts(conn: &rusqlite::Connection, id: i64) -> Vec<serde_json::Value> {
    db::events(conn, id)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "run_start")
        .map(|e| serde_json::from_str(e.body.as_deref().unwrap()).unwrap())
        .collect()
}

fn prompt_of(run: &serde_json::Value) -> String {
    std::fs::read_to_string(run["prompt_file"].as_str().unwrap()).unwrap()
}

#[test]
fn task_resumes_its_one_session_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_config(dir.path(), "", false);
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    run_dispatch_once(dir.path(), &path, &cfg);
    let session = db::session(&conn, id).unwrap().unwrap();
    assert_eq!((session.agent.as_str(), session.resume_count), ("first", 0));

    // Escalated, then resolved back to pending.
    db::set_task(
        &mut conn,
        &Actor::new("agent"),
        id,
        &db::Changes {
            state: Some("escalated".into()),
            ..Default::default()
        },
    )
    .unwrap();
    bounce(&mut conn, id);
    run_dispatch_once(dir.path(), &path, &cfg);

    // A timed wait (`axe wait --for`) that has passed.
    db::set_task(
        &mut conn,
        &Actor::new("agent"),
        id,
        &db::Changes {
            state: Some("pending".into()),
            not_before: Some("+1s".into()),
            ..Default::default()
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(2100));
    run_dispatch_once(dir.path(), &path, &cfg);

    // A dependency wait (`axe wait --on`) whose prerequisite finished.
    let prereq = db::add_task(
        &mut conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project: "a",
            title: "prereq",
            detail: "",
            state: "blocked",
        },
    )
    .unwrap();
    db::edit_deps(&mut conn, &Actor::new("agent"), id, &[prereq], &[]).unwrap();
    bounce(&mut conn, id);
    run_dispatch_once(dir.path(), &path, &cfg);
    assert_eq!(starts(&conn, id).len(), 3, "waits on its prerequisite");
    db::set_task(
        &mut conn,
        &Actor::new("human"),
        prereq,
        &db::Changes {
            state: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    run_dispatch_once(dir.path(), &path, &cfg);

    let runs = starts(&conn, id);
    assert_eq!(runs.len(), 4);
    assert_eq!(runs[0]["mode"], "fresh");
    assert_eq!(prompt_of(&runs[0]), fresh_prompt(id, "a"));
    for run in &runs[1..] {
        assert_eq!(run["mode"], "resume");
        assert_eq!(run["agent"], "first");
        assert_eq!(run["session_id"], session.session_id);
        assert_eq!(prompt_of(run), resume_prompt(id));
    }
    let stored = db::session(&conn, id).unwrap().unwrap();
    assert_eq!(stored.session_id, session.session_id);
    assert_eq!(stored.resume_count, 3);
    // Resuming did not advance the cursor: the next fresh task picks second.
    let other = add(&mut conn, "a");
    run_dispatch_once(dir.path(), &path, &cfg);
    assert_eq!(starts(&conn, other)[0]["agent"], "second");
}

#[test]
fn builtin_claude_launches_without_a_system_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ws/a")).unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\n\"$AXE_BIN\" set \"$AXE_TASK\" --state blocked --note paused\n",
    )
    .unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = dir.path().join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            "workspace_root = \"{}/ws\"\nlog_dir = \"{}/logs\"\n",
            dir.path().display(),
            dir.path().display()
        ),
    )
    .unwrap();
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    let env_path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let once = || {
        let srv = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
        let out = srv
            .dispatcher(&["--once"], &[("PATH", env_path.as_str())])
            .wait();
        assert!(out.status.success(), "{out:?}");
    };
    once();
    bounce(&mut conn, id);
    once();
    let runs = starts(&conn, id);
    let cmd = |run: &serde_json::Value| -> Vec<String> {
        serde_json::from_value(run["cmd"].clone()).unwrap()
    };
    let sid = runs[0]["session_id"].as_str().unwrap();
    let argv = |session_flag: &str, prompt: String| -> Vec<String> {
        [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            session_flag,
            sid,
            "--permission-mode",
            "bypassPermissions",
        ]
        .map(String::from)
        .into_iter()
        .chain([prompt])
        .collect()
    };
    assert_eq!(cmd(&runs[0]), argv("--session-id", fresh_prompt(id, "a")));
    assert_eq!(cmd(&runs[1]), argv("--resume", resume_prompt(id)));
    assert!(runs
        .iter()
        .flat_map(cmd)
        .all(|a| !a.contains("system-prompt")));
}

#[test]
fn failed_resume_retries_fresh_in_same_pass() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_config(dir.path(), "", true);
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    run_dispatch_once(dir.path(), &path, &cfg);
    bounce(&mut conn, id);
    run_dispatch_once(dir.path(), &path, &cfg);
    let runs = starts(&conn, id);
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[1]["mode"], "resume");
    assert_eq!(runs[2]["mode"], "fresh");
    assert_ne!(runs[1]["session_id"], runs[2]["session_id"]);
    assert_eq!(db::get_task(&conn, id).unwrap().state, "blocked");
    assert!(db::events(&conn, id).unwrap().iter().any(|e| e
        .body
        .as_deref()
        .is_some_and(|b| b.contains("resume exited non-zero"))));
    assert_eq!(prompt_of(&runs[1]), resume_prompt(id));
    assert_eq!(prompt_of(&runs[2]), fresh_prompt(id, "a"));
}

#[test]
fn max_resumes_starts_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_config(dir.path(), "max_resumes = 1", false);
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    for _ in 0..3 {
        bounce(&mut conn, id);
        run_dispatch_once(dir.path(), &path, &cfg);
    }
    let runs = starts(&conn, id);
    assert_eq!(runs[1]["mode"], "resume");
    assert_eq!(runs[2]["mode"], "fresh");
    assert_ne!(runs[0]["session_id"], runs[2]["session_id"]);
    assert!(db::events(&conn, id).unwrap().iter().any(|e| e
        .body
        .as_deref()
        .is_some_and(|b| b.contains("max_resumes reached"))));
}

#[test]
fn removed_agent_and_missing_resume_template_fall_back() {
    for removed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_config(dir.path(), "", false);
        let path = dir.path().join("t.db");
        let mut conn = db::open(&path).unwrap();
        let id = add(&mut conn, "a");
        run_dispatch_once(dir.path(), &path, &cfg);
        bounce(&mut conn, id);
        let text = std::fs::read_to_string(&cfg).unwrap();
        let text = if removed {
            let start = text.find("[agents.first]").unwrap();
            let end = text.find("[agents.second]").unwrap();
            format!(
                "{}{}",
                text[..start].replace("[\"first\", \"second\"]", "[\"second\"]"),
                &text[end..]
            )
        } else {
            text.lines()
                .filter(|line| !line.starts_with("resume_cmd"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        std::fs::write(&cfg, text).unwrap();
        run_dispatch_once(dir.path(), &path, &cfg);
        let runs = starts(&conn, id);
        assert_eq!(runs[1]["mode"], "fresh");
        assert!(db::events(&conn, id)
            .unwrap()
            .iter()
            .any(
                |e| e.body.as_deref().is_some_and(|b| b.contains(if removed {
                    "stored agent no longer exists"
                } else {
                    "no resume_cmd"
                }))
            ));
    }
}

#[test]
fn jsonl_session_is_saved_and_resumed_even_without_new_id_output() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_config(dir.path(), "", false);
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    for name in ["first", "second"] {
        let agent = &mut config["agents"][name];
        agent["session"] = toml::Value::String("jsonl:thread_id".into());
        let script = agent["cmd"][2].as_str().unwrap();
        agent["cmd"][2] =
            toml::Value::String(format!(r#"echo '{{"thread_id":"thread-one"}}'; {script}"#));
    }
    // Only the fresh command emits a thread id; resume inherits the stored id.
    std::fs::write(&cfg, toml::to_string(&config).unwrap()).unwrap();
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    run_dispatch_once(dir.path(), &path, &cfg);
    assert_eq!(
        db::session(&conn, id).unwrap().unwrap().session_id,
        "thread-one"
    );
    bounce(&mut conn, id);
    run_dispatch_once(dir.path(), &path, &cfg);
    let runs = starts(&conn, id);
    assert_eq!(runs[1]["session_id"], "thread-one");
    assert_eq!(prompt_of(&runs[1]), resume_prompt(id));
    assert_eq!(
        db::session(&conn, id).unwrap().unwrap().session_id,
        "thread-one"
    );
}

#[test]
fn failed_resume_after_task_change_does_not_retry() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = session_config(dir.path(), "", false);
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    for name in ["first", "second"] {
        config["agents"][name]["resume_cmd"][2] =
            toml::Value::String(r#""$AXE_BIN" note "$AXE_TASK" "partial progress"; exit 9"#.into());
    }
    std::fs::write(&cfg, toml::to_string(&config).unwrap()).unwrap();
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    run_dispatch_once(dir.path(), &path, &cfg);
    bounce(&mut conn, id);
    run_dispatch_once(dir.path(), &path, &cfg);
    assert_eq!(starts(&conn, id).len(), 2);
    assert_eq!(db::get_task(&conn, id).unwrap().state, "escalated");
}

const CLAUDE_FIXTURE: &str = include_str!("fixtures/claude-stream.jsonl");
const CODEX_FIXTURE: &str = include_str!("fixtures/codex-exec.jsonl");

fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn otel_note(srv: &Server, id: i64, nth: usize) -> Option<String> {
    let events = srv.get(&format!("/api/tasks/{id}/events"));
    events
        .as_array()?
        .iter()
        .filter_map(|e| e["body"].as_str()?.strip_prefix("otel="))
        .nth(nth)
        .map(String::from)
}

#[test]
fn server_dispatcher_tracks_live_runs_with_activity_and_otel() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    // Each run prints a recorded agent stream, reports its OTel env, then waits
    // for the test to release it before pausing the task.
    let script = |fixture: &str| {
        format!(
            r#"["sh", "-c", "cat \"$FIXTURES/{fixture}\"; \"$AXE_BIN\" note \"$AXE_TASK\" \"otel=$OTEL_RESOURCE_ATTRIBUTES\"; while [ ! -f \"$RELEASE\" ]; do sleep 0.05; done; rm -f \"$RELEASE\"; \"$AXE_BIN\" set \"$AXE_TASK\" --state blocked", "{{session_id}}", "{{prompt}}"]"#
        )
    };
    std::fs::write(
        &cfg,
        format!(
            r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[defaults]
agents = ["rec"]
[agents.rec]
cmd = {fresh}
resume_cmd = {resume}
session = "preset"
[otel]
prefix = "orchid"
resource = {{ "service.namespace" = "orchid", "deployment.environment" = "prod-vm" }}
"#,
            ws = dir.path().join("ws").display(),
            logs = dir.path().join("logs").display(),
            fresh = script("claude-stream.jsonl"),
            resume = script("codex-exec.jsonl"),
        ),
    )
    .unwrap();
    let release = dir.path().join("release");
    let srv = Server::start_with(
        dir.path(),
        &cfg,
        Some(TOKEN),
        &[],
        &[
            ("FIXTURES", fixtures.to_str().unwrap()),
            ("RELEASE", release.to_str().unwrap()),
            ("OTEL_RESOURCE_ATTRIBUTES", "inherited=1"),
        ],
    );
    let _dispatcher = srv.dispatcher(
        &[],
        &[
            ("FIXTURES", fixtures.to_str().unwrap()),
            ("RELEASE", release.to_str().unwrap()),
            ("OTEL_RESOURCE_ATTRIBUTES", "inherited=1"),
        ],
    );
    let id: i64 = srv
        .ok(&["add", "--project", "a", "--title", "live"])
        .trim()
        .parse()
        .unwrap();

    let expected = [
        ("fresh", axe::activity::from_text(CLAUDE_FIXTURE).unwrap()),
        ("resume", axe::activity::from_text(CODEX_FIXTURE).unwrap()),
    ];
    let mut session_id = None;
    for (nth, (mode, activity)) in expected.iter().enumerate() {
        let run = poll("live run with activity", || {
            let runs = srv.get("/api/runs?live=1");
            let run = runs.as_array()?.first()?.clone();
            (run["current_activity"].as_str() == Some(activity.as_str())).then_some(run)
        });
        assert_eq!(run["task_id"], id);
        assert_eq!(run["mode"], *mode);
        assert_eq!(run["agent"], "rec");
        assert_eq!(run["project"], "a");
        assert_eq!(run["live"], true);
        assert!(run["last_activity_at"].is_string());
        let claim = run["claim"].as_str().unwrap().to_string();
        assert!(claim.starts_with("dispatch:"));
        let sid = run["session_id"].as_str().unwrap().to_string();
        if let Some(prev) = &session_id {
            assert_eq!(&sid, prev, "resume keeps the session");
        }
        session_id = Some(sid.clone());
        assert_eq!(srv.get("/api/overview")["running"], 1);
        let session = srv.get(&format!("/api/sessions/{sid}"));
        assert_eq!(
            (&session["task_id"], &session["agent"]),
            (&Value::from(id), &Value::from("rec"))
        );
        assert_eq!(session["run"]["id"], run["id"]);

        // The child env carries per-run OTel resource attributes.
        let otel = poll("otel note", || otel_note(&srv, id, nth));
        assert_eq!(
            otel,
            format!("inherited=1,deployment.environment=prod-vm,service.namespace=orchid,orchid.task_id={id},orchid.project=a,orchid.agent=rec,orchid.mode={mode},orchid.run={claim}")
        );

        std::fs::write(&release, "").unwrap();
        poll("run end", || {
            let runs = srv.get(&format!("/api/runs?task={id}"));
            let run = runs.as_array()?.first()?.clone();
            (run["live"] == false).then_some(())
        });
        let runs = srv.get(&format!("/api/runs?task={id}"));
        assert_eq!(runs[0]["exit_code"], 0);
        assert_eq!(runs[0]["current_activity"], activity.as_str());
        assert_eq!(srv.get("/api/runs?live=1"), serde_json::json!([]));
        assert_eq!(srv.get(&format!("/api/tasks/{id}"))["state"], "blocked");
        if nth == 0 {
            // Resolve the hold: the next run resumes the session.
            srv.ok(&[
                "set",
                &id.to_string(),
                "--state",
                "pending",
                "--note",
                "again",
            ]);
        }
    }
    let runs = srv.get(&format!("/api/runs?task={id}&limit=1"));
    assert_eq!(runs.as_array().unwrap().len(), 1);
    assert_eq!(runs[0]["mode"], "resume");
}

#[test]
fn otel_attributes_append_and_encode() {
    assert_eq!(
        otel_resource_attributes(None, &[("a", "x"), ("b", "y z,=%")]),
        "a=x,b=y%20z%2C%3D%25"
    );
    assert_eq!(
        otel_resource_attributes(Some(" k=v "), &[("a", "dispatch:1")]),
        "k=v,a=dispatch:1"
    );
    assert_eq!(otel_resource_attributes(Some(""), &[("a", "1")]), "a=1");
}

#[test]
fn resetting_run_budget_never_overwrites_recorded_logs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let db_path = dir.path().join("t.db");
    let mut conn = db::open(&db_path).unwrap();
    let id = add(&mut conn, "a");
    for attempt in 0..3 {
        if attempt > 0 {
            db::set_task(
                &mut conn,
                &Actor::new("human"),
                id,
                &db::Changes {
                    reset_runs: true,
                    state: Some("pending".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        run_dispatch_once(dir.path(), &db_path, &cfg);
        let runs = db::list_runs(
            &conn,
            &db::RunFilter {
                task: Some(id),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        std::fs::write(&runs[0].log_path, format!("attempt {attempt}\n")).unwrap();
        for (index, run) in runs.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(&run.log_path).unwrap(),
                format!("attempt {}\n", attempt - index)
            );
        }
    }
}

#[test]
fn interrupted_and_failed_runs_escalate() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join("a")).unwrap();
    std::fs::create_dir_all(ws.join("b")).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
workspace_root = "{ws}"
log_dir = "{logs}"
[pool]
total = 2
[agents.hang]
cmd = ["sh", "-c", "sleep 30"]
[agents.bad]
cmd = ["sh", "-c", "exit 4"]
[[rule]]
match = {{ project = "a" }}
agents = ["hang"]
[[rule]]
match = {{ project = "b" }}
agents = ["bad"]
"#,
            ws = ws.display(),
            logs = dir.path().join("logs").display(),
        ),
    )
    .unwrap();
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let hang = add(&mut conn, "a");
    let bad = add(&mut conn, "b");
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    let mut worker = srv.dispatcher(&[], &[]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let h = db::get_task(&conn, hang).unwrap();
        let b = db::get_task(&conn, bad).unwrap();
        if h.state == "in_progress" && b.state == "escalated" {
            break;
        }
        assert!(Instant::now() < deadline, "{h:?} {b:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::process::Command::new("kill")
        .args(["-TERM", &worker.0.id().to_string()])
        .status()
        .unwrap();
    assert!(worker.0.wait().unwrap().success());

    for (id, blocker) in [
        (hang, "blocked: dispatcher stopped\n"),
        (
            bad,
            "blocked: session exited without updating task (exit 4)\n",
        ),
    ] {
        let t = db::get_task(&conn, id).unwrap();
        assert_eq!((t.state.as_str(), t.claimed_by), ("escalated", None));
        let ev = db::events(&conn, id).unwrap();
        let note = ev.last().unwrap().body.as_deref().unwrap();
        assert!(note.starts_with(blocker), "{note}");
        assert!(
            note.ends_with(&format!("resume: axe set {id} --state pending")),
            "{note}"
        );
    }
    let ov = db::overview(&conn).unwrap();
    let q: Vec<i64> = ov.ops_queue.iter().map(|q| q.task.id).collect();
    assert_eq!(q, [hang, bad]);
    assert!(ov
        .ops_queue
        .iter()
        .all(|q| q.blocked_by.as_deref() == Some("dispatcher")));
}

#[test]
fn dispatcher_skips_waiting_tasks_without_using_slots_or_budget() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());
    let db_path = dir.path().join("t.db");
    let mut conn = db::open(&db_path).unwrap();
    // The prerequisite is on an external hold, so it is never claimed.
    let prereq = db::add_task(
        &mut conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project: "a",
            title: "prereq",
            detail: "",
            state: "blocked",
        },
    )
    .unwrap();
    let waiting = db::add_task_with_deps(
        &mut conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project: "a",
            title: "waits",
            detail: "Outcome: x",
            state: "pending",
        },
        &[prereq],
    )
    .unwrap();
    let ready = add(&mut conn, "a");
    drop(conn);

    run_dispatch_once(dir.path(), &db_path, &cfg);
    let mut conn = db::open(&db_path).unwrap();
    // The newer ready task ran; the older waiting one was never claimed.
    assert_eq!(db::get_task(&conn, ready).unwrap().runs, 1);
    let t = db::get_task(&conn, waiting).unwrap();
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0));
    assert!(db::list_runs(
        &conn,
        &db::RunFilter {
            task: Some(waiting),
            limit: 10,
            ..Default::default()
        }
    )
    .unwrap()
    .is_empty());

    db::set_task(
        &mut conn,
        &Actor::new("human"),
        prereq,
        &db::Changes {
            state: Some("done".into()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(conn);
    run_dispatch_once(dir.path(), &db_path, &cfg);
    let conn = db::open(&db_path).unwrap();
    let t = db::get_task(&conn, waiting).unwrap();
    // Became eligible on its own once the prerequisite was done.
    assert_eq!((t.state.as_str(), t.runs), ("done", 1));
    let ev = db::events(&conn, waiting).unwrap();
    let claimed = ev.iter().find(|e| e.kind == "claimed").unwrap();
    assert_eq!(claimed.actor, "dispatcher");
    let start = ev.iter().find(|e| e.kind == "run_start").unwrap();
    assert!(start.id > claimed.id);
}

#[test]
fn task_api_outage_keeps_session_and_budget_and_retries_after_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join("a")).unwrap();
    std::fs::create_dir_all(ws.join("b")).unwrap();
    let outage = r#"echo '{"type":"result","is_error":true,"terminal_reason":"api_error","api_error_status":403,"result":"API Error: 403 Attention Required"}'; exit 1"#;
    let bad_request = r#"echo '{"type":"result","is_error":true,"terminal_reason":"api_error","api_error_status":400,"result":"API Error: 400 prompt too long"}'; exit 1"#;
    let toml_str = |s: &str| serde_json::to_string(s).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"
workspace_root = "{ws}"
log_dir = "{logs}"
[pool]
total = 2
[agents.outage]
cmd = ["sh", "-c", {outage}, "{{session_id}}", "{{prompt}}"]
resume_cmd = ["sh", "-c", {outage}, "{{session_id}}", "{{prompt}}"]
session = "preset"
[agents.bad_request]
cmd = ["sh", "-c", {bad_request}, "{{session_id}}", "{{prompt}}"]
session = "preset"
[[rule]]
match = {{ project = "a" }}
agents = ["outage"]
[[rule]]
match = {{ project = "b" }}
agents = ["bad_request"]
"#,
            ws = ws.display(),
            logs = dir.path().join("logs").display(),
            outage = toml_str(outage),
            bad_request = toml_str(bad_request),
        ),
    )
    .unwrap();
    let path = dir.path().join("t.db");
    let mut conn = db::open(&path).unwrap();
    let id = add(&mut conn, "a");
    let real = add(&mut conn, "b");
    run_dispatch_once(dir.path(), &path, &config);

    let t = db::get_task(&conn, id).unwrap();
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0), "{t:?}");
    assert!(t.not_before.is_some(), "retries after a backoff");
    let session = db::session(&conn, id).unwrap().expect("session kept");
    let ev = db::events(&conn, id).unwrap();
    assert!(!ev
        .iter()
        .any(|e| e.kind == "state" && e.to_value.as_deref() == Some("escalated")));
    let end = ev.iter().rfind(|e| e.kind == "run_end").unwrap();
    assert!(end.body.as_deref().unwrap().contains("\"api_error\""));
    let note = ev.last().unwrap().body.as_deref().unwrap();
    assert!(
        note.contains("model API error (403") && note.contains("in 60s"),
        "{note}"
    );

    // The backoff passes: the same session resumes, and the next outage waits longer.
    conn.execute("UPDATE tasks SET not_before=NULL WHERE id=?1", [id])
        .unwrap();
    run_dispatch_once(dir.path(), &path, &config);
    let t = db::get_task(&conn, id).unwrap();
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0));
    let runs = starts(&conn, id);
    assert_eq!(runs.len(), 2, "no fresh-session fallback");
    assert_eq!(runs[1]["mode"], "resume");
    assert_eq!(
        runs[1]["session_id"].as_str(),
        Some(session.session_id.as_str())
    );
    let ev = db::events(&conn, id).unwrap();
    let note = ev.last().unwrap().body.as_deref().unwrap();
    assert!(
        note.contains("in 120s") && note.contains("run 2 in a row"),
        "{note}"
    );

    // A non-transient API error is a real failure: escalated, run counted.
    let t = db::get_task(&conn, real).unwrap();
    assert_eq!((t.state.as_str(), t.runs), ("escalated", 1));
}

#[test]
fn api_outage_detection_reads_only_the_last_structured_result() {
    use axe::agent_proc::{api_backoff_seconds, api_outage};
    let dir = tempfile::tempdir().unwrap();
    let check = |name: &str, text: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, text).unwrap();
        api_outage(&p)
    };
    let result = |status: &str, is_error: bool, reason: &str| {
        format!(
            r#"{{"type":"result","subtype":"success","is_error":{is_error},"terminal_reason":"{reason}"{status},"result":"Failed to authenticate. API Error: 403 Attention Required! | Cloudflare"}}"#
        )
    };
    let edge = result(
        r#","api_error_status":403,"api_error_origin":"first_party""#,
        true,
        "api_error",
    );
    let o = check("edge", &format!("{{\"type\":\"system\"}}\n{edge}\n")).expect("403 edge block");
    assert_eq!(o["status"], 403);
    assert!(o["message"].as_str().unwrap().contains("Cloudflare"));
    for status in [408, 429, 500, 503, 529] {
        let line = result(
            &format!(r#","api_error_status":{status}"#),
            true,
            "api_error",
        );
        assert!(check("t", &line).is_some(), "{status}");
    }
    for (name, text) in [
        (
            "bad request",
            result(r#","api_error_status":400"#, true, "api_error"),
        ),
        (
            "auth",
            result(r#","api_error_status":401"#, true, "api_error"),
        ),
        ("no status", result("", true, "api_error")),
        (
            "not an error",
            result(r#","api_error_status":529"#, false, "api_error"),
        ),
        (
            "other reason",
            result(r#","api_error_status":529"#, true, "max_turns"),
        ),
        (
            "recovered",
            format!("{edge}\n{}", result("", false, "completed")),
        ),
        (
            "no result",
            "{\"type\":\"thread.started\",\"thread_id\":\"t\"}\nplain text\n".into(),
        ),
    ] {
        assert_eq!(check("t", &text), None, "{name}");
    }
    assert_eq!(api_outage(&dir.path().join("missing")), None);
    let backoff: Vec<i64> = [0, 1, 2, 3, 5, 6, 40].map(api_backoff_seconds).into();
    assert_eq!(backoff, [60, 60, 120, 240, 960, 1800, 1800]);
}
