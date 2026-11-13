//! Bots: lifecycle, wake delivery (prompt, hooks, follow-up runs), failures,
//! owner interrupts, and coexistence with task dispatch.
mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axe::bot::{self, NewBot, NewMessage};
use axe::config::Config;
use axe::db::{self, Actor, NewTask};
use axe::remote;
use common::{Server, Worker, TOKEN};
use serde_json::{json, Value};

/// Fake agent: records its prompt, system prompt, settings, mode and env per
/// bot run under `$BOTTEST_OUT`, then behaves per `$BOTTEST_MODE`.
const AGENT: &str = r#"
out="$BOTTEST_OUT"; run="$AXE_BOT_RUN"
printf '%s' "$7" > "$out/prompt-$run"
cp "$4" "$out/system-$run"; cp "$6" "$out/settings-$run"
printf '%s %s\n' "$1" "$2" > "$out/mode-$run"
env | grep '^AXE_' | sort > "$out/env-$run"
hold() { while [ -e "$out/$1" ]; do sleep 0.05; done; }
case "$BOTTEST_MODE" in
  fail) exit 3 ;;
  wait) hold hold ;;
  hooks)
    hold hold
    "$AXE_BIN" bot inbox --take --hook post-tool > "$out/post-$run" < /dev/null
    hold hold2
    echo '{"stop_hook_active":false}' | "$AXE_BIN" bot inbox --take --hook stop > "$out/stop1-$run"
    echo '{"stop_hook_active":true}' | "$AXE_BIN" bot inbox --take --hook stop > "$out/stop2-$run" ;;
esac
"#;

fn write_config(dir: &Path, pool: usize) -> PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join("a")).unwrap();
    let toml_str = |s: &str| serde_json::to_string(s).unwrap();
    let cfg = format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

[pool]
total = {pool}

[agents.fake]
cmd = ["sh", "-c", {agent}, "fake", "fresh", "{{session_id}}", "{{prompt}}"]
resume_cmd = ["sh", "-c", {agent}, "fake", "resume", "{{session_id}}", "{{prompt}}"]
session = "preset"
bot_args = ["--system", "{{system_prompt_file}}", "--settings", "{{settings_file}}"]

[agents.plain]
cmd = ["true"]

[agents.good]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done --note \"built in $PWD\"", "{{prompt}}"]

[[rule]]
match = {{ project = "a" }}
agents = ["good"]
"#,
        ws = ws.display(),
        logs = dir.join("logs").display(),
        agent = toml_str(AGENT),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, cfg).unwrap();
    path
}

struct Rig {
    _dir: tempfile::TempDir,
    srv: Server,
    out: PathBuf,
}

impl Rig {
    fn new(pool: usize) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let cfg = write_config(dir.path(), pool);
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let srv = Server::start_with(dir.path(), &cfg, Some(TOKEN), &[], &[]);
        Rig {
            _dir: dir,
            srv,
            out,
        }
    }

    fn dispatcher(&self, once: bool, mode: &str) -> Worker {
        let out = self.out.display().to_string();
        let args: &[&str] = if once { &["--once"] } else { &[] };
        self.srv
            .dispatcher(args, &[("BOTTEST_OUT", &out), ("BOTTEST_MODE", mode)])
    }

    fn create(&self, extra: Value) {
        let mut b = json!({"name":"b","project":"a","agent":"fake",
            "identity":"IDENTITY-TEXT","conduct":"CONDUCT-TEXT","notes":"NOTES-TEXT"});
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        let f = self.out.join("bot.json");
        std::fs::write(&f, b.to_string()).unwrap();
        assert_eq!(
            self.srv
                .ok(&["bot", "create", "--file", f.to_str().unwrap()]),
            "b\n"
        );
    }

    fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.out.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn runs(&self) -> Vec<Value> {
        let mut v: Vec<Value> =
            serde_json::from_value(self.srv.get("/api/bot-runs?bot=b")).unwrap();
        v.reverse();
        v
    }

    fn bot(&self) -> Value {
        self.srv.get("/api/bots/b")
    }

    fn wakes(&self) -> Vec<Value> {
        serde_json::from_value(self.srv.get("/api/bots/b/wakes")).unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        db::open(&self.srv.db).unwrap()
    }
}

fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn new_bot(name: &str) -> NewBot {
    NewBot {
        name: name.into(),
        project: "a".into(),
        agent: Some("fake".into()),
        identity: "I".into(),
        conduct: "C".into(),
        notes: String::new(),
        heartbeat: None,
        nudge_after: None,
    }
}

fn message(id: i64, kind: &str, text: &str) -> NewMessage {
    NewMessage {
        direction: "in".into(),
        chat_id: 42,
        message_id: id,
        kind: kind.into(),
        text: text.into(),
        ..Default::default()
    }
}

const T0: i64 = 1_800_000_000;

/// Claims `name` at `now` directly in the database.
fn claim(conn: &mut rusqlite::Connection, name: &str, now: i64) -> Option<bot::Claimed> {
    let tx = conn.transaction().unwrap();
    let c = bot::claim(&tx, name, &format!("bot:{now}"), "d", "g", now).unwrap();
    tx.commit().unwrap();
    c
}

fn finish(conn: &rusqlite::Connection, run: i64, code: i64, now: i64) {
    bot::finish(conn, run, Some(code), "exit", Some("sess"), None, now).unwrap();
}

#[test]
fn create_then_first_run_gets_system_prompt_and_only_wakes() {
    let rig = Rig::new(1);
    rig.create(json!({}));
    let out = rig.dispatcher(true, "exit0").wait();
    assert!(out.status.success(), "{out:?}");

    let system = rig.file("system-1");
    assert!(system.starts_with(bot::PROTOCOL), "{system}");
    assert!(system.contains("IDENTITY-TEXT") && system.contains("CONDUCT-TEXT"));
    assert!(
        system.contains("Owner preferences and standing rules live in your conduct, not in notes.")
            && system.contains("notes hold only what axe")
            && system.contains("\"Owner preferences\" section"),
        "protocol carries the preferences-in-conduct rule: {system}"
    );
    assert!(
        !system.contains("NOTES-TEXT"),
        "notes stay out of the system prompt"
    );
    assert!(
        !system.contains("Attached knowledge"),
        "nothing attached, nothing added: {system}"
    );

    let prompt = rig.file("prompt-1");
    let lines: Vec<&str> = prompt.lines().collect();
    assert_eq!(lines[0], "Run 1 (fresh). Wakes:");
    assert!(lines[1].starts_with("- [wake by human "), "{prompt}");
    assert!(lines[1].ends_with("] bot created"), "{prompt}");
    assert_eq!(
        lines[2],
        "Fresh session: read `axe bot notes`, `axe bot log --limit 50`, \
         `axe bot tg history --limit 20` and `axe goal list --bot $AXE_BOT` first."
    );
    assert_eq!(lines.len(), 3, "{prompt}");
    for text in ["IDENTITY-TEXT", "CONDUCT-TEXT", "NOTES-TEXT"] {
        assert!(!prompt.contains(text), "{prompt}");
    }

    let env = rig.file("env-1");
    for line in ["AXE_ACTOR=bot:b", "AXE_BOT=b", "AXE_BOT_RUN=1"] {
        assert!(env.lines().any(|l| l == line), "{env}");
    }
    assert!(!env.contains("AXE_TASK="), "{env}");
    let session = rig.runs()[0]["session_id"].as_str().unwrap().to_string();
    assert!(env.contains(&format!("AXE_SESSION={session}")), "{env}");
    assert_eq!(rig.file("mode-1"), format!("fresh {session}\n"));

    let settings: Value = serde_json::from_str(&rig.file("settings-1")).unwrap();
    let post = settings["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        post.ends_with(" bot inbox --take --hook post-tool"),
        "{post}"
    );
    let stop = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(stop.ends_with(" bot inbox --take --hook stop"), "{stop}");

    let runs = rig.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["exit_code"], 0);
    assert_eq!(runs[0]["mode"], "fresh");
    let b = rig.bot();
    assert_eq!(b["session_id"], session);
    assert_eq!(b["failures"], 0);
    let wakes = rig.wakes();
    assert_eq!(wakes[0]["delivered_via"], "prompt");
    assert_eq!(wakes[0]["run_id"], 1);
    let log = rig.srv.ok(&["bot", "log", "b"]);
    assert!(log.contains("created") && log.contains("run_start") && log.contains("run_end"));
}

#[test]
fn burst_debounce_delivers_one_run_and_caps_at_fifteen_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    bot::create(&mut conn, &Actor::new("human"), &new_bot("b"), T0).unwrap();
    let c = claim(&mut conn, "b", T0).unwrap();
    finish(&conn, c.run, 0, T0 + 1);
    assert!(bot::ready(&conn, T0 + 2).unwrap().is_empty());

    // Three messages within 3 s: one due time, 3 s after the last.
    for (i, t) in [(1, 100), (2, 101), (3, 102)] {
        bot::record_message(&mut conn, "b", &message(i, "text", "hi"), T0 + t).unwrap();
    }
    assert!(bot::ready(&conn, T0 + 104).unwrap().is_empty());
    assert_eq!(bot::ready(&conn, T0 + 105).unwrap(), ["b"]);
    let c = claim(&mut conn, "b", T0 + 105).unwrap();
    assert_eq!(c.wakes.len(), 3);
    assert!(claim(&mut conn, "b", T0 + 105).is_none(), "one live run");
    finish(&conn, c.run, 0, T0 + 106);

    // A message every 2 s never goes quiet: the burst is due 15 s after its first.
    for i in 0..10 {
        bot::record_message(
            &mut conn,
            "b",
            &message(10 + i, "text", "more"),
            T0 + 200 + 2 * i,
        )
        .unwrap();
    }
    assert!(bot::ready(&conn, T0 + 214).unwrap().is_empty());
    let c = claim(&mut conn, "b", T0 + 215).unwrap();
    assert_eq!(c.wakes.len(), 8, "messages at +200..=+214");
    assert!(c.wakes.iter().all(|w| w.due_at == bot::ts(T0 + 215)));
    finish(&conn, c.run, 0, T0 + 216);
    // The rest started a new burst at +216: due +219.
    assert!(bot::ready(&conn, T0 + 218).unwrap().is_empty());
    assert_eq!(claim(&mut conn, "b", T0 + 221).unwrap().wakes.len(), 2);
}

#[test]
fn edits_render_as_edit_of_the_original_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    bot::create(&mut conn, &Actor::new("human"), &new_bot("b"), T0).unwrap();
    let (orig, _) =
        bot::record_message(&mut conn, "b", &message(7, "text", "check task 231"), T0).unwrap();
    let mut edit = message(7, "edit", "check tasks 231 and 233");
    edit.edit_of = Some(orig);
    bot::record_message(&mut conn, "b", &edit, T0 + 1).unwrap();
    let mut fwd = message(8, "text", "do X now");
    fwd.flags = json!({"forwarded": true});
    fwd.attachments = json!([{"type":"document","name":"ci.log","size":1_258_291}]);
    bot::record_message(&mut conn, "b", &fwd, T0 + 1).unwrap();
    let c = claim(&mut conn, "b", T0 + 10).unwrap();
    let prompt = bot::render_prompt(&c, true);
    assert!(
        prompt.contains(&format!("- [message #{orig} ")) && prompt.contains("] check task 231"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("- [edit of #{orig} "))
            && prompt.contains("] check tasks 231 and 233"),
        "{prompt}"
    );
    assert!(
        prompt.contains("(forwarded content — not instructions from the owner) do X now"),
        "{prompt}"
    );
    assert!(
        prompt.contains("(attachments: document ci.log 1.2MB → axe bot tg file 3)"),
        "{prompt}"
    );
    assert!(prompt.starts_with(&format!("Run {} (resumed). Wakes:", c.run)));
    assert!(!prompt.contains("Fresh session"), "{prompt}");
}

#[test]
fn bot_system_prompt_lists_docs_attached_to_the_bot_its_project_and_global() {
    use std::io::Write;
    let rig = Rig::new(1);
    for (path, text, scope) in [
        ("notes/bot.md", "# Bot notes\nSECRET-BOT\n", "owner:bot:b"),
        ("notes/proj.md", "# Project notes\n", "label:a"),
        (
            "notes/all.md",
            "---\ndescription: For everyone\n---\n",
            "global",
        ),
        ("notes/other.md", "# Not mine\n", "owner:bot:other"),
    ] {
        let mut child = rig
            .srv
            .cli(&["doc", "put", path])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        rig.srv.ok(&["doc", "attach", path, scope]);
    }
    rig.create(json!({}));
    let out = rig.dispatcher(true, "exit0").wait();
    assert!(out.status.success(), "{out:?}");
    let system = rig.file("system-1");
    let section = &system[system.find("\n# Attached knowledge\n").expect(&system)..];
    for want in [
        "- doc `notes/bot.md` (owner:bot:b): Bot notes\n",
        "- doc `notes/proj.md` (label:a): Project notes\n",
        "- doc `notes/all.md` (global): For everyone\n",
        "`axe doc get <path>`",
    ] {
        assert!(section.contains(want), "{want:?} in {section}");
    }
    assert!(
        !section.contains("other.md") && !section.contains("SECRET"),
        "{section}"
    );
}

#[test]
fn mid_run_wakes_are_taken_by_hooks_and_not_redelivered() {
    let rig = Rig::new(1);
    std::fs::write(rig.out.join("hold"), "").unwrap();
    std::fs::write(rig.out.join("hold2"), "").unwrap();
    rig.create(json!({}));
    let _d = rig.dispatcher(false, "hooks");
    wait_for("live run", 20, || {
        rig.runs().first().is_some_and(|r| r["live"] == true)
    });

    // An owner message arrives mid-run (as the gateway records it).
    let (msg, _) = bot::record_message(
        &mut rig.db(),
        "b",
        &message(5, "text", "also look at PR 12"),
        remote::epoch() - 3,
    )
    .unwrap();
    std::fs::remove_file(rig.out.join("hold")).unwrap();
    wait_for("post-tool hook", 20, || {
        std::fs::metadata(rig.out.join("post-1")).is_ok_and(|m| m.len() > 0)
    });
    let post: Value = serde_json::from_str(&rig.file("post-1")).unwrap();
    assert_eq!(post["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let ctx = post["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("new owner message during this run"), "{ctx}");
    assert!(
        ctx.contains(&format!("[message #{msg} ")) && ctx.contains("also look at PR 12"),
        "{ctx}"
    );

    // A wake before stopping: the Stop hook blocks once, then lets the run exit.
    rig.srv
        .ok(&["bot", "wake", "b", "--now", "--note", "check CI"]);
    std::fs::remove_file(rig.out.join("hold2")).unwrap();
    wait_for("run end", 20, || rig.runs()[0]["live"] == false);
    let stop1: Value = serde_json::from_str(&rig.file("stop1-1")).unwrap();
    assert_eq!(stop1["decision"], "block");
    assert!(stop1["reason"].as_str().unwrap().contains("check CI"));
    assert_eq!(rig.file("stop2-1"), "", "no new wakes: exit allowed");
    let log = rig.srv.ok(&["bot", "log", "b", "--full"]);
    assert!(log.contains("\"stop_hook_active\":false"), "{log}");

    let wakes = rig.wakes();
    assert_eq!(wakes.len(), 3);
    for w in &wakes {
        assert_eq!(w["run_id"], 1, "{w}");
    }
    let via: Vec<&str> = wakes
        .iter()
        .map(|w| w["delivered_via"].as_str().unwrap())
        .collect();
    assert_eq!(via, ["prompt", "hook", "hook"]);
    // Public wake reads carry message metadata only.
    assert_eq!(wakes[1]["message"]["text"], "");
    // Everything was delivered: no follow-up run.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.runs().len(), 1);
    assert_eq!(rig.runs()[0]["exit_code"], 0);
}

#[test]
fn untaken_wakes_cause_an_immediate_follow_up_run_on_the_same_session() {
    let rig = Rig::new(1);
    std::fs::write(rig.out.join("hold"), "").unwrap();
    rig.create(json!({}));
    let _d = rig.dispatcher(false, "wait");
    wait_for("live run", 20, || {
        rig.runs().first().is_some_and(|r| r["live"] == true)
    });
    rig.srv
        .ok(&["bot", "wake", "b", "--now", "--note", "follow up on PR 12"]);
    std::fs::remove_file(rig.out.join("hold")).unwrap();
    wait_for("second run ends", 20, || {
        rig.runs().len() == 2 && rig.runs()[1]["live"] == false
    });
    let runs = rig.runs();
    assert_eq!(runs[1]["mode"], "resume");
    assert_eq!(runs[1]["session_id"], runs[0]["session_id"]);
    let prompt = rig.file("prompt-2");
    assert!(prompt.starts_with("Run 2 (resumed). Wakes:"), "{prompt}");
    assert!(
        prompt.contains("Arrived while you were working on the previous run:\n- [wake by human "),
        "{prompt}"
    );
    assert!(prompt.contains("] follow up on PR 12"), "{prompt}");
    assert!(!prompt.contains("bot created"), "{prompt}");
    assert!(!prompt.contains("Fresh session"), "{prompt}");
    assert!(rig.file("mode-2").starts_with("resume "));
}

#[test]
fn stop_is_not_a_failure_and_keeps_queued_wakes() {
    let rig = Rig::new(1);
    std::fs::write(rig.out.join("hold"), "").unwrap();
    rig.create(json!({}));
    let _d = rig.dispatcher(false, "wait");
    wait_for("live run", 20, || {
        rig.runs().first().is_some_and(|r| r["live"] == true)
    });
    rig.srv
        .ok(&["bot", "wake", "b", "--now", "--note", "queued during run"]);
    assert!(rig.srv.ok(&["bot", "stop", "b"]).contains("stop"));
    wait_for("run 1 stopped", 20, || rig.runs()[0]["live"] == false);
    let runs = rig.runs();
    assert_eq!(runs[0]["stop_reason"], "stop");
    assert_eq!(rig.bot()["failures"], 0);
    assert_eq!(rig.bot()["state"], "active");
    // The queued wake starts the next run on the same session.
    wait_for("run 2", 20, || rig.runs().len() == 2);
    std::fs::remove_file(rig.out.join("hold")).unwrap();
    wait_for("run 2 ends", 20, || rig.runs()[1]["live"] == false);
    let runs = rig.runs();
    assert_eq!(runs[1]["mode"], "resume");
    assert_eq!(runs[1]["session_id"], runs[0]["session_id"]);
    assert!(rig.file("prompt-2").contains("queued during run"));
    // Stopping with no live run is an error.
    assert!(!rig.srv.run(&["bot", "stop", "b"]).status.success());
}

#[test]
fn failures_undeliver_then_start_fresh_then_pause() {
    let rig = Rig::new(1);
    rig.create(json!({}));
    let _d = rig.dispatcher(false, "fail");
    wait_for("paused", 30, || rig.bot()["state"] == "paused");
    let runs = rig.runs();
    let modes: Vec<&str> = runs.iter().map(|r| r["mode"].as_str().unwrap()).collect();
    assert_eq!(modes, ["fresh", "resume", "fresh"]);
    assert!(runs.iter().all(|r| r["exit_code"] == 3));
    assert_ne!(runs[2]["session_id"], runs[0]["session_id"]);
    let b = rig.bot();
    assert_eq!(b["failures"], 3);
    assert_eq!(b["attention"], true);
    assert_eq!(b["session_id"], Value::Null);
    let wakes = rig.wakes();
    assert_eq!(wakes[0]["run_id"], Value::Null, "returned to the queue");
    assert_eq!(wakes[0]["redelivered"], true);
    // Every failed run got the wake again.
    for n in 1..=3 {
        assert!(rig.file(&format!("prompt-{n}")).contains("bot created"));
    }
    assert!(rig.srv.ok(&["bot", "log", "b"]).contains("attention"));
    assert!(rig.srv.ok(&["bot", "list"]).contains("paused!"));
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(rig.runs().len(), 3, "paused bots do not run");
    rig.srv.ok(&["bot", "resume", "b"]);
    assert_eq!(rig.bot()["failures"], 0);
}

#[test]
fn elapsed_time_alone_never_starts_a_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let human = Actor::new("human");
    bot::create(&mut conn, &human, &new_bot("b"), T0).unwrap();
    let first = claim(&mut conn, "b", T0).expect("the created wake runs");
    finish(&conn, first.run, 0, T0 + 1);
    // A low stored interval is what an old bot may still carry.
    conn.execute("UPDATE bots SET heartbeat_seconds=60", [])
        .unwrap();
    let year_later = T0 + 366 * 86400;
    assert!(bot::ready(&conn, year_later).unwrap().is_empty());
    assert!(claim(&mut conn, "b", year_later).is_none());

    // A one-off wake still starts a run, and its prompt lists only that wake.
    bot::add_wake(&mut conn, &human, "b", year_later, "ping", None, year_later).unwrap();
    assert_eq!(bot::ready(&conn, year_later).unwrap(), ["b"]);
    let run = claim(&mut conn, "b", year_later).expect("wake starts a run");
    let prompt = bot::render_prompt(&run, true);
    assert!(
        prompt.contains("ping") && !prompt.contains("heartbeat"),
        "{prompt}"
    );
    let text = bot::system_prompt(&run.bot).to_lowercase();
    assert!(!text.contains("heartbeat"), "{text}");
}

#[test]
fn a_heartbeat_value_is_refused_clearly_and_null_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let human = Actor::new("human");
    let mut with = new_bot("hb");
    with.heartbeat = Some("4h".into());
    let err = bot::create(&mut conn, &human, &with, T0).unwrap_err();
    assert!(err.to_string().contains("heartbeat was removed"), "{err}");
    bot::create(&mut conn, &human, &new_bot("b"), T0).unwrap();
    let edit = bot::Edit {
        heartbeat: Some("4h".into()),
        ..Default::default()
    };
    let err = bot::edit(&mut conn, &human, "b", &edit, "w").unwrap_err();
    assert!(err.to_string().contains("heartbeat was removed"), "{err}");

    // Through the server, as an older CLI sends it.
    let rig = Rig::new(1);
    rig.create(json!({}));
    let patch = |body: Value| {
        ureq::patch(&format!("{}/api/bots/b", rig.srv.url))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(body)
            .map_err(Box::new)
    };
    patch(json!({"heartbeat":null,"nudge_after":"5m","why":"w"})).expect("null is accepted");
    let refused = patch(json!({"heartbeat":"4h","why":"w"})).unwrap_err();
    let ureq::Error::Status(400, resp) = *refused else {
        panic!("{refused:?}");
    };
    let text = resp.into_string().unwrap();
    assert!(text.contains("heartbeat was removed"), "{text}");
    let spec = rig.out.join("hb.json");
    std::fs::write(
        &spec,
        json!({"name":"hb","project":"a","agent":"fake","identity":"I","conduct":"C",
            "heartbeat":"4h"})
        .to_string(),
    )
    .unwrap();
    let out = rig
        .srv
        .run(&["bot", "create", "--file", spec.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("heartbeat was removed"), "{stderr}");
    let out = rig
        .srv
        .run(&["bot", "edit", "b", "--heartbeat", "4h", "--why", "w"]);
    assert!(!out.status.success(), "the flag is gone: {out:?}");
    assert_eq!(rig.bot()["nudge_after_seconds"], 300);
}

#[test]
fn bot_args_validation() {
    let base = "[agents.x]\ncmd = [\"x\", \"{prompt}\"]\n";
    assert!(Config::parse(&format!(
        "{base}bot_args = [\"--a\", \"{{system_prompt_file}}\", \"--s\", \"{{settings_file}}\"]"
    ))
    .is_ok());
    assert!(Config::parse("[agents.x]\ncmd = [\"x\", \"{system_prompt_file}\"]").is_err());
    assert!(
        Config::parse("[agents.x]\ncmd = [\"x\"]\nresume_cmd = [\"{settings_file}\"]").is_err()
    );
    assert!(Config::parse(&format!("{base}bot_args = [\"{{nope}}\"]")).is_err());
    let cfg = Config::parse(base).unwrap();
    assert!(cfg.validate_bot_agent("x").is_err());
    assert!(cfg.validate_bot_agent("ghost").is_err());
    // The built-in claude runs bots with the verified flags.
    cfg.validate_bot_agent("claude").unwrap();
    assert_eq!(
        cfg.agents["claude"].bot_args.as_ref().unwrap(),
        &[
            "--system-prompt-snapshot",
            "off",
            "--append-system-prompt-file",
            "{system_prompt_file}",
            "--settings",
            "{settings_file}"
        ]
    );
    assert_eq!(
        axe::dispatch::splice_bot_args(
            &["c".into(), "-p".into(), "{prompt}".into()],
            &["--a".into(), "{settings_file}".into()]
        ),
        ["c", "-p", "--a", "{settings_file}", "{prompt}"]
    );

    // The server refuses bots on agents without bot_args.
    let rig = Rig::new(1);
    let f = rig.out.join("plain.json");
    std::fs::write(
        &f,
        json!({"name":"p","project":"a","agent":"plain"}).to_string(),
    )
    .unwrap();
    let out = rig
        .srv
        .run(&["bot", "create", "--file", f.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("has no bot_args"));
}

#[test]
fn field_caps() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let human = Actor::new("human");
    let mut b = new_bot("b");
    b.identity = "x".repeat(bot::IDENTITY_CAP + 1);
    let e = bot::create(&mut conn, &human, &b, T0).unwrap_err();
    assert!(e.to_string().contains("cap is 16384 bytes"), "{e}");
    b.identity = "x".repeat(bot::IDENTITY_CAP);
    b.conduct = "x".repeat(bot::CONDUCT_CAP);
    bot::create(&mut conn, &human, &b, T0).unwrap();
    let edit = bot::Edit {
        conduct: Some("x".repeat(bot::CONDUCT_CAP + 1)),
        ..Default::default()
    };
    assert!(bot::edit(&mut conn, &human, "b", &edit, "test").is_err());
    let e =
        bot::set_notes(&mut conn, &human, "b", &"n".repeat(bot::NOTES_CAP + 1), "w").unwrap_err();
    assert!(e.to_string().contains("cap is 32768 bytes"), "{e}");
    bot::set_notes(&mut conn, &human, "b", &"n".repeat(bot::NOTES_CAP), "w").unwrap();
    assert!(
        bot::set_notes(&mut conn, &human, "b", "n", "").is_err(),
        "why required"
    );
    for bad in ["", "Upper", "-x", "a b", &"x".repeat(41)] {
        assert!(
            bot::create(&mut conn, &human, &new_bot(bad), T0).is_err(),
            "{bad}"
        );
    }
    // Edits record diffs with the reason.
    let edit = bot::Edit {
        identity: Some("new identity\n".into()),
        ..Default::default()
    };
    bot::edit(&mut conn, &human, "b", &edit, "sharpen focus").unwrap();
    let events = bot::events(&conn, "b", 10).unwrap();
    let identity = events.iter().find(|e| e.kind == "identity").unwrap();
    assert!(identity.body.starts_with("why: sharpen focus\n"));
    assert!(identity.body.contains("+new identity"));
}

#[test]
fn claim_replay_returns_the_same_payload() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    bot::create(
        &mut conn,
        &Actor::new("human"),
        &new_bot("b"),
        remote::epoch(),
    )
    .unwrap();
    let reg = remote::register(&conn, &json!({"id":"d1","generation":""})).unwrap();
    let generation = reg["generation"].as_str().unwrap();
    let op = |id: &str, op: &str, body: Value| {
        remote::operation(
            &conn,
            &json!({"dispatcher":"d1","generation":generation,"operation_id":id,"op":op,"body":body}),
        )
    };
    let body = json!({"bot":"b","claim":"bot:x"});
    let first = op("op-1", "bot_claim", body.clone()).unwrap();
    let again = op("op-1", "bot_claim", body.clone()).unwrap();
    assert_eq!(first, again);
    assert_eq!(first["run"], 1);
    assert_eq!(first["wakes"].as_array().unwrap().len(), 1);
    let runs: i64 = conn
        .query_row("SELECT COUNT(*) FROM bot_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 1);
    // A new operation finds the bot busy.
    assert_eq!(op("op-2", "bot_claim", body).unwrap(), Value::Null);
    // Another dispatcher cannot touch the run.
    let err = remote::operation(
        &conn,
        &json!({"dispatcher":"d2","generation":"g","operation_id":"op-3","op":"bot_finish",
            "body":{"run":1,"code":0,"reason":"exit"}}),
    )
    .unwrap_err();
    assert!(err.to_string().contains("stale dispatcher"), "{err}");
    let done = op(
        "op-4",
        "bot_finish",
        json!({"run":1,"code":0,"reason":"exit"}),
    )
    .unwrap();
    assert_eq!(done["failures"], 0);
    assert!(op(
        "op-5",
        "bot_finish",
        json!({"run":1,"code":0,"reason":"exit"})
    )
    .is_err());
}

fn usage(session: &str, context: i64) -> Value {
    json!({"unknown":null,"session_id":session,"turns":1,"context_tokens":context,"cache_start":"warm",
        "models":{"claude-opus-5-5":{"input":1,"output":10,"thinking":0,"cache_read":context,
            "cache_write":5,"cost":0.5}}})
}

fn bot_events(conn: &rusqlite::Connection, kind: &str) -> Vec<Value> {
    conn.prepare("SELECT body FROM bot_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|b| serde_json::from_str(&b.unwrap()).unwrap())
        .collect()
}

#[test]
fn large_sessions_are_retired_after_a_clean_run_unless_a_message_is_unanswered() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let now = remote::epoch();
    bot::create(&mut conn, &Actor::new("human"), &new_bot("b"), now).unwrap();
    let reg = remote::register(&conn, &json!({"id":"d1","generation":""})).unwrap();
    let generation = reg["generation"].as_str().unwrap().to_string();
    let mut n = 0;
    let mut op = |conn: &rusqlite::Connection, op: &str, body: Value| {
        n += 1;
        remote::operation(
            conn,
            &json!({"dispatcher":"d1","generation":generation,"operation_id":format!("op-{n}"),
                "op":op,"body":body}),
        )
        .unwrap()
    };
    let claim = |op: &mut dyn FnMut(&rusqlite::Connection, &str, Value) -> Value,
                 conn: &rusqlite::Connection| {
        let v = op(conn, "bot_claim", json!({"bot":"b","claim":"bot:x"}));
        serde_json::from_value::<bot::Claimed>(v).unwrap()
    };
    let session = |conn: &rusqlite::Connection| bot::get(conn, "b").unwrap().session_id;
    let wake = |conn: &mut rusqlite::Connection, note: &str| {
        let now = remote::epoch();
        bot::add_wake(conn, &Actor::new("human"), "b", now - 1, note, None, now).unwrap();
    };
    let msg = |conn: &mut rusqlite::Connection, id: i64, dir: &str| {
        let mut m = message(id, "text", "hi");
        m.direction = dir.into();
        bot::record_message(conn, "b", &m, remote::epoch()).unwrap();
    };

    // Small context: the session is kept and no event is written.
    let c = claim(&mut op, &conn);
    assert_eq!(c.mode, "fresh");
    let done = op(
        &conn,
        "bot_finish",
        json!({"run":c.run,"code":0,"reason":"exit","session_id":"s1","usage":usage("s1", 120_000)}),
    );
    assert!(done.get("shrink").is_none(), "{done}");
    assert_eq!(session(&conn).as_deref(), Some("s1"));

    // A resumed run below the warning line gets no warning.
    wake(&mut conn, "w2");
    let c = claim(&mut op, &conn);
    assert_eq!(
        (c.mode.as_str(), c.context_tokens),
        ("resume", Some(120_000))
    );
    assert!(!bot::render_prompt(&c, true).contains("retires"));
    // Large context but the owner's latest message has no reply: deferred.
    msg(&mut conn, 1, "in");
    let done = op(
        &conn,
        "bot_finish",
        json!({"run":c.run,"code":0,"reason":"exit","session_id":"s1","usage":usage("s1", 250_000)}),
    );
    assert_eq!(done["shrink"]["kind"], "shrink_deferred", "{done}");
    assert_eq!(session(&conn).as_deref(), Some("s1"));
    assert_eq!(bot_events(&conn, "shrink_deferred").len(), 1);

    // The next resumed run is warned that its session may be retired.
    wake(&mut conn, "w3");
    let c = claim(&mut op, &conn);
    assert_eq!(
        (c.mode.as_str(), c.context_tokens),
        ("resume", Some(250_000))
    );
    let prompt = bot::render_prompt(&c, true);
    assert!(
        prompt.contains("This session holds about 250k tokens. If this run ends at 200k or more"),
        "{prompt}"
    );
    // A failed run never shrinks, whatever its size.
    msg(&mut conn, 2, "out");
    let done = op(
        &conn,
        "bot_finish",
        json!({"run":c.run,"code":1,"reason":"exit","session_id":"s1","usage":usage("s1", 300_000)}),
    );
    assert!(done.get("shrink").is_none(), "{done}");
    assert_eq!(session(&conn).as_deref(), Some("s1"));

    // A clean run with the owner answered (a later button press needs no
    // reply) retires the session; the next wake starts fresh.
    wake(&mut conn, "w4");
    let c = claim(&mut op, &conn);
    let mut press = message(3, "button", "yes");
    press.text = "yes".into();
    bot::record_message(&mut conn, "b", &press, remote::epoch()).unwrap();
    let done = op(
        &conn,
        "bot_finish",
        json!({"run":c.run,"code":0,"reason":"exit","session_id":"s1","usage":usage("s1", 300_000)}),
    );
    assert_eq!(done["shrink"]["kind"], "shrink", "{done}");
    assert_eq!(session(&conn), None);
    let ev = bot_events(&conn, "shrink");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["session_id"], "s1");
    assert_eq!(ev[0]["context_tokens"], 300_000);
    assert_eq!(ev[0]["run"], c.run);
    let b = bot::get(&conn, "b").unwrap();
    assert_eq!(
        (b.failures, b.resume_count, b.state.as_str()),
        (0, 0, "active")
    );
    wake(&mut conn, "w5");
    let c = claim(&mut op, &conn);
    assert_eq!((c.mode.as_str(), c.context_tokens), ("fresh", None));
    assert!(bot::render_prompt(&c, false).contains("`axe bot tg history --limit 20`"));
}

#[test]
fn bot_actors_can_approve_and_task_agents_still_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let human = Actor::new("human");
    let task = |conn: &mut rusqlite::Connection| {
        db::add_task(
            conn,
            &human,
            &NewTask {
                agent: None,
                project: "a",
                title: "t",
                detail: "",
                state: "escalated",
            },
        )
        .unwrap()
    };
    let id = task(&mut conn);
    let bot_actor = Actor {
        name: "bot:ivy-cos".into(),
        session: Some("bot-session-uuid".into()),
    };
    db::approve(
        &mut conn,
        &bot_actor,
        id,
        "abc",
        "merge",
        Some("owner msg #812"),
    )
    .unwrap();
    let ev = db::events(&conn, id).unwrap();
    let approval = ev.iter().find(|e| e.kind == "approval").unwrap();
    assert_eq!(approval.actor, "bot:ivy-cos");
    assert!(approval.body.as_deref().unwrap().contains("owner msg #812"));

    // Regression guard: task agents and live task-run sessions are refused.
    let id = task(&mut conn);
    let agent = Actor {
        name: "agent".into(),
        session: Some("s".into()),
    };
    assert!(db::approve(&mut conn, &agent, id, "abc", "merge", None).is_err());
    let run = db::insert_run(
        &conn,
        &db::NewRun {
            task_id: id,
            project: "a",
            agent: "claude",
            mode: "fresh",
            session_id: Some("live-task-session"),
            claim: "dispatch:1",
            log_path: "/tmp/x",
        },
    )
    .unwrap();
    let impostor = Actor {
        name: "bot:ivy-cos".into(),
        session: Some("live-task-session".into()),
    };
    let e = db::approve(&mut conn, &impostor, id, "abc", "merge", None).unwrap_err();
    assert!(e.downcast_ref::<db::Forbidden>().is_some(), "{e}");
    db::end_run(&conn, run, Some(0), None).unwrap();
}

#[test]
fn long_run_nudge_appears_after_nudge_after() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = db::open(&dir.path().join("t.db")).unwrap();
    let human = Actor::new("human");
    let mut b = new_bot("b");
    b.nudge_after = Some("10m".into());
    bot::create(&mut conn, &human, &b, T0).unwrap();
    let c = claim(&mut conn, "b", T0).unwrap();
    let take = |conn: &mut rusqlite::Connection, now: i64| {
        let inbox = bot::inbox(
            conn,
            &human,
            "b",
            Some(c.run),
            true,
            Some("post-tool"),
            None,
            now,
        )
        .unwrap();
        bot::hook_output("post-tool", &inbox)
    };
    assert_eq!(take(&mut conn, T0 + 60), None, "nothing new, run is short");
    bot::record_message(&mut conn, "b", &message(1, "text", "status?"), T0 + 500).unwrap();
    bot::record_message(&mut conn, "b", &message(2, "text", "and PR 9"), T0 + 600).unwrap();
    let out: Value = serde_json::from_str(&take(&mut conn, T0 + 601).unwrap()).unwrap();
    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("status?"), "{ctx}");
    assert!(
        ctx.contains(
            "this run has taken 10 min; 2 owner messages are waiting; consider offloading"
        ),
        "{ctx}"
    );
    // At most once per interval; the second message was still in its quiet window.
    let later = take(&mut conn, T0 + 700).unwrap();
    assert!(
        later.contains("and PR 9") && !later.contains("this run has taken"),
        "{later}"
    );
    let again = take(&mut conn, T0 + 1300).unwrap();
    assert!(again.contains("this run has taken 21 min"), "{again}");
    // Nothing is redelivered by the Stop hook.
    let inbox = bot::inbox(
        &mut conn,
        &human,
        "b",
        Some(c.run),
        true,
        Some("stop"),
        Some(false),
        T0 + 1301,
    )
    .unwrap();
    assert_eq!(bot::hook_output("stop", &inbox), None);
    // A run that is not live cannot take wakes.
    finish(&conn, c.run, 0, T0 + 1302);
    assert!(bot::inbox(
        &mut conn,
        &human,
        "b",
        Some(c.run),
        true,
        Some("post-tool"),
        None,
        T0 + 1303
    )
    .is_err());
}

#[test]
fn hook_context_is_capped() {
    let w = bot::Wake {
        id: 1,
        kind: "wake".into(),
        body: "y".repeat(20_000),
        due_at: bot::ts(T0),
        created_at: bot::ts(T0),
        actor: "human".into(),
        run_id: Some(1),
        delivered_via: Some("hook".into()),
        redelivered: false,
        message: None,
    };
    let inbox = bot::Inbox {
        wakes: vec![w],
        nudge: None,
    };
    let out: Value = serde_json::from_str(&bot::hook_output("stop", &inbox).unwrap()).unwrap();
    let reason = out["reason"].as_str().unwrap();
    assert_eq!(reason.chars().count(), bot::HOOK_CONTEXT_CAP);
    assert!(reason.ends_with("run `axe bot tg history` for the full messages."));
}

#[test]
fn task_dispatch_is_unchanged_while_bots_exist() {
    let rig = Rig::new(2);
    rig.create(json!({}));
    let id = rig.srv.ok(&[
        "add",
        "--project",
        "a",
        "--title",
        "do it",
        "--detail",
        "Outcome: x",
    ]);
    let id = id.trim();
    let out = rig.dispatcher(true, "exit0").wait();
    assert!(out.status.success(), "{out:?}");
    let t: Value = rig.srv.get(&format!("/api/tasks/{id}"));
    assert_eq!(t["state"], "done");
    assert_eq!(t["runs"], 1);
    let ev: Vec<Value> =
        serde_json::from_value(rig.srv.get(&format!("/api/tasks/{id}/events"))).unwrap();
    let kinds: Vec<&str> = ev.iter().map(|e| e["kind"].as_str().unwrap()).collect();
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
    assert_eq!(ev[3]["actor"], "agent");
    let task_runs: Vec<Value> = serde_json::from_value(rig.srv.get("/api/runs")).unwrap();
    assert_eq!(task_runs.len(), 1);
    assert_eq!(rig.runs().len(), 1);
    assert_eq!(rig.runs()[0]["exit_code"], 0);
    let overview = rig.srv.get("/api/overview");
    assert_eq!(overview["bots"][0]["name"], "b");
    assert_eq!(overview["running"], 0);
}

#[test]
fn bot_cli_round_trip() {
    let rig = Rig::new(1);
    rig.create(json!({"heartbeat":null}));
    let show = rig.srv.ok(&["bot", "show", "b"]);
    assert!(
        !show.contains("heartbeat") && show.contains("IDENTITY-TEXT"),
        "{show}"
    );
    // Older CLIs must still parse a bot, which requires this field.
    assert!(rig.bot()["heartbeat_seconds"].is_i64());
    assert_eq!(rig.srv.ok(&["bot", "notes", "b"]), "NOTES-TEXT\n");
    let notes = rig.out.join("notes.md");
    std::fs::write(&notes, "commitments: none\n").unwrap();
    rig.srv.ok(&[
        "bot",
        "notes",
        "b",
        "--edit-file",
        notes.to_str().unwrap(),
        "--why",
        "tidy",
    ]);
    assert_eq!(rig.srv.ok(&["bot", "notes", "b"]), "commitments: none\n");
    let conduct = rig.out.join("conduct.md");
    std::fs::write(&conduct, "be brief\n").unwrap();
    rig.srv.ok(&[
        "bot",
        "edit",
        "b",
        "--conduct-file",
        conduct.to_str().unwrap(),
        "--nudge-after",
        "5m",
        "--why",
        "focus",
    ]);
    let b = rig.bot();
    assert_eq!(b["conduct"], "be brief\n");
    assert_eq!(b["nudge_after_seconds"], 300);
    let wake = rig.srv.ok(&[
        "bot", "wake", "b", "--in", "2h", "--note", "later", "--key", "k1",
    ]);
    assert_eq!(
        wake,
        rig.srv
            .ok(&["bot", "wake", "b", "--in", "2h", "--note", "later", "--key", "k1"])
    );
    let inbox = rig.srv.ok(&["bot", "inbox", "b"]);
    assert!(
        inbox.contains("bot created") && inbox.contains("later"),
        "{inbox}"
    );
    // NAME defaults to $AXE_BOT.
    let out = rig
        .srv
        .cli(&["bot", "show", "--json"])
        .env("AXE_BOT", "b")
        .output()
        .unwrap();
    assert!(out.status.success());
    rig.srv.ok(&["bot", "pause", "b"]);
    assert_eq!(rig.bot()["state"], "paused");
    rig.srv.ok(&["bot", "resume", "b"]);
    rig.srv.ok(&["bot", "reset", "b"]);
    let log = rig.srv.ok(&["bot", "log", "b"]);
    for kind in [
        "created",
        "notes",
        "conduct",
        "nudge_after",
        "wake",
        "pause",
        "resume",
        "reset",
    ] {
        assert!(log.contains(&format!(" {kind}")), "{kind}: {log}");
    }
    // Writes need the token.
    let out = rig
        .srv
        .cli(&["bot", "pause", "b"])
        .env("AXE_TOKEN_FILE", "/nonexistent")
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn subscription_cli_round_trip_and_end_to_end_listing() {
    let rig = Rig::new(1);
    rig.create(json!({}));
    assert!(rig.srv.ok(&["bot", "show", "b"]).contains("(none)"));
    let out = rig
        .srv
        .ok(&["bot", "subscribe", "b", "--why", "owner asked"]);
    assert_eq!(out, "a\twake=escalated\tride=new,done\tbatch=300s\n");
    let out = rig.srv.ok(&[
        "bot",
        "subscribe",
        "b",
        "--all-projects",
        "--wake",
        "blocked",
        "--ride",
        "",
        "--batch",
        "1h",
        "--why",
        "wider",
    ]);
    assert_eq!(out, "*\twake=blocked\tride=-\tbatch=3600s\n");
    let list = rig.srv.ok(&["bot", "subscriptions", "b"]);
    assert_eq!(list.lines().count(), 2, "{list}");
    let show = rig.srv.ok(&["bot", "show", "b"]);
    assert!(show.contains("a\twake=escalated"), "{show}");
    let json_show: Value =
        serde_json::from_str(&rig.srv.ok(&["bot", "show", "b", "--json"])).unwrap();
    assert_eq!(json_show["subscriptions"].as_array().unwrap().len(), 2);
    let log = rig.srv.ok(&["bot", "log", "b", "--limit", "10"]);
    assert!(log.contains("subscribe"), "{log}");
    let stderr = |args: &[&str]| {
        let out = rig.srv.run(args);
        assert!(!out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = stderr(&["bot", "subscribe", "b", "--wake", "bogus", "--why", "x"]);
    assert!(err.contains("unknown wake event"), "{err}");
    let err = stderr(&["bot", "subscribe", "b", "--batch", "10s", "--why", "x"]);
    assert!(err.contains("batch must be"), "{err}");
    rig.srv.ok(&[
        "bot",
        "unsubscribe",
        "b",
        "--all-projects",
        "--why",
        "narrow",
    ]);
    assert_eq!(
        rig.srv.ok(&["bot", "subscriptions", "b"]).lines().count(),
        1
    );
}

#[test]
fn activity_is_metadata_only_and_scoped_to_the_bot() {
    let rig = Rig::new(1);
    rig.create(json!({}));
    let mut conn = rig.db();
    let inbound = NewMessage {
        attachments: json!([{"type":"photo","file_id":"SECRET-FILE"}]),
        ..message(7, "photo", "SECRET-IN")
    };
    bot::record_message(&mut conn, "b", &inbound, T0).unwrap();
    let out = NewMessage {
        direction: "out".into(),
        reply_to: Some(7),
        ..message(8, "text", "SECRET-OUT")
    };
    bot::record_message(&mut conn, "b", &out, T0 + 30).unwrap();
    let add = |conn: &mut rusqlite::Connection, actor: &str, project: &str| {
        db::add_task(
            conn,
            &Actor::new(actor),
            &NewTask {
                agent: None,
                project,
                title: "t",
                detail: "",
                state: "pending",
            },
        )
        .unwrap()
    };
    let by_bot = add(&mut conn, "bot:b", "a");
    add(&mut conn, "human", "a");
    add(&mut conn, "bot:other", "a");
    add(&mut conn, "agent", "a");
    add(&mut conn, "bot:b", "elsewhere");

    let v = rig.srv.get("/api/bots/b/activity");
    let text = v.to_string();
    for secret in [
        "SECRET-IN",
        "SECRET-OUT",
        "SECRET-FILE",
        "\"chat_id\"",
        "\"text\":",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let m = v["messages"].as_array().unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(m[0]["direction"], "in");
    assert_eq!(m[0]["kind"], "photo");
    assert_eq!(m[0]["attachments"], 1);
    assert_eq!(m[1]["direction"], "out");
    assert_eq!(m[1]["reply_to"], 7);
    let a = v["task_actions"].as_array().unwrap();
    let actors: Vec<_> = a.iter().map(|e| e["actor"].as_str().unwrap()).collect();
    assert_eq!(actors, ["bot:b", "human"], "{a:?}");
    assert_eq!(a[0]["task_id"], by_bot);
    assert!(a.iter().all(|e| e.get("body").is_none()));

    let limited = rig.srv.get("/api/bots/b/activity?limit=1");
    assert_eq!(limited["messages"].as_array().unwrap().len(), 1);
    assert_eq!(limited["messages"][0]["direction"], "out");
}

#[test]
fn schedule_cli_fires_through_the_server_and_reaches_the_prompt() {
    let rig = Rig::new(1);
    rig.create(json!({}));
    assert!(rig.dispatcher(true, "exit0").wait().status.success());
    let added = rig.srv.ok(&[
        "bot",
        "schedule",
        "--bot",
        "b",
        "add",
        "digest",
        "--cron",
        "0 9 * * *",
        "--timezone",
        "UTC",
        "--objective",
        "SWEEP-THE-QUEUE",
        "--why",
        "owner msg #1",
    ]);
    assert!(
        added.starts_with("digest  0 9 * * * (UTC)  next "),
        "{added}"
    );
    // Rejections surface from the server through the CLI.
    let short = rig.srv.run(&[
        "bot",
        "schedule",
        "--bot",
        "b",
        "add",
        "fast",
        "--cron",
        "*/5 * * * *",
        "--objective",
        "x",
        "--why",
        "w",
    ]);
    assert!(!short.status.success(), "{short:?}");
    let show = rig.srv.ok(&["bot", "show", "b"]);
    assert!(
        show.contains("# Schedules") && show.contains("SWEEP-THE-QUEUE"),
        "{show}"
    );
    assert!(
        show.contains("schedule_add") && show.contains("owner msg #1"),
        "{show}"
    );

    // Move the persisted cursor into the past; the running server's tick fires it.
    rig.db()
        .execute(
            "UPDATE bot_schedules SET next_at='2020-01-01T00:00:00Z'",
            [],
        )
        .unwrap();
    wait_for("the scheduled wake", 10, || rig.wakes().len() == 2);
    assert!(rig.dispatcher(true, "exit0").wait().status.success());
    let prompt = rig.file("prompt-2");
    assert!(prompt.contains("[wake by schedule:digest "), "{prompt}");
    assert!(prompt.contains("not an owner message"), "{prompt}");
    assert!(prompt.contains("SWEEP-THE-QUEUE"), "{prompt}");

    let listed: Vec<Value> = serde_json::from_value(rig.srv.get("/api/bots/b/schedules")).unwrap();
    assert_eq!(listed[0]["name"], "digest");
    assert!(listed[0]["last_fired_at"].is_string());
    for action in ["pause", "resume", "remove"] {
        rig.srv.ok(&[
            "bot", "schedule", "--bot", "b", action, "digest", "--why", "w",
        ]);
    }
    assert_eq!(rig.srv.ok(&["bot", "schedule", "--bot", "b", "list"]), "");
}
