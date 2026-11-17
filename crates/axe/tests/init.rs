mod common;

use std::path::Path;
use std::process::Command;

use common::{Server, BIN, TOKEN};

#[test]
fn init_writes_a_starter_config_whose_stub_agent_finishes_a_first_task() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().to_str().unwrap();
    let cfg = d.path().join("conf/axe/config.toml");
    let init = |args: &[&str]| {
        Command::new(BIN)
            .arg("init")
            .args(args)
            .env("AXE_CONFIG", &cfg)
            .env("HOME", home)
            .output()
            .unwrap()
    };
    let out = init(&[]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let demo = d.path().join("workspace/demo");
    assert!(
        stdout.contains(&format!("mkdir -p {}", demo.display())),
        "{stdout}"
    );
    let written = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(
        String::from_utf8(init(&["--print"]).stdout).unwrap(),
        written
    );
    // Never overwrites.
    let again = init(&[]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("exists"));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), written);

    // The printed first task: the stub agent runs `axe` from PATH.
    std::fs::create_dir_all(&demo).unwrap();
    let path = format!(
        "{}:{}",
        Path::new(BIN).parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let env = [("HOME", home), ("PATH", path.as_str())];
    let s = Server::start_with(d.path(), &cfg, Some(TOKEN), &[], &env);
    let id = s.ok(&[
        "add",
        "--project",
        "demo",
        "--title",
        "first task",
        "--agent",
        "stub",
    ]);
    let id = id.trim();
    assert!(s.dispatcher(&["--once"], &env).wait().status.success());
    assert_eq!(s.get(&format!("/api/tasks/{id}"))["state"], "done");
    assert!(s.ok(&["log", id, "--full"]).contains("stub agent ran in"));
}
