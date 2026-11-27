//! Runtimes (`runtime.rs`): a registry of harness + login on the node.
//! The least-used healthy runtime runs a task, a stored session resumes on its
//! own runtime (sessions from before runtimes resolve to the runtime adopting
//! the default login), an unhealthy runtime is skipped for the next `use`
//! entry or holds the task, a failure class marks the runtime, and the
//! heartbeat carries no login directory or secret.
mod common;

use std::path::{Path, PathBuf};

use axe::db;
use common::{Server, TOKEN};
use serde_json::{json, Value};

/// Fake CLI: logs `<task or bot> <mode> <runtime env>` to `$RT_OUT/calls`,
/// then, per files in `$RT_OUT`: `fail` names a log to print before exit 1,
/// `pending` leaves the task waiting an hour (its session stays), `sleep` waits a
/// second; otherwise the task is done.
const AGENT: &str = r#"
out="$RT_OUT"
printf '%s %s %s %s\n' "${AXE_TASK:-bot:$AXE_BOT}" "$1" "${CLAUDE_CONFIG_DIR:-default}" "${CODEX_HOME:-default}" >> "$out/calls"
[ -e "$out/sleep" ] && sleep 1
if [ -e "$out/fail" ]; then cat "$(cat "$out/fail")"; exit 1; fi
[ -n "$AXE_BOT" ] && exit 0
if [ -e "$out/pending" ]; then "$AXE_BIN" set "$AXE_TASK" --state pending --not-before +1h --note again; else "$AXE_BIN" set "$AXE_TASK" --state done; fi
"#;

struct Rig {
    dir: tempfile::TempDir,
    srv: Server,
}

impl Rig {
    /// `runtimes`: (name, runtime.toml body, state.json body). "" is a
    /// runtime checked before (`{"state":"ok"}`), "-" one never checked
    /// (no state.json).
    fn new(runtimes: &[(&str, &str, &str)]) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let ws = d.join("ws");
        std::fs::create_dir_all(ws.join("p")).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        let reg = d.join("runtimes");
        for (name, toml, state) in runtimes {
            let rd = reg.join(name);
            std::fs::create_dir_all(&rd).unwrap();
            // Never the real CLI's login check: it may pass on a host that has one.
            let probe = if toml.contains("probe") {
                ""
            } else {
                "probe = [\"false\"]\n"
            };
            std::fs::write(rd.join("runtime.toml"), format!("{toml}{probe}")).unwrap();
            match *state {
                "-" => {}
                "" => std::fs::write(rd.join("state.json"), r#"{"state":"ok"}"#).unwrap(),
                s => std::fs::write(rd.join("state.json"), s).unwrap(),
            }
        }
        let q = |s: &str| serde_json::to_string(s).unwrap();
        let cfg = format!(
            r#"workspace_root = {ws}
log_dir = {logs}
poll_seconds = 1
[pool]
total = 8

[runtimes]
dir = {reg}

[agents.claude]
cmd = ["sh", "-c", {agent}, "sh", "fresh", "{{session_id}}", "{{prompt}}"]
resume_cmd = ["sh", "-c", {agent}, "sh", "resume", "{{session_id}}", "{{prompt}}"]
session = "preset"
harness = "claude-code"
bot_args = []

[agents.codex]
cmd = ["sh", "-c", {agent}, "sh", "fresh", "{{prompt}}"]
session = "none"
harness = "codex"
"#,
            ws = q(&ws.display().to_string()),
            logs = q(&d.join("logs").display().to_string()),
            reg = q(&reg.display().to_string()),
            agent = q(AGENT),
        );
        let path = d.join("config.toml");
        std::fs::write(&path, cfg).unwrap();
        let srv = Server::start_with(d, &path, Some(TOKEN), &[], &[]);
        Rig { dir, srv }
    }

    fn out(&self) -> PathBuf {
        self.dir.path().join("out")
    }

    fn add(&self) -> i64 {
        self.srv
            .ok(&["add", "--project", "p", "--title", "t"])
            .trim()
            .parse()
            .unwrap()
    }

    fn dispatch(&self) -> String {
        let out = self.out().display().to_string();
        let w = self.srv.dispatcher(&["--once"], &[("RT_OUT", &out)]).wait();
        assert!(w.status.success(), "{w:?}");
        String::from_utf8_lossy(&w.stderr).into_owned()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.out().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn task(&self, id: i64) -> db::Task {
        db::get_task(&db::open(&self.srv.db).unwrap(), id).unwrap()
    }

    fn starts(&self, id: i64) -> Vec<Value> {
        db::events(&db::open(&self.srv.db).unwrap(), id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "run_start")
            .map(|e| serde_json::from_str(e.body.as_deref().unwrap()).unwrap())
            .collect()
    }

    fn state(&self, name: &str) -> Value {
        let p = self
            .dir
            .path()
            .join("runtimes")
            .join(name)
            .join("state.json");
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }

    fn set_state(&self, name: &str, state: &str) {
        let p = self
            .dir
            .path()
            .join("runtimes")
            .join(name)
            .join("state.json");
        std::fs::write(p, state).unwrap();
    }

    /// Makes a waiting task ready now.
    fn wake(&self, id: i64) {
        db::open(&self.srv.db)
            .unwrap()
            .execute("UPDATE tasks SET not_before=NULL WHERE id=?1", [id])
            .unwrap();
    }

    fn set_rule(&self, uses: &[&str]) {
        let mut a = vec!["rule", "add", "--default"];
        for u in uses {
            a.extend(["--use", u]);
        }
        self.srv.ok(&a);
    }
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/failures")
        .join(name)
        .display()
        .to_string()
}

const CLAUDE: &str = "harness = \"claude-code\"\n";
const CODEX: &str = "harness = \"codex\"\n";
const NEEDS_LOGIN: &str = r#"{"state":"needs_login"}"#;
const LIMITED: &str = r#"{"state":"limited","until":4000000000}"#;

fn claude_on(dir: &str) -> String {
    format!(
        "{CLAUDE}login_dir = {}\n",
        serde_json::to_string(dir).unwrap()
    )
}

#[test]
fn least_used_healthy_runtime_runs_each_task_on_its_own_login() {
    let d = tempfile::tempdir().unwrap();
    let two = d.path().join("claude-2-login").display().to_string();
    // claude-1 adopts the CLI's default login in place: nothing is set.
    let rig = Rig::new(&[
        (
            "claude-1",
            &format!("{CLAUDE}login_dir = \"~/.claude\"\n"),
            "",
        ),
        ("claude-2", &claude_on(&two), ""),
        ("codex-1", CODEX, ""),
    ]);
    std::fs::write(rig.out().join("sleep"), "").unwrap();
    let (a, b) = (rig.add(), rig.add());
    let stderr = rig.dispatch();
    assert!(
        stderr.contains("axe node: runtimes claude-1 (claude-code, ok), claude-2 (claude-code, ok), codex-1 (codex, ok)"),
        "{stderr}"
    );
    let mut calls = rig.calls();
    calls.sort();
    assert_eq!(
        calls,
        [
            format!("{a} fresh default default"),
            format!("{b} fresh {two} default"),
        ],
        "the second task goes to the runtime with no live run"
    );
    for (id, name) in [(a, "claude-1"), (b, "claude-2")] {
        let t = rig.task(id);
        assert_eq!(t.state, "done");
        assert_eq!(t.labels, [format!("runtime:{name}")]);
        let s = &rig.starts(id)[0];
        assert_eq!(
            (&s["agent"], &s["runtime"], &s["harness"]),
            (&json!(name), &json!(name), &json!("claude-code")),
            "{s}"
        );
        let runs = rig.srv.get(&format!("/api/runs?task={id}"));
        assert_eq!(runs[0]["agent"], name, "the run records its runtime");
    }
}

#[test]
fn unhealthy_runtimes_are_skipped_for_the_next_use_entry_or_hold_the_task() {
    let rig = Rig::new(&[
        ("claude-1", CLAUDE, NEEDS_LOGIN),
        (
            "claude-2",
            &claude_on(&tempfile::tempdir().unwrap().path().display().to_string()),
            LIMITED,
        ),
        ("codex-1", CODEX, ""),
    ]);
    rig.set_rule(&["claude-code", "codex"]);
    let a = rig.add();
    rig.dispatch();
    assert_eq!(rig.calls(), [format!("{a} fresh default default")]);
    assert_eq!(rig.task(a).labels, ["runtime:codex-1"]);
    assert_eq!(rig.starts(a)[0]["use"], "codex");

    // No healthy runtime of any use entry: the task waits, run refunded.
    rig.set_rule(&["claude-code"]);
    let b = rig.add();
    rig.dispatch();
    let t = rig.task(b);
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0), "{t:?}");
    assert!(t.not_before.is_some(), "{t:?}");
    assert!(rig.starts(b).is_empty());
    let log = rig.srv.ok(&["log", &b.to_string()]);
    assert!(log.contains("held for a healthy runtime"), "{log}");
    assert!(log.contains("claude-1 needs_login"), "{log}");

    // No healthy runtime at all: nothing is claimed.
    rig.set_state("codex-1", r#"{"state":"degraded","until":4000000000}"#);
    let c = rig.add();
    let stderr = rig.dispatch();
    assert!(
        stderr.contains("claims held: no healthy runtime"),
        "{stderr}"
    );
    let t = rig.task(c);
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0));
}

#[test]
fn a_stored_session_resumes_on_its_own_runtime() {
    let d = tempfile::tempdir().unwrap();
    let two = d.path().join("claude-2-login").display().to_string();
    let rig = Rig::new(&[
        ("claude-1", CLAUDE, LIMITED),
        ("claude-2", &claude_on(&two), ""),
    ]);
    std::fs::write(rig.out().join("pending"), "").unwrap();
    let id = rig.add();
    rig.dispatch();
    // claude-1 is healthy again and least used by name, but the session is claude-2's.
    rig.set_state("claude-1", r#"{"state":"ok"}"#);
    std::fs::remove_file(rig.out().join("pending")).unwrap();
    rig.wake(id);
    rig.dispatch();
    assert_eq!(
        rig.calls(),
        [
            format!("{id} fresh {two} default"),
            format!("{id} resume {two} default")
        ]
    );
    let s = &rig.starts(id)[1];
    assert_eq!(
        (&s["mode"], &s["agent"]),
        (&json!("resume"), &json!("claude-2")),
        "{s}"
    );
    let t = rig.task(id);
    assert_eq!(t.state, "done", "{}", rig.srv.ok(&["log", &id.to_string()]));

    // A session from before runtimes names its agent: it ran on the default
    // login, so it resumes on the runtime that adopts it.
    let legacy = rig.add();
    let c = db::open(&rig.srv.db).unwrap();
    let s = db::Session {
        agent: "claude".into(),
        session_id: "legacy-session".into(),
        resume_count: 0,
    };
    db::save_session(&c, legacy, &s).unwrap();
    rig.dispatch();
    let s = &rig.starts(legacy)[0];
    assert_eq!(
        (&s["mode"], &s["agent"]),
        (&json!("resume"), &json!("claude-1")),
        "{s}"
    );
    assert_eq!(s["session_id"], "legacy-session");
}

#[test]
fn a_bot_session_from_before_runtimes_still_resumes() {
    let rig = Rig::new(&[("claude-1", CLAUDE, "")]);
    let f = rig.out().join("bot.json");
    let bot =
        json!({"name":"b","project":"p","agent":"claude","identity":"I","conduct":"C","notes":""});
    std::fs::write(&f, bot.to_string()).unwrap();
    rig.srv
        .ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    let c = db::open(&rig.srv.db).unwrap();
    c.execute(
        "UPDATE bots SET session_agent='claude', session_id='old-bot-session' WHERE name='b'",
        [],
    )
    .unwrap();
    rig.dispatch();
    assert_eq!(rig.calls(), ["bot:b resume default default"]);
    let b = rig.srv.get("/api/bots/b");
    assert_eq!(
        (&b["session_agent"], &b["session_id"]),
        (&json!("claude-1"), &json!("old-bot-session"))
    );
}

#[test]
fn failure_classes_mark_the_runtime_and_card_once() {
    let rig = Rig::new(&[("claude-1", CLAUDE, ""), ("codex-1", CODEX, "")]);
    rig.set_rule(&["claude-code"]);
    std::fs::write(rig.out().join("fail"), fixture("claude-usage-limit.jsonl")).unwrap();
    let id = rig.add();
    let stderr = rig.dispatch();
    let s = rig.state("claude-1");
    assert_eq!(s["state"], "limited", "{s}");
    assert!(s["until"].as_i64().unwrap() > 1_700_000_000, "{s}");
    assert_eq!(s["last_failure"]["class"], "quota_exhausted");
    // The server took the card (no Telegram here: not sent, but not retried).
    assert!(
        stderr.contains("runtime claude-1 turned limited: card"),
        "{stderr}"
    );
    assert_eq!(s["alerted"], "limited");
    let t = rig.task(id);
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0), "not escalated");

    // An expired login needs a new one; a probe that fails keeps it so.
    rig.set_state("claude-1", r#"{"state":"ok"}"#);
    std::fs::write(rig.out().join("fail"), fixture("claude-auth-expired.jsonl")).unwrap();
    rig.wake(id);
    rig.dispatch();
    let s = rig.state("claude-1");
    assert_eq!(
        (&s["state"], &s["alerted"]),
        (&json!("needs_login"), &json!("needs_login")),
        "{s}"
    );
    assert_eq!(rig.task(id).state, "pending");

    // Codex failures mark codex runtimes.
    rig.set_rule(&["codex"]);
    std::fs::write(rig.out().join("fail"), fixture("codex-at-capacity.jsonl")).unwrap();
    rig.wake(id);
    rig.dispatch();
    let s = rig.state("codex-1");
    assert_eq!(
        (&s["state"], &s["last_failure"]["class"]),
        (&json!("degraded"), &json!("provider_unavailable")),
        "{s}"
    );
}

#[test]
fn a_runtime_whose_login_fails_in_a_run_needs_login_and_refunds_the_run() {
    // A task run: refunded and held, the runtime needs a login, one card.
    let rig = Rig::new(&[("claude-1", CLAUDE, "")]);
    std::fs::write(
        rig.out().join("fail"),
        fixture("claude-not-logged-in.jsonl"),
    )
    .unwrap();
    let id = rig.add();
    let stderr = rig.dispatch();
    let s = rig.state("claude-1");
    assert_eq!(
        (&s["state"], &s["alerted"], &s["last_failure"]["class"]),
        (
            &json!("needs_login"),
            &json!("needs_login"),
            &json!("auth_invalid")
        ),
        "{s}"
    );
    assert_eq!(
        stderr
            .matches("runtime claude-1 turned needs_login: card")
            .count(),
        1,
        "{stderr}"
    );
    let t = rig.task(id);
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0), "{t:?}");

    // A bot run: an API error, not a failure; the bot stays active.
    let rig = Rig::new(&[("claude-1", CLAUDE, "")]);
    std::fs::write(
        rig.out().join("fail"),
        fixture("claude-not-logged-in.jsonl"),
    )
    .unwrap();
    let f = rig.out().join("bot.json");
    let bot =
        json!({"name":"b","project":"p","agent":"claude","identity":"I","conduct":"C","notes":""});
    std::fs::write(&f, bot.to_string()).unwrap();
    rig.srv
        .ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    let stderr = rig.dispatch();
    assert_eq!(rig.calls(), ["bot:b fresh default default"]);
    let runs = rig.srv.get("/api/bot-runs?bot=b");
    assert_eq!(runs[0]["stop_reason"], "api_error", "{runs}");
    let b = rig.srv.get("/api/bots/b");
    assert_eq!(
        (&b["state"], &b["failures"]),
        (&json!("active"), &json!(0)),
        "{b}"
    );
    assert_eq!(rig.state("claude-1")["state"], "needs_login");
    assert!(
        stderr.contains("runtime claude-1 turned needs_login: card"),
        "{stderr}"
    );
}

#[test]
fn a_login_probe_that_passes_makes_the_runtime_ok_again() {
    let rig = Rig::new(&[(
        "claude-1",
        &format!("{CLAUDE}probe = [\"true\"]\n"),
        NEEDS_LOGIN,
    )]);
    let id = rig.add();
    // Claims are held until the probe, started and collected on later ticks, passes.
    let out = rig.out().display().to_string();
    let worker = rig.srv.dispatcher(&[], &[("RT_OUT", &out)]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while rig.task(id).state != "done" && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    drop(worker);
    assert_eq!(rig.state("claude-1")["state"], "ok");
    assert_eq!(rig.task(id).state, "done");
}

/// A runtime never checked (no state.json) takes no run before its login
/// probe, the CLI's own `codex login status`, passes: without a login it
/// shows "not logged in" and holds its task; once logged in it runs.
#[test]
fn a_never_checked_codex_runtime_runs_only_after_its_login_probe_passes() {
    use std::os::unix::fs::PermissionsExt as _;
    // No `probe` line, so the default `codex login status` is checked (the
    // word keeps the rig from adding its failing one).
    let toml = format!("{CODEX}# default probe: codex login status\n");
    let rig = Rig::new(&[("codex-1", &toml, "-")]);
    let bin = rig.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("codex");
    std::fs::write(
        &fake,
        "#!/bin/sh\n[ \"$1 $2\" = \"login status\" ] || exit 2\n\
         [ -e \"$RT_OUT/loggedin\" ] && { echo 'Logged in using ChatGPT'; exit 0; }\n\
         echo 'Not logged in'; exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let out = rig.out().display().to_string();
    let id: i64 = rig
        .srv
        .ok(&["add", "--project", "p", "--title", "t", "--agent", "codex"])
        .trim()
        .parse()
        .unwrap();
    let wait = |what: &str, f: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    // No login: the probe fails, nothing runs, no card.
    let worker = rig
        .srv
        .dispatcher(&[], &[("RT_OUT", &out), ("PATH", &path)]);
    wait("the probe", &|| {
        std::fs::read_to_string(rig.dir.path().join("runtimes/codex-1/probe.log"))
            .is_ok_and(|l| l.contains("Not logged in"))
    });
    wait("the report", &|| {
        rig.srv
            .ok(&["runtimes"])
            .contains("codex-1 (codex): not logged in, 0 live")
    });
    std::thread::sleep(std::time::Duration::from_secs(2));
    drop(worker);
    assert!(rig.calls().is_empty(), "{:?}", rig.calls());
    let t = rig.task(id);
    assert_eq!((t.state.as_str(), t.runs), ("pending", 0), "{t:?}");

    // Logged in: the next node's first probe passes, the runtime is ok
    // (and saved), and the task runs.
    std::fs::write(rig.out().join("loggedin"), "").unwrap();
    let worker = rig
        .srv
        .dispatcher(&[], &[("RT_OUT", &out), ("PATH", &path)]);
    wait("the task", &|| rig.task(id).state == "done");
    drop(worker);
    let s = rig.state("codex-1");
    assert_eq!(s["state"], "ok", "{s}");
    assert!(s["login_at"].is_i64(), "{s}");
    assert_eq!(rig.calls().len(), 1, "{:?}", rig.calls());
}

#[test]
fn the_heartbeat_carries_state_and_quota_but_no_login_or_secret() {
    let d = tempfile::tempdir().unwrap();
    let secret_dir = d.path().join("secret-login-dir").display().to_string();
    let rig = Rig::new(&[
        (
            "claude-1",
            &format!("{}plan = \"max\"\n", claude_on(&secret_dir)),
            "",
        ),
        ("codex-1", CODEX, LIMITED),
    ]);
    // A run that reports its quota window.
    let log = rig.out().join("quota.jsonl");
    std::fs::write(
        &log,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1795323600,"rateLimitType":"seven_day","unifiedWindows":{"seven_day":{"utilization":0.25,"resetsAt":1795323600,"status":"allowed"}}}}
"#,
    )
    .unwrap();
    // The fake prints the log and exits 1 (a task error; the quota still counts).
    std::fs::write(rig.out().join("fail"), log.display().to_string()).unwrap();
    rig.add();
    rig.dispatch();
    std::fs::remove_file(rig.out().join("fail")).unwrap();

    let v = rig.srv.get("/api/runtimes");
    assert_eq!(v["managed"], true, "{v}");
    assert_eq!(v["stale"], false, "{v}");
    let list = v["runtimes"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let keys = [
        "name",
        "harness",
        "plan",
        "state",
        "never_logged_in",
        "healthy",
        "until",
        "last_failure",
        "quota",
        "in_use",
    ];
    for r in list {
        let mut got: Vec<&str> = r.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        got.sort();
        let mut want = keys.to_vec();
        want.sort();
        assert_eq!(got, want, "{r}");
    }
    let text = v.to_string();
    assert!(!text.contains("secret-login-dir"), "{text}");
    assert!(!text.to_lowercase().contains("token"), "{text}");
    let claude = &list[0];
    assert_eq!(
        (&claude["name"], &claude["plan"], &claude["state"]),
        (&json!("claude-1"), &json!("max"), &json!("ok"))
    );
    assert_eq!(claude["quota"]["utilization"], 0.25, "{claude}");
    assert_eq!(claude["quota"]["window"], "seven_day");
    let codex = &list[1];
    assert_eq!(
        (&codex["state"], &codex["healthy"], &codex["until"]),
        (&json!("limited"), &json!(false), &json!(4000000000i64))
    );

    let shown = rig.srv.ok(&["runtimes"]);
    assert!(
        shown.contains("claude-1 (claude-code): ok, plan max, 0 live, quota 25% of seven_day"),
        "{shown}"
    );
    assert!(
        shown.contains("codex-1 (codex): limited until 2096-"),
        "{shown}"
    );
}

#[test]
fn a_registry_that_shares_one_login_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let reg = d.path().join("runtimes");
    for n in ["claude-1", "claude-2"] {
        std::fs::create_dir_all(reg.join(n)).unwrap();
        std::fs::write(reg.join(n).join("runtime.toml"), CLAUDE).unwrap();
    }
    let cfg = format!(
        "[runtimes]\ndir = {}\n",
        serde_json::to_string(&reg.display().to_string()).unwrap()
    );
    let err = axe::config::Config::parse(&cfg).unwrap_err();
    assert!(
        format!("{err:#}").contains("share one claude-code login"),
        "{err:#}"
    );
}

/// Task 1933: a Claude runtime added from `/runtimes` (`Registry::create`,
/// as the node applies it) persists: a node started afterwards lists it and
/// picks it as well as the first, each on its own login; once removed, no
/// run goes to it.
#[test]
fn an_added_runtime_survives_a_restart_and_shares_the_work() {
    let rig = Rig::new(&[(
        "claude-1",
        &format!("{CLAUDE}login_dir = \"~/.claude\"\n"),
        "",
    )]);
    let cfg_path = rig.dir.path().join("config.toml");
    let cfg = axe::config::Config::parse(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    let mut reg = axe::runtime::Registry::load(&cfg).unwrap();
    assert_eq!(reg.create(&cfg, "claude-code").unwrap(), "claude-2");
    let home = rig
        .dir
        .path()
        .join("runtimes/claude-2/home")
        .display()
        .to_string();
    // It needs a login first: no run goes to it.
    let a = rig.add();
    let stderr = rig.dispatch();
    assert!(
        stderr.contains(
            "axe node: runtimes claude-1 (claude-code, ok), claude-2 (claude-code, needs_login)"
        ),
        "{stderr}"
    );
    assert_eq!(rig.calls(), [format!("{a} fresh default default")]);

    // Logged in (its check passed): both share the work.
    rig.set_state("claude-2", r#"{"state":"ok"}"#);
    std::fs::write(rig.out().join("sleep"), "").unwrap();
    let (b, c) = (rig.add(), rig.add());
    rig.dispatch();
    let mut logins: Vec<String> = rig.calls()[1..]
        .iter()
        .map(|l| l.split_once(' ').unwrap().1.to_string())
        .collect();
    logins.sort();
    assert_eq!(
        logins,
        [
            format!("fresh {home} default"),
            "fresh default default".to_string()
        ],
        "one of tasks {b} and {c} on each login"
    );

    // Removed while idle: the next node runs only on claude-1.
    let mut reg = axe::runtime::Registry::load(&cfg).unwrap();
    reg.remove("claude-2", 0).unwrap();
    let d = rig.add();
    let stderr = rig.dispatch();
    assert!(
        stderr.contains("axe node: runtimes claude-1 (claude-code, ok)\n"),
        "{stderr}"
    );
    assert_eq!(
        rig.calls().last().unwrap(),
        &format!("{d} fresh default default")
    );
}
