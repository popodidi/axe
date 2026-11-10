//! Dispatcher integration for resource admission: an isolated queue, `sh`
//! agents as the process launcher, and synthetic resource samples.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axe::admission::{self, Gate};
use axe::config::Config;
use axe::db::{self, Actor, NewTask};
use axe::dispatch::Dispatcher;
use axe::remote::Store;
use common::gate::{defaults, Rig, GIB};
use common::{Server, TOKEN};

/// Runs until `ws/go` exists, so tests control when sessions end.
const WAIT_AGENT: &str = r#"[agents.run]
cmd = ["sh", "-c", "while [ ! -e ../go ]; do sleep 0.1; done"]"#;

struct Env {
    dir: tempfile::TempDir,
    db: std::path::PathBuf,
}

impl Env {
    fn conn(&self) -> rusqlite::Connection {
        db::open(&self.db).unwrap()
    }
    fn go(&self) {
        std::fs::write(self.dir.path().join("ws/go"), "").unwrap();
    }
}

fn env(total: usize, extra: &str) -> (Env, Config) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    for p in ["a", "b"] {
        std::fs::create_dir_all(ws.join(p)).unwrap();
    }
    let cfg = Config::parse(&format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[defaults]
agents = ["run"]
max_runs = 2
[pool]
total = {total}
{extra}
"#,
        ws = ws.display(),
        logs = dir.path().join("logs").display(),
    ))
    .unwrap();
    let db = dir.path().join("t.db");
    (Env { dir, db }, cfg)
}

fn add(conn: &mut rusqlite::Connection, state: &str, deps: &[i64]) -> i64 {
    db::add_task_with_deps(
        conn,
        &Actor::new("human"),
        &NewTask {
            agent: None,
            project: "a",
            title: "t",
            detail: "",
            state,
        },
        deps,
    )
    .unwrap()
}

/// The task of every run_start, in admission order.
fn admissions(conn: &rusqlite::Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT task_id FROM events WHERE kind = 'run_start' ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn run_rows(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap()
}

/// Counts runs in the database each time the fake probe reads disk.
fn observe_runs(rig: &Rig, env: &Env) {
    let path = env.db.clone();
    rig.r.lock().unwrap().on_sample = Some(Box::new(move || run_rows(&db::open(&path).unwrap())));
}

fn warm_gate(rig: &Rig) -> Gate {
    let mut g = rig.gate(defaults());
    g.check();
    rig.run(&mut g, 60, 10);
    assert!(g.admitting());
    g
}

/// An in-process dispatcher with a controlled gate, registered with an
/// isolated `axe server` on `env`'s queue.
struct Live {
    d: Dispatcher,
    stop: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
    srv: Server,
}

impl std::ops::Deref for Live {
    type Target = Dispatcher;
    fn deref(&self) -> &Dispatcher {
        &self.d
    }
}

impl std::ops::DerefMut for Live {
    fn deref_mut(&mut self) -> &mut Dispatcher {
        &mut self.d
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Starts a server for `env` and connects a dispatcher using `gate`; the stop
/// flag also ends the dispatcher's control connection.
fn connect(env: &Env, cfg: Config, gate: Gate) -> (Server, Arc<AtomicBool>, Dispatcher) {
    let (srv, stop, _, d) = connect_draining(env, cfg, gate);
    (srv, stop, d)
}

/// `connect`, also returning the dispatcher's drain flag (SIGUSR1).
fn connect_draining(
    env: &Env,
    cfg: Config,
    gate: Gate,
) -> (Server, Arc<AtomicBool>, Arc<AtomicBool>, Dispatcher) {
    let config = env.dir.path().join("config.toml");
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let srv = Server::start_with(env.dir.path(), &config, Some(TOKEN), &[], &[]);
    let stop = Arc::new(AtomicBool::new(false));
    let client = axe::client::Client::at(srv.url.clone(), "dispatcher".into()).with_token(TOKEN);
    let store = Store::open_with(&cfg, client, Arc::clone(&stop), None).unwrap();
    store.start_control(&cfg).unwrap();
    let draining = Arc::clone(&store.draining);
    let d = Dispatcher::new(cfg, store)
        .with_gate(gate)
        .with_hold_path(hold_file(env));
    (srv, stop, draining, d)
}

/// This queue's admission hold file.
fn hold_file(env: &Env) -> std::path::PathBuf {
    env.dir.path().join("admission-hold.json")
}

fn hold_until(env: &Env, until: &str, reason: Option<&str>) {
    admission::write_hold(
        &hold_file(env),
        &admission::Hold {
            until: db::parse_wake(until).unwrap(),
            reason: reason.map(Into::into),
            set_at: Some(db::now()),
            set_by: Some("human".into()),
        },
    )
    .unwrap();
}

fn live(env: &Env, cfg: Config, gate: Gate) -> Live {
    let (srv, stop, draining, d) = connect_draining(env, cfg, gate);
    Live {
        d,
        stop,
        draining,
        srv,
    }
}

fn task(env: &Env, id: i64) -> db::Task {
    db::get_task(&env.conn(), id).unwrap()
}

#[test]
fn gated_work_stays_pending_unclaimed_and_resumes_oldest_first() {
    let (env, cfg) = env(5, WAIT_AGENT);
    let mut conn = env.conn();
    let b0 = add(&mut conn, "pending", &[]);
    let b1 = add(&mut conn, "pending", &[]);
    let v0 = add(&mut conn, "pending", &[]);
    let held = add(&mut conn, "escalated", &[]);
    let waiting = add(&mut conn, "pending", &[b1]);
    let d0 = add(&mut conn, "pending", &[]);
    let d1 = add(&mut conn, "pending", &[]);
    let events_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();

    let rig = Rig::default();
    let gate = warm_gate(&rig);
    rig.set_disk(Ok(10 * GIB));
    observe_runs(&rig, &env);
    let mut d = live(&env, cfg, gate);
    let status = d.admission();
    // Ten minutes of polls under disk pressure.
    for _ in 0..60 {
        rig.advance(10);
        d.poll().unwrap();
    }
    assert_eq!(d.live(), 0, "no session process started");
    assert_eq!(run_rows(&conn), 0);
    assert!(admissions(&conn).is_empty());
    for id in [b0, b1, v0, waiting, d0, d1] {
        let t = task(&env, id);
        assert_eq!(
            (t.state.as_str(), t.runs, t.claimed_by.as_deref()),
            ("pending", 0, None),
            "#{id}: no claim and no run budget while gated"
        );
    }
    assert_eq!(task(&env, held).state, "escalated");
    // A global pause writes nothing to any task's log.
    let events_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events_after, events_before);
    let v = admission::view(&status);
    assert_eq!(v["admitting"], false);
    assert!(v["reason"]
        .as_str()
        .unwrap()
        .starts_with("disk /fake/workspace 10.0 GiB free"));
    // The server reports what the dispatcher decided, for `axe admission`/`show`.
    let served = d.srv.get("/api/admission");
    assert_eq!(
        (served["enabled"].clone(), served["admitting"].clone()),
        (true.into(), false.into())
    );
    assert_eq!(served["reason"], v["reason"]);
    assert!(
        served["report_age_seconds"].as_u64().unwrap() <= 1,
        "{served}"
    );

    // Pressure clears: oldest ready first; holds and prerequisites still hold.
    rig.set_disk(Ok(30 * GIB));
    rig.r.lock().unwrap().observed.clear();
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(admissions(&conn), [b0, b1, v0, d0, d1]);
    // One periodic sample, then a fresh sample before each of the five claims,
    // each taken after the previous session started.
    assert_eq!(rig.r.lock().unwrap().observed, [0, 0, 1, 2, 3, 4]);
    assert_eq!(d.live(), 5);
    for id in [b0, b1, v0, d0, d1] {
        assert_eq!(task(&env, id).runs, 1, "#{id}");
    }
    assert_eq!(task(&env, held).state, "escalated");
    assert_eq!(task(&env, waiting).runs, 0);
    assert_eq!(admission::view(&status)["admitting"], true);
    assert_eq!(d.srv.get("/api/admission")["admitting"], true);
    env.go();
    d.shutdown().unwrap();
}

#[test]
fn pressure_between_slots_stops_further_claims_and_running_sessions_continue() {
    let (env, cfg) = env(5, WAIT_AGENT);
    let mut conn = env.conn();
    let d0 = add(&mut conn, "pending", &[]);
    let v0 = add(&mut conn, "pending", &[]);
    let b0 = add(&mut conn, "pending", &[]);

    let rig = Rig::default();
    let gate = warm_gate(&rig);
    observe_runs(&rig, &env);
    // The first session's start pushes free disk below the pause threshold.
    rig.r.lock().unwrap().disk_by_observed =
        Some(Box::new(|runs| if runs == 0 { 30 * GIB } else { 10 * GIB }));
    let mut d = live(&env, cfg, gate);
    rig.advance(10);
    d.poll().unwrap();
    // Three slots were eligible; only the first passed its own fresh check.
    assert_eq!(admissions(&conn), [d0]);
    assert_eq!(d.live(), 1);
    let claim = task(&env, d0).claimed_by;
    for _ in 0..30 {
        rig.advance(10);
        d.poll().unwrap();
    }
    // The running session is untouched; queued work waits without budget use.
    assert_eq!(d.live(), 1);
    let t = task(&env, d0);
    assert_eq!((t.state.as_str(), t.claimed_by), ("in_progress", claim));
    for id in [v0, b0] {
        let t = task(&env, id);
        assert_eq!((t.state.as_str(), t.runs), ("pending", 0), "#{id}");
    }
    // The running session finishes normally while still gated.
    env.go();
    let deadline = Instant::now() + Duration::from_secs(10);
    while d.live() > 0 {
        assert!(Instant::now() < deadline, "session did not exit");
        std::thread::sleep(Duration::from_millis(50));
        rig.advance(1);
        d.poll().unwrap();
    }
    let ended: Option<String> = conn
        .query_row("SELECT ended_at FROM runs WHERE task_id = ?1", [d0], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(ended.is_some());
    assert_eq!(admissions(&conn).len(), 1);
    d.shutdown().unwrap();
}

#[test]
fn live_dispatcher_samples_on_interval_quietly_and_admits_promptly_on_recovery() {
    let (env, cfg) = env(3, "[agents.run]\ncmd = [\"sh\", \"-c\", \"sleep 30\"]");
    let cfg = Config {
        poll_seconds: 3600, // only a reopened gate can trigger the pass below
        ..cfg
    };
    let mut conn = env.conn();
    let b0 = add(&mut conn, "pending", &[]);

    let rig = Rig::default();
    rig.set_disk(Ok(GIB));
    let resources = Config::parse(
        "[resources]\nsample_seconds = 1\ncpu_window_seconds = 1\nstale_seconds = 2\n",
    )
    .unwrap()
    .resources
    .unwrap();
    let gate = rig.gate_real_clock(resources);
    let (_srv, stop, mut d) = connect(&env, cfg, gate);
    let handle = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || d.run(false, stop).unwrap())
    };
    std::thread::sleep(Duration::from_millis(4500));
    let calls = rig.disk_calls();
    // ~1 sample per second plus one pre-claim check per pass (a busy loop would
    // be thousands); bounds leave room for a loaded host.
    assert!((2..=12).contains(&calls), "{calls} disk samples in 4.5 s");
    assert_eq!(task(&env, b0).runs, 0);
    let lines = rig.lines();
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("paused new sessions"))
            .count(),
        1,
        "{lines:?}"
    );
    // Plus the one startup line for bot runs (disk pressure is above the floor).
    assert_eq!(
        lines.iter().filter(|l| l.contains("bot runs")).count(),
        1,
        "{lines:?}"
    );
    assert!(lines.len() <= 5, "{lines:?}");

    // Recovery reopens admission at the next sample, without waiting for the poll.
    rig.set_disk(Ok(100 * GIB));
    let deadline = Instant::now() + Duration::from_secs(20);
    while admissions(&conn).is_empty() {
        assert!(
            !handle.is_finished() && Instant::now() < deadline,
            "not admitted after recovery: {:?}",
            rig.lines()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(task(&env, b0).state, "in_progress");
    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
}

// ---- real binary: live readings, synthetic thresholds -----------------------

/// A server config watching `dir/ws` with a fast 1 s window and `thresholds`.
fn server_config(dir: &std::path::Path, thresholds: &str) -> std::path::PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join("a")).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[agents.done]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done --note admitted"]
[defaults]
agents = ["done"]
[resources]
sample_seconds = 1
cpu_window_seconds = 1
stale_seconds = 2
{thresholds}
"#,
            ws = ws.display(),
            logs = dir.join("logs").display(),
        ),
    )
    .unwrap();
    cfg
}

/// Disk pause/resume far above any real filesystem: always constrained, no I/O.
const UNREACHABLE_DISK: &str = "disk_pause_below_gib = 1048576\ndisk_resume_above_gib = 1048577";
/// Thresholds no healthy test host crosses.
const LENIENT: &str = "disk_pause_below_gib = 0\ndisk_resume_above_gib = 0.001\nmemory_pause_below_percent = 0\nmemory_resume_above_percent = 0.001\ncpu_pause_above_percent = 100\ncpu_resume_below_percent = 99.999";

fn wait_sampled(s: &Server) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let a = s.get("/api/admission");
        if a["checked_at"].is_string() {
            return a;
        }
        assert!(Instant::now() < deadline, "never sampled: {a}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn server_reports_live_readings_and_show_explains_resource_wait() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = server_config(dir.path(), UNREACHABLE_DISK);
    let s = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    // The dispatcher samples this guest and reports to the server.
    let _dispatcher = s.dispatcher(&[], &[]);
    let id = s
        .ok(&["add", "--project", "a", "--title", "waits"])
        .trim()
        .to_string();
    let a = wait_sampled(&s);
    assert_eq!(
        (a["enabled"].clone(), a["admitting"].clone()),
        (true.into(), false.into())
    );
    let disk = &a["signals"][0];
    assert_eq!(
        (disk["name"].as_str(), disk["level"].as_str()),
        (Some("disk"), Some("constrained"))
    );
    // The reported value is this guest's statvfs reading for the workspace.
    use axe::admission::Probe;
    let real = axe::admission::LinuxProbe
        .disk_available(&dir.path().join("ws"))
        .unwrap() as f64;
    assert!(
        (disk["value"].as_f64().unwrap() - real).abs() < GIB as f64,
        "{disk} vs {real}"
    );
    assert_eq!(disk["pause"], "< 1048576 GiB free");
    assert_eq!(a["thresholds"]["disk_resume_above_gib"], 1048577.0);
    assert_eq!(a["signals"][1]["name"], "memory");
    assert_eq!(s.get("/api/overview")["admission"]["enabled"], true);

    let show = s.ok(&["show", &id]);
    assert!(
        show.contains("\nwaiting: resource admission paused (disk "),
        "{show}"
    );
    let out = s.ok(&["admission"]);
    assert!(out.starts_with("admission: paused (disk "), "{out}");
    assert!(out.contains("\nmemory "), "{out}");
    let json: serde_json::Value = serde_json::from_str(&s.ok(&["admission", "--json"])).unwrap();
    assert_eq!(json["enabled"], true);
    // A dependency wait is still reported as such, not as resource pressure.
    let dep = s
        .ok(&[
            "add",
            "--project",
            "a",
            "--title",
            "dep",
            "--blocked-by",
            &id,
        ])
        .trim()
        .to_string();
    let show = s.ok(&["show", &dep]);
    assert!(
        show.contains("waiting: waiting on prerequisites #"),
        "{show}"
    );
    assert!(!show.contains("resource admission"), "{show}");
}

#[test]
fn admission_is_unknown_until_a_dispatcher_reports_and_ungated_without_resources() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = server_config(dir.path(), "");
    // Drop [resources] so this dispatcher is not gated.
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, &text[..text.find("[resources]").unwrap()]).unwrap();
    let s = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    let id = s
        .ok(&["add", "--project", "a", "--title", "t"])
        .trim()
        .to_string();
    // No dispatcher has reported: unknown, never claimed healthy.
    let a = s.get("/api/admission");
    assert_eq!(
        (a["enabled"].clone(), a["admitting"].clone()),
        (serde_json::Value::Null, serde_json::Value::Null)
    );
    assert_eq!(
        s.ok(&["admission"]),
        "admission: unknown: no dispatcher has reported since the server started\n"
    );
    assert!(!s.ok(&["show", &id]).contains("waiting:"));

    let _dispatcher = s.dispatcher(&[], &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let a = loop {
        let a = s.get("/api/admission");
        if !a["admitting"].is_null() {
            break a;
        }
        assert!(Instant::now() < deadline, "never reported: {a}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        (a["enabled"].clone(), a["admitting"].clone()),
        (false.into(), true.into())
    );
    assert_eq!(
        s.ok(&["admission"]),
        "admission: not gated (no [resources] in the dispatcher config)\n\
         tasks:  0/1 slots\n\
         bots:   0/0 slots (one per active bot)\n"
    );
    assert_eq!(a["bots_admitting"], true);
    assert_eq!(
        a["pools"],
        serde_json::json!({"tasks":{"used":0,"total":1},
            "bots":{"used":0,"total":0,"configured":null}})
    );
    assert!(!s.ok(&["show", &id]).contains("waiting:"));
}

#[test]
fn once_pass_waits_for_a_full_window_then_obeys_the_gate() {
    for (thresholds, admitted) in [(LENIENT, true), (UNREACHABLE_DISK, false)] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = server_config(dir.path(), thresholds);
        // Queue the task before the single pass starts so it cannot race setup.
        let db = dir.path().join("t.db");
        let id = add(&mut db::open(&db).unwrap(), "pending", &[]);
        let s = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
        let out = s.dispatcher(&["--once"], &[]).wait();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        assert!(
            stderr.contains("axe dispatcher: pool total=1 bots=one per active bot\n"),
            "{stderr}"
        );
        assert!(
            stderr.contains("axe dispatcher: resource admission on "),
            "{stderr}"
        );
        let t = db::get_task(&db::open(&db).unwrap(), id).unwrap();
        if admitted {
            assert_eq!((t.state.as_str(), t.runs), ("done", 1), "{stderr}");
        } else {
            assert_eq!(
                (t.state.as_str(), t.runs, t.claimed_by),
                ("pending", 0, None)
            );
            assert!(
                stderr.contains("axe admission: paused new sessions: disk "),
                "{stderr}"
            );
        }
    }
}

// ---- resume-to-fresh fallback ------------------------------------------------

/// A one-slot pool whose task resumes a stored session that waits for `ws/fail`
/// and exits 9 without task changes; fresh sessions wait for `ws/go`.
fn resume_env() -> (Env, Config, i64) {
    let (env, cfg) = env(
        1,
        r#"[agents.run]
cmd = ["sh", "-c", "while [ ! -e ../go ]; do sleep 0.1; done"]
resume_cmd = ["sh", "-c", "while [ ! -e ../fail ]; do sleep 0.1; done; exit 9"]
session = "preset"
"#,
    );
    let mut conn = env.conn();
    let id = add(&mut conn, "pending", &[]);
    db::save_session(
        &conn,
        id,
        &db::Session {
            agent: "run".into(),
            session_id: "saved-session".into(),
            resume_count: 0,
        },
    )
    .unwrap();
    (env, cfg, id)
}

fn notes(conn: &rusqlite::Connection, id: i64, prefix: &str) -> usize {
    db::events(conn, id)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "note" && e.body.as_deref().is_some_and(|b| b.starts_with(prefix)))
        .count()
}

/// Polls until the resumed session has exited and been reaped.
fn fail_resume(env: &Env, d: &mut Dispatcher, rig: &Rig) {
    std::fs::write(env.dir.path().join("ws/fail"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while d.live() > 0 {
        assert!(Instant::now() < deadline, "resume did not exit");
        std::thread::sleep(Duration::from_millis(50));
        rig.advance(1);
        d.poll().unwrap();
    }
}

#[test]
fn failed_resume_under_pressure_defers_its_fresh_fallback_until_recovery() {
    let (env, cfg, id) = resume_env();
    let conn = env.conn();
    let rig = Rig::default();
    let gate = warm_gate(&rig);
    observe_runs(&rig, &env);
    let mut d = live(&env, cfg, gate);
    d.poll().unwrap();
    assert_eq!(run_rows(&conn), 1);
    let claimed = task(&env, id);
    assert_eq!(claimed.runs, 1);

    rig.set_disk(Ok(10 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    fail_resume(&env, &mut d, &rig);
    // No new process; the claim, slot and run budget are kept.
    assert_eq!((run_rows(&conn), d.live(), d.deferred()), (1, 0, 1));
    let t = task(&env, id);
    assert_eq!(
        (t.state.as_str(), t.claimed_by.as_deref(), t.runs),
        ("in_progress", claimed.claimed_by.as_deref(), 1)
    );
    assert_eq!(notes(&conn, id, "fresh session fallback deferred: "), 1);
    // Ten minutes of pressure: still no process, no further notes or budget.
    for _ in 0..60 {
        rig.advance(10);
        d.poll().unwrap();
    }
    assert_eq!((run_rows(&conn), d.live(), d.deferred()), (1, 0, 1));
    assert_eq!(notes(&conn, id, "fresh session fallback"), 1);
    assert_eq!(task(&env, id).runs, 1);

    // Recovery: the fallback starts fresh on its original claim, after a fresh check.
    rig.set_disk(Ok(30 * GIB));
    rig.r.lock().unwrap().observed.clear();
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!((run_rows(&conn), d.live(), d.deferred()), (2, 1, 0));
    assert_eq!(rig.r.lock().unwrap().observed, [1, 1]);
    let starts: Vec<serde_json::Value> = db::events(&conn, id)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "run_start")
        .map(|e| serde_json::from_str(e.body.as_deref().unwrap()).unwrap())
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1]["mode"], "fresh");
    assert_eq!(starts[1]["rule"], "defaults");
    assert!(starts[1].get("slot").is_none());
    let t = task(&env, id);
    assert_eq!(
        (t.state.as_str(), t.claimed_by.as_deref(), t.runs),
        ("in_progress", claimed.claimed_by.as_deref(), 1)
    );
    env.go();
    d.shutdown().unwrap();
}

#[test]
fn deferred_fallback_slot_is_held_and_released_on_resolution_or_shutdown() {
    let (env, cfg, id) = resume_env();
    let mut conn = env.conn();
    let other = add(&mut conn, "pending", &[]);
    let rig = Rig::default();
    let gate = warm_gate(&rig);
    let mut d = live(&env, cfg, gate);
    d.poll().unwrap();
    rig.set_disk(Ok(10 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    fail_resume(&env, &mut d, &rig);
    assert_eq!(d.deferred(), 1);
    // Recovered: the deferred fallback keeps the pool's only slot, so the other
    // task still waits.
    rig.set_disk(Ok(30 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(d.live(), 1);
    assert_eq!(task(&env, other).runs, 0);
    env.go();
    d.shutdown().unwrap();

    // A deferred claim that ops resolves meanwhile is dropped, never spawned.
    let (env, cfg, id2) = resume_env();
    let rig = Rig::default();
    let gate = warm_gate(&rig);
    let mut d = live(&env, cfg.clone(), gate);
    d.poll().unwrap();
    rig.set_disk(Ok(10 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    fail_resume(&env, &mut d, &rig);
    db::set_task(
        &mut env.conn(),
        &Actor::new("human"),
        id2,
        &db::Changes {
            state: Some("escalated".into()),
            ..Default::default()
        },
    )
    .unwrap();
    rig.set_disk(Ok(30 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!((d.deferred(), d.live(), run_rows(&env.conn())), (0, 0, 1));
    assert_eq!(task(&env, id2).state, "escalated");
    d.shutdown().unwrap();

    // Shutdown while deferred escalates the claim like a stopped session.
    let (env, cfg, id3) = resume_env();
    let rig = Rig::default();
    let gate = warm_gate(&rig);
    let mut d = live(&env, cfg, gate);
    d.poll().unwrap();
    rig.set_disk(Ok(10 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    fail_resume(&env, &mut d, &rig);
    d.shutdown().unwrap();
    let t = task(&env, id3);
    assert_eq!((t.state.as_str(), t.claimed_by), ("escalated", None));
    assert!(db::events(&env.conn(), id3).unwrap().iter().any(|e| e
        .body
        .as_deref()
        .is_some_and(|b| b.contains("dispatcher stopped"))));

    // A drain releases a deferred claim before exiting, so restart needs no
    // reconciliation.
    let (env, cfg, id4) = resume_env();
    let rig = Rig::default();
    let gate = warm_gate(&rig);
    let mut d = live(&env, cfg, gate);
    d.poll().unwrap();
    rig.set_disk(Ok(10 * GIB));
    rig.advance(10);
    d.poll().unwrap();
    fail_resume(&env, &mut d, &rig);
    assert_eq!(d.deferred(), 1);
    d.draining.store(true, Ordering::SeqCst);
    let stop = Arc::clone(&d.stop);
    d.d.run(false, stop).unwrap();
    assert_eq!((d.deferred(), d.live(), run_rows(&env.conn())), (0, 0, 1));
    let t = task(&env, id4);
    assert_eq!((t.state.as_str(), t.claimed_by), ("escalated", None));
    let _ = id;
}

/// A task agent and a bot agent that both run until `ws/go` exists.
const BOT_AGENTS: &str = r#"[agents.run]
cmd = ["sh", "-c", "while [ ! -e ../go ]; do sleep 0.1; done"]
[agents.botrun]
cmd = ["sh", "-c", "while [ ! -e ../go ]; do sleep 0.1; done"]
bot_args = ["--bot"]"#;

fn add_bot(env: &Env, name: &str) {
    axe::bot::create(
        &mut env.conn(),
        &Actor::new("human"),
        &axe::bot::NewBot {
            name: name.into(),
            project: "a".into(),
            agent: Some("botrun".into()),
            identity: "I".into(),
            conduct: "C".into(),
            notes: String::new(),
            heartbeat: None,
            nudge_after: None,
        },
        axe::remote::epoch(),
    )
    .unwrap();
}

/// Bot runs that have not ended, as the server records them.
fn live_bot_rows(env: &Env) -> i64 {
    env.conn()
        .query_row(
            "SELECT COUNT(*) FROM bot_runs WHERE ended_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// `[pool] bots` is separate from `total`: full task slots never delay a bot,
/// bots never exceed their cap, and tasks still fill `total` while bots run.
#[test]
fn bot_pool_is_separate_from_task_slots() {
    let (env, cfg) = env(2, &format!("bots = 2\n{BOT_AGENTS}"));
    let mut conn = env.conn();
    for _ in 0..3 {
        add(&mut conn, "pending", &[]);
    }
    let rig = Rig::default();
    let mut d = live(&env, cfg, warm_gate(&rig));
    rig.advance(10);
    d.poll().unwrap();
    // (c) Tasks fill `total` (2 of 3 ready) with no bot around.
    assert_eq!((d.live_tasks(), d.live_bots()), (2, 0));

    // (a) Task slots are full (live tasks == total): a ready bot still spawns.
    add_bot(&env, "b1");
    rig.advance(1);
    d.poll().unwrap();
    assert_eq!((d.live_tasks(), d.live_bots()), (2, 1));

    // (b) Three more ready bots: only one more slot; bots never exceed N.
    for name in ["b2", "b3", "b4"] {
        add_bot(&env, name);
    }
    for _ in 0..5 {
        rig.advance(10);
        d.poll().unwrap();
        assert_eq!((d.live_tasks(), d.live_bots()), (2, 2));
        assert_eq!(live_bot_rows(&env), 2);
    }
    let served = d.srv.get("/api/admission");
    assert_eq!(
        served["pools"],
        serde_json::json!({"tasks":{"used":2,"total":2},
            "bots":{"used":2,"total":2,"configured":2}}),
        "{served}"
    );
    assert_eq!(served["bots_admitting"], true);
    let out = d.srv.ok(&["admission"]);
    assert!(
        out.contains("tasks:  2/2 slots\nbots:   2/2 slots\n"),
        "{out}"
    );
    env.go();
    d.shutdown().unwrap();
}

/// (c) With bots running, tasks still get all of `total`.
#[test]
fn tasks_fill_total_while_bots_run() {
    let (env, cfg) = env(2, &format!("bots = 1\n{BOT_AGENTS}"));
    add_bot(&env, "b1");
    let rig = Rig::default();
    let mut d = live(&env, cfg, warm_gate(&rig));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!((d.live_tasks(), d.live_bots()), (0, 1));
    let mut conn = env.conn();
    for _ in 0..3 {
        add(&mut conn, "pending", &[]);
    }
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!((d.live_tasks(), d.live_bots()), (2, 1));
    env.go();
    d.shutdown().unwrap();
}

/// (d) The gate's normal pause holds tasks but not bots; the hard floor (half
/// the disk or memory pause threshold) holds both.
#[test]
fn normal_pause_holds_tasks_not_bots_and_the_floor_holds_both() {
    let (env, cfg) = env(2, &format!("bots = 3\n{BOT_AGENTS}"));
    let mut conn = env.conn();
    let t = add(&mut conn, "pending", &[]);
    add_bot(&env, "b1");
    let rig = Rig::default();
    let gate = warm_gate(&rig);
    // Below the 15 GiB pause, above the 7.5 GiB floor.
    rig.set_disk(Ok(10 * GIB));
    let mut d = live(&env, cfg, gate);
    let status = d.admission();
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!((d.live_tasks(), d.live_bots()), (0, 1));
    assert_eq!(task(&env, t).runs, 0);
    let v = admission::view(&status);
    assert_eq!(
        (v["admitting"].clone(), v["bots_admitting"].clone()),
        (false.into(), true.into())
    );

    // Disk below the floor: neither a new bot nor the task starts; the live
    // bot run is untouched.
    rig.set_disk(Ok(5 * GIB));
    add_bot(&env, "b2");
    for _ in 0..5 {
        rig.advance(10);
        d.poll().unwrap();
    }
    assert_eq!((d.live_tasks(), d.live_bots()), (0, 1));
    let v = admission::view(&status);
    assert_eq!(v["bots_admitting"], false);
    assert!(
        v["bots_reason"]
            .as_str()
            .unwrap()
            .starts_with("disk 5.0 GiB free (floor < 7.5 GiB free)"),
        "{v}"
    );
    let out = d.srv.ok(&["admission"]);
    assert!(
        out.contains("bots: held at the hard floor (disk 5.0 GiB free"),
        "{out}"
    );

    // Memory below its floor (7.5%) holds bots too, even with disk healthy;
    // CPU pressure alone never does.
    rig.set_disk(Ok(100 * GIB));
    rig.set_memory(Ok(5.0));
    for _ in 0..3 {
        rig.advance(10);
        d.poll().unwrap();
    }
    assert_eq!((d.live_tasks(), d.live_bots()), (0, 1));
    // CPU pressure builds while memory is still low, then memory recovers.
    rig.set_busy(99.0);
    for _ in 0..8 {
        rig.advance(10);
        d.poll().unwrap();
    }
    assert_eq!(d.live_bots(), 1);
    rig.set_memory(Ok(50.0));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(admission::view(&status)["admitting"], false);
    assert_eq!((d.live_tasks(), d.live_bots()), (0, 2));
    assert_eq!(task(&env, t).runs, 0);
    env.go();
    d.shutdown().unwrap();
}

/// (e) Without `bots`, every active bot gets a slot, and task slots are untouched.
#[test]
fn omitted_bots_gives_one_slot_per_active_bot() {
    let (env, cfg) = env(1, BOT_AGENTS);
    assert_eq!(cfg.pool_bots(), None);
    let mut conn = env.conn();
    add(&mut conn, "pending", &[]);
    add(&mut conn, "pending", &[]);
    for name in ["b1", "b2", "b3"] {
        add_bot(&env, name);
    }
    let rig = Rig::default();
    let mut d = live(&env, cfg, warm_gate(&rig));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!((d.live_tasks(), d.live_bots()), (1, 3));
    let served = d.srv.get("/api/admission");
    assert_eq!(
        served["pools"]["bots"],
        serde_json::json!({"used":3,"total":3,"configured":null}),
        "{served}"
    );
    env.go();
    d.shutdown().unwrap();
}

/// A manual hold claims no ready task while running task sessions continue and
/// bots are still admitted; it expires without a restart, and a release
/// re-admits at once. The server reports it in `/api/admission`.
#[test]
fn admission_hold_blocks_new_task_sessions_only_and_expires_or_releases() {
    let (env, cfg) = env(4, &format!("bots = 1\n{BOT_AGENTS}"));
    let mut conn = env.conn();
    let running = add(&mut conn, "pending", &[]);
    let rig = Rig::default();
    let mut d = live(&env, cfg, warm_gate(&rig));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(admissions(&conn), [running]);

    hold_until(&env, "+3s", Some("cutover"));
    let held = add(&mut conn, "pending", &[]);
    add_bot(&env, "b1");
    for _ in 0..5 {
        rig.advance(10);
        d.poll().unwrap();
    }
    // The running session is untouched, the bot starts, the new task waits
    // unclaimed with no run budget spent.
    assert_eq!((d.live_tasks(), d.live_bots()), (1, 1));
    assert_eq!(task(&env, running).state, "in_progress");
    let t = task(&env, held);
    assert_eq!(
        (t.state.as_str(), t.runs, t.claimed_by),
        ("pending", 0, None)
    );
    let a = d.srv.get("/api/admission");
    assert_eq!(a["admitting"], false, "{a}");
    assert_eq!(a["bots_admitting"], true, "{a}");
    assert_eq!(a["hold"]["reason"], "cutover", "{a}");
    assert!(
        a["reason"]
            .as_str()
            .unwrap()
            .starts_with("admission hold until "),
        "{a}"
    );
    let out = d.srv.ok(&["admission"]);
    assert!(
        out.starts_with("admission: paused (admission hold until "),
        "{out}"
    );
    assert!(out.contains("\nhold: no new task sessions until "), "{out}");
    assert!(out.contains(" (cutover), set by human at "), "{out}");
    let show = d.srv.ok(&["show", &held.to_string()]);
    assert!(show.contains("\nwaiting: admission hold until "), "{show}");

    // Expiry re-admits on the next pass, without a restart.
    std::thread::sleep(Duration::from_millis(3200));
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(admissions(&conn), [running, held]);
    let a = d.srv.get("/api/admission");
    assert_eq!(
        (a["admitting"].clone(), a["hold"].clone()),
        (true.into(), serde_json::Value::Null)
    );

    // Release ends a long hold at once.
    hold_until(&env, "+1h", None);
    let released = add(&mut conn, "pending", &[]);
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(task(&env, released).runs, 0);
    assert!(admission::remove_hold(&hold_file(&env)).unwrap());
    rig.advance(10);
    d.poll().unwrap();
    assert_eq!(admissions(&conn), [running, held, released]);
    env.go();
    d.shutdown().unwrap();
}

/// `axe admission hold|release` against a real dispatcher process: the
/// command returns once the server reports the change.
#[test]
fn admission_hold_and_release_cli_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = server_config(dir.path(), LENIENT);
    let s = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
    let _dispatcher = s.dispatcher(&[], &[]);
    wait_sampled(&s);
    let past = s.run(&["admission", "hold", "--until", "2000-01-01T00:00Z"]);
    assert!(!past.status.success());
    assert!(String::from_utf8_lossy(&past.stderr).contains("not in the future"));

    let out = s.ok(&[
        "admission",
        "hold",
        "--until",
        "+1h",
        "--reason",
        "quiet window",
    ]);
    assert!(out.starts_with("admission hold set until "), "{out}");
    assert!(out.contains("\nhold: no new task sessions until "), "{out}");
    assert!(dir.path().join("admission-hold.json").exists());
    let a = s.get("/api/admission");
    assert_eq!(a["hold"]["reason"], "quiet window", "{a}");
    let json: serde_json::Value = serde_json::from_str(&s.ok(&["admission", "--json"])).unwrap();
    assert_eq!(json["hold"]["reason"], "quiet window");
    let id = s
        .ok(&["add", "--project", "a", "--title", "held"])
        .trim()
        .to_string();
    std::thread::sleep(Duration::from_millis(2500));
    let t: serde_json::Value = s.get(&format!("/api/tasks/{id}"));
    assert_eq!(
        (t["state"].as_str(), t["runs"].as_i64()),
        (Some("pending"), Some(0)),
        "{t}"
    );

    let out = s.ok(&["admission", "release"]);
    assert!(out.starts_with("admission hold released"), "{out}");
    assert!(!out.contains("hold:"), "{out}");
    assert!(s.get("/api/admission")["hold"].is_null());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let t: serde_json::Value = s.get(&format!("/api/tasks/{id}"));
        if t["state"] == "done" {
            break;
        }
        assert!(Instant::now() < deadline, "not admitted after release: {t}");
        std::thread::sleep(Duration::from_millis(200));
    }
    let out = s.ok(&["admission", "release"]);
    assert!(out.starts_with("no admission hold was set"), "{out}");
}
