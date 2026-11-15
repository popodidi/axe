//! Why an agent run failed, read from its log: Claude Code stream-json
//! (`claude -p --output-format stream-json`) or Codex JSONL (`codex exec --json`).
//!
//! Three classes are not the task's fault, so the dispatcher retries them later
//! instead of escalating: an exhausted usage quota, an invalid login and an
//! unavailable provider. Only the run's final outcome counts (a retried error the
//! CLI recovered from is not a failure), and anything unrecognized is a task
//! error (`None`), handled as before.
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{json, Value};

pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
pub const AUTH_INVALID: &str = "auth_invalid";
pub const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";

/// Longest message kept, class prefix included.
const MAX_MESSAGE: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub class: &'static str,
    /// HTTP status, when the CLI reported one.
    pub status: Option<i64>,
    pub message: String,
    /// Unix seconds when the quota resets, when the CLI reported it.
    pub retry_at: Option<i64>,
}

impl Failure {
    /// The `api_error` object sent with a run's end. `message` starts with the
    /// class, so every note that quotes it names the class.
    pub fn to_json(&self) -> Value {
        let message: String = format!("{}: {}", self.class, self.message)
            .chars()
            .take(MAX_MESSAGE)
            .collect();
        json!({"class": self.class, "status": self.status, "message": message,
               "retry_at": self.retry_at})
    }

    /// `"<status> <class>: <message>"` for a note, without a missing status.
    pub fn note(&self) -> String {
        note(&self.to_json())
    }
}

/// [`Failure::note`] for a received `api_error` object.
pub fn note(api_error: &Value) -> String {
    let message = api_error["message"].as_str().unwrap_or("");
    match api_error["status"].as_i64() {
        Some(status) => format!("{status} {message}"),
        None => message.to_string(),
    }
}

/// HTTP statuses that mean "the API is unavailable right now", not "this request
/// is wrong": an edge block (403), timeout, rate limit and server errors
/// (including 529 overloaded).
fn transient_status(status: i64) -> bool {
    matches!(status, 403 | 408 | 429 | 500..=599)
}

/// Claude Code's typed `api_error` kinds on a failed `result`, by class.
const CLAUDE_QUOTA: &[&str] = &["usage_limit_reached"];
const CLAUDE_AUTH: &[&str] = &[
    "stored_login_refused",
    "gateway_signin_required",
    "gateway_session_expired",
];
const CLAUDE_OUTAGE: &[&str] = &[
    "connection_failed",
    "stream_idle_timeout",
    "network_edge_blocked",
];

/// Classifies the failure that ended the run logged at `path`.
pub fn classify(path: &Path) -> Option<Failure> {
    let reader = BufReader::new(File::open(path).ok()?);
    let mut result = None;
    let mut rate_limit = None;
    let mut codex_error = None;
    for line in reader.split(b'\n') {
        let Ok(line) = line else { break };
        let text = String::from_utf8_lossy(&line);
        // Cheap filter: only top-level event types we read; JSON nested in tool
        // output is escaped and never matches.
        if ![
            "\"type\":\"result\"",
            "\"type\":\"rate_limit_event\"",
            "\"type\":\"turn.",
            "\"type\":\"error\"",
        ]
        .iter()
        .any(|k| text.contains(k))
        {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(text.trim()) else {
            continue;
        };
        match v["type"].as_str() {
            Some("result") => result = Some(v),
            Some("rate_limit_event") => rate_limit = Some(v["rate_limit_info"].clone()),
            Some("turn.started" | "turn.completed") => codex_error = None,
            Some("turn.failed") => codex_error = v["error"]["message"].as_str().map(String::from),
            Some("error") => codex_error = v["message"].as_str().map(String::from),
            _ => {}
        }
    }
    match result {
        Some(r) => claude(&r, rate_limit.as_ref()),
        None => codex(codex_error.as_deref()?),
    }
}

/// Claude Code: the last `result` must be an error. A quota shows as the typed
/// `usage_limit_reached` or a latest `rate_limit_event` with status `rejected`
/// (its `resetsAt` is the retry time); an expired login as 401 or a typed login
/// kind; an outage as a transient status or a typed network kind.
fn claude(r: &Value, rate_limit: Option<&Value>) -> Option<Failure> {
    if r["is_error"] != true {
        return None;
    }
    let status = r["api_error_status"].as_i64();
    let kind = r["api_error"].as_str().unwrap_or("");
    let api_error = r["terminal_reason"] == "api_error";
    let rejected = rate_limit.filter(|l| l["status"] == "rejected");
    let class = if CLAUDE_QUOTA.contains(&kind) || rejected.is_some() {
        QUOTA_EXHAUSTED
    } else if CLAUDE_AUTH.contains(&kind) || (api_error && status == Some(401)) {
        AUTH_INVALID
    } else if CLAUDE_OUTAGE.contains(&kind) || (api_error && status.is_some_and(transient_status)) {
        PROVIDER_UNAVAILABLE
    } else {
        return None;
    };
    let retry_at = (class == QUOTA_EXHAUSTED)
        .then(|| rate_limit?["resetsAt"].as_i64())
        .flatten()
        // Seconds in the stream; accept milliseconds too.
        .map(|t| if t > 100_000_000_000 { t / 1000 } else { t });
    Some(Failure {
        class,
        status,
        message: r["result"].as_str().unwrap_or("").to_string(),
        retry_at,
    })
}

/// Codex: the message of the last `turn.failed` or `error` event not followed
/// by a new or completed turn. Codex reports these as text only; the phrases are
/// its `CodexErr` messages (openai/codex `codex-rs/protocol/src/error.rs`).
fn codex(message: &str) -> Option<Failure> {
    let m = message.to_lowercase().replace('\u{2019}', "'");
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    let class = if has(&[
        "hit your usage limit",
        "quota exceeded",
        "workspace is out of credits",
        "hit your spend cap",
    ]) {
        QUOTA_EXHAUSTED
    } else if has(&[
        "access token could not be refreshed",
        "unexpected status 401",
    ]) {
        AUTH_INVALID
    } else if has(&[
        "model is at capacity",
        "experiencing high demand",
        "exceeded retry limit, last status: 429",
        "exceeded retry limit, last status: 5",
        "unexpected status 5",
    ]) {
        PROVIDER_UNAVAILABLE
    } else {
        return None;
    };
    Some(Failure {
        class,
        status: None,
        message: message.to_string(),
        retry_at: None,
    })
}
