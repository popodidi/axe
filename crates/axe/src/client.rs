//! HTTP client for `axe server`, used by every CLI command except `server`
//! and `config`.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config;

/// `axe server`'s default listen address.
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8790";
/// How long the CLI rides out a server outage (a restart is ~30-90s).
pub const DEFAULT_RETRY_SECS: u64 = 120;
/// Header carrying a write's client-generated id; see `server::replayable`.
pub const REQUEST_ID_HEADER: &str = "X-Axe-Request-Id";

#[derive(Clone)]
pub struct Client {
    base: String,
    agent: ureq::Agent,
    actor: String,
    session: Option<String>,
    /// Explicit token instead of `$AXE_TOKEN` or the token file.
    token: Option<String>,
    /// Retry budget for [`retryable`] errors; zero means a single attempt.
    retry: Duration,
}

/// `$AXE_RETRY_SECS`, else [`DEFAULT_RETRY_SECS`]; `0` disables retries.
pub fn retry_budget() -> Duration {
    Duration::from_secs(
        env_nonempty("AXE_RETRY_SECS")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_RETRY_SECS),
    )
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

impl Client {
    /// Server from `$AXE_SERVER`, session from `$AXE_SESSION`.
    pub fn from_env(actor: String) -> Client {
        let base = env_nonempty("AXE_SERVER").unwrap_or_else(|| DEFAULT_SERVER.into());
        Self::at(base, actor)
    }
    pub fn at(base: String, actor: String) -> Client {
        Client {
            base: base.trim_end_matches('/').to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .build(),
            actor,
            session: env_nonempty("AXE_SESSION"),
            token: None,
            retry: Duration::ZERO,
        }
    }

    /// Retries transient errors ([`retryable`]) with backoff for up to
    /// `budget`. Each write carries one request id across its attempts so the
    /// server applies it once.
    pub fn retrying(mut self, budget: Duration) -> Client {
        self.retry = budget;
        self
    }

    /// Runs `attempt` until it succeeds, fails non-retryably, or the retry
    /// budget runs out. Writes get a request id, the same on every attempt.
    fn attempts<T>(
        &self,
        write: bool,
        mut attempt: impl FnMut(Option<&str>) -> Result<T>,
    ) -> Result<T> {
        let id = (write && !self.retry.is_zero()).then(|| uuid::Uuid::new_v4().to_string());
        let deadline = Instant::now() + self.retry;
        let mut delay = Duration::from_millis(500);
        let mut warned = false;
        loop {
            match attempt(id.as_deref()) {
                Err(e) if retryable(&e) && Instant::now() + delay <= deadline => {
                    if !warned {
                        eprintln!(
                            "axe: server unavailable ({e}); retrying for up to {}s",
                            self.retry.as_secs()
                        );
                        warned = true;
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(8));
                }
                r => return r,
            }
        }
    }

    /// Uses `token` for writes instead of the environment or token file.
    pub fn with_token(mut self, token: &str) -> Client {
        self.token = Some(token.to_string());
        self
    }

    /// `$AXE_TOKEN`, else the token file.
    pub fn token(&self) -> Result<Option<String>> {
        if let Some(t) = &self.token {
            return Ok(Some(t.clone()));
        }
        match env_nonempty("AXE_TOKEN") {
            Some(t) => Ok(Some(t.trim().to_string())),
            None => config::read_token(&config::token_path()?),
        }
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        self.attempts(false, |_| {
            let mut req = self.agent.get(&format!("{}{path}", self.base));
            for (k, v) in query {
                req = req.query(k, v);
            }
            self.finish(req.call())
        })
    }

    /// Sends a write with the actor, session, and bearer token.
    pub fn send<T: DeserializeOwned>(&self, method: &str, path: &str, body: Value) -> Result<T> {
        self.send_timeout(method, path, body, None)
    }

    /// [`Client::send`] with a per-request timeout (slow server-side work,
    /// such as paced Telegram sends).
    pub fn send_timeout<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        mut body: Value,
        timeout: Option<Duration>,
    ) -> Result<T> {
        body["actor"] = Value::from(self.actor.as_str());
        body["session"] = self.session.as_deref().map_or(Value::Null, Value::from);
        self.attempts(true, |id| {
            let mut req = self.agent.request(method, &format!("{}{path}", self.base));
            if let Some(t) = timeout {
                req = req.timeout(t);
            }
            if let Some(token) = self.token()? {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
            if let Some(id) = id {
                req = req.set(REQUEST_ID_HEADER, id);
            }
            self.finish(req.send_json(body.clone()))
        })
    }

    /// A GET that needs the token (message content).
    pub fn get_private<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T> {
        self.attempts(false, |_| {
            let mut req = self.agent.get(&format!("{}{path}", self.base));
            for (k, v) in query {
                req = req.query(k, v);
            }
            if let Some(token) = self.token()? {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
            self.finish(req.call())
        })
    }

    /// Downloads a file route: (percent-decoded `X-Axe-File-Name`, bytes).
    pub fn get_file(&self, path: &str) -> Result<(Option<String>, Vec<u8>)> {
        self.attempts(false, |_| {
            let mut req = self.agent.get(&format!("{}{path}", self.base));
            if let Some(token) = self.token()? {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
            match req.call() {
                Ok(resp) => {
                    let name = resp
                        .header("X-Axe-File-Name")
                        .map(|n| crate::server::pct_decode(n, false));
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes)?;
                    Ok((name, bytes))
                }
                Err(e) => self.finish(Err(e)),
            }
        })
    }

    /// POSTs raw bytes; the actor and session go in the query.
    pub fn upload<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        bytes: &[u8],
    ) -> Result<T> {
        self.attempts(true, |id| {
            let mut req = self
                .agent
                .post(&format!("{}{path}", self.base))
                .timeout(Duration::from_secs(600))
                .set("Content-Type", "application/octet-stream")
                .query("actor", &self.actor);
            if let Some(s) = &self.session {
                req = req.query("session", s);
            }
            for (k, v) in query {
                req = req.query(k, v);
            }
            if let Some(token) = self.token()? {
                req = req.set("Authorization", &format!("Bearer {token}"));
            }
            if let Some(id) = id {
                req = req.set(REQUEST_ID_HEADER, id);
            }
            self.finish(req.send_bytes(bytes))
        })
    }

    pub fn backup(&self, to: &std::path::Path) -> Result<()> {
        if to.exists() {
            bail!("backup target {} already exists", to.display());
        }
        let mut req = self.agent.post(&format!("{}/api/backup", self.base));
        if let Some(token) = self.token()? {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        let response = req.call()?;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(to)?;
        let result =
            std::io::copy(&mut response.into_reader(), &mut file).and_then(|_| file.sync_all());
        if result.is_err() {
            let _ = std::fs::remove_file(to);
        }
        result?;
        Ok(())
    }
    pub fn base(&self) -> &str {
        &self.base
    }

    fn finish<T: DeserializeOwned>(
        &self,
        r: std::result::Result<ureq::Response, ureq::Error>,
    ) -> Result<T> {
        match r {
            Ok(resp) => resp.into_json().map_err(|e| {
                ClientError {
                    code: None,
                    ingress_unavailable: false,
                    message: format!("incomplete server response: {e}"),
                }
                .into()
            }),
            Err(ureq::Error::Status(code, resp)) => {
                let server_error = resp
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| v["error"].as_str().map(String::from));
                // Traefik returns a plain 404 while a Recreate service has no
                // endpoints. Keep semantic axe JSON errors non-retryable.
                let ingress_unavailable = code == 404 && server_error.is_none();
                let msg = server_error.unwrap_or_else(|| format!("HTTP {code}"));
                if code == 401 {
                    bail!("{msg}: set AXE_TOKEN or AXE_TOKEN_FILE to the server token");
                }
                Err(ClientError {
                    code: Some(code),
                    ingress_unavailable,
                    message: msg,
                }
                .into())
            }
            Err(e) => Err(ClientError {
                code: None,
                ingress_unavailable: false,
                message: format!("cannot reach axe server at {}: {e}", self.base),
            }
            .into()),
        }
    }
}

#[derive(Debug)]
pub struct ClientError {
    pub code: Option<u16>,
    pub ingress_unavailable: bool,
    pub message: String,
}
impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ClientError {}
pub fn retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ClientError>()
        .is_some_and(|e| e.ingress_unavailable || e.code.is_none_or(|c| c >= 500))
}
