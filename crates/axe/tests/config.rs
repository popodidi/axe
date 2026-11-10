use axe::config::{Config, Prompts, SessionMode, Strategy};

const CFG: &str = r#"
[defaults]
agents = ["claude"]
max_runs = 6

[agents.codex]
cmd = ["codex", "exec", "{prompt}"]
session = "jsonl:thread_id"

[[rule]]
match = { project = "ivy-ops" }
agents = ["codex"]

[[rule]]
agents = ["claude", "codex"]
strategy = "round_robin"

[[rule]]
match = { project = "ivy-ops" }
max_runs = 3

[[rule]]
match = { project = "ivy-ops" }
max_runs = 4
"#;

#[test]
fn missing_file_uses_builtin_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(&dir.path().join("nope.toml")).unwrap();
    assert_eq!(cfg.agents.keys().collect::<Vec<_>>(), ["claude"]);
    assert_eq!(
        cfg.agents["claude"].session_mode().unwrap(),
        SessionMode::Preset
    );
    let r = cfg.resolve("orchid");
    assert_eq!((r.agents, r.max_runs), (vec!["claude".to_string()], 6));
    assert_eq!(r.max_resumes, None);
    assert_eq!(cfg.pool_total(), 1);
}

#[test]
fn builtin_claude_has_no_system_prompt() {
    let cfg = Config::default();
    let claude = &cfg.agents["claude"];
    assert_eq!(
        args(&claude.cmd),
        [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--session-id",
            "{session_id}",
            "--permission-mode",
            "bypassPermissions",
            "{prompt}"
        ]
    );
    assert_eq!(
        args(claude.resume_cmd.as_deref().unwrap()),
        [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--resume",
            "{session_id}",
            "--permission-mode",
            "bypassPermissions",
            "{prompt}"
        ]
    );
}

#[test]
fn project_rules_override_catch_all_rules_fieldwise() {
    let cfg = Config::parse(CFG).unwrap();

    // Catch-all rule only.
    let r = cfg.resolve("orchid");
    assert_eq!(r.agents, ["claude", "codex"]);
    assert_eq!(r.strategy, Strategy::RoundRobin);
    assert_eq!(r.max_runs, 6);
    assert_eq!(r.rule_key, "project=*");

    // The project rule wins agents even though the catch-all comes later in the
    // file; strategy falls back to the catch-all; the last project rule wins max_runs.
    let r = cfg.resolve("ivy-ops");
    assert_eq!(r.agents, ["codex"]);
    assert_eq!(r.strategy, Strategy::RoundRobin);
    assert_eq!(r.max_runs, 4);
    assert_eq!(r.rule_key, "project=ivy-ops");

    // No rules: defaults.
    let r = Config::default().resolve("fern");
    assert_eq!((r.rule_key.as_str(), r.max_runs), ("defaults", 6));
}

#[test]
fn max_resumes_resolves_fieldwise() {
    let cfg = Config::parse(
        r#"
[defaults]
max_resumes = 8
[[rule]]
match = { project = "a" }
max_resumes = 2
"#,
    )
    .unwrap();
    assert_eq!(cfg.resolve("a").max_resumes, Some(2));
    assert_eq!(cfg.resolve("b").max_resumes, Some(8));
    assert!(Config::parse("[defaults]\nmax_resumes = -1").is_err());
}

#[test]
fn pool_total_must_be_positive() {
    assert_eq!(Config::parse("[pool]\ntotal = 5").unwrap().pool_total(), 5);
    let err = Config::parse("[pool]\ntotal = 0").unwrap_err();
    assert_eq!(format!("{err:#}"), "pool.total must be >= 1");
    assert!(Config::parse("[pool]\ntotal = -1").is_err());
}

#[test]
fn pool_bots_is_optional_positive_and_separate() {
    // Omitted: one slot per active bot, task total unchanged.
    let cfg = Config::parse("[pool]\ntotal = 5").unwrap();
    assert_eq!((cfg.pool_total(), cfg.pool_bots()), (5, None));
    assert_eq!(Config::parse("").unwrap().pool_bots(), None);
    let cfg = Config::parse("[pool]\ntotal = 5\nbots = 2").unwrap();
    assert_eq!((cfg.pool_total(), cfg.pool_bots()), (5, Some(2)));
    let err = Config::parse("[pool]\nbots = 0").unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "pool.bots must be >= 1 (omit it for one slot per active bot)"
    );
    assert!(Config::parse("[pool]\nbots = -1").is_err());
    assert!(Config::parse("[pool]\nbots = \"auto\"").is_err());
    // deny_unknown_fields still rejects a misspelt key next to it.
    assert!(Config::parse("[pool]\nbots = 2\nbot = 2").is_err());
    // Round-trips through serialization (the dispatcher config the tests write).
    let back = Config::parse(&toml::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back.pool, cfg.pool);
    let none = Config::parse("").unwrap();
    assert!(!toml::to_string(&none).unwrap().contains("bots"));
}

#[test]
fn retired_keys_are_rejected() {
    for old in [
        "personas = [\"builder\"]",
        "looped = [\"builder\"]",
        "persona_dir = \"~/p\"",
        "[pool]\ntotal = 3\npriority = [\"builder\"]",
        "[defaults]\ncontinuity = \"resume\"",
        "[defaults]\nconcurrency = 1",
        "[[rule]]\nmatch = { persona = \"builder\" }\nmax_runs = 1",
        "[[rule]]\ncontinuity = \"resume\"",
    ] {
        assert!(Config::parse(old).is_err(), "{old}");
    }
}

#[test]
fn unknown_template_variables_are_rejected() {
    for var in ["persona_file", "prompt_with_persona", "persona", "bogus"] {
        let cfg = format!("[agents.x]\ncmd = [\"x\", \"--f={{{var}}}\"]");
        let err = Config::parse(&cfg).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown template variable"),
            "{var}: {err:#}"
        );
        let resume = format!("[agents.x]\ncmd = [\"x\"]\nresume_cmd = [\"x\", \"{{{var}}}\"]");
        assert!(Config::parse(&resume).is_err(), "{var}");
    }
    // Known variables and non-identifier braces (inline JSON) are accepted.
    Config::parse(
        r#"[agents.x]
cmd = ["x", "{session_id}", "{prompt}", "{cwd}", "{task_id}", "{project}", "{\"a\":1}"]"#,
    )
    .unwrap();
}

#[test]
fn validation_errors() {
    assert!(Config::parse("[defaults]\nagents = [\"nope\"]").is_err());
    assert!(Config::parse("[defaults]\nagents = []").is_err());
    assert!(Config::parse("[[rule]]\nmatch = { project = \"a\" }\nagents = [\"ghost\"]").is_err());
    assert!(Config::parse("[agents.x]\ncmd = [\"x\"]\nsession = \"bogus\"").is_err());
    assert!(Config::parse("[agents.x]\ncmd = ['x']\nresume_cmd = []").is_err());
    assert!(Config::parse("poll_seconds = 0").is_err());
    assert!(Config::parse("unknown_key = 1").is_err());
}

#[test]
fn deployment_config_parses() {
    let cfg = Config::parse(
        r#"
workspace_root = "~/workspace"
log_dir = "~/.local/state/axe/logs"

[defaults]
agents = ["claude"]
max_runs = 20

[agents.codex]
cmd = ["codex", "exec", "--json", "{prompt}"]
resume_cmd = ["codex", "exec", "--json", "resume", "{session_id}", "{prompt}"]
session = "jsonl:thread_id"

[pool]
total = 5
bots = 2

[worktrees]
branch_prefix = "agent/"

[otel]
prefix = "acme"
resource = { "service.namespace" = "acme", "deployment.environment" = "lab" }

[metrics]
projects = ["alpha", "beta"]

[prompts]
fresh = "Do task {id} of {project}."
"#,
    )
    .unwrap();
    assert_eq!(cfg.pool_total(), 5);
    assert_eq!(cfg.pool_bots(), Some(2));
    assert!(cfg.rules.is_empty());
    assert_eq!(cfg.worktrees.as_ref().unwrap().branch_prefix, "agent/");
    assert_eq!(cfg.defaults.agents, ["claude"]);
    assert_eq!(cfg.defaults.strategy, Strategy::Fixed);
    assert_eq!(cfg.defaults.max_runs, 20);
    assert_eq!(
        cfg.agents["codex"].session_mode().unwrap(),
        SessionMode::Jsonl("thread_id".into())
    );
    for a in cfg.agents.values() {
        assert!(a.resume_cmd.is_some());
    }
    assert_eq!(cfg.metrics.projects, ["alpha", "beta"]);
    assert_eq!(
        cfg.otel.attributes(&[("task_id", "7")]),
        [
            ("deployment.environment".to_string(), "lab".to_string()),
            ("service.namespace".into(), "acme".into()),
            ("acme.task_id".into(), "7".into()),
        ]
    );
    assert_eq!(
        Prompts::render(&cfg.prompts.fresh, 7, "alpha"),
        "Do task 7 of alpha."
    );
    assert_eq!(cfg.prompts.resume, Prompts::default().resume);
}

#[test]
fn neutral_defaults() {
    let cfg = Config::default();
    assert!(cfg.server.cors_origins.is_empty());
    assert!(cfg.metrics.projects.is_empty());
    assert!(cfg.otel.resource.is_empty());
    assert_eq!(
        cfg.otel.attributes(&[("run", "dispatch:1")]),
        [("axe.run".to_string(), "dispatch:1".to_string())]
    );
    assert_eq!(
        Config::parse("[worktrees]\n")
            .unwrap()
            .worktrees
            .unwrap()
            .branch_prefix,
        "axe/"
    );
    assert_eq!(
        Prompts::render(&cfg.prompts.fresh, 3, "alpha"),
        "Work axe task 3 in project alpha: run axe show 3 and axe log 3, then carry it to done. Around a merge or deploy: axe protect 3, then --clear."
    );
    assert_eq!(axe::client::DEFAULT_SERVER, "http://127.0.0.1:8790");
}

#[test]
fn metrics_projects_are_validated() {
    assert!(Config::parse("[metrics]\nprojects = [\"other\"]").is_err());
    assert!(Config::parse("[metrics]\nprojects = [\"all\"]").is_err());
    assert!(Config::parse("[metrics]\nprojects = [\"a\\\"b\"]").is_err());
    assert!(Config::parse("[metrics]\nprojects = [\"\"]").is_err());
    assert!(Config::parse("[metrics]\nprojects = [\"a-b_c.d\"]").is_ok());
    assert!(Config::parse("[otel]\nbogus = 1").is_err());
    assert!(Config::parse("[prompts]\nbogus = \"x\"").is_err());
}

#[test]
fn worktrees_table_is_optional_and_ignores_retired_keys() {
    assert!(Config::parse("").unwrap().worktrees.is_none());
    assert!(Config::parse("[worktrees]\n").unwrap().worktrees.is_some());
    let old = "[worktrees]\nkeeper_config = \"k\"\nobserver_config = \"o\"\n";
    assert!(Config::parse(old).unwrap().worktrees.is_some());
}

fn args(c: &[String]) -> Vec<&str> {
    c.iter().map(String::as_str).collect()
}
