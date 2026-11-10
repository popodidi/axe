//! Dispatcher-owned task worktrees: readiness before claim, allocation, reuse
//! and the session cwd/env.
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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

struct Fixture {
    dir: tempfile::TempDir,
    canonical: PathBuf,
    root: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        let canonical = ws.join(PROJECT);
        let origin = base.join("origin.git");
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
        std::fs::write(canonical.join("AGENTS.md"), "x\n").unwrap();
        git(&canonical, &["add", "."]);
        git(&canonical, &["commit", "-qm", "base"]);
        git(
            &canonical,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&canonical, &["push", "-q", "origin", "main"]);
        git(&canonical, &["fetch", "-q", "origin"]);
        git(&canonical, &["remote", "set-head", "origin", "main"]);
        // The first run records its environment, commits, leaves a local file and
        // yields back to pending; a later run records and finishes.
        let script = base.join("agent.sh");
        std::fs::write(
            &script,
            r#"set -e
first="$RECORD/$AXE_TASK-1"
if [ -e "$first" ]; then out="$RECORD/$AXE_TASK-2"; else out="$first"; fi
{ echo "actor=$AXE_ACTOR"; echo "pwd=$PWD"; echo "worktree=$AXE_WORKTREE"; } > "$out"
if [ "$out" = "$first" ]; then
  echo change > "$AXE_WORKTREE/change"
  git -C "$AXE_WORKTREE" add change
  git -C "$AXE_WORKTREE" -c user.name=t -c user.email=t@example.invalid commit -qm change
  echo "head=$(git -C "$AXE_WORKTREE" rev-parse HEAD)" >> "$out"
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
        Fixture {
            dir,
            canonical,
            root: ws.join(".worktrees").join(PROJECT),
            config,
        }
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

    /// What run `n` (1 or 2+) of task `id` recorded.
    fn record(&self, id: i64, n: u32) -> String {
        std::fs::read_to_string(self.base().join(format!("record/{id}-{n}"))).unwrap()
    }

    fn last_note(&self, id: i64) -> String {
        self.events(id)
            .iter()
            .rev()
            .find(|e| e.kind == "note")
            .and_then(|e| e.body.clone())
            .unwrap()
    }

    fn checkout(&self, id: i64) -> PathBuf {
        self.root.join(format!("task-{id}--work"))
    }
}

#[test]
fn sessions_run_in_one_reused_task_worktree() {
    let f = Fixture::new();
    let id = f.add();
    // An empty directory left by an interrupted allocation is completed in place.
    std::fs::create_dir_all(f.checkout(id)).unwrap();
    f.dispatch_once();
    let path = f.checkout(id);
    assert_eq!(f.task(id).state, "pending", "{:?}", f.events(id));
    let built = f.record(id, 1);
    assert!(built.starts_with("actor=agent\n"), "{built}");
    assert!(
        built.contains(&format!("pwd={}\n", path.display())),
        "{built}"
    );
    assert!(
        built.contains(&format!("worktree={}\n", path.display())),
        "{built}"
    );
    let head = built.lines().find_map(|l| l.strip_prefix("head=")).unwrap();
    assert_eq!(
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        format!("axe/task-{id}--work")
    );
    // The next run (and any retry) reuses the same checkout unchanged.
    f.dispatch_once();
    assert_eq!(f.task(id).state, "done", "{:?}", f.events(id));
    let verified = f.record(id, 2);
    assert!(
        verified.contains(&format!("pwd={}\n", path.display())),
        "{verified}"
    );
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&path, &["status", "--porcelain"]), "?? local");
}

#[test]
fn reopened_task_resumes_its_branch_after_cleanup() {
    let f = Fixture::new();
    let id = f.add();
    for _ in 0..3 {
        if f.task(id).state == "done" {
            break;
        }
        f.dispatch_once();
    }
    assert_eq!(f.task(id).state, "done", "{:?}", f.events(id));
    let head = git(&f.checkout(id), &["rev-parse", "HEAD"]);
    // Housekeeper deletes the directory; the branch stays.
    std::fs::remove_dir_all(f.checkout(id)).unwrap();
    git(&f.canonical, &["worktree", "prune"]);
    let mut conn = db::open(&f.base().join("t.db")).unwrap();
    db::set_task(
        &mut conn,
        &Actor::new("human"),
        id,
        &db::Changes {
            state: Some("pending".into()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(conn);
    f.dispatch_once();
    assert_eq!(f.task(id).state, "done", "{:?}", f.events(id));
    assert_eq!(git(&f.checkout(id), &["rev-parse", "HEAD"]), head);
}

#[test]
fn missing_default_branch_escalates_without_claim_or_run() {
    let f = Fixture::new();
    git(&f.canonical, &["remote", "set-head", "origin", "--delete"]);
    let id = f.add();
    f.dispatch_once();
    let t = f.task(id);
    assert_eq!((t.state.as_str(), t.runs), ("escalated", 0));
    assert!(!f
        .events(id)
        .iter()
        .any(|e| e.kind == "claimed" || e.kind == "run_start"));
    let note = f.last_note(id);
    assert!(
        note.starts_with(&format!(
            "blocked: project {PROJECT} has no task worktree base: "
        )),
        "{note}"
    );
    assert!(note.contains("refs/remotes/origin/HEAD"), "{note}");
    assert!(
        note.contains(&format!("resume: axe set {id} --state pending")),
        "{note}"
    );
    assert!(!f.checkout(id).exists());
}

#[test]
fn foreign_directory_is_never_adopted_or_cleaned() {
    let f = Fixture::new();
    let id = f.add();
    let path = f.checkout(id);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("partial"), "keep").unwrap();
    f.dispatch_once();
    assert_eq!(f.task(id).state, "escalated");
    assert!(
        f.last_note(id).contains("not a worktree of"),
        "{}",
        f.last_note(id)
    );
    assert_eq!(
        std::fs::read_to_string(path.join("partial")).unwrap(),
        "keep"
    );
    assert!(!f.base().join(format!("record/{id}-1")).exists());
}
