//! Where a task without repos runs: its run workspace, with no project
//! checkout needed; a directory an older node left in the project's
//! worktree root is resumed in place. Also the clone URL of a run
//! workspace's repos.
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use axe::db::{self, Actor, NewTask};
use common::{Server, TOKEN};

const PROJECT: &str = "proj";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

struct Fixture {
    dir: tempfile::TempDir,
    ws: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // The first run records its environment, leaves a file and yields back
        // to pending; a later run records and finishes.
        let script = base.join("agent.sh");
        std::fs::write(
            &script,
            r#"set -e
first="$RECORD/$AXE_TASK-1"
if [ -e "$first" ]; then out="$RECORD/$AXE_TASK-2"; else out="$first"; fi
{ echo "actor=$AXE_ACTOR"; echo "pwd=$PWD"; echo "worktree=$AXE_WORKTREE"; } > "$out"
if [ "$out" = "$first" ]; then
  echo uncommitted > "$AXE_WORKTREE/local"
  "$AXE_BIN" set "$AXE_TASK" --state pending
else
  "$AXE_BIN" set "$AXE_TASK" --state done
fi
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
"#,
                ws = ws.display(),
                logs = base.join("logs").display(),
                script = script.display(),
            ),
        )
        .unwrap();
        Fixture { dir, ws, config }
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn add(&self) -> i64 {
        let mut conn = db::open(&self.base().join("t.db")).unwrap();
        db::add_task(
            &mut conn,
            &Actor::new("human"),
            &NewTask {
                agent: None,
                project: PROJECT,
                title: "t",
                detail: "Outcome: x",
                state: "pending",
            },
        )
        .unwrap()
    }

    fn dispatch_once(&self) {
        let base = self.base();
        let record = base.join("record");
        let env = [("RECORD", record.to_str().unwrap())];
        let srv = Server::start_with(&base, &self.config, Some(TOKEN), &[], &[]);
        let out = srv.dispatcher(&["--once"], &env).wait();
        assert!(out.status.success(), "{out:?}");
    }

    fn task(&self, id: i64) -> db::Task {
        db::get_task(&db::open(&self.base().join("t.db")).unwrap(), id).unwrap()
    }

    fn events(&self, id: i64) -> Vec<db::Event> {
        db::events(&db::open(&self.base().join("t.db")).unwrap(), id).unwrap()
    }

    /// What run `n` (1 or 2) of task `id` recorded.
    fn record(&self, id: i64, n: u32) -> String {
        std::fs::read_to_string(self.base().join(format!("record/{id}-{n}"))).unwrap()
    }

    /// Runs task `id` twice: it yields after the first run, finishes after
    /// the second. Both runs must have worked in `dir`.
    fn runs_twice_in(&self, id: i64, dir: &Path) {
        self.dispatch_once();
        assert_eq!(self.task(id).state, "pending", "{:?}", self.events(id));
        self.dispatch_once();
        assert_eq!(self.task(id).state, "done", "{:?}", self.events(id));
        for n in [1, 2] {
            let rec = self.record(id, n);
            assert!(rec.starts_with("actor=agent\n"), "{rec}");
            assert!(rec.contains(&format!("pwd={}\n", dir.display())), "{rec}");
            assert!(
                rec.contains(&format!("worktree={}\n", dir.display())),
                "{rec}"
            );
        }
        // The second run found the first one's file untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("local")).unwrap(),
            "uncommitted\n"
        );
    }
}

#[test]
fn task_without_repos_runs_in_its_run_workspace_with_no_project_checkout() {
    let f = Fixture::new();
    let id = f.add();
    f.runs_twice_in(id, &f.ws.join(format!(".runs/task-{id}")));
    assert!(!f.ws.join(PROJECT).exists());
    assert!(!f.ws.join(".worktrees").exists());
    assert!(!f.events(id).iter().any(|e| e
        .body
        .as_deref()
        .unwrap_or("")
        .contains("worktree base")));
}

#[test]
fn a_project_checkout_without_origin_head_is_not_needed() {
    // The project's own clone has no origin at all; nothing reads it.
    let f = Fixture::new();
    let canonical = f.ws.join(PROJECT);
    std::fs::create_dir_all(&canonical).unwrap();
    git(&canonical, &["init", "-q", "-b", "main"]);
    let id = f.add();
    f.runs_twice_in(id, &f.ws.join(format!(".runs/task-{id}")));
    assert!(!f.ws.join(".worktrees").exists());
    assert!(!canonical.join("local").exists());
}

#[test]
fn a_task_left_in_an_old_project_worktree_resumes_there() {
    // An older node ran this repo-less task in the project's worktree
    // root; its work there is not stranded. Nothing is created or reset.
    let f = Fixture::new();
    let id = f.add();
    let old = f.ws.join(format!(".worktrees/{PROJECT}/task-{id}--work"));
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("earlier"), "keep").unwrap();
    f.runs_twice_in(id, &old);
    assert_eq!(
        std::fs::read_to_string(old.join("earlier")).unwrap(),
        "keep"
    );
    assert!(!f.ws.join(format!(".runs/task-{id}")).exists());
}

#[test]
fn clone_url_comes_only_from_the_registry_and_names_the_gap() {
    use axe::worktree::{clone_url, repo_readiness_with, CloneUrl};
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("ws/here")).unwrap();
    let cfg = axe::config::Config::parse(&format!(
        r#"
workspace_root = "{ws}"
log_dir = "{logs}"
[agents.a]
cmd = ["true"]
[repos.local]
url = "https://example.com/local.git"
"#,
        ws = base.join("ws").display(),
        logs = base.join("logs").display(),
    ))
    .unwrap();
    let asked = std::cell::Cell::new(0);
    let server = |name: &str| -> anyhow::Result<Option<String>> {
        asked.set(asked.get() + 1);
        Ok(match name {
            "local" | "remote" => Some(format!("https://server.example/{name}.git")),
            "bad" => Some("https://u:p@server.example/bad.git".into()),
            _ => None,
        })
    };
    // A retired config url is never used: the server's URL is.
    assert_eq!(
        clone_url(&cfg, "local", &server),
        CloneUrl::Url("https://server.example/local.git".into())
    );
    assert_eq!(asked.get(), 1);
    // An existing canonical clone needs no URL and asks nothing.
    assert_eq!(clone_url(&cfg, "here", &server), CloneUrl::None);
    assert_eq!(asked.get(), 1);
    assert_eq!(
        clone_url(&cfg, "remote", &server),
        CloneUrl::Url("https://server.example/remote.git".into())
    );
    assert_eq!(asked.get(), 2);
    // A registry URL with credentials is refused.
    assert!(matches!(
        clone_url(&cfg, "bad", &server),
        CloneUrl::Unknown(_)
    ));
    // Not registered: the gap names only `axe repo add`.
    let none = clone_url(&cfg, "ghost", &server);
    assert_eq!(none, CloneUrl::None);
    let gap = repo_readiness_with(&cfg, "ghost", &none).unwrap_err();
    assert!(
        gap.starts_with("repo ghost has no worktree base: ")
            && gap.contains("the server's repo registry has no clone URL for it: register one with `axe repo add ghost <url>`")
            && !gap.contains("[repos.")
            && !gap.contains("config"),
        "{gap}"
    );
    // A retired config url does not fill the gap either.
    let local = clone_url(&cfg, "local", &|_| Ok(None));
    assert_eq!(local, CloneUrl::None);
    assert!(repo_readiness_with(&cfg, "local", &local)
        .unwrap_err()
        .contains("`axe repo add local <url>`"));
    // An unreachable server: the gap names that cause.
    let down = |_: &str| -> anyhow::Result<Option<String>> {
        anyhow::bail!("cannot reach axe server at http://127.0.0.1:9: connection refused")
    };
    let unknown = clone_url(&cfg, "ghost", &down);
    let gap = repo_readiness_with(&cfg, "ghost", &unknown).unwrap_err();
    assert!(
        gap.contains("the server's repo registry could not be read for its clone URL: cannot reach axe server"),
        "{gap}"
    );
}
