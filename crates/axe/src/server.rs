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
use crate::blob;
use crate::bot;
use crate::channel;
use crate::config::{self, Config};
use crate::db::{self, Actor, Changes, Claim, Filter, NewTask, RunFilter};
use crate::doc;
use crate::form;
use crate::goal;
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
    blobs: blob::Store,
    /// The doc store; `None` when it could not open (no `git`).
    docs: Option<doc::Store>,
    /// Sealed forms, in memory only (`form.rs`).
    forms: form::Forms,
    /// Open web login codes (`login.rs`).
    logins: crate::login::Codes,
}

impl State {
    /// Checks an agent name against the config. A miss re-reads the config
    /// file, so an agent added since the server started is usable without a
    /// restart; the rest of the loaded config is unchanged.
    fn check_agent(&self, check: impl Fn(&Config) -> Result<()>) -> Result<()> {
        check(&self.cfg).or_else(|miss| match Config::load(&self.config_path) {
            Ok(fresh) => check(&fresh),
            Err(e) => Err(anyhow::anyhow!(
                "{miss:#} (re-reading {}: {e:#})",
                self.config_path.display()
            )),
        })
    }
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
    fn runtimes(&self) -> serde_json::Value {
        crate::runtime::view(&self.admission())
    }
    fn form_submit(&self, sealed: &str) -> Result<form::Form> {
        let f = self.forms.submit(sealed, remote::epoch())?;
        Ok(deliver_form(self, &f.spec.form).unwrap_or(f))
    }
    fn form_close(&self, form: &str, bot: &str, chat: i64) -> Option<Vec<(String, String)>> {
        let at = form::Prompt {
            bot: bot.into(),
            chat,
        };
        let open = self.forms.claim_close(form, &at)?;
        Some(form::buttons(&open, self.cfg.forms.page()))
    }
    fn runtime_login(&self, bot: &str, runtime: &str, method: &str) -> Result<String> {
        runtime_login(self, bot, runtime, method)
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
    let blobs = blob::Store::open(&cfg.blobs, &opts.db_path).context("open the blob store")?;
    let docs = doc::Store::open(&cfg.docs, &opts.db_path)
        .map_err(|e| eprintln!("axe server: doc store disabled: {e:#}"))
        .ok();
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
        telegram: cfg
            .telegram
            .as_ref()
            .map(|t| Arc::new(Gateway::new(t, cfg.blobs.public_url.clone()))),
        #[cfg(not(feature = "telegram"))]
        telegram: None,
        blobs,
        docs,
        forms: form::Forms::default(),
        logins: crate::login::Codes::default(),
    });
    {
        let st = Arc::clone(&state);
        std::thread::spawn(move || form_loop(&st));
    }
    #[cfg(feature = "telegram")]
    if let Some(gw) = &state.telegram {
        gw.bind(&state.conn());
        eprintln!("axe server: telegram gateway for bots {:?}", gw.names());
        gw.start(Arc::clone(&state) as Arc<dyn channel::Store>);
    }
    #[cfg(not(feature = "telegram"))]
    if cfg.telegram.is_some() {
        eprintln!("axe server: [telegram] is configured but {NO_TELEGRAM}; ignoring it");
    }
    state.metrics.refresh(&state.conn())?;
    listener.set_nonblocking(true)?;
    {
        let st = Arc::clone(&state);
        std::thread::spawn(move || housekeeping_loop(&st));
    }
    while !stop.load(Ordering::SeqCst) {
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
    /// Length of a body left unread for streaming (blob uploads).
    streamed: Option<u64>,
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
    } else if e.downcast_ref::<doc::Conflict>().is_some() {
        409
    } else if e.downcast_ref::<doc::Invalid>().is_some() {
        400
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
        303 => "See Other",
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
/// `token` decides whether a large upload body is allowed. A blob upload's
/// body stays in `reader` for the handler to stream.
fn read_request(
    reader: &mut BufReader<&TcpStream>,
    token: Option<&str>,
) -> Result<Option<Request>, HttpError> {
    let bad = |m: &str| HttpError(400, m.to_string());
    let mut head = 0usize;
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let limit = (MAX_HEADER_BYTES - head) as u64;
        let n = (&mut *reader)
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
    let mut streamed = None;
    if let Some(len) = headers.get("content-length") {
        let len: usize = len.parse().map_err(|_| bad("invalid content-length"))?;
        if method == "POST" && path == ["api", "blobs"] {
            streamed = Some(len as u64);
        } else {
            if len > body_limit(method, &path, bearer_ok(&headers, token)) {
                return Err(HttpError(413, "request body too large".into()));
            }
            body.resize(len, 0);
            reader
                .read_exact(&mut body)
                .map_err(|_| bad("truncated body"))?;
        }
    } else if headers.contains_key("transfer-encoding") {
        return Err(HttpError(411, "content-length required".into()));
    }
    Ok(Some(Request {
        method: method.to_string(),
        path,
        query,
        headers,
        body,
        streamed,
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

/// GETs of message content (`/api/bots/<name>/tg/...`, asks, which quote
/// cards), blobs and docs need the token, unlike other reads.
fn private_read(path: &[String]) -> bool {
    (path.len() >= 4 && path[0] == "api" && path[1] == "bots" && path[3] == "tg")
        || (path.len() >= 2
            && path[0] == "api"
            && matches!(
                path[1].as_str(),
                "blobs" | "docs" | "store" | "asks" | "forms"
            ))
}

/// Store reads a dashboard read session (`/api/session`) may make without
/// the token: blob and doc GETs, and signing a link to a blob.
fn store_read(req: &Request) -> bool {
    let p: Vec<&str> = req.path.iter().map(String::as_str).collect();
    match (req.method.as_str(), p.as_slice()) {
        ("GET", ["api", "blobs" | "docs" | "store", ..]) => true,
        ("POST", ["api", "blobs", sha, "link"]) => blob::check_sha(sha).is_ok(),
        _ => false,
    }
}

/// Longest sealed message link a Telegram button carries.
#[cfg(feature = "telegram")]
const SEALED_LINK_MAX: usize = 4096;

/// Name of the dashboard's read-session cookie.
const SESSION_COOKIE: &str = "axe_read";

/// The expiry of the request's valid read-session cookie.
fn session(st: &State, req: &Request) -> Option<i64> {
    let cookies = req.headers.get("cookie")?;
    cookies
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .filter(|(k, _)| *k == SESSION_COOKIE)
        .find_map(|(_, v)| st.blobs.verify_session(v, remote::epoch()))
}

/// A new read-session cookie and its expiry. `SameSite=Lax`, so a sign-in
/// link opened from another app lands signed in; the cookie only reads, and
/// every write needs the token, so it carries no cross-site write.
fn session_cookie(st: &State) -> (String, i64) {
    let exp = remote::epoch() + blob::SESSION_SECS;
    let cookie = format!(
        "{SESSION_COOKIE}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
        st.blobs.session_value(exp),
        blob::SESSION_SECS
    );
    (cookie, exp)
}

/// `/api/session` and `/api/session-end`: the browser's read session. A
/// `POST /api/session` with the token (or a redeemed login code, `/login`)
/// sets an HttpOnly cookie that lets the browser make reads (never writes)
/// for [`blob::SESSION_SECS`], so the page keeps no token. `GET` reports it;
/// `POST /api/session-end` clears it. Returns the response and the
/// `Set-Cookie` value, if any.
fn session_route(st: &State, req: &Request) -> (Response, Option<String>) {
    let at = |exp: i64| {
        time::OffsetDateTime::from_unix_timestamp(exp)
            .ok()
            .and_then(|t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
    };
    let open = |exp: Option<i64>| json!({"read": exp.is_some(), "expires_at": exp.and_then(at)});
    let reply = |status: u16, v: serde_json::Value| Response {
        status,
        body: serde_json::to_vec(&v).unwrap_or_default(),
    };
    let ending = req.path[1] == "session-end";
    match (req.method.as_str(), ending) {
        ("GET", false) => (reply(200, open(session(st, req))), None),
        ("POST", false) if authorized(st, req) => {
            let (cookie, exp) = session_cookie(st);
            (reply(200, open(Some(exp))), Some(cookie))
        }
        ("POST", false) => (error(401, "unauthorized"), None),
        ("POST", true) => (
            reply(200, open(None)),
            Some(format!(
                "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
            )),
        ),
        _ => (error(405, "use GET or POST"), None),
    }
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

/// Whether a request may go on past the sign-in gate: with the token,
/// anything; with a read session (`/api/session`, `/login`), every GET but
/// message content ([`private_read`]) and the store reads ([`store_read`]).
/// `/healthz` and `/metrics` (counters for the scraper) need neither. With
/// no token configured (loopback only), everything may.
fn allowed(st: &State, req: &Request) -> bool {
    let read = req.method == "GET";
    if read && matches!(req.path.as_slice(), [p] if p == "healthz" || p == "metrics") {
        return true;
    }
    if authorized(st, req) {
        return true;
    }
    session(st, req).is_some() && ((read && !private_read(&req.path)) || store_read(req))
}

/// A browser page (the dashboard) rather than an API route.
fn is_page(path: &[String]) -> bool {
    !matches!(
        path.first().map(String::as_str),
        Some("api" | "healthz" | "metrics" | "b")
    )
}

/// `/login`: the sign-in page. `GET ?code=` redeems a one-time login code
/// (`POST /api/login-links`) and `POST` a form with the token; both set the
/// read-session cookie and go to `/`. Returns the status, page and extra
/// header lines.
fn login_route(st: &State, req: &Request) -> (u16, String, String) {
    let signed_in = |st: &State| {
        let (cookie, _) = session_cookie(st);
        (
            303,
            String::new(),
            format!("Set-Cookie: {cookie}\r\nLocation: /\r\n"),
        )
    };
    let refused = |note: &str| (401, crate::login::page(note), String::new());
    match req.method.as_str() {
        "GET" => match req.query("code") {
            Some(code) if st.logins.redeem(code, remote::epoch()) => signed_in(st),
            Some(_) => refused("This sign-in link was already used or has expired."),
            None if session(st, req).is_some() => (303, String::new(), "Location: /\r\n".into()),
            None => (200, crate::login::page(""), String::new()),
        },
        "POST" => {
            let body = String::from_utf8_lossy(&req.body);
            let given = body
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "token")
                .map(|(_, v)| pct_decode(v, true))
                .unwrap_or_default();
            if token_ok(given.trim(), st.token.as_deref()) {
                signed_in(st)
            } else {
                refused("Wrong token.")
            }
        }
        _ => (405, crate::login::page(""), String::new()),
    }
}

fn write_page(w: &mut TcpStream, status: u16, html: &str, extra: &str) -> std::io::Result<()> {
    write_head(
        w,
        status,
        "text/html; charset=utf-8",
        Some(html.len()),
        None,
        extra,
    )?;
    w.write_all(html.as_bytes())?;
    w.flush()
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
    token_ok(given, Some(token))
}

/// `given` is the server token (always true without a token).
fn token_ok(given: &str, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return true;
    };
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
    let mut reader = BufReader::new(&stream);
    let req = match read_request(&mut reader, st.token.as_deref()) {
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
    if req.path.first().is_some_and(|p| p == "b") && req.method == "GET" {
        return blob_link_get(st, &mut w, &req, &observe);
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
    if req.path.first().is_some_and(|p| p == "api")
        && matches!(
            req.path.get(1).map(String::as_str),
            Some("session" | "session-end")
        )
        && req.path.len() == 2
    {
        let (r, cookie) = session_route(st, &req);
        observe(r.status);
        write_head(
            &mut w,
            r.status,
            "application/json",
            Some(r.body.len()),
            None,
            &cookie
                .map(|c| format!("Set-Cookie: {c}\r\n"))
                .unwrap_or_default(),
        )?;
        w.write_all(&r.body)?;
        return Ok(w.flush()?);
    }
    if req.path == ["login"] {
        let (status, page, extra) = login_route(st, &req);
        observe(status);
        return Ok(write_page(&mut w, status, &page, &extra)?);
    }
    let read = req.method == "GET";
    if !allowed(st, &req) {
        observe(401);
        if read && is_page(&req.path) {
            return Ok(write_page(&mut w, 401, &crate::login::page(""), "")?);
        }
        return Ok(write_response(&mut w, &error(401, "unauthorized"), None)?);
    }
    if req.path.first().is_some_and(|p| p == "api") && req.path.get(1).is_some_and(|p| p == "blobs")
    {
        return blob_api(st, &mut w, &mut reader, &req, &observe);
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
    #[cfg(feature = "embed-ui")]
    if read && crate::web::is_ui_path(&req.path) {
        if let Some((mime, body)) = crate::web::asset(&req.path) {
            observe(200);
            write_head(&mut w, 200, mime, Some(body.len()), None, "")?;
            w.write_all(&body)?;
            return Ok(w.flush()?);
        }
    }
    if read && req.path == ["api", "stream"] {
        observe(200);
        return Ok(stream_events(st, &mut w, cors)?);
    }
    // A login code is never stored: a retried mint just mints another one.
    let request_id = req
        .headers
        .get(REQUEST_ID_HEADER)
        .filter(|_| !read && req.path != ["api", "login-links"]);
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
/// Writes a blob's bytes after a 200 head with `extra` headers.
fn send_blob(
    w: &mut TcpStream,
    meta: &blob::Meta,
    mut body: Box<dyn Read + Send>,
    extra: &str,
) -> Result<()> {
    write_head(
        w,
        200,
        &meta.content_type,
        Some(meta.size as usize),
        None,
        &format!("X-Axe-Sha256: {}\r\n{extra}", meta.sha256),
    )?;
    std::io::copy(&mut body, w)?;
    Ok(w.flush()?)
}

/// `/api/blobs` routes (the caller checked the token): upload, list, download
/// and link signing.
fn blob_api(
    st: &State,
    w: &mut TcpStream,
    reader: &mut BufReader<&TcpStream>,
    req: &Request,
    observe: &dyn Fn(u16),
) -> Result<()> {
    let path: Vec<&str> = req.path.iter().map(String::as_str).collect();
    // `None`: the response was streamed already.
    let result = (|| -> Result<Option<Response>> {
        if let Some(sha) = path.get(2).filter(|p| **p != "resolve") {
            blob::check_sha(sha).map_err(|e| HttpError(400, e.to_string()))?;
        }
        match (req.method.as_str(), path.as_slice()) {
            ("POST", ["api", "blobs"]) => {
                let len = req
                    .streamed
                    .ok_or_else(|| HttpError(411, "content-length required".into()))?;
                if len > st.blobs.max_bytes {
                    return Err(HttpError(
                        413,
                        format!("blob is over the {} byte limit", st.blobs.max_bytes),
                    )
                    .into());
                }
                let ct = req
                    .headers
                    .get("content-type")
                    .map_or(blob::OCTET_STREAM, String::as_str);
                let (meta, created) = st.blobs.put(reader, len, ct)?;
                let mut v = serde_json::to_value(&meta)?;
                v["created"] = created.into();
                ok(if created { 201 } else { 200 }, &v).map(Some)
            }
            ("GET", ["api", "blobs"]) if req.flag("refs") => {
                let maps = crate::blob_refs::link_maps(st.docs.as_ref())?;
                let refs = crate::blob_refs::index(&st.conn(), &maps)?;
                let mut out = Vec::new();
                for meta in st.blobs.list()? {
                    let mut v = serde_json::to_value(&meta)?;
                    let r = refs.get(&meta.sha256).cloned().unwrap_or_default();
                    v["names"] = json!(r.names);
                    v["tasks"] = json!(r.tasks);
                    out.push(v);
                }
                ok(200, &out).map(Some)
            }
            ("GET", ["api", "blobs"]) => ok(200, &st.blobs.list()?).map(Some),
            ("GET", ["api", "blobs", "resolve"]) => {
                let url = req
                    .query("url")
                    .ok_or_else(|| HttpError(400, "url is required".into()))?;
                let (sha, name) = match crate::blob_refs::signed_links(url).into_iter().next() {
                    Some(hit) => hit,
                    None => {
                        let maps = crate::blob_refs::link_maps(st.docs.as_ref())?;
                        let m = maps
                            .get(&crate::blob_refs::url_key(url))
                            .ok_or_else(|| HttpError(404, "no stored blob for this url".into()))?;
                        let name = m
                            .name
                            .clone()
                            .or_else(|| crate::blob_refs::last_segment(url));
                        (m.sha256.clone(), name)
                    }
                };
                let meta = st
                    .blobs
                    .stat(&sha)?
                    .ok_or_else(|| HttpError(404, format!("blob {sha} not found")))?;
                let mut v = serde_json::to_value(&meta)?;
                v["name"] = json!(name);
                ok(200, &v).map(Some)
            }
            ("GET", ["api", "blobs", sha]) => {
                let Some((meta, body)) = st.blobs.open_blob(sha)? else {
                    return Err(HttpError(404, format!("blob {sha} not found")).into());
                };
                observe(200);
                // A browser may open this with a read session: never as
                // active content on the server's origin.
                send_blob(
                    w,
                    &meta,
                    body,
                    "X-Content-Type-Options: nosniff\r\nContent-Security-Policy: sandbox\r\n",
                )?;
                Ok(None)
            }
            ("POST", ["api", "blobs", sha, "link"]) => {
                #[derive(Deserialize)]
                struct Link {
                    /// Lifetime in seconds.
                    ttl: i64,
                    name: Option<String>,
                }
                let b: Link = req.json()?;
                if b.ttl <= 0 || b.ttl > blob::MAX_LINK_SECS {
                    return Err(HttpError(
                        400,
                        format!("ttl must be 1..={} seconds", blob::MAX_LINK_SECS),
                    )
                    .into());
                }
                let name = b.name.filter(|n| !n.is_empty());
                if name
                    .as_deref()
                    .is_some_and(|n| n.contains('/') || n.chars().any(char::is_control))
                {
                    return Err(HttpError(400, "name must be a file name".into()).into());
                }
                if st.blobs.stat(sha)?.is_none() {
                    return Err(HttpError(404, format!("blob {sha} not found")).into());
                }
                let exp = remote::epoch() + b.ttl;
                let path = st.blobs.link_path(sha, name.as_deref(), exp);
                let url = st
                    .blobs
                    .public_url
                    .as_deref()
                    .map(|base| format!("{}{path}", base.trim_end_matches('/')));
                let expires_at = time::OffsetDateTime::from_unix_timestamp(exp)?
                    .format(&time::format_description::well_known::Rfc3339)?;
                ok(
                    201,
                    &json!({"path": path, "url": url, "expires_at": expires_at}),
                )
                .map(Some)
            }
            _ => Err(HttpError(404, "no such blob route".into()).into()),
        }
    })();
    let r = match result {
        Ok(None) => return Ok(()),
        Ok(Some(r)) => r,
        Err(e) => blob_error(&e),
    };
    observe(r.status);
    Ok(write_response(w, &r, None)?)
}

/// `GET /b/<sha>[/<name>]?exp=..&sig=..`: a signed link, readable without the
/// token until it expires. The blob renders inline in a sandbox, so HTML
/// cannot act on this server's origin.
fn blob_link_get(
    st: &State,
    w: &mut TcpStream,
    req: &Request,
    observe: &dyn Fn(u16),
) -> Result<()> {
    let result = (|| -> Result<(blob::Meta, Box<dyn Read + Send>, Option<&str>)> {
        let (sha, name) = match req.path.as_slice() {
            [_, sha] => (sha.as_str(), None),
            [_, sha, name] => (sha.as_str(), Some(name.as_str())),
            _ => return Err(HttpError(404, "no such link".into()).into()),
        };
        blob::check_sha(sha).map_err(|e| HttpError(404, e.to_string()))?;
        let (exp, sig) = (
            req.query("exp").unwrap_or(""),
            req.query("sig").unwrap_or(""),
        );
        match st.blobs.verify_link(sha, name, exp, sig, remote::epoch()) {
            Ok(()) => {}
            Err(blob::LinkError::Invalid) => {
                return Err(HttpError(403, "invalid link signature".into()).into())
            }
            Err(blob::LinkError::Expired) => {
                return Err(HttpError(403, "link expired".into()).into())
            }
        }
        match st.blobs.open_blob(sha)? {
            Some((meta, body)) => Ok((meta, body, name)),
            None => Err(HttpError(404, format!("blob {sha} not found")).into()),
        }
    })();
    match result {
        Ok((meta, body, name)) => {
            observe(200);
            let file = name
                .map(|n| {
                    let ascii: String = n
                        .chars()
                        .map(|c| {
                            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    format!(
                        "; filename=\"{ascii}\"; filename*=UTF-8''{}",
                        channel::pct_encode(n)
                    )
                })
                .unwrap_or_default();
            send_blob(
                w,
                &meta,
                body,
                &format!(
                    "Content-Disposition: inline{file}\r\nX-Content-Type-Options: nosniff\r\n\
                     Content-Security-Policy: sandbox\r\n"
                ),
            )
        }
        Err(e) => {
            let r = blob_error(&e);
            observe(r.status);
            Ok(write_response(w, &r, None)?)
        }
    }
}

/// Request problems carry an [`HttpError`]; any other blob error is the
/// store's (disk, S3), so a 500.
fn blob_error(e: &anyhow::Error) -> Response {
    let mut r = error_response(e);
    if e.downcast_ref::<HttpError>().is_none() {
        r.status = 500;
    }
    r
}

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
            write_head(
                w,
                200,
                &mime,
                Some(bytes.len()),
                None,
                &format!("X-Axe-File-Name: {}\r\n", channel::pct_encode(&name)),
            )?;
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

/// Days `GET /api/store/stats` counts as recent.
const STORE_RECENT_DAYS: i64 = 7;
/// Content types `GET /api/store/stats` names; the rest are summed.
const STORE_TOP_TYPES: usize = 4;

/// `GET /api/store/stats`: blob totals, recent and by content type, blobs no
/// task links, and the doc count (`docs` is null when the doc store is off).
fn store_stats(st: &State) -> Result<Response> {
    let maps = crate::blob_refs::link_maps(st.docs.as_ref())?;
    let refs = crate::blob_refs::index(&st.conn(), &maps)?;
    let since = time::OffsetDateTime::now_utc() - time::Duration::days(STORE_RECENT_DAYS);
    let stats = blob::stats(&st.blobs.list()?, since, STORE_TOP_TYPES, |sha| {
        refs.contains_key(sha)
    })?;
    let mut v = serde_json::to_value(&stats)?;
    v["recent_days"] = STORE_RECENT_DAYS.into();
    v["docs"] = match &st.docs {
        Some(docs) => {
            let mut t = blob::Tally::default();
            for d in docs.list(None)? {
                t.add(d.size);
            }
            serde_json::to_value(t)?
        }
        None => serde_json::Value::Null,
    };
    ok(200, &v)
}

/// `/api/docs` routes (the token was checked: doc reads are private too).
fn doc_route(st: &State, req: &Request, path: &[&str]) -> Result<Response> {
    let docs = st
        .docs
        .as_ref()
        .ok_or_else(|| HttpError(503, "the doc store is disabled (is git installed?)".into()))?;
    let need = |k: &str| {
        req.query(k)
            .ok_or_else(|| anyhow::Error::from(HttpError(400, format!("{k} is required"))))
    };
    match (req.method.as_str(), path) {
        ("GET", ["api", "docs"]) => ok(200, &docs.list(req.query("prefix"))?),
        ("GET", ["api", "docs", "file"]) => {
            ok(200, &docs.get(need("path")?, req.query("version"))?)
        }
        ("GET", ["api", "docs", "log"]) => ok(200, &docs.log(need("path")?)?),
        ("GET", ["api", "docs", "diff"]) => ok(
            200,
            &docs.diff(need("path")?, req.query("from"), req.query("to"))?,
        ),
        ("GET", ["api", "docs", "attachments"]) => {
            let scope = req.query("scope").map(doc::Scope::parse).transpose()?;
            ok(200, &docs.attachments(scope.as_ref())?)
        }
        ("POST", ["api", "docs"]) => {
            #[derive(Deserialize)]
            struct Put {
                path: String,
                content: Option<String>,
                files: Option<Vec<doc::File>>,
                /// The version this write edits; "" creates only.
                base: Option<String>,
                #[serde(default)]
                message: String,
                #[serde(flatten)]
                who: Who,
            }
            let b: Put = req.json()?;
            let body = match (b.content, b.files) {
                (Some(c), None) => doc::Body::Doc(c),
                (None, Some(f)) => doc::Body::Folder(f),
                _ => {
                    return Err(
                        HttpError(400, "give exactly one of content or files".into()).into(),
                    )
                }
            };
            let actor = b.who.actor().name;
            let w = docs.put(&b.path, &body, b.base.as_deref(), &actor, &b.message)?;
            ok(if w.changed { 201 } else { 200 }, &w)
        }
        ("POST", ["api", "docs", verb @ ("attach" | "detach")]) => {
            #[derive(Deserialize)]
            struct Attach {
                path: String,
                scope: String,
                #[serde(flatten)]
                who: Who,
            }
            let b: Attach = req.json()?;
            let scope = doc::Scope::parse(&b.scope)?;
            let actor = b.who.actor().name;
            ok(
                200,
                &docs.attach(&b.path, &scope, *verb == "attach", &actor)?,
            )
        }
        _ => Err(HttpError(404, "no such doc route".into()).into()),
    }
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
    /// Goal name to link the task to.
    goal: Option<String>,
    owner: Option<String>,
    /// Omitted or null: the project checkout; a list (maybe empty): a run
    /// workspace with these repos.
    repos: Option<Vec<String>>,
    #[serde(default)]
    labels: Vec<String>,
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
    goal: Option<String>,
    /// New owner, or "none" to clear it.
    owner: Option<String>,
    /// Replaces the repo list.
    repos: Option<Vec<String>>,
    #[serde(default)]
    add_labels: Vec<String>,
    #[serde(default)]
    remove_labels: Vec<String>,
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
    if path.get(1) == Some(&"docs") && path[0] == "api" {
        return doc_route(st, req, &path);
    }
    if req.method == "GET" && path == ["api", "store", "stats"] {
        return store_stats(st);
    }
    if path.get(1) == Some(&"goals") && path[0] == "api" {
        if let Some(r) = goal_route(st, req, &path)? {
            return Ok(r);
        }
    }
    if path.get(1) == Some(&"forms") && path[0] == "api" {
        return form_route(st, req, &path);
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
            st.check_agent(|c| c.validate_agent(b.template.agent.as_deref()))?;
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
        ("GET", ["api", "rules"]) => ok(200, &crate::rule::list(&st.conn())?),
        ("GET", ["api", "rules", "events"]) => {
            let before = req
                .query("before")
                .map(str::parse)
                .transpose()?
                .unwrap_or(i64::MAX);
            ok(200, &crate::rule::events(&st.conn(), before)?)
        }
        ("POST", ["api", "rules"]) => {
            #[derive(Deserialize)]
            struct Add {
                rule: crate::rule::NewRule,
                #[serde(flatten)]
                who: Who,
            }
            let b: Add = req.json()?;
            let rule = crate::rule::add(&mut st.conn(), &b.who.actor(), &b.rule)?;
            ok(200, &rule)
        }
        ("POST", ["api", "rules", id, "remove"]) => {
            let b: Who = req.json()?;
            let rule = crate::rule::remove(&mut st.conn(), &b.actor(), task_id(id)?)?;
            ok(200, &rule)
        }
        ("GET", ["api", "repos"]) => ok(200, &crate::repo::list(&st.conn())?),
        ("GET", ["api", "repos", name]) => match crate::repo::get(&st.conn(), name)? {
            Some(r) => ok(200, &r),
            None => Err(HttpError(404, format!("repo {name} not found")).into()),
        },
        ("PUT", ["api", "repos", name]) => {
            let b: crate::repo::NewRepo = req.json()?;
            ok(200, &crate::repo::put(&st.conn(), name, &b)?)
        }
        ("DELETE", ["api", "repos", name]) => ok(200, &crate::repo::remove(&st.conn(), name)?),
        ("GET", ["healthz"]) => ok(200, &json!({ "ok": true })),
        ("POST", ["api", "login-links"]) => {
            #[derive(Deserialize)]
            struct B {
                ttl: Option<i64>,
            }
            let b: B = req.json()?;
            let ttl = b.ttl.unwrap_or(crate::login::DEFAULT_TTL);
            if !(crate::login::MIN_TTL..=crate::login::MAX_TTL).contains(&ttl) {
                return Err(HttpError(400, "a login link lives 1m to 1h".into()).into());
            }
            let now = remote::epoch();
            let code = st.logins.mint(ttl, now);
            let base = match &st.blobs.public_url {
                Some(u) => u.trim_end_matches('/').to_string(),
                None => format!(
                    "http://{}",
                    req.headers
                        .get("host")
                        .map_or("127.0.0.1:8790", String::as_str)
                ),
            };
            ok(
                201,
                &json!({"url": format!("{base}/login?code={code}"),
                        "expires_at": crate::sealed::format_time(now + ttl)}),
            )
        }
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
                    label: req.query("label"),
                    owner: req.query("owner"),
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
                goal: req.query("goal"),
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
        ("GET", ["api", "asks"]) => {
            let now = crate::routine::now();
            let bound = |name, default: &str| {
                let s = req.query(name).unwrap_or(default);
                crate::flow::parse_bound(s, now).map_err(|e| HttpError(400, format!("{e:#}")))
            };
            let f = crate::ask::Filter {
                since: bound("since", "7d")?,
                until: asks_until(req, now)?,
                project: req.query("project"),
                bot: req.query("bot"),
                kind: req.query("kind"),
                unrated: req
                    .query("unrated")
                    .is_some_and(|v| v == "true" || v == "1"),
            };
            ok(200, &crate::ask::list(&st.conn(), &f)?)
        }
        ("GET", ["api", "asks", "stats"]) => {
            let now = crate::routine::now();
            let bound = |name, default: &str| {
                let s = req.query(name).unwrap_or(default);
                crate::flow::parse_bound(s, now).map_err(|e| HttpError(400, format!("{e:#}")))
            };
            let (since, until) = (bound("since", "28d")?, asks_until(req, now)?);
            if since >= until {
                return Err(HttpError(400, "since must be before until".into()).into());
            }
            ok(
                200,
                &crate::ask::stats(&st.conn(), req.query("project"), since, until)?,
            )
        }
        ("GET", ["api", "asks", "retro"]) => {
            let limit = retro_limit(req.query("limit"))?;
            ok(200, &retro_preview(st, req.query("bot"), limit)?)
        }
        ("GET", ["api", "asks", id]) => ok(
            200,
            &crate::ask::find(&st.conn(), id).map_err(|e| HttpError(404, format!("{e:#}")))?,
        ),
        ("POST", ["api", "asks", id, "rate"]) => {
            #[derive(Deserialize)]
            struct Rate {
                rating: String,
                #[serde(flatten)]
                who: Who,
            }
            let b: Rate = req.json()?;
            let mut conn = st.conn();
            let a = crate::ask::rate(&mut conn, &b.who.actor(), id, &b.rating)
                .map_err(|e| HttpError(400, format!("{e:#}")))?;
            ok(200, &a)
        }
        ("POST", ["api", "asks", id, "rec"]) => {
            #[derive(Deserialize)]
            struct One {
                rating: String,
                why: Option<String>,
                #[serde(flatten)]
                who: Who,
            }
            let b: One = req.json()?;
            let rec = crate::ask::Rec {
                ask: id.to_string(),
                rating: b.rating,
                why: b.why,
            };
            let mut conn = st.conn();
            let mut a = crate::ask::recommend(&mut conn, &b.who.actor(), &[rec])
                .map_err(|e| HttpError(400, format!("{e:#}")))?;
            ok(200, &a.pop())
        }
        ("POST", ["api", "asks", "recs"]) => {
            #[derive(Deserialize)]
            struct Many {
                recs: Vec<crate::ask::Rec>,
                #[serde(flatten)]
                who: Who,
            }
            let b: Many = req.json()?;
            let mut conn = st.conn();
            let a = crate::ask::recommend(&mut conn, &b.who.actor(), &b.recs)
                .map_err(|e| HttpError(400, format!("{e:#}")))?;
            ok(200, &a)
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
                origin: req.query("origin"),
            };
            if let Some(o) = q.origin.filter(|o| !crate::flow::ORIGINS.contains(o)) {
                return Err(HttpError(400, format!("invalid origin: {o}")).into());
            }
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
        ("GET", ["api", "runtimes"]) => ok(200, &crate::runtime::view(&st.admission())),
        // A runtime turned needs_login or limited: one card to the owner.
        ("POST", ["api", "dispatcher", "runtime-alert"]) => {
            let b: serde_json::Value = req.json()?;
            ok(200, &runtime_alert(st, &b)?)
        }
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
            st.check_agent(|c| c.validate_agent(b.agent.as_deref()))?;
            let mut conn = st.conn();
            let id = db::add_task_full(
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
                b.goal.as_deref(),
                &db::Fields {
                    owner: b.owner,
                    repos: b.repos,
                    labels: b.labels,
                },
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
                goal: b.goal,
                owner: b.owner,
                repos: b.repos,
                add_labels: b.add_labels,
                remove_labels: b.remove_labels,
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

/// `/api/goals` routes (`goal.rs`); `None` when the path is not a goal route.
fn goal_route(st: &State, req: &Request, path: &[&str]) -> Result<Option<Response>> {
    #[derive(Deserialize)]
    struct Propose {
        goal: goal::NewGoal,
        owner_ref: Option<String>,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct EditBody {
        #[serde(flatten)]
        edit: goal::Edit,
        why: String,
        owner_ref: Option<String>,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct ChangeBody {
        why: String,
        owner_ref: Option<String>,
        /// The bot to attach or detach (`join`, `leave`).
        bot: Option<String>,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct RenameBody {
        to: String,
        why: String,
        owner_ref: Option<String>,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct RecordBody {
        kind: String,
        task_id: Option<i64>,
        body: String,
        #[serde(flatten)]
        who: Who,
    }
    let r = match (req.method.as_str(), path) {
        ("GET", ["api", "goals"]) => ok(
            200,
            &goal::list(&st.conn(), req.query("bot"), req.query("state"))?,
        ),
        ("GET", ["api", "goals", name]) => ok(200, &goal::detail(&st.conn(), name)?),
        ("GET", ["api", "goals", name, "events"]) => ok(
            200,
            &goal::events(
                &st.conn(),
                name,
                limit(req, DEFAULT_RUNS_LIMIT)?,
                req.flag("trajectory"),
            )?,
        ),
        ("POST", ["api", "goals"]) => {
            let b: Propose = req.json()?;
            ok(
                201,
                &goal::propose(
                    &mut st.conn(),
                    &b.who.actor(),
                    &b.goal,
                    b.owner_ref.as_deref(),
                    remote::epoch(),
                )?,
            )
        }
        ("PATCH", ["api", "goals", name]) => {
            let b: EditBody = req.json()?;
            ok(
                200,
                &goal::edit(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    &b.edit,
                    &b.why,
                    b.owner_ref.as_deref(),
                )?,
            )
        }
        (
            "POST",
            ["api", "goals", name, action @ ("activate" | "pause" | "resume" | "done" | "drop")],
        ) => {
            let b: ChangeBody = req.json()?;
            ok(
                200,
                &goal::change(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    action,
                    &b.why,
                    b.owner_ref.as_deref(),
                )?,
            )
        }
        ("POST", ["api", "goals", name, action @ ("join" | "leave")]) => {
            let b: ChangeBody = req.json()?;
            let bot = b.bot.as_deref().context("bot is required")?;
            ok(
                200,
                &goal::membership(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    action,
                    bot,
                    &b.why,
                    b.owner_ref.as_deref(),
                )?,
            )
        }
        ("POST", ["api", "goals", name, "rename"]) => {
            let b: RenameBody = req.json()?;
            ok(
                200,
                &goal::rename(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    b.to.trim(),
                    &b.why,
                    b.owner_ref.as_deref(),
                )?,
            )
        }
        ("POST", ["api", "goals", name, "events"]) => {
            let b: RecordBody = req.json()?;
            let id = goal::record(
                &mut st.conn(),
                &b.who.actor(),
                name,
                &b.kind,
                b.task_id,
                &b.body,
            )?;
            ok(201, &json!({"event": id}))
        }
        _ => return Ok(None),
    };
    r.map(Some)
}

/// `/api/bots` and `/api/bot-runs` routes; `None` when the path is not a bot route.
/// After a bot rename is stored: moves the docs attached to `owner:bot:<old>`
/// and re-binds the Telegram gateway. Neither is part of the database
/// transaction; a failure is reported in `warnings`, the rename stands.
fn bot_renamed(st: &State, actor: &Actor, b: bot::Bot, moved: bot::Renamed) -> serde_json::Value {
    let mut warnings = Vec::new();
    let mut docs = Vec::new();
    if let Some(store) = &st.docs {
        let (from, to) = (
            doc::Scope::Owner(format!("bot:{}", moved.from)),
            doc::Scope::Owner(format!("bot:{}", moved.to)),
        );
        match store.move_scope(&from, &to, &actor.name) {
            Ok(paths) => docs = paths,
            Err(e) => warnings.push(format!(
                "docs attached to {from} were not moved ({e:#}); attach them to {to} by hand"
            )),
        }
    }
    #[cfg(feature = "telegram")]
    let telegram = st
        .telegram
        .as_ref()
        .and_then(|gw| gw.renamed(&moved.from, &moved.to));
    #[cfg(not(feature = "telegram"))]
    let telegram: Option<String> = None;
    json!({"bot": b, "moved": moved, "docs": docs, "telegram": telegram, "warnings": warnings})
}

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
        /// Labels the wake `[goal NAME]`; the goal must exist.
        goal: Option<String>,
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
        task: Option<i64>,
        why: String,
        #[serde(flatten)]
        who: Who,
    }
    #[derive(Deserialize)]
    struct ScheduleAdd {
        #[serde(flatten)]
        schedule: schedule::NewSchedule,
        goal: Option<String>,
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
    #[derive(Deserialize)]
    struct RenameBody {
        to: String,
        why: String,
        owner_ref: Option<String>,
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
                &schedule::add_for_goal(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    &b.schedule,
                    b.goal.as_deref(),
                    &b.why,
                    now,
                )?,
            )
        }
        ("PATCH", ["api", "bots", name, "schedules", sched]) => {
            #[derive(Deserialize)]
            struct ScheduleEdit {
                goal: String,
                why: String,
                #[serde(flatten)]
                who: Who,
            }
            let b: ScheduleEdit = req.json()?;
            ok(
                200,
                &schedule::set_goal(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    sched,
                    &b.goal,
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
            match b.task {
                Some(task) => {
                    subscription::unwatch(&mut st.conn(), &b.who.actor(), name, task, &b.why)?
                }
                None => subscription::unsubscribe(
                    &mut st.conn(),
                    &b.who.actor(),
                    name,
                    b.project.as_deref(),
                    &b.why,
                )?,
            }
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
            st.check_agent(|c| c.validate_bot_agent(b.bot.agent.as_deref().unwrap_or("claude")))?;
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
            let mut conn = st.conn();
            let goal = match &b.goal {
                Some(g) if !b.note.trim().is_empty() => goal::resolve(&conn, g)?,
                _ => None,
            };
            let note = match goal {
                Some(g) => goal::label(&g.name, &b.note),
                None => b.note,
            };
            let (id, due_at) = bot::add_wake(
                &mut conn,
                &b.who.actor(),
                name,
                due,
                &note,
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
        ("POST", ["api", "bots", name, "rename"]) => {
            let b: RenameBody = req.json()?;
            let actor = b.who.actor();
            let (renamed, moved) = bot::rename(
                &mut st.conn(),
                &actor,
                name,
                b.to.trim(),
                &b.why,
                b.owner_ref.as_deref(),
            )?;
            ok(200, &bot_renamed(st, &actor, renamed, moved))
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
            &crate::board::show(&st.conn(), name, st.blobs.public_url.as_deref())?,
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
        ("GET", ["api", "bots", name, "tg", "goal-board"]) => ok(
            200,
            &crate::goal_board::show(
                &st.conn(),
                name,
                crate::remote::epoch(),
                st.blobs.public_url.as_deref(),
            )?,
        ),
        ("POST", ["api", "bots", name, "tg", "goal-board"]) => {
            #[derive(Deserialize)]
            struct Board {
                msg: Option<i64>,
            }
            let b: Board = req.json()?;
            let msg = crate::goal_board::set_message(&mut st.conn(), name, b.msg)?;
            ok(200, &json!({"message": msg}))
        }
        ("POST", ["api", "bots", name, "tg", action]) => tg_route(st, req, name, action),
        _ => return Ok(None),
    };
    r.map(Some)
}

/// The `until` bound of an asks read; without one, through the current
/// second (asks made this second included).
fn asks_until(req: &Request, now: i64) -> Result<i64> {
    match req.query("until") {
        Some(s) => {
            Ok(crate::flow::parse_bound(s, now).map_err(|e| HttpError(400, format!("{e:#}")))?)
        }
        None => Ok(now + 1),
    }
}

#[cfg(not(feature = "telegram"))]
fn runtime_alert(_: &State, _: &serde_json::Value) -> Result<serde_json::Value> {
    Ok(json!({"sent": false, "reason": NO_TELEGRAM}))
}

/// A retro card's `limit` (query or body): a positive count, at most
/// `ask::RETRO_ITEMS`; none is the default.
fn retro_limit(s: Option<&str>) -> Result<Option<usize>> {
    let Some(s) = s else {
        return Ok(None);
    };
    match s.trim().parse::<usize>() {
        Ok(n) if (1..=crate::ask::RETRO_ITEMS).contains(&n) => Ok(Some(n)),
        _ => Err(HttpError(
            400,
            format!(
                "limit {s:?}: expected 1 to {} asks",
                crate::ask::RETRO_ITEMS
            ),
        )
        .into()),
    }
}

/// `GET /api/asks/retro[?bot=&limit=]`: the retro card bot `bot` would send
/// now (its own asks of its interval, its size unless `limit`), or every
/// unrated ask of the last 7 days.
fn retro_preview(st: &State, bot: Option<&str>, limit: Option<usize>) -> Result<crate::ask::Retro> {
    let now = crate::routine::now();
    if let Some(b) = bot {
        return crate::ask::bot_retro(&st.conn(), now, b, limit);
    }
    let scope = crate::ask::RetroScope {
        limit,
        ..Default::default()
    };
    crate::ask::retro(&st.conn(), now, &scope)
}

/// Sends a runtime card through the gateway, when one is configured.
#[cfg(feature = "telegram")]
fn runtime_alert(st: &State, b: &serde_json::Value) -> Result<serde_json::Value> {
    match &st.telegram {
        Some(gw) => gw.runtime_alert(st, b),
        None => Ok(json!({"sent": false, "reason": "telegram is not configured on this server"})),
    }
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
        /// Buttons per row; absent or `[]` is one row.
        #[serde(default)]
        rows: Vec<usize>,
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
        "send-sealed" => {
            #[derive(Deserialize)]
            struct Sealed {
                text: String,
                link: String,
            }
            let b: Sealed = req.json()?;
            // Only a link to the form page's message mode: the button opens
            // nothing else, and the link carries only ciphertext.
            let prefix = format!("{}#m=", st.cfg.forms.page());
            if !b.link.starts_with(&prefix) || b.link.len() > SEALED_LINK_MAX {
                return Err(HttpError(
                    400,
                    format!("a sealed message link is {prefix}<message>, at most {SEALED_LINK_MAX} bytes"),
                )
                .into());
            }
            ok(200, &gw.send_sealed(st, name, &b.text, &b.link)?)
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
                    &b.rows,
                )?,
            )
        }
        "retro" => {
            let v: serde_json::Value = req.json()?;
            let limit = match &v["limit"] {
                serde_json::Value::Null => None,
                l => retro_limit(Some(&l.to_string()))?,
            };
            let o: telegram::SendOpts = serde_json::from_value(v)
                .map_err(|e| HttpError(400, format!("invalid JSON body: {e}")))?;
            ok(200, &gw.send_retro(st, name, &o, limit)?)
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

/// Sends one job to the connected dispatcher (`dispatcher`: only when it is
/// that one) and waits up to `timeout` for its answer. `Err` means no
/// dispatcher answered.
fn relay_job(
    st: &State,
    query: serde_json::Value,
    dispatcher: Option<&str>,
    timeout: Duration,
) -> std::result::Result<serde_json::Value, String> {
    let sender = st
        .relay
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(id, _, _)| dispatcher.is_none_or(|d| d == id))
        .map(|(_, _, s)| s.clone());
    let Some(sender) = sender else {
        return Err("the dispatcher is not connected".into());
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    sender
        .try_send(remote::LogJob { query, reply: tx })
        .map_err(|_| "the dispatcher is busy".to_string())?;
    rx.recv_timeout(timeout)
        .map_err(|_| "the dispatcher did not answer".to_string())
}

/// How long the server waits for the dispatcher to sign or deliver a form.
const FORM_RELAY_TIMEOUT: Duration = Duration::from_secs(8);
/// Longest `GET /api/forms/<id>?wait=` hold.
const FORM_WAIT_MAX: u64 = 60;

/// Delivers a submitted form through the dispatcher and records the outcome
/// (a task event for a task target). `None` when it is not submitted.
fn deliver_form(st: &State, id: &str) -> Option<form::Form> {
    let job = st.forms.job(id)?;
    let dispatcher = st.forms.get(id, remote::epoch())?.spec.dispatcher;
    let d = match relay_job(st, job, Some(&dispatcher), FORM_RELAY_TIMEOUT) {
        Ok(v) => match v["error"].as_str() {
            Some(e) => form::Delivery::Refused(e.to_string()),
            None => form::Delivery::Done(v["delivered"].clone()),
        },
        Err(e) => form::Delivery::Unavailable(e),
    };
    let f = st.forms.finish(id, &d, remote::epoch())?;
    match &d {
        form::Delivery::Done(_) => {
            eprintln!(
                "axe server: form {id}: delivered to {}",
                f.spec.target.describe()
            );
            form_event(st, &f, &form::outcome_text(&f));
        }
        form::Delivery::Refused(e) => eprintln!("axe server: form {id}: refused: {e}"),
        form::Delivery::Unavailable(e) => eprintln!("axe server: form {id}: held: {e}"),
    }
    st.notifier.bump();
    Some(f)
}

/// A `form` event on the form's task (names and outcome only, never a value).
fn form_event(st: &State, f: &form::Form, body: &str) {
    let crate::sealed::Target::Task { task } = f.spec.target else {
        return;
    };
    let r = (|| -> Result<()> {
        let conn = st.conn();
        let tx = conn.unchecked_transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
            [task],
            |r| r.get(0),
        )?;
        if exists {
            let actor = Actor::new("forms");
            let body = format!("{body} (form {})", f.spec.form);
            db::write_event(&tx, &actor, task, "form", None, Some(f.state), Some(&body))?;
            tx.commit()?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        eprintln!("axe server: form {}: event: {e:#}", f.spec.form);
    }
}

/// Expires forms, retries deliveries held for an offline dispatcher and
/// closes finished forms' prompts (their outcome, with a keyboard of the
/// forms still open in that chat, or none).
/// The once-a-second routine and bot-schedule ticks and the 15 s metrics
/// snapshot. They take the shared DB connection, so they run here and not on
/// the accept loop: a slow commit must never delay `accept()` (and with it
/// `/healthz`, which needs no DB).
fn housekeeping_loop(st: &State) {
    let mut last_metrics = Instant::now();
    while !st.stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        match routine::tick(&mut st.conn(), routine::now()) {
            Ok(n) if n > 0 => st.notifier.bump(),
            Err(e) => eprintln!("routine scheduler: {e:#}"),
            _ => {}
        }
        match schedule::tick(&mut st.conn(), routine::now()) {
            Ok(n) if n > 0 => st.notifier.bump(),
            Err(e) => eprintln!("bot schedule tick: {e:#}"),
            _ => {}
        }
        if last_metrics.elapsed() >= Duration::from_secs(15) {
            if let Err(e) = st.metrics.refresh(&st.conn()) {
                eprintln!("metrics snapshot: {e:#}");
            }
            last_metrics = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
    }
}

fn form_loop(st: &State) {
    while !st.stop.load(Ordering::SeqCst) {
        st.forms.sweep(remote::epoch());
        for id in st.forms.undelivered() {
            if let Some(f) = deliver_form(st, &id) {
                // Refused but still open; a finished form closes below.
                if f.state == form::OPEN {
                    form_notify(st, &f, None);
                }
            }
        }
        for (f, open) in st.forms.take_closing(remote::epoch()) {
            let buttons = form::buttons(&open, st.cfg.forms.page());
            form_notify(st, &f, Some(&buttons));
        }
        for _ in 0..50 {
            if st.stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Tells the owner a form's outcome where its prompt went; `open` (the open
/// forms' buttons there) replaces the prompt's keyboard (none: removes it).
#[cfg(feature = "telegram")]
fn form_notify(st: &State, f: &form::Form, open: Option<&[(String, String)]>) {
    if let (Some(gw), Some(p)) = (&st.telegram, &f.prompt) {
        gw.form_outcome(st, &p.bot, p.chat, f, open);
    }
}

#[cfg(not(feature = "telegram"))]
fn form_notify(_: &State, _: &form::Form, _: Option<&[(String, String)]>) {}

/// `/api/forms` routes (the token was checked: forms are private reads).
fn form_route(st: &State, req: &Request, path: &[&str]) -> Result<Response> {
    #[derive(Deserialize)]
    struct Ask {
        request: crate::sealed::Request,
        /// Send the owner a Telegram prompt (default true).
        telegram: Option<bool>,
        /// The Telegram bot that sends it: required unless `telegram` is false.
        bot: Option<String>,
    }
    #[derive(Deserialize)]
    struct Submit {
        sealed: String,
    }
    let now = remote::epoch();
    let page = st.cfg.forms.page();
    match (req.method.as_str(), path) {
        ("POST", ["api", "forms"]) => {
            let b: Ask = req.json()?;
            if matches!(b.request.target, crate::sealed::Target::Runtime { .. }) {
                return Err(HttpError(
                    400,
                    "runtime logins start from the owner's Telegram /runtimes".into(),
                )
                .into());
            }
            let bot =
                match (b.telegram, b.bot.as_deref()) {
                    (Some(false), _) => None,
                    (_, None) => return Err(HttpError(
                        400,
                        "name the Telegram bot that sends the prompt (bot), or set telegram false"
                            .into(),
                    )
                    .into()),
                    (_, Some(bot)) => {
                        form_sender(st, bot).map_err(|e| HttpError(400, format!("{e:#}")))?;
                        Some(bot)
                    }
                };
            let f = sign_form(st, &b.request, now)?;
            let mut out = f.view(page);
            out["telegram"] = match bot {
                Some(bot) => form_prompt(st, &f, bot),
                None => json!({"sent": false, "reason": "not asked"}),
            };
            let fields: Vec<String> = f
                .spec
                .fields
                .iter()
                .map(|x| format!("{} ({})", x.name, x.kind.as_str()))
                .collect();
            form_event(
                st,
                &f,
                &format!(
                    "asked: {}; fields {}; expires {}",
                    f.spec.purpose,
                    fields.join(", "),
                    f.spec.expires
                ),
            );
            eprintln!(
                "axe server: form {}: asked for {}",
                f.spec.form,
                f.spec.target.describe()
            );
            ok(201, &out)
        }
        ("GET", ["api", "forms"]) => ok(
            200,
            &st.forms
                .list(now)
                .iter()
                .map(|f| f.view(page))
                .collect::<Vec<_>>(),
        ),
        ("GET", ["api", "forms", id]) => {
            let wait = match req.query("wait") {
                Some(w) => w
                    .parse::<u64>()
                    .map_err(|_| HttpError(400, format!("invalid wait {w:?}")))?
                    .min(FORM_WAIT_MAX),
                None => 0,
            };
            let f = if wait > 0 {
                st.forms.wait(id, Duration::from_secs(wait), remote::epoch)
            } else {
                st.forms.get(id, now)
            };
            match f {
                Some(f) => ok(200, &f.view(page)),
                None => Err(HttpError(
                    404,
                    format!("no form {id} (unknown, or finished over an hour ago)"),
                )
                .into()),
            }
        }
        ("POST", ["api", "forms", id, "submit"]) => {
            let b: Submit = req.json()?;
            let parsed = crate::sealed::Sealed::parse(&b.sealed)
                .map_err(|e| HttpError(400, format!("{e:#}")))?;
            if parsed.form != *id {
                return Err(HttpError(400, "the sealed reply is for another form".into()).into());
            }
            let f = channel::Store::form_submit(st, &b.sealed)
                .map_err(|e| HttpError(409, format!("{e:#}")))?;
            ok(200, &f.view(page))
        }
        _ => Err(HttpError(
            404,
            format!("no route for {} /{}", req.method, path.join("/")),
        )
        .into()),
    }
}

/// Validates `req`, has the dispatcher sign it and keeps the form.
fn sign_form(st: &State, req: &crate::sealed::Request, now: i64) -> Result<form::Form> {
    req.validate()
        .map_err(|e| HttpError(400, format!("{e:#}")))?;
    let job = json!({"kind": "form_sign", "request": req});
    let v = relay_job(st, job, None, FORM_RELAY_TIMEOUT)
        .map_err(|e| HttpError(503, format!("cannot sign the form: {e}")))?;
    if let Some(e) = v["error"].as_str() {
        return Err(HttpError(400, format!("the dispatcher refused the form: {e}")).into());
    }
    let signed: crate::sealed::Signed = serde_json::from_value(v["signed"].clone())
        .map_err(|e| HttpError(502, format!("invalid signed form: {e}")))?;
    Ok(st
        .forms
        .add(req, signed, now)
        .map_err(|e| HttpError(502, format!("{e:#}")))?)
}

/// A login of a dispatcher runtime started from the owner's Telegram: a
/// sealed form for a credential, or the device code the CLI printed.
fn runtime_login(st: &State, bot: &str, runtime: &str, method: &str) -> Result<String> {
    let view = crate::runtime::view(&st.admission());
    let harness = view["runtimes"]
        .as_array()
        .and_then(|l| l.iter().find(|r| r["name"] == runtime))
        .and_then(|r| r["harness"].as_str())
        .with_context(|| format!("the dispatcher reports no runtime {runtime}"))?
        .to_string();
    let Some((_, _, label)) = crate::runtime::login_methods(&harness)
        .iter()
        .find(|(m, _, _)| *m == method)
    else {
        bail!("runtime {runtime} ({harness}) has no {method} login");
    };
    if method == "device" {
        let job = json!({"kind": "runtime_login", "runtime": runtime, "method": method});
        let v = relay_job(st, job, None, FORM_RELAY_TIMEOUT).map_err(anyhow::Error::msg)?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        eprintln!("axe server: runtime {runtime}: device login started");
        return Ok(format!(
            "Log in runtime {runtime} ({harness}) with a device code: open the link below and              enter the code. The runtime is ok again at its next login check.\n\n{}",
            v["output"].as_str().unwrap_or("")
        ));
    }
    let req = crate::sealed::Request {
        purpose: format!("Log in runtime {runtime} ({harness}) with a {label}"),
        target: crate::sealed::Target::Runtime {
            runtime: runtime.into(),
            login: method.into(),
        },
        fields: vec![crate::sealed::Field {
            name: "secret".into(),
            label: (*label).to_string(),
            kind: crate::sealed::Kind::Secret,
            required: true,
            max: None,
            choices: vec![],
            env: None,
        }],
        ttl: None,
        from: "/runtimes".into(),
    };
    let f = sign_form(st, &req, remote::epoch())?;
    let sent = form_prompt(st, &f, bot);
    eprintln!(
        "axe server: form {}: asked for {}",
        f.spec.form,
        f.spec.target.describe()
    );
    match sent["sent"].as_bool() {
        Some(true) => Ok(format!(
            "Sealed form sent for runtime {runtime}: tap \"Open sealed form\"."
        )),
        _ => bail!(
            "the form could not be sent: {}",
            sent["reason"].as_str().unwrap_or("unknown")
        ),
    }
}

/// Checks that bot `name` can send a form's Telegram prompt.
#[cfg(feature = "telegram")]
fn form_sender(st: &State, name: &str) -> Result<()> {
    let gw = st
        .telegram
        .as_ref()
        .context("telegram is not configured on this server")?;
    gw.form_sender(name).map(|_| ())
}

#[cfg(not(feature = "telegram"))]
fn form_sender(_: &State, _: &str) -> Result<()> {
    bail!(NO_TELEGRAM)
}

/// Sends the form's Telegram prompt from bot `name`; returns what happened.
#[cfg(feature = "telegram")]
fn form_prompt(st: &State, f: &form::Form, name: &str) -> serde_json::Value {
    let Some(gw) = &st.telegram else {
        return json!({"sent": false, "reason": "telegram is not configured on this server"});
    };
    let page = st.cfg.forms.page();
    // The prompt is recorded before it is sent, so a form closing meanwhile
    // keeps this one's button in the keyboard it re-sends.
    let open = |at: &form::Prompt| form::buttons(&st.forms.prompt_at(&f.spec.form, at), page);
    match gw.form_prompt(st, name, f, &crate::sealed::link(page, &f.signed), open) {
        Ok(v) => v,
        Err(e) => {
            st.forms.clear_prompt(&f.spec.form);
            json!({"sent": false, "reason": format!("{e:#}")})
        }
    }
}

#[cfg(not(feature = "telegram"))]
fn form_prompt(_: &State, _: &form::Form, _: &str) -> serde_json::Value {
    json!({"sent": false, "reason": NO_TELEGRAM})
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
