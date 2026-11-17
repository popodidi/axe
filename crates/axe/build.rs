//! Sets `AXE_VERSION`, the `axe --version` string: the crate version, plus the
//! Git commit when one is known, so a `main` build is distinct from the
//! release it follows. The commit comes from `$AXE_GIT_COMMIT` (the Docker
//! build, which has no `.git`), else from `git rev-parse HEAD` in this source
//! tree (a clone, or a `cargo install --git` checkout). A crates.io build has
//! neither and reports the version alone.
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=AXE_GIT_COMMIT");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let commit = std::env::var("AXE_GIT_COMMIT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .or_else(git_head);
    let full = match commit {
        Some(c) => format!("{version} ({})", &c[..c.len().min(12)]),
        None => version,
    };
    println!("cargo:rustc-env=AXE_VERSION={full}");
}

fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// HEAD's commit, and rerun triggers for the files that move it.
fn git_head() -> Option<String> {
    let head = git(&["rev-parse", "--verify", "-q", "HEAD"])?;
    let mut watch = vec!["HEAD".to_string(), "packed-refs".to_string()];
    watch.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for name in watch {
        if let Some(p) = git(&["rev-parse", "--path-format=absolute", "--git-path", &name]) {
            if Path::new(&p).exists() {
                println!("cargo:rerun-if-changed={p}");
            }
        }
    }
    Some(head)
}
