//! `axe server`: the only process that opens the database. It serves a small
//! HTTP/1.1 JSON API (one request per connection) and runs the dispatcher loop
//! in the same process.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

use crate::admission;
use crate::bot;
use crate::channel;
use crate::config::{self, Config};
use crate::db::{self, Actor, Changes, Claim, Filter, NewTask, RunFilter};
use crate::remote;
use crate::routine;
use crate::schedule;
use crate::subscription;
#[cfg(feature = "telegram")]
use crate::telegram;
#[cfg(feature = "telegram")]
use crate::telegram::Gateway;

/// Stands in for the Telegram gateway in a build without the `telegram`
/// feature: it has no values, so `State::telegram` is always `None`.
#[cfg(not(feature = "telegram"))]
enum Gateway {}

#[cfg(not(feature = "telegram"))]
impl Gateway {
    fn names(&self) -> Vec<String> {
        match *self {}
    }
}

#[cfg(not(feature = "telegram"))]
const NO_TELEGRAM: &str = "this server was built without the telegram feature";

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8790";
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// `bot tg send-file` bodies carry the raw file (Telegram's 50 MB upload cap).
const MAX_UPLOAD_BYTES: usize = channel::UPLOAD_LIMIT as usize + 1024 * 1024;
const MAX_CONNECTIONS: usize = 256;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const KEEPALIVE: Duration = Duration::from_secs(15);
const DEFAULT_RUNS_LIMIT: i64 = 50;
const MAX_RUNS_LIMIT: i64 = 1000;

pub struct Options {
    pub listen: String,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub token_path: PathBuf,
}

/// Change counter bumped on every committed database change; SSE streams wait on it.
#[derive(Default)]
struct Notifier {
    seq: Mutex<u64>,
    cv: Condvar,
}

impl Notifier {
    fn current(&self) -> u64 {
        *self.seq.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        *self.seq.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.cv.notify_all();
    }

    /// Waits until the counter moves past `seen` or `timeout` elapses.
    fn wait_past(&self, seen: u64, timeout: Duration) -> u64 {
        let guard = self.seq.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = self
            .cv
            .wait_timeout_while(guard, timeout, |s| *s == seen)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }
}

struct State {
    metrics: crate::metrics::Metrics,
    cfg: Config,
    config_path: PathBuf,
    db_path: PathBuf,
    conn: Mutex<Connection>,
    token: Option<String>,
    notifier: Notifier,
    stop: Arc<AtomicBool>,
    connections: AtomicUsize,
    /// When `cli:` replay rows were last pruned (epoch seconds).
    last_replay_prune: std::sync::atomic::AtomicI64,
    relay: remote::Relay,
    /// The dispatcher's last admission report and when it arrived.
    admission: Mutex<Option<(serde_json::Value, Instant)>>,
    /// Telegram gateway, when `[telegram]` is configured.
    telegram: Option<Arc<Gateway>>,
}

impl channel::Store for State {
    fn conn(&self) -> MutexGuard<'_, Connection> {
        State::conn(self)
    }
    fn changed(&self) {
        self.notifier.bump();
    }
    fn seq(&self) -> u64 {
        self.notifier.current()
    }
    fn wait_changed(&self, seen: u64, timeout: Duration) -> u64 {
        self.notifier.wait_past(seen, timeout)
    }
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

impl State {
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn admission(&self) -> serde_json::Value {
        let report = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        admission::reported(report.as_ref())
    }
}

/// Binds the API, then runs the dispatcher on the calling thread until `stop`
/// (or, with `once`, until the single pass and its sessions finish).
pub fn run(opts: Options, cfg: Config, stop: Arc<AtomicBool>) -> Result<()> {
    let token = config::read_token(&opts.token_path)?;
    let addrs: Vec<SocketAddr> = opts
        .listen
        .to_socket_addrs()
        .with_context(|| format!("resolve listen address {}", opts.listen))?
        .collect();
    if addrs.is_empty() {
        bail!("listen address {} resolved to nothing", opts.listen);
    }
    if token.is_none() && !addrs.iter().all(|a| a.ip().is_loopback()) {
        bail!(
            "refusing to listen on non-loopback {} without a token; create {} (0600, e.g. `openssl rand -hex 32`)",
            opts.listen,
            opts.token_path.display()
        );
    }
    let api_conn = db::open(&opts.db_path)?;
    let listener =
        TcpListener::bind(&addrs[..]).with_context(|| format!("bind {}", opts.listen))?;
    let local = listener.local_addr()?;
    println!("axe server listening on http://{local}");
    std::io::stdout().flush()?;
    if token.is_none() {
        eprintln!(
            "axe server: no token at {}; writes are unauthenticated (loopback only)",
            opts.token_path.display()
        );
    }

    let state = Arc::new(State {
        metrics: crate::metrics::Metrics::new(&cfg.metrics.projects),
        cfg: cfg.clone(),
        config_path: opts.config_path,
        db_path: opts.db_path,
        conn: Mutex::new(api_conn),
        token,
        notifier: Notifier::default(),
        stop: Arc::clone(&stop),
        connections: AtomicUsize::new(0),
        last_replay_prune: std::sync::atomic::AtomicI64::new(0),
        relay: Mutex::new(None),
        admission: Mutex::new(None),
        #[cfg(feature = "telegram")]
        telegram: cfg.telegram.as_ref().map(|t| Arc::new(Gateway::new(t))),
        #[cfg(not(feature = "telegram"))]
        telegram: None,
    });
    #[cfg(feature = "telegram")]
    if let Some(gw) = &state.telegram {
        eprintln!("axe server: telegram gateway for bots {:?}", gw.names());
        gw.start(Arc::clone(&state) as Arc<dyn channel::Store>);
    }
    #[cfg(not(feature = "telegram"))]
    if cfg.telegram.is_some() {
        eprintln!("axe server: [telegram] is configured but {NO_TELEGRAM}; ignoring it");
    }
    state.metrics.refresh(&state.conn())?;
    listener.set_nonblocking(true)?;
    let mut last_metrics = Instant::now();
    let mut last_tick = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        if last_tick.elapsed() >= Duration::from_secs(1) {
            match routine::tick(&mut state.conn(), routine::now()) {
                Ok(n) if n > 0 => state.notifier.bump(),
                Err(e) => eprintln!("routine scheduler: {e:#}"),
                _ => {}
            }
            match schedule::tick(&mut state.conn(), routine::now()) {
                Ok(n) if n > 0 => state.notifier.bump(),
                Err(e) => eprintln!("bot schedule tick: {e:#}"),
                _ => {}
            }
            last_tick = Instant::now();
        }
        if last_metrics.elapsed() >= Duration::from_secs(15) {
            if let Err(e) = state.metrics.refresh(&state.conn()) {
                eprintln!("metrics snapshot: {e:#}");
            }
            last_metrics = Instant::now();
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if state.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    state.connections.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let st = Arc::clone(&state);
                std::thread::spawn(move || {
                    let _ = handle(&st, stream);
                    st.connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

struct Request {
    method: String,
    path: Vec<String>,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn query(&self, key: &str) -> Option<&str> {
        self.query
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn flag(&self, key: &str) -> bool {
        matches!(self.query(key), Some("1" | "true" | "yes"))
    }

    fn json<T: DeserializeOwned>(&self) -> Result<T> {
        let body: &[u8] = if self.body.is_empty() {
            b"{}"
        } else {
            &self.body
        };
        serde_json::from_slice(body)
            .map_err(|e| HttpError(400, format!("invalid JSON body: {e}")).into())
    }
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

/// An error carrying its HTTP status.
#[derive(Debug)]
struct HttpError(u16, String);

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for HttpError {}

fn ok(status: u16, v: &impl serde::Serialize) -> Result<Response> {
    Ok(Response {
        status,
        body: serde_json::to_vec(v)?,
    })
}

fn error(status: u16, msg: &str) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&json!({ "error": msg })).unwrap_or_default(),
    }
}

fn error_response(e: &anyhow::Error) -> Response {
    let msg = format!("{e:#}");
    let status = if e.to_string() == "dispatcher lease expired" {
        503
    } else if let Some(h) = e.downcast_ref::<HttpError>() {
        h.0
    } else if e.downcast_ref::<db::Forbidden>().is_some() {
        403
    } else if e.downcast_ref::<rusqlite::Error>().is_some() {
        500
    } else if e.to_string().ends_with("not found") {
        404
    } else {
        400
    };
    error(status, &msg)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

pub(crate) fn pct_decode(s: &str, plus_is_space: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' if plus_is_space => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads one request; `Ok(None)` when the peer closed without sending one.
/// Reads one request; `token` decides whether a large upload body is allowed.
fn read_request(stream: &TcpStream, token: Option<&str>) -> Result<Option<Request>, HttpError> {
    let bad = |m: &str| HttpError(400, m.to_string());
    let mut reader = BufReader::new(stream);
    let mut head = 0usize;
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let limit = (MAX_HEADER_BYTES - head) as u64;
        let n = (&mut reader)
            .take(limit)
            .read_line(&mut line)
            .map_err(|_| bad("unreadable request"))?;
        if n == 0 {
            if lines.is_empty() {
                return Ok(None);
            }
            return Err(bad("truncated request"));
        }
        head += n;
        if !line.ends_with('\n') {
            return Err(HttpError(431, "request head too large".into()));
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let mut parts = lines[0].split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(bad("malformed request line"));
    };
    let mut headers = HashMap::new();
    for h in &lines[1..] {
        let (k, v) = h.split_once(':').ok_or_else(|| bad("malformed header"))?;
        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (pct_decode(k, true), pct_decode(v, true))
        })
        .collect();
    let path: Vec<String> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| pct_decode(s, false))
        .collect();
    let mut body = Vec::new();
    if let Some(len) = headers.get("content-length") {
        let len: usize = len.parse().map_err(|_| bad("invalid content-length"))?;
        if len > body_limit(method, &path, bearer_ok(&headers, token)) {
            return Err(HttpError(413, "request body too large".into()));
        }
        body.resize(len, 0);
        reader
            .read_exact(&mut body)
            .map_err(|_| bad("truncated body"))?;
    } else if headers.contains_key("transfer-encoding") {
        return Err(HttpError(411, "content-length required".into()));
    }
    Ok(Some(Request {
        method: method.to_string(),
        path,
        query,
        headers,
        body,
    }))
}

/// Body size limit of a request: only authorized uploads may be large.
fn body_limit(method: &str, path: &[String], authorized: bool) -> usize {
    let p: Vec<&str> = path.iter().map(String::as_str).collect();
    match (method, p.as_slice()) {
        ("POST", ["api", "bots", _, "tg", "send-file"]) if authorized => MAX_UPLOAD_BYTES,
        _ => MAX_BODY_BYTES,
    }
}

/// GETs of message content (`/api/bots/<name>/tg/...`) need the token, unlike
/// other reads.
fn private_read(path: &[String]) -> bool {
    path.len() >= 4 && path[0] == "api" && path[1] == "bots" && path[3] == "tg"
}

fn write_head(
    w: &mut TcpStream,
    status: u16,
    content_type: &str,
    length: Option<usize>,
    cors: Option<&str>,
    extra: &str,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        reason(status)
    );
    if let Some(len) = length {
        head.push_str(&format!("Content-Length: {len}\r\n"));
    }
    if let Some(origin) = cors {
        head.push_str(&format!(
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        ));
    }
    head.push_str(extra);
    head.push_str("\r\n");
    w.write_all(head.as_bytes())
}

fn write_response(w: &mut TcpStream, r: &Response, cors: Option<&str>) -> std::io::Result<()> {
    write_head(
        w,
        r.status,
        "application/json",
        Some(r.body.len()),
        cors,
        "",
    )?;
    w.write_all(&r.body)?;
    w.flush()
}

fn authorized(st: &State, req: &Request) -> bool {
    bearer_ok(&req.headers, st.token.as_deref())
}

/// The request carries the server token (always true without a token).
fn bearer_ok(headers: &HashMap<String, String>, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
    let given = headers
        .get("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    // Constant-time comparison.
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn handle(st: &State, stream: TcpStream) -> Result<()> {
    let start = Instant::now();
    // Accepted sockets inherit the listener's non-blocking flag on BSD/macOS.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut w = stream.try_clone()?;
    let req = match read_request(&stream, st.token.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return Ok(()),
        Err(HttpError(status, msg)) => {
            st.metrics.observe("OTHER", "unknown", status, start);
            return Ok(write_response(&mut w, &error(status, &msg), None)?);
        }
    };
    let metric_route = crate::metrics::route(&req.path);
    let observe = |status| st.metrics.observe(&req.method, metric_route, status, start);
    let cors = req
        .headers
        .get("origin")
        .filter(|o| st.cfg.server.cors_origins.contains(o))
        .map(String::as_str);
    if req.method == "OPTIONS" {
        observe(204);
        // CORS is GET-only: preflights never allow writes from browsers.
        write_head(
            &mut w,
            204,
            "text/plain",
            Some(0),
            cors,
            if cors.is_some() {
                "Access-Control-Allow-Methods: GET\r\nAccess-Control-Max-Age: 600\r\n"
            } else {
                ""
            },
        )?;
        return Ok(w.flush()?);
    }
    if req.path == ["api", "dispatcher", "connect"] {
        if !authorized(st, &req) {
            observe(401);
            return Ok(write_response(&mut w, &error(401, "unauthorized"), None)?);
        }
        let r = connect_dispatcher(st, stream, &req, start);
        if let Err(e) = &r {
            eprintln!("axe server: dispatcher connection ended: {e:#}");
        }
        return r;
    }
    if req.path == ["api", "backup"] && req.method == "POST" {
        if !authorized(st, &req) {
            observe(401);
            return Ok(write_response(&mut w, &error(401, "unauthorized"), None)?);
        }
        let path = std::env::temp_dir().join(format!("axe-backup-{}.db", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            db::backup(&st.conn(), &path)?;
            let mut file = std::fs::File::open(&path)?;
            observe(200);
            write_head(
                &mut w,
                200,
                "application/octet-stream",
                Some(file.metadata()?.len() as usize),
                None,
                "",
            )?;
            std::io::copy(&mut file, &mut w)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(path);
        return result;
    }
    let read = req.method == "GET";
    if (!read || private_read(&req.path)) && !authorized(st, &req) {
        observe(401);
        return Ok(write_response(&mut w, &error(401, "unauthorized"), None)?);
    }
    if read && req.path.len() >= 6 && private_read(&req.path) && req.path[4] == "file" {
        return tg_file(st, &mut w, &req, &observe);
    }
    if read && req.path == ["metrics"] {
        let body = st.metrics.render(st.connections.load(Ordering::SeqCst));
        observe(200);
        write_head(
            &mut w,
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            Some(body.len()),
            cors,
            "",
        )?;
        w.write_all(body.as_bytes())?;
        return Ok(w.flush()?);
    }
    if read && req.path == ["api", "stream"] {
        observe(200);
        return Ok(stream_events(st, &mut w, cors)?);
    }
    let request_id = req
        .headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| req.headers.get(crate::compat::OLD_REQUEST_ID_HEADER))
        .filter(|_| !read);
    let resp = match request_id {
        Some(id) => replayable(st, &req, id),
        None => route(st, &req).unwrap_or_else(|e| error_response(&e)),
    };
    observe(resp.status);
    if !read && resp.status < 300 {
        st.notifier.bump();
    }
    Ok(write_response(
        &mut w,
        &resp,
        if read { cors } else { None },
    )?)
}

/// Header a retrying client sets on writes; the same id on every attempt.
const REQUEST_ID_HEADER: &str = "x-axe-request-id";
/// How long a request id is remembered. Clients retry for about two minutes.
const REPLAY_WINDOW_SECS: i64 = 3600;

/// FNV-1a: a hash that stays stable across server builds, so a retry that
/// spans a server upgrade still matches its original request.
fn fnv1a(parts: &[&[u8]]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.iter().chain(&[0xff]) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// Runs a write at most once per client request id. The id is reserved in
/// `dispatch_ops` (the dispatcher's replay table, rows keyed `cli:<id>`) before
/// the write runs and the response is stored after it, so a retry after a lost
/// reply returns the original response instead of applying the write again.
/// A retry while the reservation has no response (still running, or the server
/// stopped mid-write) gets a 503 and never re-runs the write.
fn replayable(st: &State, req: &Request, id: &str) -> Response {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return error(400, "invalid request id");
    }
    let key = format!("cli:{id}");
    let mut query: Vec<_> = req.query.iter().collect();
    query.sort();
    let query = serde_json::to_vec(&query).unwrap_or_default();
    let path = req.path.join("/");
    let fingerprint = fnv1a(&[req.method.as_bytes(), path.as_bytes(), &query, &req.body]);
    let reserved = (|| -> Result<Option<Response>> {
        let conn = st.conn();
        let now = remote::epoch();
        let last = st.last_replay_prune.load(Ordering::SeqCst);
        if now - last >= 60
            && st
                .last_replay_prune
                .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            conn.execute(
                "DELETE FROM dispatch_ops WHERE dispatcher='cli' \
                 AND CAST(json_extract(request,'$.at') AS INTEGER) < ?1",
                [now - REPLAY_WINDOW_SECS],
            )?;
        }
        let prior: Option<(String, String)> = conn
            .query_row(
                "SELECT request,response FROM dispatch_ops WHERE id=?1 AND dispatcher='cli'",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((request, response)) = prior else {
            conn.execute(
                "INSERT INTO dispatch_ops VALUES(?1,'cli',?2,'')",
                rusqlite::params![
                    key,
                    json!({"at": now, "fingerprint": fingerprint}).to_string()
                ],
            )?;
            return Ok(None);
        };
        let request: serde_json::Value = serde_json::from_str(&request)?;
        if request["fingerprint"] != fingerprint.as_str() {
            return Ok(Some(error(
                409,
                "request id reused with a different request",
            )));
        }
        if response.is_empty() {
            return Ok(Some(error(
                503,
                &format!(
                    "request {id} is still running or was interrupted by a server restart; \
                     its outcome is unknown"
                ),
            )));
        }
        let stored: serde_json::Value = serde_json::from_str(&response)?;
        let status = stored["status"].as_u64().unwrap_or(500) as u16;
        let body = stored["body"].as_str().unwrap_or_default();
        if status >= 500 {
            // A replayed server error is final: re-running it could repeat a
            // side effect, and retrying the replay would only wait out the bound.
            let msg = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or_else(|| format!("HTTP {status}"));
            return Ok(Some(error(
                409,
                &format!("request {id} already failed: {msg}"),
            )));
        }
        Ok(Some(Response {
            status,
            body: body.as_bytes().to_vec(),
        }))
    })();
    match reserved {
        Ok(Some(replay)) => return replay,
        Ok(None) => {}
        Err(e) => return error_response(&e),
    }
    let resp = route(st, req).unwrap_or_else(|e| error_response(&e));
    let stored = json!({
        "status": resp.status,
        "body": String::from_utf8_lossy(&resp.body),
    });
    if let Err(e) = st.conn().execute(
        "UPDATE dispatch_ops SET response=?2 WHERE id=?1",
        rusqlite::params![key, stored.to_string()],
    ) {
        eprintln!("axe server: cannot record response of request {id}: {e:#}");
    }
    resp
}

/// `GET /api/bots/<name>/tg/file/<msg>[/<n>]`: streams an inbound attachment
/// through the server (nothing is stored). The file name is in
/// `X-Axe-File-Name`, percent-encoded.
#[cfg(not(feature = "telegram"))]
fn tg_file(_: &State, w: &mut TcpStream, _: &Request, observe: &dyn Fn(u16)) -> Result<()> {
    let r = error_response(&HttpError(409, NO_TELEGRAM.into()).into());
    observe(r.status);
    Ok(write_response(w, &r, None)?)
}

#[cfg(feature = "telegram")]
fn tg_file(st: &State, w: &mut TcpStream, req: &Request, observe: &dyn Fn(u16)) -> Result<()> {
    let result = (|| -> Result<(String, String, Vec<u8>)> {
        let gw = st
            .telegram
            .as_ref()
            .ok_or_else(|| HttpError(409, "telegram is not configured".into()))?;
        if req.path.len() > 7 {
            return Err(HttpError(404, "no such file route".into()).into());
        }
        let msg = task_id(&req.path[5])?;
        let n = req
            .path
            .get(6)
            .map(|n| n.parse::<usize>())
            .transpose()
            .map_err(|_| HttpError(400, "invalid attachment index".into()))?;
        gw.file(st, &req.path[2], msg, n)
    })();
    match result {
        Ok((name, mime, bytes)) => {
            observe(200);
            let mime = if mime
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "/+.-".contains(c))
            {
                mime
            } else {
                "application/octet-stream".into()
            };
            write_head(w, 200, &mime, Some(bytes.len()), None, &{
                let name = channel::pct_encode(&name);
                format!(
                    "X-Axe-File-Name: {name}\r\n{}: {name}\r\n",
                    crate::compat::OLD_FILE_NAME_HEADER
                )
            })?;
            w.write_all(&bytes)?;
            Ok(w.flush()?)
        }
        Err(e) => {
            let r = error_response(&e);
            observe(r.status);
            Ok(write_response(w, &r, None)?)
        }
    }
}

/// Server-sent events: a `change` event (data `{"seq":N}`) on every committed
/// task, event, or run change; clients refetch what they display.
fn stream_events(st: &State, w: &mut TcpStream, cors: Option<&str>) -> std::io::Result<()> {
    write_head(w, 200, "text/event-stream", None, cors, "")?;
    let mut seen = st.notifier.current();
    write!(
        w,
        "retry: 3000\n\nevent: change\ndata: {{\"seq\":{seen}}}\n\n"
    )?;
    w.flush()?;
    while !st.stop.load(Ordering::SeqCst) {
        let seq = st.notifier.wait_past(seen, KEEPALIVE);
        if seq != seen {
            seen = seq;
            write!(w, "event: change\ndata: {{\"seq\":{seen}}}\n\n")?;
        } else {
            w.write_all(b": keepalive\n\n")?;
        }
        w.flush()?;
    }
    Ok(())
}

/// Actor fields every write carries.
#[derive(Deserialize)]
struct Who {
    actor: Option<String>,
    session: Option<String>,
}

impl Who {
    fn actor(self) -> Actor {
        Actor {
            name: self
                .actor
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| "human".into()),
            session: self.session.filter(|s| !s.is_empty()),
        }
    }
}

#[derive(Deserialize)]
struct AddBody {
    agent: Option<String>,
    project: String,
    title: String,
    #[serde(default)]
    detail: String,
    state: Option<String>,
    /// Prerequisite task ids, created atomically with the task.
    #[serde(default)]
    blocked_by: Vec<i64>,
    workload: Option<String>,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct DepsBody {
    #[serde(default)]
    add: Vec<i64>,
    #[serde(default)]
    remove: Vec<i64>,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct PatchBody {
    #[serde(default)]
    reset_runs: bool,
    state: Option<String>,
    title: Option<String>,
    detail: Option<String>,
    note: Option<String>,
    not_before: Option<String>,
    escalation_class: Option<String>,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct ClassifyBody {
    escalation: Option<String>,
    episode: Option<i64>,
    workload: Option<String>,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct ProtectBody {
    #[serde(default)]
    clear: bool,
    note: Option<String>,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct NoteBody {
    body: String,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct ArtifactBody {
    value: String,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct ApproveBody {
    sha: String,
    scope: String,
    note: Option<String>,
    #[serde(flatten)]
    who: Who,
}

#[derive(Deserialize)]
struct ClaimBody {
    project: Option<String>,
    by: String,
    #[serde(flatten)]
    who: Who,
}

fn task_id(s: &str) -> Result<i64> {
    s.parse()
        .map_err(|_| HttpError(400, format!("invalid task id {s:?}")).into())
}

fn route(st: &State, req: &Request) -> Result<Response> {
    let path: Vec<&str> = req.path.iter().map(String::as_str).collect();
    if matches!(path.get(1), Some(&("bots" | "bot-runs"))) && path[0] == "api" {
        if let Some(r) = bot_route(st, req, &path)? {
            return Ok(r);
        }
    }
    match (req.method.as_str(), path.as_slice()) {
        ("GET", ["api", "routines"]) => ok(200, &routine::list(&st.conn())?),
        ("GET", ["api", "routines", id]) => ok(200, &routine::get(&st.conn(), task_id(id)?)?),
        ("GET", ["api", "routines", id, kind @ ("history" | "events")]) => {
            let before = req
                .query("before")
                .map(str::parse)
                .transpose()?
                .unwrap_or(i64::MAX);
            let conn = st.conn();
            let rows = if *kind == "history" {
                routine::history(&conn, task_id(id)?, before)?
            } else {
                routine::events(&conn, task_id(id)?, before)?
            };
            ok(200, &rows)
        }
        ("GET", ["api", "tasks", id, "routine"]) => {
            ok(200, &routine::backlink(&st.conn(), task_id(id)?)?)
        }
        ("POST", ["api", "routines"]) | ("PUT", ["api", "routines", _]) => {
            #[derive(Deserialize)]
            struct Save {
                template: routine::Template,
                #[serde(flatten)]
                who: Who,
            }
            let b: Save = req.json()?;
            st.cfg.validate_agent(b.template.agent.as_deref())?;
            let id = path.get(2).map(|id| task_id(id)).transpose()?;
            let mut conn = st.conn();
            let id = routine::save(&mut conn, &b.who.actor(), id, &b.template, routine::now())?;
            ok(200, &routine::get(&conn, id)?)
        }
        (
            "POST",
            ["api", "routines", id, action @ ("pause" | "resume" | "archive" | "trigger")],
        ) => {
            #[derive(Deserialize)]
            struct Action {
                key: Option<String>,
                #[serde(flatten)]
                who: Who,
            }
            let b: Action = req.json()?;
            let mut conn = st.conn();
            let id = task_id(id)?;
            if *action == "trigger" {
                let key = b.key.context("manual trigger requires a retry key")?;
                ok(
                    200,
                    &json!({"task_id":routine::trigger(&mut conn,&b.who.actor(),id,&key)?}),
                )
            } else {
                routine::change(&mut conn, &b.who.actor(), id, action, routine::now())?;
                ok(200, &routine::get(&conn, id)?)
            }
        }
        ("GET", ["healthz"]) => ok(200, &json!({ "ok": true })),
        ("GET", ["api", "tasks"]) => {
            let state = req.query("state");
            if let Some(s) = state {
                db::validate_state(s)?;
            }
            let tasks = db::list_tasks(
                &st.conn(),
                &Filter {
                    state,
                    project: req.query("project"),
                    include_done: req.flag("all"),
                },
            )?;
            ok(200, &tasks)
        }
        ("GET", ["api", "tasks", id]) => ok(200, &db::get_task(&st.conn(), task_id(id)?)?),
        ("GET", ["api", "tasks", id, "events"]) => {
            let conn = st.conn();
            let id = task_id(id)?;
            db::get_task(&conn, id)?;
            ok(200, &db::events(&conn, id)?)
        }
        ("GET", ["api", "stats", "history"]) => ok(200, &db::stats_history(&st.conn())?),
        ("GET", ["api", "stats", "cost"]) => {
            let now = crate::routine::now();
            let bound = |name, default: &str| {
                let s = req.query(name).unwrap_or(default);
                crate::flow::parse_bound(s, now).map_err(|e| HttpError(400, format!("{e:#}")))
            };
            let (since, until) = (bound("since", "30d")?, bound("until", "0s")?);
            let step_days = match req.query("step_days") {
                None => None,
                Some(s) => {
                    let days = s
                        .parse::<i64>()
                        .ok()
                        .filter(|d| {
                            *d > 0
                                && (until - since).max(0) / (d * 86_400)
                                    < crate::usage::MAX_SERIES_BUCKETS
                        })
                        .ok_or_else(|| HttpError(400, format!("invalid step_days: {s}")))?;
                    Some(days)
                }
            };
            let breakdown = req.query("breakdown");
            if let Some(b) = breakdown.filter(|b| !crate::usage::BREAKDOWNS.contains(b)) {
                return Err(HttpError(400, format!("invalid breakdown: {b}")).into());
            }
            let query = crate::usage::CostQuery {
                project: req.query("project"),
                source: req.query("source"),
                bot: req.query("bot"),
                routine: req.query("routine"),
                agent: req.query("agent"),
                workload: req.query("workload"),
                model: req.query("model"),
                breakdown,
                options: req.flag("options"),
            };
            ok(
                200,
                &crate::usage::cost_stats(&st.conn(), since, until, step_days, &query)?,
            )
        }
        ("GET", ["api", "usage", "catalog"]) => ok(200, &crate::usage::catalog(&st.conn())?),
        ("POST", ["api", "usage", "ingest"]) => {
            #[derive(serde::Deserialize)]
            struct Batch {
                items: Vec<crate::usage::IngestItem>,
            }
            let batch: Batch = req.json()?;
            let changed = crate::usage::ingest(&mut st.conn(), &batch.items)
                .map_err(|e| HttpError(400, format!("{e:#}")))?;
            ok(200, &serde_json::json!({ "changed": changed }))
        }
        ("GET", ["api", "stats", "flow"]) => {
            let now = crate::routine::now();
            let project = req
                .query("project")
                .ok_or_else(|| HttpError(400, "project is required".into()))?;
            let bound = |name, default: &str| {
                let s = req.query(name).unwrap_or(default);
                crate::flow::parse_bound(s, now).map_err(|e| HttpError(400, format!("{e:#}")))
            };
            let q = crate::flow::Query {
                project,
                since: bound("since", "14d")?,
                until: bound("until", "0s")?,
                workload: req.query("workload"),
            };
            ok(200, &crate::flow::stats(&st.conn(), &q)?)
        }
        ("GET", ["api", "runs"]) => {
            let task = req.query("task").map(task_id).transpose()?;
            let limit = match req.query("limit") {
                Some(l) => l
                    .parse::<i64>()
                    .map_err(|_| HttpError(400, format!("invalid limit {l:?}")))?,
                None => DEFAULT_RUNS_LIMIT,
            };
            let f = RunFilter {
                task,
                live: req.flag("live"),
                limit: limit.clamp(1, MAX_RUNS_LIMIT),
            };
            ok(200, &db::list_runs(&st.conn(), &f)?)
        }
        ("GET", ["api", "runs", id, "log"]) => {
            log_cursor(req)?;
            let (run, reused) = {
                let conn = st.conn();
                let run = db::get_run(&conn, task_id(id)?)?;
                let reused = db::run_log_reused(&conn, &run)?;
                (run, reused)
            };
            if reused {
                return ok(
                    200,
                    &json!({"run": run, "unavailable": "Historical log path was reused by another attempt"}),
                );
            }
            let owner: Option<String> = st
                .conn()
                .query_row(
                    "SELECT dispatcher FROM dispatch_runs WHERE run_id=?1",
                    [run.id],
                    |r| r.get(0),
                )
                .optional()?;
            relay_log(
                st,
                req,
                owner,
                json!({"kind":"log","run":run.id}),
                serde_json::to_value(run)?,
            )
        }

        ("POST", ["api", "tasks", id, "abort"]) => {
            let b: serde_json::Value = req.json()?;
            let who: Who = req.json()?;
            let run = b["run"].as_i64().context("missing run")?;
            let result = remote::abort(&st.conn(), id.parse()?, run, &who.actor())?;
            ok(
                if result["status"] == "confirmed" {
                    200
                } else {
                    202
                },
                &result,
            )
        }

        ("POST", ["api", "dispatcher", "recovery"]) => {
            let b: serde_json::Value = req.json()?;
            let unresolved:bool=st.conn().query_row("SELECT EXISTS(SELECT 1 FROM runs r JOIN dispatch_runs d ON r.id=d.run_id WHERE r.ended_at IS NULL AND d.dispatcher=?1) OR EXISTS(SELECT 1 FROM tasks t JOIN dispatch_claims c ON t.claimed_by=c.claim WHERE t.state='in_progress' AND c.dispatcher=?1) OR EXISTS(SELECT 1 FROM bot_runs WHERE ended_at IS NULL AND dispatcher=?1)",[remote::field(&b,"id")?],|r|r.get(0))?;
            ok(200, &json!({"unresolved":unresolved}))
        }
        ("GET", ["api", "dispatcher"]) => {
            let owner: Option<(String, i64)> = st
                .conn()
                .query_row(
                    "SELECT id,expires FROM dispatcher WHERE singleton=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            ok(
                200,
                &json!({"owner":owner,"connected":st.relay.lock().unwrap().is_some()}),
            )
        }
        ("POST", ["api", "dispatcher", "log-catalog"]) => {
            let b: serde_json::Value = req.json()?;
            let conn = st.conn();
            remote::heartbeat(
                &conn,
                remote::field(&b, "id")?,
                remote::field(&b, "generation")?,
            )?;
            let runs = db::list_runs(
                &conn,
                &RunFilter {
                    limit: i64::MAX,
                    ..Default::default()
                },
            )?;
            let mut result = Vec::new();
            for run in runs {
                let owner: Option<String> = conn
                    .query_row(
                        "SELECT dispatcher FROM dispatch_runs WHERE run_id=?1",
                        [run.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if run.ended_at.is_some()
                    && owner
                        .as_deref()
                        .is_none_or(|id| Some(id) == b["id"].as_str())
                {
                    result.push(run);
                }
            }
            ok(200, &result)
        }
        ("POST", ["api", "dispatcher", "admission"]) => {
            let b: serde_json::Value = req.json()?;
            remote::heartbeat(
                &st.conn(),
                remote::field(&b, "id")?,
                remote::field(&b, "generation")?,
            )?;
            *st.admission.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((b["admission"].clone(), Instant::now()));
            ok(200, &json!({"ok":true}))
        }
        ("POST", ["api", "dispatcher", "adopt-logs"]) => {
            let b: serde_json::Value = req.json()?;
            let conn = st.conn();
            let id = remote::field(&b, "id")?;
            let generation = remote::field(&b, "generation")?;
            remote::heartbeat(&conn, id, generation)?;
            let ids: Vec<i64> = serde_json::from_value(b["runs"].clone())?;
            let tx = conn.unchecked_transaction()?;
            for run in ids {
                tx.execute("INSERT OR IGNORE INTO dispatch_runs SELECT id,?2,?3,'finished' FROM runs WHERE id=?1 AND ended_at IS NOT NULL",rusqlite::params![run,id,generation])?;
            }
            tx.commit()?;
            ok(200, &json!({"ok":true}))
        }
        ("POST", ["api", "dispatcher", "reconcile"]) => ok(
            200,
            &remote::reconcile(&st.conn(), &req.json::<serde_json::Value>()?)?,
        ),
        ("POST", ["api", "dispatcher", "register"]) => ok(
            200,
            &remote::register(&st.conn(), &req.json::<serde_json::Value>()?)?,
        ),
        ("POST", ["api", "dispatcher", "operation"]) => ok(
            200,
            &remote::operation(&st.conn(), &req.json::<serde_json::Value>()?)?,
        ),
        ("GET", ["api", "overview"]) => {
            let mut v = serde_json::to_value(db::overview(&st.conn())?)?;
            v["admission"] = st.admission();
            v["bots"] = json!(bot::list(&st.conn())?
                .iter()
                .map(bot::summary)
                .collect::<Vec<_>>());
            ok(200, &v)
        }
        ("GET", ["api", "admission"]) => ok(200, &st.admission()),
        ("GET", ["api", "sessions", sid]) => match db::find_session(&st.conn(), sid)? {
            Some(s) => ok(200, &s),
            None => Err(HttpError(404, format!("session {sid} not found")).into()),
        },
        ("GET", ["api", "config"]) => {
            // Telegram ids and token paths are not public.
            let mut config = serde_json::to_value(&st.cfg)?;
            if let Some(t) = config.get_mut("telegram") {
                *t = json!({"bots": st.telegram.as_ref().map(|g| g.names())});
            }
            ok(
                200,
                &json!({
                    "config_path": st.config_path.display().to_string(),
                    "db_path": st.db_path.display().to_string(),
                    "auth": st.token.is_some(),
                    "config": config,
                }),
            )
        }
        ("POST", ["api", "tasks"]) => {
            let b: AddBody = req.json()?;
            st.cfg.validate_agent(b.agent.as_deref())?;
            let mut conn = st.conn();
            let id = db::add_classified_task(
                &mut conn,
                &b.who.actor(),
                &NewTask {
                    agent: b.agent.as_deref(),
                    project: &b.project,
                    title: &b.title,
                    detail: &b.detail,
                    state: b.state.as_deref().unwrap_or("pending"),
                },
                &b.blocked_by,
                b.workload.as_deref(),
            )?;
            ok(201, &json!({ "id": id, "task": db::get_task(&conn, id)? }))
        }
        ("PATCH", ["api", "tasks", id]) => {
            let id = task_id(id)?;
            let b: PatchBody = req.json()?;
            let c = Changes {
                reset_runs: b.reset_runs,
                state: b.state,
                title: b.title,
                detail: b.detail,
                note: b.note,
                not_before: b.not_before,
                escalation_class: b.escalation_class,
            };
            let mut conn = st.conn();
            let changed = db::set_task(&mut conn, &b.who.actor(), id, &c)?;
            ok(
                200,
                &json!({ "changed": changed, "task": db::get_task(&conn, id)? }),
            )
        }
        ("POST", ["api", "tasks", id, kind @ ("notes" | "artifacts")]) => {
            let id = task_id(id)?;
            let (kind, body, who) = if *kind == "notes" {
                let b: NoteBody = req.json()?;
                ("note", b.body, b.who)
            } else {
                let b: ArtifactBody = req.json()?;
                ("artifact", b.value, b.who)
            };
            let mut conn = st.conn();
            db::append(&mut conn, &who.actor(), id, kind, &body)?;
            ok(201, &json!({ "task": db::get_task(&conn, id)? }))
        }
        ("POST", ["api", "tasks", id, "classify"]) => {
            let id = task_id(id)?;
            let b: ClassifyBody = req.json()?;
            let mut conn = st.conn();
            db::classify(
                &mut conn,
                &b.who.actor(),
                id,
                b.escalation.as_deref(),
                b.workload.as_deref(),
                b.episode,
            )?;
            ok(200, &json!({ "task": db::get_task(&conn, id)? }))
        }
        ("POST", ["api", "tasks", id, "protect"]) => {
            let id = task_id(id)?;
            let b: ProtectBody = req.json()?;
            let result =
                remote::protect(&st.conn(), id, b.clear, b.note.as_deref(), &b.who.actor())?;
            ok(200, &result)
        }
        ("POST", ["api", "tasks", id, "deps"]) => {
            let id = task_id(id)?;
            let b: DepsBody = req.json()?;
            let mut conn = st.conn();
            let changed = db::edit_deps(&mut conn, &b.who.actor(), id, &b.add, &b.remove)?;
            ok(
                200,
                &json!({ "changed": changed, "task": db::get_task(&conn, id)? }),
            )
        }
        ("POST", ["api", "tasks", id, "approve"]) => {
            let id = task_id(id)?;
            let b: ApproveBody = req.json()?;
            let mut conn = st.conn();
            db::approve(
                &mut conn,
                &b.who.actor(),
                id,
                &b.sha,
                &b.scope,
                b.note.as_deref(),
            )?;
            ok(200, &json!({ "task": db::get_task(&conn, id)? }))
        }
        ("POST", ["api", "claim"]) => {
            let b: ClaimBody = req.json()?;
            let claimed = db::claim(
                &mut st.conn(),
                &b.who.actor(),
                b.project.as_deref(),
                &b.by,
                None,
            )?;
            let task = match claimed {
                Claim::Claimed(t) => Some(t),
                _ => None,
            };
            ok(200, &json!({ "task": task }))
        }
        _ => Err(HttpError(
            404,
            format!("no route for {} /{}", req.method, path.join("/")),
        )
        .into()),
    }
}

/// Log cursor parameters of a run log request: (after, before).
fn log_cursor(req: &Request) -> Result<(Option<u64>, Option<u64>)> {
    if req
        .query
        .keys()
        .any(|k| !["after", "before", "generation"].contains(&k.as_str()))
    {
        return Err(HttpError(400, "unsupported log parameter".into()).into());
    }
    let offset = |key| -> Result<Option<u64>> {
        req.query(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| HttpError(400, "invalid log cursor".into()).into())
            })
            .transpose()
    };
    let after = offset("after")?;
    let before = offset("before")?;
    if after.is_some() && before.is_some() {
        return Err(HttpError(400, "choose after or before".into()).into());
    }
    Ok((after, before))
}

/// Asks the owning dispatcher for a page of a run log; `query` names the log
/// (`{"kind":"log"|"bot_log","run":id}`), `run` is echoed in the response.
fn relay_log(
    st: &State,
    req: &Request,
    owner: Option<String>,
    mut query: serde_json::Value,
    run: serde_json::Value,
) -> Result<Response> {
    let (after, before) = log_cursor(req)?;
    query["after"] = json!(after);
    query["before"] = json!(before);
    query["generation"] = json!(req.query("generation"));
    let sender = st
        .relay
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(id, _, _)| Some(id) == owner.as_ref())
        .map(|(_, _, s)| s.clone());
    let Some(sender) = sender else {
        return ok(
            200,
            &json!({"run":run,"unavailable":"Dispatcher unavailable","retryable":true}),
        );
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if sender
        .try_send(remote::LogJob { query, reply: tx })
        .is_err()
    {
        return ok(
            200,
            &json!({"run":run,"unavailable":"Dispatcher busy","retryable":true}),
        );
    }
    let mut response = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| json!({"unavailable":"Dispatcher unavailable","retryable":true}));
    response["run"] = run;
    ok(200, &response)
}

/// Public wake view: message text, attachments and the owner's Telegram
/// `/wake` notes need the token.
fn redact(mut wakes: Vec<bot::Wake>) -> Vec<bot::Wake> {
    for w in &mut wakes {
        if w.actor == "owner" {
            w.body = String::new();
        }
        if let Some(m) = &mut w.message {
            m.text = String::new();
            m.attachments = json!([]);
        }
    }
    wakes
}

fn limit(req: &Request, default: i64) -> Result<i64> {
    Ok(match req.query("limit") {
        Some(l) => l
            .parse::<i64>()
            .map_err(|_| HttpError(400, format!("invalid limit {l:?}")))?,
        None => default,
    }
    .clamp(1, MAX_RUNS_LIMIT))
}

/// `/api/bots` and `/api/bot-runs` routes; `None` when the path is not a bot route.
fn bot_route(st: &State, req: &Request, path: &[&str]) -> Result<Option<Response>> {
    #[derive(Deserialize)]
    struct Create {
        bot: bot::NewBot,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct EditBody {
        #[serde(flatten)]
        edit: bot::Edit,
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct NotesBody {
        notes: String,
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct WakeBody {
        in_seconds: Option<i64>,
        at: Option<String>,
        #[serde(default)]
        now: bool,
        note: String,
        key: Option<String>,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct InboxBody {
        run: Option<i64>,
        #[serde(default)]
        take: bool,
        hook: Option<String>,
        stop_hook_active: Option<bool>,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct SubscribeBody {
        #[serde(flatten)]
        change: subscription::Change,
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct UnsubscribeBody {
        project: Option<String>,
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct ScheduleAdd {
        #[serde(flatten)]
        schedule: schedule::NewSchedule,
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct ScheduleChange {
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    let now = remote::epoch();
    let r = match (req.method.as_str(), path) {
        ("GET", ["api", "bots", name, "schedules"]) => ok(200, &schedule::list(&st.conn(), name)?),
        ("POST", ["api", "bots", name, "schedules"]) => {
            let b: ScheduleAdd = req.json()?;
            ok(
                201,
                &schedule::add(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    &b.schedule,
                    &b.why,
                    now,
                )?,
            )
        }
        (
            "POST",
            ["api", "bots", name, "schedules", sched, action @ ("pause" | "resume" | "remove")],
        ) => {
            let b: ScheduleChange = req.json()?;
            schedule::change(
                &mut st.conn(),
                &b.who.actor(),
                name,
                sched,
                action,
                &b.why,
                now,
            )?;
            ok(200, &json!({"schedule": sched, "action": action}))
        }
        ("GET", ["api", "bots", name, "subscriptions"]) => {
            ok(200, &subscription::list(&st.conn(), name)?)
        }
        ("POST", ["api", "bots", name, "subscriptions"]) => {
            let b: SubscribeBody = req.json()?;
            ok(
                200,
                &subscription::subscribe(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    &b.change,
                    &b.why,
                    now,
                )?,
            )
        }
        ("POST", ["api", "bots", name, "subscriptions", "remove"]) => {
            let b: UnsubscribeBody = req.json()?;
            subscription::unsubscribe(
                &mut st.conn(),
                &b.who.actor(),
                name,
                b.project.as_deref(),
                &b.why,
            )?;
            ok(200, &json!({"removed": true}))
        }
        ("GET", ["api", "bots"]) => {
            let bots: Vec<_> = bot::list(&st.conn())?.iter().map(bot::summary).collect();
            ok(200, &bots)
        }
        ("GET", ["api", "bots", name]) => ok(200, &bot::get(&st.conn(), name)?),
        ("GET", ["api", "bots", name, "wakes"]) => ok(
            200,
            &redact(bot::wakes(
                &st.conn(),
                name,
                req.flag("undelivered"),
                limit(req, DEFAULT_RUNS_LIMIT)?,
            )?),
        ),
        ("GET", ["api", "bots", name, "events"]) => ok(
            200,
            &bot::events(&st.conn(), name, limit(req, DEFAULT_RUNS_LIMIT)?)?,
        ),
        ("GET", ["api", "bots", name, "activity"]) => ok(
            200,
            &bot::activity(&st.conn(), name, limit(req, MAX_RUNS_LIMIT)?)?,
        ),
        ("GET", ["api", "bot-runs"]) => ok(
            200,
            &bot::runs(
                &st.conn(),
                req.query("bot"),
                req.flag("live"),
                limit(req, DEFAULT_RUNS_LIMIT)?,
            )?,
        ),
        ("GET", ["api", "bot-runs", id]) => ok(200, &bot::get_run(&st.conn(), task_id(id)?)?),
        ("GET", ["api", "bot-runs", id, "log"]) => {
            log_cursor(req)?;
            let run = bot::get_run(&st.conn(), task_id(id)?)?;
            let owner = Some(run.dispatcher.clone());
            relay_log(
                st,
                req,
                owner,
                json!({"kind":"bot_log","run":run.id}),
                serde_json::to_value(run)?,
            )
        }
        ("POST", ["api", "bots"]) => {
            let b: Create = req.json()?;
            st.cfg
                .validate_bot_agent(b.bot.agent.as_deref().unwrap_or("claude"))?;
            let created = bot::create(&mut st.conn(), &b.who.actor(), &b.bot, now)?;
            ok(201, &created)
        }
        ("PATCH", ["api", "bots", name]) => {
            let b: EditBody = req.json()?;
            ok(
                200,
                &bot::edit(&mut st.conn(), &b.who.actor(), name, &b.edit, &b.why)?,
            )
        }
        ("PUT", ["api", "bots", name, "notes"]) => {
            let b: NotesBody = req.json()?;
            ok(
                200,
                &bot::set_notes(&mut st.conn(), &b.who.actor(), name, &b.notes, &b.why)?,
            )
        }
        ("POST", ["api", "bots", name, "wakes"]) => {
            let b: WakeBody = req.json()?;
            let due = match (b.in_seconds, &b.at, b.now) {
                (Some(s), None, false) if s > 0 => now + s,
                (None, Some(at), false) => {
                    bot::epoch_of(&db::parse_wake(at)?).context("invalid wake time")?
                }
                (None, None, true) => now,
                _ => bail!("give exactly one of in_seconds (> 0), at or now"),
            };
            let (id, due_at) = bot::add_wake(
                &mut st.conn(),
                &b.who.actor(),
                name,
                due,
                &b.note,
                b.key.as_deref(),
                now,
            )?;
            ok(201, &json!({"wake":id,"due_at":due_at}))
        }
        ("POST", ["api", "bots", name, "inbox"]) => {
            let b: InboxBody = req.json()?;
            if let Some(h) = &b.hook {
                if !matches!(h.as_str(), "post-tool" | "stop") {
                    bail!("hook must be post-tool or stop");
                }
            }
            ok(
                200,
                &bot::inbox(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    b.run,
                    b.take || b.hook.is_some(),
                    b.hook.as_deref(),
                    b.stop_hook_active,
                    now,
                )?,
            )
        }
        ("POST", ["api", "bots", name, action @ ("pause" | "resume" | "stop" | "reset")]) => {
            let who: Who = req.json()?;
            ok(
                200,
                &bot::change(&mut st.conn(), &who.actor(), name, action)?,
            )
        }
        ("GET", ["api", "bots", name, "tg", "history"]) => {
            let before = req.query("before").map(task_id).transpose()?;
            ok(
                200,
                &channel::history(
                    &st.conn(),
                    name,
                    limit(req, DEFAULT_RUNS_LIMIT)?,
                    before,
                    req.query("search"),
                )?,
            )
        }
        ("GET", ["api", "bots", name, "tg", "board"]) => ok(
            200,
            &crate::board::show(&st.conn(), name, crate::remote::epoch())?,
        ),
        ("POST", ["api", "bots", name, "tg", "board"]) => {
            #[derive(Deserialize)]
            struct Board {
                msg: Option<i64>,
            }
            let b: Board = req.json()?;
            let msg = crate::board::set_message(&mut st.conn(), name, b.msg)?;
            ok(200, &json!({"message": msg}))
        }
        ("POST", ["api", "bots", name, "tg", action]) => tg_route(st, req, name, action),
        _ => return Ok(None),
    };
    r.map(Some)
}

/// `POST /api/bots/<name>/tg/<action>`: outbound Telegram calls.
#[cfg(not(feature = "telegram"))]
fn tg_route(_: &State, _: &Request, _: &str, _: &str) -> Result<Response> {
    Err(HttpError(409, NO_TELEGRAM.into()).into())
}

/// `POST /api/bots/<name>/tg/<action>`: outbound Telegram calls.
#[cfg(feature = "telegram")]
fn tg_route(st: &State, req: &Request, name: &str, action: &str) -> Result<Response> {
    #[derive(Deserialize)]
    struct Send {
        text: String,
        #[serde(flatten)]
        opts: telegram::SendOpts,
    }
    #[derive(Deserialize)]
    struct Msg {
        msg: i64,
        #[serde(default)]
        emoji: String,
    }
    #[derive(Deserialize)]
    struct Edit {
        msg: i64,
        text: Option<String>,
        #[serde(default)]
        html: bool,
        /// Replaces the keyboard; `[]` removes it; absent keeps the old
        /// behavior (no `reply_markup` sent).
        buttons: Option<Vec<(String, String)>>,
    }
    #[derive(Deserialize)]
    struct Pin {
        msg: i64,
        #[serde(default)]
        silent: bool,
    }
    #[derive(Deserialize)]
    struct Typing {
        chat: Option<i64>,
    }
    let gw = st
        .telegram
        .as_ref()
        .ok_or_else(|| HttpError(409, "telegram is not configured on this server".into()))?;
    match action {
        "send" => {
            let b: Send = req.json()?;
            ok(200, &gw.send(st, name, &b.text, &b.opts)?)
        }
        "send-file" => {
            let num = |k: &str| -> Result<Option<i64>> {
                req.query(k)
                    .map(|v| {
                        v.parse()
                            .map_err(|_| HttpError(400, format!("invalid {k} {v:?}")).into())
                    })
                    .transpose()
            };
            let opts = telegram::SendOpts {
                chat: num("chat")?,
                reply_to: num("reply_to")?,
                thread: num("thread")?,
                ..Default::default()
            };
            let kind = req.query("as").unwrap_or("document");
            telegram::check_upload(kind, req.body.len() as u64)
                .map_err(|e| HttpError(413, format!("{e:#}")))?;
            ok(
                200,
                &gw.send_file(
                    st,
                    name,
                    kind,
                    req.query("name").unwrap_or("file"),
                    &req.body,
                    req.query("caption"),
                    &opts,
                )?,
            )
        }
        "edit" => {
            let b: Edit = req.json()?;
            ok(
                200,
                &gw.edit(
                    st,
                    name,
                    b.msg,
                    b.text.as_deref(),
                    b.html,
                    b.buttons.as_deref(),
                )?,
            )
        }
        "pin" | "unpin" => {
            let b: Pin = req.json()?;
            ok(200, &gw.pin(st, name, b.msg, action == "pin", b.silent)?)
        }
        "react" => {
            let b: Msg = req.json()?;
            ok(200, &gw.react(st, name, b.msg, &b.emoji)?)
        }
        "delete" => {
            let b: Msg = req.json()?;
            ok(200, &gw.delete(st, name, b.msg)?)
        }
        "typing" => {
            let b: Typing = req.json()?;
            ok(200, &gw.typing(st, name, b.chat)?)
        }
        _ => Err(HttpError(404, format!("no telegram action {action:?}")).into()),
    }
}

fn connect_dispatcher(
    st: &State,
    mut stream: TcpStream,
    req: &Request,
    start: Instant,
) -> Result<()> {
    let id = req
        .query("id")
        .context("missing dispatcher id")?
        .to_string();
    let generation = req
        .query("generation")
        .context("missing generation")?
        .to_string();
    remote::heartbeat(&st.conn(), &id, &generation)?;
    let key = req
        .headers
        .get("sec-websocket-key")
        .context("missing websocket key")?;
    if req.method != "GET"
        || req.headers.get("sec-websocket-version").map(String::as_str) != Some("13")
        || !req
            .headers
            .get("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    {
        bail!("invalid websocket upgrade");
    }
    // Registration already fenced ownership, so a new connection replaces one
    // whose peer died before its heartbeat noticed.
    let mut relay = st.relay.lock().unwrap();
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    st.metrics
        .observe(&req.method, crate::metrics::route(&req.path), 101, start);
    write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")?;
    stream.flush()?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut ws = tungstenite::WebSocket::from_raw_socket(
        stream,
        tungstenite::protocol::Role::Server,
        Some(remote::socket_config()),
    );
    let (sender, receiver) = std::sync::mpsc::sync_channel::<remote::LogJob>(4);
    let connection = uuid::Uuid::new_v4().to_string();
    *relay = Some((id.clone(), connection.clone(), sender));
    drop(relay);
    let result = (|| -> Result<()> {
        while !st.stop.load(Ordering::SeqCst) {
            let job = match receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(job) => Some(job),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                // A newer connection replaced this one.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            let mut query = job
                .as_ref()
                .map(|j| j.query.clone())
                .unwrap_or_else(|| json!({"kind":"heartbeat"}));
            query["aborts"] = json!(remote::pending_aborts(&st.conn(), &id, &generation)?);
            query["dispatcher_generation"] = json!(generation);
            ws.send(tungstenite::Message::text(query.to_string()))?;
            let response: serde_json::Value = serde_json::from_str(ws.read()?.to_text()?)?;
            remote::heartbeat(&st.conn(), &id, &generation)?;
            if let Some(job) = job {
                let _ = job.reply.send(response);
            }
        }
        Ok(())
    })();
    let mut relay = st.relay.lock().unwrap();
    if relay
        .as_ref()
        .is_some_and(|(i, c, _)| i == &id && c == &connection)
    {
        *relay = None;
    }
    result
}
