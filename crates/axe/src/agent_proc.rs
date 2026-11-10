//! Agent process plumbing shared by task and bot runs: command templates,
//! spawning in an owned process group with a log, session parsing, telemetry
//! attributes, and termination.
use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;

/// Longest argument kept verbatim in a `run_start` event's `cmd`.
pub const MAX_LOGGED_ARG: usize = 200;

/// A worker command: its own process group (and, on Linux, a child subreaper),
/// stdin closed, stdout and stderr appended to `log`. `env` is applied after
/// `env_remove`, so a removed variable can be set again.
pub fn command(
    argv: &[String],
    cwd: &Path,
    env_remove: &[&str],
    env: &[(&str, String)],
    log: File,
) -> Result<Command> {
    let mut command = Command::new(&argv[0]);
    // Keep double-forked descendants attached to the managed worker, even
    // when they create a new session. Retained across exec on Linux.
    #[cfg(target_os = "linux")]
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .process_group(0)
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for k in crate::compat::both_names(env_remove) {
        command.env_remove(k);
    }
    for (k, v) in crate::compat::child_env(env) {
        command.env(k, v);
    }
    Ok(command)
}

/// The argv recorded in a `run_start` event: arguments longer than
/// [`MAX_LOGGED_ARG`] (the prompt) are replaced by `@<prompt file>`.
pub fn logged_cmd(argv: &[String], prompt_path: &Path) -> Vec<String> {
    let prompt_ref = format!("@{}", prompt_path.display());
    argv.iter()
        .map(|a| {
            if a.chars().count() > MAX_LOGGED_ARG {
                prompt_ref.clone()
            } else {
                a.clone()
            }
        })
        .collect()
}

/// Finds the session id in agent JSONL output: the first line carrying `key`,
/// else the first carrying `session_id`.
pub fn parse_jsonl_session(text: &str, key: &str) -> Option<String> {
    let find = |k: &str| {
        text.lines().find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            v.get(k)?.as_str().map(String::from)
        })
    };
    find(key).or_else(|| find("session_id"))
}

/// Substitutes `{name}` placeholders in one pass so values are never re-expanded.
pub fn render(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after
            .find('}')
            .and_then(|end| vars.get(&after[..end]).map(|v| (end, v)))
        {
            Some((end, v)) => {
                out.push_str(v);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Appends `attrs` to an inherited `OTEL_RESOURCE_ATTRIBUTES` value, percent-encoding
/// characters that would break the `key=value,...` format.
pub fn otel_resource_attributes(inherited: Option<&str>, attrs: &[(&str, &str)]) -> String {
    let encode = |v: &str| {
        let mut out = String::new();
        for c in v.chars() {
            if matches!(c, '%' | ',' | '=' | ';' | '"' | '\\')
                || c.is_whitespace()
                || c.is_control()
            {
                for b in c.to_string().bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let ours = attrs
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect::<Vec<_>>()
        .join(",");
    match inherited.map(str::trim).filter(|s| !s.is_empty()) {
        Some(prev) => format!("{prev},{ours}"),
        None => ours,
    }
}

/// Linux pidfds fence signals against PID reuse. Freeze the tree to prevent
/// further forks before SIGKILL; never acknowledge an incomplete termination.
#[cfg(target_os = "linux")]
pub fn kill_tree(child: &mut Child) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    struct Process {
        pid: i32,
        fd: OwnedFd,
        owned: bool,
    }
    impl Process {
        fn open(pid: i32) -> Result<Self> {
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                pid,
                fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
                owned: false,
            })
        }
        fn signal(&self, signal: i32) -> Result<()> {
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::ESRCH) {
                    return Err(e.into());
                }
            }
            Ok(())
        }
        fn exited(&self) -> bool {
            let mut poll = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut poll, 1, 0) > 0 }
        }
    }
    // On any error, kill what was frozen, but do not confirm termination.
    impl Drop for Process {
        fn drop(&mut self) {
            if self.owned {
                let _ = self.signal(libc::SIGKILL);
            }
        }
    }
    fn stat(pid: i32) -> Result<(char, i32)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let mut fields = text
            .rsplit_once(") ")
            .context("invalid process stat")?
            .1
            .split_whitespace();
        let state = fields
            .next()
            .context("missing state")?
            .chars()
            .next()
            .unwrap();
        let parent = fields.next().context("missing parent")?.parse()?;
        Ok((state, parent))
    }
    let mut processes = vec![Process::open(child.id() as i32)?];
    processes[0].owned = true;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for process in &processes {
            process.signal(libc::SIGSTOP)?;
            while !process.exited() && !matches!(stat(process.pid)?.0, 'T' | 't') {
                anyhow::ensure!(Instant::now() < deadline, "process did not stop");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        anyhow::ensure!(
            !processes[0].exited(),
            "worker exited before tree could be frozen"
        );
        let mut added = false;
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok((_, parent)) = stat(pid) else { continue };
            if processes.iter().any(|p| p.pid == parent) && !processes.iter().any(|p| p.pid == pid)
            {
                match Process::open(pid) {
                    Ok(mut process) => {
                        // Revalidate parent after opening the stable process handle.
                        if stat(pid)
                            .is_ok_and(|(_, parent)| processes.iter().any(|p| p.pid == parent))
                        {
                            process.owned = true;
                            processes.push(process);
                            added = true;
                        }
                    }
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.raw_os_error() == Some(libc::ESRCH)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        if !added {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "process tree did not quiesce");
    }
    for process in processes.iter().rev() {
        process.signal(libc::SIGKILL)?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while processes.iter().any(|p| !p.exited()) {
        anyhow::ensure!(Instant::now() < deadline, "process termination unconfirmed");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn kill_tree(_child: &mut Child) -> Result<()> {
    anyhow::bail!("remote abort requires a Linux dispatcher with pidfd support")
}

pub fn kill_child(child: &mut Child) {
    // Each worker has its own process group, including subprocesses writing logs.
    // The group id is the child PID created above; never signal the dispatcher's group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
}
