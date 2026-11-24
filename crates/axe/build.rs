//! Sets `AXE_VERSION`, the `axe --version` string: the crate version, plus the
//! Git commit when one is known, so a `main` build is distinct from the
//! release it follows. The commit comes from `$AXE_GIT_COMMIT` (the Docker
//! build, which has no `.git`), else from `git rev-parse HEAD` in this source
//! tree (a clone, or a `cargo install --git` checkout). A crates.io build has
//! neither and reports the version alone.
//!
//! It also embeds `skills/axe` (`axe skill install`, `skill.rs`): it writes
//! `$OUT_DIR/skill_files.rs`, the skill's files as `(relative path, bytes)`.
//! A build outside this repository (a crates.io package) has no
//! `skills/axe` and embeds none.
use std::path::{Path, PathBuf};
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
    embed_skill();
}

/// Writes `skill_files.rs`: `SKILL_FILES`, every file under `skills/axe`
/// sorted by its `/`-separated relative path.
fn embed_skill() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../../skills/axe");
    let mut files = Vec::new();
    if root.is_dir() {
        println!("cargo:rerun-if-changed={}", root.display());
        walk(&root, &root, &mut files);
    }
    files.sort();
    let mut out = String::from("pub static SKILL_FILES: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in files {
        out.push_str(&format!("    ({rel:?}, include_bytes!({abs:?})),\n"));
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("skill_files.rs");
    std::fs::write(dest, out).unwrap();
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(root, &path, files);
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap();
            let rel: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
            let abs = path.canonicalize().unwrap();
            files.push((rel.join("/"), abs.to_string_lossy().into_owned()));
        }
    }
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
