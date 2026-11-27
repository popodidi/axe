//! `axe workspace`: a task's run folder built locally, and `sync` writing
//! store-backed edits back.
mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::Server;
use serde_json::Value;

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

/// A laptop: clones `one` (with a skill) and `two` under `ws/`, each with an
/// origin; a config naming them; a home for the default folder.
struct Laptop {
    _dir: tempfile::TempDir,
    base: PathBuf,
    srv: Server,
    config: PathBuf,
}

impl Laptop {
    fn new() -> Laptop {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let ws = base.join("ws");
        for name in ["one", "two"] {
            let canonical = ws.join(name);
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
            if name == "one" {
                let skill = canonical.join(".agents/skills/build");
                std::fs::create_dir_all(&skill).unwrap();
                std::fs::write(
                    skill.join("SKILL.md"),
                    "---\nname: build\ndescription: Build the thing\n---\nbody\n",
                )
                .unwrap();
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
        let server_cfg = base.join("server.toml");
        std::fs::write(&server_cfg, "").unwrap();
        let srv = Server::start_with(&base, &server_cfg, Some(common::TOKEN), &[], &[]);
        let config = base.join("laptop.toml");
        std::fs::write(
            &config,
            format!(
                "workspace_root = \"{}\"\nlog_dir = \"{}\"\n",
                ws.display(),
                base.join("logs").display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(base.join("home")).unwrap();
        Laptop {
            _dir: dir,
            base,
            srv,
            config,
        }
    }

    fn axe(&self, args: &[&str]) -> Output {
        self.srv
            .cli(args)
            .env("AXE_CONFIG", &self.config)
            .env("HOME", self.base.join("home"))
            .current_dir(&self.base)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.axe(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// Puts a doc or a folder of `files` and attaches it to `scope`.
    fn attach(&self, path: &str, files: &[(&str, &str)], scope: &str) {
        let src = tempfile::tempdir().unwrap();
        let arg = if let [("", text)] = files {
            let p = src.path().join("doc.md");
            std::fs::write(&p, text).unwrap();
            p
        } else {
            for (f, text) in files {
                let p = src.path().join(f);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
            src.path().to_path_buf()
        };
        self.ok(&["doc", "put", path, arg.to_str().unwrap()]);
        self.ok(&["doc", "attach", path, scope]);
    }

    fn versions(&self, path: &str) -> usize {
        let v: Value = serde_json::from_str(&self.ok(&["doc", "log", path, "--json"])).unwrap();
        v.as_array().unwrap().len()
    }

    fn task(&self, repos: &str) -> String {
        self.ok(&[
            "add",
            "--project",
            "proj",
            "--title",
            "t",
            "--detail",
            "Outcome: x",
            "--repo",
            repos,
        ])
        .trim()
        .to_string()
    }
}

const SKILL: &str = "---\nname: review\ndescription: Review it\n---\nbody\n";

#[test]
fn task_workspace_has_both_worktrees_on_the_task_branch() {
    let l = Laptop::new();
    l.attach(
        "s/review",
        &[
            ("SKILL.md", SKILL),
            ("scripts/check.sh", "#!/bin/sh\necho ok\n"),
        ],
        "label:proj",
    );
    l.attach("notes/plan.md", &[("", "# The plan\nbody\n")], "repo:two");
    let id = l.task("one,two");
    let branch = format!("axe/task-{id}--work");
    // A run pushed the task branch of repo one from another machine.
    let other = l.base.join("other");
    git(
        &l.base,
        &[
            "clone",
            "-q",
            l.base.join("one.git").to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    git(&other, &["checkout", "-qb", &branch]);
    std::fs::write(other.join("work.txt"), "pushed\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-qm", "work"]);
    git(&other, &["push", "-q", "origin", &branch]);
    let pushed = git(&other, &["rev-parse", "HEAD"]);

    let out = l.ok(&["workspace", &id]);
    let dir = l.base.join(format!("home/axe/task-{id}"));
    assert_eq!(out.trim(), dir.display().to_string());
    for repo in ["one", "two"] {
        let wt = dir.join("repos").join(repo);
        assert_eq!(
            git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
            branch,
            "{repo}"
        );
    }
    // Pushed: it starts there and tracks it. Not pushed: from the base.
    assert_eq!(git(&dir.join("repos/one"), &["rev-parse", "HEAD"]), pushed);
    assert_eq!(
        git(
            &dir.join("repos/one"),
            &["rev-parse", "--abbrev-ref", "@{u}"]
        ),
        format!("origin/{branch}")
    );
    assert_eq!(
        git(&dir.join("repos/two"), &["rev-parse", "HEAD"]),
        git(&l.base.join("ws/two"), &["rev-parse", "origin/main"])
    );

    let agents = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
    for want in [
        &format!("# Task {id}: t\n\nGenerated by `axe workspace`; edits here are lost when it rebuilds.\n"),
        &format!("- Repos: one, two (each a worktree under `repos/`, on branch `{branch}`)"),
        "Outcome: x",
        "- `skills/build` (one): Build the thing\n- `skills/review` (label:proj, attached): Review it\n",
        "attached docs under `docs/store/`; after editing them, `axe workspace sync`",
    ] {
        assert!(agents.contains(want), "{want:?} in {agents}");
    }
    assert_eq!(
        std::fs::read_link(dir.join("skills/build")).unwrap(),
        Path::new("../repos/one/.agents/skills/build")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".claude/skills/review/SKILL.md")).unwrap(),
        SKILL
    );
    assert!(dir.join(".agents/skills/review/scripts/check.sh").is_file());
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/store/notes/plan.md")).unwrap(),
        "# The plan\nbody\n"
    );
    let docs = std::fs::read_to_string(dir.join("docs/index.md")).unwrap();
    assert!(
        docs.contains("- doc [`notes/plan.md`](store/notes/plan.md) (repo:two): The plan\n"),
        "{docs}"
    );
    let m: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(".axe/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        m["target"],
        serde_json::json!({"task": id.parse::<i64>().unwrap()})
    );
    let locals: Vec<&str> = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["local"].as_str().unwrap())
        .collect();
    assert_eq!(locals, ["skills/review", "docs/store/notes/plan.md"]);

    // Nothing claimed, no run recorded.
    let t: Value = serde_json::from_str(&l.ok(&["show", &id, "--json"])).unwrap();
    assert_eq!(
        (t["state"].as_str(), t["runs"].as_i64()),
        (Some("pending"), Some(0))
    );

    // A rebuild keeps worktree changes and a commit made here.
    std::fs::write(dir.join("repos/two/local.txt"), "keep\n").unwrap();
    l.ok(&["workspace", &id]);
    assert!(dir.join("repos/two/local.txt").is_file());
}

#[test]
fn in_progress_task_is_refused() {
    let l = Laptop::new();
    let id = l.task("one");
    l.ok(&["set", &id, "--state", "in_progress"]);
    let out = l.axe(&["workspace", &id]);
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("in_progress") && err.contains("Hold it first"),
        "{err}"
    );
    assert!(!l.base.join(format!("home/axe/task-{id}")).exists());

    // Built while held, then picked up: sync is refused too.
    l.ok(&["set", &id, "--state", "blocked"]);
    let dir = l.base.join("w");
    l.ok(&["workspace", &id, "--dir", dir.to_str().unwrap()]);
    l.ok(&["set", &id, "--state", "in_progress"]);
    let out = l.axe(&["workspace", "sync", dir.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("in_progress"));
}

#[test]
fn sync_writes_edits_back_once_and_a_conflict_keeps_the_local_edit() {
    let l = Laptop::new();
    l.attach(
        "s/review",
        &[
            ("SKILL.md", SKILL),
            ("scripts/check.sh", "#!/bin/sh\necho ok\n"),
        ],
        "label:proj",
    );
    l.attach("notes/plan.md", &[("", "# The plan\nbody\n")], "label:proj");
    let id = l.task("one");
    let dir = l.base.join("w");
    l.ok(&["workspace", &id, "--dir", dir.to_str().unwrap()]);
    let plan = dir.join("docs/store/notes/plan.md");
    let skill = dir.join("skills/review/SKILL.md");

    // No edits: nothing written.
    l.ok(&["workspace", "sync", dir.to_str().unwrap()]);
    assert_eq!(
        (l.versions("s/review"), l.versions("notes/plan.md")),
        (1, 1)
    );

    // Edit both, and a generated file; run sync from inside the folder.
    std::fs::write(&skill, format!("{SKILL}more\n")).unwrap();
    std::fs::write(&plan, "# The plan\nbody v2\n").unwrap();
    std::fs::write(dir.join("AGENTS.md"), "mine\n").unwrap();
    let out = l
        .srv
        .cli(&["workspace", "sync"])
        .env("AXE_CONFIG", &l.config)
        .current_dir(dir.join("repos/one"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("  s/review  written") && stdout.contains("  notes/plan.md  written"),
        "{stdout}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("AGENTS.md is generated"),
        "{out:?}"
    );
    assert_eq!(
        (l.versions("s/review"), l.versions("notes/plan.md")),
        (2, 2)
    );
    assert!(l.ok(&["doc", "get", "s/review"]).contains("more"));
    assert!(l.ok(&["doc", "get", "s/review"]).contains("echo ok"));
    assert_eq!(
        l.ok(&["doc", "get", "notes/plan.md"]),
        "# The plan\nbody v2\n"
    );
    // Rebuilt: AGENTS.md is generated again.
    assert!(std::fs::read_to_string(dir.join("AGENTS.md"))
        .unwrap()
        .starts_with("# Task"));

    // Twice with no edits: nothing written.
    l.ok(&["workspace", "sync", dir.to_str().unwrap()]);
    assert_eq!(
        (l.versions("s/review"), l.versions("notes/plan.md")),
        (2, 2)
    );

    // The store moves on while the doc is edited here: a conflict.
    let theirs = l.base.join("theirs.md");
    std::fs::write(&theirs, "# The plan\ntheir change\n").unwrap();
    l.ok(&["doc", "put", "notes/plan.md", theirs.to_str().unwrap()]);
    std::fs::write(&plan, "# The plan\nmy change\n").unwrap();
    let out = l.axe(&["workspace", "sync", dir.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("conflict: notes/plan.md: your edit is based on"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# The plan\nmy change\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("docs/store/notes/plan.md.store")).unwrap(),
        "# The plan\ntheir change\n"
    );
    assert_eq!(l.versions("notes/plan.md"), 3);
    assert_eq!(
        l.ok(&["doc", "get", "notes/plan.md"]),
        "# The plan\ntheir change\n"
    );
    // Open: neither a rebuild nor another sync touches it.
    let out = l.axe(&["workspace", &id, "--dir", dir.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("conflict open: docs/store/notes/plan.md")
    );
    assert!(!l
        .axe(&["workspace", "sync", dir.to_str().unwrap()])
        .status
        .success());
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# The plan\nmy change\n"
    );

    // Merged and the copy deleted: written on top of their version.
    std::fs::write(&plan, "# The plan\ntheir change\nmy change\n").unwrap();
    std::fs::remove_file(dir.join("docs/store/notes/plan.md.store")).unwrap();
    l.ok(&["workspace", "sync", dir.to_str().unwrap()]);
    assert_eq!(l.versions("notes/plan.md"), 4);
    assert_eq!(
        l.ok(&["doc", "get", "notes/plan.md"]),
        "# The plan\ntheir change\nmy change\n"
    );

    // A new file is never sent, and keeps the folder from being rebuilt.
    std::fs::write(dir.join("docs/store/notes/new.md"), "new\n").unwrap();
    let out = l.axe(&["workspace", "sync", dir.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("docs/store/notes/new.md is not in the store"),
        "{err}"
    );
    assert!(err.contains("not rebuilt"), "{err}");
    assert!(dir.join("docs/store/notes/new.md").is_file());
    assert!(!l.ok(&["doc", "ls"]).contains("notes/new.md"));
}

#[test]
fn a_folder_that_is_not_a_workspace_is_never_replaced() {
    let l = Laptop::new();
    let id = l.task("one");
    let dir = l.base.join("mine");
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/notes.md"), "mine\n").unwrap();
    let out = l.axe(&["workspace", &id, "--dir", dir.to_str().unwrap()]);
    assert!(!out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not an axe workspace"));
    assert!(dir.join("docs/notes.md").is_file());
}

#[test]
fn a_repo_registered_only_on_the_server_is_cloned_and_built() {
    let l = Laptop::new();
    // `three` exists only as an origin; the laptop config names no repos.
    let origin = l.base.join("three.git");
    let seed = l.base.join("seed-three");
    git(
        &l.base,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "three\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "base"]);
    git(&seed, &["push", "-q", origin.to_str().unwrap(), "main"]);
    let id = l.task("three");

    // Unregistered: the gap says how to register it.
    let out = l.axe(&["workspace", &id]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("repo three has no worktree base")
            && err.contains("`axe repo add three <url>`"),
        "{err}"
    );

    l.ok(&["repo", "add", "three", origin.to_str().unwrap()]);
    let ls = l.ok(&["repo", "ls"]);
    assert_eq!(ls, format!("three\t{}\n", origin.display()));
    let dir = l.base.join("dir-three");
    l.ok(&["workspace", &id, "--dir", dir.to_str().unwrap()]);
    let canonical = l.base.join("ws/three");
    assert_eq!(
        git(&canonical, &["remote", "get-url", "origin"]),
        origin.display().to_string()
    );
    assert_eq!(
        git(
            &dir.join("repos/three"),
            &["rev-parse", "--abbrev-ref", "HEAD"]
        ),
        format!("axe/task-{id}--work")
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("repos/three/README.md")).unwrap(),
        "three\n"
    );
}

#[test]
fn repo_registry_cli_and_api() {
    let l = Laptop::new();
    l.ok(&[
        "repo",
        "add",
        "a",
        "https://example.com/a.git",
        "--default-branch",
        "trunk",
    ]);
    let r: Value = serde_json::from_str(&l.ok(&["repo", "show", "a"])).unwrap();
    assert_eq!(
        (r["url"].as_str(), r["default_branch"].as_str()),
        (Some("https://example.com/a.git"), Some("trunk"))
    );
    let all: Value = serde_json::from_str(&l.ok(&["repo", "ls", "--json"])).unwrap();
    assert_eq!(all.as_array().unwrap().len(), 1);
    // The CLI refuses credentials before sending; so does the server.
    let out = l.axe(&["repo", "add", "b", "https://u:p@example.com/b.git"]);
    assert!(!out.status.success());
    let url = |p: &str| format!("{}{p}", l.srv.url);
    let put = |path: &str, auth: Option<&str>, body: Value| {
        let mut req = ureq::put(&url(path));
        if let Some(t) = auth {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        match req.send_json(body) {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("{e}"),
        }
    };
    let auth = Some(common::TOKEN);
    for bad in ["https://u:p@example.com/b.git", "-uupload-pack=x", ""] {
        assert_eq!(
            put("/api/repos/b", auth, serde_json::json!({"url": bad})),
            400,
            "{bad}"
        );
    }
    // Writes need the token.
    assert_eq!(
        put(
            "/api/repos/b",
            None,
            serde_json::json!({"url": "https://example.com/b.git"})
        ),
        401
    );
    assert!(l.axe(&["repo", "show", "b"]).status.code() != Some(0));
    l.ok(&["repo", "rm", "a"]);
    assert_eq!(
        l.ok(&["repo", "ls"]),
        "no repos registered: add one with `axe repo add <name> <url>`\n"
    );
    assert!(!l.axe(&["repo", "rm", "a"]).status.success());
    match ureq::get(&url("/api/repos/a"))
        .set("Authorization", &format!("Bearer {}", common::TOKEN))
        .call()
    {
        Err(ureq::Error::Status(404, _)) => {}
        other => panic!("{other:?}"),
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn an_unreachable_server_fails_fast_and_writes_nothing() {
    let l = Laptop::new();
    let id = l.task("one");
    let dir = l.base.join("w");
    let d = dir.to_str().unwrap();
    // A name that never resolves (RFC 6761), and a port nothing listens on:
    // no retries, so each ends well inside the 10s DNS and 5s connect bounds.
    for (server, host) in [
        ("http://axe.invalid", "axe.invalid"),
        ("http://127.0.0.1:9", "127.0.0.1:9"),
    ] {
        let start = std::time::Instant::now();
        let out = l
            .srv
            .cli(&["workspace", &id, "--dir", d])
            .env("AXE_CONFIG", &l.config)
            .env("HOME", l.base.join("home"))
            .env("AXE_SERVER", server)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{out:?}");
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
        let err = stderr(&out);
        assert!(
            err.contains(&format!("read task {id} from the axe server at"))
                && err.contains(host)
                && err.contains("nothing was changed"),
            "{err}"
        );
        assert!(!dir.exists());
    }
    // sync too: a built folder, then the server goes away.
    l.ok(&["workspace", &id, "--dir", d]);
    let out = l
        .srv
        .cli(&["workspace", "sync", d])
        .env("AXE_CONFIG", &l.config)
        .env("AXE_SERVER", "http://axe.invalid")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr(&out).contains("axe.invalid"), "{out:?}");
}

#[test]
fn a_build_that_did_not_finish_resumes_and_sync_waits_for_it() {
    let l = Laptop::new();
    l.attach("notes/plan.md", &[("", "# The plan\nbody\n")], "label:proj");
    let id = l.task("one,two");
    let dir = l.base.join("w");
    let d = dir.to_str().unwrap();
    // Repo two's origin is gone: its worktree cannot be fetched.
    let two = l.base.join("ws/two");
    let origin = l.base.join("two.git");
    git(
        &two,
        &["remote", "set-url", "origin", "/nonexistent/two.git"],
    );
    let out = l.axe(&["workspace", &id, "--dir", d]);
    assert!(!out.status.success(), "{out:?}");
    let err = stderr(&out);
    assert!(
        err.contains("did not finish")
            && err.contains(&format!("run `axe workspace {id} --dir {d}` again"))
            && err.contains("set up repos/two"),
        "{err}"
    );
    assert!(dir.join("repos/one").is_dir());
    assert!(!dir.join(".axe/manifest.json").exists());

    // sync there refuses and names the command that finishes the build.
    let out = l.axe(&["workspace", "sync", d]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr(&out).contains(&format!("run `axe workspace {id} --dir {d}` to finish it")),
        "{out:?}"
    );

    // Once the network is back, the same command resumes it.
    git(
        &two,
        &["remote", "set-url", "origin", origin.to_str().unwrap()],
    );
    let out = l.axe(&["workspace", &id, "--dir", d]);
    assert!(out.status.success(), "{out:?}");
    assert!(stderr(&out).contains("resuming it"), "{out:?}");
    assert!(dir.join("repos/two/README.md").is_file());
    assert!(dir.join("docs/store/notes/plan.md").is_file());
    assert!(dir.join(".axe/manifest.json").is_file());
    assert!(!dir.join(".axe/building.json").exists());
    l.ok(&["workspace", "sync", d]);
}

#[test]
fn a_folder_an_older_build_left_needs_force() {
    let l = Laptop::new();
    let id = l.task("one");
    let dir = l.base.join("w");
    let d = dir.to_str().unwrap();
    // What an interrupted build by an older axe left: worktrees and
    // generated files, no manifest and no build marker.
    l.ok(&["workspace", &id, "--dir", d]);
    std::fs::remove_dir_all(dir.join(".axe")).unwrap();
    std::fs::remove_dir_all(dir.join("docs/store")).unwrap_or_default();
    let out = l.axe(&["workspace", &id, "--dir", d]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr(&out).contains("did not finish")
            && stderr(&out).contains(&format!("Run `axe workspace {id} --dir {d} --force`")),
        "{out:?}"
    );
    // A commit on the task branch survives --force.
    std::fs::write(dir.join("repos/one/work.txt"), "w\n").unwrap();
    git(&dir.join("repos/one"), &["add", "."]);
    git(&dir.join("repos/one"), &["commit", "-qm", "work"]);
    let head = git(&dir.join("repos/one"), &["rev-parse", "HEAD"]);
    let out = l.axe(&["workspace", &id, "--dir", d, "--force"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(git(&dir.join("repos/one"), &["rev-parse", "HEAD"]), head);
    assert!(dir.join(".axe/manifest.json").is_file());

    // --force still refuses a folder that is not a workspace.
    let mine = l.base.join("mine");
    std::fs::create_dir_all(mine.join("docs")).unwrap();
    std::fs::write(mine.join("docs/notes.md"), "mine\n").unwrap();
    let out = l.axe(&["workspace", &id, "--dir", mine.to_str().unwrap(), "--force"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr(&out).contains("is not an axe workspace"), "{out:?}");
    assert!(mine.join("docs/notes.md").is_file());
}

#[test]
fn force_refuses_while_a_repo_or_a_store_file_has_changes() {
    let l = Laptop::new();
    l.attach("notes/plan.md", &[("", "# The plan\nbody\n")], "label:proj");
    let id = l.task("one");
    let dir = l.base.join("w");
    let d = dir.to_str().unwrap();
    l.ok(&["workspace", &id, "--dir", d]);
    let plan = dir.join("docs/store/notes/plan.md");
    std::fs::write(&plan, "# The plan\nmine\n").unwrap();
    std::fs::write(dir.join("repos/one/scratch.txt"), "mine\n").unwrap();
    let out = l.axe(&["workspace", &id, "--dir", d, "--force"]);
    assert!(!out.status.success(), "{out:?}");
    let err = stderr(&out);
    assert!(
        err.contains("--force would lose these, so nothing was changed")
            && err.contains("edited: docs/store/notes/plan.md")
            && err.contains("repos/one: 1 uncommitted or untracked paths"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# The plan\nmine\n"
    );
    assert!(dir.join("repos/one/scratch.txt").is_file());

    // Synced and committed: --force rebuilds.
    l.ok(&["workspace", "sync", d]);
    let wt = dir.join("repos/one");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "scratch"]);
    let out = l.axe(&["workspace", &id, "--dir", d, "--force", "-v"]);
    assert!(out.status.success(), "{out:?}");
    let err = stderr(&out);
    assert!(
        err.contains("cleared") && err.contains("remove repos/one"),
        "{err}"
    );
    assert!(dir.join("repos/one/scratch.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(&plan).unwrap(),
        "# The plan\nmine\n"
    );
}

#[test]
fn verbose_prints_steps_and_vv_the_git_and_http_calls() {
    let l = Laptop::new();
    l.attach("notes/plan.md", &[("", "# The plan\nbody\n")], "label:proj");
    let id = l.task("one");
    let d = l.base.join("w");
    let d = d.to_str().unwrap();
    let quiet = l.axe(&["workspace", &id, "--dir", d]);
    assert!(quiet.status.success(), "{quiet:?}");
    assert_eq!(stderr(&quiet), "");
    let v = stderr(&l.axe(&["workspace", &id, "--dir", d, "-v"]));
    for want in [
        &format!("read task {id}\n"),
        "worktree repos/one\n",
        "write the attached docs\n",
    ] {
        assert!(v.contains(want), "{want:?} in {v}");
    }
    assert!(!v.contains("GET ") && !v.contains("git -C"), "{v}");
    let vv = stderr(&l.axe(&["workspace", &id, "--dir", d, "-vv"]));
    assert!(
        vv.contains(&format!("/api/tasks/{id}\n")) && vv.contains("git -C"),
        "{vv}"
    );
    let sync = stderr(&l.axe(&["workspace", "sync", d, "-v"]));
    assert!(sync.contains("write the attached docs"), "{sync}");
}
