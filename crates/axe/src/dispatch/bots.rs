//! Bot runs in the node: claim ready bots, render their system prompt and
//! hook settings, spawn, relay activity, kill stopped or paused runs, and finish.
//! Bots have their own pool (`[pool] bots`), separate from task slots, and
//! the resource gate holds them only at its hard floor (`admission.rs`).
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc;

use anyhow::{Context, Result};
use serde_json::json;

use super::{Dispatcher, NoPick, Pick};
use crate::activity;
use crate::agent_proc::{self, kill_child, kill_tree, render};
use crate::bot::{self, Claimed};
use crate::config::SessionMode;
use crate::transcribe::{self, Transcript};

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

/// A claimed bot run waiting for its voice messages' transcripts
/// (`transcribe.rs`) before it spawns. A worker thread fetches each voice
/// file through the API and transcribes it; the node loop spawns the run
/// once all are back ([`Dispatcher::poll_preparing`]).
pub(super) struct Preparing {
    claimed: Claimed,
    pick: Option<Pick>,
    done: mpsc::Receiver<Vec<(i64, Transcript)>>,
}

/// The voice messages among `c`'s wakes still without a transcript.
pub fn voice_wakes(c: &Claimed) -> Vec<i64> {
    c.wakes
        .iter()
        .filter_map(|w| w.message.as_ref())
        .filter(|m| transcribe::needs_transcript(&m.kind, &m.flags))
        .map(|m| m.id)
        .collect()
}

/// Puts `results` on `c`'s wake messages, as stored on the server.
pub fn apply_transcripts(c: &mut Claimed, results: &[(i64, Transcript)]) {
    for w in &mut c.wakes {
        let Some(m) = &mut w.message else { continue };
        if let Some((_, t)) = results.iter().find(|(id, _)| *id == m.id) {
            if !m.flags.is_object() {
                m.flags = json!({});
            }
            m.flags["transcript"] = json!(t);
        }
    }
}

/// Fetches and transcribes voice messages `msgs` of bot `bot` (the worker
/// thread's job): every message gets a transcript, failures included.
pub fn transcribe_voices(
    client: &crate::client::Client,
    engine: &dyn transcribe::Engine,
    bot: &str,
    msgs: &[i64],
) -> Vec<(i64, Transcript)> {
    msgs.iter()
        .map(|&id| {
            let t = match client.get_file(&format!("/api/bots/{bot}/tg/file/{id}")) {
                Ok((_, bytes)) => transcribe::transcribe(engine, &bytes),
                Err(e) => Transcript {
                    error: Some(format!("could not fetch the voice file: {e:#}")),
                    engine: engine.name(),
                    ..Default::default()
                },
            };
            (id, t)
        })
        .collect()
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
        self.cfg.pool_bots().is_none_or(|n| self.live_bots() < n)
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
            let (transcribe_voice, pick) = self.bot_pick(&name)?;
            let pick = match pick {
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
            let voices = if transcribe_voice {
                voice_wakes(&c)
            } else {
                Vec::new()
            };
            if !voices.is_empty() {
                self.prepare_bot(c, pick, voices);
                continue;
            }
            if let Err(e) = self.spawn_bot(&c, pick.as_ref()) {
                self.store.request::<serde_json::Value>(
                    "bot_finish",
                    json!({"run":c.run,"code":null,"reason":"error","note":format!("{e:#}")}),
                )?;
            }
        }
        Ok(())
    }

    /// The runtime of bot `name`'s next run: its primary (the bot's agent)
    /// while healthy, else its backup ([`bot::Bot::backup`]) while healthy,
    /// else none (its wakes wait). A run on another runtime than its
    /// session's starts a fresh session (`spawn_bot`), so moving to the
    /// backup and back each start fresh; notes carry over. A stored rule
    /// naming the bot only sets the model and effort of the picked runtime's
    /// harness ([`crate::runtime_config`]).
    /// Also returns whether the bot's voice messages are transcribed
    /// (`RuntimeConfig::transcribe_voice`).
    fn bot_pick(&self, name: &str) -> Result<(bool, std::result::Result<Pick, NoPick>)> {
        // Bots run no task, so the config's task limits do not apply.
        let config = self.runtime_config(json!({"bot":name}), "")?;
        Ok((config.transcribe_voice, self.pick(&config, true)))
    }

    /// Starts transcribing claimed run `c`'s voice messages `msgs` on a
    /// worker thread; [`Dispatcher::poll_preparing`] spawns the run after.
    fn prepare_bot(&mut self, c: Claimed, pick: Option<Pick>, msgs: Vec<i64>) {
        let (tx, rx) = mpsc::channel();
        // A failing download must not hold the reply for the node's whole
        // retry budget.
        let client = self
            .store
            .client
            .clone()
            .retrying(std::time::Duration::from_secs(15));
        let bot = c.bot.name.clone();
        let cache = self.stt_dir.clone();
        eprintln!(
            "axe node: bot {bot} run {}: transcribing voice message(s) {msgs:?}",
            c.run
        );
        std::thread::spawn(move || {
            let results = match cache {
                Some(dir) => {
                    let engine = transcribe::SenseVoice::new(dir);
                    transcribe_voices(&client, &engine, &bot, &msgs)
                }
                None => msgs
                    .iter()
                    .map(|&id| {
                        (
                            id,
                            Transcript {
                                error: Some("no cache folder for the speech model".into()),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
            };
            let _ = tx.send(results);
        });
        self.preparing.push(Preparing {
            claimed: c,
            pick,
            done: rx,
        });
    }

    /// Spawns the preparing runs whose transcripts are back, after storing
    /// each on its message. A lost worker counts as a failed transcription.
    pub(super) fn poll_preparing(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.preparing.len() {
            let results = match self.preparing[i].done.try_recv() {
                Ok(r) => r,
                Err(mpsc::TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => voice_wakes(&self.preparing[i].claimed)
                    .into_iter()
                    .map(|id| {
                        (
                            id,
                            Transcript {
                                error: Some("the transcription worker stopped".into()),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
            };
            let Preparing {
                mut claimed, pick, ..
            } = self.preparing.swap_remove(i);
            for (msg, t) in &results {
                match (&t.text, &t.error) {
                    (Some(text), _) => eprintln!(
                        "axe node: bot {} voice #{msg}: {} chars, {:.1}s audio, {} ms, peak {} MB",
                        claimed.bot.name,
                        text.chars().count(),
                        t.audio_secs.unwrap_or_default(),
                        t.ms,
                        t.peak_rss_mb.map_or("?".into(), |m| m.to_string()),
                    ),
                    (None, e) => eprintln!(
                        "axe node: bot {} voice #{msg}: transcription failed: {}",
                        claimed.bot.name,
                        e.as_deref().unwrap_or("-")
                    ),
                }
                // Best effort: the prompt carries it either way.
                if let Err(e) = self.store.request::<serde_json::Value>(
                    "bot_transcript",
                    json!({"run":claimed.run,"message":msg,"transcript":t}),
                ) {
                    eprintln!(
                        "axe node: bot {} voice #{msg}: store transcript: {e:#}",
                        claimed.bot.name
                    );
                }
            }
            apply_transcripts(&mut claimed, &results);
            if let Err(e) = self.spawn_bot(&claimed, pick.as_ref()) {
                self.store.request::<serde_json::Value>(
                    "bot_finish",
                    json!({"run":claimed.run,"code":null,"reason":"error","note":format!("{e:#}")}),
                )?;
            }
        }
        Ok(())
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
            .bot_args()
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
            ("system_prompt_toml", bot::toml_string(&system)),
            ("hooks_toml", bot::hooks_toml(exe)),
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
        let spliced = splice_bot_args(template, &extra);
        let argv: Vec<String> = spliced.iter().map(|a| render(a, &vars)).collect();
        // The logged command names the files instead of the inlined system
        // prompt and hooks (`{system_prompt_toml}`, `{hooks_toml}`).
        let mut shown_vars = vars.clone();
        shown_vars.insert("system_prompt_toml", format!("@{}", system_file.display()));
        shown_vars.insert("hooks_toml", format!("@{}", settings_file.display()));
        let shown: Vec<String> = spliced.iter().map(|a| render(a, &shown_vars)).collect();
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
            "settings_file":settings_file,"cmd":agent_proc::logged_cmd(&shown, &prompt_path),
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
        self.poll_preparing()?;
        if self.live_bots() == 0 {
            return Ok(());
        }
        let kills: Vec<(i64, String)> = self
            .store
            .request("bot_control", json!({}))
            .unwrap_or_default();
        // A run stopped or paused while its voice messages are transcribed
        // never spawns; its worker's result is dropped.
        let mut j = 0;
        while j < self.preparing.len() {
            let run = self.preparing[j].claimed.run;
            let Some((_, reason)) = kills.iter().find(|(r, _)| *r == run) else {
                j += 1;
                continue;
            };
            let reason = reason.clone();
            self.preparing.swap_remove(j);
            self.store.request::<serde_json::Value>(
                "bot_finish",
                json!({"run":run,"code":null,"reason":reason,
                    "note":format!("bot {reason} requested before the run started")}),
            )?;
        }
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
        for p in std::mem::take(&mut self.preparing) {
            self.store.request::<serde_json::Value>(
                "bot_finish",
                json!({"run":p.claimed.run,"code":null,"reason":"dispatcher_stopped","note":null}),
            )?;
        }
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
