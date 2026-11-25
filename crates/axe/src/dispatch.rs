use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::remote::Store;
use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::activity;
use crate::admission::{self, Gate};
use crate::agent_proc::{self, kill_child, kill_tree};
pub use crate::agent_proc::{otel_resource_attributes, parse_jsonl_session, render};
use crate::config::{Config, Prompts, SessionMode, Strategy};
use crate::db::{self, Changes, Claim, Task};
use crate::rule::{self, Use};
use crate::runtime::{self, Registry};

mod bots;
mod pre_check;
pub mod services;
pub use bots::splice_bot_args;

const TICK: Duration = Duration::from_secs(2);
/// Longest gap between admission reports while no new sample was taken.
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// How often a drain past its deadline re-checks protected runs.
const DRAIN_CHECK: Duration = Duration::from_secs(10);
/// How long a drain lets live bot runs finish once every task run has ended.
const DRAIN_BOT_GRACE: Duration = Duration::from_secs(60);
/// `finish` note of a run aborted by a bounded drain.
const DRAIN_ABORT_NOTE: &str = "aborted by rollout drain";

/// The runtime of a new run (`runtime.rs`), what picked it, and the `use`
/// entry whose model and effort it runs with (none: the agent's own defaults).
#[derive(Debug, Clone)]
struct Pick {
    /// The runtime's name; without `[runtimes]`, the agent's.
    agent: String,
    rule: String,
    with: Option<Use>,
    /// The cost controls of the routine that created the task, if any.
    profile: Option<crate::routine::Profile>,
}

/// Why no runtime was picked.
#[derive(Debug)]
enum NoPick {
    /// Nothing configured can run it: the task escalates.
    Missing(String),
    /// Runtimes exist but none is healthy now: the task waits until `until`.
    Unhealthy { until: i64, note: String },
}

impl NoPick {
    fn note(self) -> String {
        match self {
            NoPick::Missing(n) | NoPick::Unhealthy { note: n, .. } => n,
        }
    }
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

struct Run {
    resume_count: u64,
    resumed: bool,
    task_id: i64,
    claim: String,
    session_mode: SessionMode,
    session_id: Option<String>,
    log_path: PathBuf,
    pick: Pick,
    child: Child,
    /// Row in the `runs` table.
    run_id: i64,
    activity: Option<String>,
    log_len: u64,
    /// The server recorded this run's drain abort; only the kill may remain.
    drain_aborting: bool,
    /// Unix seconds when the run read its runtime's login.
    started_at: i64,
}

/// Progress of a bounded drain, from the first tick that saw SIGUSR1.
struct Drain {
    abort_at: Instant,
    next_check: Instant,
    /// Set when the last task run ends; bots are not admitted after it.
    bots_until: Option<Instant>,
}

pub struct Dispatcher {
    cfg: Config,
    store: Store,
    runs: Vec<Run>,
    server_url: Option<String>,
    /// Resource admission; `None` when `[resources]` is not configured.
    gate: Option<Gate>,
    admission: admission::Handle,
    /// Fresh-session fallbacks held by a paused gate. Each keeps its claim and
    /// slot (and spends no further run budget) until admission reopens.
    deferred: Vec<Deferred>,
    /// Routine pre-checks running before a task's first agent run; each holds
    /// its claim and a task slot (`dispatch/pre_check.rs`).
    pre_checks: Vec<pre_check::PreCheck>,
    /// The server refused `routine_profile` (an older server); logged once.
    profiles_unsupported: bool,
    /// Gate sample count, (task, bot) slot use, hold and time of the last
    /// admission report to the server.
    reported: Option<(u64, (usize, usize), Value, Instant)>,
    /// The manual admission hold file (`axe admission hold`).
    hold_path: PathBuf,
    /// Hold reason last written to the journal, so only changes are logged.
    hold_logged: Option<String>,
    /// Live bot runs, in their own pool (see `dispatch/bots.rs`).
    bots: Vec<bots::BotRun>,
    /// The server refused bot operations (an older server); logged once.
    bots_unsupported: bool,
    drain: Option<Drain>,
    /// Stored rules pick agents (`rule.rs`); false while the deprecated
    /// `[defaults]`/`[[rule]]` config does (see [`Dispatcher::init_rules`]).
    stored_rules: bool,
    /// The runtimes runs go to, with their health (`runtime.rs`).
    runtimes: Registry,
    /// Cards to the owner not yet accepted by the server.
    alerts: Vec<runtime::Alert>,
    /// The last alert send failed; logged once per streak.
    alert_failed: bool,
    /// Why claims are held for runtime health, as last logged.
    runtime_hold_logged: Option<String>,
    /// Bots held for runtime health, with the reason last logged.
    bots_held: BTreeMap<String, String>,
    /// The sealed form key's public half (`sealed::Desk::public`), reported
    /// to the server; null when the key does not exist.
    form_key: Value,
    /// Bot services (`dispatch/services.rs`); run only by a long-running
    /// dispatcher (not `--once`).
    services: services::Supervisor,
    /// Service notices not yet accepted by the server, oldest first.
    service_notices: Vec<crate::service::Notice>,
    /// When services last synced with the server.
    service_sync: Option<Instant>,
    /// The last service sync or notice failed; logged once per streak.
    service_sync_failed: bool,
    /// A drain woke the owning bots: services keep running until then.
    services_warned_until: Option<Instant>,
}

/// The public half of the sealed form key at `path`, when it exists (the
/// control connection makes it at start).
fn form_key(path: &std::path::Path) -> Value {
    if !path.exists() {
        return Value::Null;
    }
    match crate::sealed::Key::load_or_create(path) {
        Ok(k) => json!({"pubkey": k.pubkey(), "fingerprint": k.fingerprint()}),
        Err(e) => {
            eprintln!("axe node: sealed form key: {e:#}");
            Value::Null
        }
    }
}

/// A resume-to-fresh fallback waiting for resource admission.
struct Deferred {
    task_id: i64,
    claim: String,
    pick: Pick,
}

/// Escalates a node-claimed task; its session resumes once ops resolves the cause.
fn block_changes(task_id: i64, blocker: &str) -> Changes {
    Changes {
        state: Some("escalated".into()),
        note: Some(db::block_note(
            blocker,
            "ops: inspect the run log and fix the cause",
            &format!("axe task set {task_id} --state pending"),
        )),
        ..Default::default()
    }
}

fn dep_lines(label: &str, deps: &[db::DepRef]) -> String {
    let mut s = format!("{label}:");
    if deps.is_empty() {
        s.push_str(" -");
    }
    for d in deps {
        s.push_str(&format!(
            "\n  #{} [{}] {} {}",
            d.id, d.project, d.status, d.title
        ));
    }
    s
}

/// Derived state label shared by `axe task list` and `axe task show`: dependency
/// waits read `blocked (deps #1, #2)` (`hold+deps` with a stored hold), wake waits
/// read `waiting until <time>`; otherwise the derived status. The web mirrors it
/// in `taskStatusLabel` (`web/lib/model.ts`).
pub fn status_label(t: &Task) -> String {
    if t.state == "pending" && t.waiting_on.is_empty() && t.sleeping() {
        return format!("waiting until {}", t.not_before.as_deref().unwrap_or("?"));
    }
    if t.waiting_on.is_empty() || t.status != "blocked" {
        return t.status.clone();
    }
    let kind = if t.state == "blocked" {
        "hold+deps"
    } else {
        "deps"
    };
    format!("blocked ({kind} {})", db::ids(&t.waiting_on))
}

/// Why the task cannot be claimed, or `None` when it is ready, running or done.
pub fn wait_reason(t: &Task) -> Option<String> {
    let deps = (!t.waiting_on.is_empty())
        .then(|| format!("waiting on prerequisites {}", db::ids(&t.waiting_on)));
    match (t.state.as_str(), deps) {
        ("blocked" | "escalated", Some(d)) => Some(format!(
            "hold (resolve it explicitly) and {d}; both must clear"
        )),
        ("blocked" | "escalated", None) => {
            Some("hold: resolve it explicitly, then set pending".into())
        }
        ("pending", Some(d)) => Some(format!("{d}; becomes ready automatically")),
        ("pending", None) if t.sleeping() => {
            Some(format!("{}; becomes ready automatically", status_label(t)))
        }
        (_, Some(d)) => Some(format!("{d} (prerequisite reopened; not cancelled)")),
        _ => None,
    }
}

/// Prerequisites and dependents with their status.
pub fn format_deps(t: &Task) -> String {
    let mut s = format!(
        "{}\n{}",
        dep_lines("blocked_by", &t.deps.blocked_by),
        dep_lines("blocks", &t.deps.blocks)
    );
    if let Some(r) = wait_reason(t) {
        s.push_str(&format!("\nwaiting: {r}"));
    }
    s
}

pub fn format_show(t: &Task) -> String {
    let label = status_label(t);
    let state = if t.status == t.state {
        label
    } else {
        format!("{label} (stored {})", t.state)
    };
    let mut s = format!(
        "id: {}\nproject: {}\ntitle: {}\nstate: {}\nclaimed_by: {}\nruns: {}\ncreated_at: {}\nupdated_at: {}",
        t.id,
        t.project,
        t.title,
        state,
        t.claimed_by.as_deref().unwrap_or("-"),
        t.runs,
        t.created_at,
        t.updated_at,
    );
    s.push_str(&format!("\nagent: {}", t.agent.as_deref().unwrap_or("-")));
    s.push_str(&format!("\nowner: {}", t.owner.as_deref().unwrap_or("-")));
    let repos = match t.repos.as_deref() {
        Some(r) if !r.is_empty() => r.join(", "),
        _ => "none".into(),
    };
    s.push_str(&format!("\nrepos: {repos}"));
    if !t.labels.is_empty() {
        s.push_str(&format!("\nlabels: {}", t.labels.join(", ")));
    }
    if let Some(at) = &t.not_before {
        s.push_str(&format!("\nnot_before: {at}"));
    }
    if !t.deps.blocked_by.is_empty() || !t.deps.blocks.is_empty() {
        s.push('\n');
        s.push_str(&format_deps(t));
    }
    if !t.detail.is_empty() {
        s.push_str("\n\n");
        s.push_str(&t.detail);
    }
    s
}

/// Cuts every line longer than `max` chars to `max` chars plus `…`.
pub fn truncate_lines(text: &str, max: usize) -> String {
    text.split('\n')
        .map(|line| match line.char_indices().nth(max) {
            Some((i, _)) => format!("{}…", &line[..i]),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Formats one event; with `body_max`, body lines are cut by [`truncate_lines`].
pub fn format_event(e: &db::Event, body_max: Option<usize>) -> String {
    let mut s = format!("{} {} {}", e.at, e.actor, e.kind);
    match (&e.from_value, &e.to_value) {
        (Some(f), Some(t)) => s.push_str(&format!(" {f} -> {t}")),
        (None, Some(t)) => s.push_str(&format!(" {t}")),
        _ => {}
    }
    if let Some(b) = &e.body {
        match body_max {
            Some(max) => s.push_str(&format!(": {}", truncate_lines(b, max))),
            None => s.push_str(&format!(": {b}")),
        }
    }
    s
}

/// Built-in prompt for a task's first run, or a fresh session after a
/// fallback (`[prompts] fresh`).
pub fn fresh_prompt(id: i64, project: &str) -> String {
    Prompts::render(&Prompts::default().fresh, id, project)
}

/// Built-in prompt for resuming a task's stored session (`[prompts] resume`).
pub fn resume_prompt(id: i64) -> String {
    Prompts::render(&Prompts::default().resume, id, "")
}

/// Built-in text appended to the prompt of the run that follows a drain
/// abort (`[prompts] interrupted`).
pub fn interrupted_notice() -> String {
    Prompts::default().interrupted
}

impl Dispatcher {
    pub fn new(cfg: Config, store: Store) -> Self {
        let url = store.client.base().to_string();
        let cfg_forms_path = cfg.forms.key_path();
        let gate = cfg
            .resources
            .clone()
            .map(|r| Gate::new(r, cfg.resources_path()));
        let admission = match &gate {
            Some(g) => g.handle(),
            None => admission::handle(None),
        };
        // The config was validated with its registry; a registry that broke
        // since then leaves runs on the default logins, loudly.
        let runtimes = Registry::load(&cfg).unwrap_or_else(|e| {
            eprintln!(
                "axe node: runtimes unavailable ({e:#}); each agent runs on the default login"
            );
            let mut plain = cfg.clone();
            plain.runtimes = None;
            Registry::load(&plain).expect("implicit runtimes always load")
        });
        Dispatcher {
            store,
            runs: Vec::new(),
            server_url: Some(url.clone()),
            gate,
            admission,
            deferred: Vec::new(),
            pre_checks: Vec::new(),
            profiles_unsupported: false,
            reported: None,
            hold_path: admission::hold_path(),
            hold_logged: None,
            bots: Vec::new(),
            bots_unsupported: false,
            drain: None,
            stored_rules: false,
            runtimes,
            alerts: Vec::new(),
            alert_failed: false,
            runtime_hold_logged: None,
            bots_held: BTreeMap::new(),
            form_key: form_key(&cfg_forms_path),
            services: services::Supervisor::new(
                cfg.services.clone(),
                cfg.log_dir(),
                cfg.run_root(),
                Some(url),
            ),
            service_notices: Vec::new(),
            service_sync: None,
            service_sync_failed: false,
            services_warned_until: None,
            cfg,
        }
    }

    /// Chooses how new sessions pick their agent, once at start. The config's
    /// `[defaults]`/`[[rule]]` agent lists are imported as stored rules the
    /// first time (`rule::import`); from then on stored rules pick. The config
    /// keeps picking (deprecated) when stored rules cannot express it
    /// ([`Config::rules_to_import`]) or the server has no rules.
    fn init_rules(&mut self) {
        let rules = match self.cfg.rules_to_import() {
            Ok(r) => r,
            Err(reason) => {
                eprintln!(
                    "axe node: agents picked by the deprecated [defaults]/[[rule]] config: \
                     stored rules cannot express it ({reason})"
                );
                return;
            }
        };
        let body = json!({"rules":rules,"source":"dispatcher config [defaults]/[[rule]]"});
        match self.store.request::<Value>("rule_import", body) {
            Ok(v) => {
                self.stored_rules = true;
                if v["already"] != true {
                    eprintln!("axe node: imported config rules once: {v}");
                }
                eprintln!("axe node: agents picked by stored rules (axe rule list)");
                if self.cfg.sets_legacy_selection() {
                    eprintln!(
                        "axe node: [defaults] and [[rule]] agents/strategy are deprecated \
                         and no longer pick agents; edit selection with axe rule"
                    );
                }
            }
            Err(e) => eprintln!(
                "axe node: stored rules unavailable ({e:#}); agents picked by the \
                 deprecated [defaults]/[[rule]] config"
            ),
        }
    }

    /// Live task and bot runs of runtime `name`.
    fn in_use(&self, name: &str) -> usize {
        self.runs.iter().filter(|r| r.pick.agent == name).count()
            + self.bots.iter().filter(|b| b.agent == name).count()
    }

    /// Whether runtime `name` can run a bot (its agent has `bot_args`) or, with
    /// `resume`, resume a session.
    fn can_run(&self, name: &str, bot: bool, resume: bool) -> bool {
        self.runtimes
            .get(name)
            .and_then(|r| self.cfg.agents.get(&r.agent))
            .is_some_and(|a| (!bot || a.bot_args.is_some()) && (!resume || a.resume_cmd.is_some()))
    }

    /// The least-used healthy runtime of `harness` (then by name); bots also
    /// need `bot_args`. `Err(true)`: the harness has runtimes, none healthy.
    fn least_used(&self, harness: &str, bot: bool, now: i64) -> std::result::Result<String, bool> {
        let fit: Vec<&runtime::Runtime> = self
            .runtimes
            .of(harness)
            .filter(|r| self.can_run(&r.name, bot, false))
            .collect();
        fit.iter()
            .filter(|r| r.healthy(now))
            .min_by_key(|r| self.in_use(&r.name))
            .map(|r| r.name.clone())
            .ok_or(!fit.is_empty())
    }

    /// `name: state` of every unhealthy runtime of `harnesses`, for notes.
    fn unhealthy_states(&self, harnesses: &[&str], now: i64) -> String {
        let s: Vec<String> = self
            .runtimes
            .runtimes
            .iter()
            .filter(|r| harnesses.contains(&r.harness.as_str()) && !r.healthy(now))
            .map(|r| match r.health.until {
                Some(u) => format!("{} {} until {}", r.name, r.state(now), crate::bot::ts(u)),
                None => format!("{} {}", r.name, r.state(now)),
            })
            .collect();
        s.join(", ")
    }

    /// Walks a rule's `use` list: the first entry with a healthy runtime of
    /// its harness wins, the least used such runtime first (then by name). An
    /// entry whose runtimes are all unhealthy is skipped for the next. No
    /// matching rule means `claude-code`. Bots also need `bot_args`.
    fn pick_from(&self, rule: Option<&rule::Rule>, bot: bool) -> std::result::Result<Pick, NoPick> {
        let fallback = [Use {
            harness: "claude-code".into(),
            model: None,
            effort: None,
        }];
        let uses = rule.map_or(&fallback[..], |r| &r.uses);
        let key = rule.map_or("no rule".to_string(), rule::Rule::key);
        let now = now_unix();
        let mut unhealthy = Vec::new();
        for u in uses {
            match self.least_used(&u.harness, bot, now) {
                Ok(name) => {
                    return Ok(Pick {
                        agent: name,
                        rule: key,
                        with: Some(u.clone()),
                        profile: None,
                    })
                }
                Err(true) => unhealthy.push(u.harness.as_str()),
                Err(false) => {}
            }
        }
        let tried: Vec<String> = uses.iter().map(Use::to_string).collect();
        if !unhealthy.is_empty() {
            return Err(NoPick::Unhealthy {
                until: self.runtimes.next_healthy(&unhealthy, now),
                note: format!(
                    "{key}: no healthy runtime for any use entry ({}): {}",
                    tried.join(", "),
                    self.unhealthy_states(&unhealthy, now)
                ),
            });
        }
        Err(NoPick::Missing(format!(
            "{key}: no configured agent for any use entry ({})",
            tried.join(", ")
        )))
    }

    /// A pinned agent (task or bot): its runtime while healthy, else the
    /// least-used healthy runtime of the same harness.
    fn pin_pick(&self, name: &str, why: &str, bot: bool) -> std::result::Result<Pick, NoPick> {
        let Some(rt) = self.runtimes.resolve(name) else {
            return Err(NoPick::Missing(format!("unknown agent {name}")));
        };
        let now = now_unix();
        let pick = |agent: String| Pick {
            agent,
            rule: why.to_string(),
            with: None,
            profile: None,
        };
        if rt.healthy(now) {
            return Ok(pick(rt.name.clone()));
        }
        let harness = rt.harness.as_str();
        match self.least_used(harness, bot, now) {
            Ok(other) => Ok(pick(other)),
            Err(_) => Err(NoPick::Unhealthy {
                until: self.runtimes.next_healthy(&[harness], now),
                note: format!(
                    "{why} {name}: no healthy {harness} runtime: {}",
                    self.unhealthy_states(&[harness], now)
                ),
            }),
        }
    }

    /// Returns a claimed task to pending until `until` (Unix seconds),
    /// refunding the run: no runtime it may use is healthy now.
    fn release(&mut self, task_id: i64, claim: &str, until: i64, note: String) -> Result<()> {
        let wait = (until - now_unix()).max(60);
        self.store.request::<bool>(
            "release",
            json!({"task":task_id,"claim":claim,"wait":wait,
                   "note":format!("held for a healthy runtime, retrying in {wait}s: {note}")}),
        )?;
        Ok(())
    }

    /// Lapses limits, probes logins and sends pending runtime cards.
    fn runtime_tick(&mut self) {
        if !self.runtimes.managed {
            return;
        }
        self.runtimes.tick(now_unix());
        let logins = std::mem::take(&mut *self.store.logins.lock().unwrap());
        for name in logins {
            self.runtimes.check_now(&name);
        }
        let checked = self.runtimes.probe();
        self.alerts.extend(checked);
        let mut i = 0;
        while i < self.alerts.len() {
            let a = self.alerts[i].clone();
            // The runtime recovered before the card went out: no card.
            if self
                .runtimes
                .get(&a.runtime)
                .map(|r| r.health.state.as_str())
                != Some(a.state.as_str())
            {
                self.alerts.remove(i);
                continue;
            }
            match self.store.client.send::<Value>(
                "POST",
                "/api/dispatcher/runtime-alert",
                a.to_json(),
            ) {
                Ok(v) => {
                    eprintln!(
                        "axe node: runtime {} turned {}: card {v}",
                        a.runtime, a.state
                    );
                    // A login check's card is no state episode of its own.
                    if !matches!(a.class.as_str(), runtime::LOGIN_OK | runtime::LOGIN_FAILED) {
                        self.runtimes.alerted(&a.runtime, &a.state);
                    }
                    self.alerts.remove(i);
                    self.alert_failed = false;
                }
                Err(e) => {
                    if !self.alert_failed {
                        eprintln!("axe node: runtime {} card not sent yet: {e:#}", a.runtime);
                    }
                    self.alert_failed = true;
                    i += 1;
                }
            }
        }
    }

    /// Records what a finished run of runtime `name` said about its login:
    /// the latest quota, and a failure class that marks the runtime.
    /// `started_at`: when the run read the runtime's login (Unix seconds).
    fn note_runtime(
        &mut self,
        name: &str,
        failure: Option<&crate::failure::Failure>,
        log: &std::path::Path,
        started_at: i64,
    ) {
        if let Some(q) = runtime::quota_from_log(log) {
            self.runtimes.set_quota(name, q);
        }
        let Some(f) = failure else {
            return;
        };
        let backoff = agent_proc::api_backoff_seconds(1);
        if let Some(alert) = self.runtimes.mark(name, f, backoff, now_unix(), started_at) {
            self.alerts.push(alert);
        }
        if self.runtimes.managed {
            let r = self.runtimes.get(name);
            eprintln!(
                "axe node: runtime {name}: {} -> {}",
                f.class,
                r.map_or("?", |r| r.health.state.as_str())
            );
        }
    }

    /// Replaces the resource gate (tests use controlled samples and clocks).
    pub fn with_gate(mut self, gate: Gate) -> Self {
        self.admission = gate.handle();
        self.gate = Some(gate);
        self
    }

    /// Reads the admission hold from `path` instead of `admission::hold_path()`.
    pub fn with_hold_path(mut self, path: PathBuf) -> Self {
        self.hold_path = path;
        self
    }

    /// The manual admission hold now; journals each change of state.
    fn hold(&mut self) -> admission::HoldState {
        let hold = admission::read_hold(&self.hold_path, time::OffsetDateTime::now_utc());
        let reason = hold.reason();
        if reason != self.hold_logged {
            match (&reason, &hold) {
                (Some(r), _) => eprintln!("axe admission: task admission held: {r}"),
                (None, admission::HoldState::Expired(h)) => {
                    eprintln!(
                        "axe admission: hold expired at {}; admitting tasks",
                        h.until
                    )
                }
                (None, _) => eprintln!("axe admission: hold released; admitting tasks"),
            }
            self.hold_logged = reason;
        }
        hold
    }

    /// Latest resource admission status, as reported to the server.
    pub fn admission(&self) -> admission::Handle {
        Arc::clone(&self.admission)
    }

    /// Live agent sessions, task and bot runs.
    pub fn live(&self) -> usize {
        self.runs.len() + self.pre_checks.len() + self.bots.len()
    }

    /// Live task runs; with deferred fallbacks they fill `[pool] total`.
    pub fn live_tasks(&self) -> usize {
        self.runs.len() + self.pre_checks.len()
    }

    /// Live bot runs; they fill `[pool] bots`, never `[pool] total`.
    pub fn live_bots(&self) -> usize {
        self.bots.len()
    }

    /// Task and bot slot use for `axe admission`: `bots.total` is the
    /// configured cap, or with none, the number of active bots.
    fn pools(&self) -> serde_json::Value {
        let bots_total = match self.cfg.pool_bots() {
            Some(n) => Some(n),
            None => self
                .store
                .client
                .get::<Vec<serde_json::Value>>("/api/bots", &[])
                .ok()
                .map(|b| b.iter().filter(|b| b["state"] == "active").count()),
        };
        json!({
            "tasks": {"used": self.runs.len() + self.pre_checks.len() + self.deferred.len(), "total": self.cfg.pool_total()},
            "bots": {"used": self.bots.len(), "total": bots_total,
                "configured": self.cfg.pool_bots()},
        })
    }

    /// Fresh-session fallbacks waiting for resource admission.
    pub fn deferred(&self) -> usize {
        self.deferred.len()
    }

    /// One poll: reap finished sessions, take a due resource sample, then admit
    /// ready work. Tests drive the node with this and a fake clock.
    pub fn poll(&mut self) -> Result<()> {
        self.reap()?;
        self.reap_bots()?;
        self.update_activity()?;
        if let Some(g) = &mut self.gate {
            g.tick();
        }
        self.runtime_tick();
        self.schedule()?;
        self.report_admission();
        Ok(())
    }

    /// Kills live sessions and escalates their tasks, as on SIGTERM.
    pub fn shutdown(&mut self) -> Result<()> {
        self.stop_all()
    }

    /// Publishes the admission status to the server after each new resource
    /// sample, and at least every `REPORT_EVERY`, so `axe admission` and
    /// `axe task show` describe this node. Best effort: a failed report only
    /// lets the server's copy go stale, which it then reports as unknown.
    fn report_admission(&mut self) {
        let samples = self.gate.as_ref().map_or(0, |g| g.samples());
        // Slot use changes are reported at once too.
        let used = (self.runs.len() + self.deferred.len(), self.bots.len());
        // So is a hold being set, released or expiring.
        let hold = self.hold();
        let runtimes = self.runtimes.heartbeat(|n| self.in_use(n), now_unix());
        let held = json!([hold.json(), runtimes]);
        if self.reported.as_ref().is_some_and(|(n, u, h, at)| {
            *n == samples && *u == used && *h == held && at.elapsed() < REPORT_EVERY
        }) {
            return;
        }
        let (id, generation) = {
            let j = self.store.journal.lock().unwrap();
            (j.id.clone(), j.generation.clone())
        };
        let mut view = admission::view(&self.admission);
        admission::apply_hold(&mut view, &hold);
        view["pools"] = self.pools();
        view["runtimes"] = runtimes;
        view["forms"] = self.form_key.clone();
        let body = json!({"id":id,"generation":generation,"admission":view});
        let failed = self
            .store
            .client
            .send::<serde_json::Value>("POST", "/api/dispatcher/admission", body)
            .err();
        // Log the first failure of a streak only, so an unreachable server
        // cannot flood the journal every sample.
        if let Some(e) = &failed {
            if self.reported.is_some() {
                eprintln!("axe node: admission report failed: {e:#}");
            }
            self.reported = None;
        } else {
            self.reported = Some((samples, used, held, Instant::now()));
        }
    }

    pub fn run(&mut self, once: bool, stop: Arc<AtomicBool>) -> Result<()> {
        self.init_rules();
        let bots = self
            .cfg
            .pool_bots()
            .map_or("one per active bot".to_string(), |n| n.to_string());
        eprintln!("axe node: pool total={} bots={bots}", self.cfg.pool_total());
        if self.runtimes.managed {
            let now = now_unix();
            let list: Vec<String> = self
                .runtimes
                .runtimes
                .iter()
                .map(|r| format!("{} ({}, {})", r.name, r.harness, r.state(now)))
                .collect();
            eprintln!("axe node: runtimes {}", list.join(", "));
        }
        match &self.gate {
            Some(g) => eprintln!("axe node: resource admission {}", g.summary()),
            None => eprintln!("axe node: resource admission off (no [resources])"),
        }
        let poll = Duration::from_secs(self.cfg.poll_seconds);
        let mut next_poll = Instant::now();
        let mut next_bot_poll = Instant::now();
        let mut scheduled = false;
        loop {
            if stop.load(Ordering::SeqCst) {
                self.stop_all()?;
                return Ok(());
            }
            self.reap()?;
            self.reap_bots()?;
            self.update_activity()?;
            if !once {
                self.services_tick();
            }
            let reopened = self.gate.as_mut().is_some_and(|g| g.tick());
            self.runtime_tick();
            self.report_admission();
            if self.store.draining.load(Ordering::SeqCst) {
                if self.drain_step()? {
                    // Deferred fallbacks hold claims without a process and bots
                    // past their grace are still live: release them as a stop
                    // does, so a restart needs no reconciliation.
                    self.stop_all()?;
                    return Ok(());
                }
                // Task runs may outlast the drain window, and bots warned of
                // their services stopping prepare for it: bots keep answering.
                let warning = self
                    .services_warned_until
                    .is_some_and(|t| Instant::now() < t);
                if (!self.runs.is_empty() || warning) && Instant::now() >= next_bot_poll {
                    self.schedule_bots()?;
                    next_bot_poll = Instant::now() + TICK;
                }
                sleep_interruptible(Duration::from_millis(200), &stop);
                continue;
            }
            if once {
                // A single pass waits for a complete CPU window instead of
                // reporting startup as pressure.
                let warming = self.gate.as_ref().is_some_and(|g| g.warming_up());
                if !scheduled && warming {
                    let due = self.gate.as_ref().map_or(TICK, |g| g.until_due());
                    sleep_interruptible(due.min(TICK), &stop);
                    continue;
                }
                // Deferred fallbacks hold claims: retry them when admission reopens.
                if !scheduled || (reopened && !self.deferred.is_empty()) {
                    self.schedule()?;
                    scheduled = true;
                }
                if self.live() == 0 && self.deferred.is_empty() {
                    return Ok(());
                }
                sleep_interruptible(Duration::from_millis(200), &stop);
                continue;
            }
            if reopened || Instant::now() >= next_poll {
                self.schedule()?;
                next_poll = Instant::now() + poll;
                next_bot_poll = Instant::now() + TICK;
            }
            // Bot wakes (messages) are latency sensitive: check them every tick.
            if Instant::now() >= next_bot_poll {
                self.schedule_bots()?;
                next_bot_poll = Instant::now() + TICK;
            }
            let until_poll = next_poll.saturating_duration_since(Instant::now());
            let mut wait = TICK
                .min(until_poll)
                .min(next_bot_poll.saturating_duration_since(Instant::now()));
            if let Some(g) = &self.gate {
                wait = wait.min(g.until_due());
            }
            sleep_interruptible(wait, &stop);
        }
    }

    /// One drain tick: past the deadline, abort unprotected task runs. True
    /// once every task run has ended and bots have finished or used their grace.
    fn drain_step(&mut self) -> Result<bool> {
        let now = Instant::now();
        let grace = Duration::from_secs(self.cfg.drain_abort_seconds);
        self.drain.get_or_insert_with(|| {
            eprintln!(
                "axe node: draining; aborting unprotected task runs after {}s",
                grace.as_secs()
            );
            Drain {
                abort_at: now + grace,
                next_check: now,
                bots_until: None,
            }
        });
        // Services keep running until their bots had the warning window.
        let warned = self.warn_services_stop(now);
        let drain = self.drain.as_mut().expect("drain set above");
        if self.runs.is_empty() && self.pre_checks.is_empty() {
            let until = *drain
                .bots_until
                .get_or_insert(now.max(warned) + DRAIN_BOT_GRACE);
            return Ok(now >= warned && (self.bots.is_empty() || now >= until));
        }
        if now >= drain.abort_at && now >= drain.next_check {
            drain.next_check = now + DRAIN_CHECK;
            self.abort_unprotected()?;
        }
        Ok(false)
    }

    /// Aborts every task run the server does not hold as protected; the server
    /// returns each task to pending, in its session, with its budget intact.
    fn abort_unprotected(&mut self) -> Result<()> {
        self.stop_pre_checks("aborted by rollout drain")?;
        let mut i = 0;
        while i < self.runs.len() {
            let run_id = self.runs[i].run_id;
            if !self
                .store
                .request::<bool>("drain_abort", json!({"run":run_id}))?
            {
                i += 1;
                continue;
            }
            self.runs[i].drain_aborting = true;
            if let Err(e) = kill_tree(&mut self.runs[i].child) {
                eprintln!("axe node: run {run_id} drain abort unconfirmed: {e:#}");
                i += 1;
                continue;
            }
            let run = self.runs.swap_remove(i);
            self.finish(run, None, DRAIN_ABORT_NOTE.into())?;
        }
        Ok(())
    }

    /// Admits ready bots, then ready tasks one at a time, oldest first, until
    /// the task pool is full or no ready work is left.
    fn schedule(&mut self) -> Result<()> {
        // Ready bots go first, in their own pool; the gate's normal pause does
        // not hold them (only its hard floor does).
        self.schedule_bots()?;
        // Deferred fallbacks already hold their claims and task slots: they go
        // before new claims, each behind its own fresh admission check.
        while !self.deferred.is_empty() {
            if let Some(g) = &mut self.gate {
                if !g.check() {
                    return Ok(());
                }
            }
            let d = self.deferred.remove(0);
            let task = self
                .store
                .request::<Task>("task", json!({"task":d.task_id}))?;
            // Someone resolved the task meanwhile: the claim is gone.
            if task.state != "in_progress" || task.claimed_by.as_deref() != Some(&d.claim) {
                continue;
            }
            if let Err(e) = self.spawn(&task, &d.pick, &d.claim, None, false) {
                self.block_claimed(task.id, &d.claim, format!("{e:#}"))?;
            }
        }
        while self.live_tasks() + self.deferred.len() < self.cfg.pool_total() {
            // Projects come ordered by their oldest ready task.
            let Some(project) = self
                .store
                .request::<Vec<String>>("pending", json!({}))?
                .into_iter()
                .next()
            else {
                return Ok(());
            };
            // A manual hold claims nothing; it is read before every claim so an
            // expiry re-admits without a restart.
            if self.hold().reason().is_some() {
                return Ok(());
            }
            // No healthy runtime: claim nothing (no run budget spent).
            if self.runtimes.managed {
                let now = now_unix();
                let held = (!self.runtimes.any_healthy(now)).then(|| {
                    let all: Vec<&str> = self
                        .runtimes
                        .runtimes
                        .iter()
                        .map(|r| r.harness.as_str())
                        .collect();
                    self.unhealthy_states(&all, now)
                });
                if held != self.runtime_hold_logged {
                    match &held {
                        Some(why) => {
                            eprintln!("axe node: claims held: no healthy runtime ({why})")
                        }
                        None => eprintln!("axe node: a runtime is healthy again; claiming"),
                    }
                    self.runtime_hold_logged = held.clone();
                }
                if held.is_some() {
                    return Ok(());
                }
            }
            // Fresh resource sample before every claim, so each slot gets its own
            // decision and a paused gate claims nothing (no run budget spent).
            if let Some(g) = &mut self.gate {
                if !g.check() {
                    return Ok(());
                }
            }
            if !self.admit_one(&project)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Claims and spawns the oldest ready task in `project`; false when none was
    /// left to claim.
    fn admit_one(&mut self, project: &str) -> Result<bool> {
        let res = self.cfg.resolve(project);
        let claim_id = format!("dispatch:{}", uuid::Uuid::new_v4());
        let task = match self.store.request::<Claim>(
            "claim",
            json!({"project":project,"claim":claim_id,"max_runs":res.max_runs}),
        )? {
            Claim::Claimed(t) => *t,
            Claim::Blocked(_) => return Ok(true),
            Claim::None => return Ok(false),
        };
        let mut resume = self
            .store
            .request::<Option<db::Session>>("session", json!({"task":task.id}))?;
        // A rollout's abort costs the session none of its resume budget.
        let events = self
            .store
            .request::<Vec<db::Event>>("events", json!({"task":task.id}))?;
        let interrupted = events
            .iter()
            .rev()
            .find(|e| matches!(e.kind.as_str(), "run_start" | "run_interrupted"))
            .is_some_and(|e| e.kind == "run_interrupted");
        let first_run = !events.iter().any(|e| e.kind == "run_start");
        let profile = self.profile(task.id);
        let now = now_unix();
        // The runtime the stored session resumes on, or why it starts fresh.
        let mut fallback = None;
        let resume_on = resume.as_ref().and_then(|session| {
            let rt = self.runtimes.resolve(&session.agent);
            fallback = match rt {
                None => Some("stored agent no longer exists".to_string()),
                Some(_)
                    if res
                        .max_resumes
                        .is_some_and(|max| !interrupted && session.resume_count >= max) =>
                {
                    Some("max_resumes reached".into())
                }
                Some(rt) if !self.can_run(&rt.name, false, true) => {
                    Some("stored agent has no resume_cmd".into())
                }
                Some(rt) if !rt.healthy(now) => {
                    Some(format!("stored runtime {} is {}", rt.name, rt.state(now)))
                }
                Some(_) => None,
            };
            rt.filter(|_| fallback.is_none()).map(|r| r.name.clone())
        });
        let matched = match self.stored_rules {
            true => self
                .store
                .request::<Option<rule::Rule>>("rule_select", json!({"task":task.id}))?,
            false => None,
        };
        let picked = if let Some(name) = resume_on {
            // The stored session resumes on its own runtime, with the model
            // and effort the rule gives that runtime's harness.
            let harness = self.runtimes.get(&name).map(|r| r.harness.clone());
            Ok(match self.stored_rules {
                true => Pick {
                    agent: name,
                    rule: matched.as_ref().map_or("no rule".into(), rule::Rule::key),
                    with: matched.as_ref().and_then(|r| {
                        r.uses
                            .iter()
                            .find(|u| Some(&u.harness) == harness.as_ref())
                            .cloned()
                    }),
                    profile: None,
                },
                false => Pick {
                    agent: name,
                    rule: res.rule_key.clone(),
                    with: None,
                    profile: None,
                },
            })
        } else if let Some(agent) = &task.agent {
            let why = if self.stored_rules {
                "task agent"
            } else {
                &res.rule_key
            };
            self.pin_pick(agent, why, false)
        } else if !self.stored_rules {
            let agent = match res.strategy {
                Strategy::Fixed => res.agents[0].clone(),
                Strategy::RoundRobin => {
                    let i = self.store.request::<usize>(
                        "round_robin",
                        json!({"rule":res.rule_key,"len":res.agents.len()}),
                    )?;
                    res.agents[i].clone()
                }
            };
            self.pin_pick(&agent, &res.rule_key, false)
        } else {
            self.pick_from(matched.as_ref(), false)
        };
        let pick = match picked {
            Ok(p) => Pick { profile, ..p },
            Err(NoPick::Unhealthy { until, note }) => {
                self.release(task.id, &claim_id, until, note)?;
                return Ok(true);
            }
            Err(e) => {
                self.block_claimed(task.id, &claim_id, e.note())?;
                return Ok(true);
            }
        };
        if let Some(reason) = fallback {
            self.store.request::<serde_json::Value>("note",json!({"task":task.id,"claim":claim_id,"note":format!("fresh session fallback: {reason}")}))?;
            resume = None;
        }
        // A routine's pre-check runs before the task's first agent run only.
        if let Some(cmd) = pick.profile.as_ref().and_then(|p| p.pre_check.clone()) {
            if first_run && resume.is_none() {
                if let Err(e) = self.start_pre_check(&task, &pick, &claim_id, &cmd) {
                    self.block_claimed(task.id, &claim_id, format!("pre-check: {e:#}"))?;
                }
                return Ok(true);
            }
        }
        if let Err(e) = self.spawn(&task, &pick, &claim_id, resume.as_ref(), interrupted) {
            self.block_claimed(task.id, &claim_id, format!("{e:#}"))?;
        }
        Ok(true)
    }

    /// The folder a run of `task` works in, `$AXE_WORKTREE` and its cwd: its
    /// run workspace (with the pick's routine skill profile), with a worktree
    /// of each of its repos, none when it has none. Also the repos cloned
    /// first. A task created before run workspaces, with no repos and a
    /// directory left at the old [`legacy_task_dir`], resumes there unchanged
    /// so its session and work are not stranded; nothing creates one any more.
    fn run_dir(&self, task: &Task, pick: &Pick) -> Result<(PathBuf, Vec<String>)> {
        if task.repos.is_none() {
            if let Some(dir) = legacy_task_dir(&self.cfg, task) {
                return Ok((dir, Vec::new()));
            }
        }
        let skills = pick.profile.as_ref().and_then(|p| p.skills.as_deref());
        crate::run_workspace::build(&self.cfg, task, Some(&self.store.client), skills)
    }

    /// The profile of the routine that created `task`; none for other
    /// tasks, or when the server is older than routine profiles.
    fn profile(&mut self, task: i64) -> Option<crate::routine::Profile> {
        match self
            .store
            .request::<Option<crate::routine::Profile>>("routine_profile", json!({"task":task}))
        {
            Ok(p) => p,
            Err(e) => {
                if !self.profiles_unsupported {
                    eprintln!(
                        "axe node: routine profiles unavailable ({e:#}); tasks run without them"
                    );
                    self.profiles_unsupported = true;
                }
                None
            }
        }
    }

    fn block_claimed(&mut self, task_id: i64, claim: &str, note: String) -> Result<()> {
        self.store.request::<bool>(
            "block",
            json!({"task":task_id,"claim":claim,"note":block_changes(task_id,&note).note}),
        )?;
        Ok(())
    }

    fn spawn(
        &mut self,
        task: &Task,
        pick: &Pick,
        claim: &str,
        resume: Option<&db::Session>,
        interrupted: bool,
    ) -> Result<()> {
        let agent_name = pick.agent.as_str();
        let rt = self
            .runtimes
            .get(agent_name)
            .with_context(|| format!("unknown agent {agent_name}"))?
            .clone();
        let agent = self
            .cfg
            .agents
            .get(&rt.agent)
            .with_context(|| format!("unknown agent {}", rt.agent))?
            .clone();
        let session_mode = agent.session_mode()?;
        let (cwd, repos_cloned) = self.run_dir(task, pick)?;
        let log_dir = self.cfg.log_dir();
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("create log dir {}", log_dir.display()))?;
        let suffix = if resume.is_none()
            && log_dir
                .join(format!("{}-{}.log", task.id, task.runs))
                .exists()
        {
            ".fresh"
        } else {
            ""
        };
        let mut log_path = log_dir.join(format!("{}-{}{suffix}.log", task.id, task.runs));
        // Reset budgets and resumed attempts must never overwrite recorded history.
        if log_path.exists() {
            log_path = log_dir.join(format!(
                "{}-{}-{}.log",
                task.id,
                task.runs,
                uuid::Uuid::new_v4()
            ));
        }

        let session_id = resume.map(|s| s.session_id.clone()).or_else(|| {
            (session_mode == SessionMode::Preset).then(|| uuid::Uuid::new_v4().to_string())
        });
        let prompts = &self.cfg.prompts;
        let mut prompt = match resume {
            Some(_) => Prompts::render(&prompts.resume, task.id, &task.project),
            None => Prompts::render(&prompts.fresh, task.id, &task.project),
        };
        if interrupted {
            prompt = format!(
                "{prompt} {}",
                Prompts::render(&prompts.interrupted, task.id, &task.project)
            );
        }
        // Follows the log name: a refunded run number must not overwrite a prompt.
        let prompt_path = log_path.with_extension("prompt.md");
        std::fs::write(&prompt_path, &prompt)
            .with_context(|| format!("write prompt {}", prompt_path.display()))?;
        // The run folder's instructions as this run saw them, for the run
        // page: the folder is regenerated on every run. Best effort.
        let _ = std::fs::copy(cwd.join("AGENTS.md"), log_path.with_extension("system.md"));
        let vars: BTreeMap<&str, String> = BTreeMap::from([
            ("session_id", session_id.clone().unwrap_or_default()),
            ("prompt", prompt),
            ("cwd", cwd.display().to_string()),
            ("task_id", task.id.to_string()),
            ("project", task.project.clone()),
        ]);
        let template = if resume.is_some() {
            agent.resume_cmd.as_ref().expect("resume template checked")
        } else {
            &agent.cmd
        };
        let with = with_profile(
            pick.with.as_ref().filter(|u| u.harness == rt.harness),
            pick.profile.as_ref(),
            &rt.harness,
        );
        let with = with.as_ref();
        let template = match with {
            Some(u) => splice_bot_args(template, &u.args()),
            None => template.clone(),
        };
        let argv: Vec<String> = template.iter().map(|a| render(a, &vars)).collect();

        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
            .with_context(|| format!("create log {}", log_path.display()))?;
        let mode = if resume.is_some() { "resume" } else { "fresh" };
        let attrs = self.cfg.otel.attributes(&[
            ("task_id", &task.id.to_string()),
            ("project", &task.project),
            ("agent", agent_name),
            ("mode", mode),
            ("run", claim),
        ]);
        let attrs: Vec<(&str, &str)> = attrs.iter().map(|(k, v)| (&**k, &**v)).collect();
        let otel = otel_resource_attributes(
            std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok().as_deref(),
            &attrs,
        );
        let session_env = session_id.clone().unwrap_or_else(|| claim.to_string());
        let mut env = vec![
            ("AXE_ACTOR", "agent".to_string()),
            ("AXE_SESSION", session_env),
            ("AXE_TASK", task.id.to_string()),
            ("OTEL_RESOURCE_ATTRIBUTES", otel),
        ];
        if let Some(url) = &self.server_url {
            env.push(("AXE_SERVER", url.clone()));
        }
        env.push(("AXE_WORKTREE", cwd.display().to_string()));
        // The runtime's login: its directory variable, or none for the default.
        let started_at = now_unix();
        let (login, unset) = rt.env();
        env.extend(login);
        // Sealed form values for this task: the folder, and the env ones.
        let secrets = crate::sealed::secrets_dir(
            &self.cfg.run_root(),
            &crate::sealed::Target::Task { task: task.id },
        )
        .expect("a task has a secrets folder");
        env.push(("AXE_SECRETS_DIR", secrets.display().to_string()));
        let secret_env = crate::sealed::launch_env(&secrets);
        env.extend(secret_env.iter().map(|(k, v)| (k.as_str(), v.clone())));
        let mut remove = vec!["AXE_WORKTREE"];
        remove.extend(unset);
        let mut command = agent_proc::command(&argv, &cwd, &remove, &env, log)?;
        let logged_cmd = agent_proc::logged_cmd(&argv, &prompt_path);
        let resume_count = resume.map_or(0, |s| s.resume_count + u64::from(!interrupted));
        let session = session_id.as_ref().map(|id| db::Session {
            agent: agent_name.into(),
            session_id: id.clone(),
            resume_count,
        });
        let mut body = json!({"mode":mode,"agent":agent_name,"rule":pick.rule,"session_id":session_id,"log":log_path,"prompt_file":prompt_path,"cmd":logged_cmd,"worktree":cwd,"repos_cloned":repos_cloned});
        if let Some(u) = with {
            body["use"] = json!(u.to_string());
        }
        if let Some(p) = &pick.profile {
            body["routine"] = json!(p);
        }
        if self.runtimes.managed {
            body["runtime"] = json!(rt.name);
            body["harness"] = json!(rt.harness);
        }
        // With stored rules or runtimes, the task carries the runtime it ran
        // on as a `runtime:<name>` label.
        let label = format!("runtime:{agent_name}");
        let label = ((self.stored_rules || self.runtimes.managed)
            && db::validate_name("label", &label).is_ok())
        .then_some(label);
        let run_id=self.store.request::<i64>("prepare",json!({"task":task.id,"claim":claim,"agent":agent_name,"mode":mode,"session_id":session_id,"log":log_path,"session":session,"event":body,"label":label}))?;
        self.store.log(run_id, log_path.clone(), false)?;
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.store.request::<bool>("finish",json!({"run":run_id,"code":null,"resumed":false,"note":format!("spawn {:?} failed: {e}",argv[0]),"event":{}}))?;
                self.store.log(run_id, log_path.clone(), true)?;
                return Ok(());
            }
        };

        if let Err(e) = self
            .store
            .request::<serde_json::Value>("started", json!({"run":run_id}))
        {
            kill_child(&mut child);
            return Err(e);
        }
        self.runs.push(Run {
            resume_count,
            resumed: resume.is_some(),
            task_id: task.id,
            claim: claim.to_string(),
            session_mode,
            session_id,
            log_path,
            pick: pick.clone(),
            child,
            run_id,
            activity: None,
            log_len: 0,
            drain_aborting: false,
            started_at,
        });
        Ok(())
    }

    fn reap(&mut self) -> Result<()> {
        self.reap_pre_checks()?;
        let mut i = 0;
        let aborts = std::mem::take(&mut *self.store.aborts.lock().unwrap());
        while i < self.runs.len() {
            if let Some(status) = self.runs[i].child.try_wait()? {
                self.reap_one(i, status)?;
                continue;
            }
            if aborts.contains(&self.runs[i].run_id) {
                if let Err(e) = kill_tree(&mut self.runs[i].child) {
                    eprintln!(
                        "axe node: run {} abort unconfirmed: {e:#}",
                        self.runs[i].run_id
                    );
                    i += 1;
                    continue;
                }
                let run = self.runs.swap_remove(i);
                self.finish(run, None, "remote abort confirmed".into())?;
                continue;
            }
            match self.runs[i].child.try_wait()? {
                Some(status) => self.reap_one(i, status)?,
                None => i += 1,
            }
        }
        Ok(())
    }

    fn reap_one(&mut self, i: usize, status: std::process::ExitStatus) -> Result<()> {
        let mut run = self.runs.swap_remove(i);
        kill_child(&mut run.child);
        let code = status.code();
        let code_str = code.map_or("signal".to_string(), |c| c.to_string());
        let note = if run.drain_aborting {
            DRAIN_ABORT_NOTE.to_string()
        } else {
            format!("session exited without updating task (exit {code_str})")
        };
        self.finish(run, code, note)
    }

    /// Refreshes `current_activity` of live runs whose log grew since the last tick.
    fn update_activity(&mut self) -> Result<()> {
        for run in &mut self.runs {
            refresh_activity(&self.store, run)?;
        }
        self.update_bot_activity()
    }

    fn finish(&mut self, mut run: Run, code: Option<i32>, block_note: String) -> Result<()> {
        refresh_activity(&self.store, &mut run)?;
        if let SessionMode::Jsonl(key) = &run.session_mode {
            if let Ok(text) = std::fs::read_to_string(&run.log_path) {
                run.session_id = parse_jsonl_session(&text, key).or(run.session_id);
            }
        }
        let session = run.session_id.as_ref().map(|id| db::Session {
            agent: run.pick.agent.clone(),
            session_id: id.clone(),
            resume_count: run.resume_count,
        });
        let usage = crate::usage::parse_log(&run.log_path);
        // Only a plain non-zero exit is classified; stops, drains and aborts keep their path.
        let failure = (code.is_some_and(|c| c != 0) && block_note.starts_with("session exited"))
            .then(|| crate::failure::classify(&run.log_path))
            .flatten();
        self.note_runtime(
            &run.pick.agent.clone(),
            failure.as_ref(),
            &run.log_path,
            run.started_at,
        );
        let api_error = failure.map(|f| f.to_json());
        let retry=self.store.request::<bool>("finish",json!({"api_error":api_error,"usage":usage,"run":run.run_id,"aborted":block_note == "remote abort confirmed","drained":block_note == DRAIN_ABORT_NOTE,"resumed":run.resumed,"code":code,"note":block_note,"session":session,"session_id":run.session_id,"event":{"exit_code":code,"log":run.log_path,"agent":run.pick.agent,"session_id":run.session_id}}))?;
        self.store.log(run.run_id, run.log_path.clone(), true)?;
        if retry {
            let task = self
                .store
                .request::<Task>("task", json!({"task":run.task_id}))?;
            // The fresh fallback keeps the run's pick unless the task's pin changed.
            let pinned = |a: &String| self.runtimes.resolve(a).map(|r| r.name.clone());
            let pick = match &task.agent {
                Some(a) if pinned(a).as_ref() != Some(&run.pick.agent) => {
                    match self.pin_pick(a, "task agent", false) {
                        Ok(p) => Pick {
                            profile: run.pick.profile.clone(),
                            ..p
                        },
                        Err(e) => {
                            self.block_claimed(task.id, &run.claim, e.note())?;
                            return Ok(());
                        }
                    }
                }
                _ => run.pick.clone(),
            };
            // The fallback is a new process: it needs its own admission decision.
            if let Some(g) = &mut self.gate {
                if !g.check() {
                    let reason = admission::view(&g.handle())["reason"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    self.store.request::<serde_json::Value>("note",json!({"task":run.task_id,"claim":run.claim,"note":format!("fresh session fallback deferred: resume exited non-zero without task changes; resource admission paused ({reason}); claim and slot kept, starts when pressure clears")}))?;
                    self.deferred.push(Deferred {
                        task_id: run.task_id,
                        claim: run.claim.clone(),
                        pick,
                    });
                    return Ok(());
                }
            }
            self.store.request::<serde_json::Value>("note",json!({"task":run.task_id,"claim":run.claim,"note":"fresh session fallback: resume exited non-zero without task changes (retry in same pass)"}))?;
            let task = self
                .store
                .request::<Task>("task", json!({"task":run.task_id}))?;
            if let Err(e) = self.spawn(&task, &pick, &run.claim, None, false) {
                self.block_claimed(task.id, &run.claim, format!("{e:#}"))?;
            }
        }
        Ok(())
    }

    fn stop_all(&mut self) -> Result<()> {
        // Services first: SIGTERM, then SIGKILL to any still running after
        // `stop_grace_seconds`; a plain stop (not a drain) tells their bots.
        let stopping = self.services.alive_by_bot();
        if self.services_warned_until.is_none() && !stopping.is_empty() {
            for (bot, names) in &stopping {
                self.service_notices
                    .push(stop_notice(bot, names, 0, &self.generation_key()));
            }
        }
        self.services.terminate_all(Instant::now());
        self.stop_services_finish();
        self.stop_bots()?;
        self.stop_pre_checks("dispatcher stopped")?;
        let mut runs = std::mem::take(&mut self.runs);
        for run in &mut runs {
            kill_child(&mut run.child);
        }
        let mut error = None;
        for mut run in runs {
            let code = run.child.wait().ok().and_then(|s| s.code());
            if let Err(e) = self.finish(run, code, "dispatcher stopped".into()) {
                error = Some(e);
                break;
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        // Deferred fallbacks hold claims without a process; release them the same way.
        for d in std::mem::take(&mut self.deferred) {
            let task = self
                .store
                .request::<Task>("task", json!({"task":d.task_id}))?;
            if task.state == "in_progress" && task.claimed_by.as_deref() == Some(&d.claim) {
                self.block_claimed(d.task_id, &d.claim, "dispatcher stopped".into())?;
            }
        }
        Ok(())
    }
}

/// The wake that tells bot `bot` its services `names` stop in `secs` seconds.
fn stop_notice(bot: &str, names: &[String], secs: u64, key: &str) -> crate::service::Notice {
    let list = names.join(", ");
    let text = if secs > 0 {
        format!("Services stop in {secs} s: {list}. The node is draining for a restart or upgrade; they start again when it is back. Prepare now if they hold state.")
    } else {
        format!("Services stopped: {list}. The node is stopping; they start again when it is back.")
    };
    crate::service::Notice {
        bot: bot.to_string(),
        kind: "stopping".into(),
        services: names.to_vec(),
        text,
        key: format!("{bot}:stop:{key}"),
    }
}

/// The `use` entry a run starts with: the rule's (or none), its model and
/// effort replaced by the routine profile's where it sets them. A profile
/// applies only to a harness axe knows how to pass them to.
fn with_profile(
    with: Option<&Use>,
    profile: Option<&crate::routine::Profile>,
    harness: &str,
) -> Option<Use> {
    let Some(p) = profile.filter(|p| p.model.is_some() || p.effort.is_some()) else {
        return with.cloned();
    };
    if !rule::HARNESSES.contains(&harness) {
        return with.cloned();
    }
    Some(Use {
        harness: harness.to_string(),
        model: p
            .model
            .clone()
            .or_else(|| with.and_then(|u| u.model.clone())),
        effort: p
            .effort
            .clone()
            .or_else(|| with.and_then(|u| u.effort.clone())),
    })
}

fn refresh_activity(store: &Store, run: &mut Run) -> Result<()> {
    let len = std::fs::metadata(&run.log_path).map_or(0, |m| m.len());
    if len == run.log_len {
        return Ok(());
    }
    run.log_len = len;
    crate::run_log::note_growth(&run.log_path, len);
    if let Some(a) = activity::from_log(&run.log_path) {
        if run.activity.as_ref() != Some(&a) {
            store
                .request::<serde_json::Value>("activity", json!({"run":run.run_id,"activity":a}))?;
            run.activity = Some(a);
        }
    }
    Ok(())
}

impl Dispatcher {
    /// One key per dispatcher generation and second, for notice idempotency.
    fn generation_key(&self) -> String {
        let generation = self.store.journal.lock().unwrap().generation.clone();
        format!("{generation}:{}", now_unix())
    }

    /// Syncs services with the server (at most every `TICK`), supervises
    /// them and sends their notices. The server being unreachable changes
    /// nothing that runs: the last declarations stay in force.
    fn services_tick(&mut self) {
        if !self.cfg.services.enabled {
            return;
        }
        let now = Instant::now();
        if self
            .service_sync
            .is_none_or(|t| now.duration_since(t) >= TICK)
        {
            self.service_sync = Some(now);
            let (id, generation) = {
                let j = self.store.journal.lock().unwrap();
                (j.id.clone(), j.generation.clone())
            };
            let body = json!({"id":id,"generation":generation,"statuses":self.services.statuses()});
            match self
                .store
                .client
                .send_timeout::<Vec<crate::service::Service>>(
                    "POST",
                    "/api/dispatcher/services",
                    body,
                    Some(Duration::from_secs(10)),
                ) {
                Ok(decls) => {
                    if self.service_sync_failed {
                        eprintln!("axe node: services synced again");
                    }
                    self.service_sync_failed = false;
                    // A draining node starts no new service and
                    // restarts none for a changed definition.
                    if !self.store.draining.load(Ordering::SeqCst) {
                        self.services.apply(&decls, now);
                    }
                }
                Err(e) => {
                    if !self.service_sync_failed {
                        eprintln!("axe node: services sync failed (services keep running): {e:#}");
                    }
                    self.service_sync_failed = true;
                }
            }
        }
        self.services
            .hold_runs(self.store.draining.load(Ordering::SeqCst));
        let notices = self.services.tick(now);
        self.service_notices.extend(notices);
        self.send_service_notices();
    }

    /// Sends queued notices, oldest first; a failure keeps the rest queued.
    fn send_service_notices(&mut self) {
        while let Some(n) = self.service_notices.first() {
            match self.store.client.send_timeout::<Value>(
                "POST",
                "/api/dispatcher/service-notice",
                serde_json::to_value(n).expect("notice serializes"),
                Some(Duration::from_secs(10)),
            ) {
                Ok(v) => {
                    eprintln!(
                        "axe node: service notice {} to bot {} ({}): {v}",
                        n.kind,
                        n.bot,
                        n.services.join(", ")
                    );
                    self.service_notices.remove(0);
                }
                Err(e) => {
                    // An unknown bot (removed meanwhile) never accepts it.
                    if !crate::client::retryable(&e) {
                        eprintln!("axe node: service notice dropped: {e:#}");
                        self.service_notices.remove(0);
                        continue;
                    }
                    eprintln!("axe node: service notice not sent yet: {e:#}");
                    return;
                }
            }
        }
    }

    /// On the first drain tick with live services: wakes each owning bot
    /// ("services stop in N s"). Returns when the services may stop.
    fn warn_services_stop(&mut self, now: Instant) -> Instant {
        if let Some(t) = self.services_warned_until {
            return t;
        }
        let alive = self.services.alive_by_bot();
        let secs = self.cfg.services.stop_warn_seconds;
        if alive.is_empty() || secs == 0 {
            self.services_warned_until = Some(now);
            return now;
        }
        let key = self.generation_key();
        for (bot, names) in &alive {
            self.service_notices
                .push(stop_notice(bot, names, secs, &key));
        }
        eprintln!(
            "axe node: draining; services stop in {secs}s ({} bots warned)",
            alive.len()
        );
        self.send_service_notices();
        let until = now + Duration::from_secs(secs);
        self.services_warned_until = Some(until);
        until
    }

    /// Waits for services to stop (SIGKILL after the grace period), then
    /// reports their final state and sends pending notices, best effort.
    fn stop_services_finish(&mut self) {
        if self.services.alive() > 0 {
            eprintln!(
                "axe node: stopping {} services (SIGTERM, then SIGKILL after {}s)",
                self.services.alive(),
                self.cfg.services.stop_grace_seconds
            );
        }
        self.services.stop_all();
        if !self.cfg.services.enabled {
            return;
        }
        let (id, generation) = {
            let j = self.store.journal.lock().unwrap();
            (j.id.clone(), j.generation.clone())
        };
        let statuses = self.services.statuses();
        if !statuses.is_empty() {
            let body = json!({"id":id,"generation":generation,"statuses":statuses});
            let _ = self.store.client.send_timeout::<Value>(
                "POST",
                "/api/dispatcher/services",
                body,
                Some(Duration::from_secs(5)),
            );
        }
        self.send_service_notices();
    }
}

fn sleep_interruptible(d: Duration, stop: &AtomicBool) {
    let end = Instant::now() + d;
    while !stop.load(Ordering::SeqCst) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.services.kill_all();
        for run in &mut self.runs {
            kill_child(&mut run.child);
        }
        for run in &mut self.bots {
            kill_child(&mut run.child);
        }
    }
}

/// Where a task without repos ran before run workspaces:
/// `<workspace_root>/.worktrees/<project>/task-<id>--work`, when a directory is
/// still there.
fn legacy_task_dir(cfg: &Config, task: &Task) -> Option<PathBuf> {
    let dir = cfg
        .workspace_root()
        .join(".worktrees")
        .join(&task.project)
        .join(format!("task-{}--work", task.id));
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::Profile;

    fn u(s: &str) -> Use {
        s.parse().unwrap()
    }

    #[test]
    fn profile_model_and_effort_replace_the_rules_per_field() {
        let p = |m: Option<&str>, e: Option<&str>| Profile {
            routine: 1,
            model: m.map(String::from),
            effort: e.map(String::from),
            ..Profile::default()
        };
        let rule = u("claude-code:opus:high");
        // No profile, or one without model or effort: the rule's entry.
        assert_eq!(
            with_profile(Some(&rule), None, "claude-code"),
            Some(rule.clone())
        );
        assert_eq!(
            with_profile(Some(&rule), Some(&p(None, None)), "claude-code"),
            Some(rule.clone())
        );
        assert_eq!(
            with_profile(Some(&rule), Some(&p(None, Some("low"))), "claude-code"),
            Some(u("claude-code:opus:low"))
        );
        assert_eq!(
            with_profile(Some(&rule), Some(&p(Some("sonnet"), None)), "claude-code"),
            Some(u("claude-code:sonnet:high"))
        );
        // A pinned agent has no rule entry: the profile alone.
        assert_eq!(
            with_profile(None, Some(&p(Some("gpt-5"), Some("low"))), "codex"),
            Some(u("codex:gpt-5:low"))
        );
        // A harness axe cannot pass them to: unchanged.
        assert_eq!(with_profile(None, Some(&p(Some("m"), None)), ""), None);
    }
}
