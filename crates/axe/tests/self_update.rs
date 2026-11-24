//! `axe skill install`, the skill auto-refresh, the release check and `axe
//! upgrade`, each in a temporary HOME against a local mock release server.
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const BIN: &str = env!("CARGO_BIN_EXE_axe");

fn repo_skill() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/axe")
}

/// An isolated HOME, cache and config.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Home {
        let h = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(h.path().join(".claude")).unwrap();
        h
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn skill(&self) -> PathBuf {
        self.path().join(".agents/skills/axe")
    }
    fn cmd(&self, bin: &Path) -> Command {
        let mut c = Command::new(bin);
        c.env("HOME", self.path())
            .env("XDG_CACHE_HOME", self.path().join(".cache"))
            .env("AXE_CONFIG", self.path().join("config.toml"))
            .env("AXE_RELEASE_API", "http://127.0.0.1:1")
            .env_remove("AXE_TASK")
            .env_remove("AXE_BOT")
            .env_remove("AXE_SKILL_REFRESH")
            .env_remove("AXE_UPDATE_CHECK")
            .env_remove("AXE_ASSUME_TTY");
        c
    }
    fn axe(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.run(Path::new(BIN), args, env)
    }
    fn run(&self, bin: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd(bin);
        c.args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        output(&mut c)
    }
}

/// Runs `c`, retrying a spawn that races another test's copy of a binary
/// still open for writing (ETXTBSY).
fn output(c: &mut Command) -> Output {
    for _ in 0..50 {
        match c.output() {
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            r => return r.unwrap(),
        }
    }
    panic!("binary stayed busy");
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Every file under `dir` as relative path -> bytes.
fn files(dir: &Path) -> HashMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut HashMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = HashMap::new();
    walk(dir, dir, &mut out);
    out
}

/// A command that needs no server and succeeds.
const ANY: &[&str] = &["init", "--print"];

#[test]
fn install_writes_the_skill_stamp_and_link_and_is_idempotent() {
    let h = Home::new();
    let o = h.axe(&["skill", "install"], &[]);
    assert!(o.status.success(), "{o:?}");
    let mut got = files(&h.skill());
    let stamp: serde_json::Value =
        serde_json::from_slice(&got.remove(".axe-skill.json").unwrap()).unwrap();
    assert_eq!(got, files(&repo_skill()));
    assert_eq!(stamp["build"], axe::skill::BUILD);
    assert_eq!(stamp["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        stamp["sha256"],
        axe::skill::tree_hash(&h.skill()).unwrap().as_str()
    );
    let link = h.path().join(".claude/skills/axe");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("../../.agents/skills/axe")
    );
    assert_eq!(
        link.canonicalize().unwrap(),
        h.skill().canonicalize().unwrap()
    );

    let before = files(&h.skill());
    let again = h.axe(&["skill", "install"], &[]);
    assert!(again.status.success(), "{again:?}");
    assert!(stdout(&again).contains("up to date"), "{again:?}");
    assert_eq!(files(&h.skill()), before);
    let status = h.axe(&["skill", "status"], &[]);
    assert!(stdout(&status).contains("up to date"), "{status:?}");
}

#[test]
fn install_honors_dir_and_leaves_a_real_claude_dir_alone() {
    let h = Home::new();
    let real = h.path().join(".claude/skills/axe");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("SKILL.md"), "mine").unwrap();
    let dir = h.path().join("elsewhere/axe");
    let o = h.axe(&["skill", "install", "--dir", dir.to_str().unwrap()], &[]);
    assert!(o.status.success(), "{o:?}");
    assert!(dir.join("SKILL.md").is_file());
    assert!(!h.skill().exists());
    assert!(stdout(&o).contains("not a symlink; left alone"), "{o:?}");
    assert_eq!(
        std::fs::read_to_string(real.join("SKILL.md")).unwrap(),
        "mine"
    );
}

#[test]
fn install_refuses_an_edited_or_unstamped_copy_without_force() {
    let h = Home::new();
    std::fs::create_dir_all(h.skill()).unwrap();
    std::fs::write(h.skill().join("SKILL.md"), "hand copied").unwrap();
    let o = h.axe(&["skill", "install"], &[]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("axe skill install --force"), "{o:?}");
    assert_eq!(
        std::fs::read_to_string(h.skill().join("SKILL.md")).unwrap(),
        "hand copied"
    );
    let o = h.axe(&["skill", "install", "--force"], &[]);
    assert!(o.status.success(), "{o:?}");
    let mut got = files(&h.skill());
    got.remove(".axe-skill.json");
    assert_eq!(got, files(&repo_skill()));
}

/// Makes the installed copy look written by an older binary: other content,
/// stamped as such.
fn make_stale(h: &Home) {
    let skill = h.skill();
    std::fs::write(skill.join("SKILL.md"), "old skill\n").unwrap();
    std::fs::write(skill.join("removed.md"), "gone in the new skill\n").unwrap();
    let stamp = serde_json::json!({
        "version": "0.0.1",
        "build": "0.0.1 (000000000000)",
        "sha256": axe::skill::tree_hash(&skill).unwrap(),
    });
    std::fs::write(skill.join(".axe-skill.json"), stamp.to_string()).unwrap();
}

#[test]
fn any_command_refreshes_a_stale_stamped_install() {
    let h = Home::new();
    assert!(h.axe(&["skill", "install"], &[]).status.success());
    make_stale(&h);
    let o = h.axe(ANY, &[]);
    assert!(o.status.success(), "{o:?}");
    assert!(stderr(&o).contains("axe: updated the skill at"), "{o:?}");
    assert_eq!(stderr(&o).lines().count(), 1, "{o:?}");
    let mut got = files(&h.skill());
    got.remove(".axe-skill.json");
    assert_eq!(got, files(&repo_skill()));
    // Current now: the next command is silent.
    let o = h.axe(ANY, &[]);
    assert_eq!(stderr(&o), "");
}

#[test]
fn auto_refresh_leaves_edited_and_unstamped_copies_alone_with_a_hint() {
    // Hand-edited since stamping.
    let h = Home::new();
    assert!(h.axe(&["skill", "install"], &[]).status.success());
    let skill_md = h.skill().join("SKILL.md");
    let edited = format!("{}\nmy note\n", std::fs::read_to_string(&skill_md).unwrap());
    std::fs::write(&skill_md, &edited).unwrap();
    let o = h.axe(ANY, &[]);
    assert!(o.status.success(), "{o:?}");
    assert!(
        stderr(&o).contains("was edited since axe installed it"),
        "{o:?}"
    );
    assert!(stderr(&o).contains("axe skill install --force"), "{o:?}");
    assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), edited);
    // At most once a day for the same state.
    assert_eq!(stderr(&h.axe(ANY, &[])), "");
    // Never in an agent session.
    std::fs::write(&skill_md, "edited again").unwrap();
    assert_eq!(stderr(&h.axe(ANY, &[("AXE_TASK", "7")])), "");

    // Unstamped.
    let h = Home::new();
    std::fs::create_dir_all(h.skill()).unwrap();
    std::fs::write(h.skill().join("SKILL.md"), "hand copied").unwrap();
    let o = h.axe(ANY, &[]);
    assert!(stderr(&o).contains("was not installed by axe"), "{o:?}");
    assert!(stderr(&o).contains("axe skill install --force"), "{o:?}");
    assert_eq!(
        std::fs::read_to_string(h.skill().join("SKILL.md")).unwrap(),
        "hand copied"
    );
}

#[test]
fn auto_refresh_switches_off_by_env_and_config() {
    for (env, config) in [
        (Some("0"), None),
        (Some("false"), None),
        (None, Some("[cli]\nskill_refresh = false\n")),
    ] {
        let h = Home::new();
        assert!(h.axe(&["skill", "install"], &[]).status.success());
        make_stale(&h);
        let before = files(&h.skill());
        if let Some(c) = config {
            std::fs::write(h.path().join("config.toml"), c).unwrap();
        }
        let env: Vec<_> = env.map(|v| ("AXE_SKILL_REFRESH", v)).into_iter().collect();
        let o = h.axe(ANY, &env);
        assert!(o.status.success(), "{o:?}");
        assert_eq!(stderr(&o), "", "{env:?} {config:?}");
        assert_eq!(files(&h.skill()), before, "{env:?} {config:?}");
    }
    // The [cli] section is valid config.
    let h = Home::new();
    std::fs::write(
        h.path().join("config.toml"),
        "[cli]\nskill_refresh = false\nupdate_check = false\n",
    )
    .unwrap();
    let o = h.axe(&["config"], &[]);
    assert!(o.status.success(), "{o:?}");
}

/// A mock release server: fixed responses by path, counting requests.
struct Mock {
    url: String,
    hits: Arc<AtomicUsize>,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let (h, r) = (hits.clone(), routes.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut reader = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                h.fetch_add(1, Ordering::SeqCst);
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = r.lock().unwrap().get(&path).cloned();
                let (status, body) = match body {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", b"{}".to_vec()),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(&body);
            }
        });
        Mock { url, hits, routes }
    }
    fn route(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(path.into(), body.into());
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
    /// Serves a release `tag` at /releases/latest and /releases/tags/<tag>,
    /// with `assets` (name -> bytes) under /dl/.
    fn release(&self, tag: &str, assets: &[(String, Vec<u8>)]) {
        let list: Vec<_> = assets
            .iter()
            .map(|(name, bytes)| {
                self.route(&format!("/dl/{name}"), bytes.clone());
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("{}/dl/{name}", self.url),
                })
            })
            .collect();
        let body = serde_json::json!({"tag_name": tag, "prerelease": false, "assets": list});
        self.route("/releases/latest", body.to_string());
        self.route(&format!("/releases/tags/{tag}"), body.to_string());
    }
}

const TTY: (&str, &str) = ("AXE_ASSUME_TTY", "1");

#[test]
fn release_check_hints_once_a_day_and_honors_the_cache() {
    let m = Mock::start();
    m.release("v99.0.0", &[]);
    let h = Home::new();
    let api = ("AXE_RELEASE_API", m.url.as_str());
    let o = h.axe(ANY, &[api, TTY]);
    assert!(o.status.success(), "{o:?}");
    assert!(
        stderr(&o).contains("axe: v99.0.0 is available (running"),
        "{o:?}"
    );
    assert_eq!(m.hits(), 1);
    // The cached result: the hint again, no request.
    let o = h.axe(ANY, &[api, TTY]);
    assert!(stderr(&o).contains("v99.0.0 is available"), "{o:?}");
    assert_eq!(m.hits(), 1);
    // An expired cache checks again.
    let cache = h.path().join(".cache/axe/release-check.json");
    std::fs::write(&cache, r#"{"checked_at":0,"latest":"v99.0.0"}"#).unwrap();
    h.axe(ANY, &[api, TTY]);
    assert_eq!(m.hits(), 2);
}

#[test]
fn release_check_is_silent_for_equal_or_older_releases() {
    for tag in [format!("v{}", env!("CARGO_PKG_VERSION")), "v0.0.1".into()] {
        let m = Mock::start();
        m.release(&tag, &[]);
        let h = Home::new();
        let o = h.axe(ANY, &[("AXE_RELEASE_API", &m.url), TTY]);
        assert_eq!(m.hits(), 1);
        assert_eq!(stderr(&o), "", "{tag}");
    }
}

#[test]
fn release_check_is_silent_off_a_terminal_in_agent_sessions_and_when_off() {
    type Case<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>);
    let cases: &[Case] = &[
        (&[], None),
        (&[TTY, ("AXE_TASK", "12")], None),
        (&[TTY, ("AXE_BOT", "helper")], None),
        (&[TTY, ("AXE_UPDATE_CHECK", "0")], None),
        (&[TTY], Some("[cli]\nupdate_check = false\n")),
    ];
    for (env, config) in cases {
        let m = Mock::start();
        m.release("v99.0.0", &[]);
        let h = Home::new();
        if let Some(c) = config {
            std::fs::write(h.path().join("config.toml"), c).unwrap();
        }
        let mut env = env.to_vec();
        env.push(("AXE_RELEASE_API", &m.url));
        let o = h.axe(ANY, &env);
        assert!(o.status.success(), "{o:?}");
        assert_eq!(stderr(&o), "", "{env:?}");
        assert_eq!(m.hits(), 0, "{env:?}");
    }
}

#[test]
fn a_failed_release_check_never_changes_the_exit_code() {
    let h = Home::new();
    // Nothing listens on port 1.
    let down = [("AXE_RELEASE_API", "http://127.0.0.1:1"), TTY];
    let off = [("AXE_UPDATE_CHECK", "0")];
    let o = h.axe(ANY, &down);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert_eq!(stderr(&o), "");
    // A failing command fails the same with or without the check.
    std::fs::write(h.path().join("config.toml"), "not = [valid").unwrap();
    std::fs::remove_file(h.path().join(".cache/axe/release-check.json")).ok();
    let failed = h.axe(&["config"], &down);
    let plain = h.axe(&["config"], &off);
    assert!(!plain.status.success());
    assert_eq!(failed.status.code(), plain.status.code());
}

/// A release tarball whose `axe` is a script printing `marker`.
fn fake_tarball(marker: &str) -> Vec<u8> {
    let d = tempfile::tempdir().unwrap();
    let script = d.path().join("axe");
    std::fs::write(&script, format!("#!/bin/sh\necho {marker}\n")).unwrap();
    let tgz = d.path().join("out.tar.gz");
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(&tgz)
        .arg("-C")
        .arg(d.path())
        .arg("axe")
        .status()
        .unwrap()
        .success();
    assert!(ok);
    std::fs::read(tgz).unwrap()
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// A copy of the axe binary in its own directory, to upgrade in place.
fn copy_bin(h: &Home) -> PathBuf {
    let dir = h.path().join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let exe = dir.join("axe");
    std::fs::copy(BIN, &exe).unwrap();
    exe
}

fn leftovers(exe: &Path) -> Vec<String> {
    std::fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "axe")
        .collect()
}

#[test]
fn upgrade_replaces_the_binary_when_the_checksum_matches() {
    let m = Mock::start();
    let tag = "v99.0.0";
    let name = axe::upgrade::asset_name(tag, &axe::upgrade::platform());
    let tgz = fake_tarball("upgraded-axe");
    let sum = format!("{}  {name}\n", sha256_hex(&tgz));
    m.release(
        tag,
        &[
            (name.clone(), tgz),
            (format!("{name}.sha256"), sum.clone().into_bytes()),
            ("SHA256SUMS".into(), sum.into_bytes()),
        ],
    );
    let h = Home::new();
    let exe = copy_bin(&h);
    let api = ("AXE_RELEASE_API", m.url.as_str());

    let check = h.run(&exe, &["upgrade", "--check"], &[api]);
    assert!(check.status.success(), "{check:?}");
    assert!(stdout(&check).contains("latest v99.0.0"), "{check:?}");
    assert!(stdout(&check).contains("upgrade available"), "{check:?}");
    assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(BIN).unwrap());

    let o = h.run(&exe, &["upgrade"], &[api]);
    assert!(o.status.success(), "{o:?}");
    assert!(stdout(&o).contains("to v99.0.0"), "{o:?}");
    // The new binary ran the skill refresh.
    assert!(stdout(&o).contains("upgraded-axe"), "{o:?}");
    let new = output(&mut Command::new(&exe));
    assert_eq!(stdout(&new).trim(), "upgraded-axe");
    assert_eq!(leftovers(&exe), Vec::<String>::new());
}

#[test]
fn upgrade_with_a_bad_checksum_keeps_the_old_binary() {
    let m = Mock::start();
    let tag = "v99.0.0";
    let name = axe::upgrade::asset_name(tag, &axe::upgrade::platform());
    let sum = format!("{}  {name}\n", "0".repeat(64));
    m.release(
        tag,
        &[
            (name.clone(), fake_tarball("bad")),
            ("SHA256SUMS".into(), sum.into_bytes()),
        ],
    );
    let h = Home::new();
    let exe = copy_bin(&h);
    let o = h.run(&exe, &["upgrade"], &[("AXE_RELEASE_API", &m.url)]);
    assert!(!o.status.success(), "{o:?}");
    assert!(stderr(&o).contains("checksum mismatch"), "{o:?}");
    assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(BIN).unwrap());
    assert_eq!(leftovers(&exe), Vec::<String>::new());
}

#[test]
fn upgrade_without_a_platform_asset_gives_the_cargo_command() {
    let m = Mock::start();
    m.release(
        "v99.0.0",
        &[("axe-v99.0.0-plan9-mips.tar.gz".into(), b"x".to_vec())],
    );
    let h = Home::new();
    let exe = copy_bin(&h);
    let api = ("AXE_RELEASE_API", m.url.as_str());
    let cmd = "cargo install --locked --git https://github.com/popodidi/axe --tag v99.0.0 axe-os";
    let o = h.run(&exe, &["upgrade"], &[api]);
    assert!(!o.status.success(), "{o:?}");
    assert!(stderr(&o).contains(cmd), "{o:?}");
    assert!(stderr(&o).contains("--cargo"), "{o:?}");
    let check = h.run(&exe, &["upgrade", "--check"], &[api]);
    assert!(stdout(&check).contains(cmd), "{check:?}");
    assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(BIN).unwrap());
}

#[test]
fn upgrade_to_a_named_version_and_when_current() {
    let m = Mock::start();
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    m.release(&current, &[]);
    let h = Home::new();
    let exe = copy_bin(&h);
    let api = ("AXE_RELEASE_API", m.url.as_str());
    let o = h.run(&exe, &["upgrade"], &[api]);
    assert!(o.status.success(), "{o:?}");
    assert!(stdout(&o).contains("up to date"), "{o:?}");
    let o = h.run(&exe, &["upgrade", "--version", "v98.0.0"], &[api]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("no release v98.0.0"), "{o:?}");
}

#[cfg(unix)]
#[test]
fn upgrade_refuses_a_binary_it_cannot_replace() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    let m = Mock::start();
    let tag = "v99.0.0";
    let name = axe::upgrade::asset_name(tag, &axe::upgrade::platform());
    let tgz = fake_tarball("x");
    let sum = format!("{}  {name}\n", sha256_hex(&tgz));
    m.release(tag, &[(name, tgz), ("SHA256SUMS".into(), sum.into_bytes())]);
    let h = Home::new();
    let exe = copy_bin(&h);
    let dir = exe.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let o = h.run(&exe, &["upgrade"], &[("AXE_RELEASE_API", &m.url)]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!o.status.success(), "{o:?}");
    assert!(stderr(&o).contains("is not writable"), "{o:?}");
    assert_eq!(std::fs::read(&exe).unwrap(), std::fs::read(BIN).unwrap());
}
