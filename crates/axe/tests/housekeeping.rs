//! Housekeeping against real `git worktree` fixtures and a fake server on
//! 127.0.0.1 that serves task rows and events and records bot wakes.

mod common;

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use axe::client::Client;
use axe::config::{Config, Housekeep};
use axe::housekeeping::{Keeper, Rules};
use serde_json::{json, Value};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const OLD: &str = "2020-01-01T00:00:00Z";
/// Normal retention in these tests: 48h.
const RETENTION: &str = "retention 172800s";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    fs::write(path.join("tracked.txt"), "x").unwrap();
    git(path, &["add", "tracked.txt"]);
    git(path, &["commit", "-q", "-m", "init"]);
    // Everything so far counts as pushed; ignored paths are build output.
    git(path, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let exclude = git(path, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude = path.join(exclude.trim());
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(exclude, "_repo/\nnode_modules/\nnested/\ninner-wt/\n").unwrap();
}

fn add_worktree(owner: &Path, path: &Path, branch: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    git(
        owner,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            path.to_str().unwrap(),
        ],
    );
}

/// Fake server: GET path -> JSON body, anything else a 404. Every POST is
/// recorded as `(path, body)` and answered with `post_status`.
struct Fake {
    url: String,
    posts: Arc<Mutex<Vec<(String, Value)>>>,
}

fn serve(routes: HashMap<String, Value>, post_status: u16) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let posts = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&posts);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut req = Vec::new();
            let mut buf = [0; 4096];
            let head_end = loop {
                if let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break req.len();
                }
                req.extend_from_slice(&buf[..n]);
            };
            let head = String::from_utf8_lossy(&req[..head_end]).to_string();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            while req.len() < head_end + len {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            let mut words = head.split_whitespace();
            let method = words.next().unwrap_or("").to_string();
            let path = words.next().unwrap_or("").to_string();
            let (status, body) = if method == "POST" {
                let body = serde_json::from_slice(&req[head_end..]).unwrap_or(Value::Null);
                recorded.lock().unwrap().push((path, body));
                let reply = json!({"wake": 1, "due_at": OLD}).to_string();
                (post_status, reply)
            } else {
                match routes.get(&path) {
                    Some(v) => (200, v.to_string()),
                    None => (404, r#"{"error":"not found"}"#.to_string()),
                }
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Fake { url, posts }
}

fn task(routes: &mut HashMap<String, Value>, id: u64, project: &str, state: &str, done_at: &str) {
    routes.insert(
        format!("/api/tasks/{id}"),
        json!({"id": id, "project": project, "state": state}),
    );
    routes.insert(
        format!("/api/tasks/{id}/events"),
        json!([
            {"id": 1, "task_id": id, "at": OLD, "kind": "created", "to_value": null},
            {"id": 2, "task_id": id, "at": done_at, "kind": "state", "from_value": "in_progress", "to_value": state},
        ]),
    );
}

fn ago(d: Duration) -> String {
    (OffsetDateTime::now_utc() - d).format(&Rfc3339).unwrap()
}

/// A config for `workspace` with 48h retention and extra TOML (`[housekeeping.*]` tables).
fn config(workspace: &Path, extra: &str) -> Config {
    Config::parse(&format!(
        "workspace_root = {:?}\n[housekeeping]\nretention = \"48h\"\n{extra}",
        workspace.to_str().unwrap()
    ))
    .unwrap()
}

/// A block that is always under pressure, with the short retention.
fn always_pressure(path: &Path) -> String {
    format!(
        "[housekeeping.pressure]\npath = {:?}\nfree_below_gib = 1000000000\nretention = \"1h\"\n",
        path.to_str().unwrap()
    )
}

fn calm(path: &Path) -> String {
    format!(
        "[housekeeping.pressure]\npath = {:?}\nfree_below_gib = 0\nretention = \"1h\"\n",
        path.to_str().unwrap()
    )
}

fn keeper(cfg: &Config, url: &str, apply: bool, dir: &Path) -> Keeper {
    let client = Client::at(url.to_string(), "housekeeper".into()).with_token("t");
    Keeper::new(Rules::new(cfg).unwrap(), client, apply).with_episode(dir.join("disk-pause"))
}

/// Worktree decisions: path -> (decision, reason), checking the project field.
fn worktree_lines(lines: &[Value]) -> HashMap<PathBuf, (String, String)> {
    lines
        .iter()
        .filter(|v| v.get("path").is_some() && v.get("kind").is_none())
        .map(|v| {
            assert_eq!(v["project"], "proj", "{v}");
            let s = |k: &str| v[k].as_str().unwrap().to_string();
            (PathBuf::from(s("path")), (s("decision"), s("reason")))
        })
        .collect()
}

/// One tick of a fresh keeper: `(ok, worktree decisions)`.
fn run(
    tmp: &Path,
    cfg: &Config,
    url: &str,
    apply: bool,
) -> (bool, HashMap<PathBuf, (String, String)>) {
    let tick = keeper(cfg, url, apply, tmp).tick(None, &AtomicBool::new(false));
    (tick.ok, worktree_lines(&tick.lines))
}

#[test]
fn removes_only_old_done_task_worktrees() {
    removes_only_old_done(|_| String::new(), RETENTION);
}

#[test]
fn removes_only_old_done_task_worktrees_under_pressure() {
    removes_only_old_done(always_pressure, "retention 3600s");
}

fn removes_only_old_done(pressure: fn(&Path) -> String, retention_reason: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let main = workspace.join("proj");
    let dep = workspace.join("dep");
    let dep2 = workspace.join("dep2");
    let root = workspace.join(".worktrees/proj");
    repo(&main);
    repo(&dep);
    repo(&dep2);
    fs::create_dir_all(&root).unwrap();

    // task-7: clean worktree with ignored nested repos, worktrees and build output.
    let wt7 = root.join("task-7--work");
    add_worktree(&main, &wt7, "task-7");
    repo(&wt7.join("nested"));
    git(
        &wt7.join("nested"),
        &["worktree", "add", "-q", "-b", "inner", "../inner-wt"],
    );
    // A dependency worktree nested at <task worktree>/_repo/<name>.
    let nested_dep = wt7.join("_repo/dep2");
    add_worktree(&dep2, &nested_dep, "task-7");
    fs::create_dir_all(wt7.join("node_modules/pkg")).unwrap();
    fs::write(wt7.join("node_modules/pkg/index.js"), "x").unwrap();
    // task-8: dependency worktree under <root>/<repo>/.
    let wt8 = root.join("dep/task-8--work");
    add_worktree(&dep, &wt8, "task-8");
    // Retained: not done, too recent, other project, unknown task, symlink, other name.
    for name in [
        "task-9--work",
        "task-10--work",
        "task-11--work",
        "task-13--work",
        "legacy-context",
    ] {
        fs::create_dir(root.join(name)).unwrap();
    }
    std::os::unix::fs::symlink(&main, root.join("task-12--work")).unwrap();

    let mut routes = HashMap::new();
    task(&mut routes, 7, "proj", "done", OLD);
    task(&mut routes, 8, "proj", "done", OLD);
    task(&mut routes, 9, "proj", "pending", OLD);
    task(&mut routes, 10, "proj", "done", &ago(Duration::minutes(5)));
    task(&mut routes, 11, "other", "done", OLD);
    task(&mut routes, 12, "proj", "done", OLD);
    let url = serve(routes, 200).url;

    let retained = |lines: &HashMap<PathBuf, (String, String)>| {
        for (name, reason) in [
            ("task-9--work", "task state is \"pending\""),
            ("task-10--work", retention_reason),
            ("task-11--work", "project \"other\""),
            ("task-12--work", "not a real directory"),
            ("task-13--work", "lookup failed"),
        ] {
            let (decision, why) = &lines[&root.join(name)];
            assert_eq!(decision, "retain", "{name}");
            assert!(why.contains(reason), "{name}: {why}");
        }
        assert!(!lines.contains_key(&root.join("legacy-context")));
        assert_eq!(lines.len(), 7);
    };

    let cfg = config(&workspace, &pressure(&tmp));
    // Outage retains everything.
    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let (ok, lines) = run(&tmp, &cfg, &format!("http://{dead}"), true);
    assert!(ok);
    assert!(lines.values().all(|(d, _)| d == "retain"));
    assert!(lines[&wt7].1.contains("lookup failed"));
    assert!(wt7.exists() && wt8.exists());

    let (ok, lines) = run(&tmp, &cfg, &url, false);
    assert!(ok);
    assert_eq!(lines[&wt7].0, "would_delete");
    assert_eq!(lines[&wt8].0, "would_delete");
    retained(&lines);
    assert!(wt7.join("nested/.git").exists() && wt8.exists());

    let (ok, lines) = run(&tmp, &cfg, &url, true);
    assert!(ok);
    assert_eq!(lines[&wt7].0, "deleted", "{:?}", lines[&wt7]);
    assert_eq!(lines[&wt8].0, "deleted", "{:?}", lines[&wt8]);
    retained(&lines);
    assert!(!wt7.exists() && !wt8.exists());
    assert!(!git(&main, &["worktree", "list"]).contains("task-7--work"));
    assert!(!git(&dep, &["worktree", "list"]).contains("task-8--work"));
    assert!(!git(&dep2, &["worktree", "list"]).contains("task-7--work"));
    // Branches are never deleted.
    assert!(git(&main, &["branch", "--list", "task-7"]).contains("task-7"));
    assert!(git(&dep, &["branch", "--list", "task-8"]).contains("task-8"));
    for name in [
        "task-9--work",
        "task-10--work",
        "task-11--work",
        "task-13--work",
        "legacy-context",
    ] {
        assert!(root.join(name).is_dir(), "{name}");
    }
    assert!(fs::symlink_metadata(root.join("task-12--work"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(main.join(".git").is_dir());
}

#[test]
fn each_safety_condition_blocks_removal() {
    safety_conditions_block(|_| String::new());
}

#[test]
fn each_safety_condition_blocks_removal_under_pressure() {
    safety_conditions_block(always_pressure);
}

fn safety_conditions_block(pressure: fn(&Path) -> String) {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let main = workspace.join("proj");
    let dep = workspace.join("dep");
    let root = workspace.join(".worktrees/proj");
    repo(&main);
    repo(&dep);
    let mut routes = HashMap::new();
    let mut worktree = |id: u64| {
        let wt = root.join(format!("task-{id}--work"));
        add_worktree(&main, &wt, &format!("task-{id}"));
        task(&mut routes, id, "proj", "done", OLD);
        wt
    };

    let clean = worktree(1);
    fs::create_dir_all(clean.join("node_modules/pkg")).unwrap();
    fs::write(clean.join("node_modules/pkg/index.js"), "x").unwrap();
    let clean_dep = worktree(2);
    add_worktree(&dep, &clean_dep.join("_repo/dep"), "task-2");

    let dirty = worktree(3);
    fs::write(dirty.join("tracked.txt"), "changed").unwrap();
    let untracked = worktree(4);
    fs::write(untracked.join("notes.txt"), "x").unwrap();
    let unpushed = worktree(5);
    git(&unpushed, &["commit", "-q", "--allow-empty", "-m", "local"]);
    let nested_dirty = worktree(6);
    add_worktree(&dep, &nested_dirty.join("_repo/dep"), "task-6");
    fs::write(nested_dirty.join("_repo/dep/notes.txt"), "x").unwrap();
    let nested_unpushed = worktree(7);
    add_worktree(&dep, &nested_unpushed.join("_repo/dep"), "task-7");
    git(
        &nested_unpushed.join("_repo/dep"),
        &["commit", "-q", "--allow-empty", "-m", "local"],
    );
    let live_run = worktree(8);
    let live_process = worktree(9);
    let locked = worktree(10);
    git(&main, &["worktree", "lock", locked.to_str().unwrap()]);
    let pushed = worktree(11);
    git(&pushed, &["commit", "-q", "--allow-empty", "-m", "pushed"]);
    git(
        &pushed,
        &["update-ref", "refs/remotes/origin/task-11", "HEAD"],
    );
    let not_git = root.join("task-12--work");
    fs::create_dir_all(&not_git).unwrap();
    task(&mut routes, 12, "proj", "done", OLD);

    routes.insert(
        "/api/tasks/8/events".into(),
        json!([
            {"at": OLD, "kind": "state", "to_value": "done"},
            {"at": OLD, "kind": "run_start", "to_value": "claim"},
        ]),
    );
    let url = serve(routes, 200).url;
    let mut sleeper = Command::new("sleep")
        .arg("60")
        .current_dir(&live_process)
        .spawn()
        .unwrap();

    let kept = [
        (&dirty, "worktree has 1 modified"),
        (&untracked, "worktree has 1 modified or untracked"),
        (&unpushed, "worktree has 1 commits on no remote"),
        (&nested_dirty, "_repo/dep has 1 modified"),
        (&nested_unpushed, "_repo/dep has 1 commits"),
        (&live_run, "live run"),
        (&live_process, "running inside"),
        (&not_git, "not a registered git worktree"),
    ];

    let cfg = config(&workspace, &pressure(&tmp));
    let (ok, lines) = run(&tmp, &cfg, &url, false);
    assert!(ok);
    for wt in [&clean, &clean_dep, &locked, &pushed] {
        assert_eq!(lines[wt].0, "would_delete", "{:?}", lines[wt]);
    }

    let (ok, lines) = run(&tmp, &cfg, &url, true);
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert!(!ok, "a failed removal is reported");
    for (wt, reason) in kept {
        let (decision, why) = &lines[wt];
        assert_eq!(decision, "retain", "{wt:?}: {why}");
        assert!(why.contains(reason), "{wt:?}: {why}");
        assert!(wt.exists(), "{wt:?}");
    }
    // A failed removal is reported and does not stop later candidates.
    assert_eq!(lines[&locked].0, "error", "{:?}", lines[&locked]);
    assert!(locked.exists());
    for wt in [&clean, &clean_dep, &pushed] {
        assert_eq!(lines[wt].0, "deleted", "{:?}", lines[wt]);
        assert!(!wt.exists(), "{wt:?}");
    }
    assert!(!git(&dep, &["worktree", "list"]).contains("task-2--work"));
    assert!(git(&dep, &["branch", "--list", "task-2"]).contains("task-2"));
}

#[test]
fn retention_is_short_only_under_pressure() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let main = workspace.join("proj");
    let root = workspace.join(".worktrees/proj");
    repo(&main);
    let wt = root.join("task-1--work");
    add_worktree(&main, &wt, "task-1");
    let mut routes = HashMap::new();
    task(&mut routes, 1, "proj", "done", &ago(Duration::hours(2)));
    let url = serve(routes, 200).url;

    let (ok, lines) = run(&tmp, &config(&workspace, &calm(&tmp)), &url, true);
    assert!(ok);
    assert_eq!(lines[&wt].0, "retain", "{:?}", lines[&wt]);
    assert!(lines[&wt].1.contains(RETENTION));

    let pressed = config(&workspace, &always_pressure(&tmp));
    let (ok, lines) = run(&tmp, &pressed, &url, false);
    assert!(ok);
    assert_eq!(lines[&wt].0, "would_delete", "{:?}", lines[&wt]);
    assert!(wt.exists(), "dry-run keeps it");

    let (ok, lines) = run(&tmp, &pressed, &url, true);
    assert!(ok);
    assert_eq!(lines[&wt].0, "deleted", "{:?}", lines[&wt]);
    assert!(!wt.exists());
}

#[test]
fn unpressured_ticks_sweep_hourly() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    fs::create_dir_all(workspace.join(".worktrees/proj/task-1--work")).unwrap();
    let url = serve(HashMap::new(), 200).url;
    let stop = AtomicBool::new(false);
    let sweep = |k: &mut Keeper| worktree_lines(&k.tick(None, &stop).lines).len();

    for apply in [true, false] {
        let mut k = keeper(&config(&workspace, &calm(&tmp)), &url, apply, &tmp);
        assert_eq!(sweep(&mut k), 1);
        assert_eq!(sweep(&mut k), 0, "swept again within the hour");
    }
    let mut once = keeper(&config(&workspace, &calm(&tmp)), &url, false, &tmp).one_off();
    assert_eq!(sweep(&mut once), 1);
    assert_eq!(sweep(&mut once), 1, "a one-off sweep always reports");
    let mut pressed = keeper(
        &config(&workspace, &always_pressure(&tmp)),
        &url,
        true,
        &tmp,
    );
    assert_eq!(sweep(&mut pressed), 1);
    assert_eq!(sweep(&mut pressed), 1, "pressure sweeps every tick");
    let mut plain = keeper(&config(&workspace, ""), &url, true, &tmp);
    assert_eq!(sweep(&mut plain), 1);
    assert_eq!(
        sweep(&mut plain),
        1,
        "without [housekeeping.pressure] every tick sweeps"
    );
}

/// An executable that appends its arguments to `log`, one call per line.
fn recorder(dir: &Path, name: &str, log: &Path) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\necho \"$*\" >> {}\n", log.display()),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn cache_trim_runs_only_under_pressure() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("missing-workspace");
    let log = tmp.join("trim.log");
    let bin = recorder(&tmp, "trim", &log);
    let url = serve(HashMap::new(), 200).url;
    let block = |below: u64| {
        let bin = bin.to_str().unwrap();
        config(
            &workspace,
            &format!(
                "[housekeeping.pressure]\npath = {:?}\nfree_below_gib = {below}\n\
                 cache_trim = [[{bin:?}, \"go\"], [{bin:?}, \"uv\"]]\n",
                tmp.to_str().unwrap()
            ),
        )
    };
    let stop = AtomicBool::new(false);

    assert!(keeper(&block(0), &url, true, &tmp).tick(None, &stop).ok);
    assert!(calls(&log).is_empty(), "no pressure, no trim");

    let tick = keeper(&block(1_000_000_000), &url, false, &tmp).tick(None, &stop);
    assert!(tick.ok);
    assert!(
        tick.lines.iter().any(|l| l["result"] == "would_run"),
        "{:?}",
        tick.lines
    );
    assert!(calls(&log).is_empty(), "dry-run does not trim");

    let mut k = keeper(&block(1_000_000_000), &url, true, &tmp);
    assert!(k.tick(None, &stop).ok);
    assert_eq!(calls(&log), ["go", "uv"]);
    assert!(k.tick(None, &stop).ok);
    assert_eq!(calls(&log), ["go", "uv"], "at most hourly");
}

fn admission(admitting: Value, disk: &str) -> Value {
    json!({"enabled": true, "admitting": admitting, "checked_at": OLD,
           "signals": [{"name": "disk", "level": disk}, {"name": "memory", "level": "clear"}]})
}

fn alert_config(tmp: &Path, after: &str) -> Config {
    config(
        &tmp.join("missing-workspace"),
        &format!("[housekeeping.alert]\nbot = \"axe-os\"\nafter = {after:?}\n"),
    )
}

#[test]
fn disk_pause_alerts_once_per_episode() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let fake = serve(HashMap::new(), 200);
    let wakes = || fake.posts.lock().unwrap().clone();
    let paused = admission(json!(false), "constrained");
    let clear = admission(json!(true), "clear");
    let memory_only = admission(json!(false), "clear");
    let unknown = admission(Value::Null, "constrained");
    let stop = AtomicBool::new(false);
    let mut late = keeper(&alert_config(&tmp, "1h"), &fake.url, true, &tmp);
    let mut soon = keeper(&alert_config(&tmp, "1s"), &fake.url, true, &tmp);
    let mut dry = keeper(&alert_config(&tmp, "1s"), &fake.url, false, &tmp);

    // Before the deadline nothing fires, and a dry-run never does.
    assert!(late.tick(Some(&paused), &stop).ok);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(dry.tick(Some(&paused), &stop).ok);
    assert!(wakes().is_empty());

    // Past the deadline it fires once, however many ticks follow.
    assert!(soon.tick(Some(&paused), &stop).ok);
    assert!(soon.tick(Some(&paused), &stop).ok);
    let fired = wakes();
    assert_eq!(fired.len(), 1, "{fired:?}");
    assert_eq!(fired[0].0, "/api/bots/axe-os/wakes");
    assert_eq!(fired[0].1["now"], true);
    let key = fired[0].1["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("housekeeping-disk-pause-"), "{key}");

    // An unknown report neither ends the episode nor fires.
    assert!(soon.tick(Some(&unknown), &stop).ok);
    assert!(soon.tick(Some(&paused), &stop).ok);
    assert_eq!(wakes().len(), 1);

    // A pause for another reason is not a disk episode.
    assert!(soon.tick(Some(&memory_only), &stop).ok);
    assert_eq!(wakes().len(), 1);

    // Clearing ends it; a later pause is a new episode with a new key.
    assert!(soon.tick(Some(&clear), &stop).ok);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(soon.tick(Some(&paused), &stop).ok);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(soon.tick(Some(&paused), &stop).ok);
    let fired = wakes();
    assert_eq!(fired.len(), 2, "{fired:?}");
    assert_ne!(fired[0].1["key"], fired[1].1["key"]);
}

#[test]
fn failed_alert_is_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let fake = serve(HashMap::new(), 500);
    let paused = admission(json!(false), "constrained");
    let stop = AtomicBool::new(false);
    let mut k = keeper(&alert_config(&tmp, "1s"), &fake.url, true, &tmp);
    assert!(k.tick(Some(&paused), &stop).ok, "the episode starts");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(!k.tick(Some(&paused), &stop).ok, "reported");
    assert!(!k.tick(Some(&paused), &stop).ok);
    let posts = fake.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 2, "retried until it lands");
    assert_eq!(posts[0].1["key"], posts[1].1["key"], "same episode key");
}

/// Task-path decisions: path -> (task, decision, reason).
fn path_lines(lines: &[Value]) -> HashMap<PathBuf, (Value, String, String)> {
    lines
        .iter()
        .filter(|v| v["kind"] == "cache")
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap().to_string();
            (
                PathBuf::from(s("path")),
                (v["task"].clone(), s("decision"), s("reason")),
            )
        })
        .collect()
}

/// Cache dirs with build output, and a fake server: 1 done long ago, 2 done
/// two hours ago, 3 live (in progress), 4 done but its run has not ended,
/// 5 done long ago with a scratch dir under a second template.
fn cache_fixture(tmp: &Path) -> (PathBuf, String) {
    let caches = tmp.join("cache/proj");
    for name in [
        "task-1-target",
        "task-2-target",
        "task-3-target",
        "task-4-target",
        "task-5-scratch",
        "task-x-target",
        "taskx-target",
    ] {
        fs::create_dir_all(caches.join(name).join("debug/deps")).unwrap();
        fs::write(caches.join(name).join("debug/deps/lib.rlib"), "x").unwrap();
    }
    let mut routes = HashMap::new();
    task(&mut routes, 1, "proj", "done", OLD);
    task(&mut routes, 2, "other", "done", &ago(Duration::hours(2)));
    task(&mut routes, 3, "proj", "in_progress", OLD);
    task(&mut routes, 4, "proj", "done", OLD);
    task(&mut routes, 5, "proj", "done", OLD);
    routes.insert(
        "/api/tasks/4/events".into(),
        json!([
            {"at": OLD, "kind": "run_start", "to_value": null},
            {"at": OLD, "kind": "state", "to_value": "done"},
        ]),
    );
    (caches, serve(routes, 200).url)
}

fn cache_config(tmp: &Path, caches: &Path, extra: &str) -> Config {
    let target = caches.join("task-{id}-target");
    let scratch = caches.join("task-{id}-scratch");
    config(
        &tmp.join("missing-workspace"),
        &format!(
            "task_paths = [{:?}, {:?}]\n{extra}",
            target.to_str().unwrap(),
            scratch.to_str().unwrap()
        ),
    )
}

#[test]
fn done_task_caches_go_after_retention() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let (caches, url) = cache_fixture(&tmp);
    let at = |name: &str| caches.join(name);
    let cfg = cache_config(&tmp, &caches, "");
    let stop = AtomicBool::new(false);

    let tick = keeper(&cfg, &url, false, &tmp).tick(None, &stop);
    let lines = path_lines(&tick.lines);
    assert!(tick.ok, "{lines:?}");
    assert_eq!(lines[&at("task-1-target")].1, "would_delete");
    assert!(at("task-1-target").exists(), "dry-run keeps it");

    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &stop);
    let lines = path_lines(&tick.lines);
    assert!(tick.ok, "{lines:?}");
    let decision = |name: &str| lines[&at(name)].clone();
    assert_eq!(decision("task-1-target").1, "deleted");
    assert_eq!(decision("task-1-target").0, json!(1));
    assert!(!at("task-1-target").exists());
    assert_eq!(decision("task-5-scratch").1, "deleted");
    assert!(!at("task-5-scratch").exists());
    let (_, d, reason) = decision("task-2-target");
    assert_eq!(d, "retain");
    assert!(reason.contains(RETENTION), "{reason}");
    let (_, d, reason) = decision("task-3-target");
    assert_eq!(
        (d.as_str(), reason.as_str()),
        ("retain", "task state is \"in_progress\"")
    );
    let (_, d, reason) = decision("task-4-target");
    assert_eq!(
        (d.as_str(), reason.as_str()),
        ("retain", "task has a live run")
    );
    let (task, d, reason) = decision("task-x-target");
    assert_eq!((task, d.as_str()), (Value::Null, "retain"));
    assert!(reason.contains("no task id"), "{reason}");
    assert!(!lines.contains_key(&at("taskx-target")), "not a task path");
    for name in [
        "task-2-target",
        "task-3-target",
        "task-4-target",
        "task-x-target",
        "taskx-target",
    ] {
        assert!(at(name).exists(), "{name}");
    }
}

#[test]
fn pressure_reclaims_done_task_caches_regardless_of_retention() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let (caches, url) = cache_fixture(&tmp);
    let at = |name: &str| caches.join(name);
    let cfg = cache_config(&tmp, &caches, &always_pressure(&tmp));

    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &AtomicBool::new(false));
    let lines = path_lines(&tick.lines);
    assert!(tick.ok, "{lines:?}");
    for name in ["task-1-target", "task-2-target"] {
        assert_eq!(
            lines[&at(name)].1,
            "deleted",
            "{name}: {:?}",
            lines[&at(name)]
        );
        assert!(!at(name).exists(), "{name}");
    }
    for name in ["task-3-target", "task-4-target", "task-x-target"] {
        assert_eq!(lines[&at(name)].1, "retain", "{name}");
        assert!(at(name).exists(), "{name}");
    }
    assert!(at("taskx-target").exists());
}

#[test]
fn dispatcher_thread_applies_and_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let main = workspace.join("proj");
    repo(&main);
    let wt = workspace.join(".worktrees/proj/task-1--work");
    add_worktree(&main, &wt, "task-1");
    let mut routes = HashMap::new();
    task(&mut routes, 1, "proj", "done", OLD);
    let url = serve(routes, 200).url;
    let cfg = config(&workspace, "");
    let stop = Arc::new(AtomicBool::new(false));
    let client = Client::at(url.clone(), "housekeeper".into());

    let off = axe::housekeeping::start(
        &cfg,
        Housekeep::Off,
        client.clone(),
        axe::admission::handle(None),
        Arc::clone(&stop),
    )
    .unwrap();
    assert!(off.is_none());

    let handle = axe::housekeeping::start(
        &cfg,
        Housekeep::Apply,
        client,
        axe::admission::handle(None),
        Arc::clone(&stop),
    )
    .unwrap()
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while wt.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!wt.exists(), "the first tick runs at start");
    stop.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    handle.join().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[test]
fn cli_dry_run_prints_decisions() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let main = workspace.join("proj");
    repo(&main);
    let wt = workspace.join(".worktrees/proj/task-1--work");
    add_worktree(&main, &wt, "task-1");
    let recent = workspace.join(".worktrees/proj/task-2--work");
    add_worktree(&main, &recent, "task-2");
    let mut routes = HashMap::new();
    task(&mut routes, 1, "proj", "done", OLD);
    task(&mut routes, 2, "proj", "done", &ago(Duration::minutes(1)));
    let url = serve(routes, 200).url;
    let config = tmp.join("config.toml");
    let cli = |args: &[&str], cfg: &str| {
        fs::write(&config, cfg).unwrap();
        let mut c = Command::new(common::BIN);
        c.args(args)
            .env("AXE_CONFIG", &config)
            .env("AXE_SERVER", &url)
            .env("AXE_RETRY_SECS", "0")
            .env("AXE_ADMISSION_HOLD", tmp.join("hold.json"))
            .output()
            .unwrap()
    };
    let good = format!("workspace_root = {:?}\n", workspace.to_str().unwrap());

    let out = cli(&["housekeeping", "--dry-run"], &good);
    assert!(out.status.success(), "{out:?}");
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let lines = worktree_lines(&lines);
    assert_eq!(lines[&wt].0, "would_delete", "{lines:?}");
    assert_eq!(lines[&recent].0, "retain");
    assert!(lines[&recent].1.contains("retention 21600s"), "{lines:?}");
    assert!(wt.exists() && recent.exists(), "dry-run deletes nothing");

    // Removal is only a dispatcher mode; bad config and modes are rejected.
    assert!(!cli(&["housekeeping"], &good).status.success());
    let bad = format!("{good}[housekeeping]\nretention = \"soon\"\n");
    assert!(!cli(&["housekeeping", "--dry-run"], &bad).status.success());
    let out = cli(&["dispatch", "--housekeep=sometimes"], &good);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

/// A task row as the server derives it: stored `state`, derived `status` and
/// `updated_at`, and events with a done event when the state is done.
fn held_task(
    routes: &mut HashMap<String, Value>,
    id: u64,
    state: &str,
    status: &str,
    updated: &str,
) {
    routes.insert(
        format!("/api/tasks/{id}"),
        json!({"id": id, "project": "proj", "state": state, "status": status, "updated_at": updated}),
    );
    let mut events = vec![json!({"at": OLD, "kind": "created", "to_value": null})];
    if state == "done" {
        events.push(json!({"at": updated, "kind": "state", "to_value": "done"}));
    }
    routes.insert(format!("/api/tasks/{id}/events"), json!(events));
}

fn live_run(routes: &mut HashMap<String, Value>, id: u64) {
    routes.insert(
        format!("/api/tasks/{id}/events"),
        json!([{"at": OLD, "kind": "run_start", "to_value": null}]),
    );
}

/// Sets every mtime and atime in the tree to 2020.
fn age_tree(path: &Path) {
    let out = Command::new("find")
        .arg(path)
        .args([
            "-exec",
            "touch",
            "-h",
            "-d",
            "2020-01-01T00:00:00",
            "{}",
            "+",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write_mib(path: &Path, mib: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Random-ish bytes so no filesystem stores it sparse or compressed.
    let data: Vec<u8> = (0..mib << 20).map(|i| (i * 7919 % 251) as u8).collect();
    fs::write(path, data).unwrap();
}

/// Lines of one `kind`: path -> (task, decision, reason).
fn kind_lines(lines: &[Value], kind: &str) -> HashMap<PathBuf, (Value, String, String)> {
    lines
        .iter()
        .filter(|v| v["kind"] == kind)
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap().to_string();
            (
                PathBuf::from(s("path")),
                (v["task"].clone(), s("decision"), s("reason")),
            )
        })
        .collect()
}

fn tmp_config(tmp: &Path, scratch: &Path, extra: &str) -> Config {
    config(
        &tmp.join("missing-workspace"),
        &format!(
            "[housekeeping.tmp]\ndir = {:?}\nfloor_mib = 2\nshared = [\"shared-*\"]\n{extra}",
            scratch.to_str().unwrap()
        ),
    )
}

/// Rule A: task-named scratch. Tasks: 1 done long ago, 2 done just now,
/// 3 in progress, 4 blocked (a pending task waiting) idle for hours, 5
/// escalated but updated a minute ago, 6 done with a live run, 7 done long
/// ago (scratch held open by a process), 8 blocked idle (small scratch).
fn scratch_fixture(tmp: &Path) -> (PathBuf, String) {
    let scratch = tmp.join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    for name in [
        "t1",
        "task2-ci",
        "task-3-run",
        "t4ci-run",
        "5-build",
        "t6",
        "t7",
        "t8",
    ] {
        fs::create_dir_all(scratch.join(name).join("sub")).unwrap();
        fs::write(scratch.join(name).join("sub/f"), "x").unwrap();
    }
    fs::write(scratch.join("t1_list.json"), "[]").unwrap();
    write_mib(&scratch.join("t4ci-run/big"), 3);
    write_mib(&scratch.join("5-build/big"), 3);
    // A read-only tree (Go module cache) still goes.
    let ro = scratch.join("t1/sub");
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    // Names that map to no task: a date, a leading zero, no digits.
    for name in ["2026-11-12-notes", "t07", "tsx-1000"] {
        fs::create_dir_all(scratch.join(name)).unwrap();
    }
    let mut routes = HashMap::new();
    let hours = ago(Duration::hours(5));
    held_task(&mut routes, 1, "done", "done", OLD);
    held_task(&mut routes, 2, "done", "done", &ago(Duration::minutes(1)));
    held_task(&mut routes, 3, "in_progress", "in_progress", OLD);
    held_task(&mut routes, 4, "pending", "blocked", &hours);
    held_task(
        &mut routes,
        5,
        "escalated",
        "escalated",
        &ago(Duration::minutes(1)),
    );
    held_task(&mut routes, 6, "done", "done", OLD);
    live_run(&mut routes, 6);
    held_task(&mut routes, 7, "done", "done", OLD);
    held_task(&mut routes, 8, "blocked", "blocked", &hours);
    (scratch, serve(routes, 200).url)
}

#[test]
fn task_scratch_follows_task_state() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let (scratch, url) = scratch_fixture(&tmp);
    let at = |name: &str| scratch.join(name);
    let cfg = tmp_config(&tmp, &scratch, "");
    let mut holder = Command::new("sleep")
        .arg("30")
        .current_dir(at("t7/sub"))
        .spawn()
        .unwrap();
    let stop = AtomicBool::new(false);

    let tick = keeper(&cfg, &url, false, &tmp).tick(None, &stop);
    let dry = kind_lines(&tick.lines, "tmp_task");
    assert!(tick.ok, "{dry:?}");
    assert_eq!(dry[&at("t1")].1, "would_delete", "{:?}", dry[&at("t1")]);
    assert_eq!(
        dry[&at("t4ci-run")].1,
        "would_delete",
        "{:?}",
        dry[&at("t4ci-run")]
    );
    assert!(
        at("t1").exists() && at("t4ci-run").exists(),
        "dry-run keeps them"
    );

    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &stop);
    holder.kill().unwrap();
    holder.wait().unwrap();
    let lines = kind_lines(&tick.lines, "tmp_task");
    assert!(tick.ok, "{lines:?}");
    let d = |name: &str| lines[&at(name)].clone();
    for (name, task) in [("t1", 1), ("t1_list.json", 1), ("t4ci-run", 4)] {
        assert_eq!(d(name).0, json!(task), "{name}");
        assert_eq!(d(name).1, "deleted", "{name}: {:?}", d(name));
        assert!(!at(name).exists(), "{name}");
    }
    assert!(
        d("t4ci-run").2.contains("blocked, idle"),
        "{:?}",
        d("t4ci-run")
    );
    let retained = [
        ("task2-ci", "done 60s ago, retention 172800s"),
        ("task-3-run", "task status is \"in_progress\""),
        ("5-build", "escalated, updated 60s ago, idle 3600s"),
        ("t6", "task has a live run"),
        ("t7", "in use by a process"),
        ("t8", "below the floor of 2097152"),
    ];
    for (name, reason) in retained {
        let (_, decision, why) = d(name);
        assert_eq!(decision, "retain", "{name}");
        assert!(
            why.contains(reason) || why.starts_with(reason),
            "{name}: {why}"
        );
        assert!(at(name).exists(), "{name}");
    }
    for name in ["2026-11-12-notes", "t07", "tsx-1000"] {
        assert!(!lines.contains_key(&at(name)), "{name} maps to no task");
        assert!(at(name).exists(), "{name}");
    }
}

#[test]
fn stale_scratch_goes_only_under_pressure() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let scratch = tmp.join("scratch");
    for name in [
        "old",
        "fresh",
        "shared-read",
        "shared-old",
        "held",
        "listening",
    ] {
        fs::create_dir_all(scratch.join(name).join("sub")).unwrap();
        fs::write(scratch.join(name).join("sub/f"), "x").unwrap();
    }
    fs::write(scratch.join("old-file.log"), "x").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(scratch.join("listening/sock")).unwrap();
    for name in [
        "old",
        "shared-read",
        "shared-old",
        "held",
        "listening",
        "old-file.log",
    ] {
        age_tree(&scratch.join(name));
    }
    // A shared cache read recently (atime) counts as used.
    let out = Command::new("touch")
        .args(["-a"])
        .arg(scratch.join("shared-read/sub/f"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let mut holder = Command::new("sleep")
        .arg("30")
        .current_dir(scratch.join("held/sub"))
        .spawn()
        .unwrap();
    let url = serve(HashMap::new(), 200).url;
    let at = |name: &str| scratch.join(name);
    let stop = AtomicBool::new(false);

    // Calm: the dispatcher only counts stale directories.
    let cfg = tmp_config(&tmp, &scratch, &calm(&tmp));
    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &stop);
    assert!(
        kind_lines(&tick.lines, "tmp_stale").is_empty(),
        "{:?}",
        tick.lines
    );
    let count = tick
        .lines
        .iter()
        .find(|l| l["event"] == "tmp_stale")
        .unwrap();
    assert_eq!(count["stale_dirs"], 4, "{count}");
    // A one-off dry run lists them.
    let tick = keeper(&cfg, &url, false, &tmp).one_off().tick(None, &stop);
    let calm_lines = kind_lines(&tick.lines, "tmp_stale");
    let (_, decision, reason) = &calm_lines[&at("old")];
    assert_eq!(decision, "retain");
    assert!(
        reason.starts_with("not under pressure; would go: scratch unused"),
        "{reason}"
    );
    assert!(at("old").exists());

    let cfg = tmp_config(&tmp, &scratch, &always_pressure(&tmp));
    let dry = keeper(&cfg, &url, false, &tmp).tick(None, &stop);
    assert_eq!(
        kind_lines(&dry.lines, "tmp_stale")[&at("old")].1,
        "would_delete"
    );
    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &stop);
    holder.kill().unwrap();
    holder.wait().unwrap();
    drop(listener);
    let lines = kind_lines(&tick.lines, "tmp_stale");
    assert!(tick.ok, "{lines:?}");
    for name in ["old", "shared-old"] {
        assert_eq!(
            lines[&at(name)].1,
            "deleted",
            "{name}: {:?}",
            lines[&at(name)]
        );
        assert!(!at(name).exists(), "{name}");
    }
    assert!(lines[&at("shared-old")]
        .2
        .starts_with("shared cache unused"));
    for (name, reason) in [
        ("held", "in use by a process"),
        ("listening", "sock is in use"),
    ] {
        let (_, decision, why) = &lines[&at(name)];
        assert_eq!(decision, "retain", "{name}");
        assert!(why.contains(reason), "{name}: {why}");
        assert!(at(name).exists(), "{name}");
    }
    for name in ["fresh", "shared-read"] {
        assert!(!lines.contains_key(&at(name)), "{name} is in use: no line");
        assert!(at(name).exists(), "{name}");
    }
    assert!(
        at("old-file.log").exists(),
        "only directories are stale scratch"
    );
}

/// Rule C: build output in the worktree of an idle task.
#[test]
fn idle_worktrees_lose_ignored_build_output_only() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let main = workspace.join("proj");
    let root = workspace.join(".worktrees/proj");
    repo(&main);
    let wt = |id: u64| root.join(format!("task-{id}--work"));
    for id in [1, 2, 3, 4] {
        add_worktree(&main, &wt(id), &format!("task-{id}"));
        // Ignored build output at depth, one not ignored, one tracked.
        write_mib(&wt(id).join("web/node_modules/pkg/index.js"), 1);
        fs::create_dir_all(wt(id).join("target/debug")).unwrap();
        fs::write(wt(id).join("target/debug/bin"), "x").unwrap();
        fs::create_dir_all(wt(id).join("docs/out")).unwrap();
        fs::write(wt(id).join("docs/out/page.md"), "tracked").unwrap();
        git(&wt(id), &["add", "docs/out/page.md"]);
        git(&wt(id), &["commit", "-q", "-m", "docs"]);
        // A dependency checkout under the ignored `_repo/` with its own rules.
        let dep = wt(id).join("_repo/dep");
        repo(&dep);
        fs::write(dep.join(".gitignore"), "/target/\n").unwrap();
        fs::create_dir_all(dep.join("target/release/.fingerprint")).unwrap();
        fs::write(dep.join("target/release/.fingerprint/x"), "x").unwrap();
        // Go-style read-only module cache inside the output.
        fs::set_permissions(
            dep.join("target/release"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
    }
    // 1 blocked idle (unpushed commit keeps the worktree), 2 escalated just now,
    // 3 in progress, 4 done long ago with a live run.
    let mut routes = HashMap::new();
    held_task(
        &mut routes,
        1,
        "blocked",
        "blocked",
        &ago(Duration::hours(2)),
    );
    held_task(
        &mut routes,
        2,
        "escalated",
        "escalated",
        &ago(Duration::minutes(5)),
    );
    held_task(&mut routes, 3, "in_progress", "in_progress", OLD);
    held_task(&mut routes, 4, "done", "done", OLD);
    live_run(&mut routes, 4);
    let url = serve(routes, 200).url;
    let cfg = config(
        &workspace,
        "[housekeeping.build_output]\nnames = [\"target\", \"node_modules\", \"out\"]\n",
    );

    let tick = keeper(&cfg, &url, false, &tmp).tick(None, &AtomicBool::new(false));
    let dry = kind_lines(&tick.lines, "build_output");
    assert!(tick.ok, "{dry:?}");
    assert_eq!(dry[&wt(1).join("web/node_modules")].1, "would_delete");
    assert!(wt(1).join("web/node_modules").exists());

    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &AtomicBool::new(false));
    let lines = kind_lines(&tick.lines, "build_output");
    assert!(tick.ok, "{lines:?}");
    for rel in ["web/node_modules", "_repo/dep/target"] {
        let (task, decision, reason) = &lines[&wt(1).join(rel)];
        assert_eq!(
            (task, decision.as_str()),
            (&json!(1), "deleted"),
            "{rel}: {reason}"
        );
        assert!(reason.starts_with("blocked, idle"), "{reason}");
        assert!(!wt(1).join(rel).exists(), "{rel}");
    }
    let (_, decision, reason) = &lines[&wt(1).join("target")];
    assert_eq!(
        (decision.as_str(), reason.as_str()),
        ("retain", "not ignored by git")
    );
    let (_, decision, reason) = &lines[&wt(1).join("docs/out")];
    assert_eq!(decision, "retain");
    assert!(
        reason.contains("not ignored by git") || reason.contains("tracks"),
        "{reason}"
    );
    assert!(wt(1).join("docs/out/page.md").exists());
    assert!(wt(1).join("tracked.txt").exists());
    assert!(wt(1).join("_repo/dep/tracked.txt").exists());
    assert!(wt(1).join("target/debug/bin").exists());
    // The worktree itself stays: the task is not done.
    assert!(worktree_lines(&tick.lines)[&wt(1)].0 == "retain");
    // Not idle long enough, or in progress: silent and untouched.
    for id in [2, 3] {
        assert!(
            !lines.keys().any(|p| p.starts_with(wt(id))),
            "task {id}: {lines:?}"
        );
        assert!(wt(id).join("web/node_modules").exists(), "task {id}");
    }
    let (_, decision, reason) = &lines[&wt(4).join("web/node_modules")];
    assert_eq!(
        (decision.as_str(), reason.as_str()),
        ("retain", "task has a live run")
    );
    assert!(wt(4).join("web/node_modules").exists());
}

/// Rule D: cache-root entries no task path claims.
#[test]
fn orphan_caches_go_once_unused() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let caches = tmp.join("cache");
    for name in ["legacy-target", "fresh-target", "task-1-target"] {
        fs::create_dir_all(caches.join(name).join("debug")).unwrap();
        fs::write(caches.join(name).join("debug/x"), "x").unwrap();
    }
    fs::set_permissions(
        caches.join("legacy-target/debug"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    age_tree(&caches.join("legacy-target"));
    age_tree(&caches.join("task-1-target"));
    let mut routes = HashMap::new();
    held_task(&mut routes, 1, "in_progress", "in_progress", OLD);
    let url = serve(routes, 200).url;
    let cfg = config(
        &tmp.join("missing-workspace"),
        &format!(
            "task_paths = [{:?}]\n[housekeeping.orphan_caches]\nroots = [{:?}]\nunused = \"7d\"\n",
            caches.join("task-{id}-target").to_str().unwrap(),
            caches.to_str().unwrap()
        ),
    );
    let at = |name: &str| caches.join(name);

    let dry = keeper(&cfg, &url, false, &tmp).tick(None, &AtomicBool::new(false));
    assert_eq!(
        kind_lines(&dry.lines, "orphan_cache")[&at("legacy-target")].1,
        "would_delete"
    );

    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &AtomicBool::new(false));
    let lines = kind_lines(&tick.lines, "orphan_cache");
    assert!(tick.ok, "{lines:?}");
    let (task, decision, reason) = &lines[&at("legacy-target")];
    assert_eq!(
        (task, decision.as_str()),
        (&Value::Null, "deleted"),
        "{reason}"
    );
    assert!(!at("legacy-target").exists());
    let (_, decision, reason) = &lines[&at("fresh-target")];
    assert_eq!(decision, "retain");
    assert!(reason.contains("unused 604800s"), "{reason}");
    assert!(
        !lines.contains_key(&at("task-1-target")),
        "a task path, not an orphan"
    );
    assert_eq!(
        kind_lines(&tick.lines, "cache")[&at("task-1-target")].1,
        "retain"
    );
    assert!(at("task-1-target").exists() && at("fresh-target").exists());
}

/// A run folder as `run_workspace.rs` lays it out: generated files and one
/// worktree of each of `repos` (owner, name) under `repos/<name>`.
fn run_folder(dir: &Path, repos: &[(&Path, &str)]) {
    fs::create_dir_all(dir.join("docs")).unwrap();
    fs::create_dir_all(dir.join("skills")).unwrap();
    fs::write(dir.join("AGENTS.md"), "x").unwrap();
    fs::write(dir.join("docs/index.md"), "x").unwrap();
    for agent in [".agents", ".claude"] {
        fs::create_dir_all(dir.join(agent)).unwrap();
        std::os::unix::fs::symlink("../skills", dir.join(agent).join("skills")).unwrap();
    }
    let name = dir.file_name().unwrap().to_str().unwrap();
    for (owner, repo) in repos {
        add_worktree(
            owner,
            &dir.join("repos").join(repo),
            &format!("{name}--work"),
        );
        std::os::unix::fs::symlink(
            format!("../repos/{repo}/.agents/skills/s"),
            dir.join("skills").join(repo),
        )
        .unwrap();
    }
}

#[test]
fn done_task_run_folders_go_after_retention() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path().canonicalize().unwrap();
    let workspace = tmp.join("workspace");
    let src = workspace.join("src");
    let lib = workspace.join("lib");
    repo(&src);
    repo(&lib);
    let runs = workspace.join(".runs");
    let folder = |id: u64| runs.join(format!("task-{id}"));

    // task-20: done long ago, two repo worktrees -> removed and deregistered.
    run_folder(&folder(20), &[(&src, "src"), (&lib, "lib")]);
    // task-21: done long ago, no repos -> removed.
    run_folder(&folder(21), &[]);
    // Kept: waiting (pending), stored blocked, escalated, in progress with a
    // live run, done too recently, done but a live run, an extra file, a dirty
    // repo worktree, an unpushed commit.
    for id in 22..=30 {
        run_folder(&folder(id), &[(&src, "src")]);
    }
    fs::write(folder(28).join("notes.md"), "mine").unwrap();
    fs::write(folder(29).join("repos/src/tracked.txt"), "changed").unwrap();
    git(
        &folder(30).join("repos/src"),
        &["commit", "-q", "--allow-empty", "-m", "local"],
    );
    // Bot run folder of a done-looking id, and an unrelated name.
    run_folder(&runs.join("bot-task-20"), &[(&src, "src")]);
    fs::create_dir_all(runs.join("scratch")).unwrap();

    let mut routes = HashMap::new();
    for id in [20, 21, 28, 29, 30] {
        task(&mut routes, id, "proj", "done", OLD);
    }
    held_task(&mut routes, 22, "pending", "blocked", OLD);
    held_task(&mut routes, 23, "blocked", "blocked", OLD);
    held_task(&mut routes, 24, "escalated", "escalated", OLD);
    task(&mut routes, 25, "proj", "in_progress", OLD);
    live_run(&mut routes, 25);
    task(&mut routes, 26, "proj", "done", &ago(Duration::minutes(5)));
    task(&mut routes, 27, "proj", "done", OLD);
    routes.insert(
        "/api/tasks/27/events".into(),
        json!([
            {"at": OLD, "kind": "state", "to_value": "done"},
            {"at": OLD, "kind": "run_start", "to_value": null},
        ]),
    );
    let url = serve(routes, 200).url;
    let cfg = config(&workspace, "");

    let retained = |lines: &HashMap<PathBuf, (Value, String, String)>| {
        for (id, reason) in [
            (22, "task state is \"pending\""),
            (23, "task state is \"blocked\""),
            (24, "task state is \"escalated\""),
            (25, "task state is \"in_progress\""),
            (26, RETENTION),
            (27, "live run"),
            (28, "notes.md is not generated by axe"),
            (29, "repos/src has 1 modified"),
            (30, "repos/src has 1 commits on no remote branch"),
        ] {
            let (task, decision, why) = &lines[&folder(id)];
            assert_eq!(task, &json!(id));
            assert_eq!(decision, "retain", "task-{id}: {why}");
            assert!(why.contains(reason), "task-{id}: {why}");
            assert!(folder(id).join("repos/src/.git").exists() || id == 22);
        }
        assert!(!lines
            .keys()
            .any(|p| p.starts_with(runs.join("bot-task-20"))));
        assert!(!lines.contains_key(&runs.join("scratch")));
        assert_eq!(lines.len(), 11);
    };

    let tick = keeper(&cfg, &url, false, &tmp).tick(None, &AtomicBool::new(false));
    assert!(tick.ok);
    let lines = kind_lines(&tick.lines, "run_folder");
    assert_eq!(
        lines[&folder(20)].1,
        "would_delete",
        "{:?}",
        lines[&folder(20)]
    );
    assert_eq!(lines[&folder(21)].1, "would_delete");
    retained(&lines);
    assert!(folder(20).exists() && folder(21).exists());

    let tick = keeper(&cfg, &url, true, &tmp).tick(None, &AtomicBool::new(false));
    assert!(tick.ok, "{:?}", tick.lines);
    let lines = kind_lines(&tick.lines, "run_folder");
    assert_eq!(lines[&folder(20)].1, "deleted", "{:?}", lines[&folder(20)]);
    assert!(lines[&folder(20)].2.contains("repos/lib removed"));
    assert_eq!(lines[&folder(21)].1, "deleted", "{:?}", lines[&folder(21)]);
    retained(&lines);
    assert!(!folder(20).exists() && !folder(21).exists());
    // Deregistered in each source repo; branches stay.
    let src_list = git(&src, &["worktree", "list"]);
    assert!(!src_list.contains("/task-20/"), "{src_list}");
    assert!(!git(&lib, &["worktree", "list"]).contains("/task-20/"));
    assert!(src_list.contains("/task-22/"), "{src_list}");
    assert!(git(&src, &["branch", "--list", "task-20--work"]).contains("task-20--work"));
    assert!(runs.join("bot-task-20/repos/src/.git").exists());
    assert!(src_list.contains("bot-task-20/"));
    assert!(runs.join("scratch").is_dir());
}
