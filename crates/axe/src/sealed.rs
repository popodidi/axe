//! Sealed forms: a one-off form that carries input from any channel to the
//! dispatcher, which alone can read it.
//!
//! - The node holds `dispatcher.key`, a P-256 private key (0600, made
//!   on first use). Its public key and fingerprint are not secret.
//! - A form [`Spec`] (purpose, typed fields, target, expiry) is made and
//!   signed (ECDSA P-256, SHA-256) by the node; the server only relays
//!   it. The link to the static form page carries it in the URL fragment as
//!   `f=<base64url spec>.<base64url signature>`.
//! - The page encrypts the values as one JSON object to the node key:
//!   ephemeral ECDH P-256, HKDF-SHA256 (salt: ephemeral then dispatcher
//!   public key, info [`HKDF_INFO`]) and AES-256-GCM with
//!   `form|dispatcher|expires` as associated data. The result is the text
//!   `sealed.v1.<form>.<base64url(ephemeral public key | iv | ciphertext)>`
//!   ([`Sealed`]), which any channel can carry back.
//! - The node checks its own signature, the expiry and that the form
//!   was not used, decrypts, checks the values against the fields and writes
//!   them to the signed target ([`Desk`]). A value never leaves the
//!   node host and is never logged.
use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aes_gcm::aead::{Aead, Payload as Payload_};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

pub const VERSION: u32 = 1;
/// The prefix of a sealed reply, before the form id.
pub const SEALED_PREFIX: &str = "sealed.v1.";
pub const HKDF_INFO: &[u8] = b"sealed-form v1";
/// A form's life when the request names none.
pub const DEFAULT_TTL: i64 = 15 * 60;
pub const MIN_TTL: i64 = 60;
pub const MAX_TTL: i64 = 60 * 60;
pub const MAX_FIELDS: usize = 16;
const MAX_PURPOSE: usize = 200;
const MAX_LABEL: usize = 64;
const MAX_CHOICES: usize = 20;
/// Largest `max` a field may declare (bytes of its value).
pub const MAX_VALUE: usize = 64 * 1024;
/// Largest sealed reply accepted, comfortably above [`MAX_FIELDS`] x [`MAX_VALUE`]
/// is not needed: a reply over this is refused before decryption.
pub const MAX_SEALED: usize = 256 * 1024;
const PUBKEY_LEN: usize = 65;
const IV_LEN: usize = 12;
/// Environment variables a field may never set on a run.
const RESERVED_ENV: [&str; 12] = [
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "PWD",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "OTEL_RESOURCE_ATTRIBUTES",
    "IFS",
    "ENV",
    "BASH_ENV",
    "NODE_OPTIONS",
];

// ---------------------------------------------------------------- the spec

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Masked input.
    Secret,
    Text,
    /// A short one-time code.
    Code,
    /// One of `choices`.
    Choice,
    /// A file, sent as `{"name": ..., "data": <base64>}`.
    File,
}

impl Kind {
    fn default_max(self) -> usize {
        match self {
            Kind::Secret => 4096,
            Kind::Text => 1024,
            Kind::Code => 64,
            Kind::Choice => 64,
            // A Telegram Mini App reply is at most 4096 bytes in all.
            Kind::File => 2048,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Secret => "secret",
            Kind::Text => "text",
            Kind::Code => "code",
            Kind::Choice => "choice",
            Kind::File => "file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// The value's file name in the target: `[a-z][a-z0-9_]{0,31}`.
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Most bytes of the value (a file's decoded size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// For a task or bot target: also set as this environment variable on
    /// the target's later runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

impl Field {
    pub fn max(&self) -> usize {
        self.max.unwrap_or_else(|| self.kind.default_max())
    }

    /// Parses `NAME:TYPE[:FLAGS...]` from the CLI, for example
    /// `token:secret:required:env=GH_TOKEN`, `plan:choice=max|pro`,
    /// `cert:file:max=4096`, `note:text:label=A note`.
    pub fn parse(s: &str) -> Result<Field> {
        let mut parts = s.split(':');
        let name = parts.next().unwrap_or("").to_string();
        let ty = parts
            .next()
            .with_context(|| format!("field {s:?}: expected NAME:TYPE"))?;
        let (ty, choices) = match ty.split_once('=') {
            Some((t, c)) => (t, c.split('|').map(String::from).collect()),
            None => (ty, Vec::new()),
        };
        let kind: Kind = serde_json::from_value(json!(ty)).map_err(|_| {
            anyhow::anyhow!("field {name}: unknown type {ty:?} (secret, text, code, choice, file)")
        })?;
        let mut f = Field {
            name,
            label: String::new(),
            kind,
            required: false,
            max: None,
            choices,
            env: None,
        };
        for flag in parts {
            match flag.split_once('=') {
                None if flag == "required" => f.required = true,
                Some(("max", n)) => {
                    f.max = Some(
                        n.parse()
                            .with_context(|| format!("field {}: max {n:?}", f.name))?,
                    )
                }
                Some(("env", v)) => f.env = Some(v.to_string()),
                Some(("label", v)) => f.label = v.to_string(),
                _ => bail!(
                    "field {}: unknown flag {flag:?} (required, max=N, env=VAR, label=TEXT)",
                    f.name
                ),
            }
        }
        f.validate()?;
        Ok(f)
    }

    pub fn validate(&self) -> Result<()> {
        let n = &self.name;
        let ok_name = n.len() <= 32
            && n.starts_with(|c: char| c.is_ascii_lowercase())
            && n.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !ok_name {
            bail!("field name {n:?}: use [a-z][a-z0-9_]{{0,31}}");
        }
        if self.label.chars().count() > MAX_LABEL {
            bail!("field {n}: label over {MAX_LABEL} characters");
        }
        let max = self.max();
        if max == 0 || max > MAX_VALUE {
            bail!("field {n}: max must be 1..={MAX_VALUE} bytes");
        }
        match self.kind {
            Kind::Choice => {
                if self.choices.is_empty() || self.choices.len() > MAX_CHOICES {
                    bail!("field {n}: a choice needs 1..={MAX_CHOICES} choices");
                }
                if self
                    .choices
                    .iter()
                    .any(|c| c.is_empty() || c.chars().count() > MAX_LABEL)
                {
                    bail!("field {n}: each choice is 1..={MAX_LABEL} characters");
                }
            }
            _ if !self.choices.is_empty() => bail!("field {n}: only a choice has choices"),
            _ => {}
        }
        if let Some(env) = &self.env {
            check_env_name(env).with_context(|| format!("field {n}"))?;
            if self.kind == Kind::File {
                bail!("field {n}: a file cannot be an environment variable");
            }
        }
        Ok(())
    }
}

/// An environment variable a form may set on a run.
pub fn check_env_name(env: &str) -> Result<()> {
    let ok = !env.is_empty()
        && env.len() <= 64
        && env.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && env
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        bail!("env {env:?}: use [A-Z_][A-Z0-9_]{{0,63}}");
    }
    if RESERVED_ENV.contains(&env)
        || env.starts_with("AXE_")
        || env.starts_with("LD_")
        || env.starts_with("DYLD_")
    {
        bail!("env {env:?} is reserved");
    }
    Ok(())
}

/// Where the values go. The node writes only to the target inside the
/// spec it signed, so a relay cannot redirect them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Target {
    /// Files in the task's secrets folder (`AXE_SECRETS_DIR`).
    Task { task: i64 },
    /// Files in the bot's secrets folder (`AXE_SECRETS_DIR`).
    Bot { bot: String },
    /// A git credential for `https://<host>` (fields `token`, optional
    /// `username`), used by the node's clones and fetches.
    Git { host: String },
    /// A login of one of the node's runtimes (`runtime::login_methods`);
    /// one field, `secret`.
    Runtime { runtime: String, login: String },
    /// The owner's reply key for sealed messages back to the owner
    /// ([`ReplyKey`]); one field, `passphrase`.
    Reply,
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::Task { task } => format!("task {task}"),
            Target::Bot { bot } => format!("bot {bot}"),
            Target::Git { host } => format!("git credential for {host}"),
            Target::Runtime { runtime, login } => format!("runtime {runtime} login ({login})"),
            Target::Reply => "the owner's reply key".into(),
        }
    }

    fn validate(&self, fields: &[Field]) -> Result<()> {
        match self {
            Target::Task { task } if *task <= 0 => bail!("invalid task {task}"),
            Target::Task { .. } => Ok(()),
            Target::Bot { bot } => crate::db::validate_name("bot", bot),
            Target::Git { host } => {
                check_host(host)?;
                let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                if !names.contains(&"token")
                    || names.iter().any(|n| !matches!(*n, "token" | "username"))
                {
                    bail!("a git credential form has the fields token and, optionally, username");
                }
                if fields.iter().any(|f| f.env.is_some()) {
                    bail!("a git credential is not an environment variable");
                }
                Ok(())
            }
            Target::Runtime { runtime, login } => {
                crate::db::validate_name("runtime", runtime)?;
                if !matches!(login.as_str(), "api_key" | "token") {
                    bail!("unknown runtime login {login:?}");
                }
                match fields {
                    [f] if f.name == "secret"
                        && f.kind == Kind::Secret
                        && f.required
                        && f.env.is_none() =>
                    {
                        Ok(())
                    }
                    _ => bail!("a runtime login form has one required secret field, secret"),
                }
            }
            Target::Reply => match fields {
                [f] if f.name == "passphrase"
                    && f.kind == Kind::Secret
                    && f.required
                    && f.env.is_none() =>
                {
                    Ok(())
                }
                _ => bail!("a reply key form has one required secret field, passphrase"),
            },
        }
    }
}

/// A git host name: letters, digits, dots, dashes and an optional port.
pub fn check_host(host: &str) -> Result<()> {
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        && !host.starts_with(['.', '-', ':']);
    if !ok {
        bail!("invalid git host {host:?}");
    }
    Ok(())
}

/// What a caller asks for; the node turns it into a signed [`Spec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub purpose: String,
    pub target: Target,
    pub fields: Vec<Field>,
    /// Seconds the form stays open ([`DEFAULT_TTL`] when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<i64>,
    /// Who asked, for the page: `task 12`, `bot ops`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
}

impl Request {
    /// Form schema validation; the server and the node both run it.
    pub fn validate(&self) -> Result<()> {
        let p = self.purpose.trim();
        if p.is_empty() || p.chars().count() > MAX_PURPOSE {
            bail!("purpose must be 1..={MAX_PURPOSE} characters");
        }
        if self.from.chars().count() > MAX_LABEL {
            bail!("from over {MAX_LABEL} characters");
        }
        if self.fields.is_empty() || self.fields.len() > MAX_FIELDS {
            bail!("a form has 1..={MAX_FIELDS} fields");
        }
        let mut names = std::collections::BTreeSet::new();
        let mut envs = std::collections::BTreeSet::new();
        for f in &self.fields {
            f.validate()?;
            if !names.insert(f.name.as_str()) {
                bail!("field {} appears twice", f.name);
            }
            if let Some(e) = &f.env {
                if !envs.insert(e.as_str()) {
                    bail!("env {e} appears twice");
                }
            }
        }
        if !self.fields.iter().any(|f| f.required) {
            bail!("a form needs at least one required field");
        }
        if let Some(t) = self.ttl {
            if !(MIN_TTL..=MAX_TTL).contains(&t) {
                bail!("ttl must be {MIN_TTL}..={MAX_TTL} seconds");
            }
        }
        self.target.validate(&self.fields)
    }
}

/// The signed form the page renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub v: u32,
    /// 128 random bits, hex.
    pub form: String,
    /// The node's id.
    pub dispatcher: String,
    /// The node's public key (uncompressed SEC1, base64url).
    pub pubkey: String,
    /// RFC 3339, UTC, whole seconds.
    pub expires: String,
    pub purpose: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    pub target: Target,
    pub fields: Vec<Field>,
}

impl Spec {
    pub fn expires_at(&self) -> Result<i64> {
        parse_time(&self.expires)
    }

    /// The associated data every sealed reply to this form is bound to.
    pub fn aad(&self) -> String {
        format!("{}|{}|{}", self.form, self.dispatcher, self.expires)
    }
}

/// A spec as signed: the exact JSON bytes and the signature, both base64url.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    pub spec: String,
    pub sig: String,
}

impl Signed {
    /// The link fragment value: `<spec>.<sig>`.
    pub fn token(&self) -> String {
        format!("{}.{}", self.spec, self.sig)
    }

    /// Decodes the spec without checking the signature (the server's view).
    pub fn decode(&self) -> Result<Spec> {
        let bytes = B64.decode(&self.spec).context("spec is not base64url")?;
        serde_json::from_slice(&bytes).context("invalid form spec")
    }
}

/// The form page link: `<page>#f=<spec>.<sig>`.
pub fn link(page: &str, signed: &Signed) -> String {
    let page = page.split('#').next().unwrap_or(page);
    format!("{page}#f={}", signed.token())
}

pub fn parse_time(s: &str) -> Result<i64> {
    Ok(
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .with_context(|| format!("invalid time {s:?}"))?
            .unix_timestamp(),
    )
}

pub fn format_time(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

// ------------------------------------------------------------ sealed reply

/// `sealed.v1.<form>.<base64url>`: what a channel carries back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub form: String,
    /// Ephemeral public key, IV and AES-GCM ciphertext with its tag.
    pub blob: Vec<u8>,
}

impl Sealed {
    pub fn parse(s: &str) -> Result<Sealed> {
        let s = s.trim();
        if s.len() > MAX_SEALED {
            bail!("sealed reply over {MAX_SEALED} bytes");
        }
        let rest = s
            .strip_prefix(SEALED_PREFIX)
            .context("not a sealed reply")?;
        let (form, data) = rest.split_once('.').context("sealed reply without data")?;
        if !is_form_id(form) {
            bail!("sealed reply with an invalid form id");
        }
        let blob = B64.decode(data).context("sealed reply is not base64url")?;
        if blob.len() < PUBKEY_LEN + IV_LEN + 16 {
            bail!("sealed reply too short");
        }
        Ok(Sealed {
            form: form.to_string(),
            blob,
        })
    }

    pub fn text(&self) -> String {
        format!("{SEALED_PREFIX}{}.{}", self.form, B64.encode(&self.blob))
    }

    /// True when `s` looks like a sealed reply (cheap; [`Sealed::parse`] checks).
    pub fn looks_like(s: &str) -> bool {
        s.trim_start().starts_with(SEALED_PREFIX)
    }
}

pub fn is_form_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn new_form_id() -> String {
    let mut b = [0u8; 16];
    getrandom(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn getrandom(buf: &mut [u8]) {
    use rand_core::RngCore;
    rand_core::OsRng.fill_bytes(buf);
}

fn derive_key(shared: &p256::ecdh::SharedSecret, eph: &[u8], dispatcher: &[u8]) -> [u8; 32] {
    let salt = [eph, dispatcher].concat();
    let hk = shared.extract::<sha2_10::Sha256>(Some(&salt));
    let mut key = [0u8; 32];
    hk.expand(HKDF_INFO, &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    key
}

/// Encrypts `values` (a JSON object) to the form, as the page does. The
/// dispatcher never calls it; tests and `axe form seal` do.
pub fn seal(spec: &Spec, values: &Value) -> Result<Sealed> {
    let pub_raw = B64.decode(&spec.pubkey).context("spec pubkey")?;
    let recipient = PublicKey::from_sec1_bytes(&pub_raw).context("spec pubkey")?;
    let eph = p256::ecdh::EphemeralSecret::random(&mut rand_core::OsRng);
    let eph_raw = eph.public_key().to_encoded_point(false);
    let key = derive_key(
        &eph.diffie_hellman(&recipient),
        eph_raw.as_bytes(),
        &pub_raw,
    );
    let mut iv = [0u8; IV_LEN];
    getrandom(&mut iv);
    let aad = spec.aad();
    let ct = Aes256Gcm::new_from_slice(&key)
        .expect("32-byte key")
        .encrypt(
            &Nonce::from(iv),
            Payload_ {
                msg: &serde_json::to_vec(values)?,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    Ok(Sealed {
        form: spec.form.clone(),
        blob: [eph_raw.as_bytes(), &iv, &ct].concat(),
    })
}

// --------------------------------------------------------------- the key

/// The node's key pair.
pub struct Key {
    secret: SecretKey,
}

impl Key {
    /// Reads the key at `path`, or makes one (0600) when there is none. A key
    /// file that others can read is refused.
    pub fn load_or_create(path: &Path) -> Result<Key> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mode = std::fs::metadata(path)?.permissions().mode();
                if mode & 0o077 != 0 {
                    bail!(
                        "{} is readable by others (mode {:o}); chmod 600 it",
                        path.display(),
                        mode & 0o777
                    );
                }
                let bytes = B64
                    .decode(text.trim())
                    .with_context(|| format!("{}: not a node key", path.display()))?;
                let secret = SecretKey::from_slice(&bytes)
                    .with_context(|| format!("{}: not a P-256 key", path.display()))?;
                Ok(Key { secret })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = SecretKey::random(&mut rand_core::OsRng);
                if let Some(dir) = path.parent() {
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(dir)?;
                }
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .with_context(|| format!("create {}", path.display()))?;
                writeln!(f, "{}", B64.encode(secret.to_bytes()))?;
                f.sync_all()?;
                Ok(Key { secret })
            }
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    #[cfg(test)]
    pub fn random() -> Key {
        Key {
            secret: SecretKey::random(&mut rand_core::OsRng),
        }
    }

    fn public_raw(&self) -> Vec<u8> {
        self.secret
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec()
    }

    pub fn pubkey(&self) -> String {
        B64.encode(self.public_raw())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_raw())
    }

    /// Makes and signs the spec for `req` (validated first).
    pub fn sign(&self, dispatcher: &str, req: &Request, now: i64) -> Result<(Spec, Signed)> {
        req.validate()?;
        let spec = Spec {
            v: VERSION,
            form: new_form_id(),
            dispatcher: dispatcher.to_string(),
            pubkey: self.pubkey(),
            expires: format_time(now + req.ttl.unwrap_or(DEFAULT_TTL)),
            purpose: req.purpose.trim().to_string(),
            from: req.from.clone(),
            target: req.target.clone(),
            fields: req.fields.clone(),
        };
        let bytes = serde_json::to_vec(&spec)?;
        let sig: Signature = SigningKey::from(&self.secret).sign(&bytes);
        let signed = Signed {
            spec: B64.encode(&bytes),
            sig: B64.encode(sig.to_bytes()),
        };
        Ok((spec, signed))
    }

    /// Checks that `signed` is a spec this key signed, and returns it.
    pub fn verify(&self, signed: &Signed) -> Result<Spec> {
        let bytes = B64.decode(&signed.spec).context("spec is not base64url")?;
        let sig = B64
            .decode(&signed.sig)
            .context("signature is not base64url")?;
        let sig = Signature::from_slice(&sig).context("invalid signature")?;
        VerifyingKey::from(&SigningKey::from(&self.secret))
            .verify(&bytes, &sig)
            .map_err(|_| anyhow::anyhow!("the form was not signed by this node"))?;
        let spec: Spec = serde_json::from_slice(&bytes).context("invalid form spec")?;
        if spec.pubkey != self.pubkey() {
            bail!("the form names another node key");
        }
        Ok(spec)
    }

    /// Decrypts a sealed reply to `spec` into its values object.
    pub fn open(&self, spec: &Spec, sealed: &Sealed) -> Result<Map<String, Value>> {
        if sealed.form != spec.form {
            bail!("the sealed reply is for another form");
        }
        let (eph_raw, rest) = sealed.blob.split_at(PUBKEY_LEN);
        let (iv, ct) = rest.split_at(IV_LEN);
        let eph = PublicKey::from_sec1_bytes(eph_raw).context("invalid ephemeral key")?;
        let shared = p256::ecdh::diffie_hellman(self.secret.to_nonzero_scalar(), eph.as_affine());
        let key = derive_key(&shared, eph_raw, &self.public_raw());
        let aad = spec.aad();
        let plain = Aes256Gcm::new_from_slice(&key)
            .expect("32-byte key")
            .decrypt(
                &Nonce::from(<[u8; IV_LEN]>::try_from(iv).expect("split at IV_LEN")),
                Payload_ {
                    msg: ct,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("the sealed reply does not decrypt for this form"))?;
        let v: Value = serde_json::from_slice(&plain).context("sealed values are not JSON")?;
        match v {
            Value::Object(m) => Ok(m),
            _ => bail!("sealed values are not an object"),
        }
    }
}

/// `SHA256:` and the first 16 bytes of the public key's SHA-256, in groups of four hex digits.
pub fn fingerprint(public_raw: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(public_raw);
    let hex: Vec<String> = d[..16]
        .chunks(2)
        .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
        .collect();
    format!("SHA256:{}", hex.join(" "))
}

/// The fingerprint of a base64url public key, as the page shows it.
pub fn fingerprint_b64(pubkey: &str) -> Result<String> {
    Ok(fingerprint(
        &B64.decode(pubkey).context("pubkey is not base64url")?,
    ))
}

/// A checked value: a string, or a file's bytes.
#[derive(Debug, PartialEq, Eq)]
pub enum Val {
    Text(String),
    File { name: String, data: Vec<u8> },
}

impl Val {
    fn bytes(&self) -> &[u8] {
        match self {
            Val::Text(s) => s.as_bytes(),
            Val::File { data, .. } => data,
        }
    }
}

/// Checks decrypted values against the spec's fields: no unknown names,
/// required ones present, types, sizes and choices.
pub fn check_values(spec: &Spec, values: &Map<String, Value>) -> Result<BTreeMap<String, Val>> {
    for k in values.keys() {
        if !spec.fields.iter().any(|f| &f.name == k) {
            bail!("value for unknown field {k:?}");
        }
    }
    let mut out = BTreeMap::new();
    for f in &spec.fields {
        let v = match values.get(&f.name) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() && f.kind != Kind::File => None,
            Some(v) => Some(v),
        };
        let Some(v) = v else {
            if f.required {
                bail!("field {} is required", f.name);
            }
            continue;
        };
        let val = match f.kind {
            Kind::File => {
                let name = v["name"].as_str().unwrap_or("file").to_string();
                let data = v["data"]
                    .as_str()
                    .with_context(|| format!("field {}: a file needs data", f.name))?;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .or_else(|_| B64.decode(data))
                    .with_context(|| format!("field {}: file data is not base64", f.name))?;
                Val::File { name, data }
            }
            _ => Val::Text(
                v.as_str()
                    .with_context(|| format!("field {}: expected text", f.name))?
                    .to_string(),
            ),
        };
        if val.bytes().len() > f.max() {
            bail!("field {} is over {} bytes", f.name, f.max());
        }
        if let (Kind::Choice, Val::Text(s)) = (f.kind, &val) {
            if !f.choices.contains(s) {
                bail!("field {}: not one of the choices", f.name);
            }
        }
        if let Val::Text(s) = &val {
            if f.kind != Kind::Text && s.contains(['\n', '\r', '\0']) {
                bail!("field {}: a single line is expected", f.name);
            }
        }
        out.insert(f.name.clone(), val);
    }
    Ok(out)
}

// ------------------------------------------------------ targets on disk

/// The secrets folder of a task or bot target, outside its `repos/`:
/// `<run_root>/task-<id>/.secrets` or `<run_root>/bot-<name>/.secrets`.
pub fn secrets_dir(run_root: &Path, target: &Target) -> Option<PathBuf> {
    match target {
        Target::Task { task } => Some(run_root.join(format!("task-{task}")).join(".secrets")),
        Target::Bot { bot } => Some(run_root.join(format!("bot-{bot}")).join(".secrets")),
        Target::Git { .. } | Target::Runtime { .. } | Target::Reply => None,
    }
}

/// The map of environment variable to value file in a secrets folder.
const ENV_FILE: &str = ".env.json";

fn private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Writes `bytes` to `path` as a 0600 file in a 0700 folder.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    write_private(path, bytes)
}

/// Writes `bytes` to `path` as a 0600 file, replacing it atomically.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let r = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.with_context(|| format!("write {}", path.display()))
}

/// Writes each value to `<dir>/<field>` (0600) and records the fields with
/// an environment variable in `<dir>/.env.json`. Returns the files written.
fn write_folder(dir: &Path, spec: &Spec, vals: &BTreeMap<String, Val>) -> Result<Vec<String>> {
    private_dir(dir)?;
    let env_path = dir.join(ENV_FILE);
    let mut env: BTreeMap<String, String> = match std::fs::read(&env_path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    };
    let mut written = Vec::new();
    for f in &spec.fields {
        let Some(v) = vals.get(&f.name) else {
            continue;
        };
        write_private(&dir.join(&f.name), v.bytes())?;
        written.push(f.name.clone());
        if let Some(var) = &f.env {
            env.insert(var.clone(), f.name.clone());
        }
    }
    write_private(&env_path, &serde_json::to_vec_pretty(&env)?)?;
    Ok(written)
}

/// The environment a run of the folder's target gets: each recorded
/// variable with its value file's contents. Unreadable or reserved entries
/// are skipped.
pub fn launch_env(dir: &Path) -> Vec<(String, String)> {
    let Ok(b) = std::fs::read(dir.join(ENV_FILE)) else {
        return Vec::new();
    };
    let env: BTreeMap<String, String> = serde_json::from_slice(&b).unwrap_or_default();
    env.into_iter()
        .filter(|(k, f)| check_env_name(k).is_ok() && is_field_file(f))
        .filter_map(|(k, f)| Some((k, std::fs::read_to_string(dir.join(f)).ok()?)))
        .collect()
}

fn is_field_file(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Sets the git credential for `https://<host>` in a `git credential-store`
/// file (0600), replacing an earlier one for the same host.
pub fn write_git_credential(path: &Path, host: &str, username: &str, token: &str) -> Result<()> {
    check_host(host)?;
    let enc = |s: &str| crate::channel::pct_encode(s);
    let line = format!("https://{}:{}@{host}", enc(username), enc(token));
    let suffix = format!("@{host}");
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.trim_end().ends_with(&suffix))
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    };
    lines.push(line);
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    write_private(path, format!("{}\n", lines.join("\n")).as_bytes())
}

// ------------------------------------------- sealed messages to the owner
//
// The reverse of a form: an agent or the node sends the owner a secret,
// a link or a small file over a channel that must see only ciphertext.
//
// - The owner sets a passphrase once through a reply key form
//   ([`Target::Reply`]). The node derives the reply key
//   `K = PBKDF2-HMAC-SHA256(passphrase, salt, iter)` (random 16-byte salt,
//   [`REPLY_ITERATIONS`]) and keeps only `K`, salt and count in
//   `reply.key` (0600, [`ReplyKey`]); the passphrase is dropped.
// - A message is AES-256-GCM under `K` with a random 12-byte IV. Its header
//   ([`MessageHeader`]: id, sender, salt, count, expiry) is plaintext and is
//   the associated data, so a relay cannot alter the expiry or swap keys.
//   The text is `<base64url(header JSON)>.<base64url(iv | ciphertext)>`; the
//   link is the form page with `#m=<text>` in its fragment (never sent to the
//   page's host). The page asks for the passphrase, derives `K` and decrypts
//   in the browser.
// - Expiry: the page refuses a message past its authenticated expiry. One
//   use: what must be single-use (a login code) is enforced where it is
//   redeemed; the page also remembers opened ids per device and warns on a
//   second open. Replay: without `K` a message is useless; the ciphertext
//   stays in the chat, so send only material that expires or can be rotated.
// - The server and the channel see the header, the ciphertext and the
//   message text the sender chose (sender and purpose); never `K`, the
//   passphrase or the plaintext.

/// The reply key file's name next to the config (`[forms] reply_key_file`).
pub const REPLY_KEY_FILE: &str = "reply.key";
pub const MESSAGE_VERSION: u32 = 1;
/// PBKDF2 iterations for a new reply key (OWASP 2023, PBKDF2-HMAC-SHA256).
pub const REPLY_ITERATIONS: u32 = 600_000;
/// Shortest passphrase a reply key form accepts, in characters.
pub const MIN_PASSPHRASE: usize = 12;
/// A message's life when the sender names none.
pub const MESSAGE_DEFAULT_TTL: i64 = 15 * 60;
pub const MESSAGE_MAX_TTL: i64 = 24 * 3600;
/// Largest plaintext body: the whole link rides in a Mini App button URL.
pub const MAX_MESSAGE: usize = 2048;
const SALT_LEN: usize = 16;

/// PBKDF2-HMAC-SHA256 with a 32-byte output (one block), as WebCrypto's
/// `deriveBits({name: "PBKDF2", hash: "SHA-256"}, ..., 256)`.
pub fn pbkdf2_sha256(pass: &[u8], salt: &[u8], iter: u32) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    type H = Hmac<sha2_10::Sha256>;
    let mac = <H as Mac>::new_from_slice(pass).expect("HMAC takes any key length");
    let mut u = {
        let mut m = mac.clone();
        m.update(salt);
        m.update(&1u32.to_be_bytes());
        m.finalize().into_bytes()
    };
    let mut out: [u8; 32] = u.into();
    for _ in 1..iter {
        let mut m = mac.clone();
        m.update(&u);
        u = m.finalize().into_bytes();
        out.iter_mut().zip(u.iter()).for_each(|(o, b)| *o ^= b);
    }
    out
}

/// The owner's reply key (`reply.key`): the derived key, never the passphrase.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyKey {
    pub v: u32,
    /// base64url salt.
    pub salt: String,
    pub iter: u32,
    /// base64url 32-byte AES key.
    key: String,
}

impl std::fmt::Debug for ReplyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReplyKey({})", self.id())
    }
}

impl ReplyKey {
    /// A key from `passphrase` with a fresh salt and [`REPLY_ITERATIONS`].
    pub fn create(passphrase: &str) -> Result<ReplyKey> {
        if passphrase.chars().count() < MIN_PASSPHRASE {
            bail!("the passphrase is shorter than {MIN_PASSPHRASE} characters");
        }
        let mut salt = [0u8; SALT_LEN];
        getrandom(&mut salt);
        Ok(ReplyKey::derive(passphrase, &salt, REPLY_ITERATIONS))
    }

    pub fn derive(passphrase: &str, salt: &[u8], iter: u32) -> ReplyKey {
        ReplyKey {
            v: MESSAGE_VERSION,
            salt: B64.encode(salt),
            iter,
            key: B64.encode(pbkdf2_sha256(passphrase.as_bytes(), salt, iter)),
        }
    }

    /// The key's id: its salt (not secret; the page caches keys by it).
    pub fn id(&self) -> &str {
        &self.salt
    }

    pub fn load(path: &Path) -> Result<ReplyKey> {
        let b = std::fs::read(path).map_err(|e| {
            anyhow::anyhow!(
                "no reply key at {} ({e}): the owner sets one with `axe form reply-key --bot <bot>`",
                path.display()
            )
        })?;
        let k: ReplyKey = serde_json::from_slice(&b)
            .with_context(|| format!("invalid reply key {}", path.display()))?;
        if k.v != MESSAGE_VERSION || k.bytes()?.len() != 32 || k.iter == 0 {
            bail!("invalid reply key {}", path.display());
        }
        Ok(k)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_private_file(path, &serde_json::to_vec_pretty(self)?)
    }

    fn bytes(&self) -> Result<Vec<u8>> {
        B64.decode(&self.key).context("invalid reply key")
    }
}

/// A sealed message's plaintext header, authenticated as associated data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageHeader {
    pub v: u32,
    /// 32 hex digits.
    pub id: String,
    /// Who sent it (`task 7`, `bot axe-os`), shown by the page.
    pub from: String,
    pub salt: String,
    pub iter: u32,
    /// RFC 3339 UTC.
    pub expires: String,
}

/// What a message carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Secret(String),
    /// An access link the page opens with one tap.
    Link(String),
    File {
        name: String,
        data: Vec<u8>,
    },
}

impl Payload {
    fn body(&self, purpose: &str) -> Value {
        match self {
            Payload::Secret(s) => json!({"purpose": purpose, "kind": "secret", "secret": s}),
            Payload::Link(l) => json!({"purpose": purpose, "kind": "link", "link": l}),
            Payload::File { name, data } => json!({"purpose": purpose, "kind": "file",
                "file": {"name": name,
                         "data": base64::engine::general_purpose::STANDARD.encode(data)}}),
        }
    }
}

/// Seals `payload` to the owner's reply key; returns the header and the
/// message text (`<header>.<iv | ciphertext>`, base64url).
pub fn seal_message(
    key: &ReplyKey,
    from: &str,
    purpose: &str,
    payload: &Payload,
    ttl: i64,
    now: i64,
) -> Result<(MessageHeader, String)> {
    if !(MIN_TTL..=MESSAGE_MAX_TTL).contains(&ttl) {
        bail!("a sealed message lives 1m to 24h");
    }
    if let Payload::Link(l) = payload {
        if !(l.starts_with("https://") || l.starts_with("http://")) {
            bail!("a link must be http(s)");
        }
    }
    let header = MessageHeader {
        v: MESSAGE_VERSION,
        id: new_form_id(),
        from: from.chars().take(MAX_LABEL).collect(),
        salt: key.salt.clone(),
        iter: key.iter,
        expires: format_time(now + ttl),
    };
    let plain = serde_json::to_vec(&payload.body(purpose))?;
    if plain.len() > MAX_MESSAGE {
        bail!(
            "the message is {} bytes; a sealed message holds at most {MAX_MESSAGE}",
            plain.len()
        );
    }
    let aad = B64.encode(serde_json::to_vec(&header)?);
    let cipher = Aes256Gcm::new_from_slice(&key.bytes()?).context("reply key")?;
    let mut iv = [0u8; IV_LEN];
    getrandom(&mut iv);
    let ct = cipher
        .encrypt(
            &Nonce::from(iv),
            Payload_ {
                msg: &plain,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("cannot seal the message"))?;
    let mut data = iv.to_vec();
    data.extend_from_slice(&ct);
    Ok((header, format!("{aad}.{}", B64.encode(data))))
}

/// Opens a message text with the reply key, as the page does; checks the
/// expiry. Returns the header and the body (`purpose`, `kind`, value).
pub fn open_message(key: &ReplyKey, text: &str, now: i64) -> Result<(MessageHeader, Value)> {
    let (aad, data) = text.split_once('.').context("not a sealed message")?;
    let header: MessageHeader = serde_json::from_slice(&B64.decode(aad)?)?;
    if header.v != MESSAGE_VERSION {
        bail!("unsupported message version {}", header.v);
    }
    if header.salt != key.salt || header.iter != key.iter {
        bail!("the message is sealed to another reply key");
    }
    if now > parse_time(&header.expires)? {
        bail!("the message expired at {}", header.expires);
    }
    let data = B64.decode(data)?;
    if data.len() <= IV_LEN {
        bail!("truncated message");
    }
    let cipher = Aes256Gcm::new_from_slice(&key.bytes()?).context("reply key")?;
    let plain = cipher
        .decrypt(
            &Nonce::from(<[u8; IV_LEN]>::try_from(&data[..IV_LEN]).expect("split at IV_LEN")),
            Payload_ {
                msg: &data[IV_LEN..],
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("the message does not open with this key"))?;
    Ok((header, serde_json::from_slice(&plain)?))
}

/// The page link that opens a message.
pub fn message_link(page: &str, text: &str) -> String {
    format!("{page}#m={text}")
}

// ------------------------------------------------------- the node side

/// The node's half: signs requests and delivers sealed replies. It
/// answers the server's relay jobs `form_sign` and `form_deliver`.
pub struct Desk {
    key: Key,
    id: String,
    run_root: PathBuf,
    git_credentials: PathBuf,
    /// Delivered forms and their expiry (Unix seconds), so each is used once.
    used_path: PathBuf,
    used: Mutex<BTreeMap<String, i64>>,
    /// The node config, for runtime logins.
    cfg: Option<crate::config::Config>,
    /// Runtimes with a device login in progress.
    logins: std::sync::Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// Where a reply key form writes the owner's [`ReplyKey`].
    reply_key: PathBuf,
    /// Runtimes just logged in, for the node to check at once.
    logged_in: std::sync::Arc<Mutex<Vec<String>>>,
}

fn push_login(logged_in: &Mutex<Vec<String>>, runtime: &str) {
    let mut l = logged_in.lock().unwrap_or_else(|e| e.into_inner());
    if !l.iter().any(|r| r == runtime) {
        l.push(runtime.to_string());
    }
}

/// How long a device login's first output is collected for the owner.
pub const DEVICE_OUTPUT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

impl Desk {
    pub fn new(
        key: Key,
        id: &str,
        run_root: PathBuf,
        git_credentials: PathBuf,
        used_path: PathBuf,
    ) -> Desk {
        let used = std::fs::read(&used_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Desk {
            key,
            id: id.to_string(),
            run_root,
            reply_key: git_credentials.with_file_name(REPLY_KEY_FILE),
            git_credentials,
            used_path,
            used: Mutex::new(used),
            cfg: None,
            logins: Default::default(),
            logged_in: Default::default(),
        }
    }

    /// The desk with the node config, which runtime logins need.
    pub fn with_config(mut self, cfg: crate::config::Config) -> Desk {
        self.cfg = Some(cfg);
        self
    }

    /// Opens the desk of `cfg`'s dispatcher `id`, making its key on first use.
    pub fn open(cfg: &crate::config::Config, id: &str) -> Result<Desk> {
        let key = Key::load_or_create(&cfg.forms.key_path())?;
        Ok(Desk::new(
            key,
            id,
            cfg.run_root(),
            cfg.forms.git_credentials_path(),
            cfg.log_dir().join(".dispatcher").join("forms-used.json"),
        )
        .with_config(cfg.clone())
        .with_reply_key(cfg.forms.reply_key_path()))
    }

    /// The desk telling the node, through `logged_in`, which runtimes
    /// a login just finished for.
    pub fn with_logged_in(mut self, logged_in: std::sync::Arc<Mutex<Vec<String>>>) -> Desk {
        self.logged_in = logged_in;
        self
    }

    fn logged_in(&self, runtime: &str) {
        push_login(&self.logged_in, runtime);
    }

    /// The desk writing the owner's reply key to `path`.
    pub fn with_reply_key(mut self, path: PathBuf) -> Desk {
        self.reply_key = path;
        self
    }

    pub fn key(&self) -> &Key {
        &self.key
    }

    /// The key's public half, for the server: `{"pubkey","fingerprint"}`.
    pub fn public(&self) -> Value {
        json!({"pubkey": self.key.pubkey(), "fingerprint": self.key.fingerprint()})
    }

    /// Answers one relay job; errors come back as `{"error": ...}` and never
    /// carry a value.
    pub fn handle(&self, q: &Value, now: i64) -> Value {
        let r = match q["kind"].as_str() {
            Some("form_sign") => self.sign(q, now),
            Some("form_deliver") => self.deliver(q, now),
            Some("runtime_login") => self.device_login(q),
            _ => Err(anyhow::anyhow!("unknown form job")),
        };
        r.unwrap_or_else(|e| json!({"error": format!("{e:#}")}))
    }

    fn sign(&self, q: &Value, now: i64) -> Result<Value> {
        let req: Request = serde_json::from_value(q["request"].clone()).context("form request")?;
        let (spec, signed) = self.key.sign(&self.id, &req, now)?;
        Ok(
            json!({"form": spec.form, "expires": spec.expires, "signed": signed,
                  "fingerprint": self.key.fingerprint()}),
        )
    }

    fn runtime(&self, name: &str) -> Result<(crate::config::Config, crate::runtime::Runtime)> {
        let cfg = self
            .cfg
            .clone()
            .context("runtime logins need the node config")?;
        let reg = crate::runtime::Registry::load(&cfg)?;
        if !reg.managed {
            bail!("this node has no runtime registry ([runtimes] dir)");
        }
        let rt = reg
            .get(name)
            .cloned()
            .with_context(|| format!("no runtime {name} on this node"))?;
        Ok((cfg, rt))
    }

    /// Starts a runtime's device code login; returns what the CLI printed.
    fn device_login(&self, q: &Value) -> Result<Value> {
        let name = q["runtime"].as_str().context("missing runtime")?;
        let (cfg, rt) = self.runtime(name)?;
        if !self
            .logins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string())
        {
            bail!("a login of runtime {name} is already in progress");
        }
        let logins = std::sync::Arc::clone(&self.logins);
        let logged_in = std::sync::Arc::clone(&self.logged_in);
        let key = name.to_string();
        let r = rt.device_login(&cfg, DEVICE_OUTPUT_WAIT, move |_| {
            logins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            // Checked whatever the CLI said: the check tells the owner.
            push_login(&logged_in, &key);
        });
        if r.is_err() {
            self.logins
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(name);
        }
        Ok(json!({"output": r?}))
    }

    fn deliver(&self, q: &Value, now: i64) -> Result<Value> {
        let signed: Signed = serde_json::from_value(q["signed"].clone()).context("signed form")?;
        let spec = self.key.verify(&signed)?;
        if spec.dispatcher != self.id {
            bail!("the form names another node");
        }
        let expires = spec.expires_at()?;
        if now > expires {
            bail!("the form expired");
        }
        let sealed = Sealed::parse(q["sealed"].as_str().context("missing sealed reply")?)?;
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if used.contains_key(&spec.form) {
            bail!("the form was already used");
        }
        let values = self.key.open(&spec, &sealed)?;
        let vals = check_values(&spec, &values)?;
        let delivered = self.write(&spec, &vals)?;
        used.retain(|_, exp| *exp >= now);
        used.insert(spec.form.clone(), expires);
        if let Err(e) = write_private(&self.used_path, &serde_json::to_vec(&*used)?) {
            eprintln!("axe node: forms: record used form: {e:#}");
        }
        Ok(json!({"delivered": delivered}))
    }

    /// Writes the values to the spec's target; returns where (no values).
    fn write(&self, spec: &Spec, vals: &BTreeMap<String, Val>) -> Result<Value> {
        match &spec.target {
            Target::Task { .. } | Target::Bot { .. } => {
                let dir = secrets_dir(&self.run_root, &spec.target).expect("folder target");
                let files = write_folder(&dir, spec, vals)?;
                let env: Vec<&str> = spec
                    .fields
                    .iter()
                    .filter(|f| vals.contains_key(&f.name))
                    .filter_map(|f| f.env.as_deref())
                    .collect();
                Ok(
                    json!({"target": spec.target.describe(), "dir": dir, "files": files, "env": env}),
                )
            }
            Target::Git { host } => {
                let token = match vals.get("token") {
                    Some(Val::Text(t)) => t,
                    _ => bail!("a git credential needs a token"),
                };
                let user = match vals.get("username") {
                    Some(Val::Text(u)) => u.as_str(),
                    _ => "x-access-token",
                };
                write_git_credential(&self.git_credentials, host, user, token)?;
                Ok(json!({"target": spec.target.describe(), "host": host}))
            }
            Target::Runtime { runtime, login } => {
                let value = match vals.get("secret") {
                    Some(Val::Text(t)) => t,
                    _ => bail!("a runtime login needs its secret"),
                };
                let (cfg, rt) = self.runtime(runtime)?;
                rt.login(&cfg, login, value)?;
                self.logged_in(runtime);
                Ok(json!({"target": spec.target.describe(), "runtime": runtime,
                          "note": "the login is checked now; a runtime card follows with the result"}))
            }
            Target::Reply => {
                let pass = match vals.get("passphrase") {
                    Some(Val::Text(t)) => t,
                    _ => bail!("a reply key needs its passphrase"),
                };
                let key = ReplyKey::create(pass)?;
                key.save(&self.reply_key)?;
                Ok(
                    json!({"target": spec.target.describe(), "path": self.reply_key,
                          "key": key.id()}),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: Target) -> Request {
        Request {
            purpose: "Deploy token".into(),
            target,
            fields: vec![
                Field::parse("token:secret:required:env=DEPLOY_TOKEN").unwrap(),
                Field::parse("note:text").unwrap(),
            ],
            ttl: None,
            from: "task 7".into(),
        }
    }

    fn desk(dir: &Path) -> Desk {
        Desk::new(
            Key::random(),
            "d1",
            dir.join("runs"),
            dir.join("git-credentials"),
            dir.join("used.json"),
        )
    }

    fn signed(d: &Desk, r: &Request, now: i64) -> (Spec, Signed) {
        let v = d.handle(&json!({"kind":"form_sign","request":r}), now);
        let s: Signed = serde_json::from_value(v["signed"].clone()).unwrap();
        (s.decode().unwrap(), s)
    }

    fn deliver(d: &Desk, s: &Signed, sealed: &Sealed, now: i64) -> Value {
        d.handle(
            &json!({"kind":"form_deliver","signed":s,"sealed":sealed.text()}),
            now,
        )
    }

    fn hexs(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn pbkdf2_matches_known_vectors() {
        // RFC 7914 section 11 / common PBKDF2-HMAC-SHA256 vectors, 32 bytes.
        assert_eq!(
            hexs(&pbkdf2_sha256(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hexs(&pbkdf2_sha256(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        assert_eq!(
            hexs(&pbkdf2_sha256(b"password", b"salt", 4096)),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    /// A fast key for tests (the real count is [`REPLY_ITERATIONS`]).
    fn reply_key() -> ReplyKey {
        ReplyKey::derive("correct horse battery", &[7u8; SALT_LEN], 1000)
    }

    #[test]
    fn sealed_messages_round_trip_and_hold_no_plaintext() {
        let k = reply_key();
        let now = 1_800_000_000;
        for p in [
            Payload::Secret("dummy-secret-91d2".into()),
            Payload::Link("http://axe.example/login?code=dummy-code-77ab".into()),
            Payload::File {
                name: "id.txt".into(),
                data: b"dummy-file-3c4d".to_vec(),
            },
        ] {
            let (h, text) = seal_message(&k, "task 7", "Web login", &p, 600, now).unwrap();
            for needle in [
                "dummy-secret-91d2",
                "dummy-code-77ab",
                "dummy-file-3c4d",
                "Web login",
            ] {
                assert!(!text.contains(needle), "{needle} in {text}");
            }
            let link = message_link("https://page.example/", &text);
            assert!(link.starts_with("https://page.example/#m="));
            let (h2, body) = open_message(&k, &text, now + 599).unwrap();
            assert_eq!(h, h2);
            assert_eq!(h.from, "task 7");
            assert_eq!(body["purpose"], "Web login");
            match &p {
                Payload::Secret(s) => assert_eq!(body["secret"], s.as_str()),
                Payload::Link(l) => assert_eq!(body["link"], l.as_str()),
                Payload::File { name, .. } => {
                    assert_eq!(body["file"]["name"], name.as_str());
                    assert_eq!(body["file"]["data"], "ZHVtbXktZmlsZS0zYzRk");
                }
            }
            // Expired, tampered header, tampered ciphertext, another key.
            assert!(open_message(&k, &text, now + 601).is_err());
            let (aad, data) = text.split_once('.').unwrap();
            let mut hdr: MessageHeader = serde_json::from_slice(&B64.decode(aad).unwrap()).unwrap();
            hdr.expires = format_time(now + 86_000);
            let forged = format!("{}.{data}", B64.encode(serde_json::to_vec(&hdr).unwrap()));
            assert!(open_message(&k, &forged, now).is_err());
            let mut raw = B64.decode(data).unwrap();
            *raw.last_mut().unwrap() ^= 1;
            assert!(open_message(&k, &format!("{aad}.{}", B64.encode(raw)), now).is_err());
            let other = ReplyKey::derive("correct horse battery", &[8u8; SALT_LEN], 1000);
            assert!(open_message(&other, &text, now).is_err());
            let same_salt = ReplyKey::derive("wrong horse battery", &[7u8; SALT_LEN], 1000);
            assert!(open_message(&same_salt, &text, now).is_err());
        }
        let big = Payload::Secret("x".repeat(MAX_MESSAGE));
        assert!(seal_message(&k, "t", "p", &big, 600, now).is_err());
        let s = Payload::Secret("s".into());
        assert!(seal_message(&k, "t", "p", &s, 10, now).is_err());
        assert!(seal_message(&k, "t", "p", &s, MESSAGE_MAX_TTL + 1, now).is_err());
        assert!(seal_message(
            &k,
            "t",
            "p",
            &Payload::Link("javascript:x".into()),
            600,
            now
        )
        .is_err());
    }

    /// The vector `sealed-form`'s `test/unit.mjs` opens with WebCrypto.
    #[test]
    fn sealed_message_cross_vector_opens() {
        let text = include_str!("../tests/fixtures/sealed-message-vector.txt").trim();
        let (h, body) = open_message(&reply_key(), text, 4_000_000_000).unwrap();
        assert_eq!(h.from, "task 7");
        assert_eq!(body["kind"], "link");
        assert_eq!(body["link"], "http://axe.example/login?code=vector");
    }

    #[test]
    fn reply_key_files_and_forms() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ReplyKey::create("too short").is_err());
        let path = dir.path().join("cfg").join(REPLY_KEY_FILE);
        let err = ReplyKey::load(&path).unwrap_err().to_string();
        assert!(err.contains("axe form reply-key"), "{err}");
        let k = reply_key();
        k.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = ReplyKey::load(&path).unwrap();
        assert_eq!(back.id(), k.id());
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("correct horse"), "no passphrase on disk");
        assert!(!format!("{k:?}").contains(&k.key), "Debug hides the key");

        // A reply key form takes exactly one required secret, passphrase.
        let mut r = req(Target::Reply);
        assert!(r.validate().is_err());
        r.fields = vec![Field::parse("passphrase:secret:required").unwrap()];
        r.validate().unwrap();
    }

    #[test]
    fn field_parse_and_validation() {
        let f = Field::parse("plan:choice=max|pro:required:label=Plan").unwrap();
        assert_eq!(f.kind, Kind::Choice);
        assert_eq!(f.choices, ["max", "pro"]);
        assert!(f.required);
        assert!(Field::parse("Token:secret").is_err());
        assert!(Field::parse("t:password").is_err());
        assert!(Field::parse("t:choice").is_err());
        assert!(Field::parse("t:text:env=PATH").is_err());
        assert!(Field::parse("t:text:env=AXE_TASK").is_err());
        assert!(Field::parse("t:text:env=LD_PRELOAD").is_err());
        assert!(Field::parse("t:file:env=X").is_err());
        assert!(Field::parse("t:secret:max=0").is_err());
        assert!(Field::parse(&format!("t:secret:max={}", MAX_VALUE + 1)).is_err());
        assert!(Field::parse("t:secret:bogus").is_err());
    }

    #[test]
    fn request_validation() {
        let ok = req(Target::Task { task: 7 });
        ok.validate().unwrap();
        let mut r = ok.clone();
        r.fields.push(r.fields[0].clone());
        assert!(r.validate().unwrap_err().to_string().contains("twice"));
        let mut r = ok.clone();
        r.fields.iter_mut().for_each(|f| f.required = false);
        assert!(r.validate().is_err());
        let mut r = ok.clone();
        r.ttl = Some(10);
        assert!(r.validate().is_err());
        let mut r = ok.clone();
        r.purpose = " ".into();
        assert!(r.validate().is_err());
        let mut r = ok.clone();
        r.target = Target::Task { task: 0 };
        assert!(r.validate().is_err());
        let mut r = ok.clone();
        r.fields = (0..=MAX_FIELDS)
            .map(|i| Field::parse(&format!("f{i}:text:required")).unwrap())
            .collect();
        assert!(r.validate().is_err());
        // A git credential has exactly token (+ username), no env.
        let mut r = ok;
        r.target = Target::Git {
            host: "github.com".into(),
        };
        assert!(r.validate().is_err());
        r.fields = vec![Field::parse("token:secret:required").unwrap()];
        r.validate().unwrap();
        r.target = Target::Git {
            host: "evil/host".into(),
        };
        assert!(r.validate().is_err());
    }

    #[test]
    fn round_trip_to_a_task() {
        let tmp = tempfile::tempdir().unwrap();
        let d = desk(tmp.path());
        let (spec, s) = signed(&d, &req(Target::Task { task: 7 }), 1000);
        assert_eq!(spec.expires_at().unwrap(), 1000 + DEFAULT_TTL);
        assert_eq!(spec.from, "task 7");
        let sealed = seal(&spec, &json!({"token": "s3cret-value", "note": "hi"})).unwrap();
        assert!(!sealed.text().contains("s3cret"));
        let v = deliver(&d, &s, &sealed, 1001);
        assert!(v["error"].is_null(), "{v}");
        assert!(!v.to_string().contains("s3cret"));
        let dir = tmp.path().join("runs/task-7/.secrets");
        assert_eq!(v["delivered"]["dir"], json!(dir));
        assert_eq!(
            std::fs::read_to_string(dir.join("token")).unwrap(),
            "s3cret-value"
        );
        let mode = std::fs::metadata(dir.join("token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dmode & 0o777, 0o700);
        assert_eq!(
            launch_env(&dir),
            vec![("DEPLOY_TOKEN".to_string(), "s3cret-value".to_string())]
        );
        // Only the requesting task's folder was written.
        let runs: Vec<_> = std::fs::read_dir(tmp.path().join("runs"))
            .unwrap()
            .collect();
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn one_use_and_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let d = desk(tmp.path());
        let (spec, s) = signed(&d, &req(Target::Task { task: 7 }), 1000);
        let sealed = seal(&spec, &json!({"token": "x"})).unwrap();
        assert!(deliver(&d, &s, &sealed, 1001)["error"].is_null());
        let again = deliver(&d, &s, &sealed, 1002);
        assert!(again["error"].as_str().unwrap().contains("already used"));
        // The used set survives a node restart.
        let d2 = Desk::new(
            Key {
                secret: d.key.secret.clone(),
            },
            "d1",
            tmp.path().join("runs"),
            tmp.path().join("git-credentials"),
            tmp.path().join("used.json"),
        );
        assert!(deliver(&d2, &s, &sealed, 1003)["error"]
            .as_str()
            .unwrap()
            .contains("already used"));
        let (spec, s) = signed(&d, &req(Target::Task { task: 7 }), 1000);
        let sealed = seal(&spec, &json!({"token": "x"})).unwrap();
        let late = deliver(&d, &s, &sealed, 1000 + DEFAULT_TTL + 1);
        assert!(late["error"].as_str().unwrap().contains("expired"));
    }

    #[test]
    fn wrong_dispatcher_and_tampering_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let d = desk(tmp.path());
        let other = desk(&tmp.path().join("other"));
        let (spec, s) = signed(&d, &req(Target::Task { task: 7 }), 1000);
        let sealed = seal(&spec, &json!({"token": "x"})).unwrap();
        // Another node's key did not sign it.
        let e = deliver(&other, &s, &sealed, 1001);
        assert!(e["error"].as_str().unwrap().contains("not signed"), "{e}");
        // A relay that rewrites the target breaks the signature.
        let mut forged = spec.clone();
        forged.target = Target::Task { task: 8 };
        let bad = Signed {
            spec: B64.encode(serde_json::to_vec(&forged).unwrap()),
            sig: s.sig.clone(),
        };
        let e = deliver(
            &d,
            &bad,
            &seal(&forged, &json!({"token":"x"})).unwrap(),
            1001,
        );
        assert!(e["error"].as_str().unwrap().contains("not signed"), "{e}");
        // Sealed to another node's key (a swapped pubkey).
        let (ospec, _) = signed(&other, &req(Target::Task { task: 7 }), 1000);
        let mut swapped = spec.clone();
        swapped.pubkey = ospec.pubkey.clone();
        let e = deliver(
            &d,
            &s,
            &seal(&swapped, &json!({"token":"x"})).unwrap(),
            1001,
        );
        assert!(
            e["error"].as_str().unwrap().contains("does not decrypt"),
            "{e}"
        );
        // A flipped ciphertext byte.
        let mut t = sealed.clone();
        let last = t.blob.len() - 1;
        t.blob[last] ^= 1;
        let e = deliver(&d, &s, &t, 1001);
        assert!(
            e["error"].as_str().unwrap().contains("does not decrypt"),
            "{e}"
        );
        // A reply sealed for one form replayed against another form.
        let (spec2, s2) = signed(&d, &req(Target::Task { task: 7 }), 1000);
        let moved = Sealed {
            form: spec2.form.clone(),
            blob: sealed.blob.clone(),
        };
        let e = deliver(&d, &s2, &moved, 1001);
        assert!(
            e["error"].as_str().unwrap().contains("does not decrypt"),
            "{e}"
        );
        // Garbage.
        assert!(Sealed::parse("sealed.v1.nothex.AAAA").is_err());
        assert!(Sealed::parse("hello").is_err());
        // Nothing was written anywhere.
        assert!(!tmp.path().join("runs").exists());
        // The untouched reply still works.
        assert!(deliver(&d, &s, &sealed, 1001)["error"].is_null());
    }

    #[test]
    fn values_are_checked_against_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let d = desk(tmp.path());
        let mut r = req(Target::Bot { bot: "ops".into() });
        r.fields.push(Field::parse("plan:choice=max|pro").unwrap());
        r.fields.push(Field::parse("cert:file:max=8").unwrap());
        let (spec, s) = signed(&d, &r, 1000);
        for (bad, why) in [
            (json!({}), "required"),
            (json!({"token": "x", "extra": "y"}), "unknown field"),
            (json!({"token": "x", "plan": "free"}), "choices"),
            (json!({"token": "a\nb"}), "single line"),
            (json!({"token": 5}), "expected text"),
            (
                json!({"token": "x", "cert": {"name": "c", "data": "AAAAAAAAAAAAAAAA"}}),
                "over 8",
            ),
        ] {
            let e = deliver(&d, &s, &seal(&spec, &bad).unwrap(), 1001);
            assert!(e["error"].as_str().unwrap().contains(why), "{bad}: {e}");
        }
        let ok =
            json!({"token": "x", "plan": "pro", "cert": {"name": "c.pem", "data": "aGVsbG8="}});
        let v = deliver(&d, &s, &seal(&spec, &ok).unwrap(), 1001);
        assert!(v["error"].is_null(), "{v}");
        let dir = tmp.path().join("runs/bot-ops/.secrets");
        assert_eq!(std::fs::read(dir.join("cert")).unwrap(), b"hello");
        assert_eq!(std::fs::read_to_string(dir.join("plan")).unwrap(), "pro");
    }

    #[test]
    fn git_credential_target() {
        let tmp = tempfile::tempdir().unwrap();
        let d = desk(tmp.path());
        let r = Request {
            purpose: "GitHub token".into(),
            target: Target::Git {
                host: "github.com".into(),
            },
            fields: vec![Field::parse("token:secret:required").unwrap()],
            ttl: Some(300),
            from: String::new(),
        };
        let path = tmp.path().join("git-credentials");
        write_git_credential(&path, "example.org", "u", "keep").unwrap();
        for token in ["first", "gh p/@:x"] {
            let (spec, s) = signed(&d, &r, 1000);
            let v = deliver(
                &d,
                &s,
                &seal(&spec, &json!({"token": token})).unwrap(),
                1001,
            );
            assert!(v["error"].is_null(), "{v}");
            assert!(!v.to_string().contains(token));
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "https://u:keep@example.org\nhttps://x-access-token:gh%20p%2F%40%3Ax@github.com\n"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn runtime_login_target() {
        let field = |s: &str| Field::parse(s).unwrap();
        let mut r = Request {
            purpose: "Log in".into(),
            target: Target::Runtime {
                runtime: "claude-1".into(),
                login: "token".into(),
            },
            fields: vec![field("secret:secret:required")],
            ttl: None,
            from: String::new(),
        };
        r.validate().unwrap();
        r.fields = vec![field("key:secret:required")];
        assert!(r.validate().is_err());
        r.fields = vec![field("secret:secret:required"), field("note:text")];
        assert!(r.validate().is_err());
        r.fields = vec![field("secret:secret:required")];
        r.target = Target::Runtime {
            runtime: "claude-1".into(),
            login: "device".into(),
        };
        assert!(r.validate().is_err());
        // A desk without a runtime registry refuses the delivery.
        let tmp = tempfile::tempdir().unwrap();
        let d = desk(tmp.path());
        r.target = Target::Runtime {
            runtime: "claude-1".into(),
            login: "token".into(),
        };
        let (spec, s) = signed(&d, &r, 1000);
        let e = deliver(
            &d,
            &s,
            &seal(&spec, &json!({"secret": "oat"})).unwrap(),
            1001,
        );
        assert!(e["error"].as_str().unwrap().contains("node config"), "{e}");
    }

    #[test]
    fn key_file_is_private_and_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cfg/dispatcher.key");
        let a = Key::load_or_create(&path).unwrap();
        let b = Key::load_or_create(&path).unwrap();
        assert_eq!(a.pubkey(), b.pubkey());
        assert_eq!(a.fingerprint(), fingerprint_b64(&a.pubkey()).unwrap());
        assert!(a.fingerprint().starts_with("SHA256:"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Key::load_or_create(&path).is_err());
    }

    #[test]
    fn link_carries_the_spec_in_the_fragment() {
        let k = Key::random();
        let (_, s) = k.sign("d1", &req(Target::Task { task: 7 }), 0).unwrap();
        let l = link("https://example.github.io/form/", &s);
        let (page, frag) = l.split_once('#').unwrap();
        assert_eq!(page, "https://example.github.io/form/");
        assert_eq!(frag, format!("f={}", s.token()));
        assert_eq!(k.verify(&s).unwrap(), s.decode().unwrap());
    }
}
