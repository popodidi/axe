//! The coding CLIs axe drives (harnesses), one row each: everything axe
//! knows about a harness is here, so adding one is adding a row. Callers
//! look a harness up by name ([`get`]) or by its binary ([`by_bin`]).

/// The harness of a task with no rule, and of runtime use labels then.
pub const DEFAULT_HARNESS: &str = "claude-code";

/// The agent template a bot runs on when none is set (`[agents.claude]`,
/// built in).
pub const DEFAULT_AGENT: &str = "claude";

/// Which session transcripts a harness keeps under its login directory
/// (`retention::scan_sessions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transcripts {
    Claude,
    Codex,
}

#[derive(Debug)]
pub struct Harness {
    /// The name rules, runtimes and agents use (`claude-code`).
    pub name: &'static str,
    /// For people (`Claude`).
    pub display: &'static str,
    /// The CLI's binary: an agent whose `cmd` runs it drives this harness.
    pub bin: &'static str,
    /// The name stem of runtimes `/runtimes` Add creates (`claude-2`).
    pub name_prefix: &'static str,
    /// The CLI's login directory variable and its default directory.
    pub login_var: (&'static str, &'static str),
    /// Exits 0 while logged in.
    pub probe: &'static [&'static str],
    /// How a runtime can be logged in from a sealed form or a relayed
    /// device code: (method, the environment variable it stores, or `None`
    /// for the CLI's own login; label).
    pub logins: &'static [(&'static str, Option<&'static str>, &'static str)],
    /// The format of a login method's credential: (method, prefix, min, max).
    pub login_checks: &'static [(&'static str, &'static str, usize, usize)],
    /// The CLI's own login reading a credential on stdin (`None` method var).
    pub stdin_login: &'static [&'static str],
    /// The CLI's web login, and whether it reads the page's code on stdin.
    pub device_login: (&'static [&'static str], bool),
    /// What a runtime's own login directory shares with the default one, as
    /// links: instructions, settings and skills, never a credential.
    pub shared: &'static [&'static str],
    /// Arguments a bot run splices in when the agent sets no `bot_args`.
    pub bot_args: &'static [&'static str],
    /// The model and effort arguments, with `{model}` / `{effort}`.
    pub model_args: &'static [&'static str],
    pub effort_args: &'static [&'static str],
    /// Its transcripts, for retention.
    pub transcripts: Transcripts,
    /// Runs read mid-run task notes through a settings-file hook
    /// (`task_inbox`).
    pub inbox_hook: bool,
}

pub const HARNESSES: &[Harness] = &[
    Harness {
        name: "claude-code",
        display: "Claude",
        bin: "claude",
        name_prefix: "claude",
        login_var: ("CLAUDE_CONFIG_DIR", "~/.claude"),
        probe: &["claude", "auth", "status"],
        // `claude auth login` prints a claude.ai URL and reads the code the
        // page shows from stdin (relayed from a sealed form); Claude Code
        // reads an API key, or a `claude setup-token` token, from the
        // environment ahead of its login directory.
        logins: &[
            ("device", None, "web sign-in"),
            (
                "token",
                Some("CLAUDE_CODE_OAUTH_TOKEN"),
                "setup-token token",
            ),
            ("api_key", Some("ANTHROPIC_API_KEY"), "API key"),
        ],
        // `claude setup-token` prints `sk-ant-oat01-` and about 95 more
        // characters, wrapped on a narrow terminal.
        login_checks: &[
            ("token", "sk-ant-oat01-", 90, 256),
            ("api_key", "sk-ant-", 40, 256),
        ],
        stdin_login: &[],
        device_login: (&["auth", "login", "--claudeai"], true),
        shared: &[
            "CLAUDE.md",
            "settings.json",
            "skills",
            "agents",
            "commands",
            "plugins",
        ],
        // The system prompt appended from a file and the inbox hooks
        // (`bot::settings_json`) from a settings file.
        bot_args: &[
            "--system-prompt-snapshot",
            "off",
            "--append-system-prompt-file",
            "{system_prompt_file}",
            "--settings",
            "{settings_file}",
        ],
        model_args: &["--model", "{model}"],
        effort_args: &["--effort", "{effort}"],
        transcripts: Transcripts::Claude,
        inbox_hook: true,
    },
    Harness {
        name: "codex",
        display: "Codex",
        bin: "codex",
        name_prefix: "codex",
        login_var: ("CODEX_HOME", "~/.codex"),
        probe: &["codex", "login", "status"],
        // `codex login --with-api-key` reads the key on stdin;
        // `codex login --device-auth` prints a URL and a code.
        logins: &[
            ("device", None, "device code"),
            ("api_key", None, "API key"),
        ],
        login_checks: &[("api_key", "sk-", 20, 512)],
        stdin_login: &["login", "--with-api-key"],
        device_login: (&["login", "--device-auth"], false),
        shared: &["AGENTS.md", "config.toml", "skills"],
        // `codex exec` and `codex exec resume` (verified on codex-cli
        // 0.172): the system prompt as `developer_instructions` (added to
        // Codex's own instructions, not replacing them) and the same inbox
        // hooks as an inline `hooks` table. Codex runs only hooks it has
        // trusted; these are axe's own, written per run, so trust is
        // bypassed for the run.
        bot_args: &[
            "--dangerously-bypass-hook-trust",
            "-c",
            "developer_instructions={system_prompt_toml}",
            "-c",
            "hooks={hooks_toml}",
        ],
        model_args: &["-m", "{model}"],
        effort_args: &["-c", "model_reasoning_effort=\"{effort}\""],
        transcripts: Transcripts::Codex,
        inbox_hook: false,
    },
];

/// The harness named `name`.
pub fn get(name: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.name == name)
}

/// The harness whose CLI binary is `bin` (a file name).
pub fn by_bin(bin: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.bin == bin)
}

/// Whether axe knows harness `name`.
pub fn known(name: &str) -> bool {
    get(name).is_some()
}

/// The known harness names, for messages: `claude-code, codex`.
pub fn names() -> String {
    HARNESSES
        .iter()
        .map(|h| h.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `args` with `{key}` replaced by `value`.
pub fn fill(args: &[&str], key: &str, value: &str) -> Vec<String> {
    let k = format!("{{{key}}}");
    args.iter().map(|a| a.replace(&k, value)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every harness has a complete row: a missing probe, login or argument
    /// template would only fail on a live run.
    #[test]
    fn every_harness_row_is_complete() {
        for h in HARNESSES {
            let n = h.name;
            assert!(!h.display.is_empty() && !h.bin.is_empty(), "{n}");
            assert!(!h.name_prefix.is_empty(), "{n}: name_prefix");
            assert!(
                !h.login_var.0.is_empty() && !h.login_var.1.is_empty(),
                "{n}"
            );
            assert_eq!(h.probe.first(), Some(&h.bin), "{n}: probe runs the CLI");
            assert!(h.probe.len() > 1, "{n}: probe");
            assert!(!h.logins.is_empty(), "{n}: logins");
            for (m, var, label) in h.logins {
                assert!(!m.is_empty() && !label.is_empty(), "{n}");
                if *m != "device" && var.is_none() {
                    assert!(!h.stdin_login.is_empty(), "{n}: {m} needs stdin_login");
                }
            }
            for (m, prefix, min, max) in h.login_checks {
                assert!(h.logins.iter().any(|(x, _, _)| x == m), "{n}: check {m}");
                assert!(!prefix.is_empty() && min < max, "{n}: check {m}");
            }
            if h.logins.iter().any(|(m, _, _)| *m == "device") {
                assert!(!h.device_login.0.is_empty(), "{n}: device_login");
            }
            assert!(!h.shared.is_empty(), "{n}: shared");
            assert!(!h.bot_args.is_empty(), "{n}: bot_args");
            assert!(h.model_args.iter().any(|a| a.contains("{model}")), "{n}");
            assert!(h.effort_args.iter().any(|a| a.contains("{effort}")), "{n}");
            assert_eq!(by_bin(h.bin).map(|x| x.name), Some(n));
        }
        assert!(known(DEFAULT_HARNESS));
        assert_eq!(by_bin(DEFAULT_AGENT).map(|h| h.name), Some(DEFAULT_HARNESS));
    }
}
