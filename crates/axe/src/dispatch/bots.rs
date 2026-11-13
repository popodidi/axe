//! Bot runs in the dispatcher: claim ready bots, render their system prompt and
//! hook settings, spawn, relay activity, kill stopped or paused runs, and finish.
//! Bots have their own pool (`[pool] bots`), separate from task slots, and
//! the resource gate holds them only at its hard floor (`admission.rs`).
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Child;

use anyhow::{Context, Result};
use serde_json::json;

use super::Dispatcher;
use crate::activity;
use crate::agent_proc::{self, kill_child, kill_tree, render};
use crate::bot::{self, Claimed};
use crate::config::SessionMode;
use crate::worktree;

pub(super) struct BotRun {
    run_id: i64,
    name: String,
    session_mode: SessionMode,
    session_id: Option<String>,
    log_path: PathBuf,
    pub(super) child: Child,
    activity: Option<String>,
    log_len: u64,
}

/// Splices `bot_args` in before the first argument that holds `{prompt}`, or
/// appends them when the template has none.
pub fn splice_bot_args(template: &[String], bot_args: &[String]) -> Vec<String> {
    let at = template
        .iter()
        .position(|a| a.contains("{prompt}"))
        .unwrap_or(template.len());
    let mut out = template[..at].to_vec();
    out.extend_from_slice(bot_args);
    out.extend_from_slice(&template[at..]);
    out
}

impl Dispatcher {
    /// True while the bot pool has a free slot. Task runs never count here.
    fn bot_slot_free(&self) -> bool {
        self.cfg.pool_bots().is_none_or(|n| self.bots.len() < n)
    }

    /// Claims and spawns ready bots while the bot pool has room and the
    /// resource gate is above its hard floor. Task slots play no part.
    pub(super) fn schedule_bots(&mut self) -> Result<()> {
        if !self.bot_slot_free() {
            return Ok(());
        }
        let ready = match self.store.request::<Vec<String>>("bot_ready", json!({})) {
            Ok(r) => {
                self.bots_unsupported = false;
                r
            }
            Err(e) => {
                // An older server has no bot operations; tasks still run.
                if !self.bots_unsupported {
                    eprintln!("axe dispatcher: bot scheduling unavailable: {e:#}");
                    self.bots_unsupported = true;
                }
                return Ok(());
            }
        };
        for name in ready {
            if !self.bot_slot_free() {
                break;
            }
            if let Some(g) = &mut self.gate {
                if !g.check_bots() {
                    return Ok(());
                }
            }
            let claim = format!("bot:{}", uuid::Uuid::new_v4());
            let Some(c) = self
                .store
                .request::<Option<Claimed>>("bot_claim", json!({"bot":name,"claim":claim}))?
            else {
                continue;
            };
            if let Err(e) = self.spawn_bot(&c) {
                self.store.request::<serde_json::Value>(
                    "bot_finish",
                    json!({"run":c.run,"code":null,"reason":"error","note":format!("{e:#}")}),
                )?;
            }
        }
        Ok(())
    }

    fn spawn_bot(&mut self, c: &Claimed) -> Result<()> {
        let b = &c.bot;
        let agent = self
            .cfg
            .agents
            .get(&b.agent)
            .with_context(|| format!("unknown agent {}", b.agent))?
            .clone();
        let bot_args = agent
            .bot_args
            .clone()
            .with_context(|| format!("agent {} has no bot_args", b.agent))?;
        let session_mode = agent.session_mode()?;
        let resume = c
            .session
            .as_ref()
            .filter(|s| s.agent == b.agent && agent.resume_cmd.is_some());
        let worktree = match &self.cfg.worktrees {
            Some(_) => {
                let ready =
                    worktree::readiness(&self.cfg, &b.project).map_err(anyhow::Error::msg)?;
                Some(worktree::allocate_dir(&ready, &format!("bot-{}", b.name))?)
            }
            None => None,
        };
        let cwd = worktree
            .clone()
            .unwrap_or_else(|| self.cfg.workspace_root().join(&b.project));
        if !cwd.is_dir() {
            anyhow::bail!("project directory {} does not exist", cwd.display());
        }
        let dir = self.cfg.log_dir().join("bots").join(&b.name);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let system_file = dir.join("system.md");
        // Docs attached to the bot (owner `bot:<name>`), its project and global.
        let attached = crate::context::resolve_or_empty(
            &self.store.client,
            &crate::context::bot_scopes(&b.name, &b.project),
            &[],
        );
        let system = bot::system_prompt(b) + &crate::context::prompt_section(&attached);
        std::fs::write(&system_file, system)
            .with_context(|| format!("write {}", system_file.display()))?;
        let settings_file = dir.join("settings.json");
        // After an atomic binary replacement Linux reports "<path> (deleted)";
        // the hook must call the installed path.
        let exe = std::env::current_exe().context("locate the axe binary")?;
        let exe = exe.display().to_string();
        let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
        std::fs::write(&settings_file, bot::settings_json(exe))
            .with_context(|| format!("write {}", settings_file.display()))?;

        let session_id = resume.map(|s| s.session_id.clone()).or_else(|| {
            (session_mode == SessionMode::Preset).then(|| uuid::Uuid::new_v4().to_string())
        });
        let prompt = bot::render_prompt(c, resume.is_some());
        let prompt_path = dir.join(format!("{}.prompt.md", c.run));
        std::fs::write(&prompt_path, &prompt)
            .with_context(|| format!("write prompt {}", prompt_path.display()))?;
        let log_path = dir.join(format!("{}.log", c.run));
        let vars: BTreeMap<&str, String> = BTreeMap::from([
            ("session_id", session_id.clone().unwrap_or_default()),
            ("prompt", prompt),
            ("cwd", cwd.display().to_string()),
            ("task_id", String::new()),
            ("project", b.project.clone()),
            ("system_prompt_file", system_file.display().to_string()),
            ("settings_file", settings_file.display().to_string()),
        ]);
        let template = match resume {
            Some(_) => agent.resume_cmd.as_ref().expect("resume template checked"),
            None => &agent.cmd,
        };
        let argv: Vec<String> = splice_bot_args(template, &bot_args)
            .iter()
            .map(|a| render(a, &vars))
            .collect();
        let mode = if resume.is_some() { "resume" } else { "fresh" };
        let attrs = self.cfg.otel.attributes(&[
            ("bot", &b.name),
            ("project", &b.project),
            ("agent", &b.agent),
            ("mode", mode),
            ("bot_run", &c.run.to_string()),
        ]);
        let attrs: Vec<(&str, &str)> = attrs.iter().map(|(k, v)| (&**k, &**v)).collect();
        let otel = agent_proc::otel_resource_attributes(
            std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok().as_deref(),
            &attrs,
        );
        let mut env = vec![
            ("AXE_ACTOR", format!("bot:{}", b.name)),
            ("AXE_BOT", b.name.clone()),
            ("AXE_BOT_RUN", c.run.to_string()),
            (
                "AXE_SESSION",
                session_id
                    .clone()
                    .unwrap_or_else(|| format!("bot-run:{}", c.run)),
            ),
            ("OTEL_RESOURCE_ATTRIBUTES", otel),
        ];
        if let Some(url) = &self.server_url {
            env.push(("AXE_SERVER", url.clone()));
        }
        if let Some(path) = &worktree {
            env.push(("AXE_WORKTREE", path.display().to_string()));
        }
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
            .with_context(|| format!("create log {}", log_path.display()))?;
        let mut command =
            agent_proc::command(&argv, &cwd, &["AXE_TASK", "AXE_WORKTREE"], &env, log)?;
        let event = json!({"run":c.run,"mode":mode,"agent":b.agent,"session_id":session_id,
            "log":log_path,"prompt_file":prompt_path,"system_prompt_file":system_file,
            "settings_file":settings_file,"cmd":agent_proc::logged_cmd(&argv, &prompt_path),
            "worktree":worktree,"wakes":c.wakes.iter().map(|w| w.id).collect::<Vec<_>>()});
        self.store.request::<serde_json::Value>(
            "bot_started",
            json!({"run":c.run,"mode":mode,"session_id":session_id,"log":log_path,"event":event}),
        )?;
        self.store.log_bot(c.run, log_path.clone(), false)?;
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.store.request::<serde_json::Value>(
                    "bot_finish",
                    json!({"run":c.run,"code":null,"reason":"spawn_failed",
                        "note":format!("spawn {:?} failed: {e}", argv[0])}),
                )?;
                self.store.log_bot(c.run, log_path, true)?;
                return Ok(());
            }
        };
        self.bots.push(BotRun {
            run_id: c.run,
            name: b.name.clone(),
            session_mode,
            session_id,
            log_path,
            child,
            activity: None,
            log_len: 0,
        });
        Ok(())
    }

    /// Finishes exited bot runs, and kills runs whose bot was stopped or paused.
    pub(super) fn reap_bots(&mut self) -> Result<()> {
        if self.bots.is_empty() {
            return Ok(());
        }
        let kills: Vec<(i64, String)> = self
            .store
            .request("bot_control", json!({}))
            .unwrap_or_default();
        let mut i = 0;
        while i < self.bots.len() {
            if let Some(status) = self.bots[i].child.try_wait()? {
                let run = self.bots.swap_remove(i);
                self.finish_bot(run, status.code(), "exit", None)?;
                continue;
            }
            let Some((_, reason)) = kills.iter().find(|(r, _)| *r == self.bots[i].run_id) else {
                i += 1;
                continue;
            };
            let reason = reason.clone();
            let mut run = self.bots.swap_remove(i);
            if let Err(e) = kill_tree(&mut run.child) {
                eprintln!(
                    "axe dispatcher: bot {} run {} kill: {e:#}; killing its group",
                    run.name, run.run_id
                );
                kill_child(&mut run.child);
            }
            let note = format!("killed by dispatcher: bot {reason} requested");
            self.finish_bot(run, None, &reason, Some(note))?;
        }
        Ok(())
    }

    pub(super) fn update_bot_activity(&mut self) -> Result<()> {
        for run in &mut self.bots {
            let len = std::fs::metadata(&run.log_path).map_or(0, |m| m.len());
            if len == run.log_len {
                continue;
            }
            run.log_len = len;
            if let Some(a) = activity::from_log(&run.log_path) {
                if run.activity.as_ref() != Some(&a) {
                    self.store.request::<serde_json::Value>(
                        "bot_activity",
                        json!({"run":run.run_id,"activity":a}),
                    )?;
                    run.activity = Some(a);
                }
            }
        }
        Ok(())
    }

    fn finish_bot(
        &mut self,
        mut run: BotRun,
        code: Option<i32>,
        reason: &str,
        note: Option<String>,
    ) -> Result<()> {
        kill_child(&mut run.child);
        if let SessionMode::Jsonl(key) = &run.session_mode {
            if let Ok(text) = std::fs::read_to_string(&run.log_path) {
                run.session_id = agent_proc::parse_jsonl_session(&text, key).or(run.session_id);
            }
        }
        let usage = crate::usage::parse_log(&run.log_path);
        // A failed exit caused by a model-API outage is transient, not a failure.
        let outage = (reason == "exit" && code != Some(0))
            .then(|| agent_proc::api_outage(&run.log_path))
            .flatten();
        let (reason, note) = match &outage {
            Some(o) => (
                "api_error",
                Some(format!(
                    "model API error {}: {}",
                    o["status"],
                    o["message"].as_str().unwrap_or("")
                )),
            ),
            None => (reason, note),
        };
        self.store.request::<serde_json::Value>(
            "bot_finish",
            json!({"usage":usage,"run":run.run_id,"code":code,"reason":reason,"session_id":run.session_id,"note":note}),
        )?;
        self.store.log_bot(run.run_id, run.log_path, true)
    }

    /// Kills live bot runs on dispatcher shutdown; not counted as failures.
    pub(super) fn stop_bots(&mut self) -> Result<()> {
        let mut runs = std::mem::take(&mut self.bots);
        for run in &mut runs {
            kill_child(&mut run.child);
        }
        for run in runs {
            self.finish_bot(run, None, "dispatcher_stopped", None)?;
        }
        Ok(())
    }
}
