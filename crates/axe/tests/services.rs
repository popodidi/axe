//! Bot services: declarations through the CLI, and the node running
//! them (start on dispatcher start, restart with backoff, crash-loop wakes,
//! SIGTERM then SIGKILL on stop, the pre-stop wake on a drain).
mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axe::db;
use common::{Server, Worker, TOKEN};
use serde_json::{json, Value};

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Rig {
    dir: tempfile::TempDir,
    srv: Server,
}

impl Rig {
    /// A server with bot `b` (its runs exit at once) and `services` as the
    /// `[services]` section.
    fn new(services: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

[pool]
total = 1

[agents.quiet]
cmd = ["sh", "-c", "exit 0", "quiet", "{{prompt}}"]
bot_args = []

[services]
{services}
"#,
                ws = ws.display(),
                logs = dir.path().join("logs").display(),
            ),
        )
        .unwrap();
        let srv = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
        let f = dir.path().join("bot.json");
        std::fs::write(
            &f,
            json!({"name":"b","project":"a","agent":"quiet","identity":"I","conduct":"C"})
                .to_string(),
        )
        .unwrap();
        srv.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
        Rig { dir, srv }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn dispatcher(&self) -> Worker {
        let token = self.srv.token_file.to_str().unwrap().to_string();
        self.srv.dispatcher(
            &[],
            &[("AXE_TOKEN_FILE", &token), ("AXE_ACTOR", "dispatcher")],
        )
    }

    fn service(&self) -> Value {
        self.srv.get("/api/bots/b/services/s")
    }

    fn state(&self) -> String {
        self.service()["status"]["state"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn service_wakes(&self) -> Vec<Value> {
        let w: Vec<Value> = serde_json::from_value(self.srv.get("/api/bots/b/wakes")).unwrap();
        w.into_iter()
            .filter(|w| w["actor"] == "dispatcher")
            .collect()
    }

    fn add(&self, command: &str) {
        self.srv.ok(&[
            "service",
            "add",
            "s",
            "--bot",
            "b",
            "--command",
            command,
            "--restart",
            "always",
        ]);
    }
}

fn signal(d: &Worker, sig: i32) {
    unsafe {
        libc::kill(d.0.id() as i32, sig);
    }
}

fn lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn cli_declares_lists_starts_stops_and_removes_services() {
    let r = Rig::new("");
    let s = &r.srv;
    let out = s.ok(&[
        "service",
        "add",
        "s",
        "--bot",
        "b",
        "--command",
        "sleep 30",
        "--env",
        "A=1",
        "--env",
        "B=x=y",
        "--cwd",
        "sub",
    ]);
    assert!(out.starts_with("b/s\tpending"), "{out}");
    // $AXE_BOT is the default bot.
    let mut c = s.cli(&["service", "list", "--json"]);
    c.env("AXE_BOT", "b");
    let list: Vec<Value> = serde_json::from_slice(&c.output().unwrap().stdout).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["env"], json!({"A":"1","B":"x=y"}));
    assert_eq!(
        (&list[0]["cwd"], &list[0]["restart"], &list[0]["enabled"]),
        (&json!("sub"), &json!("on-failure"), &json!(true))
    );
    let out = s.ok(&["service", "stop", "s", "--bot", "b"]);
    assert!(out.contains("(disabled)"), "{out}");
    assert_eq!(r.service()["enabled"], false);
    s.ok(&["service", "start", "s", "--bot", "b"]);
    assert_eq!(r.service()["enabled"], true);
    // Replacing keeps the switch and records an update.
    s.ok(&["service", "stop", "s", "--bot", "b"]);
    s.ok(&["service", "add", "s", "--bot", "b", "--command", "sleep 60"]);
    assert_eq!(
        (&r.service()["command"], &r.service()["enabled"]),
        (&json!("sleep 60"), &json!(false))
    );
    assert!(s.ok(&["service", "ls", "--all"]).contains("b/s\t"));
    // Invalid declarations are refused.
    for args in [
        &["service", "add", "bad/name", "--bot", "b", "--command", "x"][..],
        &["service", "add", "t", "--bot", "b", "--command", " "],
        &[
            "service",
            "add",
            "t",
            "--bot",
            "b",
            "--command",
            "x",
            "--env",
            "AXE_BOT=x",
        ],
        &[
            "service",
            "add",
            "t",
            "--bot",
            "b",
            "--command",
            "x",
            "--env",
            "NOEQ",
        ],
        &[
            "service",
            "add",
            "t",
            "--bot",
            "b",
            "--command",
            "x",
            "--restart",
            "sometimes",
        ],
        &["service", "add", "t", "--bot", "nobody", "--command", "x"],
    ] {
        assert!(!s.run(args).status.success(), "{args:?}");
    }
    // The changes are bot events.
    let events: Vec<Value> = serde_json::from_value(s.get("/api/bots/b/events?limit=50")).unwrap();
    let actions: Vec<&str> = events
        .iter()
        .filter(|e| e["kind"] == "service")
        .filter_map(|e| {
            let body: Value = serde_json::from_str(e["body"].as_str()?).ok()?;
            Some(match body["action"].as_str()? {
                "add" => "add",
                "update" => "update",
                "start" => "start",
                "stop" => "stop",
                _ => "other",
            })
        })
        .collect();
    for a in ["add", "stop", "start", "update"] {
        assert!(actions.contains(&a), "{a}: {actions:?}");
    }
    assert!(s
        .ok(&["service", "remove", "s", "--bot", "b"])
        .contains("removed"));
    assert!(!s
        .run(&["service", "remove", "s", "--bot", "b"])
        .status
        .success());
    assert!(s
        .ok(&["service", "list", "--bot", "b"])
        .starts_with("no services"));
}

#[test]
fn cli_subscriptions_route_a_dispatcher_reported_failure_to_a_batch_wake() {
    let r = Rig::new("restart_delay_seconds = 30");
    let s = &r.srv;
    // Long enough for a sync (every 2 s) to see it running.
    r.add("sleep 3; exit 7");
    // The default: the owning bot, failures only.
    let out = s.ok(&["bot", "subscriptions", "b"]);
    assert!(
        out.contains("service b/s\twake=failed,crash_loop\tbatch=60s"),
        "{out}"
    );
    s.ok(&[
        "bot",
        "subscribe",
        "b",
        "--service",
        "s",
        "--wake",
        "failed,started",
        "--batch",
        "1m",
        "--why",
        "test",
    ]);
    assert!(!s
        .run(&[
            "bot",
            "subscribe",
            "b",
            "--service",
            "s",
            "--ride",
            "failed",
            "--why",
            "x"
        ])
        .status
        .success());
    let mut d = r.dispatcher();
    let batch = || -> Option<Value> {
        let w: Vec<Value> = serde_json::from_value(s.get("/api/bots/b/wakes")).unwrap();
        w.into_iter().find(|w| {
            w["actor"] == "axe"
                && w["body"]
                    .as_str()
                    .unwrap_or("")
                    .contains("service failed: b/s: exit 7")
        })
    };
    wait_for("failure batch", 30, || batch().is_some());
    let body = batch().unwrap()["body"].as_str().unwrap().to_string();
    assert!(body.contains("service started: b/s"), "{body}");
    signal(&d, libc::SIGTERM);
    assert!(d.0.wait().unwrap().success());
    s.ok(&[
        "bot",
        "unsubscribe",
        "b",
        "--service",
        "b/s",
        "--why",
        "quiet",
    ]);
    assert!(s.ok(&["bot", "subscriptions", "b"]).trim().is_empty());
}

#[test]
fn migrates_v21_to_the_services_table_and_keeps_it_after_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v21.db");
    let mut c = db::open(&path).unwrap();
    axe::bot::create(
        &mut c,
        &db::Actor::new("test"),
        &serde_json::from_value(json!({"name":"b","project":"a","agent":"claude"})).unwrap(),
        1_800_000_000,
    )
    .unwrap();
    c.execute_batch("DROP TABLE services; PRAGMA user_version=21;")
        .unwrap();
    drop(c);
    let mut c = db::open(&path).unwrap();
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 25);
    let new: axe::service::NewService =
        serde_json::from_value(json!({"command":"sleep 1"})).unwrap();
    axe::service::put(&mut c, &db::Actor::new("t"), "b", "s", &new).unwrap();
    c.pragma_update(None, "user_version", 21).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    assert_eq!(axe::service::list(&c, None).unwrap().len(), 1);
}

#[test]
fn migrates_v22_to_the_schedule_column_and_keeps_it_after_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v22.db");
    let mut c = db::open(&path).unwrap();
    axe::bot::create(
        &mut c,
        &db::Actor::new("test"),
        &serde_json::from_value(json!({"name":"b","project":"a","agent":"claude"})).unwrap(),
        1_800_000_000,
    )
    .unwrap();
    let long: axe::service::NewService =
        serde_json::from_value(json!({"command":"sleep 1"})).unwrap();
    axe::service::put(&mut c, &db::Actor::new("t"), "b", "long", &long).unwrap();
    c.execute_batch("ALTER TABLE services DROP COLUMN schedule; PRAGMA user_version=22;")
        .unwrap();
    drop(c);
    let mut c = db::open(&path).unwrap();
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 25);
    assert_eq!(
        axe::service::get(&c, "b", "long").unwrap().spec.schedule,
        None
    );
    // A scheduled service is stored `restart never`, whatever it was sent with.
    let new: axe::service::NewService = serde_json::from_value(
        json!({"command":"true","schedule":{"cron":"*/5 * * * *","timezone":"Asia/Taipei"}}),
    )
    .unwrap();
    let s = axe::service::put(&mut c, &db::Actor::new("t"), "b", "cron", &new).unwrap();
    assert_eq!((s.kind(), s.spec.restart.as_str()), ("scheduled", "never"));
    c.pragma_update(None, "user_version", 22).unwrap();
    drop(c);
    let c = db::open(&path).unwrap();
    let s = axe::service::get(&c, "b", "cron").unwrap();
    let sch = s.spec.schedule.unwrap();
    assert_eq!(
        (
            sch.cron.as_str(),
            sch.timezone.as_str(),
            sch.timeout_seconds,
            sch.catch_up
        ),
        ("*/5 * * * *", "Asia/Taipei", 3600, true)
    );
}

#[test]
fn a_scheduled_service_catches_up_once_reports_its_run_and_a_node_restart_replays_nothing() {
    let r = Rig::new("");
    let runs = r.path("runs");
    let cmd = format!(
        "echo run >> {}; echo ran-$AXE_SERVICE; exit 3",
        runs.display()
    );
    // --restart is for long-running services only.
    assert!(!r
        .srv
        .run(&[
            "service",
            "add",
            "s",
            "--bot",
            "b",
            "--command",
            &cmd,
            "--cron",
            "* * * * *",
            "--restart",
            "always",
        ])
        .status
        .success());
    // Yearly: no run time comes during the test, so a run is the catch-up.
    r.srv.ok(&[
        "service",
        "add",
        "s",
        "--bot",
        "b",
        "--command",
        &cmd,
        "--cron",
        "0 0 1 1 *",
        "--timeout",
        "30s",
    ]);
    let v = r.service();
    assert_eq!(v["restart"], "never");
    assert_eq!(
        v["schedule"],
        json!({"cron":"0 0 1 1 *","timezone":"UTC","timeout_seconds":30,"catch_up":true})
    );
    // Declared long ago, never run: the node missed a run time.
    let c = rusqlite::Connection::open(&r.srv.db).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute("UPDATE services SET updated_at='2024-06-01T00:00:00Z'", [])
        .unwrap();
    drop(c);
    let mut d = r.dispatcher();
    wait_for("catch-up run", 20, || lines(&runs).len() == 1);
    wait_for("reported run", 10, || {
        r.state() == "scheduled" && r.service()["status"]["finished_at"].is_string()
    });
    let st = r.service()["status"].clone();
    assert_eq!(st["last_exit"], "exit 3");
    assert!(
        st["next_run_at"]
            .as_str()
            .unwrap()
            .ends_with("-01-01T00:00:00Z"),
        "{st}"
    );
    let logs = r.srv.ok(&["service", "logs", "s", "--bot", "b", "-n", "5"]);
    assert!(
        logs.contains("ran-s") && logs.contains("exit 3 ---"),
        "{logs}"
    );
    let show = r.srv.ok(&["bot", "show", "b"]);
    assert!(
        show.contains("s (scheduled 0 0 1 1 * UTC): scheduled, last run ")
            && show.contains("ago: exit 3, next run in "),
        "{show}"
    );
    let list = r.srv.ok(&["service", "list", "--bot", "b"]);
    assert!(list.contains("[cron 0 0 1 1 * UTC, timeout 30s]"), "{list}");
    // The failed run reaches the owning bot (subscribed to failures).
    let failed = || {
        let w: Vec<Value> = serde_json::from_value(r.srv.get("/api/bots/b/wakes")).unwrap();
        w.iter().any(|w| {
            w["body"]
                .as_str()
                .unwrap_or("")
                .contains("service failed: b/s: exit 3")
        })
    };
    wait_for("failure event", 20, failed);
    // A node restart: the run is recorded, so nothing is replayed.
    signal(&d, libc::SIGTERM);
    assert!(d.0.wait().unwrap().success());
    let before = r.service()["status"]["reported_at"].clone();
    let mut d = r.dispatcher();
    wait_for("report after restart", 20, || {
        r.service()["status"]["reported_at"] != before && r.state() == "scheduled"
    });
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(lines(&runs).len(), 1, "no replay");
    signal(&d, libc::SIGTERM);
    assert!(d.0.wait().unwrap().success());
}

#[test]
fn services_start_with_the_dispatcher_stop_with_sigterm_and_start_again() {
    let r = Rig::new("stop_grace_seconds = 5");
    let starts = r.path("starts");
    let terms = r.path("terms");
    r.add(&format!(
        "echo started-$AXE_SERVICE; echo $$ >> {starts}; trap 'echo term >> {terms}; exit 0' TERM; while :; do sleep 0.1; done",
        starts = starts.display(),
        terms = terms.display()
    ));
    let mut d = r.dispatcher();
    wait_for("service start", 20, || lines(&starts).len() == 1);
    wait_for("running state", 10, || r.state() == "running");
    let st = r.service()["status"].clone();
    assert_eq!(st["pid"].as_u64().unwrap().to_string(), lines(&starts)[0]);
    assert!(st["dispatcher"].is_string() && st["started_at"].is_string());
    let logs = r.srv.ok(&["service", "logs", "s", "--bot", "b", "-n", "5"]);
    assert!(logs.contains("started-s"), "{logs}");
    // `axe bot show` lists the bot's services with kind, state and uptime.
    let show = r.srv.ok(&["bot", "show", "b"]);
    assert!(
        show.contains("# Services\n\ns (long-running): running, up ")
            && show.contains("logs: axe service logs <name> --bot b"),
        "{show}"
    );
    let v: Value = serde_json::from_str(&r.srv.ok(&["bot", "show", "b", "--json"])).unwrap();
    assert_eq!(v["services"][0]["status"]["state"], "running");
    // Dispatcher stop: the service gets SIGTERM and exits in its grace period.
    let t = Instant::now();
    signal(&d, libc::SIGTERM);
    assert!(d.0.wait().unwrap().success());
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(lines(&terms), ["term"]);
    assert_eq!(r.state(), "stopped");
    // The bot hears its services stopped.
    let w = r.service_wakes();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0]["body"]
        .as_str()
        .unwrap()
        .contains("Services stopped: s"));
    // Dispatcher start: it runs again.
    let mut d = r.dispatcher();
    wait_for("service restart", 20, || lines(&starts).len() == 2);
    wait_for("running state", 10, || r.state() == "running");
    // Stopped by its bot: it stays stopped.
    r.srv.ok(&["service", "stop", "s", "--bot", "b"]);
    wait_for("stopped", 10, || r.state() == "stopped");
    assert_eq!(lines(&terms).len(), 2);
    signal(&d, libc::SIGTERM);
    assert!(d.0.wait().unwrap().success());
}

#[test]
fn a_service_ignoring_sigterm_is_killed_after_the_grace_period() {
    let r = Rig::new("stop_grace_seconds = 2");
    let ready = r.path("ready");
    r.add(&format!(
        "trap 'echo ignored' TERM; touch {}; while :; do sleep 0.1; done",
        ready.display()
    ));
    let mut d = r.dispatcher();
    wait_for("service start", 20, || ready.exists());
    wait_for("running", 10, || r.state() == "running");
    let pid = r.service()["status"]["pid"].as_i64().unwrap() as i32;
    let t = Instant::now();
    signal(&d, libc::SIGTERM);
    // Within the grace period the service still lives.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "alive in its grace period"
    );
    assert!(d.0.wait().unwrap().success());
    let took = t.elapsed();
    assert!(took >= Duration::from_secs(2), "{took:?}");
    assert!(took < Duration::from_secs(9), "{took:?}");
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "killed");
    let st = r.service()["status"].clone();
    assert_eq!(
        (st["state"].as_str(), st["last_exit"].as_str()),
        (Some("stopped"), Some("signal 9"))
    );
}

#[test]
fn a_crash_loop_restarts_with_backoff_wakes_the_bot_then_alerts_the_owner() {
    let r = Rig::new(
        "restart_delay_seconds = 1\nrestart_max_seconds = 2\ncrash_loop_restarts = 3\nescalate_after_seconds = 3",
    );
    let starts = r.path("starts");
    r.add(&format!("date +%s.%N >> {}; exit 7", starts.display()));
    let mut d = r.dispatcher();
    wait_for("crash-loop wake", 30, || {
        r.service_wakes()
            .iter()
            .any(|w| w["body"].as_str().unwrap().contains("crash looping"))
    });
    wait_for("crash loop state", 10, || {
        r.service()["status"]["crash_loop_since"].is_string()
    });
    let st = r.service()["status"].clone();
    assert_eq!(st["last_exit"], "exit 7");
    // Restarted with growing delays: 1 s, then 2 s.
    let times: Vec<f64> = lines(&starts).iter().map(|l| l.parse().unwrap()).collect();
    assert!(times.len() >= 3, "{times:?}");
    let gaps: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps[0] >= 0.9, "{gaps:?}");
    assert!(gaps[1] >= 1.9, "{gaps:?}");
    // Still looping: the owner is alerted (no Telegram here) and the bot told.
    wait_for("escalation", 30, || {
        r.service_wakes().iter().any(|w| {
            w["body"]
                .as_str()
                .unwrap()
                .contains("owner has been alerted")
        })
    });
    let crash = r
        .service_wakes()
        .iter()
        .filter(|w| {
            w["body"]
                .as_str()
                .unwrap()
                .starts_with("Service s is crash looping")
        })
        .count();
    assert_eq!(crash, 1, "one wake per crash loop");
    // Fixed by the bot: a new definition starts fresh and the loop ends.
    let ok = r.path("ok");
    r.add(&format!("touch {}; sleep 30", ok.display()));
    wait_for("fixed service", 20, || ok.exists());
    wait_for("running", 10, || r.state() == "running");
    signal(&d, libc::SIGTERM);
    assert!(d.0.wait().unwrap().success());
}

#[test]
fn a_drain_wakes_the_owning_bot_and_waits_before_stopping_services() {
    let r = Rig::new("stop_warn_seconds = 3");
    let terms = r.path("terms");
    let ready = r.path("ready");
    r.add(&format!(
        "trap 'date +%s.%N >> {terms}; exit 0' TERM; touch {ready}; while :; do sleep 0.1; done",
        terms = terms.display(),
        ready = ready.display()
    ));
    let mut d = r.dispatcher();
    wait_for("service start", 20, || ready.exists());
    wait_for("running", 10, || r.state() == "running");
    let t = Instant::now();
    signal(&d, libc::SIGUSR1);
    wait_for("pre-stop wake", 10, || !r.service_wakes().is_empty());
    let w = r.service_wakes();
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(
        w[0]["body"]
            .as_str()
            .unwrap()
            .starts_with("Services stop in 3 s: s."),
        "{w:?}"
    );
    // The wake reaches the bot: a bot run takes it while the drain waits.
    wait_for("bot run takes the wake", 10, || {
        r.service_wakes()[0]["run_id"].is_i64()
    });
    assert!(lines(&terms).is_empty(), "still running in the window");
    assert!(d.0.wait().unwrap().success());
    assert!(t.elapsed() >= Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(lines(&terms).len(), 1);
    assert_eq!(r.state(), "stopped");
}

#[test]
fn a_notice_wakes_its_bot_once_per_key_and_escalation_has_owner_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = db::open(&dir.path().join("n.db")).unwrap();
    axe::bot::create(
        &mut c,
        &db::Actor::new("test"),
        &serde_json::from_value(json!({"name":"b","project":"a","agent":"claude"})).unwrap(),
        1_800_000_000,
    )
    .unwrap();
    let n = |kind: &str, key: &str| axe::service::Notice {
        bot: "b".into(),
        kind: kind.into(),
        services: vec!["s".into()],
        text: "t".into(),
        key: key.into(),
    };
    let a = axe::service::notify(&mut c, &n("escalate", "k1")).unwrap();
    assert!(a.owner_text.unwrap().contains("service s keeps crashing"));
    let again = axe::service::notify(&mut c, &n("escalate", "k1")).unwrap();
    assert_eq!((again.wake, again.owner_text), (a.wake, None));
    let b = axe::service::notify(&mut c, &n("crash_loop", "k2")).unwrap();
    assert!(b.owner_text.is_none() && b.wake != a.wake);
    assert!(axe::service::notify(&mut c, &n("other", "k3")).is_err());
    let mut n4 = n("stopping", "k4");
    n4.bot = "nobody".into();
    assert!(axe::service::notify(&mut c, &n4).is_err());
}
