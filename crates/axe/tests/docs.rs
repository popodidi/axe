//! `axe doc` and the `/api/docs` routes: versioned docs with git as the
//! backend, and attachments to scopes.
mod common;

use common::{Server, TOKEN};
use serde_json::{json, Value};

fn json(out: &str) -> Value {
    serde_json::from_str(out).unwrap()
}

/// (status, body) of an authorized API write.
fn post(srv: &Server, path: &str, body: Value) -> (u16, Value) {
    match ureq::post(&format!("{}{path}", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(body)
    {
        Ok(r) => (r.status(), r.into_json().unwrap()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap()),
        Err(e) => panic!("POST {path}: {e}"),
    }
}

/// An authorized API read.
fn get(srv: &Server, path: &str) -> Value {
    ureq::get(&format!("{}{path}", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .into_json()
        .unwrap()
}

fn put(srv: &Server, path: &str, stdin: &str, extra: &[&str]) -> Value {
    use std::io::Write;
    let mut args = vec!["doc", "put", path, "--json"];
    args.extend_from_slice(extra);
    let mut child = srv
        .cli(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    json(&String::from_utf8(out.stdout).unwrap())
}

#[test]
fn put_get_versions_log_and_diff() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());

    // Round trip; each changing write is a new version.
    let v1 = put(&srv, "notes/plan.md", "one\ntwo\n", &["-m", "first draft"]);
    assert_eq!(v1["changed"], true);
    assert_eq!(srv.ok(&["doc", "get", "notes/plan.md"]), "one\ntwo\n");
    let v2 = put(&srv, "notes/plan.md", "one\n2\n", &[]);
    assert_eq!(v2["changed"], true);
    assert_eq!(v2["previous"], v1["version"]);
    assert_ne!(v2["version"], v1["version"]);
    let same = put(&srv, "notes/plan.md", "one\n2\n", &[]);
    assert_eq!(same["changed"], false);
    assert_eq!(same["version"], v2["version"]);

    // Past versions stay readable.
    let old = json(&srv.ok(&[
        "doc",
        "get",
        "notes/plan.md",
        "--version",
        v1["version"].as_str().unwrap(),
        "--json",
    ]));
    assert_eq!(old["content"], "one\ntwo\n");
    assert_eq!(old["version"], v1["version"]);

    // Log: newest first, with author and message.
    let log = json(&srv.ok(&["doc", "log", "notes/plan.md", "--json"]));
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["version"], v2["version"]);
    assert_eq!(log[1]["version"], v1["version"]);
    assert_eq!(log[1]["message"], "first draft");
    assert_eq!(log[1]["author"], "human");

    // Diff: the last change by default, or between named versions.
    let d = srv.ok(&["doc", "diff", "notes/plan.md"]);
    assert!(d.contains("-two\n+2\n"), "{d}");
    assert!(d.contains("a/notes/plan.md"), "{d}");
    let d = srv.ok(&[
        "doc",
        "diff",
        "notes/plan.md",
        v2["version"].as_str().unwrap(),
        v1["version"].as_str().unwrap(),
    ]);
    assert!(d.contains("-2\n+two\n"), "{d}");
    // The first version diffs against nothing.
    let first = get(
        &srv,
        &format!(
            "/api/docs/diff?path=notes/plan.md&to={}",
            v1["version"].as_str().unwrap()
        ),
    );
    assert_eq!(first["from"], Value::Null);
    assert!(
        first["diff"].as_str().unwrap().contains("+one\n+two\n"),
        "{first}"
    );

    // ls shows current docs with their versions.
    put(&srv, "memory/a.md", "a\n", &[]);
    let ls = json(&srv.ok(&["doc", "ls", "--json"]));
    let paths: Vec<&str> = ls
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["memory/a.md", "notes/plan.md"]);
    assert_eq!(ls[1]["version"], v2["version"]);
    let ls = json(&srv.ok(&["doc", "ls", "notes", "--json"]));
    assert_eq!(ls.as_array().unwrap().len(), 1);

    // Missing docs and bad input.
    let out = srv.run(&["doc", "get", "nope.md"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    let (status, _) = post(&srv, "/api/docs", json!({"path": "../x", "content": "x"}));
    assert_eq!(status, 400);

    // Reads need the token, like blobs.
    let r = ureq::get(&format!("{}/api/docs", srv.url)).call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))));
}

#[test]
fn a_skill_is_a_folder_of_docs() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let skill = dir.path().join("review");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: review\n---\nBody\n").unwrap();
    std::fs::write(skill.join("references/checks.md"), "- one\n").unwrap();
    std::fs::write(skill.join(".hidden"), "skip").unwrap();

    let w = json(&srv.ok(&[
        "doc",
        "put",
        "skills/review",
        skill.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(w["changed"], true);
    let got = json(&srv.ok(&["doc", "get", "skills/review", "--json"]));
    assert_eq!(got["kind"], "folder");
    assert_eq!(
        got["files"],
        json!([
            {"path": "SKILL.md", "content": "---\nname: review\n---\nBody\n"},
            {"path": "references/checks.md", "content": "- one\n"},
        ])
    );
    // One file of the folder reads as a doc.
    assert_eq!(
        srv.ok(&["doc", "get", "skills/review/references/checks.md"]),
        "- one\n"
    );

    // A folder write replaces the folder: removed files go away.
    std::fs::remove_file(skill.join("references/checks.md")).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: review\n---\nBody v2\n").unwrap();
    srv.ok(&["doc", "put", "skills/review", skill.to_str().unwrap()]);
    let out = dir.path().join("out");
    srv.ok(&["doc", "get", "skills/review", "-o", out.to_str().unwrap()]);
    assert_eq!(
        std::fs::read_to_string(out.join("SKILL.md")).unwrap(),
        "---\nname: review\n---\nBody v2\n"
    );
    assert!(!out.join("references").exists());
    let log = json(&srv.ok(&["doc", "log", "skills/review", "--json"]));
    assert_eq!(log.as_array().unwrap().len(), 2);
    let d = srv.ok(&["doc", "diff", "skills/review"]);
    assert!(d.contains("+Body v2") && d.contains("-- one"), "{d}");
}

#[test]
fn attach_and_detach_to_each_scope() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    put(&srv, "policy/delivery.md", "ship it\n", &[]);
    put(&srv, "skills/review/SKILL.md", "skill\n", &[]);

    let scopes = ["global", "label:infra", "owner:bot:ops", "repo:axe"];
    for s in scopes {
        let out = srv.ok(&["doc", "attach", "policy/delivery.md", s]);
        assert!(out.contains("attached"), "{out}");
    }
    srv.ok(&["doc", "attach", "skills/review", "label:infra"]);
    // Attaching twice changes nothing.
    assert!(srv
        .ok(&["doc", "attach", "skills/review", "label:infra"])
        .contains("unchanged"));

    for s in scopes {
        let v = json(&srv.ok(&["doc", "ls", "--scope", s, "--json"]));
        let mut paths: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["path"].as_str().unwrap())
            .collect();
        paths.sort();
        let want: &[&str] = if s == "label:infra" {
            &["policy/delivery.md", "skills/review"]
        } else {
            &["policy/delivery.md"]
        };
        assert_eq!(paths, want, "{s}");
        assert!(v.as_array().unwrap().iter().all(|a| a["exists"] == true));
    }
    // Other names of a kind are separate scopes.
    let v = json(&srv.ok(&["doc", "ls", "--scope", "label:other", "--json"]));
    assert_eq!(v, json!([]));
    // Every attachment at once.
    let all = get(&srv, "/api/docs/attachments");
    assert_eq!(all.as_array().unwrap().len(), 5);

    for s in scopes {
        let out = srv.ok(&["doc", "detach", "policy/delivery.md", s]);
        assert!(out.contains("detached"), "{out}");
    }
    let v = json(&srv.ok(&["doc", "ls", "--scope", "label:infra", "--json"]));
    assert_eq!(v[0]["path"], "skills/review");
    assert_eq!(v.as_array().unwrap().len(), 1);
    let v = json(&srv.ok(&["doc", "ls", "--scope", "global", "--json"]));
    assert_eq!(v, json!([]));

    // Detaching what is not attached, attaching what does not exist, bad scopes.
    let out = srv.run(&["doc", "detach", "policy/delivery.md", "global"]);
    assert!(!out.status.success());
    let out = srv.run(&["doc", "attach", "nope.md", "global"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    let (status, _) = post(
        &srv,
        "/api/docs/attach",
        json!({"path": "policy/delivery.md", "scope": "team:x"}),
    );
    assert_eq!(status, 400);
}

#[test]
fn concurrent_writes_conflict_instead_of_losing_changes() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let v1 = put(&srv, "conduct.md", "v1\n", &[]);
    let base = v1["version"].as_str().unwrap().to_string();

    // Eight writers edit the same version at once: one wins, the rest get
    // 409 with the version they must merge with.
    let results: Vec<(u16, Value)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (srv, base) = (&srv, &base);
                s.spawn(move || {
                    post(
                        srv,
                        "/api/docs",
                        json!({"path": "conduct.md", "content": format!("writer {i}\n"), "base": base}),
                    )
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let won: Vec<_> = results.iter().filter(|(c, _)| *c == 201).collect();
    let lost: Vec<_> = results.iter().filter(|(c, _)| *c == 409).collect();
    assert_eq!((won.len(), lost.len()), (1, 7), "{results:?}");
    let winner = won[0].1["version"].as_str().unwrap();
    for (_, e) in &lost {
        assert!(e["error"].as_str().unwrap().contains(winner), "{e}");
    }

    // The CLI reports the conflict; --new refuses an existing doc.
    let out = srv
        .cli(&["doc", "put", "conduct.md", "--base", &base])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("conflict"));
    let out = srv
        .cli(&["doc", "put", "conduct.md", "--new"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("conflict"));

    // Writes without a base all land, one version each, none lost.
    std::thread::scope(|s| {
        for i in 0..6 {
            let srv = &srv;
            s.spawn(move || {
                let (c, v) = post(
                    srv,
                    "/api/docs",
                    json!({"path": "conduct.md", "content": format!("free {i}\n")}),
                );
                assert_eq!(c, 201, "{v}");
            });
        }
    });
    let log = json(&srv.ok(&["doc", "log", "conduct.md", "--json"]));
    assert_eq!(log.as_array().unwrap().len(), 1 + 1 + 6);
}

#[test]
fn docs_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    put(&srv, "a.md", "kept\n", &[]);
    srv.ok(&["doc", "attach", "a.md", "repo:axe"]);
    drop(srv);
    let srv = Server::start(dir.path());
    assert_eq!(srv.ok(&["doc", "get", "a.md"]), "kept\n");
    let v = json(&srv.ok(&["doc", "ls", "--scope", "repo:axe", "--json"]));
    assert_eq!(v[0]["path"], "a.md");
    assert!(dir.path().join("docs.git/HEAD").is_file());
}
