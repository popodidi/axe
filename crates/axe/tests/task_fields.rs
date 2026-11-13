//! Task fields (owner, repos, labels) through the CLI and API, and the run
//! workspace a task with repos gets from the dispatcher.
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Server, TOKEN};
use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn show(srv: &Server, id: &str) -> Value {
    serde_json::from_str(&srv.ok(&["show", id.trim(), "--json"])).unwrap()
}

fn ids(srv: &Server, args: &[&str]) -> Vec<i64> {
    let mut a = vec!["list", "--json"];
    a.extend_from_slice(args);
    let v: Value = serde_json::from_str(&srv.ok(&a)).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect()
}

#[test]
fn owner_repos_and_labels_round_trip_through_cli_and_api() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let a = srv.ok(&[
        "add",
        "--project",
        "p",
        "--title",
        "a",
        "--owner",
        "bot:ops",
        "--repo",
        "one,two",
        "--repo",
        "one",
        "--label",
        "x",
        "--label",
        "y,x",
    ]);
    let t = show(&srv, &a);
    assert_eq!(t["owner"], "bot:ops");
    assert_eq!(t["repos"], json!(["one", "two"]));
    assert_eq!(t["labels"], json!(["x", "y"]));
    let text = srv.ok(&["show", a.trim()]);
    assert!(
        text.contains("\nowner: bot:ops\nrepos: one, two\nlabels: x, y"),
        "{text}"
    );

    // A task without the fields keeps its project checkout.
    let b = srv.ok(&["add", "--project", "p", "--title", "b"]);
    let t = show(&srv, &b);
    assert_eq!(
        (t["owner"].clone(), t["repos"].clone()),
        (Value::Null, Value::Null)
    );
    assert_eq!(t["labels"], json!([]));
    assert!(srv
        .ok(&["show", b.trim()])
        .contains("repos: - (project checkout p)"));

    srv.ok(&[
        "set",
        a.trim(),
        "--owner",
        "none",
        "--no-repos",
        "--label",
        "z",
        "--unlabel",
        "x",
    ]);
    let t = show(&srv, &a);
    assert_eq!(t["owner"], Value::Null);
    assert_eq!(t["repos"], json!([]));
    assert_eq!(t["labels"], json!(["y", "z"]));
    srv.ok(&["set", b.trim(), "--owner", "alice", "--repo", "one"]);
    assert_eq!(show(&srv, &b)["repos"], json!(["one"]));

    // The creator stays the first event; each change is its own event.
    let log: Value = serde_json::from_str(&srv.ok(&["log", a.trim(), "--json"])).unwrap();
    let kinds: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["created", "owner", "repos", "labels", "owner", "repos", "labels"]
    );
    let repos = &log[5];
    assert_eq!(
        (repos["from_value"].as_str(), repos["to_value"].as_str()),
        (Some("one,two"), Some(""))
    );

    // The API takes the same fields.
    let v: Value = ureq::post(&format!("{}/api/tasks", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(json!({"project":"q","title":"c","owner":"bob","repos":[],"labels":["p"]}))
        .unwrap()
        .into_json()
        .unwrap();
    let c = v["id"].as_i64().unwrap();
    assert_eq!(v["task"]["repos"], json!([]));
    let v: Value = ureq::patch(&format!("{}/api/tasks/{c}", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(json!({"add_labels":["k"],"remove_labels":["p"],"owner":"carol"}))
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(v["task"]["labels"], json!(["k"]));
    assert_eq!(v["task"]["owner"], "carol");
    assert_eq!(srv.get(&format!("/api/tasks/{c}"))["labels"], json!(["k"]));

    // Filters: a label matches labels and the project; owner matches exactly.
    let (a, b): (i64, i64) = (a.trim().parse().unwrap(), b.trim().parse().unwrap());
    assert_eq!(ids(&srv, &["--label", "z"]), [a]);
    assert_eq!(ids(&srv, &["--label", "p"]), [a, b]);
    assert_eq!(ids(&srv, &["--label", "k"]), [c]);
    assert_eq!(ids(&srv, &["--owner", "alice"]), [b]);
    let api: Vec<i64> = srv
        .get("/api/tasks?label=q&owner=carol")
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(api, [c]);

    // Bad names are refused and nothing is written.
    for args in [
        ["add", "--project", "p", "--title", "d", "--label", "a b"],
        ["add", "--project", "p", "--title", "d", "--repo", "../x"],
        ["add", "--project", "p", "--title", "d", "--owner", "none"],
    ] {
        assert!(!srv.run(&args).status.success(), "{args:?}");
    }
    assert!(!srv
        .run(&["set", &a.to_string(), "--repo", ".git"])
        .status
        .success());
    assert_eq!(ids(&srv, &["--all"]), [a, b, c]);
}

/// A workspace root with canonical clones `proj`, `one` (with AGENTS.md, a
/// skill and docs) and `two`, each with an origin and `origin/HEAD`.
struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Fixture {
    fn new(clones: &[&str]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        for name in clones {
            let canonical = if *name == "two" {
                // A repo outside the workspace root, found through `[repos.two]`.
                base.join("elsewhere/two")
            } else {
                ws.join(name)
            };
            let origin = base.join(format!("{name}.git"));
            std::fs::create_dir_all(&canonical).unwrap();
            git(
                &base,
                &[
                    "init",
                    "-q",
                    "--bare",
                    "-b",
                    "main",
                    origin.to_str().unwrap(),
                ],
            );
            git(&canonical, &["init", "-q", "-b", "main"]);
            std::fs::write(canonical.join("README.md"), format!("{name}\n")).unwrap();
            if *name == "one" {
                std::fs::write(canonical.join("AGENTS.md"), "rules\n").unwrap();
                let skill = canonical.join(".agents/skills/build");
                std::fs::create_dir_all(&skill).unwrap();
                std::fs::write(
                    skill.join("SKILL.md"),
                    "---\nname: build\ndescription: Build the thing\n---\nbody\n",
                )
                .unwrap();
                std::fs::create_dir_all(canonical.join("docs/guide")).unwrap();
                std::fs::write(canonical.join("docs/index.md"), "# One docs\n").unwrap();
                std::fs::write(canonical.join("docs/guide/start.md"), "x\n# Start here\n").unwrap();
            }
            git(&canonical, &["add", "."]);
            git(&canonical, &["commit", "-qm", "base"]);
            git(
                &canonical,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            git(&canonical, &["push", "-q", "origin", "main"]);
            git(&canonical, &["fetch", "-q", "origin"]);
            git(&canonical, &["remote", "set-head", "origin", "main"]);
        }
        let script = base.join("agent.sh");
        std::fs::write(
            &script,
            r#"set -e
{ echo "pwd=$PWD"; echo "worktree=$AXE_WORKTREE"; } > "$RECORD/$AXE_TASK"
"$AXE_BIN" set "$AXE_TASK" --state done
"#,
        )
        .unwrap();
        std::fs::create_dir_all(base.join("record")).unwrap();
        let config = base.join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"
workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[defaults]
agents = ["rec"]
[agents.rec]
cmd = ["sh", "{script}"]
session = "none"
[worktrees]
[repos.two]
path = "{two}"
"#,
                ws = ws.display(),
                logs = base.join("logs").display(),
                script = script.display(),
                two = base.join("elsewhere/two").display(),
            ),
        )
        .unwrap();
        Fixture { dir, config }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Adds tasks through the CLI, then runs one dispatcher pass per task.
    fn run(&self, tasks: &[&[&str]]) -> (Server, Vec<String>) {
        self.run_with(|_| {}, tasks)
    }

    /// [`Fixture::run`] after `setup` (e.g. docs to attach) on the server.
    fn run_with(&self, setup: impl FnOnce(&Server), tasks: &[&[&str]]) -> (Server, Vec<String>) {
        let base = self.base();
        let srv = Server::start_with(&base, &self.config, Some(TOKEN), &[], &[]);
        setup(&srv);
        let mut ids = Vec::new();
        for args in tasks {
            let mut a = vec!["add", "--title", "t", "--detail", "Outcome: x"];
            a.extend_from_slice(args);
            ids.push(srv.ok(&a).trim().to_string());
        }
        let record = base.join("record");
        let env = [("RECORD", record.to_str().unwrap())];
        for _ in tasks {
            let out = srv.dispatcher(&["--once"], &env).wait();
            assert!(out.status.success(), "{out:?}");
        }
        (srv, ids)
    }

    fn record(&self, id: &str) -> String {
        std::fs::read_to_string(self.base().join(format!("record/{id}"))).unwrap_or_default()
    }

    fn run_dir(&self, id: &str) -> PathBuf {
        self.base().join(format!("ws/.runs/task-{id}"))
    }
}

#[test]
fn two_repo_task_gets_both_worktrees_on_its_branch() {
    let f = Fixture::new(&["proj", "one", "two"]);
    let (srv, ids) = f.run(&[&["--project", "proj", "--repo", "one,two", "--owner", "bot:b"]]);
    let id = &ids[0];
    assert_eq!(show(&srv, id)["state"], "done", "{}", srv.ok(&["log", id]));
    let run = f.run_dir(id);
    let rec = f.record(id);
    assert!(rec.contains(&format!("pwd={}\n", run.display())), "{rec}");
    assert!(
        rec.contains(&format!("worktree={}\n", run.display())),
        "{rec}"
    );
    let branch = format!("axe/task-{id}--work");
    for (repo, canonical) in [
        ("one", f.base().join("ws/one")),
        ("two", f.base().join("elsewhere/two")),
    ] {
        let wt = run.join("repos").join(repo);
        assert_eq!(git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]), branch);
        assert_eq!(
            git(
                &wt,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            ),
            canonical.join(".git").display().to_string()
        );
    }
    // The project checkout is not used.
    assert!(!f
        .base()
        .join(format!("ws/.worktrees/proj/task-{id}--work"))
        .exists());
    let agents = std::fs::read_to_string(run.join("AGENTS.md")).unwrap();
    for want in [
        &format!("# Task {id}: t\n"),
        "- Owner: bot:b\n",
        "- Labels: proj\n",
        &format!("- Repos: one, two (each a worktree under `repos/`, on branch `{branch}`)"),
        "Outcome: x",
        "- `repos/one`: its rules are in `repos/one/AGENTS.md`\n- `repos/two`\n",
        "- `skills/build` (one): Build the thing\n",
    ] {
        assert!(agents.contains(want), "{want:?} in {agents}");
    }
    assert_eq!(
        std::fs::read_to_string(run.join("skills/build/SKILL.md")).unwrap(),
        "---\nname: build\ndescription: Build the thing\n---\nbody\n"
    );
    assert!(run.join(".claude/skills/build/SKILL.md").is_file());
    assert!(run.join(".agents/skills/build/SKILL.md").is_file());
    let docs = std::fs::read_to_string(run.join("docs/index.md")).unwrap();
    assert!(docs.contains("## one\n\n- [Start here](../repos/one/docs/guide/start.md)\n- [One docs](../repos/one/docs/index.md)\n"), "{docs}");
    assert!(!docs.contains("## two"), "{docs}");
    // Nothing attached: no attached entries anywhere.
    assert!(!docs.contains("## Attached"), "{docs}");
    assert!(!agents.contains("attached"), "{agents}");
    let skills: Vec<_> = std::fs::read_dir(run.join("skills"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(skills, ["build"]);
}

/// Puts a doc (stdin) or a folder (a temp dir of `files`) and attaches it.
fn attach(srv: &Server, path: &str, files: &[(&str, &str)], scopes: &[&str]) {
    use std::io::Write;
    if let [("", text)] = files {
        let mut child = srv
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
    } else {
        let dir = tempfile::tempdir().unwrap();
        for (f, text) in files {
            std::fs::write(dir.path().join(f), text).unwrap();
        }
        srv.ok(&["doc", "put", path, dir.path().to_str().unwrap()]);
    }
    for s in scopes {
        srv.ok(&["doc", "attach", path, s]);
    }
}

#[test]
fn run_workspace_indexes_attached_docs_by_precedence() {
    let f = Fixture::new(&["proj", "one"]);
    let skill = |d: &str| format!("---\nname: x\ndescription: {d}\n---\nSECRET body of {d}\n");
    let (g, l, o, r, b) = (
        skill("global review"),
        skill("label review"),
        skill("owner review"),
        skill("repo review"),
        skill("attached build"),
    );
    let (srv, ids) = f.run_with(
        |srv| {
            attach(srv, "g/review", &[("SKILL.md", &g)], &["global"]);
            attach(srv, "l/review", &[("SKILL.md", &l)], &["label:z"]);
            attach(srv, "o/review", &[("SKILL.md", &o)], &["owner:bot:b"]);
            attach(srv, "r/review", &[("SKILL.md", &r)], &["repo:one"]);
            // Same name as repo one's own skill: the repo's wins.
            attach(srv, "o/build", &[("SKILL.md", &b)], &["owner:bot:b"]);
            attach(
                srv,
                "notes/plan.md",
                &[("", "# The plan\nSECRET plan body\n")],
                &["label:proj", "global"],
            );
            attach(
                srv,
                "notes/other.md",
                &[("", "# Other\n")],
                &["label:elsewhere"],
            );
        },
        &[&[
            "--project",
            "proj",
            "--repo",
            "one",
            "--owner",
            "bot:b",
            "--label",
            "z",
        ]],
    );
    let id = &ids[0];
    assert_eq!(show(&srv, id)["state"], "done", "{}", srv.ok(&["log", id]));
    let run = f.run_dir(id);

    // The repo's own skill is untouched; the winning attached skill is an
    // index entry (name and description, no body) where Claude Code looks.
    assert_eq!(
        std::fs::read_to_string(run.join("skills/build/SKILL.md")).unwrap(),
        "---\nname: build\ndescription: Build the thing\n---\nbody\n"
    );
    let stub = std::fs::read_to_string(run.join(".claude/skills/review/SKILL.md")).unwrap();
    assert!(
        stub.starts_with("---\nname: \"review\"\ndescription: \"repo review\"\n---\n"),
        "{stub}"
    );
    assert!(stub.contains("`axe doc get r/review`"), "{stub}");
    let agents = std::fs::read_to_string(run.join("AGENTS.md")).unwrap();
    assert!(agents.contains("- `skills/build` (one): Build the thing\n- `skills/review` (repo:one, attached): repo review\n"), "{agents}");
    assert!(
        agents.contains(&format!("`axe context explain {id}`")),
        "{agents}"
    );
    let docs = std::fs::read_to_string(run.join("docs/index.md")).unwrap();
    assert!(
        docs.contains("- doc `notes/plan.md` (label:proj): The plan\n"),
        "{docs}"
    );
    assert!(!docs.contains("other.md"), "{docs}");
    for text in [&stub, &agents, &docs] {
        assert!(!text.contains("SECRET"), "{text}");
    }
    // The body loads on use.
    assert!(srv
        .ok(&["doc", "get", "r/review"])
        .contains("SECRET body of repo review"));

    // explain lists every candidate with its scope and why.
    let out = srv
        .cli(&["context", "explain", id])
        .env("AXE_CONFIG", &f.config)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<String> = text
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().take(4).collect::<Vec<_>>().join(" "))
        .collect();
    assert_eq!(
        rows,
        [
            "included skill repos/one/.agents/skills/build repo:one",
            "included skill r/review repo:one",
            "included doc notes/plan.md label:proj",
            "skipped skill o/build owner:bot:b",
            "skipped skill o/review owner:bot:b",
            "skipped skill l/review label:z",
            "skipped skill g/review global",
            "skipped doc notes/plan.md global",
        ],
        "{text}"
    );
    assert!(
        text.starts_with(
            "scopes (most specific first): repo:one, owner:bot:b, label:proj, label:z, global\n"
        ),
        "{text}"
    );
    assert!(text.contains("shadowed by repos/one/.agents/skills/build from repo:one: a repo's own skills are not overridable"), "{text}");
    let json: Value = serde_json::from_str(&srv.ok(&["context", "explain", id, "--json"])).unwrap();
    assert_eq!(json["items"][2]["reason"], "attached to label:proj");
}

#[test]
fn zero_repo_task_gets_a_bare_run_dir() {
    let f = Fixture::new(&["proj"]);
    let (srv, ids) = f.run(&[&["--project", "proj", "--no-repos"]]);
    let id = &ids[0];
    assert_eq!(show(&srv, id)["state"], "done", "{}", srv.ok(&["log", id]));
    let run = f.run_dir(id);
    assert!(f.record(id).contains(&format!("pwd={}\n", run.display())));
    assert!(!run.join("repos").exists());
    assert!(!run.join(".git").exists());
    let agents = std::fs::read_to_string(run.join("AGENTS.md")).unwrap();
    assert!(agents.contains("- Repos: none\n"), "{agents}");
    assert!(agents.contains("This task has no repos.\n"), "{agents}");
    assert!(run.join("skills").is_dir() && run.join("docs/index.md").is_file());
}

#[test]
fn project_only_task_runs_unchanged_beside_a_repo_task() {
    // `bare` has no clone: its project-only task escalates before any claim as
    // before, while its task with repos is claimed and runs.
    let f = Fixture::new(&["proj", "one"]);
    let (srv, ids) = f.run(&[
        &["--project", "proj"],
        &["--project", "bare", "--repo", "one"],
        &["--project", "bare"],
    ]);
    let wt = f
        .base()
        .join(format!("ws/.worktrees/proj/task-{}--work", ids[0]));
    assert_eq!(show(&srv, &ids[0])["state"], "done");
    let rec = f.record(&ids[0]);
    assert!(rec.contains(&format!("pwd={}\n", wt.display())), "{rec}");
    assert!(
        rec.contains(&format!("worktree={}\n", wt.display())),
        "{rec}"
    );
    assert!(!f.run_dir(&ids[0]).exists());
    assert_eq!(
        show(&srv, &ids[1])["state"],
        "done",
        "{}",
        srv.ok(&["log", &ids[1]])
    );
    assert!(f.run_dir(&ids[1]).join("repos/one").is_dir());
    let held = show(&srv, &ids[2]);
    assert_eq!(
        (held["state"].as_str(), held["runs"].as_i64()),
        (Some("escalated"), Some(0))
    );
}

#[test]
fn missing_repo_escalates_the_claimed_task() {
    let f = Fixture::new(&["proj"]);
    let (srv, ids) = f.run(&[&["--project", "proj", "--repo", "ghost"]]);
    let t = show(&srv, &ids[0]);
    assert_eq!(t["state"], "escalated", "{t}");
    let log = srv.ok(&["log", &ids[0], "--full"]);
    assert!(log.contains("repo ghost has no worktree base"), "{log}");
}
