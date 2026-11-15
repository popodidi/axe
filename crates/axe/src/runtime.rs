//! Runtimes: a harness plus exactly one login, held by the dispatcher.
//!
//! With `[runtimes] dir` set, each `<dir>/<name>/runtime.toml` is one runtime:
//! its harness, the `[agents.*]` command template that drives it and the login
//! directory the CLI keeps its credentials in. The dispatcher never reads,
//! copies or writes the login itself: it only points the CLI at its directory
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), and the CLI's own default directory is
//! adopted in place by setting nothing. Each runtime's health (ok, limited
//! until a time, needs_login, degraded, disabled) lives in `<name>/state.json`
//! next to it; a run's failure class (`failure.rs`) moves it, and the server
//! sees it only through [`Registry::heartbeat`], which carries no credential,
//! path or token.
//!
//! Without `[runtimes]`, every `[agents.*]` entry is a runtime of its own on
//! the dispatcher's default login, always healthy: selection and retries work
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

/// How long a runtime stays limited when the quota reset time is unknown.
pub const LIMITED_DEFAULT_SECONDS: i64 = 3600;
/// How often a runtime that needs a login is probed (`probe`).
pub const PROBE_EVERY: Duration = Duration::from_secs(300);
/// Longest a probe may run before it counts as failed.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest failure message kept (as `failure::Failure::to_json`).
const MAX_MESSAGE: usize = 200;

/// `[runtimes]` in the dispatcher config.
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
}

impl Default for Health {
    fn default() -> Self {
        Health {
            state: OK.into(),
            until: None,
            last_failure: None,
            quota: None,
            alerted: None,
        }
    }
}

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

/// Variables a runtime sets and removes in its CLI's environment.
pub type LoginEnv = (Vec<(&'static str, String)>, Vec<&'static str>);

/// The CLI's login directory variable and default directory, per harness.
fn login_var(harness: &str) -> Option<(&'static str, &'static str)> {
    match harness {
        "claude-code" => Some(("CLAUDE_CONFIG_DIR", "~/.claude")),
        "codex" => Some(("CODEX_HOME", "~/.codex")),
        _ => None,
    }
}

/// Exits 0 while logged in: `claude auth status`, `codex login status`.
fn default_probe(harness: &str) -> Vec<String> {
    let argv: &[&str] = match harness {
        "claude-code" => &["claude", "auth", "status"],
        "codex" => &["codex", "login", "status"],
        _ => &[],
    };
    argv.iter().map(|s| s.to_string()).collect()
}

impl Runtime {
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
        match &self.login_dir {
            Some(d) => (vec![(var, d.display().to_string())], vec![]),
            None => (vec![], vec![var]),
        }
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
    probes: BTreeMap<String, (Child, Instant)>,
    probed: BTreeMap<String, Instant>,
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
        // Never two runtimes on one login (#2779): quota and expiry are a login's.
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
        Ok(Registry::new(runtimes, true))
    }

    pub fn new(runtimes: Vec<Runtime>, managed: bool) -> Registry {
        Registry {
            runtimes,
            managed,
            probes: BTreeMap::new(),
            probed: BTreeMap::new(),
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
    pub fn mark(&mut self, name: &str, f: &Failure, backoff: i64, now: i64) -> Option<Alert> {
        if !self.managed {
            return None;
        }
        let rt = self.get_mut(name)?;
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
            eprintln!("axe dispatcher: runtime {name}: {e:#}");
        }
        alert
    }

    /// The owner got the card for `state` of `name`.
    pub fn alerted(&mut self, name: &str, state: &str) {
        if let Some(rt) = self.get_mut(name) {
            rt.health.alerted = Some(state.into());
            if let Err(e) = rt.save() {
                eprintln!("axe dispatcher: runtime {name}: {e:#}");
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
            eprintln!("axe dispatcher: runtime {name}: {e:#}");
        }
    }

    /// Lapsed limits return to ok, and the card episode ends with them.
    pub fn tick(&mut self, now: i64) {
        for rt in &mut self.runtimes {
            if matches!(rt.health.state.as_str(), LIMITED | DEGRADED)
                && rt.health.until.is_none_or(|u| u <= now)
            {
                rt.health.state = OK.into();
                rt.health.until = None;
                rt.health.alerted = None;
                if let Err(e) = rt.save() {
                    eprintln!("axe dispatcher: runtime {}: {e:#}", rt.name);
                }
            }
        }
    }

    /// Probes runtimes that need a login, at most every [`PROBE_EVERY`], with
    /// the harness's status command in the runtime's environment: a zero exit
    /// makes the runtime ok again. A probe never marks a runtime unhealthy.
    /// Non-blocking: a started probe is collected on a later call.
    pub fn probe(&mut self) {
        let mut done = Vec::new();
        for (name, (child, started)) in &mut self.probes {
            match child.try_wait() {
                Ok(Some(status)) => done.push((name.clone(), status.success())),
                Ok(None) if started.elapsed() < PROBE_TIMEOUT => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    done.push((name.clone(), false));
                }
            }
        }
        for (name, ok) in done {
            self.probes.remove(&name);
            if !ok {
                continue;
            }
            let Some(rt) = self.get_mut(&name) else {
                continue;
            };
            if rt.health.state == NEEDS_LOGIN {
                eprintln!("axe dispatcher: runtime {name}: login probe passed; ok again");
                rt.health.state = OK.into();
                rt.health.until = None;
                rt.health.alerted = None;
                if let Err(e) = rt.save() {
                    eprintln!("axe dispatcher: runtime {name}: {e:#}");
                }
            }
        }
        if !self.managed {
            return;
        }
        let due: Vec<(String, Vec<String>, LoginEnv)> = self
            .runtimes
            .iter()
            .filter(|r| r.health.state == NEEDS_LOGIN && !r.disabled && !r.probe.is_empty())
            .filter(|r| !self.probes.contains_key(&r.name))
            .filter(|r| {
                self.probed
                    .get(&r.name)
                    .is_none_or(|at| at.elapsed() >= PROBE_EVERY)
            })
            .map(|r| (r.name.clone(), r.probe.clone(), r.env()))
            .collect();
        for (name, argv, (set, remove)) in due {
            self.probed.insert(name.clone(), Instant::now());
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for v in remove {
                cmd.env_remove(v);
            }
            for (k, v) in set {
                cmd.env(k, v);
            }
            match cmd.spawn() {
                Ok(child) => {
                    self.probes.insert(name, (child, Instant::now()));
                }
                Err(e) => eprintln!("axe dispatcher: runtime {name}: probe {argv:?}: {e}"),
            }
        }
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
                    "plan": r.plan,
                    "state": r.state(now),
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

impl Runtime {
    fn read(cfg: &Config, name: &str, path: &Path) -> Result<Runtime> {
        crate::db::validate_name("runtime", name)?;
        let text = std::fs::read_to_string(path)?;
        let f: File = toml::from_str(&text)?;
        let Some((_, default_dir)) = login_var(&f.harness) else {
            bail!(
                "unknown harness {:?} (expected one of {})",
                f.harness,
                crate::rule::HARNESSES.join(", ")
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
            state_path: Some(state_path),
            health,
        })
    }
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

/// `GET /api/runtimes`: the runtimes in the dispatcher's last report (an
/// admission report, `admission::reported`), and whether it is stale.
pub fn view(admission: &Value) -> Value {
    let age = admission["report_age_seconds"].as_u64();
    let stale = age.is_none_or(|a| a > crate::admission::REPORT_STALE.as_secs());
    json!({
        "managed": admission["runtimes"]["managed"],
        "runtimes": admission["runtimes"]["list"].as_array().cloned().unwrap_or_default(),
        "report_age_seconds": age,
        "stale": stale,
    })
}

fn when(t: &Value) -> String {
    t.as_i64().map_or("-".into(), crate::bot::ts)
}

/// One line per runtime of a [`view`], for `axe runtimes` and `/runtimes`.
pub fn render(v: &Value) -> String {
    let list = v["runtimes"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return match v["report_age_seconds"].as_u64() {
            None => {
                "runtimes: unknown (no dispatcher has reported since the server started)".into()
            }
            Some(_) => "runtimes: none reported".into(),
        };
    }
    let mut out = Vec::new();
    for r in &list {
        let mut line = format!(
            "{} ({}): {}",
            r["name"].as_str().unwrap_or("?"),
            r["harness"].as_str().unwrap_or("?"),
            r["state"].as_str().unwrap_or("?")
        );
        if !r["until"].is_null() {
            line.push_str(&format!(" until {}", when(&r["until"])));
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
        let f = &r["last_failure"];
        if let Some(c) = f["class"].as_str() {
            line.push_str(&format!("\n  last failure {} at {}", c, when(&f["at"])));
        }
        out.push(line);
    }
    if v["stale"] == true {
        out.push(format!(
            "(stale: the dispatcher last reported {}s ago)",
            v["report_age_seconds"].as_u64().unwrap_or(0)
        ));
    }
    out.join("\n")
}

/// The owner card for an [`Alert`] (its JSON form).
pub fn alert_text(a: &Value) -> String {
    let name = a["runtime"].as_str().unwrap_or("?");
    let harness = a["harness"].as_str().unwrap_or("?");
    let message = a["message"].as_str().unwrap_or("");
    let head = match a["state"].as_str() {
        Some(NEEDS_LOGIN) => format!(
            "Runtime {name} ({harness}) needs a new login. Its runs go to other healthy \
             runtimes meanwhile; it is probed every {} minutes and is ok again once the \
             login works.",
            PROBE_EVERY.as_secs() / 60
        ),
        Some(LIMITED) => format!(
            "Runtime {name} ({harness}) hit its usage limit until {}. Its runs go to other \
             healthy runtimes or wait for the reset.",
            when(&a["until"])
        ),
        s => format!("Runtime {name} ({harness}) is {}.", s.unwrap_or("?")),
    };
    match message.is_empty() {
        true => head,
        false => format!("{head}\n\n{} {message}", a["class"].as_str().unwrap_or("")),
    }
}
