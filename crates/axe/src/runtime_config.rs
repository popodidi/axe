//! The one place a run's runtime, model and effort are resolved: the
//! layers built-in default -> project (the node config's limits and the
//! stored rule that matches the subject) -> bot (its primary and backup) ->
//! task (its agent pin) merge into one [`RuntimeConfig`]. The node then only
//! applies runtime health to it (`Dispatcher::pick`); the server labels each
//! runtime's uses with it (`server::runtimes_view`); `axe rule explain`
//! shows it.
//!
//! The project layer keeps today's selectors (owner decision (a), task
//! 1990): a rule matches on workload, repo, goal, label (the project is a
//! label), owner or bot, and the most specific rule wins its whole `use`
//! list ([`crate::rule::select`]); a bot sees only rules that name a bot.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::rule::{Rule, Use};

/// The bot layer: a bot's primary runtime (or agent template) and backup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotLayer {
    pub name: String,
    pub primary: String,
    #[serde(default)]
    pub backup: Option<String>,
}

/// What the server stores for one subject (a task or a bot): the matched
/// rule, the bot row, the task pin. The node fetches it in one operation
/// (`runtime_layers`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Layers {
    /// The stored rule [`crate::rule::select`] picked for the subject.
    #[serde(default)]
    pub rule: Option<Rule>,
    /// Set for a bot.
    #[serde(default)]
    pub bot: Option<BotLayer>,
    /// A task's agent pin (`axe task add --agent`).
    #[serde(default)]
    pub task_agent: Option<String>,
    /// The node config's agent list, only on a node whose config could not
    /// be imported as stored rules (`Dispatcher::init_rules`); stored rules
    /// do not apply then.
    #[serde(default)]
    pub config: Option<ConfigLayer>,
}

/// A project's `[defaults]`/`[[rule]]` agent list from the node config
/// (`Config::resolve`) and the agent it picks: `agents[index]`, the
/// round-robin cursor (the `round_robin` operation) or 0 for a fixed list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigLayer {
    pub agents: Vec<String>,
    pub index: usize,
    /// The config rule that supplied `agents` (`defaults`, `project=x`).
    pub rule_key: String,
}

impl Layers {
    /// Task `id`'s stored layers: the rule matching it, its agent pin.
    pub fn of_task(conn: &Connection, id: i64) -> Result<Layers> {
        let subject = crate::rule::task_subject(conn, id)?;
        let rules = crate::rule::list(conn)?;
        Ok(Layers {
            rule: crate::rule::select(&rules, &subject).cloned(),
            bot: None,
            task_agent: crate::db::get_task(conn, id)?.agent,
            config: None,
        })
    }

    /// Bot `name`'s stored layers: the rule naming it, its primary and
    /// backup.
    pub fn of_bot(conn: &Connection, name: &str) -> Result<Layers> {
        let subject = crate::rule::bot_subject(conn, name)?;
        let rules = crate::rule::list(conn)?;
        let b = crate::bot::get(conn, name)?;
        Ok(Layers {
            rule: crate::rule::select(&rules, &subject).cloned(),
            bot: Some(BotLayer {
                name: b.name,
                primary: b.agent,
                backup: b.backup,
            }),
            task_agent: None,
            config: None,
        })
    }
}

/// The node config's run limits for a project: `[defaults]`, then each
/// matching `[[rule]]` in file order, catch-all rules first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub max_runs: i64,
    pub max_resumes: Option<u64>,
}

impl Limits {
    pub fn of(cfg: &Config, project: &str) -> Limits {
        let r = cfg.resolve(project);
        Limits {
            max_runs: r.max_runs,
            max_resumes: r.max_resumes,
        }
    }
}

/// A subject's resolved runtime config (before health).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// The pinned runtime or agent template: the task's pin, or the bot's
    /// primary. `None`: walk [`RuntimeConfig::uses`].
    pub runtime: Option<String>,
    /// A bot's backup runtime.
    pub backup: Option<String>,
    /// The harness entries to walk, in order, with their model and effort:
    /// the matched rule's `use` list, else the built-in default harness.
    pub uses: Vec<Use>,
    /// The matched rule's `use` list, which sets the model and effort of a
    /// pinned or resumed runtime's harness; `None` with no rule.
    pub rule_uses: Option<Vec<Use>>,
    /// The matched rule (`rule:N (match)`), or `no rule`.
    pub rule_key: String,
    /// What picked the runtime, as runs record it: `task agent`, `bot
    /// primary`, or [`RuntimeConfig::rule_key`].
    pub why: String,
    pub max_runs: i64,
    pub max_resumes: Option<u64>,
}

/// The built-in default layer: the default harness, no model or effort.
fn default_uses() -> Vec<Use> {
    vec![Use {
        harness: crate::harness::DEFAULT_HARNESS.into(),
        model: None,
        effort: None,
    }]
}

impl RuntimeConfig {
    /// Merges the layers, most specific last: default -> project (`limits`,
    /// `l.rule`, or the node config's list `l.config`) -> bot -> task.
    pub fn resolve(l: &Layers, limits: Limits) -> RuntimeConfig {
        // Default and project. The config layer stands in for stored rules
        // on a node that could not import them.
        let rule = l.rule.as_ref().filter(|_| l.config.is_none());
        let rule_key = match &l.config {
            Some(cl) => cl.rule_key.clone(),
            None => rule.map_or("no rule".to_string(), Rule::key),
        };
        let mut c = RuntimeConfig {
            runtime: None,
            backup: None,
            uses: rule.map_or_else(default_uses, |r| r.uses.clone()),
            rule_uses: rule.map(|r| r.uses.clone()),
            why: rule_key.clone(),
            rule_key,
            max_runs: limits.max_runs,
            max_resumes: limits.max_resumes,
        };
        if let Some(cl) = &l.config {
            c.runtime = cl.agents.get(cl.index % cl.agents.len().max(1)).cloned();
        }
        // Bot.
        if let Some(b) = &l.bot {
            let primary = match b.primary.trim() {
                "" => crate::harness::DEFAULT_AGENT.to_string(),
                p => p.to_string(),
            };
            c.runtime = Some(primary);
            c.backup = b.backup.clone();
            c.why = "bot primary".into();
        }
        // Task: its pin beats the config's list too, labelled there with the
        // config rule, as before stored rules.
        if let Some(a) = &l.task_agent {
            c.runtime = Some(a.clone());
            if l.config.is_none() {
                c.why = "task agent".into();
            }
        }
        c
    }

    /// The model and effort a run on `harness` gets from the matched rule.
    pub fn with(&self, harness: &str) -> Option<Use> {
        self.rule_uses
            .as_ref()?
            .iter()
            .find(|u| u.harness == harness)
            .cloned()
    }

    /// Every harness this config may run on, for runtime use labels.
    pub fn harnesses(&self) -> Vec<String> {
        self.uses.iter().map(|u| u.harness.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::Match;

    fn rule(id: i64, uses: &str) -> Rule {
        Rule {
            id,
            matcher: Match {
                label: Some("p".into()),
                ..Default::default()
            },
            uses: uses.split(' ').map(|u| u.parse().unwrap()).collect(),
            priority: 0,
            created_at: String::new(),
        }
    }

    const LIMITS: Limits = Limits {
        max_runs: 6,
        max_resumes: None,
    };

    fn u(h: &str, m: Option<&str>, e: Option<&str>) -> Use {
        Use {
            harness: h.into(),
            model: m.map(Into::into),
            effort: e.map(Into::into),
        }
    }

    /// default -> project -> bot -> task, one row each.
    #[test]
    fn layers_resolve_most_specific_last() {
        struct Row {
            name: &'static str,
            layers: Layers,
            runtime: Option<&'static str>,
            why: &'static str,
            uses: Vec<Use>,
            with_claude: Option<Use>,
            with_codex: Option<Use>,
        }
        let bot = |primary: &str, backup: Option<&str>| BotLayer {
            name: "b".into(),
            primary: primary.into(),
            backup: backup.map(Into::into),
        };
        let rows = [
            Row {
                name: "no rule -> the default harness",
                layers: Layers::default(),
                runtime: None,
                why: "no rule",
                uses: vec![u("claude-code", None, None)],
                with_claude: None,
                with_codex: None,
            },
            Row {
                name: "a project rule replaces the default whole",
                layers: Layers {
                    rule: Some(rule(2, "codex:gpt-5:low claude-code:opus")),
                    ..Default::default()
                },
                runtime: None,
                why: "rule:2 (label=p)",
                uses: vec![
                    u("codex", Some("gpt-5"), Some("low")),
                    u("claude-code", Some("opus"), None),
                ],
                with_claude: Some(u("claude-code", Some("opus"), None)),
                with_codex: Some(u("codex", Some("gpt-5"), Some("low"))),
            },
            Row {
                name: "a bot runs on its primary (#190); a bot rule sets only model and effort",
                layers: Layers {
                    rule: Some(rule(3, "claude-code:sonnet:low")),
                    bot: Some(bot("codex-1", Some("claude-1"))),
                    ..Default::default()
                },
                runtime: Some("codex-1"),
                why: "bot primary",
                uses: vec![u("claude-code", Some("sonnet"), Some("low"))],
                with_claude: Some(u("claude-code", Some("sonnet"), Some("low"))),
                with_codex: None,
            },
            Row {
                name: "a bot with no agent runs on the default agent",
                layers: Layers {
                    bot: Some(bot("", None)),
                    ..Default::default()
                },
                runtime: Some("claude"),
                why: "bot primary",
                uses: vec![u("claude-code", None, None)],
                with_claude: None,
                with_codex: None,
            },
            Row {
                name: "a task pin keeps the rule's model and effort",
                layers: Layers {
                    rule: Some(rule(4, "codex:gpt-5:high")),
                    task_agent: Some("codex".into()),
                    ..Default::default()
                },
                runtime: Some("codex"),
                why: "task agent",
                uses: vec![u("codex", Some("gpt-5"), Some("high"))],
                with_claude: None,
                with_codex: Some(u("codex", Some("gpt-5"), Some("high"))),
            },
        ];
        for r in rows {
            let c = RuntimeConfig::resolve(&r.layers, LIMITS);
            assert_eq!(c.runtime.as_deref(), r.runtime, "{}", r.name);
            assert_eq!(c.why, r.why, "{}", r.name);
            assert_eq!(c.uses, r.uses, "{}", r.name);
            assert_eq!(c.with("claude-code"), r.with_claude, "{}", r.name);
            assert_eq!(c.with("codex"), r.with_codex, "{}", r.name);
            assert_eq!(c.max_runs, 6, "{}", r.name);
        }
        // The node config's list (a config stored rules cannot express):
        // its agent at the cursor, labelled with the config rule; a pin
        // beats it with the same label; stored rules do not apply.
        let cl = ConfigLayer {
            agents: vec!["a".into(), "b".into()],
            index: 3,
            rule_key: "project=x".into(),
        };
        let l = Layers {
            rule: Some(rule(5, "codex")),
            config: Some(cl.clone()),
            ..Default::default()
        };
        let c = RuntimeConfig::resolve(&l, LIMITS);
        assert_eq!(
            (c.runtime.as_deref(), c.why.as_str(), c.with("codex")),
            (Some("b"), "project=x", None)
        );
        let l = Layers {
            task_agent: Some("pinned".into()),
            config: Some(cl),
            ..Default::default()
        };
        let c = RuntimeConfig::resolve(&l, LIMITS);
        assert_eq!(
            (c.runtime.as_deref(), c.why.as_str()),
            (Some("pinned"), "project=x")
        );
        let b = RuntimeConfig::resolve(
            &Layers {
                bot: Some(bot("codex-1", Some("claude-1"))),
                ..Default::default()
            },
            LIMITS,
        );
        assert_eq!(b.backup.as_deref(), Some("claude-1"));
        assert_eq!(b.rule_key, "no rule");
    }

    #[test]
    fn limits_merge_defaults_then_project_rules() {
        let cfg = Config::parse(
            "[defaults]\nmax_runs = 9\n[[rule]]\nmatch = { project = \"a\" }\nmax_runs = 3\nmax_resumes = 2\n\
             [[rule]]\nmax_resumes = 5\n",
        )
        .unwrap();
        assert_eq!(
            Limits::of(&cfg, "a"),
            Limits {
                max_runs: 3,
                max_resumes: Some(2)
            }
        );
        assert_eq!(
            Limits::of(&cfg, "b"),
            Limits {
                max_runs: 9,
                max_resumes: Some(5)
            }
        );
    }
}
