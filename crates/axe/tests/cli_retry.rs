//! The CLI rides out a server outage: transient errors are retried within a
//! bound, semantic errors are not, and a retried write is applied once.
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{server_cmd, Server, TOKEN};
use serde_json::Value;

/// A server on a fixed address, so it can stop and come back at the same URL.
fn start_at(dir: &std::path::Path, addr: &str) -> Child {
    let mut child = server_cmd(
        &dir.join("t.db"),
        &dir.join("config.toml"),
        &dir.join("token"),
        &["--listen", addr],
    )
    .current_dir(dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("axe server listening on "), "{line:?}");
    child
}

fn free_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().to_string()
}

fn notes(srv: &Server, task: &str, body: &str) -> usize {
    let events: Vec<Value> = serde_json::from_str(&srv.ok(&["log", task, "--json"])).unwrap();
    events
        .iter()
        .filter(|e| e["body"].as_str().is_some_and(|n| n.contains(body)))
        .count()
}

/// Drives the CLI against `url` with `AXE_RETRY_SECS=retry`.
fn cli(srv: &Server, url: &str, retry: &str, args: &[&str]) -> std::process::Command {
    let mut c = srv.cli(args);
    c.env("AXE_SERVER", url).env("AXE_RETRY_SECS", retry);
    c
}

#[test]
fn cli_retries_through_a_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "").unwrap();
    std::fs::write(dir.path().join("token"), format!("{TOKEN}\n")).unwrap();
    let addr = free_addr();
    let url = format!("http://{addr}");
    // `srv` supplies the CLI environment; the fixed-address server holds the db.
    let srv = Server::start(dir.path());
    let mut first = start_at(dir.path(), &addr);
    let out = cli(&srv, &url, "0", &["add", "--project", "p", "--title", "t"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let id = String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .find_map(|w| w.trim_end_matches(':').parse::<i64>().ok())
        .unwrap()
        .to_string();
    first.kill().unwrap();
    first.wait().unwrap();

    // The server is down when the note is issued and returns two seconds later.
    let note = cli(&srv, &url, "30", &["note", &id, "during-outage"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let mut second = start_at(dir.path(), &addr);
    let out = note.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("retrying"));
    let read = cli(&srv, &url, "0", &["log", &id, "--json"])
        .output()
        .unwrap();
    let events: Vec<Value> = serde_json::from_slice(&read.stdout).unwrap();
    let count = events
        .iter()
        .filter(|e| e["body"].as_str() == Some("during-outage"))
        .count();
    assert_eq!(count, 1, "{events:?}");
    second.kill().unwrap();
    second.wait().unwrap();
}

#[test]
fn cli_does_not_retry_semantic_errors() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let start = Instant::now();
    let out = cli(&srv, &srv.url, "30", &["note", "9999", "x"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not found"), "{stderr}");
    assert!(!stderr.contains("retrying"), "{stderr}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn cli_gives_up_after_the_retry_bound() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let url = format!("http://{}", free_addr());
    let start = Instant::now();
    let out = cli(&srv, &url, "3", &["note", "1", "x"]).output().unwrap();
    let elapsed = start.elapsed();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("retrying for up to 3s"), "{stderr}");
    assert!(stderr.contains("cannot reach axe server"), "{stderr}");
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(15),
        "{elapsed:?}"
    );
}

/// Reads one HTTP request (head and content-length body) as raw bytes.
fn read_request(s: &mut TcpStream) -> Vec<u8> {
    let mut r = BufReader::new(s);
    let mut raw = Vec::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        raw.extend_from_slice(line.as_bytes());
        if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
            length = n.trim().parse().unwrap();
        }
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0; length];
    r.read_exact(&mut body).unwrap();
    raw.extend_from_slice(&body);
    raw
}

fn request_id(raw: &[u8]) -> Option<String> {
    String::from_utf8_lossy(raw).lines().find_map(|l| {
        l.to_lowercase()
            .strip_prefix("x-axe-request-id:")
            .map(|v| v.trim().to_string())
    })
}

#[test]
fn write_retried_after_a_lost_reply_is_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "t"]);
    let upstream = srv.url.trim_start_matches("http://").to_string();
    // A proxy that forwards every request but drops the first reply, as when a
    // server applies a write and restarts before the client reads the answer.
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let ids = Arc::clone(&seen);
    std::thread::spawn(move || {
        for (n, conn) in proxy.incoming().enumerate() {
            let mut client = conn.unwrap();
            let raw = read_request(&mut client);
            ids.lock().unwrap().push(request_id(&raw));
            let mut up = TcpStream::connect(&upstream).unwrap();
            up.write_all(&raw).unwrap();
            let mut reply = Vec::new();
            up.read_to_end(&mut reply).unwrap();
            if n > 0 {
                client.write_all(&reply).unwrap();
            }
        }
    });
    let out = cli(&srv, &url, "30", &["note", "1", "lost-reply"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let ids = seen.lock().unwrap().clone();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids[0].is_some() && ids[0] == ids[1], "{ids:?}");
    assert_eq!(notes(&srv, "1", "lost-reply"), 1);
}

#[test]
fn server_replays_a_request_id_and_rejects_its_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    srv.ok(&["add", "--project", "p", "--title", "t"]);
    let post = |body: &str| match ureq::post(&format!("{}/api/tasks/1/notes", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .set("X-Axe-Request-Id", "fixed-id-1")
        .send_json(serde_json::json!({ "body": body }))
    {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap()),
        Err(e) => panic!("{e}"),
    };
    let first = post("once");
    let replay = post("once");
    assert!(first.0 < 300, "{first:?}");
    assert_eq!(first, replay);
    assert_eq!(notes(&srv, "1", "once"), 1);
    let reused = post("different");
    assert_eq!(reused.0, 409, "{reused:?}");
    assert_eq!(notes(&srv, "1", "different"), 0);
}
