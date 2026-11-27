//! Sealed forms end to end: `axe form ask` through the server to a real
//! dispatcher, a reply sealed as the static page seals it, delivery into the
//! task's secrets folder and the task's next run, and no value in the server
//! database, events or logs.

mod common;

use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use axe::sealed::{self, Sealed, Signed};
use common::{Server, TOKEN};
use serde_json::{json, Value};

/// Unique enough to grep for anywhere.
const DUMMY: &str = "dummy-secret-8c1f0e5a7b";

fn setup() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join("a")).unwrap();
    let out = dir.path().join("agent-out");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"workspace_root = "{ws}"
log_dir = "{logs}"
poll_seconds = 1
[defaults]
agents = ["run"]
max_runs = 3
[agents.run]
cmd = ["sh", "-c", "printf '%s|%s' \"$AXE_SECRETS_DIR\" \"$DEPLOY_TOKEN\" > {out}"]
"#,
            ws = ws.display(),
            logs = dir.path().join("logs").display(),
            out = out.display(),
        ),
    )
    .unwrap();
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    (dir, srv)
}

/// `axe form ask` once the node is connected (it signs the form).
fn ask(srv: &Server, args: &[&str]) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut a = vec!["form", "ask", "--json", "--no-telegram"];
        a.extend_from_slice(args);
        let out = srv.run(&a);
        if out.status.success() {
            return serde_json::from_slice(&out.stdout).unwrap();
        }
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(
            err.contains("not connected") && Instant::now() < deadline,
            "form ask: {err}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The signed spec in a form link's fragment, as the page reads it.
fn signed(link: &str) -> Signed {
    let token = link.split_once("#f=").unwrap().1;
    let (spec, sig) = token.split_once('.').unwrap();
    Signed {
        spec: spec.into(),
        sig: sig.into(),
    }
}

fn stop(child: &mut std::process::Child) -> String {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "process did not stop");
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    err
}

fn contains(path: &Path, needle: &str) -> bool {
    std::fs::read(path)
        .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .unwrap_or(false)
}

#[test]
fn a_sealed_reply_reaches_only_the_requesting_task() {
    let (dir, mut srv) = setup();
    let task = srv
        .ok(&[
            "add",
            "--project",
            "a",
            "--title",
            "needs a token",
            "--state",
            "blocked",
        ])
        .trim()
        .trim_start_matches("added ")
        .parse::<i64>()
        .ok()
        .unwrap_or(1);
    let other = 2;
    srv.ok(&[
        "add",
        "--project",
        "a",
        "--title",
        "bystander",
        "--state",
        "blocked",
    ]);
    let mut disp = srv.dispatcher(&[], &[]);
    let task_s = task.to_string();

    // Schema validation happens before anything is signed.
    let bad = srv.run(&[
        "form",
        "ask",
        "--no-telegram",
        "--task",
        &task_s,
        "--purpose",
        "x",
        "--field",
        "Bad:secret",
    ]);
    assert!(!bad.status.success());

    let v = ask(
        &srv,
        &[
            "--task",
            &task_s,
            "--purpose",
            "Deploy token for the test",
            "--field",
            "token:secret:required:env=DEPLOY_TOKEN",
            "--field",
            "note:text",
        ],
    );
    assert_eq!(v["state"], "open");
    assert_eq!(v["target"], json!({"kind": "task", "task": task}));
    let link = v["link"].as_str().unwrap();
    assert!(link.starts_with(axe::config::DEFAULT_FORM_PAGE));
    let s = signed(link);
    let spec = s.decode().unwrap();
    assert_eq!(spec.from, format!("task {task}"));
    assert_eq!(
        v["fingerprint"].as_str().unwrap(),
        sealed::fingerprint_b64(&spec.pubkey).unwrap()
    );
    // The key is next to the node's config file, private.
    let key = dir.path().join("dispatcher.key");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A reply that does not decrypt is refused; the form stays open.
    let form = spec.form.clone();
    let mut forged = sealed::seal(&spec, &json!({"token": "x"})).unwrap();
    let n = forged.blob.len();
    forged.blob[n - 1] ^= 1;
    let out = srv.run(&["form", "submit", &forged.text()]);
    assert!(out.status.success(), "{out:?}");
    let shown: Value = serde_json::from_str(&srv.ok(&["form", "show", &form, "--json"])).unwrap();
    assert_eq!(shown["state"], "open");
    assert!(shown["error"]
        .as_str()
        .unwrap()
        .contains("does not decrypt"));

    // The real reply.
    let reply = sealed::seal(&spec, &json!({"token": DUMMY, "note": "hello"})).unwrap();
    let out = srv.ok(&["form", "submit", &reply.text()]);
    assert!(out.contains("delivered"), "{out}");
    let waited = srv.ok(&["form", "wait", &form, "--timeout", "10s", "--json"]);
    let waited: Value = serde_json::from_str(&waited).unwrap();
    assert_eq!(waited["state"], "delivered");
    assert!(!waited.to_string().contains(DUMMY));

    // One use only.
    let again = srv.run(&["form", "submit", &reply.text()]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already used"));

    // The value is a 0600 file in the requesting task's folder only.
    let secrets = dir
        .path()
        .join("ws/.runs")
        .join(format!("task-{task}"))
        .join(".secrets");
    assert_eq!(
        std::fs::read_to_string(secrets.join("token")).unwrap(),
        DUMMY
    );
    assert_eq!(
        std::fs::metadata(secrets.join("token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(!dir
        .path()
        .join("ws/.runs")
        .join(format!("task-{other}"))
        .exists());

    // The task's next run gets the folder and its env value.
    srv.ok(&["set", &task_s, "--state", "pending"]);
    let out = dir.path().join("agent-out");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !out.exists() || std::fs::read_to_string(&out).unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the task never ran");
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("{}|{DUMMY}", secrets.display())
    );

    // The task log names the form, never the value.
    let log = srv.ok(&["log", &task_s]);
    assert!(log.contains("form"), "{log}");
    assert!(log.contains("delivered"), "{log}");
    assert!(!log.contains(DUMMY));

    // Nothing secret in the server database or either log.
    let derr = stop(&mut disp.0);
    let serr = stop(&mut srv.child);
    for f in ["t.db", "t.db-wal", "t.db-shm"] {
        assert!(!contains(&dir.path().join(f), DUMMY), "{f} holds the value");
    }
    assert!(!serr.contains(DUMMY));
    assert!(!derr.contains(DUMMY));
    assert!(derr.contains("sealed form key SHA256:"), "{derr}");
    assert!(
        serr.contains(&format!("form {form}: delivered to task {task}")),
        "{serr}"
    );
    // The node's run log holds no value either (env is not logged).
    for e in std::fs::read_dir(dir.path().join("logs")).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            assert!(!contains(&p, DUMMY), "{} holds the value", p.display());
        }
    }
}

#[test]
fn forms_need_the_token_to_read() {
    let (_dir, srv) = setup();
    let r = ureq::get(&format!("{}/api/forms", srv.url)).call();
    assert!(matches!(r, Err(ureq::Error::Status(401, _))), "{r:?}");
    let parsed = Sealed::parse("sealed.v1.00000000000000000000000000000000.AAAA");
    assert!(parsed.is_err());
}

#[test]
fn an_open_form_survives_a_server_restart() {
    let (dir, mut srv) = setup();
    let task = srv
        .ok(&[
            "add",
            "--project",
            "a",
            "--title",
            "t",
            "--state",
            "blocked",
        ])
        .trim()
        .trim_start_matches("added ")
        .parse::<i64>()
        .ok()
        .unwrap_or(1);
    let task_s = task.to_string();
    let mut disp = srv.dispatcher(&[], &[]);
    let v = ask(
        &srv,
        &[
            "--task",
            &task_s,
            "--purpose",
            "p",
            "--field",
            "token:secret:required",
        ],
    );
    let spec = signed(v["link"].as_str().unwrap()).decode().unwrap();

    // The deploy restarts the server (and the node).
    stop(&mut disp.0);
    stop(&mut srv.child);
    let config = srv.config.clone();
    drop(srv);
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    let shown: Value =
        serde_json::from_str(&srv.ok(&["form", "show", &spec.form, "--json"])).unwrap();
    assert_eq!(shown["state"], "open");
    let _disp = srv.dispatcher(&[], &[]);

    // Accepted (delivered, or held until the new node connects).
    let reply = sealed::seal(&spec, &json!({"token": DUMMY})).unwrap();
    let out = srv.run(&["form", "submit", &reply.text()]);
    assert!(out.status.success(), "{out:?}");
    let waited = srv.ok(&["form", "wait", &spec.form, "--timeout", "30s", "--json"]);
    let waited: Value = serde_json::from_str(&waited).unwrap();
    assert_eq!(waited["state"], "delivered", "{waited}");
    let secrets = dir.path().join(format!("ws/.runs/task-{task}/.secrets"));
    assert_eq!(
        std::fs::read_to_string(secrets.join("token")).unwrap(),
        DUMMY
    );
    // The file kept only the signed spec, and drops the form once used.
    assert!(!contains(&dir.path().join("forms.json"), DUMMY));
    assert!(!contains(&dir.path().join("forms.json"), &spec.form));
}
