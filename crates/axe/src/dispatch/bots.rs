//! Bot runs in the node: claim ready bots, render their system prompt and
//! hook settings, spawn, relay activity, kill stopped or paused runs, and finish.
//! Bots have their own pool (`[pool] bots`), separate from task slots, and
//! the resource gate holds them only at its hard floor (`admission.rs`).
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Child;

use anyhow::{Context, Result};
use serde_json::json;

use super::{now_unix, Dispatcher, NoPick, Pick};
use crate::activity;
use crate::agent_proc::{self, kill_child, kill_tree, render};
use crate::bot::{self, Claimed};
use crate::config::SessionMode;
use crate::rule;

pub(super) struct BotRun {
    run_id: i64,
    pub(super) name: String,
    pub(super) agent: String,
    session_mode: SessionMode,
    session_id: Option<String>,
    log_path: PathBuf,
    pub(super) child: Child,
    activity: Option<String>,
    log_len: u64,
    /// Unix seconds when the run read its runtime's login.
    started_at: i64,
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
                    eprintln!("axe node: bot scheduling unavailable: {e:#}");
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
            let pick = match self.bot_pick(&name)? {
                Ok(p) => {
                    if self.bots_held.remove(&name).is_some() {
                        eprintln!("axe node: bot {name}: runtime {} healthy; running", p.agent);
                    }
                    Some(p)
                }
                // Its wakes wait; nothing is claimed.
                Err(NoPick::Unhealthy { note, .. }) => {
                    if self.bots_held.get(&name) != Some(&note) {
                        eprintln!("axe node: bot {name} held: {note}");
                        self.bots_held.insert(name.clone(), note);
                    }
                    continue;
                }
                // Spawning reports what is missing.
                Err(NoPick::Missing(_)) => None,
            };
            let agent = pick.as_ref().map(|p| p.agent.clone());
            let Some(c) = self.store.request::<Option<Claimed>>(
                "bot_claim",
                json!({"bot":name,"claim":claim,"agent":agent}),
            )?
            else {
                continue;
            };
            if let Err(e) = self.spawn_bot(&c, pick.as_ref()) {
                self.store.request::<serde_json::Value>(
                    "bot_finish",
                    json!({"run":c.run,"code":null,"reason":"error","note":format!("{e:#}")}),
                )?;
            }
        }
        Ok(())
    }

    /// The runtime of bot `name`'s next run: its stored session's runtime
    /// while healthy, else a stored rule that names the bot, else the bot's
    /// own agent (or another healthy runtime of its harness).
    fn bot_pick(&self, name: &str) -> Result<std::result::Result<Pick, NoPick>> {
        let rule = match self.stored_rules {
            true => self
                .store
                .request::<Option<rule::Rule>>("rule_select", json!({"bot":name}))?,
            false => None,
        };
        let key = rule.as_ref().map(rule::Rule::key);
        let bot = self
            .store
            .client
            .get::<serde_json::Value>(&format!("/api/bots/{name}"), &[])?;
        let now = now_unix();
        let session = bot["session_agent"]
            .as_str()
            .and_then(|a| self.runtimes.resolve(a))
            .filter(|r| r.healthy(now) && self.can_run(&r.name, true, true));
        if let Some(rt) = session {
            return Ok(Ok(Pick {
                agent: rt.name.clone(),
                rule: key.unwrap_or_else(|| "bot session".into()),
                with: rule
                    .as_ref()
                    .and_then(|r| r.uses.iter().find(|u| u.harness == rt.harness).cloned()),
                profile: None,
            }));
        }
        if let Some(r) = &rule {
            match self.pick_from(Some(r), true) {
                Ok(p) => return Ok(Ok(p)),
                Err(NoPick::Missing(note)) => {
                    eprintln!("axe node: bot {name}: {note}; using its own agent");
                }
                Err(e) => return Ok(Err(e)),
            }
        }
        let own = bot["agent"].as_str().unwrap_or("claude");
        Ok(self.pin_pick(own, "bot agent", true))
    }

    fn spawn_bot(&mut self, c: &Claimed, pick: Option<&Pick>) -> Result<()> {
        let b = &c.bot;
        let rt = self
            .runtimes
            .resolve(&b.agent)
            .with_context(|| format!("unknown agent {}", b.agent))?
            .clone();
        let agent = self
            .cfg
            .agents
            .get(&rt.agent)
            .with_context(|| format!("unknown agent {}", rt.agent))?
            .clone();
        let bot_args = agent
            .bot_args
            .clone()
            .with_context(|| format!("agent {} has no bot_args", rt.agent))?;
        let session_mode = agent.session_mode()?;
        let resume = c.session.as_ref().filter(|s| {
            self.runtimes.resolve(&s.agent).map(|r| &r.name) == Some(&rt.name)
                && agent.resume_cmd.is_some()
        });
        // Docs attached to the bot (owner `bot:<name>`), its project and global:
        // bundles with a prompt part (the bot protocol, a channel) are inlined,
        // everything else is listed.
        let attached = crate::context::resolve_or_empty(
            &self.store.client,
            &crate::context::bot_scopes(&b.name, &b.project),
            &[],
        );
        // A bot runs in its run folder, like a task, with a worktree of each
        // of its repos (none when it has none) and its attached skills written
        // in full. Its worktrees are not fetched: the bot syncs a repo when
        // it needs it (`axe repo sync`), so a reply does not wait on Git.
        let (cwd, repos_cloned) = crate::run_workspace::build_bot(
            &self.cfg,
            &b.name,
            b.home(),
            b.repos.as_deref().unwrap_or_default(),
            &attached,
            &self.store.client,
        )?;
        let dir = self.cfg.log_dir().join("bots").join(b.home());
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let system_file = dir.join("system.md");
        let system = bot::system_prompt(b, &crate::context::bot_prompt_parts(&attached))
            + &crate::context::prompt_section(&attached);
        std::fs::write(&system_file, &system)
            .with_context(|| format!("write {}", system_file.display()))?;
        // `system.md` is rewritten every run; this run's copy is for the run
        // page (`run_log::run_files`). Best effort.
        let _ = std::fs::write(dir.join(format!("{}.system.md", c.run)), &system);
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
        let with = pick
            .and_then(|p| p.with.as_ref())
            .filter(|u| u.harness == rt.harness);
        let mut extra = bot_args;
        if let Some(u) = with {
            extra.extend(u.args());
        }
        let argv: Vec<String> = splice_bot_args(template, &extra)
            .iter()
            .map(|a| render(a, &vars))
            .collect();
        let mode = if resume.is_some() { "resume" } else { "fresh" };
        let attrs = self.cfg.otel.attributes(&[
            ("bot", &b.name),
            ("project", &b.project),
            ("agent", &rt.name),
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
        env.push(("AXE_WORKTREE", cwd.display().to_string()));
        let started_at = super::now_unix();
        let (login, unset) = rt.env();
        env.extend(login);
        // Sealed form values for this bot: the folder, and the env ones.
        let secrets = crate::sealed::secrets_dir(
            &self.cfg.run_root(),
            &crate::sealed::Target::Bot {
                bot: b.home().to_string(),
            },
        )
        .expect("a bot has a secrets folder");
        env.push(("AXE_SECRETS_DIR", secrets.display().to_string()));
        let secret_env = crate::sealed::launch_env(&secrets);
        env.extend(secret_env.iter().map(|(k, v)| (k.as_str(), v.clone())));
        let mut remove = vec!["AXE_TASK", "AXE_WORKTREE"];
        remove.extend(unset);
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
            .with_context(|| format!("create log {}", log_path.display()))?;
        let mut command = agent_proc::command(&argv, &cwd, &remove, &env, log)?;
        let event = json!({"run":c.run,"mode":mode,"agent":rt.name,"session_id":session_id,
            "log":log_path,"prompt_file":prompt_path,"system_prompt_file":system_file,
            "settings_file":settings_file,"cmd":agent_proc::logged_cmd(&argv, &prompt_path),
            "worktree":cwd,"repos_cloned":repos_cloned,"wakes":c.wakes.iter().map(|w| w.id).collect::<Vec<_>>(),
            "rule":pick.map(|p| p.rule.clone()),"use":with.map(|u| u.to_string())});
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
            agent: rt.name.clone(),
            session_mode,
            session_id,
            log_path,
            child,
            activity: None,
            log_len: 0,
            started_at,
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
                    "axe node: bot {} run {} kill: {e:#}; killing its group",
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
            crate::run_log::note_growth(&run.log_path, len);
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
        let len = std::fs::metadata(&run.log_path).map_or(0, |m| m.len());
        if len != run.log_len {
            crate::run_log::note_growth(&run.log_path, len);
        }
        if let SessionMode::Jsonl(key) = &run.session_mode {
            if let Ok(text) = std::fs::read_to_string(&run.log_path) {
                run.session_id = agent_proc::parse_jsonl_session(&text, key).or(run.session_id);
            }
        }
        let usage = crate::usage::parse_log(&run.log_path);
        // A failed exit caused by quota, login or provider is not the bot's failure.
        let outage = (reason == "exit" && code != Some(0))
            .then(|| crate::failure::classify(&run.log_path))
            .flatten();
        self.note_runtime(&run.agent, outage.as_ref(), &run.log_path, run.started_at);
        let (reason, note) = match &outage {
            Some(f) => ("api_error", Some(format!("model API error {}", f.note()))),
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
