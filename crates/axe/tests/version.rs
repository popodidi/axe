use std::process::Command;

/// `axe --version` is the crate version, plus the commit the build saw
/// (`build.rs`): `$AXE_GIT_COMMIT`, else HEAD of this source tree.
#[test]
fn version_names_the_build_commit() {
    let out = Command::new(env!("CARGO_BIN_EXE_axe"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let got = String::from_utf8(out.stdout).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("AXE_GIT_COMMIT")
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let o = Command::new("git")
                .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
                .output()
                .ok()?;
            o.status
                .success()
                .then(|| String::from_utf8(o.stdout).unwrap().trim().to_string())
        });
    let want = match commit {
        Some(c) => format!("axe {version} ({})\n", &c[..c.len().min(12)]),
        None => format!("axe {version}\n"),
    };
    assert_eq!(got, want);
}
