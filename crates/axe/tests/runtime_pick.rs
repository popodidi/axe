//! The runtime, model and effort a task or bot run gets: one table of cases
//! across the layers (default, project and label rules, task pin, bot
//! primary and backup) and runtime health, end to end through the server and
//! one node pass. It pins today's picks so the unified runtime config
//! (store doc harvis/docs/decisions/axe-layering-review.md, step 7) keeps
//! every existing task and bot on the runtime it runs on now.
mod common;

use std::path::Path;

use axe::db;
use common::{Server, TOKEN};
use serde_json::{json, Value};

/// Records its arguments (NUL-separated) per run, then finishes its task.
const FAKE: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\0' "$a"; done > "$PICK_OUT/args-${AXE_TASK:-bot}-${AXE_BOT_RUN:-0}"
if [ -n "$AXE_TASK" ]; then "$AXE_BIN" set "$AXE_TASK" --state done; fi
"#;

/// Runtimes on the node: (name, harness, agent template).
const RUNTIMES: &[(&str, &str, &str)] = &[
    ("claude-1", "claude-code", "claude"),
    ("claude-2", "claude-code", "claude"),
    ("codex-1", "codex", "codex"),
    ("codex-2", "codex", "codex"),
];

struct Rig {
    _tmp: tempfile::TempDir,
    srv: Server,
}

impl Rig {
    fn dir(&self) -> &Path {
        self._tmp.path()
    }

    /// A node with every runtime in [`RUNTIMES`] ok and projects `a`, `b`.
    fn new() -> Rig {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["claude", "codex"] {
            let p = bin.join(name);
            std::fs::write(&p, FAKE).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for (name, harness, agent) in RUNTIMES {
            let rt = dir.join("runtimes").join(name);
            std::fs::create_dir_all(rt.join("home")).unwrap();
            std::fs::write(
                rt.join("runtime.toml"),
                format!(
                    "harness = \"{harness}\"\nagent = \"{agent}\"\nlogin_dir = \"{}\"\nprobe = []\n",
                    rt.join("home").display()
                ),
            )
            .unwrap();
        }
        for p in ["a", "b"] {
            std::fs::create_dir_all(dir.join("ws").join(p)).unwrap();
        }
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let cfg = format!(
            r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1

[pool]
total = 8

[worktrees]

[runtimes]
dir = "{runtimes}"

[agents.claude]
cmd = ["{bin}/claude", "-p", "--session-id", "{{session_id}}", "{{prompt}}"]
resume_cmd = ["{bin}/claude", "-p", "--resume", "{{session_id}}", "{{prompt}}"]
session = "preset"
harness = "claude-code"
bot_args = []

[agents.codex]
cmd = ["{bin}/codex", "exec", "{{prompt}}"]
session = "none"
harness = "codex"
bot_args = []
"#,
            ws = dir.join("ws").display(),
            logs = dir.join("logs").display(),
            runtimes = dir.join("runtimes").display(),
            bin = bin.display(),
        );
        let path = dir.join("config.toml");
        std::fs::write(&path, cfg).unwrap();
        let srv = Server::start_with(dir, &path, Some(TOKEN), &[], &[]);
        let rig = Rig { _tmp: tmp, srv };
        for (name, _, _) in RUNTIMES {
            rig.health(name, "ok");
        }
        rig
    }

    /// Sets runtime `name`'s health: `ok`, `needs_login`, or `limited` for
    /// a long time.
    fn health(&self, name: &str, state: &str) {
        let body = match state {
            "limited" => json!({"state": state, "until": 4_000_000_000i64}),
            s => json!({"state": s}),
        };
        std::fs::write(
            self.dir().join("runtimes").join(name).join("state.json"),
            body.to_string(),
        )
        .unwrap();
    }

    fn ok(&self, args: &[&str]) -> String {
        self.srv.ok(args)
    }

    fn dispatch_once(&self) {
        let out = self.dir().join("out").display().to_string();
        let o = self
            .srv
            .dispatcher(&["--once"], &[("PICK_OUT", &out)])
            .wait();
        assert!(o.status.success(), "{o:?}");
    }

    /// Task `id`'s first run: (runtime, rule, model/effort arguments), or
    /// `None` with no run.
    fn task_pick(&self, id: i64) -> Option<(String, String, Vec<String>)> {
        let c = db::open(&self.srv.db).unwrap();
        let start: Value = db::events(&c, id)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "run_start")
            .map(|e| serde_json::from_str(e.body.as_deref().unwrap()).unwrap())?;
        let cmd: Vec<String> = serde_json::from_value(start["cmd"].clone()).unwrap();
        Some((
            start["agent"].as_str().unwrap().to_string(),
            start["rule"].as_str().unwrap_or("").to_string(),
            model_args(&cmd),
        ))
    }

    /// Bot `b`'s latest run: (runtime, rule, model/effort arguments).
    fn bot_pick(&self) -> Option<(String, String, Vec<String>)> {
        let log = self.ok(&["bot", "log", "b", "--full", "--json"]);
        let events: Vec<Value> = serde_json::from_str(&log).unwrap();
        let start: Value = events
            .iter()
            .filter(|e| e["kind"] == "run_start")
            .map(|e| serde_json::from_str::<Value>(e["body"].as_str().unwrap()).unwrap())
            .next_back()?;
        let run = start["run"].as_i64().unwrap();
        let raw = std::fs::read(self.dir().join(format!("out/args-bot-{run}"))).unwrap();
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        Some((
            start["agent"].as_str().unwrap().to_string(),
            start["rule"].as_str().unwrap_or("").to_string(),
            model_args(&args),
        ))
    }
}

/// The model and effort arguments of a command line (both harnesses).
fn model_args(cmd: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = cmd.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--model" | "--effort" | "-m" => {
                out.push(a.clone());
                out.extend(it.next().cloned());
            }
            "-c" => {
                if let Some(v) = it.next() {
                    if v.starts_with("model_reasoning_effort=") {
                        out.extend(["-c".to_string(), v.clone()]);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// One case: rules (`axe rule add` arguments), runtime health changes, the
/// task's `axe task add` arguments, and the expected
/// (runtime, rule, model/effort arguments); `None`: no run starts.
struct TaskCase {
    name: &'static str,
    rules: &'static [&'static [&'static str]],
    health: &'static [(&'static str, &'static str)],
    task: &'static [&'static str],
    want: Option<(&'static str, &'static str, &'static [&'static str])>,
}

const TASK_CASES: &[TaskCase] = &[
    TaskCase {
        name: "no rule: the node imports the built-in default, least-used runtime by name",
        rules: &[],
        health: &[],
        task: &["--project", "a"],
        want: Some(("claude-1", "rule:1 (default)", &[])),
    },
    TaskCase {
        name: "default rule with model and effort",
        rules: &[&["--default", "--use", "claude-code:opus:high"]],
        health: &[],
        task: &["--project", "a"],
        want: Some((
            "claude-1",
            "rule:1 (default)",
            &["--model", "opus", "--effort", "high"],
        )),
    },
    TaskCase {
        name: "project rule beats the default",
        rules: &[
            &["--default", "--use", "claude-code:opus"],
            &["--match", "label=b", "--use", "codex:gpt-5:low"],
        ],
        health: &[],
        task: &["--project", "b"],
        want: Some((
            "codex-1",
            "rule:2 (label=b)",
            &["-m", "gpt-5", "-c", "model_reasoning_effort=\"low\""],
        )),
    },
    TaskCase {
        name: "the more specific rule wins whole, over priority",
        rules: &[
            &["--match", "label=b", "--use", "codex", "--priority", "9"],
            &[
                "--match",
                "label=b",
                "--match",
                "workload=feature",
                "--use",
                "claude-code:sonnet",
            ],
        ],
        health: &[],
        task: &["--project", "b", "--workload", "feature"],
        want: Some((
            "claude-1",
            "rule:2 (workload=feature,label=b)",
            &["--model", "sonnet"],
        )),
    },
    TaskCase {
        name: "an unhealthy harness falls to the next use entry",
        rules: &[&[
            "--default",
            "--use",
            "codex:gpt-5",
            "--use",
            "claude-code:opus",
        ]],
        health: &[("codex-1", "limited"), ("codex-2", "limited")],
        task: &["--project", "a"],
        want: Some(("claude-1", "rule:1 (default)", &["--model", "opus"])),
    },
    TaskCase {
        name: "a never-logged-in or limited runtime is skipped for its sibling",
        rules: &[],
        health: &[("claude-1", "needs_login")],
        task: &["--project", "a"],
        want: Some(("claude-2", "rule:1 (default)", &[])),
    },
    TaskCase {
        name: "no healthy runtime for any entry: no run, the task waits",
        rules: &[&["--default", "--use", "codex"]],
        health: &[("codex-1", "limited"), ("codex-2", "limited")],
        task: &["--project", "a"],
        want: None,
    },
    TaskCase {
        name: "a task pin (an agent template) overrides the rule's harness list",
        rules: &[&["--default", "--use", "claude-code:opus:high"]],
        health: &[],
        task: &["--project", "a", "--agent", "codex"],
        want: Some(("codex-1", "task agent", &[])),
    },
    TaskCase {
        // Step 7 (runtime_config): before, a pin dropped the rule's model
        // and effort.
        name: "a pinned task keeps the rule's model/effort",
        rules: &[&[
            "--default",
            "--use",
            "claude-code:opus",
            "--use",
            "codex:gpt-5:low",
        ]],
        health: &[],
        task: &["--project", "a", "--agent", "codex"],
        want: Some((
            "codex-1",
            "task agent",
            &["-m", "gpt-5", "-c", "model_reasoning_effort=\"low\""],
        )),
    },
    TaskCase {
        name: "a pin runs on a healthy runtime of its template",
        rules: &[],
        health: &[("codex-1", "limited")],
        task: &["--project", "a", "--agent", "codex"],
        want: Some(("codex-2", "task agent", &[])),
    },
    TaskCase {
        name: "a pin with every runtime of its harness down: no run",
        rules: &[],
        health: &[("codex-1", "limited"), ("codex-2", "limited")],
        task: &["--project", "a", "--agent", "codex"],
        want: None,
    },
];

#[test]
fn task_runs_pick_their_runtime_model_and_effort() {
    let mut failures = Vec::new();
    for case in TASK_CASES {
        let rig = Rig::new();
        for r in case.rules {
            let mut a = vec!["rule", "add"];
            a.extend_from_slice(r);
            rig.ok(&a);
        }
        for (name, state) in case.health {
            rig.health(name, state);
        }
        let mut a = vec!["add", "--title", "t"];
        a.extend_from_slice(case.task);
        let id: i64 = rig.ok(&a).trim().parse().unwrap();
        rig.dispatch_once();
        let got = rig.task_pick(id);
        let want = case.want.map(|(rt, rule, args)| {
            (
                rt.to_string(),
                rule.to_string(),
                args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        });
        if got != want {
            failures.push(format!("{}: got {got:?}, want {want:?}", case.name));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// One bot case: bot `b` with `primary` and `backup`, rules, runtime health,
/// and the expected pick of its first run (`None`: its wake waits).
struct BotCase {
    name: &'static str,
    primary: &'static str,
    backup: Option<&'static str>,
    rules: &'static [&'static [&'static str]],
    health: &'static [(&'static str, &'static str)],
    want: Option<(&'static str, &'static str, &'static [&'static str])>,
}

const BOT_CASES: &[BotCase] = &[
    BotCase {
        name: "the primary runs it",
        primary: "claude-1",
        backup: None,
        rules: &[],
        health: &[],
        want: Some(("claude-1", "bot primary", &[])),
    },
    BotCase {
        name: "a primary naming the agent template runs on its runtime",
        primary: "claude",
        backup: None,
        rules: &[],
        health: &[],
        want: Some(("claude-1", "bot primary", &[])),
    },
    BotCase {
        name: "a rule that names no bot does not apply to bots",
        primary: "claude-1",
        backup: None,
        rules: &[&["--default", "--use", "claude-code:opus:high"]],
        health: &[],
        want: Some(("claude-1", "bot primary", &[])),
    },
    BotCase {
        name: "a bot rule sets model and effort of the picked harness, not the runtime",
        primary: "claude-1",
        backup: None,
        rules: &[&[
            "--match",
            "bot=b",
            "--use",
            "codex:gpt-5",
            "--use",
            "claude-code:sonnet:low",
        ]],
        health: &[],
        want: Some((
            "claude-1",
            "bot primary",
            &["--model", "sonnet", "--effort", "low"],
        )),
    },
    BotCase {
        name: "the backup runs it while the primary is unhealthy",
        primary: "claude-1",
        backup: Some("codex-1"),
        rules: &[],
        health: &[("claude-1", "limited")],
        want: Some(("codex-1", "bot backup (claude-1 unhealthy)", &[])),
    },
    BotCase {
        name: "no backup and the primary down: no run, even with a healthy sibling",
        primary: "claude-1",
        backup: None,
        rules: &[],
        health: &[("claude-1", "limited")],
        want: None,
    },
];

#[test]
fn bot_runs_pick_their_runtime_model_and_effort() {
    let mut failures = Vec::new();
    for case in BOT_CASES {
        let rig = Rig::new();
        for r in case.rules {
            let mut a = vec!["rule", "add"];
            a.extend_from_slice(r);
            rig.ok(&a);
        }
        let f = rig.dir().join("bot.json");
        std::fs::write(
            &f,
            json!({"name":"b","project":"a","agent":case.primary}).to_string(),
        )
        .unwrap();
        rig.ok(&["bot", "create", "--file", f.to_str().unwrap()]);
        if let Some(b) = case.backup {
            rig.ok(&["bot", "edit", "b", "--backup", b, "--why", "test"]);
        }
        for (name, state) in case.health {
            rig.health(name, state);
        }
        rig.dispatch_once();
        let got = rig.bot_pick();
        let want = case.want.map(|(rt, rule, args)| {
            (
                rt.to_string(),
                rule.to_string(),
                args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
        });
        if got != want {
            failures.push(format!("{}: got {got:?}, want {want:?}", case.name));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
