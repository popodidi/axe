//! Bots on Codex: default bot arguments (system prompt and inbox hooks as
//! `-c` overrides), a JSONL session resumed by the next run, and switching a
//! bot's runtime with `axe bot edit --agent`.
mod common;

use std::path::Path;

use axe::bot::{self, NewBot};
use axe::config::Config;
use axe::db::{self, Actor};
use common::{Server, TOKEN};
use serde_json::{json, Value};

/// Fake `codex`: records its arguments (NUL-separated) per bot run and prints
/// a `thread.started` line like `codex exec --json`.
const FAKE_CODEX: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\0' "$a"; done > "$BOTTEST_OUT/args-$AXE_BOT_RUN"
echo '{"type":"thread.started","thread_id":"thread-'"$AXE_BOT_RUN"'"}'
echo '{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}'
"#;

fn rig(dir: &Path) -> Server {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let codex = bin.join("codex");
    std::fs::write(&codex, FAKE_CODEX).unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(dir.join("out")).unwrap();
    // As the VM bootstrap writes it: no bot_args.
    let cfg = format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

[worktrees]

[agents.codex]
cmd = ["{codex}", "exec", "--json", "--skip-git-repo-check", "-C", "{{cwd}}", "{{prompt}}"]
resume_cmd = ["{codex}", "exec", "resume", "--json", "--skip-git-repo-check", "{{session_id}}", "{{prompt}}"]
session = "jsonl:thread_id"

[agents.plain]
cmd = ["true"]
"#,
        ws = ws.display(),
        logs = dir.join("logs").display(),
        codex = codex.display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, cfg).unwrap();
    Server::start_with(dir, &path, Some(TOKEN), &[], &[])
}

fn args(dir: &Path, run: i64) -> Vec<String> {
    let raw = std::fs::read(dir.join(format!("out/args-{run}"))).unwrap();
    String::from_utf8(raw)
        .unwrap()
        .split_terminator('\0')
        .map(String::from)
        .collect()
}

/// The value of the `-c <key>=<value>` override, parsed as TOML.
fn override_value(args: &[String], key: &str) -> toml::Value {
    let prefix = format!("{key}=");
    let at = args
        .iter()
        .position(|a| a.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no -c {key}: {args:?}"));
    assert_eq!(args[at - 1], "-c", "{args:?}");
    let doc: toml::Table = toml::from_str(&format!("v = {}", &args[at][prefix.len()..]))
        .unwrap_or_else(|e| panic!("-c {key} is not TOML: {e}"));
    doc["v"].clone()
}

fn dispatch_once(srv: &Server, dir: &Path) {
    let out = dir.join("out").display().to_string();
    let o = srv.dispatcher(&["--once"], &[("BOTTEST_OUT", &out)]).wait();
    assert!(o.status.success(), "{o:?}");
}

#[test]
fn a_codex_agent_runs_bots_with_default_args() {
    let cfg = Config::parse(
        "[agents.codex]\ncmd = [\"/usr/local/bin/codex\", \"exec\", \"{prompt}\"]\n\
         [agents.other]\ncmd = [\"sh\", \"{prompt}\"]\n\
         [agents.mine]\ncmd = [\"codex\", \"{prompt}\"]\nbot_args = [\"--x\"]\n",
    )
    .unwrap();
    cfg.validate_bot_agent("codex").unwrap();
    assert_eq!(
        cfg.agents["codex"].bot_args().unwrap(),
        axe::harness::get("codex")
            .unwrap()
            .bot_args
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
    );
    // Configured bot_args win; other CLIs still need their own.
    assert_eq!(cfg.agents["mine"].bot_args().unwrap(), ["--x"]);
    assert!(cfg.validate_bot_agent("other").is_err());
    // The hooks override is the Claude settings' hooks object, as TOML.
    let toml_hooks: toml::Table =
        toml::from_str(&format!("v = {}", bot::hooks_toml("/opt/it's/axe"))).unwrap();
    let settings: Value = serde_json::from_str(&bot::settings_json("/opt/it's/axe")).unwrap();
    assert_eq!(
        serde_json::to_value(&toml_hooks["v"]).unwrap(),
        settings["hooks"]
    );
    let s = "line one\n\"quoted\" \\ tab\t and ''' end";
    let t: toml::Table = toml::from_str(&format!("v = {}", bot::toml_string(s))).unwrap();
    assert_eq!(t["v"].as_str(), Some(s));
}

#[test]
fn a_codex_bot_gets_its_system_prompt_and_hooks_and_resumes_its_thread() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = rig(dir);
    let f = dir.join("bot.json");
    std::fs::write(
        &f,
        json!({"name":"b","project":"a","agent":"codex","identity":"IDENTITY-TEXT",
            "conduct":"CONDUCT-TEXT"})
        .to_string(),
    )
    .unwrap();
    srv.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    dispatch_once(&srv, dir);

    let a = args(dir, 1);
    assert_eq!(&a[0..2], ["exec", "--json"]);
    assert!(
        a.contains(&"--dangerously-bypass-hook-trust".into()),
        "{a:?}"
    );
    let system = std::fs::read_to_string(dir.join("logs/bots/b/1.system.md")).unwrap();
    assert!(system.contains("IDENTITY-TEXT") && system.contains("CONDUCT-TEXT"));
    assert_eq!(
        override_value(&a, "developer_instructions").as_str(),
        Some(system.as_str())
    );
    let hooks = override_value(&a, "hooks");
    let post = hooks["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        post.ends_with(" bot inbox --take --hook post-tool"),
        "{post}"
    );
    assert_eq!(hooks["PostToolUse"][0]["matcher"].as_str(), Some("*"));
    let stop = hooks["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(stop.ends_with(" bot inbox --take --hook stop"), "{stop}");
    // The bot arguments go before the prompt, which stays last.
    assert!(a.last().unwrap().starts_with("Run 1 (fresh)"), "{a:?}");

    // The run's logged command names the files, not their inlined text.
    let log = srv.ok(&["bot", "log", "b", "--full"]);
    assert!(log.contains("developer_instructions=@"), "{log}");
    assert!(
        log.contains("1.system.md") || log.contains("system.md"),
        "{log}"
    );
    assert!(log.contains("hooks=@"), "{log}");
    assert!(!log.contains("IDENTITY-TEXT"), "{log}");

    let b = srv.get("/api/bots/b");
    assert_eq!(b["session_id"], "thread-1", "{b}");
    assert_eq!(b["session_agent"], "codex", "{b}");

    // The next wake resumes the same Codex thread with the same arguments.
    srv.ok(&["bot", "wake", "b", "--now", "--note", "again"]);
    dispatch_once(&srv, dir);
    let a = args(dir, 2);
    assert_eq!(
        &a[0..4],
        ["exec", "resume", "--json", "--skip-git-repo-check"]
    );
    assert_eq!(a[4], "thread-1");
    assert!(
        a.contains(&"--dangerously-bypass-hook-trust".into()),
        "{a:?}"
    );
    override_value(&a, "hooks");
    assert!(a.last().unwrap().starts_with("Run 2 (resumed)"), "{a:?}");
    let runs: Vec<Value> = serde_json::from_value(srv.get("/api/bot-runs?bot=b")).unwrap();
    assert!(runs.iter().all(|r| r["exit_code"] == 0), "{runs:?}");
}

#[test]
fn switching_a_bots_runtime_ends_its_session_and_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let srv = rig(dir);
    let f = dir.join("bot.json");
    std::fs::write(
        &f,
        json!({"name":"b","project":"a","agent":"claude","notes":"KEEP"}).to_string(),
    )
    .unwrap();
    srv.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    {
        let conn = db::open(&srv.db).unwrap();
        conn.execute(
            "UPDATE bots SET session_agent='claude', session_id='s-1', resume_count=3 WHERE name='b'",
            [],
        )
        .unwrap();
    }

    // Unknown agents and agents that cannot run bots are refused.
    for (agent, err) in [("ghost", "unknown agent"), ("plain", "has no bot_args")] {
        let o = srv.run(&["bot", "edit", "b", "--agent", agent, "--why", "w"]);
        assert!(!o.status.success(), "{agent}: {o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains(err), "{o:?}");
    }

    let out = srv.ok(&[
        "bot",
        "edit",
        "b",
        "--agent",
        "codex",
        "--why",
        "owner msg #5046: run on codex",
    ]);
    assert!(out.contains("runtime codex"), "{out}");
    let b = srv.get("/api/bots/b");
    assert_eq!(b["agent"], "codex");
    assert!(
        b["session_id"].is_null() && b["session_agent"].is_null(),
        "{b}"
    );
    assert_eq!(b["resume_count"], 0);
    assert_eq!(b["notes"], "KEEP");
    let log = srv.ok(&["bot", "log", "b", "--full"]);
    assert!(log.contains("agent"), "{log}");
    assert!(log.contains("claude -> codex"), "{log}");
    assert!(log.contains("owner msg #5046"), "{log}");
    assert!(log.contains("s-1"), "the ended session is recorded: {log}");

    // The next run starts fresh on the new runtime.
    dispatch_once(&srv, dir);
    let a = args(dir, 1);
    assert_eq!(&a[0..2], ["exec", "--json"]);

    // Same agent again: nothing changes, nothing recorded.
    let before = srv.ok(&["bot", "log", "b", "--full"]).len();
    srv.ok(&["bot", "edit", "b", "--agent", "codex", "--why", "w"]);
    assert_eq!(srv.ok(&["bot", "log", "b", "--full"]).len(), before);
}

#[test]
fn a_runtime_switch_is_refused_during_a_live_run() {
    let tmp = tempfile::tempdir().unwrap();
    let mut conn = db::open(&tmp.path().join("t.db")).unwrap();
    let now = 1_800_000_000;
    bot::create(
        &mut conn,
        &Actor::new("human"),
        &NewBot {
            name: "b".into(),
            project: "a".into(),
            agent: Some("claude".into()),
            identity: "I".into(),
            conduct: "C".into(),
            notes: String::new(),
            heartbeat: None,
            nudge_after: None,
        },
        now,
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    bot::claim(&tx, "b", None, "bot:1", "d", "g", now)
        .unwrap()
        .unwrap();
    tx.commit().unwrap();
    let edit = bot::Edit {
        agent: Some("codex".into()),
        ..Default::default()
    };
    let err = bot::edit(
        &mut conn,
        &Actor::new("human"),
        "b",
        &edit,
        "w",
        &|_| Ok(()),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("live run"), "{err:#}");
    assert_eq!(bot::get(&conn, "b").unwrap().agent, "claude");
}

#[test]
fn a_bot_pinned_to_an_agent_runs_on_its_only_runtime() {
    // A VM with no pre-made runtime: the owner added codex-1 (own login),
    // and the bot names the agent template `codex`.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let rt = dir.join("runtimes/codex-1");
    std::fs::create_dir_all(rt.join("home")).unwrap();
    std::fs::write(
        rt.join("runtime.toml"),
        format!(
            "harness = \"codex\"\nagent = \"codex\"\nlogin_dir = \"{}\"\n",
            rt.join("home").display()
        ),
    )
    .unwrap();
    std::fs::write(rt.join("state.json"), r#"{"state":"ok"}"#).unwrap();
    let srv = rig(dir);
    let cfg = dir.join("config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "\n[runtimes]\ndir = \"{}\"\n",
        dir.join("runtimes").display()
    ));
    std::fs::write(&cfg, text).unwrap();
    let f = dir.join("bot.json");
    std::fs::write(
        &f,
        json!({"name":"b","project":"a","agent":"codex"}).to_string(),
    )
    .unwrap();
    srv.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
    dispatch_once(&srv, dir);
    let runs: Vec<Value> = serde_json::from_value(srv.get("/api/bot-runs?bot=b")).unwrap();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["agent"], "codex-1", "{runs:?}");
    let log = srv.ok(&["bot", "log", "b", "--full"]);
    assert_eq!(runs[0]["exit_code"], 0, "{log}");
    assert_eq!(&args(dir, 1)[0..2], ["exec", "--json"]);
}
