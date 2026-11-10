//! Starts `axe server` on an ephemeral port and drives the real CLI against it.
#![allow(dead_code)]

pub mod gate;

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

pub const BIN: &str = env!("CARGO_BIN_EXE_axe");
pub const TOKEN: &str = "test-token-0123456789abcdef";

pub struct Server {
    pub url: String,
    pub child: Child,
    pub db: PathBuf,
    pub config: PathBuf,
    pub token_file: PathBuf,
}

/// Drops the old-name variables of the environment the tests run in, so
/// the compatibility import never fills in an `AXE_*` a test left unset.
pub fn isolate(c: &mut Command) -> &mut Command {
    for n in axe::compat::ENV_NAMES {
        c.env_remove(format!("{}{n}", axe::compat::OLD_ENV_PREFIX));
    }
    c
}

/// Base server command with an isolated db, config, and token file.
pub fn server_cmd(db: &Path, config: &Path, token_file: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(BIN);
    isolate(&mut c);
    c.arg("server")
        .args(args)
        .env("AXE_DB", db)
        .env("AXE_CONFIG", config)
        .env("AXE_TOKEN_FILE", token_file)
        .env("AXE_BIN", BIN)
        .env(
            "AXE_ADMISSION_HOLD",
            db.with_file_name("admission-hold.json"),
        )
        .env_remove("AXE_ACTOR")
        .env_remove("AXE_SESSION")
        .env_remove("AXE_TASK")
        .env_remove("AXE_SERVER")
        .env_remove("AXE_TOKEN")
        .env_remove("OTEL_RESOURCE_ATTRIBUTES");
    c
}

impl Server {
    /// Starts a server with `extra` args (e.g. `--once`) and `env` overrides.
    pub fn start_with(
        dir: &Path,
        config: &Path,
        token: Option<&str>,
        extra: &[&str],
        env: &[(&str, &str)],
    ) -> Server {
        let db = dir.join("t.db");
        let token_file = dir.join("token");
        match token {
            Some(t) => std::fs::write(&token_file, format!("{t}\n")).unwrap(),
            None => {
                let _ = std::fs::remove_file(&token_file);
            }
        }
        let mut args = vec!["--listen", "127.0.0.1:0"];
        args.extend_from_slice(extra);
        let mut cmd = server_cmd(&db, config, &token_file, &args);
        cmd.envs(env.iter().copied())
            .current_dir(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("axe server listening on ")
            .unwrap_or_else(|| panic!("unexpected server output {line:?}"))
            .to_string();
        Server {
            url,
            child,
            db,
            config: config.to_path_buf(),
            token_file,
        }
    }

    /// A server with the default config and the test token.
    pub fn start(dir: &Path) -> Server {
        let config = dir.join("config.toml");
        std::fs::write(&config, "").unwrap();
        Server::start_with(dir, &config, Some(TOKEN), &[], &[])
    }

    pub fn dispatcher(&self, args: &[&str], env: &[(&str, &str)]) -> Worker {
        let mut c = self.cli(&["dispatch"]);
        c.args(args)
            .env("AXE_CONFIG", &self.config)
            .env("AXE_DB", "/nonexistent/forbidden.sqlite")
            .env("AXE_BIN", BIN)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Worker(c.spawn().unwrap())
    }
    /// CLI command pointed at this server; writes authenticate via the token file.
    pub fn cli(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        isolate(&mut c);
        c.args(args)
            .env("AXE_SERVER", &self.url)
            .env("AXE_TOKEN_FILE", &self.token_file)
            .env(
                "AXE_ADMISSION_HOLD",
                self.db.with_file_name("admission-hold.json"),
            )
            .env(
                "AXE_CONFIG",
                self.config.with_file_name("client-missing.toml"),
            )
            .env_remove("AXE_DB")
            .env_remove("AXE_ACTOR")
            .env_remove("AXE_SESSION")
            .env_remove("AXE_TOKEN");
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.cli(args).output().unwrap()
    }

    /// Runs the CLI and returns stdout, asserting success.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn get(&self, path: &str) -> serde_json::Value {
        ureq::get(&format!("{}{path}", self.url))
            .call()
            .unwrap()
            .into_json()
            .unwrap()
    }

    /// Waits for a `--once` server to exit and returns its output.
    pub fn wait(mut self) -> Output {
        let status = self.child.wait().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.child
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        Output {
            status,
            stdout,
            stderr,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Worker(pub Child);
impl Worker {
    pub fn wait(mut self) -> Output {
        let status = self.0.wait().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.0
            .stdout
            .take()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        self.0
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        Output {
            status,
            stdout,
            stderr,
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Remote log fixture: owns files in the test's dispatcher directory, not in the API.
pub struct LogRelay {
    pub id: String,
    pub generation: String,
    pub logs: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<i64, (PathBuf, bool)>>>,
    socket: std::net::TcpStream,
}
impl LogRelay {
    pub fn start(srv: &Server) -> Self {
        use tungstenite::client::IntoClientRequest;
        let id = "fixture".to_string();
        let r: serde_json::Value = ureq::post(&format!("{}/api/dispatcher/register", srv.url))
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .send_json(serde_json::json!({"id":id,"generation":""}))
            .unwrap()
            .into_json()
            .unwrap();
        let generation = r["generation"].as_str().unwrap().to_string();
        let mut req = format!(
            "{}/api/dispatcher/connect?id={id}&generation={generation}",
            srv.url.replacen("http", "ws", 1)
        )
        .into_client_request()
        .unwrap();
        req.headers_mut()
            .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let (mut ws, _) = tungstenite::connect(req).unwrap();
        let socket = match ws.get_ref() {
            tungstenite::stream::MaybeTlsStream::Plain(s) => s.try_clone().unwrap(),
            _ => panic!("local socket"),
        };
        let logs = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            i64,
            (PathBuf, bool),
        >::new()));
        let shared = logs.clone();
        std::thread::spawn(move || {
            while let Ok(m) = ws.read() {
                if !m.is_text() {
                    continue;
                }
                let q: serde_json::Value = serde_json::from_str(m.to_text().unwrap()).unwrap();
                let response = if q["kind"] == "log" {
                    let entry = shared
                        .lock()
                        .unwrap()
                        .get(&q["run"].as_i64().unwrap())
                        .cloned();
                    match entry {
                        Some((path, done)) => match axe::run_log::read(
                            &path,
                            q["after"].as_u64(),
                            q["before"].as_u64(),
                            q["generation"].as_str(),
                            done,
                        ) {
                            Ok(page) => serde_json::json!({"page":page,"log_complete":done}),
                            Err(e) => serde_json::json!({"unavailable":e.to_string()}),
                        },
                        None => serde_json::json!({"unavailable":"unknown run"}),
                    }
                } else {
                    serde_json::json!({"ok":true})
                };
                if ws
                    .send(tungstenite::Message::text(response.to_string()))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            id,
            generation,
            logs,
            socket,
        }
    }
    pub fn add(&self, conn: &rusqlite::Connection, id: i64, path: &Path, done: bool) {
        conn.execute(
            "INSERT OR REPLACE INTO dispatch_runs VALUES(?1,?2,?3,'running')",
            rusqlite::params![id, self.id, self.generation],
        )
        .unwrap();
        self.logs
            .lock()
            .unwrap()
            .insert(id, (path.to_owned(), done));
    }
}
impl Drop for LogRelay {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}
