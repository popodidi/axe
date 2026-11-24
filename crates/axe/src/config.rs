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

/// `[defaults]`. `agents` and `strategy` are deprecated: stored rules pick
/// agents (`rule.rs`) once the dispatcher has imported them
/// ([`Config::rules_to_import`]).
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
    /// The harness this agent drives (`rule::HARNESSES`), so selection rules
    /// can pick it. Omitted: `claude-code` when `cmd` runs `claude`, `codex`
    /// when it runs `codex`, else none (only a task or bot pin selects it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionMode {
    Preset,
    Jsonl(String),
    None,
}

impl Agent {
    /// The configured or inferred harness (see [`Agent::harness`]).
    pub fn harness(&self) -> Option<&str> {
        if let Some(h) = &self.harness {
            return Some(h);
        }
        let bin = self.cmd.first()?;
        match Path::new(bin).file_name()?.to_str()? {
            "claude" => Some("claude-code"),
            "codex" => Some("codex"),
            _ => None,
        }
    }

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

/// `[[rule]]`, matched on project. `agents` and `strategy` are deprecated
/// like those of [`Defaults`]; `max_runs` and `max_resumes` still apply.
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
    /// `[runtimes]`: the dispatcher's runtime registry (`runtime.rs`);
    /// omitted means each agent is a runtime on the default login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtimes: Option<crate::runtime::Section>,
    #[serde(rename = "rule", default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub pool: Pool,
    /// Resource admission; omitted means new sessions are not gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// `[worktrees]`: the branch prefix and folder of run workspaces
    /// (`run_workspace.rs`); omitted means the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees: Option<Worktrees>,
    /// `[repos.<name>]`: where each repo a task can name has its canonical
    /// clone; a repo not listed is `<workspace_root>/<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, Repo>,
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
    /// `[blobs]`: the server's content-addressed blob store (`blob.rs`).
    #[serde(default)]
    pub blobs: Blobs,
    /// `[docs]`: the server's versioned doc store (`doc.rs`).
    #[serde(default)]
    pub docs: Docs,
    /// `[forms]`: sealed forms (`sealed.rs`, `form.rs`).
    #[serde(default)]
    pub forms: Forms,
    /// `[cli]`: the local CLI's skill refresh and release check (`skill.rs`,
    /// `upgrade.rs`).
    #[serde(default)]
    pub cli: CliSection,
}

/// `[cli]`: how the local `axe` keeps itself and its skill current. Both
/// default on; each also has an env switch (`AXE_SKILL_REFRESH`,
/// `AXE_UPDATE_CHECK`, `0` or `false` turns it off).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliSection {
    /// Rewrite an `axe skill install` copy that is older than this binary.
    #[serde(default = "yes")]
    pub skill_refresh: bool,
    /// Check the latest GitHub release about once a day and print a hint.
    #[serde(default = "yes")]
    pub update_check: bool,
}

fn yes() -> bool {
    true
}

impl Default for CliSection {
    fn default() -> Self {
        CliSection {
            skill_refresh: true,
            update_check: true,
        }
    }
}

/// Whether a `[cli]` switch is on: `$env` when set (`0`, `false`, `off` or
/// `no` is off), else `[cli] <key>` in the config file, else on. Reads the
/// file leniently: a missing or invalid config leaves the default.
pub fn cli_switch(env: &str, key: &str) -> bool {
    if let Some(v) = std::env::var_os(env).filter(|v| !v.is_empty()) {
        let v = v.to_string_lossy().trim().to_ascii_lowercase();
        return !matches!(v.as_str(), "0" | "false" | "off" | "no");
    }
    let Ok(path) = default_path() else {
        return true;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return true;
    };
    value
        .get("cli")
        .and_then(|c| c.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// The static sealed form page every form link opens.
pub const DEFAULT_FORM_PAGE: &str = "https://popodidi.github.io/sealed-form/";

/// `[forms]`: sealed forms. The server uses `page_url`; the dispatcher its
/// key and git credential files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forms {
    /// The static form page (default [`DEFAULT_FORM_PAGE`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    /// The dispatcher's private key (default `dispatcher.key` next to the
    /// config file, `~/.config/axe/dispatcher.key`; made on first start, 0600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_file: Option<String>,
    /// The `git credential-store` file a git credential form writes and the
    /// dispatcher's clones and fetches read (default `git-credentials` next
    /// to the config file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_credentials: Option<String>,
    /// The owner's reply key for sealed messages (`sealed.rs`; default
    /// `reply.key` next to the config file, written by a reply key form, 0600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_key_file: Option<String>,
}

impl Forms {
    pub fn page(&self) -> &str {
        self.page_url.as_deref().unwrap_or(DEFAULT_FORM_PAGE)
    }
    /// `key_file`, else `dispatcher.key` next to the config file.
    pub fn key_path(&self) -> PathBuf {
        match &self.key_file {
            Some(p) => expand_tilde(p),
            None => beside_config("dispatcher.key"),
        }
    }
    /// `reply_key_file`, else `reply.key` next to the config file.
    pub fn reply_key_path(&self) -> PathBuf {
        match &self.reply_key_file {
            Some(p) => expand_tilde(p),
            None => beside_config(crate::sealed::REPLY_KEY_FILE),
        }
    }
    /// `git_credentials`, else `git-credentials` next to the config file.
    pub fn git_credentials_path(&self) -> PathBuf {
        match &self.git_credentials {
            Some(p) => expand_tilde(p),
            None => beside_config("git-credentials"),
        }
    }
}

/// `name` in the config file's directory (`~/.config/axe` by default).
fn beside_config(name: &str) -> PathBuf {
    default_path()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| expand_tilde("~/.config/axe"))
        .join(name)
}

/// `[docs]`: where `axe server` keeps versioned docs (a bare git repository).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Docs {
    /// The repository (default: `docs.git` next to the database).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// `[blobs]`: where `axe server` keeps blobs and how it signs their links.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blobs {
    /// Local store, upload spool and link key directory (default: `blobs/`
    /// next to the database).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Largest accepted blob in MiB (default 1024).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mib: Option<u64>,
    /// Base URL of the server as a phone reaches it: signed links use it
    /// (default: the URL the CLI used), and goal pins link to the goal's web
    /// page under it (no link when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// File holding the link signing key (default: `link.key` in `dir`,
    /// created on first start). Share it between servers that serve one store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_key_file: Option<String>,
    /// Store blob contents in an S3-compatible bucket instead of `dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<BlobsS3>,
}

/// `[blobs.s3]`: an S3-compatible bucket (AWS S3, MinIO, ...). Keys are read
/// from files and never stored in the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobsS3 {
    /// Service URL, e.g. `https://s3.us-east-1.amazonaws.com` or `http://minio:9000`.
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "default_s3_region")]
    pub region: String,
    /// Key prefix inside the bucket.
    #[serde(default)]
    pub prefix: String,
    /// `https://endpoint/bucket/key` (default) instead of `https://bucket.endpoint/key`.
    #[serde(default = "default_true")]
    pub path_style: bool,
    pub access_key_id_file: String,
    pub secret_access_key_file: String,
}

fn default_s3_region() -> String {
    "us-east-1".into()
}
fn default_true() -> bool {
    true
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_output: Option<HousekeepingBuildOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmp: Option<HousekeepingTmp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphan_caches: Option<HousekeepingOrphanCaches>,
}

impl Default for Housekeeping {
    fn default() -> Self {
        Housekeeping {
            retention: default_housekeeping_retention(),
            task_paths: Vec::new(),
            pressure: None,
            alert: None,
            build_output: None,
            tmp: None,
            orphan_caches: None,
        }
    }
}

/// `[housekeeping.build_output]`: strip git-ignored build output from the
/// worktrees of idle tasks (done, blocked or escalated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HousekeepingBuildOutput {
    /// How long the task has not been updated first.
    #[serde(default = "default_idle")]
    pub idle: String,
    /// Directory names removed at any depth when git ignores them.
    #[serde(default = "default_build_output_names")]
    pub names: Vec<String>,
}

/// `[housekeeping.tmp]`: per-task and stale scratch in a shared temp directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HousekeepingTmp {
    #[serde(default = "default_tmp_dir")]
    pub dir: String,
    /// A held (blocked or escalated) task's scratch goes after this idle time
    /// once it is at least `floor_mib` large.
    #[serde(default = "default_idle")]
    pub idle: String,
    #[serde(default = "default_tmp_floor_mib")]
    pub floor_mib: u64,
    /// Under pressure, other directories unused this long go.
    #[serde(default = "default_tmp_stale")]
    pub stale: String,
    /// Shared caches (name globs with `*`): file reads count as use.
    #[serde(default = "default_tmp_shared")]
    pub shared: Vec<String>,
}

/// `[housekeeping.orphan_caches]`: entries of cache roots that no task path
/// template claims, removed once unused for `unused`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HousekeepingOrphanCaches {
    pub roots: Vec<String>,
    #[serde(default = "default_orphan_unused")]
    pub unused: String,
}

fn default_idle() -> String {
    "1h".into()
}
fn default_build_output_names() -> Vec<String> {
    ["target", "node_modules", ".next", "out"]
        .map(String::from)
        .to_vec()
}
fn default_tmp_dir() -> String {
    "/tmp".into()
}
fn default_tmp_floor_mib() -> u64 {
    1024
}
fn default_tmp_stale() -> String {
    "48h".into()
}
fn default_tmp_shared() -> Vec<String> {
    [
        "node-compile-cache",
        "tsx-*",
        "playwright-transform-cache-*",
        "pytest-of-*",
        "claude-*",
        "gh-cli-cache",
    ]
    .map(String::from)
    .to_vec()
}
fn default_orphan_unused() -> String {
    "7d".into()
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
    "Work axe task {id} in project {project}: run axe task show {id} and axe task log {id}, then carry it to done. Around a merge or deploy: axe task protect {id}, then --clear.".into()
}
fn default_resume_prompt() -> String {
    "Task {id} resumed; run axe task log {id} for what changed.".into()
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
    /// Ignored: the pinned overview it configured is retired. Accepted so
    /// older configs still load.
    #[serde(default)]
    pub board: BoardMode,
    #[serde(rename = "bot", default)]
    pub bots: Vec<TelegramBot>,
    /// Ignored: each bot's retro card is set on the bot (`axe bot edit
    /// --retro*`). Accepted so older configs still load, with a warning
    /// ([`Telegram::warnings`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retro: Option<RetroSend>,
    /// The bot that sends the owner runtime cards (a runtime needs a login
    /// or hit its limit); default the first `[[telegram.bot]]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ops_bot: Option<String>,
}

/// The retired `telegram.retro` table (one global retro slot, and a home
/// bot for asks no bot raised). Its keys are still accepted and ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetroSend {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
}

impl Telegram {
    /// Settings that load but do nothing, one line each, for the server to
    /// print at startup.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.retro.is_some() {
            out.push(
                "[telegram.retro] is ignored: each bot's retro is set on the bot (axe bot edit <bot> --retro on|off --retro-every 7d --retro-at \"mon 01:00\" --retro-size 10)"
                    .to_string(),
            );
        }
        out
    }
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

/// `telegram.board`: how the retired pinned overview reached the chat. It
/// has no effect; the values are still accepted.
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

/// Every run works in a run workspace, `<run_root>/task-<id>` or
/// `<run_root>/bot-<name>` (`run_workspace.rs`). Unknown keys from older
/// configs are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktrees {
    /// Prefix of the branch each repo worktree of a run workspace checks out
    /// (`<prefix>task-<id>--work`, `<prefix>bot-<name>--work`).
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
    /// Where run workspaces go; omitted means `<workspace_root>/.runs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_root: Option<String>,
}

impl Default for Worktrees {
    fn default() -> Self {
        Worktrees {
            branch_prefix: default_branch_prefix(),
            run_root: None,
        }
    }
}

/// `[repos.<name>]`: one repo a task can work in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    /// Its canonical clone; omitted means `<workspace_root>/<name>`.
    pub path: Option<String>,
    /// Where to clone it from when the canonical clone is missing on first
    /// use (`worktree::ensure_clone`), with the host's own Git credentials;
    /// omitted means the server's repo registry (`axe repo add`, `repo.rs`)
    /// is asked. Never embeds a password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Repo {
    fn validate(&self) -> Result<()> {
        match self.url.as_deref() {
            Some(url) => validate_repo_url(url),
            None => Ok(()),
        }
    }
}

/// A repo clone URL, from `[repos.<name>] url` or the server's repo registry
/// (`repo.rs`): a Git remote that cannot be read as an option and embeds no
/// credentials.
pub fn validate_repo_url(url: &str) -> Result<()> {
    if url.trim().is_empty() || url.starts_with('-') {
        bail!("url must be a Git remote URL");
    }
    // `scheme://user:secret@host/...`: credentials stay in the host's
    // Git credential store, never in config or on the server.
    if let Some((_, rest)) = url.split_once("://") {
        let authority = rest.split('/').next().unwrap_or_default();
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            if userinfo.contains(':') {
                bail!("url must not embed credentials; use the host's Git credentials");
            }
        }
    }
    Ok(())
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
        harness: None,
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
        for (name, r) in &self.repos {
            r.validate().with_context(|| format!("repos.{name}"))?;
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
            if let Some(h) = &a.harness {
                if !crate::rule::HARNESSES.contains(&h.as_str()) {
                    bail!(
                        "agents.{name}.harness: unknown harness {h:?} (expected one of {})",
                        crate::rule::HARNESSES.join(", ")
                    );
                }
            }
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
        if self.runtimes.is_some() {
            crate::runtime::Registry::load(self).context("runtimes")?;
        }
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
            if let Some(name) = &t.ops_bot {
                if !t.bots.iter().any(|b| &b.name == name) {
                    bail!("telegram.ops_bot {name:?} is not a [[telegram.bot]]");
                }
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
    /// The canonical clone of repo `name` (`[repos.<name>] path`).
    pub fn repo_path(&self, name: &str) -> PathBuf {
        match self.repos.get(name).and_then(|r| r.path.as_deref()) {
            Some(p) => expand_tilde(p),
            None => self.workspace_root().join(name),
        }
    }
    /// `[repos.<name>] url`: where a missing canonical clone is cloned from.
    pub fn repo_url(&self, name: &str) -> Option<&str> {
        self.repos.get(name).and_then(|r| r.url.as_deref())
    }
    /// Parent of task run workspaces (`[worktrees] run_root`).
    pub fn run_root(&self) -> PathBuf {
        match self.worktrees.as_ref().and_then(|w| w.run_root.as_deref()) {
            Some(p) => expand_tilde(p),
            None => self.workspace_root().join(".runs"),
        }
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

    /// True when `[defaults]` or a `[[rule]]` sets `agents` or `strategy`:
    /// deprecated selection settings, replaced by stored rules (`rule.rs`).
    pub fn sets_legacy_selection(&self) -> bool {
        let d = Defaults::default();
        self.defaults.agents != d.agents
            || self.defaults.strategy != d.strategy
            || self
                .rules
                .iter()
                .any(|r| r.agents.is_some() || r.strategy.is_some())
    }

    /// `[defaults]` and `[[rule]]` agent lists as stored rules, for the one-time
    /// import: the default rule, then one rule per `[[rule]]` with `agents`
    /// (`match.project` becomes `match.label`; its file position is its
    /// priority, so the last applicable rule still wins). Each agent becomes
    /// its harness. Refused, with the reason, when that could change
    /// selection: an agent with no harness, an agent that is not the only
    /// agent of its harness, or a round-robin strategy.
    pub fn rules_to_import(&self) -> std::result::Result<Vec<crate::rule::NewRule>, String> {
        let harness = |a: &str| -> std::result::Result<String, String> {
            let agent = self.agents.get(a).ok_or(format!("unknown agent {a:?}"))?;
            let h = agent
                .harness()
                .ok_or(format!("agent {a:?} has no harness"))?;
            let shared: Vec<&String> = self
                .agents
                .iter()
                .filter(|(_, x)| x.harness() == Some(h))
                .map(|(n, _)| n)
                .collect();
            if shared.len() > 1 {
                return Err(format!(
                    "agents {} share harness {h}",
                    shared
                        .iter()
                        .map(|n| format!("{n:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            Ok(h.to_string())
        };
        let uses = |agents: &[String]| -> std::result::Result<Vec<crate::rule::Use>, String> {
            agents
                .iter()
                .map(|a| {
                    Ok(crate::rule::Use {
                        harness: harness(a)?,
                        model: None,
                        effort: None,
                    })
                })
                .collect()
        };
        let round_robin = |s: Option<Strategy>, at: &str| match s {
            Some(Strategy::RoundRobin) => Err(format!("{at} uses strategy round_robin")),
            _ => Ok(()),
        };
        round_robin(Some(self.defaults.strategy), "[defaults]")?;
        let mut out = vec![crate::rule::NewRule {
            matcher: crate::rule::Match::default(),
            uses: uses(&self.defaults.agents)?,
            priority: 0,
        }];
        for (i, r) in self.rules.iter().enumerate() {
            round_robin(r.strategy, &format!("rule[{i}] ({})", r.matcher.key()))?;
            let Some(agents) = &r.agents else { continue };
            out.push(crate::rule::NewRule {
                matcher: crate::rule::Match {
                    label: r.matcher.project.clone(),
                    ..Default::default()
                },
                uses: uses(agents)?,
                priority: i as i64 + 1,
            });
        }
        Ok(out)
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
/// The config `axe init` writes: a `stub` agent for a first run without a
/// model. It must parse (`tests/config.rs`).
pub const STARTER: &str = r#"# axe config: `axe config` prints it resolved; every field is in `Config`
# (crates/axe/src/config.rs). The server checks `--agent` names against this
# file; restart `axe server` after other edits.

# Tasks without --repo run in <workspace_root>/<project>; repos default to
# <workspace_root>/<name>.
workspace_root = "~/workspace"

# A stub agent: no model. It notes where it ran and marks its task done.
#   axe task add --project demo --title "first task" --agent stub
#   axe dispatch --once
# The built-in `claude` agent (unpinned tasks) needs the Claude Code CLI
# installed and logged in on the dispatcher host.
[agents.stub]
cmd = ["sh", "-c", "axe task note \"$AXE_TASK\" \"stub agent ran in $PWD\" && axe task set \"$AXE_TASK\" --state done"]
session = "none"
"#;

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
