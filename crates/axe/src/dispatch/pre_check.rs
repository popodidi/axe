//! Routine pre-checks (`routine.rs`, `pre_check`): a shell command the
//! dispatcher runs in the task's run folder before its first agent run.
//!
//! Exit 0 marks the task done with no agent (`pre_check_done`); any other exit,
//! a signal or the timeout starts the agent under the same claim. Either way
//! the command's output (its tail) is a task note. A pre-check holds its claim
//! and a task slot while it runs; a stop or a drain kills it and returns the
//! task to pending, where the next claim runs it again.
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use super::{Dispatcher, Pick};
use crate::agent_proc::{self, kill_tree};
use crate::db::Task;

/// Longest a pre-check runs before the agent starts anyway.
pub const TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Most bytes of a pre-check's output kept in its note (the tail).
const NOTE_MAX: u64 = 3000;

pub(super) struct PreCheck {
    task_id: i64,
    claim: String,
    pick: Pick,
    child: Child,
    log_path: PathBuf,
    deadline: Instant,
}

/// The last `NOTE_MAX` bytes of the log, as text.
fn tail(path: &std::path::Path) -> String {
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let cut = len > NOTE_MAX;
    if cut && f.seek(SeekFrom::Start(len - NOTE_MAX)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).trim().to_string();
    if cut {
        format!("[...]{text}")
    } else {
        text
    }
}

impl Dispatcher {
    /// Starts `cmd` (`sh -c`) for `task` in its run folder, with the task's
    /// axe variables and sealed values, its output to a `.pre-check.log`.
    pub(super) fn start_pre_check(
        &mut self,
        task: &Task,
        pick: &Pick,
        claim: &str,
        cmd: &str,
    ) -> Result<()> {
        let (cwd, _) = self.run_dir(task, pick)?;
        let log_dir = self.cfg.log_dir();
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("create log dir {}", log_dir.display()))?;
        let mut log_path = log_dir.join(format!("{}-{}.pre-check.log", task.id, task.runs));
        if log_path.exists() {
            log_path = log_dir.join(format!(
                "{}-{}-{}.pre-check.log",
                task.id,
                task.runs,
                uuid::Uuid::new_v4()
            ));
        }
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_path)
            .with_context(|| format!("create log {}", log_path.display()))?;
        let mut env = vec![
            ("AXE_ACTOR", "pre-check".to_string()),
            ("AXE_SESSION", claim.to_string()),
            ("AXE_TASK", task.id.to_string()),
        ];
        if let Some(url) = &self.server_url {
            env.push(("AXE_SERVER", url.clone()));
        }
        env.push(("AXE_WORKTREE", cwd.display().to_string()));
        let secrets = crate::sealed::secrets_dir(
            &self.cfg.run_root(),
            &crate::sealed::Target::Task { task: task.id },
        )
        .expect("a task has a secrets folder");
        env.push(("AXE_SECRETS_DIR", secrets.display().to_string()));
        let secret_env = crate::sealed::launch_env(&secrets);
        env.extend(secret_env.iter().map(|(k, v)| (k.as_str(), v.clone())));
        let argv = ["sh".to_string(), "-c".to_string(), cmd.to_string()];
        let child = agent_proc::command(&argv, &cwd, &["AXE_WORKTREE"], &env, log)?
            .spawn()
            .context("spawn sh for the pre-check")?;
        self.pre_checks.push(PreCheck {
            task_id: task.id,
            claim: claim.to_string(),
            pick: pick.clone(),
            child,
            log_path,
            deadline: Instant::now() + TIMEOUT,
        });
        Ok(())
    }

    /// Settles every pre-check that exited or ran past its timeout.
    pub(super) fn reap_pre_checks(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.pre_checks.len() {
            let pc = &mut self.pre_checks[i];
            let outcome = match pc.child.try_wait()? {
                Some(status) => Some(match status.code() {
                    Some(c) => format!("exit {c}"),
                    None => "killed by a signal".into(),
                }),
                None if Instant::now() >= pc.deadline => {
                    if let Err(e) = kill_tree(&mut pc.child) {
                        eprintln!("axe dispatcher: pre-check kill unconfirmed: {e:#}");
                        i += 1;
                        continue;
                    }
                    Some(format!("timed out after {}s", TIMEOUT.as_secs()))
                }
                None => None,
            };
            let Some(outcome) = outcome else {
                i += 1;
                continue;
            };
            let pc = self.pre_checks.swap_remove(i);
            self.settle(pc, &outcome)?;
        }
        Ok(())
    }

    fn settle(&mut self, pc: PreCheck, outcome: &str) -> Result<()> {
        let out = tail(&pc.log_path);
        let out = if out.is_empty() {
            "(no output)".into()
        } else {
            out
        };
        let log = pc.log_path.display();
        if outcome == "exit 0" {
            let note = format!("pre-check exit 0: done with no agent run (log {log})\n{out}");
            self.store.request::<bool>(
                "pre_check_done",
                json!({"task":pc.task_id,"claim":pc.claim,"note":note}),
            )?;
            return Ok(());
        }
        let task = self
            .store
            .request::<Task>("task", json!({"task":pc.task_id}))?;
        // Someone resolved the task meanwhile: the claim is gone.
        if task.state != "in_progress" || task.claimed_by.as_deref() != Some(&pc.claim) {
            return Ok(());
        }
        // A rollout drain admits no new agent: the next claim runs it all again.
        if self.store.draining.load(Ordering::SeqCst) {
            let note = format!(
                "pre-check {outcome} while the dispatcher drains: back to pending, it runs \
                 again on the next claim (log {log})\n{out}"
            );
            self.store.request::<bool>(
                "release",
                json!({"task":pc.task_id,"claim":pc.claim,"wait":1,"note":note}),
            )?;
            return Ok(());
        }
        let note = format!("pre-check {outcome}: starting the agent (log {log})\n{out}");
        self.store.request::<serde_json::Value>(
            "note",
            json!({"task":pc.task_id,"claim":pc.claim,"note":note}),
        )?;
        if let Err(e) = self.spawn(&task, &pc.pick, &pc.claim, None, false) {
            self.block_claimed(task.id, &pc.claim, format!("{e:#}"))?;
        }
        Ok(())
    }

    /// Kills every running pre-check and returns its task to pending, the run
    /// refunded, so the next claim runs the pre-check again.
    pub(super) fn stop_pre_checks(&mut self, why: &str) -> Result<()> {
        for mut pc in std::mem::take(&mut self.pre_checks) {
            if let Err(e) = kill_tree(&mut pc.child) {
                eprintln!("axe dispatcher: pre-check kill unconfirmed: {e:#}");
            }
            self.store.request::<bool>(
                "release",
                json!({"task":pc.task_id,"claim":pc.claim,"wait":1,
                       "note":format!("pre-check {why}; it runs again on the next claim")}),
            )?;
        }
        Ok(())
    }
}
