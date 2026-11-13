//! Blob store: immutable, content-addressed files kept by `axe server`.
//!
//! A blob's id is the sha256 of its bytes, so storing the same bytes twice
//! keeps one copy. Contents live in a local directory or an S3-compatible
//! bucket (`[blobs]` in the config); there is no delete, rename or version.
//! Anyone holding a signed link (`/b/<sha256>[/<name>]?exp=..&sig=..`) can read
//! one blob until the link expires; every other blob route needs the token.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{self, Blobs, BlobsS3};

/// Default largest blob, in MiB.
pub const DEFAULT_MAX_MIB: u64 = 1024;
/// Longest link lifetime: seven days.
pub const MAX_LINK_SECS: i64 = 7 * 86400;
/// Content type of a blob stored without one.
pub const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    pub sha256: String,
    pub size: u64,
    /// Empty when the backend's listing does not carry it (S3 `ls`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    /// RFC 3339 time the blob was first stored, when the backend knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// A store: one backend plus the spool and signing key every backend uses.
pub struct Store {
    backend: Backend,
    spool: PathBuf,
    key: Vec<u8>,
    pub max_bytes: u64,
    pub public_url: Option<String>,
}

enum Backend {
    Local(PathBuf),
    S3(S3),
}

impl Store {
    /// Opens (creating as needed) the store `cfg` describes; `dir` defaults to
    /// `blobs/` next to `db_path`.
    pub fn open(cfg: &Blobs, db_path: &Path) -> Result<Store> {
        let dir = match &cfg.dir {
            Some(d) => config::expand_tilde(d),
            None => db_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("blobs"),
        };
        let spool = dir.join("tmp");
        std::fs::create_dir_all(&spool).with_context(|| format!("create {}", spool.display()))?;
        let key_path = cfg
            .link_key_file
            .as_deref()
            .map_or_else(|| dir.join("link.key"), config::expand_tilde);
        let key = link_key(&key_path)?;
        let backend = match &cfg.s3 {
            Some(s3) => Backend::S3(S3::new(s3)?),
            None => {
                let data = dir.join("sha256");
                std::fs::create_dir_all(&data)?;
                Backend::Local(data)
            }
        };
        Ok(Store {
            backend,
            spool,
            key,
            max_bytes: cfg.max_mib.unwrap_or(DEFAULT_MAX_MIB) * 1024 * 1024,
            public_url: cfg.public_url.clone(),
        })
    }

    /// Copies exactly `len` bytes from `body` into the spool while hashing
    /// them, then stores them under their sha256. Returns the blob and
    /// whether it was new (false: the same bytes were already stored).
    pub fn put(&self, body: &mut dyn Read, len: u64, content_type: &str) -> Result<(Meta, bool)> {
        if len > self.max_bytes {
            bail!(
                "blob of {len} bytes is over the {} byte limit",
                self.max_bytes
            );
        }
        let content_type = clean_content_type(content_type);
        let tmp = self.spool.join(uuid::Uuid::new_v4().to_string());
        let result = (|| {
            let mut file = File::create(&tmp)?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            let mut left = len;
            while left > 0 {
                let want = buf.len().min(left as usize);
                let n = body.read(&mut buf[..want])?;
                if n == 0 {
                    bail!("upload ended after {} of {len} bytes", len - left);
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])?;
                left -= n as u64;
            }
            file.sync_all()?;
            let sha = hex(&hasher.finalize());
            let meta = Meta {
                sha256: sha,
                size: len,
                content_type,
                created_at: Some(now_rfc3339()),
            };
            let created = match &self.backend {
                Backend::Local(dir) => local_put(dir, &tmp, &meta)?,
                Backend::S3(s3) => s3.put(&tmp, &meta)?,
            };
            Ok((meta, created))
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    /// The blob's metadata, or `None` when it is not stored.
    pub fn stat(&self, sha: &str) -> Result<Option<Meta>> {
        check_sha(sha)?;
        match &self.backend {
            Backend::Local(dir) => local_stat(dir, sha),
            Backend::S3(s3) => s3.head(sha),
        }
    }

    /// The blob's metadata and a reader of its bytes, or `None`.
    pub fn open_blob(&self, sha: &str) -> Result<Option<(Meta, Box<dyn Read + Send>)>> {
        check_sha(sha)?;
        match &self.backend {
            Backend::Local(dir) => {
                let Some(meta) = local_stat(dir, sha)? else {
                    return Ok(None);
                };
                Ok(Some((meta, Box::new(File::open(dir.join(sha))?))))
            }
            Backend::S3(s3) => s3.get(sha),
        }
    }

    /// Every stored blob, oldest first when creation times are known.
    pub fn list(&self) -> Result<Vec<Meta>> {
        let mut all = match &self.backend {
            Backend::Local(dir) => {
                let mut out = Vec::new();
                for entry in std::fs::read_dir(dir)? {
                    let name = entry?.file_name().to_string_lossy().into_owned();
                    if check_sha(&name).is_ok() {
                        if let Some(m) = local_stat(dir, &name)? {
                            out.push(m);
                        }
                    }
                }
                out
            }
            Backend::S3(s3) => s3.list()?,
        };
        all.sort_by(|a, b| (&a.created_at, &a.sha256).cmp(&(&b.created_at, &b.sha256)));
        Ok(all)
    }

    /// A signed path (`/b/<sha>[/<name>]?exp=..&sig=..`) readable until `exp`
    /// (epoch seconds).
    pub fn link_path(&self, sha: &str, name: Option<&str>, exp: i64) -> String {
        let name = name.unwrap_or("");
        let sig = hex(&hmac_sha256(
            &self.key,
            link_message(sha, name, exp).as_bytes(),
        ));
        let file = if name.is_empty() {
            String::new()
        } else {
            format!("/{}", crate::channel::pct_encode(name))
        };
        format!("/b/{sha}{file}?exp={exp}&sig={sig}")
    }

    /// Checks a link's signature and expiry at `now` (epoch seconds).
    pub fn verify_link(
        &self,
        sha: &str,
        name: Option<&str>,
        exp: &str,
        sig: &str,
        now: i64,
    ) -> Result<(), LinkError> {
        let exp: i64 = exp.parse().map_err(|_| LinkError::Invalid)?;
        let want = hex(&hmac_sha256(
            &self.key,
            link_message(sha, name.unwrap_or(""), exp).as_bytes(),
        ));
        if want.len() != sig.len()
            || want
                .bytes()
                .zip(sig.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                != 0
        {
            return Err(LinkError::Invalid);
        }
        if now >= exp {
            return Err(LinkError::Expired);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LinkError {
    Invalid,
    Expired,
}

fn link_message(sha: &str, name: &str, exp: i64) -> String {
    format!("axe-blob-link-v1\n{sha}\n{name}\n{exp}")
}

/// Reads the signing key, creating a random one (mode 0600) if missing.
fn link_key(path: &Path) -> Result<Vec<u8>> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => return Ok(s.trim().as_bytes().to_vec()),
        Ok(_) => bail!("link key file {} is empty", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let key = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    match opts.open(path) {
        Ok(mut f) => {
            f.write_all(key.as_bytes())?;
            f.sync_all()?;
            Ok(key.into_bytes())
        }
        // Another server created it first.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => link_key(path),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

/// A sha256 id: 64 lowercase hex digits.
pub fn check_sha(sha: &str) -> Result<()> {
    if sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(())
    } else {
        bail!("invalid blob id {sha:?}: want a lowercase hex sha256")
    }
}

/// A content type safe to send back in a header; anything else is
/// `application/octet-stream`.
pub fn clean_content_type(ct: &str) -> String {
    let ct = ct.trim();
    if !ct.is_empty()
        && ct.len() <= 128
        && ct.contains('/')
        && ct
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/+.-;= _".contains(c))
    {
        ct.to_string()
    } else {
        OCTET_STREAM.to_string()
    }
}

/// Content type from a file name's extension.
pub fn guess_content_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "log" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "json" => "application/json",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => OCTET_STREAM,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

// ---- local backend: `<dir>/<sha>` plus `<dir>/<sha>.json` ----

fn local_stat(dir: &Path, sha: &str) -> Result<Option<Meta>> {
    if !dir.join(sha).is_file() {
        return Ok(None);
    }
    let meta: serde_json::Value = match std::fs::read(dir.join(format!("{sha}.json"))) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => serde_json::Value::Null,
    };
    let size = std::fs::metadata(dir.join(sha))?.len();
    Ok(Some(Meta {
        sha256: sha.to_string(),
        size,
        content_type: meta["content_type"]
            .as_str()
            .map_or_else(|| OCTET_STREAM.to_string(), clean_content_type),
        created_at: meta["created_at"].as_str().map(String::from),
    }))
}

/// Moves the spooled file into place unless the blob exists. The metadata
/// goes first, so a stored blob always has it.
fn local_put(dir: &Path, tmp: &Path, meta: &Meta) -> Result<bool> {
    let data = dir.join(&meta.sha256);
    if data.is_file() {
        return Ok(false);
    }
    let meta_tmp = tmp.with_extension("json");
    std::fs::write(&meta_tmp, serde_json::to_vec(meta)?)?;
    std::fs::rename(&meta_tmp, dir.join(format!("{}.json", meta.sha256)))?;
    std::fs::rename(tmp, &data)?;
    Ok(true)
}

// ---- S3 backend: AWS Signature Version 4 over plain HTTP(S) ----

struct S3 {
    endpoint: String,
    host: String,
    bucket: String,
    region: String,
    prefix: String,
    path_style: bool,
    access_key: String,
    secret_key: String,
    agent: ureq::Agent,
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

impl S3 {
    fn new(cfg: &BlobsS3) -> Result<S3> {
        let read = |f: &str| -> Result<String> {
            let p = config::expand_tilde(f);
            let s = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
            let s = s.trim().to_string();
            if s.is_empty() {
                bail!("{} is empty", p.display());
            }
            Ok(s)
        };
        let endpoint = cfg.endpoint.trim_end_matches('/').to_string();
        let (scheme, host) = endpoint
            .split_once("://")
            .filter(|(s, h)| matches!(*s, "http" | "https") && !h.is_empty() && !h.contains('/'))
            .with_context(|| {
                format!("blobs.s3.endpoint {endpoint:?}: want http(s)://host[:port]")
            })?;
        let (endpoint, host) = if cfg.path_style {
            (endpoint.clone(), host.to_string())
        } else {
            let host = format!("{}.{host}", cfg.bucket);
            (format!("{scheme}://{host}"), host)
        };
        Ok(S3 {
            endpoint,
            host,
            bucket: cfg.bucket.clone(),
            region: cfg.region.clone(),
            prefix: cfg.prefix.clone(),
            path_style: cfg.path_style,
            access_key: read(&cfg.access_key_id_file)?,
            secret_key: read(&cfg.secret_access_key_file)?,
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(120))
                .timeout_write(Duration::from_secs(120))
                .build(),
        })
    }

    fn key(&self, sha: &str) -> String {
        format!("{}sha256/{sha}", self.prefix)
    }

    /// The request path for an object key (or the bucket root for `""`).
    fn path(&self, key: &str) -> String {
        let key = key.split('/').map(uri_encode).collect::<Vec<_>>().join("/");
        if self.path_style {
            format!("/{}/{key}", self.bucket)
        } else {
            format!("/{key}")
        }
    }

    /// A signed request; `query` must already be sorted by key.
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        payload_sha: &str,
    ) -> ureq::Request {
        let now = time::OffsetDateTime::now_utc();
        let amz_date = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        let auth = sigv4(&SigV4 {
            method,
            host: &self.host,
            path,
            query,
            payload_sha,
            amz_date: &amz_date,
            region: &self.region,
            access_key: &self.access_key,
            secret_key: &self.secret_key,
            extra_headers: &[],
        });
        let qs = canonical_query(query);
        let url = if qs.is_empty() {
            format!("{}{path}", self.endpoint)
        } else {
            format!("{}{path}?{qs}", self.endpoint)
        };
        self.agent
            .request(method, &url)
            .set("Host", &self.host)
            .set("X-Amz-Date", &amz_date)
            .set("X-Amz-Content-Sha256", payload_sha)
            .set("Authorization", &auth)
    }

    fn head(&self, sha: &str) -> Result<Option<Meta>> {
        match self
            .request("HEAD", &self.path(&self.key(sha)), &[], EMPTY_SHA256)
            .call()
        {
            Ok(r) => Ok(Some(s3_meta(sha, &r))),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(s3_error("HEAD", e)),
        }
    }

    fn get(&self, sha: &str) -> Result<Option<(Meta, Box<dyn Read + Send>)>> {
        match self
            .request("GET", &self.path(&self.key(sha)), &[], EMPTY_SHA256)
            .call()
        {
            Ok(r) => Ok(Some((s3_meta(sha, &r), Box::new(r.into_reader())))),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(s3_error("GET", e)),
        }
    }

    fn put(&self, file: &Path, meta: &Meta) -> Result<bool> {
        if self.head(&meta.sha256)?.is_some() {
            return Ok(false);
        }
        // The payload hash of a blob is its id.
        self.request(
            "PUT",
            &self.path(&self.key(&meta.sha256)),
            &[],
            &meta.sha256,
        )
        .set("Content-Type", &meta.content_type)
        .set("Content-Length", &meta.size.to_string())
        .send(File::open(file)?)
        .map_err(|e| s3_error("PUT", e))?;
        Ok(true)
    }

    fn list(&self) -> Result<Vec<Meta>> {
        let prefix = format!("{}sha256/", self.prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![];
            if let Some(t) = &token {
                query.push(("continuation-token", t.as_str()));
            }
            query.push(("list-type", "2"));
            query.push(("prefix", prefix.as_str()));
            let body = self
                .request("GET", &self.path(""), &query, EMPTY_SHA256)
                .call()
                .map_err(|e| s3_error("LIST", e))?
                .into_string()?;
            for item in xml_all(&body, "Contents") {
                let Some(sha) = xml_one(item, "Key").and_then(|k| k.strip_prefix(&prefix)) else {
                    continue;
                };
                if check_sha(sha).is_err() {
                    continue;
                }
                out.push(Meta {
                    sha256: sha.to_string(),
                    size: xml_one(item, "Size")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    content_type: String::new(),
                    created_at: xml_one(item, "LastModified").map(String::from),
                });
            }
            token = match xml_one(&body, "IsTruncated") {
                Some("true") => xml_one(&body, "NextContinuationToken").map(xml_unescape),
                _ => None,
            };
            if token.is_none() {
                return Ok(out);
            }
        }
    }
}

fn s3_meta(sha: &str, r: &ureq::Response) -> Meta {
    Meta {
        sha256: sha.to_string(),
        size: r
            .header("Content-Length")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        content_type: clean_content_type(r.header("Content-Type").unwrap_or("")),
        created_at: None,
    }
}

fn s3_error(op: &str, e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            let what = xml_one(&body, "Code").unwrap_or("").to_string();
            anyhow::anyhow!("blob store S3 {op}: HTTP {code} {what}")
        }
        e => anyhow::anyhow!("blob store S3 {op}: {e}"),
    }
}

/// Inner text of each `<tag>...</tag>` in `xml` (no nesting of the same tag).
fn xml_all<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

fn xml_one<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    xml_all(xml, tag).into_iter().next()
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// SigV4 URI encoding: everything but unreserved characters.
fn uri_encode(s: &str) -> String {
    crate::channel::pct_encode(s)
}

fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut q: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    q.sort();
    q.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Inputs of one SigV4 signature (service `s3`).
pub struct SigV4<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// Already URI-encoded path.
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub payload_sha: &'a str,
    /// `YYYYMMDDTHHMMSSZ`
    pub amz_date: &'a str,
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    /// More signed headers as (lowercase name, value).
    pub extra_headers: &'a [(&'a str, &'a str)],
}

/// The `Authorization` header value of a SigV4-signed S3 request.
pub fn sigv4(r: &SigV4) -> String {
    let mut headers: Vec<(&str, &str)> = vec![
        ("host", r.host),
        ("x-amz-content-sha256", r.payload_sha),
        ("x-amz-date", r.amz_date),
    ];
    headers.extend_from_slice(r.extra_headers);
    headers.sort();
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed: Vec<&str> = headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        r.method,
        r.path,
        canonical_query(r.query),
        r.payload_sha
    );
    let date = &r.amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", r.region);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        r.amz_date,
        sha256_hex(canonical.as_bytes())
    );
    let k = hmac_sha256(format!("AWS4{}", r.secret_key).as_bytes(), date.as_bytes());
    let k = hmac_sha256(&k, r.region.as_bytes());
    let k = hmac_sha256(&k, b"s3");
    let k = hmac_sha256(&k, b"aws4_request");
    let sig = hex(&hmac_sha256(&k, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}",
        r.access_key
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// The GET Object example of the AWS SigV4 documentation for S3.
    #[test]
    fn sigv4_matches_the_aws_get_object_example() {
        let auth = sigv4(&SigV4 {
            method: "GET",
            host: "examplebucket.s3.amazonaws.com",
            path: "/test.txt",
            query: &[],
            payload_sha: EMPTY_SHA256,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            extra_headers: &[("range", "bytes=0-9")],
        });
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn links_verify_until_expiry_and_reject_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&Blobs::default(), &dir.path().join("axe.db")).unwrap();
        let sha = sha256_hex(b"x");
        let path = store.link_path(&sha, Some("a b.txt"), 1000);
        assert!(path.starts_with(&format!("/b/{sha}/a%20b.txt?exp=1000&sig=")));
        let sig = path.rsplit_once("sig=").unwrap().1;
        assert_eq!(
            store.verify_link(&sha, Some("a b.txt"), "1000", sig, 999),
            Ok(())
        );
        assert_eq!(
            store.verify_link(&sha, Some("a b.txt"), "1000", sig, 1000),
            Err(LinkError::Expired)
        );
        assert_eq!(
            store.verify_link(&sha, Some("a b.txt"), "2000", sig, 999),
            Err(LinkError::Invalid)
        );
        assert_eq!(
            store.verify_link(&sha, None, "1000", sig, 999),
            Err(LinkError::Invalid)
        );
        // A second open reuses the key, so links survive a restart.
        let again = Store::open(&Blobs::default(), &dir.path().join("axe.db")).unwrap();
        assert_eq!(again.link_path(&sha, Some("a b.txt"), 1000), path);
    }

    #[test]
    fn content_types_are_sanitized() {
        assert_eq!(clean_content_type("image/png"), "image/png");
        assert_eq!(
            clean_content_type("text/html; charset=utf-8"),
            "text/html; charset=utf-8"
        );
        assert_eq!(clean_content_type("text/html\r\nX: y"), OCTET_STREAM);
        assert_eq!(clean_content_type(""), OCTET_STREAM);
    }
}
