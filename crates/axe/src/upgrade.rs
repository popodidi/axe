//! Releases: the once-a-day check for a newer GitHub release ([`Check`]) and
//! `axe upgrade`, which replaces this binary with a release's and then
//! refreshes the skill (`skill.rs`).
//!
//! Releases come from the GitHub REST API: `GET /repos/{owner}/{repo}/releases/latest`
//! (the newest non-prerelease, non-draft release) and
//! `GET /repos/{owner}/{repo}/releases/tags/{tag}`, unauthenticated, reading
//! `tag_name` and `assets[].{name, browser_download_url}`. `$AXE_RELEASE_API`
//! replaces the `https://api.github.com/repos/popodidi/axe` base (tests, mirrors).
//! A release's binaries are `axe-<tag>-<os>-<arch>.tar.gz` ([`asset_name`]),
//! each with `<asset>.sha256` and a line in `SHA256SUMS`.
use std::cmp::Ordering;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::skill::{agent_session, cache_dir, now_secs};

pub const REPO: &str = "popodidi/axe";
const API: &str = "https://api.github.com/repos/popodidi/axe";
/// The running binary's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Seconds a cached release check stays fresh.
const CHECK_EVERY: u64 = 24 * 3600;
/// Largest download `axe upgrade` accepts.
const MAX_DOWNLOAD: u64 = 512 << 20;

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

fn api_base() -> String {
    std::env::var("AXE_RELEASE_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| API.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout(timeout)
        .user_agent(&format!("axe/{VERSION}"))
        .build()
}

/// The latest release, or the release tagged `tag`.
pub fn fetch_release(tag: Option<&str>, timeout: Duration) -> Result<Release> {
    let url = match tag {
        Some(t) => format!("{}/releases/tags/{t}", api_base()),
        None => format!("{}/releases/latest", api_base()),
    };
    match agent(timeout)
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => Ok(r.into_json().with_context(|| format!("read {url}"))?),
        Err(ureq::Error::Status(404, _)) => match tag {
            Some(t) => bail!("no release {t} in {REPO}"),
            None => bail!("{REPO} has no published release"),
        },
        Err(e) => Err(e).with_context(|| format!("GET {url}")),
    }
}

/// A `[v]X.Y.Z[-pre][+build]` version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p.split('.').map(str::to_string).collect()),
            None => (s, vec![]),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let v = [parts.next()??, parts.next()??, parts.next()??];
        if parts.next().is_some() {
            return None;
        }
        Some(Version { core: v, pre })
    }
}

impl Ord for Version {
    /// Semver precedence: a pre-release sorts before its release; numeric
    /// identifiers compare as numbers and before alphanumeric ones.
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether release `tag` is newer than version `running`; false when either
/// does not parse.
pub fn is_newer(tag: &str, running: &str) -> bool {
    match (Version::parse(tag), Version::parse(running)) {
        (Some(t), Some(r)) => t > r,
        _ => false,
    }
}

/// `<os>-<arch>` of this build, as release assets name it.
pub fn platform() -> String {
    // `std::env::consts` names match the asset names: linux, macos;
    // x86_64, aarch64.
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The binary tarball of release `tag` for `platform`.
pub fn asset_name(tag: &str, platform: &str) -> String {
    format!("axe-{tag}-{platform}.tar.gz")
}

/// The from-source install of `tag`, for a platform without a binary.
pub fn cargo_command(tag: &str) -> String {
    format!("cargo install --locked --git https://github.com/{REPO} --tag {tag} axe-os")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cached {
    checked_at: u64,
    latest: Option<String>,
}

fn cache_file() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("release-check.json"))
}

fn read_cache() -> Option<Cached> {
    serde_json::from_slice(&std::fs::read(cache_file()?).ok()?).ok()
}

/// Records a check; a failed one (`latest` None) keeps the last known tag.
fn write_cache(latest: Option<String>) {
    let Some(file) = cache_file() else {
        return;
    };
    let latest = latest.or_else(|| read_cache().and_then(|c| c.latest));
    let c = Cached {
        checked_at: now_secs(),
        latest,
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(b) = serde_json::to_vec(&c) {
        let _ = std::fs::write(file, b);
    }
}

/// Whether the release check runs: stderr is a terminal (`AXE_ASSUME_TTY=1`
/// stands in for one), not an agent session, and not switched off
/// (`[cli] update_check = false` or `AXE_UPDATE_CHECK=0`).
pub fn check_enabled() -> bool {
    let tty =
        std::io::stderr().is_terminal() || std::env::var("AXE_ASSUME_TTY").is_ok_and(|v| v == "1");
    tty && !agent_session() && crate::config::cli_switch("AXE_UPDATE_CHECK", "update_check")
}

/// The once-a-day release check around a command: [`Check::start`] before it
/// (a background fetch when the cache is stale), [`Check::finish`] after it
/// (the hint, waiting at most a second more for the fetch). A network error
/// only skips the hint.
pub enum Check {
    Off,
    Cached(Option<String>),
    Fetching(mpsc::Receiver<Option<String>>),
}

impl Check {
    pub fn start() -> Check {
        if !check_enabled() {
            return Check::Off;
        }
        if let Some(c) = read_cache() {
            if now_secs().saturating_sub(c.checked_at) < CHECK_EVERY {
                return Check::Cached(c.latest);
            }
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let latest = fetch_release(None, Duration::from_secs(3))
                .ok()
                .map(|r| r.tag_name);
            write_cache(latest.clone());
            let _ = tx.send(latest);
        });
        Check::Fetching(rx)
    }

    pub fn finish(self) {
        let latest = match self {
            Check::Off => return,
            Check::Cached(l) => l,
            Check::Fetching(rx) => rx.recv_timeout(Duration::from_secs(1)).ok().flatten(),
        };
        if let Some(tag) = latest.filter(|t| is_newer(t, VERSION)) {
            eprintln!("axe: {tag} is available (running {VERSION}); run `axe upgrade`");
        }
    }
}

/// `axe upgrade`.
pub struct Options {
    /// A release tag (`vX.Y.Z`); default the latest release.
    pub version: Option<String>,
    /// Only report whether an upgrade is available.
    pub check: bool,
    /// On a platform without a release binary, run the `cargo install`.
    pub cargo: bool,
}

pub fn upgrade(opts: &Options) -> Result<()> {
    let tag = opts.version.as_deref().map(|v| {
        if v.starts_with('v') {
            v.to_string()
        } else {
            format!("v{v}")
        }
    });
    let release = fetch_release(tag.as_deref(), Duration::from_secs(30))?;
    let tag = release.tag_name.clone();
    if opts.version.is_none() {
        write_cache(Some(tag.clone()));
    }
    let name = asset_name(&tag, &platform());
    let asset = release.assets.iter().find(|a| a.name == name);
    let newer = is_newer(&tag, VERSION);
    let same = Version::parse(&tag) == Version::parse(VERSION);
    if opts.check {
        let what = if opts.version.is_some() {
            "release"
        } else {
            "latest"
        };
        println!("running {VERSION}, {what} {tag}");
        if newer || (opts.version.is_some() && !same) {
            match asset {
                Some(_) => println!("upgrade available: axe upgrade{}", flag(opts)),
                None => println!(
                    "no {} binary in {tag}; install from source: {}",
                    platform(),
                    cargo_command(&tag)
                ),
            }
        } else {
            println!("up to date");
        }
        return Ok(());
    }
    if same || (!newer && opts.version.is_none()) {
        println!("axe {VERSION} is up to date (release {tag})");
        return Ok(());
    }
    let Some(asset) = asset else {
        let cmd = cargo_command(&tag);
        if !opts.cargo {
            bail!(
                "{tag} has no {} binary; install it from source:\n  {cmd}\n\
                 (or run `axe upgrade{} --cargo`)",
                platform(),
                flag(opts)
            );
        }
        println!("{tag} has no {} binary; running: {cmd}", platform());
        let status = std::process::Command::new("cargo")
            .args(cmd.split(' ').skip(1))
            .status()
            .context("run cargo")?;
        if !status.success() {
            bail!("cargo install failed ({status})");
        }
        println!("installed {tag} with cargo; `axe skill install` refreshes the skill");
        return Ok(());
    };
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locate the running axe binary")?;
    replace(&exe, &release, asset)?;
    println!("upgraded {} from {VERSION} to {tag}", exe.display());
    // The new binary embeds the new skill, so it does the refresh.
    match std::process::Command::new(&exe)
        .args(["skill", "install"])
        .status()
    {
        Ok(s) if s.success() => {}
        _ => eprintln!("axe: the skill was not refreshed (see above); `axe skill install` retries"),
    }
    Ok(())
}

fn flag(opts: &Options) -> String {
    opts.version
        .as_deref()
        .map(|v| format!(" --version {v}"))
        .unwrap_or_default()
}

fn download(url: &str) -> Result<Vec<u8>> {
    let r = agent(Duration::from_secs(300))
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut buf = Vec::new();
    r.into_reader()
        .take(MAX_DOWNLOAD + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("read {url}"))?;
    if buf.len() as u64 > MAX_DOWNLOAD {
        bail!("{url} is larger than {MAX_DOWNLOAD} bytes");
    }
    Ok(buf)
}

/// The expected sha256 of `name`: its `.sha256` asset, else its `SHA256SUMS`
/// line (`<hex>  <name>`).
fn expected_sha256(release: &Release, name: &str) -> Result<String> {
    for (sums, single) in [
        (format!("{name}.sha256"), true),
        ("SHA256SUMS".into(), false),
    ] {
        let Some(a) = release.assets.iter().find(|a| a.name == sums) else {
            continue;
        };
        let text = String::from_utf8(download(&a.browser_download_url)?)
            .with_context(|| format!("{sums} is not text"))?;
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let (Some(hex), file) = (parts.next(), parts.next()) else {
                continue;
            };
            let file = file.map(|f| f.trim_start_matches('*'));
            if file == Some(name) || (single && file.is_none()) {
                let hex = hex.to_ascii_lowercase();
                if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Ok(hex);
                }
            }
        }
    }
    bail!("{} has no checksum for {name}", release.tag_name)
}

/// Downloads and verifies `asset`, then renames its `axe` over `exe`.
fn replace(exe: &Path, release: &Release, asset: &Asset) -> Result<()> {
    let dir = exe.parent().context("the axe binary has no directory")?;
    let work = dir.join(format!(".axe-upgrade-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir(&work) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            bail!(
                "cannot replace {}: {} is not writable; rerun with access to it, or reinstall",
                exe.display(),
                dir.display()
            );
        }
        return Err(e).with_context(|| format!("create {}", work.display()));
    }
    let result = (|| -> Result<()> {
        let want = expected_sha256(release, &asset.name)?;
        let bytes = download(&asset.browser_download_url)?;
        let have: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if have != want {
            bail!(
                "checksum mismatch for {}: expected {want}, got {have}; the binary was not replaced",
                asset.name
            );
        }
        let tarball = work.join(&asset.name);
        std::fs::write(&tarball, &bytes)?;
        let out = work.join("x");
        std::fs::create_dir(&out)?;
        let status = std::process::Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&out)
            .arg("axe")
            .status()
            .context("run tar")?;
        let new = out.join("axe");
        if !status.success() || !new.is_file() {
            bail!("{} has no axe binary", asset.name);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        }
        let ran = std::process::Command::new(&new)
            .arg("--version")
            .output()
            .context("run the downloaded axe")?;
        if !ran.status.success() {
            bail!("the downloaded axe does not run on this machine");
        }
        std::fs::rename(&new, exe).with_context(|| format!("replace {}", exe.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_semver_precedence() {
        assert!(is_newer("v0.6.0", "0.5.0"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.5.0", "0.5.0"));
        assert!(!is_newer("v0.4.9", "0.5.0"));
        // Pre-releases sort before their release.
        assert!(is_newer("v0.6.0", "0.6.0-rc.1"));
        assert!(!is_newer("v0.6.0-rc.1", "0.6.0"));
        assert!(is_newer("v0.6.0-rc.1", "0.5.0"));
        assert!(is_newer("v0.6.0-rc.2", "0.6.0-rc.1"));
        assert!(is_newer("v0.6.0-rc.10", "0.6.0-rc.9"));
        assert!(is_newer("v0.6.0-rc.1", "0.6.0-beta.3"));
        assert!(is_newer("v0.6.0-rc.1.1", "0.6.0-rc.1"));
        assert!(is_newer("v0.6.0-alpha", "0.6.0-1"));
        // Build metadata is ignored; garbage never compares newer.
        assert!(!is_newer("v0.5.0+abc", "0.5.0"));
        assert!(!is_newer("nightly", "0.5.0"));
        assert!(!is_newer("v1.2", "0.5.0"));
        assert!(!is_newer("v1.0.0", "dev"));
    }

    #[test]
    fn assets_are_named_by_tag_and_platform() {
        assert_eq!(
            asset_name("v0.6.0", "macos-aarch64"),
            "axe-v0.6.0-macos-aarch64.tar.gz"
        );
        assert_eq!(
            asset_name("v0.6.0", "linux-x86_64"),
            "axe-v0.6.0-linux-x86_64.tar.gz"
        );
    }
}
