use axe::{client::Client, config::Config, remote::Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{atomic::AtomicBool, Arc};

fn request(socket: &mut std::net::TcpStream) -> Value {
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(socket);
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(n) = line.to_lowercase().strip_prefix("content-length:") {
            length = n.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}
fn response(socket: &mut std::net::TcpStream, code: u16, body: &str) {
    write!(
        socket,
        "HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
}

#[test]
fn ingress_outage_retains_and_replays_the_same_durable_operation() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        log_dir: dir.path().join("logs").to_str().unwrap().into(),
        ..Config::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = Client::at(
        format!("http://{}", listener.local_addr().unwrap()),
        "test".into(),
    )
    .with_token("fixture");
    let journal_path = cfg.log_dir().join(".dispatcher/journal.json");
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        request(&mut socket);
        response(&mut socket, 200, r#"{"unresolved":false}"#);
        let (mut socket, _) = listener.accept().unwrap();
        let original = request(&mut socket);
        response(&mut socket, 404, "404 page not found\n");
        let (mut socket, _) = listener.accept().unwrap();
        let replay = request(&mut socket);
        assert_eq!(original, replay);
        let journal: Value = serde_json::from_slice(&std::fs::read(journal_path).unwrap()).unwrap();
        assert_eq!(journal["pending"]["operation_id"], replay["operation_id"]);
        response(&mut socket, 200, "null");
    });
    let store = Store::open_with(&cfg, client, Arc::new(AtomicBool::new(false)), None).unwrap();
    let _: Value = store
        .request("activity", json!({"run":1,"activity":"still working"}))
        .unwrap();
    assert!(store.journal.lock().unwrap().pending.is_none());
    server.join().unwrap();
}

#[test]
fn semantic_and_authentication_errors_still_fail_closed() {
    for (code, body, retry) in [
        (404, "404 page not found\n", true),
        (404, r#"{"error":"task not found"}"#, false),
        (401, "unauthorized", false),
        (409, r#"{"error":"stale dispatcher ownership"}"#, false),
        (500, "unavailable", true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Client::at(
            format!("http://{}", listener.local_addr().unwrap()),
            "test".into(),
        )
        .with_token("fixture");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            request(&mut socket);
            response(&mut socket, code, body);
        });
        let err = client
            .send::<Value>("POST", "/api/dispatcher/operation", json!({}))
            .unwrap_err();
        assert_eq!(axe::client::retryable(&err), retry, "{code}: {body}");
        server.join().unwrap();
    }
}
