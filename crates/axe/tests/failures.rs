//! Failure classes from agent logs (`failure.rs`). Fixtures in
//! `fixtures/failures/` are redacted real dispatcher logs where one exists
//! (Claude Code connection failure, 401 retry, missing session, not logged in;
//! Codex "at capacity"); the others keep a real log's shape with the error message the
//! CLI defines for that case (Claude Code result `api_error` kinds and
//! `rate_limit_event`; Codex `CodexErr` messages).
use std::path::Path;

use axe::failure::{classify, Failure, AUTH_INVALID, PROVIDER_UNAVAILABLE, QUOTA_EXHAUSTED};
use axe::remote::{retry_wait, QUOTA_RETRY_SECONDS};

fn fixture(name: &str) -> Option<Failure> {
    classify(&Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/failures/{name}")))
}

fn class(name: &str) -> Option<&'static str> {
    fixture(name).map(|f| f.class)
}

#[test]
fn claude_quota_is_read_from_a_rejected_rate_limit_with_its_reset() {
    let f = fixture("claude-usage-limit.jsonl").unwrap();
    assert_eq!(f.class, QUOTA_EXHAUSTED);
    assert_eq!((f.status, f.retry_at), (Some(429), Some(1793340000)));
    assert!(f.message.contains("hit your limit"), "{f:?}");
}

#[test]
fn claude_expired_login_is_auth_but_a_recovered_retry_is_not() {
    let f = fixture("claude-auth-expired.jsonl").unwrap();
    assert_eq!((f.class, f.status), (AUTH_INVALID, Some(401)));
    assert_eq!(class("claude-auth-recovered.jsonl"), None);
}

#[test]
fn claude_not_logged_in_is_auth() {
    // Real `claude -p` stream-json with no login: the result has no `api_error`
    // kind or status; the assistant message before it is typed
    // `authentication_failed`.
    let f = fixture("claude-not-logged-in.jsonl").unwrap();
    assert_eq!((f.class, f.status), (AUTH_INVALID, None));
    assert_eq!(f.message, "Not logged in \u{b7} Please run /login");
}

#[test]
fn an_auth_failed_message_counts_only_for_the_result_it_ended() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("log");
    let message =
        r#"{"type":"assistant","message":{"content":[]},"error":"authentication_failed"}"#;
    let result = |is_error: bool, reason: &str| {
        format!(
            r#"{{"type":"result","is_error":{is_error},"terminal_reason":"{reason}","api_error_status":null,"result":"x"}}"#
        )
    };
    let check = |lines: &[&str]| {
        std::fs::write(&p, lines.join("\n")).unwrap();
        classify(&p).map(|f| f.class)
    };
    let (failed, done) = (result(true, "api_error"), result(false, "completed"));
    assert_eq!(check(&[message, &failed]), Some(AUTH_INVALID));
    assert_eq!(check(&[message, &done]), None, "recovered");
    let ok = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
    assert_eq!(
        check(&[message, ok, &failed]),
        None,
        "a later message passed"
    );
    assert_eq!(
        check(&[message, &done, &failed]),
        None,
        "an earlier result's message"
    );
    assert_eq!(
        check(&[message, &result(true, "max_turns")]),
        None,
        "not an API error"
    );
}

#[test]
fn claude_unreachable_api_and_plain_rate_limit_are_provider_outages() {
    let f = fixture("claude-connection-failed.jsonl").unwrap();
    assert_eq!((f.class, f.status), (PROVIDER_UNAVAILABLE, None));
    assert!(f.message.contains("EAI_AGAIN"));
    // A 429 without a rejected quota window is the API's rate limit.
    let f = fixture("claude-rate-limited.jsonl").unwrap();
    assert_eq!(
        (f.class, f.status, f.retry_at),
        (PROVIDER_UNAVAILABLE, Some(429), None)
    );
}

#[test]
fn claude_other_errors_are_task_errors() {
    assert_eq!(class("claude-task-error.jsonl"), None);
}

#[test]
fn codex_classes_come_from_the_failed_turn_message() {
    let f = fixture("codex-usage-limit.jsonl").unwrap();
    assert_eq!(
        (f.class, f.status, f.retry_at),
        (QUOTA_EXHAUSTED, None, None)
    );
    assert_eq!(class("codex-auth-expired.jsonl"), Some(AUTH_INVALID));
    assert_eq!(class("codex-at-capacity.jsonl"), Some(PROVIDER_UNAVAILABLE));
    assert_eq!(class("codex-task-error.jsonl"), None);
    // An error followed by a completed turn ended nothing.
    assert_eq!(class("codex-recovered.jsonl"), None);
}

#[test]
fn codex_status_messages() {
    let dir = tempfile::tempdir().unwrap();
    let check = |msg: &str| {
        let p = dir.path().join("log");
        let line = serde_json::json!({"type":"turn.failed","error":{"message":msg}});
        std::fs::write(&p, format!("{{\"type\":\"turn.started\"}}\n{line}\n")).unwrap();
        classify(&p).map(|f| f.class)
    };
    assert_eq!(
        check("Quota exceeded. Check your plan and billing details."),
        Some(QUOTA_EXHAUSTED)
    );
    assert_eq!(
        check("unexpected status 401 Unauthorized: token expired"),
        Some(AUTH_INVALID)
    );
    for m in [
        "exceeded retry limit, last status: 429 Too Many Requests",
        "exceeded retry limit, last status: 503 Service Unavailable",
        "unexpected status 502 Bad Gateway: upstream",
        "We\u{2019}re currently experiencing high demand, which may cause temporary errors.",
    ] {
        assert_eq!(check(m), Some(PROVIDER_UNAVAILABLE), "{m}");
    }
    assert_eq!(
        check("unexpected status 400 Bad Request: invalid model"),
        None
    );
    assert_eq!(classify(&dir.path().join("missing")), None);
}

#[test]
fn the_wire_object_names_the_class_and_retry_waits_for_a_quota_reset() {
    let f = fixture("claude-usage-limit.jsonl").unwrap();
    let v = f.to_json();
    assert_eq!(v["class"], QUOTA_EXHAUSTED);
    assert!(v["message"]
        .as_str()
        .unwrap()
        .starts_with("quota_exhausted: "));
    assert_eq!(f.note(), format!("429 {}", v["message"].as_str().unwrap()));
    let now = 1793340000 - 600;
    assert_eq!(
        retry_wait(&v, 5, now),
        600,
        "until the reset, not the backoff"
    );
    assert_eq!(
        retry_wait(&v, 1, 1793340000 + 5),
        60,
        "a past reset retries soon"
    );
    let mut unknown = v.clone();
    unknown["retry_at"] = serde_json::Value::Null;
    assert_eq!(retry_wait(&unknown, 1, now), QUOTA_RETRY_SECONDS);
    let auth = fixture("codex-auth-expired.jsonl").unwrap().to_json();
    assert_eq!(retry_wait(&auth, 3, now), 240, "other classes back off");
    assert_eq!(
        fixture("codex-at-capacity.jsonl")
            .unwrap()
            .note()
            .split(':')
            .next(),
        Some(PROVIDER_UNAVAILABLE)
    );
}
