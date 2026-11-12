use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Fixed,
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Resumes of one task session before the next run starts fresh; omitted means unlimited.
    pub max_resumes: Option<u64>,
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,
    #[serde(default = "default_strategy")]
    pub strategy: Strategy,
    #[serde(default = "default_max_runs")]
    pub max_runs: i64,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            max_resumes: None,
            agents: default_agents(),
            strategy: default_strategy(),
            max_runs: default_max_runs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub resume_cmd: Option<Vec<String>>,
    pub cmd: Vec<String>,
    /// `preset` (dispatcher passes a UUID as {session_id}), `jsonl:<key>` (parse
    /// the session id from stdout JSON lines), or `none`.
    #[serde(default = "default_session")]
    pub session: String,
    /// Bot runs only: arguments spliced in before the `{prompt}` argument of
    /// `cmd`/`resume_cmd`. May also use `{system_prompt_file}` and
    /// `{settings_file}`. Agents without it cannot run bots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMode {
    Preset,
    Jsonl(String),
    None,
}

impl Agent {
    pub fn session_mode(&self) -> Result<SessionMode> {
        match self.session.as_str() {
            "preset" => Ok(SessionMode::Preset),
            "none" => Ok(SessionMode::None),
            s => match s.strip_prefix("jsonl:") {
                Some(key) if !key.is_empty() => Ok(SessionMode::Jsonl(key.to_string())),
                _ => bail!("invalid session mode {s:?} (preset | jsonl:<key> | none)"),
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub project: Option<String>,
}

impl Match {
    fn matches(&self, project: &str) -> bool {
        self.project.as_deref().is_none_or(|p| p == project)
    }

    pub fn key(&self) -> String {
        format!("project={}", self.project.as_deref().unwrap_or("*"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub max_resumes: Option<u64>,
    #[serde(rename = "match", default)]
    pub matcher: Match,
    pub agents: Option<Vec<String>>,
    pub strategy: Option<Strategy>,
    pub max_runs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_workspace_root")]
    pub workspace_root: String,
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Seconds a drain (SIGUSR1) waits before aborting unprotected task runs.
    #[serde(default = "default_drain_abort_seconds")]
    pub drain_abort_seconds: u64,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub agents: BTreeMap<String, Agent>,
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub pool: Pool,
    /// Resource admission; omitted means new sessions are not gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// Dispatcher-owned task worktrees; omitted means sessions start in the
    /// canonical workspace as before (see `worktree.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees: Option<Worktrees>,
    /// Telegram gateway (server only); omitted means disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<Telegram>,
    #[serde(default)]
    pub otel: Otel,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub prompts: Prompts,
    #[serde(default)]
    pub dispatcher: DispatcherSection,
    /// Worktree and task-path cleanup rules; used when `[dispatcher]
    /// housekeep` is not `off` (see `housekeeping.rs`).
    #[serde(default)]
    pub housekeeping: Housekeeping,
}

/// `[dispatcher]`: switches of `axe dispatch`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatcherSection {
    /// Housekeeping tick inside the dispatcher; `axe dispatch --housekeep`
    /// overrides it.
    #[serde(default)]
    pub housekeep: Housekeep,
}

/// Whether the dispatcher's housekeeping tick reports or removes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Housekeep {
    #[default]
    Off,
    /// Print each decision, delete nothing.
    DryRun,
    /// Delete what the rules allow.
    Apply,
}

/// `[housekeeping]`: when a done task's worktrees and task paths go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Housekeeping {
    /// How long a task stays done before its worktrees and task paths go.
    #[serde(default = "default_housekeeping_retention")]
    pub retention: String,
    /// Per-task paths holding only rebuildable output, each with one `{id}`
    /// in its last component, e.g. `~/.cache/axe/task-{id}-target`.
    #[serde(default)]
    pub task_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<HousekeepingPressure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<HousekeepingAlert>,
}

impl Default for Housekeeping {
    fn default() -> Self {
        Housekeeping {
            retention: default_housekeeping_retention(),
            task_paths: Vec::new(),
            pressure: None,
            alert: None,
        }
    }
}

/// `[housekeeping.pressure]`: low free space shortens retention and trims caches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HousekeepingPressure {
    /// A path on the filesystem to watch; omitted means `workspace_root`.
    pub path: Option<String>,
    /// Under pressure while free space is strictly below this.
    pub free_below_gib: u64,
    /// Worktree retention while under pressure; task paths go at once.
    #[serde(default = "default_pressure_retention")]
    pub retention: String,
    /// Commands (absolute argv) that trim rebuildable caches under pressure.
    #[serde(default)]
    pub cache_trim: Vec<Vec<String>>,
}

/// `[housekeeping.alert]`: wake a bot once per long admission pause on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HousekeepingAlert {
    pub bot: String,
    #[serde(default = "default_alert_after")]
    pub after: String,
}

fn default_housekeeping_retention() -> String {
    "6h".into()
}
fn default_pressure_retention() -> String {
    "1h".into()
}
fn default_alert_after() -> String {
    "15m".into()
}

/// `[otel]`: `OTEL_RESOURCE_ATTRIBUTES` the dispatcher passes to each agent
/// run. Per-run keys are `<prefix>.task_id`, `<prefix>.project`,
/// `<prefix>.agent`, `<prefix>.mode` and `<prefix>.run` (bots:
/// `<prefix>.bot` and `<prefix>.bot_run` instead of the task keys).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Otel {
    #[serde(default = "default_otel_prefix")]
    pub prefix: String,
    /// Static attributes added first, for example
    /// `"service.namespace" = "agents"`.
    #[serde(default)]
    pub resource: BTreeMap<String, String>,
}

impl Otel {
    /// The static `resource` attributes, then `per_run` keys under `prefix`.
    pub fn attributes(&self, per_run: &[(&str, &str)]) -> Vec<(String, String)> {
        self.resource
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(
                per_run
                    .iter()
                    .map(|(k, v)| (format!("{}.{k}", self.prefix), v.to_string())),
            )
            .collect()
    }
}

impl Default for Otel {
    fn default() -> Self {
        Otel {
            prefix: default_otel_prefix(),
            resource: BTreeMap::new(),
        }
    }
}

fn default_otel_prefix() -> String {
    "axe".into()
}

/// `[metrics]`: Prometheus `/metrics` settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Projects reported under their own `project` label; every other
    /// project is reported as `other`, so labels stay bounded.
    #[serde(default)]
    pub projects: Vec<String>,
}

/// `[prompts]`: the dispatcher's task prompts. `{id}` and `{project}` are
/// replaced; omitted keys use the built-in text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompts {
    /// A task's first run, or a fresh session after a fallback.
    #[serde(default = "default_fresh_prompt")]
    pub fresh: String,
    /// Resuming a task's stored session.
    #[serde(default = "default_resume_prompt")]
    pub resume: String,
    /// Appended to the prompt of the run that follows a drain abort.
    #[serde(default = "default_interrupted_prompt")]
    pub interrupted: String,
}

impl Default for Prompts {
    fn default() -> Self {
        Prompts {
            fresh: default_fresh_prompt(),
            resume: default_resume_prompt(),
            interrupted: default_interrupted_prompt(),
        }
    }
}

fn default_fresh_prompt() -> String {
    "Work axe task {id} in project {project}: run axe show {id} and axe log {id}, then carry it to done. Around a merge or deploy: axe protect {id}, then --clear.".into()
}
fn default_resume_prompt() -> String {
    "Task {id} resumed; run axe log {id} for what changed.".into()
}
fn default_interrupted_prompt() -> String {
    "Your previous run was interrupted by an axe rollout, not by a failure. Its last step may or may not have taken effect: re-check real state (PR, deploy ledger, worktree) before repeating any step.".into()
}

impl Prompts {
    /// `template` with `{id}` and `{project}` replaced.
    pub fn render(template: &str, id: i64, project: &str) -> String {
        template
            .replace("{id}", &id.to_string())
            .replace("{project}", project)
    }
}

/// `[telegram]`: one long-poll gateway per bot token inside `axe server`.
/// Tokens are read from files and never stored in the config or the database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    /// Numeric Telegram user id of the only person who may control the bots.
    pub owner_user_id: i64,
    /// Bot API base URL (tests point this at a stub).
    #[serde(default = "default_telegram_api")]
    pub api_base: String,
    /// `getUpdates` long-poll timeout in seconds.
    #[serde(default = "default_telegram_poll")]
    pub poll_timeout: u64,
    /// How a changed overview reaches the chat.
    #[serde(default)]
    pub board: BoardMode,
    #[serde(rename = "bot", default)]
    pub bots: Vec<TelegramBot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramBot {
    /// The axe bot this Telegram bot talks for.
    pub name: String,
    /// File holding the BotFather token (a mounted Secret).
    pub token_file: String,
    /// Chats this bot accepts messages from and may send to.
    pub chats: Vec<i64>,
}

/// `telegram.board`: how a changed overview reaches the chat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardMode {
    /// Send the new overview silently at the bottom of the chat, pin it,
    /// unpin the old one and delete it (or edit it to "Moved below (#new)"),
    /// so the overview is always the newest pin.
    #[default]
    Repost,
    /// Edit the overview in place; it is still re-posted when a pinned card
    /// is newer than it.
    Edit,
}

fn default_telegram_api() -> String {
    "https://api.telegram.org".into()
}
fn default_telegram_poll() -> u64 {
    30
}

/// Each session runs in `<workspace_root>/.worktrees/<project>/task-<id>--work`,
/// created or reused by the dispatcher. Unknown keys from older configs are
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktrees {
    /// Prefix of the branch a new task worktree checks out
    /// (`<prefix>task-<id>--work`).
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
}

impl Default for Worktrees {
    fn default() -> Self {
        Worktrees {
            branch_prefix: default_branch_prefix(),
        }
    }
}

fn default_branch_prefix() -> String {
    "axe/".into()
}

/// Pauses new agent sessions while guest disk, memory or CPU is tight. Each
/// signal pauses past its `pause` threshold and recovers only past its
/// `resume` threshold (hysteresis); see `admission.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Filesystem to watch; omitted means `workspace_root`.
    pub path: Option<String>,
    #[serde(default = "default_sample_seconds")]
    pub sample_seconds: u64,
    /// A CPU window whose newest sample is older than this, or whose span exceeds
    /// `cpu_window_seconds + stale_seconds`, is unknown; so is a published status.
    #[serde(default = "default_stale_seconds")]
    pub stale_seconds: u64,
    #[serde(default = "default_cpu_window_seconds")]
    pub cpu_window_seconds: u64,
    #[serde(default = "default_disk_pause_below_gib")]
    pub disk_pause_below_gib: f64,
    #[serde(default = "default_disk_resume_above_gib")]
    pub disk_resume_above_gib: f64,
    #[serde(default = "default_memory_pause_below_percent")]
    pub memory_pause_below_percent: f64,
    #[serde(default = "default_memory_resume_above_percent")]
    pub memory_resume_above_percent: f64,
    #[serde(default = "default_cpu_pause_above_percent")]
    pub cpu_pause_above_percent: f64,
    #[serde(default = "default_cpu_resume_below_percent")]
    pub cpu_resume_below_percent: f64,
}

impl Default for Resources {
    fn default() -> Self {
        toml::from_str("").expect("empty resources table is valid")
    }
}

impl Resources {
    fn validate(&self) -> Result<()> {
        if self.sample_seconds == 0 {
            bail!("resources.sample_seconds must be >= 1");
        }
        if self.cpu_window_seconds < self.sample_seconds {
            bail!("resources.cpu_window_seconds must be >= sample_seconds");
        }
        if self.stale_seconds < self.sample_seconds {
            bail!("resources.stale_seconds must be >= sample_seconds");
        }
        let values = [
            ("disk_pause_below_gib", self.disk_pause_below_gib),
            ("disk_resume_above_gib", self.disk_resume_above_gib),
            (
                "memory_pause_below_percent",
                self.memory_pause_below_percent,
            ),
            (
                "memory_resume_above_percent",
                self.memory_resume_above_percent,
            ),
            ("cpu_pause_above_percent", self.cpu_pause_above_percent),
            ("cpu_resume_below_percent", self.cpu_resume_below_percent),
        ];
        for (name, v) in values {
            if !v.is_finite() || v < 0.0 {
                bail!("resources.{name} must be a finite number >= 0");
            }
            if name.ends_with("_percent") && v > 100.0 {
                bail!("resources.{name} must be <= 100");
            }
        }
        if self.disk_resume_above_gib <= self.disk_pause_below_gib {
            bail!("resources.disk_resume_above_gib must be greater than disk_pause_below_gib");
        }
        if self.memory_resume_above_percent <= self.memory_pause_below_percent {
            bail!("resources.memory_resume_above_percent must be greater than memory_pause_below_percent");
        }
        if self.cpu_resume_below_percent >= self.cpu_pause_above_percent {
            bail!("resources.cpu_resume_below_percent must be less than cpu_pause_above_percent");
        }
        Ok(())
    }
}

/// Agent admission: at most `total` concurrent task agents, one task each, and
/// a separate cap of `bots` concurrent bot runs. Neither counts against the
/// other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    #[serde(default = "default_pool_total")]
    pub total: usize,
    /// Concurrent bot runs; omitted means one slot per active bot (a bot has
    /// at most one live run, so bots are never capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bots: Option<usize>,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            total: default_pool_total(),
            bots: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Browser origins allowed to read the GET API.
    #[serde(default = "default_cors_origins")]
    pub cors_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            cors_origins: default_cors_origins(),
        }
    }
}

/// Effective dispatch settings for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub max_resumes: Option<u64>,
    pub agents: Vec<String>,
    pub strategy: Strategy,
    pub max_runs: i64,
    /// Round-robin cursor key: the match of the rule that supplied `agents`.
    pub rule_key: String,
}

fn default_workspace_root() -> String {
    "~/workspace".into()
}
fn default_log_dir() -> String {
    "~/.local/state/axe/logs".into()
}
fn default_poll_seconds() -> u64 {
    60
}
fn default_drain_abort_seconds() -> u64 {
    900
}
fn default_agents() -> Vec<String> {
    vec!["claude".into()]
}
fn default_strategy() -> Strategy {
    Strategy::Fixed
}
fn default_max_runs() -> i64 {
    6
}
fn default_pool_total() -> usize {
    1
}
fn default_cors_origins() -> Vec<String> {
    Vec::new()
}
fn default_sample_seconds() -> u64 {
    10
}
fn default_stale_seconds() -> u64 {
    30
}
fn default_cpu_window_seconds() -> u64 {
    60
}
fn default_disk_pause_below_gib() -> f64 {
    15.0
}
fn default_disk_resume_above_gib() -> f64 {
    20.0
}
fn default_memory_pause_below_percent() -> f64 {
    15.0
}
fn default_memory_resume_above_percent() -> f64 {
    20.0
}
fn default_cpu_pause_above_percent() -> f64 {
    90.0
}
fn default_cpu_resume_below_percent() -> f64 {
    80.0
}
fn default_session() -> String {
    "none".into()
}

fn builtin_claude() -> Agent {
    Agent {
        resume_cmd: Some(
            [
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--resume",
                "{session_id}",
                "--permission-mode",
                "bypassPermissions",
                "{prompt}",
            ]
            .map(String::from)
            .to_vec(),
        ),
        cmd: [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--session-id",
            "{session_id}",
            "--permission-mode",
            "bypassPermissions",
            "{prompt}",
        ]
        .map(String::from)
        .to_vec(),
        session: "preset".into(),
        bot_args: Some(
            [
                "--system-prompt-snapshot",
                "off",
                "--append-system-prompt-file",
                "{system_prompt_file}",
                "--settings",
                "{settings_file}",
            ]
            .map(String::from)
            .to_vec(),
        ),
    }
}

impl Default for Config {
    fn default() -> Self {
        Config::parse("").expect("empty config is valid")
    }
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("AXE_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    dirs::home_dir()
        .map(|h| h.join(".config/axe/config.toml"))
        .context("cannot determine home directory")
}

/// Bearer token file: `$AXE_TOKEN_FILE`, default `~/.config/axe/token`.
pub fn token_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("AXE_TOKEN_FILE").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    dirs::home_dir()
        .map(|h| h.join(".config/axe/token"))
        .context("cannot determine home directory")
}

/// Reads a token file; a missing or blank file means no token.
pub fn read_token(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s.trim().to_string()).filter(|t| !t.is_empty())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(p));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => Config::parse(&s).with_context(|| format!("config {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn parse(s: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(s)?;
        cfg.agents
            .entry("claude".into())
            .or_insert_with(builtin_claude);
        cfg.validate()?;
        Ok(cfg)
    }

    /// A bot's agent must exist and define `bot_args`.
    pub fn validate_bot_agent(&self, agent: &str) -> Result<()> {
        match self.agents.get(agent) {
            None => bail!("unknown agent {agent:?}"),
            Some(a) if a.bot_args.is_none() => {
                bail!("agent {agent:?} has no bot_args; it cannot run bots")
            }
            Some(_) => Ok(()),
        }
    }

    pub fn validate_agent(&self, agent: Option<&str>) -> Result<()> {
        if let Some(name) = agent {
            if !self.agents.contains_key(name) {
                bail!("unknown agent {name:?}");
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.poll_seconds == 0 {
            bail!("poll_seconds must be >= 1");
        }
        for (name, a) in &self.agents {
            if a.resume_cmd.as_ref().is_some_and(|c| c.is_empty()) {
                bail!("agents.{name}.resume_cmd is empty");
            }
            if a.cmd.is_empty() {
                bail!("agents.{name}.cmd is empty");
            }
            a.session_mode()
                .with_context(|| format!("agents.{name}.session"))?;
            for arg in a.cmd.iter().chain(a.resume_cmd.iter().flatten()) {
                check_vars(arg, &[]).with_context(|| format!("agents.{name}"))?;
            }
            for arg in a.bot_args.iter().flatten() {
                check_vars(arg, &BOT_VARS).with_context(|| format!("agents.{name}.bot_args"))?;
            }
        }
        let check_agents = |list: &[String], at: &str| -> Result<()> {
            if list.is_empty() {
                bail!("{at}: agents list is empty");
            }
            for a in list {
                if !self.agents.contains_key(a) {
                    bail!("{at}: unknown agent {a:?}");
                }
            }
            Ok(())
        };
        check_agents(&self.defaults.agents, "defaults")?;
        for (i, r) in self.rules.iter().enumerate() {
            if let Some(a) = &r.agents {
                check_agents(a, &format!("rule[{i}] ({})", r.matcher.key()))?;
            }
        }
        for p in &self.metrics.projects {
            if p.is_empty()
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            {
                bail!("metrics.projects: {p:?} must be ASCII letters, digits, '-', '_' or '.'");
            }
            if crate::metrics::RESERVED_PROJECTS.contains(&p.as_str()) {
                bail!("metrics.projects: {p:?} is a reserved label");
            }
        }
        if self.pool.total == 0 {
            bail!("pool.total must be >= 1");
        }
        if self.pool.bots == Some(0) {
            bail!("pool.bots must be >= 1 (omit it for one slot per active bot)");
        }
        if let Some(r) = &self.resources {
            r.validate()?;
        }
        crate::housekeeping::Rules::new(self).context("housekeeping")?;
        if let Some(t) = &self.telegram {
            if t.owner_user_id <= 0 {
                bail!("telegram.owner_user_id must be the owner's numeric user id");
            }
            if t.poll_timeout == 0 || t.poll_timeout > 50 {
                bail!("telegram.poll_timeout must be 1..=50 seconds");
            }
            let mut names = std::collections::BTreeSet::new();
            for b in &t.bots {
                if !names.insert(&b.name) {
                    bail!("telegram.bot {:?} is configured twice", b.name);
                }
                if b.chats.is_empty() {
                    bail!("telegram.bot {:?}: chats allowlist is empty", b.name);
                }
                if b.token_file.trim().is_empty() {
                    bail!("telegram.bot {:?}: token_file is empty", b.name);
                }
            }
        }
        Ok(())
    }

    /// Concurrent task agents the dispatcher runs.
    pub fn pool_total(&self) -> usize {
        self.pool.total
    }

    /// Concurrent bot runs, separate from `pool_total`; `None` means one per
    /// active bot.
    pub fn pool_bots(&self) -> Option<usize> {
        self.pool.bots
    }

    pub fn workspace_root(&self) -> PathBuf {
        expand_tilde(&self.workspace_root)
    }
    pub fn log_dir(&self) -> PathBuf {
        expand_tilde(&self.log_dir)
    }
    /// Filesystem watched by resource admission.
    pub fn resources_path(&self) -> PathBuf {
        match self.resources.as_ref().and_then(|r| r.path.as_deref()) {
            Some(p) => expand_tilde(p),
            None => self.workspace_root(),
        }
    }

    pub fn resolve(&self, project: &str) -> Resolved {
        // Catch-all rules first, then project rules, each in file order, so the last
        // applicable rule wins.
        let mut rules: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|r| r.matcher.matches(project))
            .collect();
        rules.sort_by_key(|r| r.matcher.project.is_some());

        let d = &self.defaults;
        let mut out = Resolved {
            max_resumes: d.max_resumes,
            agents: d.agents.clone(),
            strategy: d.strategy,
            max_runs: d.max_runs,
            rule_key: "defaults".into(),
        };
        for r in rules {
            if let Some(m) = r.max_resumes {
                out.max_resumes = Some(m);
            }
            if let Some(a) = &r.agents {
                out.agents = a.clone();
                out.rule_key = r.matcher.key();
            }
            if let Some(s) = r.strategy {
                out.strategy = s;
            }
            if let Some(m) = r.max_runs {
                out.max_runs = m;
            }
        }
        out
    }
}

/// Placeholders an agent command template may use.
pub const TEMPLATE_VARS: [&str; 5] = ["session_id", "prompt", "cwd", "task_id", "project"];

/// Extra placeholders only `bot_args` may use.
pub const BOT_VARS: [&str; 2] = ["system_prompt_file", "settings_file"];

/// Rejects `{name}` placeholders that are not in [`TEMPLATE_VARS`] or `extra`.
fn check_vars(arg: &str, extra: &[&str]) -> Result<()> {
    let mut rest = arg;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else { break };
        let name = &after[..end];
        let ident = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if ident && !TEMPLATE_VARS.contains(&name) && !extra.contains(&name) {
            bail!(
                "unknown template variable {{{name}}} (expected one of {})",
                TEMPLATE_VARS
                    .iter()
                    .chain(extra)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        rest = &after[end + 1..];
    }
    Ok(())
}
