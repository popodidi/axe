//! Selection rules end to end: the one-time import of `[defaults]`/`[[rule]]`,
//! specificity and priority at claim, fallback down a rule's `use` list, the
//! model and effort arguments, the runtime label, and the edit log.
mod common;

use axe::config::Config;
use axe::db;
use common::{Server, TOKEN};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Agents that finish their task, standing in for the two harnesses.
const AGENTS: &str = r#"
[agents.claude]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done", "sh", "{prompt}"]
harness = "claude-code"
bot_args = []

[agents.codex]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done", "sh", "{prompt}"]
harness = "codex"
"#;

fn config(dir: &Path, projects: &[&str], body: &str) -> PathBuf {
    let ws = dir.join("ws");
    for p in projects {
        std::fs::create_dir_all(ws.join(p)).unwrap();
    }
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "workspace_root = {ws:?}\nlog_dir = {logs:?}\npoll_seconds = 1\n[pool]\ntotal = 8\n{body}",
            ws = ws.display().to_string(),
            logs = dir.join("logs").display().to_string(),
        ),
    )
    .unwrap();
    path
}

fn add(s: &Server, args: &[&str]) -> i64 {
    let mut a = vec!["add", "--title", "t"];
    a.extend_from_slice(args);
    s.ok(&a).trim().parse().unwrap()
}

fn dispatch_once(s: &Server) -> String {
    let out = s.dispatcher(&["--once"], &[]).wait();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The `run_start` bodies of task `id`, oldest first.
fn starts(s: &Server, id: i64) -> Vec<Value> {
    let c = db::open(&s.db).unwrap();
    db::events(&c, id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run_start")
        .map(|e| serde_json::from_str(e.body.as_deref().unwrap()).unwrap())
        .collect()
}

fn rules(s: &Server) -> Vec<Value> {
    serde_json::from_str(&s.ok(&["rule", "list", "--json"])).unwrap()
}

fn log(s: &Server) -> Vec<Value> {
    serde_json::from_str(&s.ok(&["rule", "log", "--json"])).unwrap()
}

#[test]
fn import_keeps_selection_and_runs_once() {
    let d = tempfile::tempdir().unwrap();
    let toml = format!(
        r#"{AGENTS}
[defaults]
agents = ["claude"]
strategy = "fixed"
max_runs = 20

[[rule]]
match = {{ project = "b" }}
agents = ["codex"]

[[rule]]
match = {{ project = "c" }}
agents = ["codex"]

[[rule]]
match = {{ project = "c" }}
agents = ["claude", "codex"]

[[rule]]
match = {{ project = "a" }}
max_runs = 3
"#
    );
    let cfg_path = config(d.path(), &["a", "b", "c", "d"], &toml);
    let cfg = Config::load(&cfg_path).unwrap();
    let s = Server::start_with(d.path(), &cfg_path, Some(TOKEN), &[], &[]);
    let tasks: Vec<(&str, i64)> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|p| (p, add(&s, &["--project", p])))
        .collect();
    let stderr = dispatch_once(&s);
    assert!(stderr.contains("imported config rules once"), "{stderr}");
    assert!(stderr.contains("agents picked by stored rules"), "{stderr}");

    // Every task runs on the agent the config would have picked.
    for (project, id) in &tasks {
        let want = &cfg.resolve(project).agents[0];
        let start = &starts(&s, *id)[0];
        assert_eq!(start["agent"], want.as_str(), "project {project}: {start}");
        assert!(
            start["rule"].as_str().unwrap().starts_with("rule:"),
            "{start}"
        );
        let t = db::get_task(&db::open(&s.db).unwrap(), *id).unwrap();
        assert_eq!(t.state, "done");
        assert_eq!(t.labels, [format!("runtime:{want}")]);
    }

    // The imported rows: the default, then each [[rule]] with agents, in
    // file order as priority (the max_runs-only rule is not selection).
    let got: Vec<(String, Vec<String>, i64)> = rules(&s)
        .iter()
        .map(|r| {
            (
                r["match"]["label"]
                    .as_str()
                    .unwrap_or("default")
                    .to_string(),
                r["use"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|u| u["harness"].as_str().unwrap().to_string())
                    .collect(),
                r["priority"].as_i64().unwrap(),
            )
        })
        .collect();
    let v = |h: &[&str]| h.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        got,
        [
            ("c".to_string(), v(&["claude-code", "codex"]), 3),
            ("c".to_string(), v(&["codex"]), 2),
            ("b".to_string(), v(&["codex"]), 1),
            ("default".to_string(), v(&["claude-code"]), 0),
        ]
    );

    // A second start imports nothing again.
    let stderr = dispatch_once(&s);
    assert!(!stderr.contains("imported config rules once"), "{stderr}");
    assert_eq!(rules(&s).len(), 4);
    let kinds: Vec<String> = log(&s)
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        ["import", "imported", "imported", "imported", "imported"]
    );

    // Bots keep their own agent: no imported rule names a bot.
    let c = db::open(&s.db).unwrap();
    let stored = axe::rule::list(&c).unwrap();
    let bot = axe::rule::Subject {
        bot: Some("any".into()),
        ..Default::default()
    };
    assert_eq!(axe::rule::select(&stored, &bot), None);
}

#[test]
fn config_that_rules_cannot_express_keeps_picking() {
    let d = tempfile::tempdir().unwrap();
    // Two agents share one harness: a harness-only rule cannot tell them apart.
    let toml = format!(
        r#"{AGENTS}
[agents.other]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done"]
harness = "claude-code"

[[rule]]
match = {{ project = "a" }}
agents = ["other"]
"#
    );
    let cfg = config(d.path(), &["a"], &toml);
    let s = Server::start_with(d.path(), &cfg, Some(TOKEN), &[], &[]);
    let id = add(&s, &["--project", "a"]);
    let stderr = dispatch_once(&s);
    assert!(
        stderr.contains("deprecated [defaults]/[[rule]] config"),
        "{stderr}"
    );
    assert!(stderr.contains("share harness claude-code"), "{stderr}");
    assert_eq!(starts(&s, id)[0]["agent"], "other");
    assert_eq!(starts(&s, id)[0]["rule"], "project=a");
    assert!(rules(&s).is_empty());
    let t = db::get_task(&db::open(&s.db).unwrap(), id).unwrap();
    assert!(t.labels.is_empty(), "no runtime label on the config path");
}

#[test]
fn stored_rules_pick_by_specificity_priority_and_fall_back() {
    let d = tempfile::tempdir().unwrap();
    // Only a claude-code agent: every codex entry falls through.
    let toml = r#"
[agents.claude]
cmd = ["sh", "-c", "\"$AXE_BIN\" set \"$AXE_TASK\" --state done", "sh", "{prompt}"]
harness = "claude-code"
"#;
    let cfg = config(d.path(), &["x", "y", "z", "w"], toml);
    let s = Server::start_with(d.path(), &cfg, Some(TOKEN), &[], &[]);
    let rule = |args: &[&str]| {
        let mut a = vec!["rule", "add"];
        a.extend_from_slice(args);
        s.ok(&a)
    };
    rule(&["--default", "--use", "claude-code"]);
    rule(&[
        "--match",
        "label=x",
        "--use",
        "codex:gpt-5:high",
        "--use",
        "claude-code:opus:high",
    ]);
    rule(&[
        "--match",
        "label=x",
        "--match",
        "workload=prod-bug",
        "--use",
        "claude-code:sonnet:low",
    ]);
    rule(&[
        "--match",
        "label=y",
        "--use",
        "claude-code::max",
        "--priority",
        "5",
    ]);
    rule(&["--match", "label=y", "--use", "claude-code::low"]);
    rule(&["--match", "label=z", "--use", "codex"]);
    let out = s.run(&["rule", "add", "--use", "claude-code"]);
    assert!(!out.status.success(), "a rule needs --match or --default");
    let out = s.run(&["rule", "add", "--default", "--use", "claude"]);
    assert!(
        !out.status.success(),
        "harness names are claude-code and codex"
    );

    let fallback = add(&s, &["--project", "x"]);
    let specific = add(&s, &["--project", "x", "--workload", "prod-bug"]);
    let priority = add(&s, &["--project", "w", "--label", "y"]);
    let nothing = add(&s, &["--project", "z"]);
    let default = add(&s, &["--project", "w"]);
    let stderr = dispatch_once(&s);
    assert!(stderr.contains("agents picked by stored rules"), "{stderr}");

    let args = |id: i64| -> (String, Vec<String>) {
        let st = &starts(&s, id)[0];
        let cmd: Vec<String> = st["cmd"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        // sh -c <script> sh <spliced args...> <prompt>
        (
            st["use"].as_str().unwrap_or("").to_string(),
            cmd[4..cmd.len() - 1].to_vec(),
        )
    };
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        args(fallback),
        (
            "claude-code:opus:high".into(),
            v(&["--model", "opus", "--effort", "high"])
        )
    );
    assert_eq!(
        args(specific),
        (
            "claude-code:sonnet:low".into(),
            v(&["--model", "sonnet", "--effort", "low"])
        )
    );
    assert_eq!(
        args(priority),
        ("claude-code::max".into(), v(&["--effort", "max"]))
    );
    assert_eq!(args(default), ("claude-code".into(), v(&[])));

    // No entry of the rule has an agent: the claim escalates with what was tried.
    let c = db::open(&s.db).unwrap();
    let t = db::get_task(&c, nothing).unwrap();
    assert_eq!(t.state, "escalated");
    let note = db::events(&c, nothing)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|e| e.body.filter(|b| b.contains("no configured agent")));
    assert!(note.unwrap().contains("(codex)"));
    for id in [fallback, specific, priority, default] {
        assert_eq!(db::get_task(&c, id).unwrap().state, "done");
    }

    // Rules added first: the dispatcher's import skips its own rows.
    let import = log(&s).into_iter().find(|e| e["kind"] == "import").unwrap();
    assert_eq!(import["body"]["skipped"], "rules already stored");
    assert_eq!(rules(&s).len(), 6);
}

#[test]
fn edits_are_logged_with_their_actor() {
    let d = tempfile::tempdir().unwrap();
    let s = Server::start(d.path());
    let out = s.ok(&[
        "rule",
        "add",
        "--match",
        "bot=ops",
        "--use",
        "claude-code:sonnet:medium",
        "--actor",
        "bot:ops",
    ]);
    let id: i64 = out
        .strip_prefix("rule ")
        .and_then(|r| r.split_whitespace().next())
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        out.contains("match bot=ops use claude-code:sonnet:medium"),
        "{out}"
    );
    let list = s.ok(&["rule", "list"]);
    assert!(
        list.contains(&format!("#{id} priority 0 match bot=ops")),
        "{list}"
    );
    s.ok(&["rule", "rm", &id.to_string()]);
    assert!(!s.run(&["rule", "rm", &id.to_string()]).status.success());
    assert!(s.ok(&["rule", "list"]).contains("no rules"));
    let log = log(&s);
    let got: Vec<(&str, &str, i64)> = log
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap(),
                e["actor"].as_str().unwrap(),
                e["rule_id"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, [("removed", "human", id), ("added", "bot:ops", id)]);
    assert_eq!(log[0]["body"]["use"][0]["model"], "sonnet");
    let text = s.ok(&["rule", "log"]);
    assert!(text.contains(&format!("removed rule {id}")), "{text}");
}
