mod common;

use axe::{db, metrics::Metrics};
use common::{Server, TOKEN};
use std::time::{Duration, Instant};

fn scrape(srv: &Server) -> String {
    let response = ureq::get(&format!("{}/metrics", srv.url)).call().unwrap();
    assert_eq!(
        response.header("Content-Type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    response.into_string().unwrap()
}

#[test]
fn aggregate_snapshot_is_read_only_bounded_and_survives_reset() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db::open(&dir.path().join("fixture.db")).unwrap();
    conn.execute_batch("INSERT INTO tasks (project,title,detail,state,created_at,updated_at) VALUES
        ('orchid','private-title','private-detail','pending','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z'),
        ('secret-project','hidden','secret','done','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');
        INSERT INTO runs(task_id,project,agent,mode,claim,started_at,ended_at,log_path) VALUES
        (1,'orchid','secret-agent','fresh','secret-claim','2026-01-01T00:00:00Z',NULL,'private-path'),
        (2,'secret-project','secret-agent','fresh','secret-claim','2026-01-01T00:00:00Z','2026-01-01T00:01:00Z','private-path');").unwrap();
    let changes = conn.total_changes();
    // Without `[metrics] projects`, every project is `other`.
    let unlisted = Metrics::default();
    unlisted.refresh(&conn).unwrap();
    assert!(unlisted
        .render(0)
        .contains("axe_active_runs{project=\"other\"} 1\n"));
    assert!(!unlisted.render(0).contains("orchid"));
    let m = Metrics::new(&["orchid".into()]);
    m.refresh(&conn).unwrap();
    let body = m.render(0);
    assert!(body.contains("axe_tasks{project=\"all\",state=\"pending\"} 1\n"));
    assert!(body.contains("axe_tasks{project=\"other\",state=\"done\"} 1\n"));
    assert!(body.contains("axe_active_runs{project=\"orchid\"} 1\n"));
    assert!(body.contains("axe_completed_runs{project=\"other\"} 1\n"));
    let seconds: f64 = body
        .lines()
        .find(|s| s.starts_with("axe_completed_run_duration_seconds{project=\"all\"}"))
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    assert!((seconds - 60.).abs() < 0.001);
    for secret in ["private", "secret", "hidden", "task_id", "session_id"] {
        assert!(!body.contains(secret));
    }
    assert_eq!(changes, conn.total_changes());
    // Rendering is cache-only, even if the underlying connection is unavailable.
    drop(conn);
    assert_eq!(body, m.render(0));
    let fresh = Metrics::new(&["orchid".into()]);
    let conn = db::open(&dir.path().join("fixture.db")).unwrap();
    fresh.refresh(&conn).unwrap();
    assert!(fresh
        .render(0)
        .contains("axe_completed_runs{project=\"all\"} 1\n"));
    assert!(!fresh.render(0).contains("axe_http_requests_total{"));
}

#[test]
fn real_http_metrics_auth_errors_cache_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "[metrics]\nprojects = [\"orchid\"]\n").unwrap();
    let start = || Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    let srv = start();
    let initial = scrape(&srv);
    assert!(initial.contains("axe_tasks{project=\"all\",state=\"pending\"} 0\n"));
    assert!(initial.contains("axe_pending_oldest_age_seconds{project=\"all\"} 0\n"));
    assert_eq!(srv.get("/healthz")["ok"], true);
    for n in 0..20 {
        let e = ureq::get(&format!("{}/private-{n}?token=do-not-leak", srv.url))
            .call()
            .unwrap_err();
        assert_eq!(e.into_response().unwrap().status(), 404);
    }
    let e = ureq::post(&format!("{}/api/tasks", srv.url))
        .send_json(serde_json::json!({"title":"private-title"}))
        .unwrap_err();
    assert_eq!(e.into_response().unwrap().status(), 401);
    srv.ok(&[
        "add",
        "--project",
        "orchid",
        "--title",
        "private-title",
        "--state",
        "blocked",
    ]);
    let before = srv.get("/api/tasks/1/events");
    let deadline = Instant::now() + Duration::from_secs(20);
    let body = loop {
        let body = scrape(&srv);
        if body.contains("axe_tasks{project=\"orchid\",state=\"blocked\"} 1\n") {
            break body;
        }
        assert!(Instant::now() < deadline, "snapshot did not refresh");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(body
        .contains("axe_http_requests_total{method=\"GET\",route=\"unknown\",status=\"404\"} 20\n"));
    assert!(body.contains(
        "axe_http_requests_total{method=\"POST\",route=\"/api/tasks\",status=\"401\"} 1\n"
    ));
    assert!(body.contains("axe_http_request_duration_seconds_bucket{method=\"GET\",route=\"unknown\",status=\"404\",le=\"+Inf\"} 20\n"));
    assert!(body.contains(
        "axe_http_request_duration_seconds_sum{method=\"GET\",route=\"unknown\",status=\"404\"} "
    ));
    for secret in [TOKEN, "private-title", "private-", "do-not-leak", "task_id"] {
        assert!(!body.contains(secret));
    }
    assert_eq!(before, srv.get("/api/tasks/1/events"));
    drop(srv);
    let srv = start();
    let body = scrape(&srv);
    assert!(!body.contains("route=\"unknown\""));
    assert!(body.contains("axe_tasks{project=\"orchid\",state=\"blocked\"} 1\n"));
}

#[test]
fn arbitrary_methods_and_paths_cannot_expand_labels() {
    let m = Metrics::default();
    for i in 0..1000 {
        let path = vec![format!("sensitive-{i}")];
        m.observe(
            &format!("METHOD{i}"),
            axe::metrics::route(&path),
            404,
            Instant::now(),
        );
    }
    let body = m.render(0);
    assert!(body.contains(
        "axe_http_requests_total{method=\"OTHER\",route=\"unknown\",status=\"404\"} 1000\n"
    ));
    assert!(!body.contains("sensitive"));
    assert_eq!(
        body.lines()
            .filter(|l| l.starts_with("axe_http_requests_total{"))
            .count(),
        1
    );
    assert_eq!(
        axe::metrics::route(&["api".into(), "tasks".into(), "private-id".into()]),
        "/api/tasks/:id"
    );
}

#[test]
fn bot_routes_have_bounded_labels() {
    let r = |p: &[&str]| axe::metrics::route(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(r(&["api", "bots"]), "/api/bots");
    assert_eq!(r(&["api", "bots", "ivy-cos"]), "/api/bots/:name");
    assert_eq!(
        r(&["api", "bots", "ivy-cos", "inbox"]),
        "/api/bots/:name/action"
    );
    assert_eq!(
        r(&["api", "bots", "ivy-cos", "activity"]),
        "/api/bots/:name/action"
    );
    assert_eq!(r(&["api", "bot-runs", "9", "log"]), "/api/bot-runs/:id/log");
}
