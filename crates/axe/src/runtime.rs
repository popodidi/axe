//! Runtimes: a harness plus exactly one login, held by the node.
//!
//! With `[runtimes] dir` set, each `<dir>/<name>/runtime.toml` is one runtime:
//! its harness, the `[agents.*]` command template that drives it and the login
//! directory the CLI keeps its credentials in. The node never reads,
//! copies or writes the login itself: it only points the CLI at its directory
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), and the CLI's own default directory is
//! adopted in place by setting nothing. Each runtime's health (ok, limited
//! until a time, needs_login, degraded, disabled) lives in `<name>/state.json`
//! next to it; a run's failure class (`failure.rs`) moves it, and the server
//! sees it only through [`Registry::heartbeat`], which carries no credential,
//! path or token.
//!
//! Without `[runtimes]`, every `[agents.*]` entry is a runtime of its own on
//! the node's default login, always healthy: selection and retries work
//! as before.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{expand_tilde, Config};
use crate::failure::{Failure, AUTH_INVALID, PROVIDER_UNAVAILABLE, QUOTA_EXHAUSTED};

pub const OK: &str = "ok";
pub const LIMITED: &str = "limited";
pub const NEEDS_LOGIN: &str = "needs_login";
pub const DEGRADED: &str = "degraded";
pub const DISABLED: &str = "disabled";

/// Alert classes of a login check right after a login ([`Registry::check_now`]).
pub const LOGIN_OK: &str = "login_ok";
pub const LOGIN_FAILED: &str = "login_failed";
/// Alert class and state of a runtime `/runtimes` Add made that was removed
/// again: its first login failed its check, or none came within
/// [`ADD_LOGIN_WAIT`].
pub const ADD_REMOVED: &str = "add_removed";
pub const REMOVED: &str = "removed";
/// How long a runtime `/runtimes` Add made waits for its first login
/// before it is removed: the longest sealed form, plus a margin for its
/// login check.
pub const ADD_LOGIN_WAIT: i64 = crate::sealed::MAX_TTL + 600;

/// Longest a web login (`codex login --device-auth`, `claude auth login`)
/// may wait for the owner before it is stopped: the longest sealed form.
pub const DEVICE_LOGIN_MAX: Duration = Duration::from_secs(crate::sealed::MAX_TTL as u64);

/// A web login in progress ([`Runtime::device_login`]).
pub struct DeviceLogin {
    /// What the CLI printed first: the URL, and Codex's one-time code.
    pub output: String,
    /// The CLI's stdin when it waits for the code the page shows (Claude):
    /// the code goes there, followed by a newline.
    pub code: Option<std::process::ChildStdin>,
}

/// How long a runtime stays limited when the quota reset time is unknown.
pub const LIMITED_DEFAULT_SECONDS: i64 = 3600;
/// How often a runtime that needs a login is probed (`probe`).
pub const PROBE_EVERY: Duration = Duration::from_secs(300);
/// Longest a probe may run before it counts as failed.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest failure message kept (as `failure::Failure::to_json`).
const MAX_MESSAGE: usize = 200;

/// `[runtimes]` in the node config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    /// The registry: one `<name>/runtime.toml` per runtime.
    pub dir: String,
}

/// `<dir>/<name>/runtime.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    harness: String,
    /// The `[agents.*]` command template; default: the only agent of `harness`.
    agent: Option<String>,
    /// Where the CLI keeps this runtime's login; default: the CLI's own
    /// default directory, adopted in place.
    login_dir: Option<String>,
    /// A label for people (`max`, `pro`, `api`); shown, never used.
    plan: Option<String>,
    #[serde(default)]
    disabled: bool,
    /// Command that exits 0 while the login is valid; default per harness.
    probe: Option<Vec<String>>,
}

/// A run's failure, as recorded on its runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastFailure {
    pub class: String,
    pub message: String,
    /// Unix seconds.
    pub at: i64,
}

/// Persisted runtime health (`state.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub state: String,
    /// Unix seconds when `limited` or `degraded` ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<LastFailure>,
    /// The harness's latest quota report (Claude Code `rate_limit_event`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Value>,
    /// The state the owner was last sent a card for; cleared back at `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alerted: Option<String>,
    /// Unix seconds when the latest passing login probe started: runs spawned
    /// before it may hold an older credential, so their login failures do not
    /// mark the runtime ([`Registry::mark`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_at: Option<i64>,
    /// Unix seconds when `/runtimes` Add made this runtime; cleared by its
    /// first passing login. A runtime still waiting for that login is
    /// removed when the login fails its check or after [`ADD_LOGIN_WAIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<i64>,
}

impl Runtime {
    /// Made by `/runtimes` and still waiting for its first login: it takes
    /// no run and is not shown as a runtime yet (login first).
    pub fn pending(&self) -> bool {
        self.health.added_at.is_some() && self.health.never_logged_in()
    }
}

impl Health {
    /// Needs a login and never passed one: no login check passed and no run
    /// failed on it. Shown as "not logged in" with a Log in button.
    pub fn never_logged_in(&self) -> bool {
        self.state == NEEDS_LOGIN && self.login_at.is_none() && self.last_failure.is_none()
    }
}

impl Default for Health {
    fn default() -> Self {
        Health {
            state: OK.into(),
            until: None,
            last_failure: None,
            quota: None,
            alerted: None,
            login_at: None,
            added_at: None,
        }
    }
}

/// The node's registry as its main loop last saw it (`managed`, runtimes),
/// for the sealed form desk: one registry per node, owned by the loop.
/// `None` until the loop has published it.
pub type Snapshot = std::sync::Arc<std::sync::Mutex<Option<(bool, Vec<Runtime>)>>>;

#[derive(Debug, Clone)]
pub struct Runtime {
    pub name: String,
    pub harness: String,
    /// The `[agents.*]` template it runs.
    pub agent: String,
    /// `None`: the CLI's default login directory.
    login_dir: Option<PathBuf>,
    pub plan: Option<String>,
    pub disabled: bool,
    probe: Vec<String>,
    /// `state.json`; `None` for implicit runtimes (nothing is persisted).
    state_path: Option<PathBuf>,
    /// `<name>/login/`: credentials a sealed form login wrote (0600 files
    /// named by the variable the CLI reads); `None` for implicit runtimes.
    login_secrets: Option<PathBuf>,
    pub health: Health,
}

/// A card the owner should get: the runtime turned `state`.
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub runtime: String,
    pub harness: String,
    pub state: String,
    pub class: String,
    pub message: String,
    pub until: Option<i64>,
}

impl Alert {
    pub fn to_json(&self) -> Value {
        json!({"runtime":self.runtime,"harness":self.harness,"state":self.state,
               "class":self.class,"message":self.message,"until":self.until})
    }
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Variables a runtime sets and removes in its CLI's environment.
pub type LoginEnv = (Vec<(&'static str, String)>, Vec<&'static str>);

/// The CLI's login directory variable and default directory, per harness.
fn login_var(harness: &str) -> Option<(&'static str, &'static str)> {
    crate::harness::get(harness).map(|h| h.login_var)
}

/// How each harness can be logged in ([`crate::harness::Harness::logins`]).
pub fn login_methods(
    harness: &str,
) -> &'static [(&'static str, Option<&'static str>, &'static str)] {
    crate::harness::get(harness).map_or(&[], |h| h.logins)
}

/// The format a login method's credential has, for the sealed form and
/// [`Runtime::login`]: the CLI would take a malformed one and only fail on
/// a later run.
pub fn login_check(harness: &str, method: &str) -> Option<crate::sealed::Check> {
    let (_, prefix, min, max) = crate::harness::get(harness)?
        .login_checks
        .iter()
        .find(|(m, ..)| *m == method)?;
    Some(crate::sealed::Check {
        prefix: (*prefix).into(),
        min: Some(*min),
        max: Some(*max),
        chars: Some(crate::sealed::Chars::Nospace),
    })
}

/// Exits 0 while logged in: `claude auth status`, `codex login status`.
fn default_probe(harness: &str) -> Vec<String> {
    crate::harness::get(harness)
        .map_or(&[][..], |h| h.probe)
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl Runtime {
    /// The CLI's login directory, which also holds its session transcripts;
    /// `None` for a harness axe does not know.
    pub fn login_root(&self) -> Option<PathBuf> {
        let (_, default) = login_var(&self.harness)?;
        Some(
            self.login_dir
                .clone()
                .unwrap_or_else(|| expand_tilde(default)),
        )
    }

    /// Healthy at `now`: ok, or limited/degraded past its end.
    pub fn healthy(&self, now: i64) -> bool {
        match self.health.state.as_str() {
            OK => !self.disabled,
            LIMITED | DEGRADED => !self.disabled && self.health.until.is_none_or(|u| u <= now),
            _ => false,
        }
    }

    /// The state shown at `now` (a lapsed limit shows as ok).
    pub fn state(&self, now: i64) -> &str {
        if self.disabled {
            return DISABLED;
        }
        match self.health.state.as_str() {
            LIMITED | DEGRADED if self.healthy(now) => OK,
            s => s,
        }
    }

    /// Variables to set and to remove in the CLI's environment: a runtime on
    /// the CLI's default directory removes the variable, so the CLI finds its
    /// login exactly where it always has.
    pub fn env(&self) -> LoginEnv {
        let Some((var, _)) = login_var(&self.harness) else {
            return (vec![], vec![]);
        };
        let (mut set, remove) = match &self.login_dir {
            Some(d) => (vec![(var, d.display().to_string())], vec![]),
            None => (vec![], vec![var]),
        };
        // A credential from a sealed form login: the CLI reads it from the
        // environment, ahead of its login directory.
        if let Some(dir) = &self.login_secrets {
            for (_, v, _) in login_methods(&self.harness) {
                let Some(v) = v else { continue };
                if let Ok(value) = std::fs::read_to_string(dir.join(v)) {
                    set.push((v, value.trim().to_string()));
                }
            }
        }
        (set, remove)
    }

    /// Logs this runtime in with `value` by `method` ([`login_methods`]):
    /// stores the credential the CLI reads from the environment (replacing
    /// the other one), or runs the CLI's own login with the value on stdin.
    /// The value is never in an argument, a log or an error.
    pub fn login(&self, cfg: &Config, method: &str, value: &str) -> Result<()> {
        let Some(dir) = &self.login_secrets else {
            bail!("runtime {} is not in the runtime registry", self.name);
        };
        let Some((_, var, _)) = login_methods(&self.harness)
            .iter()
            .find(|(m, _, _)| *m == method)
        else {
            bail!(
                "runtime {} ({}) has no {method} login",
                self.name,
                self.harness
            );
        };
        if method == "device" {
            bail!("a device code login takes no value");
        }
        if value.is_empty() {
            bail!("the credential is empty");
        }
        if let Some(p) = login_check(&self.harness, method).and_then(|c| c.problem(value)) {
            bail!("the {method} {p}");
        }
        if value.contains(['\n', '\r', '\0']) {
            bail!("the credential must be one line");
        }
        match var {
            Some(var) => {
                crate::sealed::write_private_file(&dir.join(var), value.as_bytes())?;
                for (_, other, _) in login_methods(&self.harness) {
                    if let Some(o) = other.filter(|o| o != var) {
                        let _ = std::fs::remove_file(dir.join(o));
                    }
                }
                Ok(())
            }
            None => {
                let args = crate::harness::get(&self.harness).map_or(&[][..], |h| h.stdin_login);
                self.cli_login(cfg, args, value)
            }
        }
    }

    /// Starts the CLI's web login and returns what it printed in the first
    /// `wait` (ANSI codes removed), with the login's stdin when it reads a
    /// code there. `codex login --device-auth` prints a URL and a one-time
    /// code the owner enters on the page; `claude auth login` prints a
    /// claude.ai URL and reads the code that page shows from stdin
    /// ([`DeviceLogin::code`]). The login keeps running in the background
    /// until it ends or [`DEVICE_LOGIN_MAX`] passes; `done` gets its outcome.
    /// A passing Claude login removes the runtime's stored token and API key,
    /// which Claude Code would read ahead of the new login.
    pub fn device_login(
        &self,
        cfg: &Config,
        wait: Duration,
        done: impl FnOnce(bool) + Send + 'static,
    ) -> Result<DeviceLogin> {
        if !login_methods(&self.harness)
            .iter()
            .any(|(m, _, _)| *m == "device")
        {
            bail!(
                "runtime {} ({}) has no device code login",
                self.name,
                self.harness
            );
        }
        let Some(secrets) = self.login_secrets.clone() else {
            bail!("runtime {} is not in the runtime registry", self.name);
        };
        let (args, reads_code) =
            crate::harness::get(&self.harness).map_or((&[][..], false), |h| h.device_login);
        let stdin = match reads_code {
            true => Stdio::piped(),
            false => Stdio::null(),
        };
        let mut cmd = self.cli(cfg, args)?;
        if reads_code {
            // No browser on the node: the URL goes to the owner.
            cmd.env("BROWSER", "/bin/false");
        }
        let mut child = cmd
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("runtime {}: start the device login", self.name))?;
        let code = child.stdin.take();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let end = Instant::now() + wait;
        let mut lines = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(l) => {
                    let l = strip_ansi(&l);
                    if !l.trim().is_empty() && lines.len() < 20 {
                        lines.push(l.trim_end().to_string());
                    }
                }
                Err(_) => break,
            }
        }
        let name = self.name.clone();
        // A passing login replaces the credentials stored for the CLI's
        // environment (Claude Code reads them ahead of its new login).
        let stale: Vec<&'static str> = login_methods(&self.harness)
            .iter()
            .filter_map(|(_, v, _)| *v)
            .collect();
        let printed = !lines.is_empty();
        std::thread::spawn(move || {
            // Drain the rest so the CLI never blocks on a full pipe.
            let drain = std::thread::spawn(move || for _ in rx {});
            let end = Instant::now() + DEVICE_LOGIN_MAX;
            let ok = loop {
                match child.try_wait() {
                    Ok(Some(s)) => break s.success(),
                    Ok(None) if printed && Instant::now() < end => {
                        std::thread::sleep(Duration::from_millis(200))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break false;
                    }
                }
            };
            let _ = drain.join();
            if ok {
                for v in stale {
                    let _ = std::fs::remove_file(secrets.join(v));
                }
            }
            eprintln!(
                "axe node: runtime {name}: device login {}",
                if ok { "succeeded" } else { "failed" }
            );
            done(ok);
        });
        if !printed {
            bail!("runtime {}: the device login printed nothing", self.name);
        }
        Ok(DeviceLogin {
            output: lines.join("\n"),
            code: code.filter(|_| reads_code),
        })
    }

    /// The CLI this runtime's agent template runs (its first argument).
    pub fn program(&self, cfg: &Config) -> Result<String> {
        cfg.agents
            .get(&self.agent)
            .and_then(|a| a.cmd.first().cloned())
            .with_context(|| format!("runtime {}: agent {} has no command", self.name, self.agent))
    }

    /// The runtime's CLI with `args` in its login environment.
    pub fn cli(&self, cfg: &Config, args: &[&str]) -> Result<Command> {
        let mut cmd = Command::new(self.program(cfg)?);
        cmd.args(args);
        let (set, remove) = self.env();
        for v in remove {
            cmd.env_remove(v);
        }
        for (k, v) in set {
            cmd.env(k, v);
        }
        Ok(cmd)
    }

    fn cli_login(&self, cfg: &Config, args: &[&str], value: &str) -> Result<()> {
        use std::io::Write as _;
        let mut child = self
            .cli(cfg, args)?
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("runtime {}: start the login", self.name))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(value.as_bytes())?;
        }
        let started = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if started.elapsed() > PROBE_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                bail!("runtime {}: the login timed out", self.name);
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read as _;
                let _ = e.read_to_string(&mut err);
            }
            let err = err.replace(value, "<redacted>");
            let err: String = err.trim().chars().take(MAX_MESSAGE).collect();
            bail!("runtime {}: the login failed ({status}): {err}", self.name);
        }
        Ok(())
    }

    /// Where a probe's output goes: `<name>/probe.log`, next to `state.json`.
    fn probe_log(&self) -> Option<PathBuf> {
        self.state_path
            .as_ref()
            .map(|p| p.with_file_name("probe.log"))
    }

    /// The last probe's output, one line, without the login's own values
    /// and at most [`MAX_MESSAGE`] characters; `None` when it printed nothing.
    fn probe_output(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.probe_log()?).ok()?;
        let mut text = strip_ansi(&text);
        for (_, v) in self.env().0 {
            if !v.is_empty() {
                text = text.replace(&v, "<redacted>");
            }
        }
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!line.is_empty()).then(|| line.chars().take(MAX_MESSAGE).collect())
    }

    fn save(&self) -> Result<()> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.health)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
    }
}

pub struct Registry {
    pub runtimes: Vec<Runtime>,
    /// Loaded from `[runtimes] dir`: health is tracked and acted on.
    pub managed: bool,
    /// `[runtimes] dir`, where [`Registry::create`] and [`Registry::remove`]
    /// keep their changes; `None` for implicit runtimes.
    dir: Option<PathBuf>,
    /// Running probes: the child, when it started, and its Unix start.
    probes: BTreeMap<String, (Child, Instant, i64)>,
    probed: BTreeMap<String, Instant>,
    /// Probes that check a login just made: their outcome is a card.
    checks: std::collections::BTreeSet<String>,
    /// Check cards ready before their probe ran (it could not start).
    ready: Vec<Alert>,
}

impl Registry {
    /// The registry `cfg` describes; see the module docs.
    pub fn load(cfg: &Config) -> Result<Registry> {
        let Some(section) = &cfg.runtimes else {
            let runtimes = cfg
                .agents
                .iter()
                .map(|(name, a)| Runtime {
                    name: name.clone(),
                    harness: a.harness().unwrap_or("").to_string(),
                    agent: name.clone(),
                    login_dir: None,
                    plan: None,
                    disabled: false,
                    probe: vec![],
                    state_path: None,
                    login_secrets: None,
                    health: Health::default(),
                })
                .collect();
            return Ok(Registry::new(runtimes, false));
        };
        let dir = expand_tilde(&section.dir);
        let mut runtimes = Vec::new();
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("runtimes.dir {}", dir.display()))?;
        for entry in entries {
            let entry = entry?;
            let file = entry.path().join("runtime.toml");
            if !file.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let rt = Runtime::read(cfg, &name, &file)
                .with_context(|| format!("runtime {}", file.display()))?;
            runtimes.push(rt);
        }
        runtimes.sort_by(|a, b| a.name.cmp(&b.name));
        check_logins(&runtimes)?;
        let mut reg = Registry::new(runtimes, true);
        reg.dir = Some(dir);
        Ok(reg)
    }

    /// Adds a runtime of `harness` and persists it in the registry
    /// directory: the next free name (`claude-2`, `codex-3`), its own login
    /// directory under `<name>/home` (sharing [`shared_entries`] with the
    /// CLI's default directory), and the agent template the harness's
    /// other runtimes use. It starts as needing a login (no card: the owner
    /// just asked for it), so no run goes to it before its login check
    /// passes. Returns its name.
    pub fn create(&mut self, cfg: &Config, harness: &str) -> Result<String> {
        let Some(dir) = self.dir.clone() else {
            bail!("this node has no runtime registry ([runtimes] dir)");
        };
        let Some(prefix) = name_prefix(harness) else {
            bail!("runtimes of harness {harness:?} cannot be added here");
        };
        // One login at a time per harness: a runtime still waiting for its
        // first login is reused, its wait started again.
        if let Some(rt) = self
            .runtimes
            .iter_mut()
            .find(|r| r.harness == harness && r.pending())
        {
            rt.health.added_at = Some(unix_now());
            rt.save()?;
            return Ok(rt.name.clone());
        }
        let agent = match self.of(harness).next() {
            Some(r) => r.agent.clone(),
            None => {
                let of: Vec<&String> = cfg
                    .agents
                    .iter()
                    .filter(|(_, a)| a.harness() == Some(harness))
                    .map(|(n, _)| n)
                    .collect();
                match of[..] {
                    [] => bail!("no [agents.*] entry drives harness {harness}"),
                    [one, ..] => one.clone(),
                }
            }
        };
        let name = (1..)
            .map(|n| format!("{prefix}-{n}"))
            .find(|n| self.get(n).is_none() && !dir.join(n).exists())
            .expect("a free name");
        let home = dir.join(&name);
        let login_dir = home.join("home");
        std::fs::create_dir_all(&login_dir)
            .with_context(|| format!("create {}", login_dir.display()))?;
        {
            use std::os::unix::fs::PermissionsExt as _;
            for d in [&home, &login_dir] {
                std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        if let Some((_, default)) = login_var(harness) {
            let default = expand_tilde(default);
            for e in shared_entries(harness) {
                // Linked even when missing, so a later install shows up too.
                if let Err(err) = std::os::unix::fs::symlink(default.join(e), login_dir.join(e)) {
                    eprintln!("axe node: runtime {name}: share {e}: {err}");
                }
            }
        }
        let made = (|| -> Result<Runtime> {
            let toml = format!(
                "# Added from /runtimes; remove it there too.\n\
                 harness = {}\nagent = {}\nlogin_dir = {}\n",
                toml_str(harness),
                toml_str(&agent),
                toml_str(&login_dir.display().to_string())
            );
            let health = Health {
                state: NEEDS_LOGIN.into(),
                alerted: Some(NEEDS_LOGIN.into()),
                added_at: Some(unix_now()),
                ..Health::default()
            };
            std::fs::write(home.join("state.json"), serde_json::to_vec_pretty(&health)?)?;
            crate::sealed::write_private_file(&home.join("runtime.toml"), toml.as_bytes())?;
            let rt = Runtime::read(cfg, &name, &home.join("runtime.toml"))?;
            let mut all = self.runtimes.clone();
            all.push(rt.clone());
            check_logins(&all)?;
            Ok(rt)
        })();
        let rt = match made {
            Ok(rt) => rt,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&home);
                return Err(e.context(format!("add runtime {name}")));
            }
        };
        self.runtimes.push(rt);
        self.runtimes.sort_by(|a, b| a.name.cmp(&b.name));
        eprintln!("axe node: runtime {name} ({harness}) added; it needs a login");
        Ok(name)
    }

    /// Removes runtime `name` from the registry and its directory, refused
    /// while it has `live` runs. Its stored credentials and a login
    /// directory inside its registry folder are deleted with it; a login
    /// directory elsewhere (the CLI's default one) is left in place. Returns
    /// what happened to its login files, for the owner.
    pub fn remove(&mut self, name: &str, live: usize) -> Result<String> {
        let Some(dir) = self.dir.clone() else {
            bail!("this node has no runtime registry ([runtimes] dir)");
        };
        let Some(rt) = self.get(name) else {
            bail!("no runtime {name} on this node");
        };
        if live > 0 {
            bail!(
                "runtime {name} has {live} live run{}; remove it once {} finished",
                if live == 1 { "" } else { "s" },
                if live == 1 { "it has" } else { "they have" }
            );
        }
        let home = dir.join(name);
        let login = match (&rt.login_dir, login_var(&rt.harness)) {
            (Some(d), _) if d.starts_with(&home) => {
                "its login (stored credentials and login directory) was deleted with it".to_string()
            }
            (Some(_), _) => "its stored credentials were deleted; its login directory outside \
                 the registry was left in place"
                .to_string(),
            (None, Some((_, default))) => format!(
                "its stored credentials were deleted; the CLI's default login ({default}) was \
                 left in place"
            ),
            (None, None) => "its stored credentials were deleted".to_string(),
        };
        if let Some((mut child, _, _)) = self.probes.remove(name) {
            let _ = child.kill();
            let _ = child.wait();
        }
        std::fs::remove_dir_all(&home).with_context(|| format!("remove {}", home.display()))?;
        self.probed.remove(name);
        self.checks.remove(name);
        self.ready.retain(|a| a.runtime != name);
        self.runtimes.retain(|r| r.name != name);
        eprintln!("axe node: runtime {name} removed");
        Ok(login)
    }

    pub fn new(runtimes: Vec<Runtime>, managed: bool) -> Registry {
        Registry {
            runtimes,
            managed,
            dir: None,
            probes: BTreeMap::new(),
            probed: BTreeMap::new(),
            checks: Default::default(),
            ready: Vec::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Runtime> {
        self.runtimes.iter().find(|r| r.name == name)
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut Runtime> {
        self.runtimes.iter_mut().find(|r| r.name == name)
    }

    /// The runtime a stored session, run or pin names. Sessions recorded
    /// before runtimes existed name their agent template (`claude`): they ran
    /// on the default login, so they belong to the runtime that adopts it.
    pub fn resolve(&self, name: &str) -> Option<&Runtime> {
        self.get(name).or_else(|| {
            self.runtimes
                .iter()
                .find(|r| r.agent == name && r.login_dir.is_none())
        })
    }

    /// Runtimes that run agent template `agent`, by name.
    pub fn of_agent<'a>(&'a self, agent: &'a str) -> impl Iterator<Item = &'a Runtime> + 'a {
        self.runtimes.iter().filter(move |r| r.agent == agent)
    }

    /// Runtimes of `harness`.
    pub fn of<'a>(&'a self, harness: &'a str) -> impl Iterator<Item = &'a Runtime> + 'a {
        self.runtimes.iter().filter(move |r| r.harness == harness)
    }

    /// Whether any runtime can take a run now.
    pub fn any_healthy(&self, now: i64) -> bool {
        self.runtimes.iter().any(|r| r.healthy(now))
    }

    /// When the earliest unhealthy runtime of `harnesses` may be healthy again
    /// (a lapsing limit, else the next login probe).
    pub fn next_healthy(&self, harnesses: &[&str], now: i64) -> i64 {
        self.runtimes
            .iter()
            .filter(|r| harnesses.contains(&r.harness.as_str()))
            .filter_map(|r| match r.health.state.as_str() {
                LIMITED | DEGRADED => r.health.until,
                _ => None,
            })
            .filter(|u| *u > now)
            .min()
            .unwrap_or(now + PROBE_EVERY.as_secs() as i64)
    }

    /// Records a run's failure class on its runtime (managed registries only).
    /// Quota limits it until the reset (an hour when unknown), an invalid login
    /// needs a new one, an unavailable provider degrades it for `backoff`
    /// seconds. Returns the card to send when the runtime newly turned
    /// limited or needs_login.
    ///
    /// `started_at` is when the run read its credential (Unix seconds): an
    /// invalid login from a run started before the latest confirmed login
    /// ([`Health::login_at`]) is that older credential's, so it is ignored.
    pub fn mark(
        &mut self,
        name: &str,
        f: &Failure,
        backoff: i64,
        now: i64,
        started_at: i64,
    ) -> Option<Alert> {
        if !self.managed {
            return None;
        }
        let rt = self.get_mut(name)?;
        if f.class == AUTH_INVALID && rt.health.login_at.is_some_and(|l| started_at < l) {
            eprintln!(
                "axe node: runtime {name}: ignoring {AUTH_INVALID} from a run started before the latest confirmed login"
            );
            return None;
        }
        let message: String = f.message.chars().take(MAX_MESSAGE).collect();
        rt.health.last_failure = Some(LastFailure {
            class: f.class.into(),
            message: message.clone(),
            at: now,
        });
        let (state, until) = match f.class {
            QUOTA_EXHAUSTED => (
                LIMITED,
                Some(
                    f.retry_at
                        .filter(|t| *t > now)
                        .unwrap_or(now + LIMITED_DEFAULT_SECONDS),
                ),
            ),
            AUTH_INVALID => (NEEDS_LOGIN, None),
            PROVIDER_UNAVAILABLE => (DEGRADED, Some(now + backoff.max(1))),
            _ => return None,
        };
        rt.health.state = state.into();
        rt.health.until = until;
        let alert = (matches!(state, LIMITED | NEEDS_LOGIN)
            && rt.health.alerted.as_deref() != Some(state))
        .then(|| Alert {
            runtime: rt.name.clone(),
            harness: rt.harness.clone(),
            state: state.into(),
            class: f.class.into(),
            message,
            until,
        });
        if let Err(e) = rt.save() {
            eprintln!("axe node: runtime {name}: {e:#}");
        }
        alert
    }

    /// The owner got the card for `state` of `name`.
    pub fn alerted(&mut self, name: &str, state: &str) {
        if let Some(rt) = self.get_mut(name) {
            rt.health.alerted = Some(state.into());
            if let Err(e) = rt.save() {
                eprintln!("axe node: runtime {name}: {e:#}");
            }
        }
    }

    /// Keeps the latest quota a run of `name` reported.
    pub fn set_quota(&mut self, name: &str, quota: Value) {
        let Some(rt) = self.get_mut(name) else {
            return;
        };
        if rt.health.quota.as_ref() == Some(&quota) {
            return;
        }
        rt.health.quota = Some(quota);
        if let Err(e) = rt.save() {
            eprintln!("axe node: runtime {name}: {e:#}");
        }
    }

    /// Lapsed limits return to ok, and the card episode ends with them. A
    /// runtime `/runtimes` Add made that got no login within
    /// [`ADD_LOGIN_WAIT`] is removed; returns its card.
    pub fn tick(&mut self, now: i64) -> Vec<Alert> {
        let abandoned: Vec<(String, String)> = self
            .runtimes
            .iter()
            .filter(|r| r.health.never_logged_in() && !self.checks.contains(&r.name))
            .filter(|r| r.health.added_at.is_some_and(|a| now - a >= ADD_LOGIN_WAIT))
            .map(|r| (r.name.clone(), r.harness.clone()))
            .collect();
        let mut alerts = Vec::new();
        for (name, harness) in abandoned {
            let wait = format!("no login within {} minutes of Add", ADD_LOGIN_WAIT / 60);
            alerts.extend(self.remove_added(&name, &harness, ADD_REMOVED, wait));
        }
        for rt in &mut self.runtimes {
            if matches!(rt.health.state.as_str(), LIMITED | DEGRADED)
                && rt.health.until.is_none_or(|u| u <= now)
            {
                rt.health.state = OK.into();
                rt.health.until = None;
                rt.health.alerted = None;
                if let Err(e) = rt.save() {
                    eprintln!("axe node: runtime {}: {e:#}", rt.name);
                }
            }
        }
        alerts
    }

    /// Removes runtime `name`, which `/runtimes` Add made and which never
    /// logged in (so it never took a run), and returns the owner's card.
    fn remove_added(
        &mut self,
        name: &str,
        harness: &str,
        class: &str,
        why: String,
    ) -> Option<Alert> {
        match self.remove(name, 0) {
            Ok(_) => Some(Alert {
                runtime: name.into(),
                harness: harness.into(),
                state: REMOVED.into(),
                class: class.into(),
                message: why,
                until: None,
            }),
            Err(e) => {
                eprintln!("axe node: runtime {name}: remove after a failed Add: {e:#}");
                None
            }
        }
    }

    /// Checks the login of `name` now, as a probe does, because a login was
    /// just made: a probe already running is replaced, and the outcome comes
    /// back from [`Registry::probe`] as a card for the owner (ok, or still
    /// needing a login with the probe's output). The periodic probe keeps
    /// its cadence from here.
    pub fn check_now(&mut self, name: &str) {
        if !self.managed {
            return;
        }
        let Some(rt) = self.get(name) else {
            eprintln!("axe node: runtime {name}: logged in, but not in this node's registry");
            return;
        };
        if rt.disabled || rt.probe.is_empty() {
            eprintln!("axe node: runtime {name}: logged in; no login check (disabled or no probe)");
            return;
        }
        if let Some((mut child, _, _)) = self.probes.remove(name) {
            let _ = child.kill();
            let _ = child.wait();
        }
        eprintln!("axe node: runtime {name}: logged in; checking the login now");
        self.checks.insert(name.to_string());
        self.start_probe(name);
    }

    fn start_probe(&mut self, name: &str) {
        let Some(rt) = self.get(name) else {
            return;
        };
        let argv = rt.probe.clone();
        let (set, remove) = rt.env();
        let out = rt.probe_log();
        self.probed.insert(name.to_string(), Instant::now());
        let started_at = unix_now();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).stdin(Stdio::null());
        match out.as_ref().map(std::fs::File::create) {
            Some(Ok(f)) => {
                let err = f.try_clone().map_or(Stdio::null(), Stdio::from);
                cmd.stdout(f).stderr(err);
            }
            _ => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        for v in remove {
            cmd.env_remove(v);
        }
        for (k, v) in set {
            cmd.env(k, v);
        }
        match cmd.spawn() {
            Ok(child) => {
                self.probes
                    .insert(name.to_string(), (child, Instant::now(), started_at));
            }
            Err(e) => {
                eprintln!("axe node: runtime {name}: probe {argv:?}: {e}");
                let status = format!("the login check did not start: {e}");
                if let Some(a) = self.finished(name, false, &status, started_at) {
                    self.ready.push(a);
                }
            }
        }
    }

    /// Probes runtimes that need a login, at most every [`PROBE_EVERY`], with
    /// the harness's status command in the runtime's environment: a zero exit
    /// makes the runtime ok again. A probe never marks a runtime unhealthy.
    /// Non-blocking: a started probe is collected on a later call. Returns
    /// the cards of finished [`Registry::check_now`] checks.
    pub fn probe(&mut self) -> Vec<Alert> {
        let mut done = Vec::new();
        for (name, (child, started, at)) in &mut self.probes {
            match child.try_wait() {
                Ok(Some(status)) => {
                    done.push((name.clone(), status.success(), status.to_string(), *at))
                }
                Ok(None) if started.elapsed() < PROBE_TIMEOUT => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    done.push((
                        name.clone(),
                        false,
                        format!("timed out after {}s", PROBE_TIMEOUT.as_secs()),
                        *at,
                    ));
                }
            }
        }
        let mut alerts = std::mem::take(&mut self.ready);
        for (name, ok, status, at) in done {
            self.probes.remove(&name);
            alerts.extend(self.finished(&name, ok, &status, at));
        }
        if !self.managed {
            return alerts;
        }
        let due: Vec<String> = self
            .runtimes
            .iter()
            .filter(|r| r.health.state == NEEDS_LOGIN && !r.disabled && !r.probe.is_empty())
            .filter(|r| !self.probes.contains_key(&r.name))
            .filter(|r| {
                self.probed
                    .get(&r.name)
                    .is_none_or(|at| at.elapsed() >= PROBE_EVERY)
            })
            .map(|r| r.name.clone())
            .collect();
        for name in due {
            self.start_probe(&name);
        }
        alerts
    }

    /// A probe of `name` started at `started_at` ended: a pass confirms the
    /// login as of then and makes a runtime that needs a login ok again.
    /// Returns the card when it was a [`Registry::check_now`] check.
    fn finished(&mut self, name: &str, ok: bool, status: &str, started_at: i64) -> Option<Alert> {
        let check = self.checks.remove(name);
        let rt = self.get_mut(name)?;
        if ok && rt.health.login_at.is_none_or(|l| l < started_at) {
            rt.health.login_at = Some(started_at);
            rt.health.added_at = None;
            if rt.health.state != NEEDS_LOGIN {
                if let Err(e) = rt.save() {
                    eprintln!("axe node: runtime {name}: {e:#}");
                }
            }
        }
        if ok && rt.health.state == NEEDS_LOGIN {
            eprintln!("axe node: runtime {name}: login probe passed; ok again");
            rt.health.state = OK.into();
            rt.health.until = None;
            rt.health.alerted = None;
            if let Err(e) = rt.save() {
                eprintln!("axe node: runtime {name}: {e:#}");
            }
        }
        if !check {
            return None;
        }
        let message = match ok {
            true => String::new(),
            false => rt.probe_output().unwrap_or_else(|| status.to_string()),
        };
        eprintln!(
            "axe node: runtime {name}: login check {}",
            if ok { "passed" } else { "failed" }
        );
        // The first login of a runtime Add made failed: Add made nothing.
        if !ok && rt.health.added_at.is_some() && rt.health.never_logged_in() {
            let harness = rt.harness.clone();
            return self.remove_added(name, &harness, LOGIN_FAILED, message);
        }
        let rt = self.get(name)?;
        Some(Alert {
            runtime: rt.name.clone(),
            harness: rt.harness.clone(),
            state: rt.health.state.clone(),
            class: if ok { LOGIN_OK } else { LOGIN_FAILED }.into(),
            message,
            until: None,
        })
    }

    /// What the server gets about each runtime: name, harness, plan, state,
    /// limit end, last failure, quota and live runs. Never a credential, a
    /// login directory or a token.
    pub fn heartbeat(&self, in_use: impl Fn(&str) -> usize, now: i64) -> Value {
        let list: Vec<Value> = self
            .runtimes
            .iter()
            .map(|r| {
                json!({
                    "name": r.name,
                    "harness": r.harness,
                    "agent": r.agent,
                    "default_login": r.login_dir.is_none(),
                    "pending": r.pending(),
                    "plan": r.plan,
                    "state": r.state(now),
                    "never_logged_in": r.health.never_logged_in(),
                    "healthy": r.healthy(now),
                    "until": (!r.healthy(now)).then_some(r.health.until).flatten(),
                    "last_failure": r.health.last_failure,
                    "quota": r.health.quota,
                    "in_use": in_use(&r.name),
                })
            })
            .collect();
        json!({"managed": self.managed, "list": list})
    }
}

/// Never two runtimes on one login (#2779): quota and expiry are a login's.
fn check_logins(runtimes: &[Runtime]) -> Result<()> {
    for (i, a) in runtimes.iter().enumerate() {
        for b in &runtimes[i + 1..] {
            if a.harness == b.harness && a.login_dir == b.login_dir {
                bail!(
                    "runtimes {} and {} share one {} login; give each its own login_dir",
                    a.name,
                    b.name,
                    a.harness
                );
            }
        }
    }
    Ok(())
}

/// What a runtime [`Registry::create`] adds shares with the CLI's default
/// directory, as links ([`crate::harness::Harness::shared`]).
fn shared_entries(harness: &str) -> &'static [&'static str] {
    crate::harness::get(harness).map_or(&[], |h| h.shared)
}

/// The name stem of runtimes [`Registry::create`] adds, per harness.
fn name_prefix(harness: &str) -> Option<&'static str> {
    crate::harness::get(harness).map(|h| h.name_prefix)
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

impl Runtime {
    fn read(cfg: &Config, name: &str, path: &Path) -> Result<Runtime> {
        crate::db::validate_name("runtime", name)?;
        let text = std::fs::read_to_string(path)?;
        let f: File = toml::from_str(&text)?;
        let Some((_, default_dir)) = login_var(&f.harness) else {
            bail!(
                "unknown harness {:?} (expected one of {})",
                f.harness,
                crate::harness::names()
            );
        };
        let agent = match f.agent {
            Some(a) => {
                let Some(t) = cfg.agents.get(&a) else {
                    bail!("agent {a:?} is not in [agents]");
                };
                if t.harness() != Some(f.harness.as_str()) {
                    bail!("agent {a:?} does not drive harness {}", f.harness);
                }
                a
            }
            None => {
                let of: Vec<&String> = cfg
                    .agents
                    .iter()
                    .filter(|(_, a)| a.harness() == Some(f.harness.as_str()))
                    .map(|(n, _)| n)
                    .collect();
                match of[..] {
                    [one] => one.clone(),
                    [] => bail!("no [agents.*] entry drives harness {}", f.harness),
                    _ => bail!(
                        "several agents drive harness {} ({}); set agent",
                        f.harness,
                        of.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                }
            }
        };
        // The CLI's default directory is adopted in place: nothing is set.
        let login_dir = f
            .login_dir
            .map(|d| expand_tilde(&d))
            .filter(|d| *d != expand_tilde(default_dir));
        let probe = f.probe.unwrap_or_else(|| default_probe(&f.harness));
        let state_path = path.with_file_name("state.json");
        let health = match std::fs::read(&state_path) {
            Ok(b) => serde_json::from_slice(&b)
                .with_context(|| format!("read {}", state_path.display()))?,
            // Never checked: no run goes to it before its login probe
            // passes (the probe runs at once; no card). Without a probe it
            // is trusted as before.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !probe.is_empty() => Health {
                state: NEEDS_LOGIN.into(),
                alerted: Some(NEEDS_LOGIN.into()),
                ..Health::default()
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Health::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", state_path.display())),
        };
        Ok(Runtime {
            name: name.into(),
            harness: f.harness,
            agent,
            login_dir,
            plan: f.plan,
            disabled: f.disabled,
            probe,
            login_secrets: Some(path.with_file_name("login")),
            state_path: Some(state_path),
            health,
        })
    }
}

/// `s` without ANSI escape sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The latest quota window a Claude Code run reported (its last
/// `rate_limit_event`): status, window type, utilization (0-1) and reset.
pub fn quota_from_log(path: &Path) -> Option<Value> {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(std::fs::File::open(path).ok()?);
    let mut last = None;
    for line in reader.split(b'\n') {
        let Ok(line) = line else { break };
        let text = String::from_utf8_lossy(&line);
        if !text.contains("\"type\":\"rate_limit_event\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(text.trim()) else {
            continue;
        };
        if v["type"] == "rate_limit_event" {
            last = Some(v["rate_limit_info"].clone());
        }
    }
    let info = last.filter(Value::is_object)?;
    let kind = info["rateLimitType"].as_str();
    let utilization = kind
        .and_then(|k| info["unifiedWindows"][k]["utilization"].as_f64())
        .or_else(|| info["utilization"].as_f64());
    Some(json!({
        "status": info["status"],
        "window": kind,
        "utilization": utilization,
        "resets_at": info["resetsAt"],
    }))
}

/// `GET /api/runtimes`: the runtimes in the node's last report (an
/// admission report, `admission::reported`), and whether it is stale.
pub fn view(admission: &Value) -> Value {
    let age = admission["report_age_seconds"].as_u64();
    let stale = age.is_none_or(|a| a > crate::admission::REPORT_STALE.as_secs());
    json!({
        "managed": admission["runtimes"]["managed"],
        "runtimes": admission["runtimes"]["list"].as_array().cloned().unwrap_or_default(),
        "report_age_seconds": age,
        "stale": stale,
        "key": admission["forms"],
    })
}

/// The listed runtime a bot's primary or backup `name` runs on, as the node
/// resolves a pin (`Dispatcher::pin_runtime`): that runtime, or the one on
/// agent `name`'s default login, else a runtime of agent template `name`, a
/// healthy one first. Pending runtimes ([`Runtime::pending`]) are not runtimes yet.
pub fn resolve_listed<'a>(list: &'a [Value], name: &str) -> Option<&'a Value> {
    let shown = || list.iter().filter(|r| r["pending"] != true);
    let of = || shown().filter(move |r| r["agent"] == name);
    shown()
        .find(|r| r["name"] == name)
        .or_else(|| of().find(|r| r["default_login"] == true))
        .or_else(|| of().find(|r| r["healthy"] == true))
        .or_else(|| of().next())
}

/// Adds each runtime's `uses` to a [`view`]: `primary of <bots>` and
/// `backup of <bots>` from each bot `(name, primary, backup)`, and `tasks`
/// when its harness is in `task_harnesses` (what selection rules pick for
/// tasks).
pub fn with_uses(
    view: &mut Value,
    bots: &[(String, String, Option<String>)],
    task_harnesses: &[String],
) {
    let list = view["runtimes"].as_array().cloned().unwrap_or_default();
    // runtime -> role -> bots
    let mut roles: BTreeMap<String, [Vec<&str>; 2]> = BTreeMap::new();
    for (bot, primary, backup) in bots {
        for (i, name) in [Some(primary), backup.as_ref()].into_iter().enumerate() {
            let Some(rt) = name.and_then(|n| resolve_listed(&list, n)) else {
                continue;
            };
            let rt = rt["name"].as_str().unwrap_or_default().to_string();
            roles.entry(rt).or_default()[i].push(bot);
        }
    }
    if let Some(l) = view["runtimes"].as_array_mut() {
        for r in l {
            let name = r["name"].as_str().unwrap_or_default().to_string();
            let mut u: Vec<String> = match roles.get(&name) {
                Some([p, b]) => [("primary", p), ("backup", b)]
                    .into_iter()
                    .filter(|(_, bots)| !bots.is_empty())
                    .map(|(role, bots)| format!("{role} of {}", bots.join(", ")))
                    .collect(),
                None => vec![],
            };
            let harness = r["harness"].as_str().unwrap_or_default();
            if r["pending"] != true && task_harnesses.iter().any(|h| h == harness) {
                u.push("tasks".into());
            }
            r["uses"] = json!(u);
        }
    }
}

fn when(t: &Value) -> String {
    t.as_i64().map_or("-".into(), crate::bot::ts)
}

/// A runtime state for people: `needs login`, `failing`.
fn state_label(state: &str) -> &str {
    match state {
        NEEDS_LOGIN => "needs login",
        DEGRADED => "failing",
        s => s,
    }
}

/// One short line per runtime of a [`view`], for `axe runtimes` and
/// `/runtimes`: name, harness, state (with its last failure when not ok),
/// plan, live runs and quota when known.
pub fn render(v: &Value) -> String {
    let list = v["runtimes"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return match v["report_age_seconds"].as_u64() {
            None => "runtimes: unknown (no node has reported since the server started)".into(),
            Some(_) => "runtimes: none reported".into(),
        };
    }
    let mut out = Vec::new();
    for r in &list {
        let state = r["state"].as_str().unwrap_or("?");
        let label = match r["never_logged_in"] == true {
            true => "not logged in",
            false => state_label(state),
        };
        let mut line = format!(
            "{} ({}): {}",
            r["name"].as_str().unwrap_or("?"),
            r["harness"].as_str().unwrap_or("?"),
            label
        );
        if !r["until"].is_null() {
            line.push_str(&format!(" until {}", when(&r["until"])));
        }
        let f = &r["last_failure"];
        if let (false, Some(c)) = (state == OK, f["class"].as_str()) {
            line.push_str(&format!(" (last failure {} at {})", c, when(&f["at"])));
        }
        if let Some(p) = r["plan"].as_str() {
            line.push_str(&format!(", plan {p}"));
        }
        line.push_str(&format!(", {} live", r["in_use"].as_u64().unwrap_or(0)));
        let q = &r["quota"];
        if let Some(u) = q["utilization"].as_f64() {
            line.push_str(&format!(
                ", quota {}% of {} (resets {})",
                (u * 100.0).round(),
                q["window"].as_str().unwrap_or("window"),
                when(&q["resets_at"])
            ));
        }
        let uses: Vec<&str> = r["uses"]
            .as_array()
            .map(|u| u.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if !uses.is_empty() {
            line.push_str(&format!("; {}", uses.join("; ")));
        }
        out.push(line);
    }
    if v["stale"] == true {
        out.push(format!(
            "(stale: the node last reported {}s ago)",
            v["report_age_seconds"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(fp) = v["key"]["fingerprint"].as_str() {
        out.push(format!(
            "dispatcher key {fp} (sealed forms show it; compare once)"
        ));
    }
    out.join("\n")
}

/// The owner card for an [`Alert`] (its JSON form).
pub fn alert_text(a: &Value) -> String {
    let name = a["runtime"].as_str().unwrap_or("?");
    let harness = a["harness"].as_str().unwrap_or("?");
    let message = a["message"].as_str().unwrap_or("");
    let head = match (a["class"].as_str(), a["state"].as_str()) {
        (Some(LOGIN_OK), _) => {
            return format!("Runtime {name} ({harness}): the new login works; the runtime is ok.")
        }
        (Some(LOGIN_FAILED), Some(REMOVED)) => format!(
            "Runtime {name} ({harness}): its first login failed its check, so the runtime \
             /runtimes added was removed again. Add it again to retry."
        ),
        (Some(ADD_REMOVED), _) => format!(
            "Runtime {name} ({harness}) was removed again: it got no login within {} minutes \
             of /runtimes Add. Add it again to log in.",
            ADD_LOGIN_WAIT / 60
        ),
        (Some(LOGIN_FAILED), Some(NEEDS_LOGIN)) => format!(
            "Runtime {name} ({harness}): the new login failed its check, so the runtime still \
             needs a login. Log in again from /runtimes; it is also rechecked every {} minutes.",
            PROBE_EVERY.as_secs() / 60
        ),
        (Some(LOGIN_FAILED), s) => format!(
            "Runtime {name} ({harness}): the new login failed its check; the runtime stays {} \
             until a run says otherwise. Log in again from /runtimes if runs fail.",
            s.unwrap_or("?")
        ),
        (_, Some(NEEDS_LOGIN)) => format!(
            "Runtime {name} ({harness}) needs a new login. Its runs go to other healthy \
             runtimes meanwhile; it is probed every {} minutes and is ok again once the \
             login works.",
            PROBE_EVERY.as_secs() / 60
        ),
        (_, Some(LIMITED)) => format!(
            "Runtime {name} ({harness}) hit its usage limit until {}. Its runs go to other \
             healthy runtimes or wait for the reset.",
            when(&a["until"])
        ),
        (_, s) => format!("Runtime {name} ({harness}) is {}.", s.unwrap_or("?")),
    };
    match (message.is_empty(), a["class"].as_str()) {
        (true, _) => head,
        (false, Some(LOGIN_FAILED)) => format!("{head}\n\nThe check said: {message}"),
        (false, class) => format!("{head}\n\n{} {message}", class.unwrap_or("")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake Codex: `login --with-api-key` keeps stdin in `$CODEX_HOME/key`
    /// (and refuses `bad`, echoing it); `login --device-auth` prints a link
    /// and a code in colour, then exits 0 after a moment.
    const FAKE_CODEX: &str = r#"#!/bin/sh
case "$1 $2" in
"login --with-api-key")
  read -r key
  if [ "$key" = "sk-bad-key-1xxxxxxxxxxxx" ]; then echo "invalid key $key" >&2; exit 1; fi
  printf '%s' "$key" > "$CODEX_HOME/key"; exit 0;;
"login --device-auth")
  printf '\033[1mOpen https://example.test/device\033[0m\nCode: ABCD-1234\n'; sleep 1; exit 0;;
esac
exit 2
"#;

    fn registry(dir: &Path) -> (Config, Registry) {
        let codex = dir.join("fake-codex");
        std::fs::write(&codex, FAKE_CODEX).unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let reg = dir.join("runtimes");
        for (name, toml) in [
            ("claude-1", "harness = \"claude-code\"\n"),
            (
                "codex-1",
                &format!(
                    "harness = \"codex\"\nlogin_dir = \"{}\"\n",
                    dir.join("codex-home").display()
                ),
            ),
        ] {
            std::fs::create_dir_all(reg.join(name)).unwrap();
            std::fs::write(
                reg.join(name).join("runtime.toml"),
                format!("{toml}probe = [\"false\"]\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(dir.join("codex-home")).unwrap();
        let cfg = Config::parse(&format!(
            "[runtimes]\ndir = \"{}\"\n[agents.claude]\ncmd = [\"claude\"]\nharness = \"claude-code\"\n\
             [agents.codex]\ncmd = [\"{}\"]\nsession = \"none\"\nharness = \"codex\"\n",
            reg.display(),
            codex.display()
        ))
        .unwrap();
        let r = Registry::load(&cfg).unwrap();
        (cfg, r)
    }

    #[test]
    fn a_claude_login_is_a_private_env_credential_replacing_the_other() {
        const OAT: &str = "sk-ant-oat01-dummy123xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        const KEY: &str = "sk-ant-api03-dummy1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, reg) = registry(tmp.path());
        let rt = reg.get("claude-1").unwrap();
        rt.login(&cfg, "token", OAT).unwrap();
        let file = tmp
            .path()
            .join("runtimes/claude-1/login/CLAUDE_CODE_OAUTH_TOKEN");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), OAT);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (set, _) = rt.env();
        assert!(set.contains(&("CLAUDE_CODE_OAUTH_TOKEN", OAT.into())));
        rt.login(&cfg, "api_key", KEY).unwrap();
        let (set, _) = rt.env();
        assert!(set.contains(&("ANTHROPIC_API_KEY", KEY.into())));
        assert!(!set.iter().any(|(k, _)| *k == "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(rt.login(&cfg, "device", "").is_err());
        assert!(rt.login(&cfg, "bogus", "x").is_err());
        assert!(rt.login(&cfg, "token", "a\nb").is_err());
        // A malformed credential is refused by name and nothing is stored:
        // never trimmed or rewritten, never a later "Not logged in".
        let err = |v: &str| rt.login(&cfg, "token", v).unwrap_err().to_string();
        let wrapped = format!("{} {}", &OAT[..50], &OAT[50..]);
        assert_eq!(
            err(&wrapped),
            "the token contains 1 space: likely a wrapped line"
        );
        assert_eq!(
            err(&format!("{OAT}\n")),
            "the token contains 1 line break at the start or end"
        );
        assert_eq!(
            err("sk-ant-api03-abc"),
            "the token must start with sk-ant-oat01-"
        );
        assert_eq!(
            err(&OAT[..60]),
            "the token is too short (60 characters; expected at least 90)"
        );
        assert!(rt.env().0.contains(&("ANTHROPIC_API_KEY", KEY.into())));
        // Without a registry there is nowhere to keep a login.
        let plain =
            Config::parse("[agents.claude]\ncmd = [\"claude\"]\nharness = \"claude-code\"\n")
                .unwrap();
        let implicit = Registry::load(&plain).unwrap();
        assert!(implicit
            .get("claude")
            .unwrap()
            .login(&plain, "token", "x")
            .is_err());
    }

    #[test]
    fn a_codex_api_key_goes_to_its_cli_on_stdin_and_errors_hide_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, reg) = registry(tmp.path());
        let rt = reg.get("codex-1").unwrap();
        rt.login(&cfg, "api_key", "sk-codex-9xxxxxxxxxxxxxxx")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("codex-home/key")).unwrap(),
            "sk-codex-9xxxxxxxxxxxxxxx"
        );
        let e = rt
            .login(&cfg, "api_key", "sk-bad-key-1xxxxxxxxxxxx")
            .unwrap_err()
            .to_string();
        assert!(e.contains("login failed"), "{e}");
        assert!(
            e.contains("<redacted>") && !e.contains("sk-bad-key-1xxxxxxxxxxxx"),
            "{e}"
        );
    }

    #[test]
    fn a_codex_device_login_relays_its_link_and_code() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, reg) = registry(tmp.path());
        let rt = reg.get("codex-1").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let out = rt
            .device_login(&cfg, Duration::from_millis(500), move |ok| {
                tx.send(ok).unwrap();
            })
            .unwrap();
        assert_eq!(
            out.output,
            "Open https://example.test/device\nCode: ABCD-1234"
        );
        assert!(out.code.is_none(), "Codex reads no code");
        assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    }

    /// Fake `claude auth login`: prints the sign-in URL and the paste
    /// prompt (no newline), reads the code from stdin and, for `good#state`,
    /// saves a login in its config directory.
    const FAKE_CLAUDE_LOGIN: &str = r#"#!/bin/sh
[ "$1 $2 $3" = "auth login --claudeai" ] || exit 3
[ "$BROWSER" = /bin/false ] || exit 4
echo "Opening browser to sign in..."
echo "If the browser didn't open, visit: https://claude.ai/oauth/authorize?code=true&state=x"
printf 'Paste code here if prompted > '
read code
[ "$code" = "good#state" ] || { echo "Couldn't log in: That code wasn't accepted"; exit 1; }
echo '{}' > "$CLAUDE_CONFIG_DIR/.credentials.json"
echo "Login successful."
"#;

    #[test]
    fn a_claude_web_login_relays_its_link_and_takes_the_code_on_stdin() {
        use std::io::Write as _;
        const OAT: &str = "sk-ant-oat01-dummy123xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let claude = dir.join("fake-claude");
        std::fs::write(&claude, FAKE_CLAUDE_LOGIN).unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let home = dir.join("claude-home");
        std::fs::create_dir_all(dir.join("runtimes/claude-2")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            dir.join("runtimes/claude-2/runtime.toml"),
            format!(
                "harness = \"claude-code\"\nlogin_dir = \"{}\"\n",
                home.display()
            ),
        )
        .unwrap();
        let cfg = Config::parse(&format!(
            "[runtimes]\ndir = \"{}\"\n[agents.claude]\ncmd = [\"{}\"]\nharness = \"claude-code\"\n",
            dir.join("runtimes").display(),
            claude.display()
        ))
        .unwrap();
        let reg = Registry::load(&cfg).unwrap();
        let rt = reg.get("claude-2").unwrap();
        // A stored token would hide the new login: it goes once that works.
        rt.login(&cfg, "token", OAT).unwrap();
        let login = |code: &str| {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut l = rt
                .device_login(&cfg, Duration::from_millis(800), move |ok| {
                    tx.send(ok).unwrap();
                })
                .unwrap();
            assert_eq!(
                l.output,
                "Opening browser to sign in...\nIf the browser didn't open, visit: \
                 https://claude.ai/oauth/authorize?code=true&state=x"
            );
            let mut stdin = l.code.take().expect("Claude reads the code on stdin");
            writeln!(stdin, "{code}").unwrap();
            rx.recv_timeout(Duration::from_secs(10)).unwrap()
        };
        assert!(!login("bad#code"), "a refused code fails the login");
        assert!(rt
            .env()
            .0
            .iter()
            .any(|(k, _)| *k == "CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(login("good#state"));
        assert!(home.join(".credentials.json").exists());
        assert!(
            !rt.env()
                .0
                .iter()
                .any(|(k, _)| *k == "CLAUDE_CODE_OAUTH_TOKEN"),
            "the stale token was removed"
        );
    }

    /// Collects `reg`'s probes until `f` holds or 10 seconds pass.
    fn collect(reg: &mut Registry, f: impl Fn(&Registry, &[Alert]) -> bool) -> Vec<Alert> {
        let end = Instant::now() + Duration::from_secs(10);
        let mut all = Vec::new();
        loop {
            all.extend(reg.probe());
            if f(reg, &all) || Instant::now() > end {
                return all;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_stored_login_is_checked_at_once_and_its_outcome_is_a_card() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _) = registry(tmp.path());
        let dir = tmp.path().join("runtimes/claude-1");
        // Passes only with the good token; otherwise says why, with the token.
        std::fs::write(
            dir.join("runtime.toml"),
            "harness = \"claude-code\"\nprobe = [\"sh\", \"-c\", \
             \"[ \\\"$CLAUDE_CODE_OAUTH_TOKEN\\\" = sk-ant-oat01-goodxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx ] || \
             { printf '\\\\033[1mInvalid token %s\\\\033[0m\\\\n' \\\"$CLAUDE_CODE_OAUTH_TOKEN\\\" >&2; exit 1; }\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("state.json"),
            r#"{"state":"needs_login","alerted":"needs_login"}"#,
        )
        .unwrap();
        let mut reg = Registry::load(&cfg).unwrap();

        // The periodic probe runs and fails: no card, still needs a login.
        let alerts = collect(&mut reg, |r, _| {
            r.probes.is_empty() && r.probed.contains_key("claude-1")
        });
        assert!(alerts.is_empty(), "{alerts:?}");
        assert_eq!(reg.get("claude-1").unwrap().health.state, NEEDS_LOGIN);
        // Its next run is minutes away.
        assert!(reg.probe().is_empty() && reg.probes.is_empty());

        // A bad login: checked now, the card says why and hides the value.
        let rt = reg.get("claude-1").unwrap().clone();
        rt.login(&cfg, "token", "sk-ant-oat01-bad-oat-77xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        reg.check_now("claude-1");
        let alerts = collect(&mut reg, |_, a| !a.is_empty());
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        let a = &alerts[0];
        assert_eq!(
            (a.class.as_str(), a.state.as_str()),
            (LOGIN_FAILED, NEEDS_LOGIN)
        );
        assert_eq!(a.message, "Invalid token <redacted>");
        let text = alert_text(&a.to_json());
        assert!(text.contains("the new login failed its check"), "{text}");
        assert!(
            text.ends_with("The check said: Invalid token <redacted>"),
            "{text}"
        );

        // A good login: checked now, ok again, and the card says so.
        rt.login(&cfg, "token", "sk-ant-oat01-goodxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        reg.check_now("claude-1");
        let alerts = collect(&mut reg, |_, a| !a.is_empty());
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(
            (alerts[0].class.as_str(), alerts[0].state.as_str()),
            (LOGIN_OK, OK)
        );
        assert_eq!(
            alert_text(&alerts[0].to_json()),
            "Runtime claude-1 (claude-code): the new login works; the runtime is ok."
        );
        let rt = reg.get("claude-1").unwrap();
        assert_eq!(
            (rt.health.state.as_str(), rt.health.alerted.as_deref()),
            (OK, None)
        );
        let saved: Health =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(saved.state, OK);

        // An ok runtime is checked too; a failure reports but keeps it ok.
        rt.login(&cfg, "token", "sk-ant-oat01-worsexxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        reg.check_now("claude-1");
        let alerts = collect(&mut reg, |_, a| !a.is_empty());
        assert_eq!(
            (alerts[0].class.as_str(), alerts[0].state.as_str()),
            (LOGIN_FAILED, OK)
        );
        assert!(alert_text(&alerts[0].to_json()).contains("stays ok"));
        // The periodic probe of an ok runtime never runs, nor makes a card.
        assert!(reg.probe().is_empty() && reg.probes.is_empty());
    }
    #[test]
    fn a_login_failure_from_a_run_older_than_the_confirmed_login_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _) = registry(tmp.path());
        let dir = tmp.path().join("runtimes/claude-1");
        std::fs::write(
            dir.join("runtime.toml"),
            "harness = \"claude-code\"\nprobe = [\"sh\", \"-c\", \
             \"[ \\\"$CLAUDE_CODE_OAUTH_TOKEN\\\" = sk-ant-oat01-goodxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx ]\"]\n",
        )
        .unwrap();
        // It was ok (checked before).
        std::fs::write(dir.join("state.json"), r#"{"state":"ok"}"#).unwrap();
        let mut reg = Registry::load(&cfg).unwrap();
        let expired = Failure {
            class: AUTH_INVALID,
            status: None,
            message: "this login reached its time limit".into(),
            retry_at: None,
        };

        // A run started on the old login fails: needs a login.
        let t0 = unix_now() - 600;
        assert!(reg.mark("claude-1", &expired, 60, t0 + 10, t0).is_some());
        assert_eq!(reg.get("claude-1").unwrap().health.state, NEEDS_LOGIN);

        // The new login is stored and its check passes: ok, login confirmed.
        let before_login = unix_now() - 1;
        let rt = reg.get("claude-1").unwrap().clone();
        rt.login(&cfg, "token", "sk-ant-oat01-goodxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        reg.check_now("claude-1");
        collect(&mut reg, |_, a| !a.is_empty());
        let health = &reg.get("claude-1").unwrap().health;
        assert_eq!(health.state, OK);
        let login_at = health
            .login_at
            .expect("the passing check confirms the login");
        assert!(login_at > before_login);
        let saved: Health =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(saved.login_at, Some(login_at));

        // A run spawned before the login ends later with the old credential's
        // failure: the runtime stays ok, no card, no recorded failure.
        let alert = reg.mark("claude-1", &expired, 60, login_at + 180, before_login);
        assert!(alert.is_none(), "{alert:?}");
        let health = &reg.get("claude-1").unwrap().health;
        assert_eq!(health.state, OK);
        assert_eq!(
            health.last_failure.as_ref().map(|f| f.at),
            Some(t0 + 10),
            "the stale failure is not recorded"
        );
        // Other classes from an older run still count (the quota is the account's).
        let quota = Failure {
            class: QUOTA_EXHAUSTED,
            status: Some(429),
            message: "limit".into(),
            retry_at: None,
        };
        assert!(reg
            .mark("claude-1", &quota, 60, login_at + 190, before_login)
            .is_some());
        reg.tick(login_at + 190 + LIMITED_DEFAULT_SECONDS);

        // A run started on the new login that fails still marks the runtime.
        assert!(reg
            .mark("claude-1", &expired, 60, login_at + 200, login_at)
            .is_some());
        assert_eq!(reg.get("claude-1").unwrap().health.state, NEEDS_LOGIN);
    }

    #[test]
    fn runtimes_added_and_removed_persist_with_their_own_logins() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut reg) = registry(tmp.path());
        let dir = tmp.path().join("runtimes");

        // A second and a third Claude: next free names, each its own login.
        assert_eq!(reg.create(&cfg, "claude-code").unwrap(), "claude-2");
        // One login at a time: Add again while claude-2 waits for its first
        // login reuses it.
        assert_eq!(reg.create(&cfg, "claude-code").unwrap(), "claude-2");
        reg.get_mut("claude-2").unwrap().health.login_at = Some(1);
        assert_eq!(reg.create(&cfg, "claude-code").unwrap(), "claude-3");
        assert_eq!(reg.create(&cfg, "codex").unwrap(), "codex-2");
        assert!(reg.create(&cfg, "gemini").is_err());
        let names = |r: &Registry| {
            r.runtimes
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&reg),
            ["claude-1", "claude-2", "claude-3", "codex-1", "codex-2"]
        );
        let two = reg.get("claude-2").unwrap();
        assert_eq!((two.agent.as_str(), two.state(0)), ("claude", NEEDS_LOGIN));
        // No card for a runtime the owner just added.
        assert_eq!(two.health.alerted.as_deref(), Some(NEEDS_LOGIN));
        let home = dir.join("claude-2/home");
        assert_eq!(
            two.env().0,
            vec![("CLAUDE_CONFIG_DIR", home.display().to_string())]
        );
        assert_eq!(
            std::fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // It shares the owner's instructions, never the default login.
        assert_eq!(
            std::fs::read_link(home.join("CLAUDE.md")).unwrap(),
            expand_tilde("~/.claude").join("CLAUDE.md")
        );
        assert!(std::fs::symlink_metadata(home.join(".credentials.json")).is_err());
        // Its login goes to its own folder.
        two.login(&cfg, "token", "sk-ant-oat01-oat-twoxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        assert!(dir.join("claude-2/login/CLAUDE_CODE_OAUTH_TOKEN").is_file());

        // A restart loads the same runtimes, with their state.
        let mut reg = Registry::load(&cfg).unwrap();
        assert_eq!(
            names(&reg),
            ["claude-1", "claude-2", "claude-3", "codex-1", "codex-2"]
        );
        assert_eq!(reg.get("claude-3").unwrap().health.state, NEEDS_LOGIN);

        // Removal is refused while it has live runs, and works when idle.
        let e = reg.remove("claude-2", 1).unwrap_err().to_string();
        assert!(e.contains("has 1 live run"), "{e}");
        assert!(dir.join("claude-2/runtime.toml").is_file());
        let note = reg.remove("claude-2", 0).unwrap();
        assert!(note.contains("deleted with it"), "{note}");
        assert!(!dir.join("claude-2").exists());
        assert!(reg.remove("claude-2", 0).is_err());
        // Its links went with it, not what they point at.
        let codex_two = dir.join("codex-2/home");
        let target = std::fs::read_link(codex_two.join("AGENTS.md")).unwrap();
        assert_eq!(target, expand_tilde("~/.codex").join("AGENTS.md"));
        // A runtime on the CLI's default login leaves that login in place.
        let note = reg.remove("claude-1", 0).unwrap();
        assert!(
            note.contains("default login (~/.claude) was left in place"),
            "{note}"
        );
        // A login directory outside the registry is kept.
        let note = reg.remove("codex-1", 0).unwrap();
        assert!(
            note.contains("outside the registry was left in place"),
            "{note}"
        );
        assert!(tmp.path().join("codex-home").is_dir());
        // The next add reuses claude-3, still waiting for its first login.
        assert_eq!(reg.create(&cfg, "claude-code").unwrap(), "claude-3");
        let reg = Registry::load(&cfg).unwrap();
        assert_eq!(names(&reg), ["claude-3", "codex-2"]);

        // Implicit runtimes cannot change.
        let plain =
            Config::parse("[agents.claude]\ncmd = [\"claude\"]\nharness = \"claude-code\"\n")
                .unwrap();
        let mut implicit = Registry::load(&plain).unwrap();
        assert!(implicit.create(&plain, "claude-code").is_err());
        assert!(implicit.remove("claude", 0).is_err());
    }

    #[test]
    fn the_list_is_one_line_per_runtime_with_failures_only_when_not_ok() {
        let v = json!({"runtimes": [
            {"name":"claude-1","harness":"claude-code","state":"ok","in_use":1,
             "last_failure":{"class":"auth_invalid","at":1800000000},
             "quota":{"utilization":0.67,"window":"seven_day","resets_at":1800000000}},
            {"name":"claude-2","harness":"claude-code","state":"needs_login","in_use":0,
             "last_failure":null},
            {"name":"codex-1","harness":"codex","state":"degraded","in_use":0,"until":1800000000,
             "last_failure":{"class":"provider_unavailable","at":1800000000}}],
            "report_age_seconds": 1, "stale": false});
        assert_eq!(
            render(&v),
            "claude-1 (claude-code): ok, 1 live, quota 67% of seven_day (resets 2027-01-15T08:00:00Z)\n\
             claude-2 (claude-code): needs login, 0 live\n\
             codex-1 (codex): failing until 2027-01-15T08:00:00Z (last failure \
             provider_unavailable at 2027-01-15T08:00:00Z), 0 live"
        );
        // Never logged in: said so.
        let v = json!({"runtimes": [{"name":"codex-2","harness":"codex","state":"needs_login",
            "never_logged_in":true,"in_use":0}], "report_age_seconds": 1});
        assert_eq!(render(&v), "codex-2 (codex): not logged in, 0 live");
    }

    /// Adds codex-2 as /runtimes Add does, with `probe` as its login check.
    fn add_codex(cfg: &Config, reg: &mut Registry, probe: &str) -> Registry {
        assert_eq!(reg.create(cfg, "codex").unwrap(), "codex-2");
        let toml = cfg_dir(cfg).join("codex-2/runtime.toml");
        let mut text = std::fs::read_to_string(&toml).unwrap();
        text.push_str(&format!("probe = {probe}\n"));
        std::fs::write(&toml, text).unwrap();
        Registry::load(cfg).unwrap()
    }

    fn cfg_dir(cfg: &Config) -> PathBuf {
        expand_tilde(&cfg.runtimes.as_ref().unwrap().dir)
    }

    #[test]
    fn a_runtime_never_logged_in_is_reported_so_until_a_login_or_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut reg) = registry(tmp.path());
        let mut reg = add_codex(&cfg, &mut reg, "[\"false\"]");
        let never = |reg: &Registry| {
            reg.heartbeat(|_| 0, 0)["list"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["name"] == "codex-2")
                .unwrap()["never_logged_in"]
                .clone()
        };
        assert_eq!(never(&reg), true);
        // A run that failed on a login is a lost login, not a new one.
        let f = Failure {
            class: AUTH_INVALID,
            status: None,
            message: "expired".into(),
            retry_at: None,
        };
        reg.mark("codex-2", &f, 60, unix_now(), unix_now());
        assert_eq!(never(&reg), false);
    }

    #[test]
    fn an_added_runtime_whose_first_login_fails_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut reg) = registry(tmp.path());
        let mut reg = add_codex(
            &cfg,
            &mut reg,
            "[\"sh\", \"-c\", \"echo Not logged in; exit 1\"]",
        );
        reg.check_now("codex-2");
        let alerts = collect(&mut reg, |_, a| !a.is_empty());
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(
            (alerts[0].class.as_str(), alerts[0].state.as_str()),
            (LOGIN_FAILED, REMOVED)
        );
        assert!(alerts[0].message.contains("Not logged in"), "{alerts:?}");
        let text = alert_text(&alerts[0].to_json());
        assert!(text.contains("was removed again. Add it again"), "{text}");
        assert!(reg.get("codex-2").is_none());
        assert!(!cfg_dir(&cfg).join("codex-2").exists());
        // A runtime that was not added by /runtimes is never removed so.
        let tmp = tempfile::tempdir().unwrap();
        let (_, mut reg) = registry(tmp.path());
        reg.check_now("codex-1");
        let alerts = collect(&mut reg, |_, a| !a.is_empty());
        assert_eq!(alerts[0].state, NEEDS_LOGIN, "{alerts:?}");
        assert!(reg.get("codex-1").is_some());
    }

    #[test]
    fn an_added_runtime_left_without_a_login_is_removed_after_the_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut reg) = registry(tmp.path());
        let mut reg = add_codex(&cfg, &mut reg, "[\"false\"]");
        let added = reg.get("codex-2").unwrap().health.added_at.unwrap();
        assert!(reg.tick(added + ADD_LOGIN_WAIT - 1).is_empty());
        assert!(reg.get("codex-2").is_some());
        // It survives a node restart with its Add time.
        let mut reg = Registry::load(&cfg).unwrap();
        let alerts = reg.tick(added + ADD_LOGIN_WAIT);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(
            (alerts[0].class.as_str(), alerts[0].state.as_str()),
            (ADD_REMOVED, REMOVED)
        );
        assert!(alert_text(&alerts[0].to_json()).contains("got no login within 70 minutes"));
        assert!(reg.get("codex-2").is_none());
        assert!(!cfg_dir(&cfg).join("codex-2").exists());

        // Once logged in it stays, however long ago it was added.
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, mut reg) = registry(tmp.path());
        let mut reg = add_codex(&cfg, &mut reg, "[\"true\"]");
        reg.check_now("codex-2");
        let alerts = collect(&mut reg, |_, a| !a.is_empty());
        assert_eq!(alerts[0].class, LOGIN_OK);
        let h = &reg.get("codex-2").unwrap().health;
        assert_eq!((h.state.as_str(), h.added_at), (OK, None));
        assert!(reg.tick(unix_now() + 10 * ADD_LOGIN_WAIT).is_empty());
        assert!(reg.get("codex-2").is_some());
    }

    #[test]
    fn a_runtime_never_checked_needs_its_login_probe_first() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, _) = registry(tmp.path());
        let dir = cfg_dir(&cfg);
        // registry() writes no state.json: both wait for their probe, no card.
        let reg = Registry::load(&cfg).unwrap();
        for name in ["claude-1", "codex-1"] {
            let h = &reg.get(name).unwrap().health;
            assert_eq!(
                (h.state.as_str(), h.alerted.as_deref()),
                (NEEDS_LOGIN, Some(NEEDS_LOGIN))
            );
            assert!(!reg.get(name).unwrap().healthy(unix_now()));
        }
        // A runtime with no login check is trusted as before.
        std::fs::write(
            dir.join("codex-1/runtime.toml"),
            "harness = \"codex\"\nprobe = []\n",
        )
        .unwrap();
        let reg = Registry::load(&cfg).unwrap();
        assert_eq!(reg.get("codex-1").unwrap().health.state, OK);
        // A passing probe makes it ok and records the check.
        std::fs::write(
            dir.join("claude-1/runtime.toml"),
            "harness = \"claude-code\"\nprobe = [\"true\"]\n",
        )
        .unwrap();
        let mut reg = Registry::load(&cfg).unwrap();
        collect(&mut reg, |r, _| {
            r.get("claude-1").unwrap().health.state == OK
        });
        assert_eq!(reg.get("claude-1").unwrap().health.state, OK);
        let saved: Health =
            serde_json::from_slice(&std::fs::read(dir.join("claude-1/state.json")).unwrap())
                .unwrap();
        assert_eq!(saved.state, OK);
        assert!(saved.login_at.is_some());
    }
}
