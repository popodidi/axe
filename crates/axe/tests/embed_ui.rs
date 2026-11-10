//! `embed-ui`: `axe server` serves the built dashboard next to the API.
#![cfg(feature = "embed-ui")]

mod common;

use common::Server;

fn get(srv: &Server, path: &str) -> (u16, String, String) {
    let r = match ureq::get(&format!("{}{path}", srv.url)).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{path}: {e}"),
    };
    let status = r.status();
    let mime = r.header("content-type").unwrap_or("").to_string();
    (status, mime, r.into_string().unwrap())
}

#[test]
fn server_serves_the_dashboard_at_root() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());

    let (status, mime, body) = get(&srv, "/");
    assert_eq!(status, 200);
    assert!(mime.starts_with("text/html"), "{mime}");
    assert!(body.contains("<title>axe · Tasks</title>"), "{body}");

    // Every script the page loads is embedded too.
    let script = body
        .split("src=\"")
        .skip(1)
        .map(|s| s.split('"').next().unwrap())
        .find(|s| s.starts_with("/_next/") && s.ends_with(".js"))
        .expect("a /_next/ script");
    let (status, mime, _) = get(&srv, script);
    assert_eq!((status, mime.as_str()), (200, "application/javascript"));

    // The API, health and unknown paths are unchanged.
    let (status, mime, body) = get(&srv, "/api/tasks");
    assert_eq!(
        (status, mime.as_str(), body.as_str()),
        (200, "application/json", "[]")
    );
    let (status, _, body) = get(&srv, "/healthz");
    assert_eq!((status, body.as_str()), (200, r#"{"ok":true}"#));
    let (status, mime, _) = get(&srv, "/no-such-page");
    assert_eq!((status, mime.as_str()), (404, "application/json"));
    let (status, mime, _) = get(&srv, "/api/no-such-route");
    assert_eq!((status, mime.as_str()), (404, "application/json"));
}
