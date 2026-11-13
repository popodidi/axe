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
use crate::worktree;

mod bots;
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

struct Run {
    resume_count: u64,
    resumed: bool,
    task_id: i64,
    claim: String,
    agent: String,
    session_mode: SessionMode,
    session_id: Option<String>,
    log_path: PathBuf,
    rule: String,
    child: Child,
    /// Row in the `runs` table.
    run_id: i64,
    activity: Option<String>,
    log_len: u64,
    /// The server recorded this run's drain abort; only the kill may remain.
    drain_aborting: bool,
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
}

/// A resume-to-fresh fallback waiting for resource admission.
struct Deferred {
    task_id: i64,
    agent: String,
    claim: String,
    rule: String,
}

/// Escalates a dispatcher-claimed task; its session resumes once ops resolves the cause.
fn block_changes(task_id: i64, blocker: &str) -> Changes {
    Changes {
        state: Some("escalated".into()),
        note: Some(db::block_note(
            blocker,
            "ops: inspect the run log and fix the cause",
            &format!("axe set {task_id} --state pending"),
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
        ("pending", None) if t.sleeping() => Some(format!(
            "waiting until {}; becomes ready automatically",
            t.not_before.as_deref().unwrap_or("?")
        )),
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
    let state = if t.status == t.state {
        t.state.clone()
    } else {
        format!("{} (stored {})", t.status, t.state)
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
    let repos = match &t.repos {
        None => format!("- (project checkout {})", t.project),
        Some(r) if r.is_empty() => "none".into(),
        Some(r) => r.join(", "),
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
        let gate = cfg
            .resources
            .clone()
            .map(|r| Gate::new(r, cfg.resources_path()));
        let admission = match &gate {
            Some(g) => g.handle(),
            None => admission::handle(None),
        };
        Dispatcher {
            cfg,
            store,
            runs: Vec::new(),
            server_url: Some(url),
            gate,
            admission,
            deferred: Vec::new(),
            reported: None,
            hold_path: admission::hold_path(),
            hold_logged: None,
            bots: Vec::new(),
            bots_unsupported: false,
            drain: None,
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
        self.runs.len() + self.bots.len()
    }

    /// Live task runs; with deferred fallbacks they fill `[pool] total`.
    pub fn live_tasks(&self) -> usize {
        self.runs.len()
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
            "tasks": {"used": self.runs.len() + self.deferred.len(), "total": self.cfg.pool_total()},
            "bots": {"used": self.bots.len(), "total": bots_total,
                "configured": self.cfg.pool_bots()},
        })
    }

    /// Fresh-session fallbacks waiting for resource admission.
    pub fn deferred(&self) -> usize {
        self.deferred.len()
    }

    /// One poll: reap finished sessions, take a due resource sample, then admit
    /// ready work. Tests drive the dispatcher with this and a fake clock.
    pub fn poll(&mut self) -> Result<()> {
        self.reap()?;
        self.reap_bots()?;
        self.update_activity()?;
        if let Some(g) = &mut self.gate {
            g.tick();
        }
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
    /// `axe show` describe this dispatcher. Best effort: a failed report only
    /// lets the server's copy go stale, which it then reports as unknown.
    fn report_admission(&mut self) {
        let samples = self.gate.as_ref().map_or(0, |g| g.samples());
        // Slot use changes are reported at once too.
        let used = (self.runs.len() + self.deferred.len(), self.bots.len());
        // So is a hold being set, released or expiring.
        let hold = self.hold();
        let held = hold.json();
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
                eprintln!("axe dispatcher: admission report failed: {e:#}");
            }
            self.reported = None;
        } else {
            self.reported = Some((samples, used, held, Instant::now()));
        }
    }

    pub fn run(&mut self, once: bool, stop: Arc<AtomicBool>) -> Result<()> {
        let bots = self
            .cfg
            .pool_bots()
            .map_or("one per active bot".to_string(), |n| n.to_string());
        eprintln!(
            "axe dispatcher: pool total={} bots={bots}",
            self.cfg.pool_total()
        );
        match &self.gate {
            Some(g) => eprintln!("axe dispatcher: resource admission {}", g.summary()),
            None => eprintln!("axe dispatcher: resource admission off (no [resources])"),
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
            let reopened = self.gate.as_mut().is_some_and(|g| g.tick());
            self.report_admission();
            if self.store.draining.load(Ordering::SeqCst) {
                if self.drain_step()? {
                    // Deferred fallbacks hold claims without a process and bots
                    // past their grace are still live: release them as a stop
                    // does, so a restart needs no reconciliation.
                    self.stop_all()?;
                    return Ok(());
                }
                // Task runs may outlast the drain window: bots keep answering.
                if !self.runs.is_empty() && Instant::now() >= next_bot_poll {
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
        let drain = self.drain.get_or_insert_with(|| {
            eprintln!(
                "axe dispatcher: draining; aborting unprotected task runs after {}s",
                grace.as_secs()
            );
            Drain {
                abort_at: now + grace,
                next_check: now,
                bots_until: None,
            }
        });
        if self.runs.is_empty() {
            let until = *drain.bots_until.get_or_insert(now + DRAIN_BOT_GRACE);
            return Ok(self.bots.is_empty() || now >= until);
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
                eprintln!("axe dispatcher: run {run_id} drain abort unconfirmed: {e:#}");
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
            if let Err(e) = self.spawn(&task, &d.agent, &d.claim, None, &d.rule, false) {
                self.block_claimed(task.id, &d.claim, format!("{e:#}"))?;
            }
        }
        while self.runs.len() + self.deferred.len() < self.cfg.pool_total() {
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
        // A project without a worktree base escalates its next task before any
        // claim, so no run budget is spent discovering the gap. A task with its
        // own repos is not held (`Claim::None`) and is claimed below.
        if self.cfg.worktrees.is_some() {
            if let Err(gap) = worktree::readiness(&self.cfg, project) {
                let held = self
                    .store
                    .request::<Claim>("hold", json!({"project":project,"note":gap}))?;
                if !matches!(held, Claim::None) {
                    return Ok(true);
                }
            }
        }
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
        let interrupted = self
            .store
            .request::<Vec<db::Event>>("events", json!({"task":task.id}))?
            .iter()
            .rev()
            .find(|e| matches!(e.kind.as_str(), "run_start" | "run_interrupted"))
            .is_some_and(|e| e.kind == "run_interrupted");
        if let Some(session) = &resume {
            let reason = if !self.cfg.agents.contains_key(&session.agent) {
                Some("stored agent no longer exists")
            } else if res
                .max_resumes
                .is_some_and(|max| !interrupted && session.resume_count >= max)
            {
                Some("max_resumes reached")
            } else if self.cfg.agents[&session.agent].resume_cmd.is_none() {
                Some("stored agent has no resume_cmd")
            } else {
                None
            };
            if let Some(reason) = reason {
                self.store.request::<serde_json::Value>("note",json!({"task":task.id,"claim":claim_id,"note":format!("fresh session fallback: {reason}")}))?;
                resume = None;
            }
        }
        let agent = if let Some(session) = &resume {
            session.agent.clone()
        } else if let Some(agent) = &task.agent {
            agent.clone()
        } else {
            match res.strategy {
                Strategy::Fixed => res.agents[0].clone(),
                Strategy::RoundRobin => {
                    let i = self.store.request::<usize>(
                        "round_robin",
                        json!({"rule":res.rule_key,"len":res.agents.len()}),
                    )?;
                    res.agents[i].clone()
                }
            }
        };
        if let Err(e) = self.spawn(
            &task,
            &agent,
            &claim_id,
            resume.as_ref(),
            &res.rule_key,
            interrupted,
        ) {
            self.block_claimed(task.id, &claim_id, format!("{e:#}"))?;
        }
        Ok(true)
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
        agent_name: &str,
        claim: &str,
        resume: Option<&db::Session>,
        rule: &str,
        interrupted: bool,
    ) -> Result<()> {
        let agent = self
            .cfg
            .agents
            .get(agent_name)
            .with_context(|| format!("unknown agent {agent_name}"))?
            .clone();
        let session_mode = agent.session_mode()?;
        let task_worktree = match &self.cfg.worktrees {
            _ if task.repos.is_some() => Some(crate::run_workspace::build(
                &self.cfg,
                task,
                Some(&self.store.client),
            )?),
            Some(_) => {
                let ready =
                    worktree::readiness(&self.cfg, &task.project).map_err(anyhow::Error::msg)?;
                Some(worktree::allocate(&ready, task.id)?)
            }
            None => None,
        };
        let cwd = task_worktree
            .clone()
            .unwrap_or_else(|| self.cfg.workspace_root().join(&task.project));
        if !cwd.is_dir() {
            anyhow::bail!("project directory {} does not exist", cwd.display());
        }
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
        if let Some(path) = &task_worktree {
            env.push(("AXE_WORKTREE", path.display().to_string()));
        }
        let mut command = agent_proc::command(&argv, &cwd, &["AXE_WORKTREE"], &env, log)?;
        let logged_cmd = agent_proc::logged_cmd(&argv, &prompt_path);
        let resume_count = resume.map_or(0, |s| s.resume_count + u64::from(!interrupted));
        let session = session_id.as_ref().map(|id| db::Session {
            agent: agent_name.into(),
            session_id: id.clone(),
            resume_count,
        });
        let body = json!({"mode":mode,"agent":agent_name,"rule":rule,"session_id":session_id,"log":log_path,"prompt_file":prompt_path,"cmd":logged_cmd,"worktree":task_worktree});
        let run_id=self.store.request::<i64>("prepare",json!({"task":task.id,"claim":claim,"agent":agent_name,"mode":mode,"session_id":session_id,"log":log_path,"session":session,"event":body}))?;
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
            agent: agent_name.to_string(),
            session_mode,
            session_id,
            log_path,
            rule: rule.to_string(),
            child,
            run_id,
            activity: None,
            log_len: 0,
            drain_aborting: false,
        });
        Ok(())
    }

    fn reap(&mut self) -> Result<()> {
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
                        "axe dispatcher: run {} abort unconfirmed: {e:#}",
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
            agent: run.agent.clone(),
            session_id: id.clone(),
            resume_count: run.resume_count,
        });
        let usage = crate::usage::parse_log(&run.log_path);
        // Only a plain non-zero exit can be an outage; stops, drains and aborts keep their path.
        let api_error = (code.is_some_and(|c| c != 0) && block_note.starts_with("session exited"))
            .then(|| agent_proc::api_outage(&run.log_path))
            .flatten();
        let retry=self.store.request::<bool>("finish",json!({"api_error":api_error,"usage":usage,"run":run.run_id,"aborted":block_note == "remote abort confirmed","drained":block_note == DRAIN_ABORT_NOTE,"resumed":run.resumed,"code":code,"note":block_note,"session":session,"session_id":run.session_id,"event":{"exit_code":code,"log":run.log_path,"agent":run.agent,"session_id":run.session_id}}))?;
        self.store.log(run.run_id, run.log_path.clone(), true)?;
        if retry {
            let task = self
                .store
                .request::<Task>("task", json!({"task":run.task_id}))?;
            let agent = task.agent.as_deref().unwrap_or(&run.agent).to_owned();
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
                        agent: agent.clone(),
                        claim: run.claim.clone(),
                        rule: run.rule.clone(),
                    });
                    return Ok(());
                }
            }
            self.store.request::<serde_json::Value>("note",json!({"task":run.task_id,"claim":run.claim,"note":"fresh session fallback: resume exited non-zero without task changes (retry in same pass)"}))?;
            let task = self
                .store
                .request::<Task>("task", json!({"task":run.task_id}))?;
            if let Err(e) = self.spawn(&task, &agent, &run.claim, None, &run.rule, false) {
                self.block_claimed(task.id, &run.claim, format!("{e:#}"))?;
            }
        }
        Ok(())
    }

    fn stop_all(&mut self) -> Result<()> {
        self.stop_bots()?;
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

fn refresh_activity(store: &Store, run: &mut Run) -> Result<()> {
    let len = std::fs::metadata(&run.log_path).map_or(0, |m| m.len());
    if len == run.log_len {
        return Ok(());
    }
    run.log_len = len;
    if let Some(a) = activity::from_log(&run.log_path) {
        if run.activity.as_ref() != Some(&a) {
            store
                .request::<serde_json::Value>("activity", json!({"run":run.run_id,"activity":a}))?;
            run.activity = Some(a);
        }
    }
    Ok(())
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
        for run in &mut self.runs {
            kill_child(&mut run.child);
        }
        for run in &mut self.bots {
            kill_child(&mut run.child);
        }
    }
}
