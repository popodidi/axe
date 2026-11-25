//! `axe blob` and the `/api/blobs` + `/b/...` routes, on the local backend and
//! (when `AXE_TEST_S3_ENDPOINT` is set) an S3-compatible one such as MinIO:
//!
//! ```sh
//! AXE_TEST_S3_ENDPOINT=http://127.0.0.1:9000 AXE_TEST_S3_ACCESS_KEY=... \
//!   AXE_TEST_S3_SECRET_KEY=... cargo test --test blobs
//! ```
mod common;

use std::path::Path;

use common::{Server, TOKEN};
use serde_json::Value;

fn sha256(bytes: &[u8]) -> String {
    axe::blob::sha256_hex(bytes)
}

fn json(out: &str) -> Value {
    serde_json::from_str(out).unwrap()
}

/// (status, headers lowercased, body) of a plain GET with no token.
fn fetch(url: &str) -> (u16, ureq::Response) {
    match ureq::get(url).call() {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(c, r)) => (c, r),
        Err(e) => panic!("GET {url}: {e}"),
    }
}

fn body(r: ureq::Response) -> Vec<u8> {
    let mut b = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut b).unwrap();
    b
}

/// The store contract both backends meet.
fn exercise(srv: &Server, dir: &Path) {
    // A body over the 8 MiB JSON request limit: uploads stream.
    let big: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let big_path = dir.join("big.bin");
    std::fs::write(&big_path, &big).unwrap();
    let png = dir.join("shot.png");
    std::fs::write(&png, b"\x89PNG not really").unwrap();

    // Put and get round trip.
    let v = json(&srv.ok(&["blob", "put", big_path.to_str().unwrap(), "--json"]));
    assert_eq!(v["sha256"], sha256(&big));
    assert_eq!(v["size"], big.len() as u64);
    assert_eq!(v["created"], true);
    assert_eq!(v["content_type"], "application/octet-stream");
    let out = dir.join("big.out");
    srv.ok(&["blob", "get", &sha256(&big), "-o", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(&out).unwrap(), big);

    // The same bytes again: deduplicated by content hash.
    let again = json(&srv.ok(&["blob", "put", big_path.to_str().unwrap(), "--json"]));
    assert_eq!(again["sha256"], v["sha256"]);
    assert_eq!(again["created"], false);
    let copy = dir.join("copy.bin");
    std::fs::write(&copy, &big).unwrap();
    let third = json(&srv.ok(&["blob", "put", copy.to_str().unwrap(), "--json"]));
    assert_eq!(third["created"], false);

    // Content type from the extension, stdout download.
    let p = json(&srv.ok(&["blob", "put", png.to_str().unwrap(), "--json"]));
    assert_eq!(p["content_type"], "image/png");
    let psha = p["sha256"].as_str().unwrap().to_string();
    let got = srv.cli(&["blob", "get", &psha]).output().unwrap();
    assert!(got.status.success());
    assert_eq!(got.stdout, b"\x89PNG not really");

    // ls shows each blob once.
    let ls = json(&srv.ok(&["blob", "ls", "--json"]));
    let mut shas: Vec<&str> = ls
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["sha256"].as_str().unwrap())
        .collect();
    shas.sort();
    let mut want = vec![sha256(&big), psha.clone()];
    want.sort();
    assert_eq!(shas, want);

    // A signed link opens without the token.
    let link = json(&srv.ok(&["blob", "link", &psha, "--name", "shot.png", "--json"]));
    let url = link["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{}/b/{psha}/shot.png?exp=", srv.url)));
    let (status, r) = fetch(url);
    assert_eq!(status, 200);
    assert_eq!(r.header("content-type"), Some("image/png"));
    assert_eq!(r.header("content-security-policy"), Some("sandbox"));
    assert!(r
        .header("content-disposition")
        .unwrap()
        .contains("shot.png"));
    assert_eq!(body(r), b"\x89PNG not really");

    // Tampering with the blob, name or expiry breaks the signature.
    let other = url.replace(&psha, &sha256(&big));
    assert_eq!(fetch(&other).0, 403);
    assert_eq!(fetch(&url.replace("/shot.png?", "/x.png?")).0, 403);
    assert_eq!(fetch(&url.replace("exp=", "exp=1")).0, 403);
    assert_eq!(fetch(&url.replace("sig=", "sig=0")).0, 403);
    assert_eq!(fetch(&format!("{}/b/{psha}", srv.url)).0, 403);

    // An expired link is rejected.
    let short = json(&srv.ok(&["blob", "link", &psha, "--ttl", "1s", "--json"]));
    let short = short["url"].as_str().unwrap();
    assert_eq!(fetch(short).0, 200);
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let (status, r) = fetch(short);
    assert_eq!(status, 403);
    assert!(String::from_utf8(body(r)).unwrap().contains("expired"));

    // Everything else needs the token.
    for path in ["/api/blobs".to_string(), format!("/api/blobs/{psha}")] {
        assert_eq!(fetch(&format!("{}{path}", srv.url)).0, 401, "{path}");
    }
    let anon = ureq::post(&format!("{}/api/blobs", srv.url)).send_bytes(b"x");
    assert!(matches!(anon, Err(ureq::Error::Status(401, _))));

    // Unknown and malformed ids.
    let missing = sha256(b"never stored");
    let out = srv.run(&["blob", "get", &missing]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    let out = srv.run(&["blob", "link", &missing]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
    let bad = ureq::get(&format!("{}/api/blobs/NOTAHASH", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call();
    assert!(matches!(bad, Err(ureq::Error::Status(400, _))));
    let out = srv.run(&["blob", "link", &psha, "--ttl", "8d"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("ttl"));
}

#[test]
fn local_store_round_trip_dedup_and_links() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    exercise(&srv, dir.path());
    // One copy of the deduplicated bytes on disk, next to the database.
    let files: Vec<_> = std::fs::read_dir(dir.path().join("blobs/sha256"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.ends_with(".json"))
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    assert_eq!(
        std::fs::read_dir(dir.path().join("blobs/tmp"))
            .unwrap()
            .count(),
        0,
        "the upload spool is emptied"
    );
}

#[test]
fn links_survive_a_restart_and_use_public_url() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[blobs]\npublic_url = \"https://files.example/\"\n",
    )
    .unwrap();
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    let f = dir.path().join("a.txt");
    std::fs::write(&f, "hi").unwrap();
    let out = srv.ok(&["blob", "put", f.to_str().unwrap(), "--link", "1h"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], sha256(b"hi"));
    let path = lines[1]
        .strip_prefix("https://files.example/b/")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(path.starts_with(&format!("{}/a.txt?exp=", sha256(b"hi"))));
    drop(srv);
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    let (status, r) = fetch(&format!("{}/b/{path}", srv.url));
    assert_eq!(status, 200);
    assert_eq!(body(r), b"hi");
}

/// A fresh bucket on the S3 endpoint, created with axe's own signer.
fn make_bucket(endpoint: &str, access: &str, secret: &str, bucket: &str) {
    let host = endpoint.split_once("://").unwrap().1;
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
    let empty = sha256(b"");
    let path = format!("/{bucket}");
    let auth = axe::blob::sigv4(&axe::blob::SigV4 {
        method: "PUT",
        host,
        path: &path,
        query: &[],
        payload_sha: &empty,
        amz_date: &amz_date,
        region: "us-east-1",
        access_key: access,
        secret_key: secret,
        extra_headers: &[],
    });
    ureq::put(&format!("{endpoint}{path}"))
        .set("X-Amz-Date", &amz_date)
        .set("X-Amz-Content-Sha256", &empty)
        .set("Authorization", &auth)
        .call()
        .unwrap();
}

#[test]
fn s3_store_round_trip_dedup_and_links() {
    let Ok(endpoint) = std::env::var("AXE_TEST_S3_ENDPOINT") else {
        eprintln!(
            "skipped: set AXE_TEST_S3_ENDPOINT, AXE_TEST_S3_ACCESS_KEY, AXE_TEST_S3_SECRET_KEY"
        );
        return;
    };
    let access = std::env::var("AXE_TEST_S3_ACCESS_KEY").unwrap();
    let secret = std::env::var("AXE_TEST_S3_SECRET_KEY").unwrap();
    let bucket = format!("axe-test-{}", uuid::Uuid::new_v4().simple());
    make_bucket(&endpoint, &access, &secret, &bucket);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ak"), &access).unwrap();
    std::fs::write(dir.path().join("sk"), &secret).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[blobs.s3]\nendpoint = \"{endpoint}\"\nbucket = \"{bucket}\"\nprefix = \"t/\"\n\
             access_key_id_file = \"{}\"\nsecret_access_key_file = \"{}\"\n",
            dir.path().join("ak").display(),
            dir.path().join("sk").display()
        ),
    )
    .unwrap();
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    exercise(&srv, dir.path());
    // Contents went to the bucket, not the local directory.
    assert_eq!(
        std::fs::read_dir(dir.path().join("blobs"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name() == "sha256")
            .count(),
        0
    );
}

/// GET with an optional cookie and no token.
fn fetch_with(url: &str, cookie: Option<&str>) -> (u16, ureq::Response) {
    let mut r = ureq::get(url);
    if let Some(c) = cookie {
        r = r.set("Cookie", c);
    }
    match r.call() {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(c, r)) => (c, r),
        Err(e) => panic!("GET {url}: {e}"),
    }
}

/// The dashboard's read session: the token buys an HttpOnly cookie that reads
/// blobs and docs and signs links, and nothing else.
#[test]
fn read_session_cookie_reads_but_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let f = dir.path().join("page.html");
    std::fs::write(&f, "<script>alert(1)</script>").unwrap();
    let sha = srv
        .ok(&["blob", "put", f.to_str().unwrap()])
        .trim()
        .to_string();

    let (status, r) = fetch(&format!("{}/api/session", srv.url));
    assert_eq!(status, 200);
    assert_eq!(json(&r.into_string().unwrap())["read"], false);
    let anon = ureq::post(&format!("{}/api/session", srv.url)).call();
    assert!(matches!(anon, Err(ureq::Error::Status(401, _))));

    let r = ureq::post(&format!("{}/api/session", srv.url))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap();
    let set = r.header("set-cookie").unwrap().to_string();
    assert!(
        set.contains("HttpOnly") && set.contains("SameSite=Lax") && set.contains("Max-Age=2592000"),
        "{set}"
    );
    assert_eq!(json(&r.into_string().unwrap())["read"], true);
    let cookie = set.split(';').next().unwrap().to_string();

    let (status, r) = fetch_with(&format!("{}/api/session", srv.url), Some(&cookie));
    assert_eq!(status, 200);
    assert_eq!(json(&r.into_string().unwrap())["read"], true);
    // Store reads work; blob bytes never run on the server's origin.
    let (status, r) = fetch_with(&format!("{}/api/blobs/{sha}", srv.url), Some(&cookie));
    assert_eq!(status, 200);
    assert_eq!(r.header("content-security-policy"), Some("sandbox"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        fetch_with(&format!("{}/api/blobs", srv.url), Some(&cookie)).0,
        200
    );
    assert_eq!(
        fetch_with(&format!("{}/api/docs", srv.url), Some(&cookie)).0,
        200
    );
    assert_eq!(
        fetch_with(&format!("{}/api/store/stats", srv.url), Some(&cookie)).0,
        200
    );
    let link = ureq::post(&format!("{}/api/blobs/{sha}/link", srv.url))
        .set("Cookie", &cookie)
        .send_json(serde_json::json!({"ttl": 60, "name": "page.html"}))
        .unwrap();
    assert!(json(&link.into_string().unwrap())["path"]
        .as_str()
        .unwrap()
        .starts_with(&format!("/b/{sha}/page.html?exp=")));
    // A service's recent log, for the Services page (the server answers,
    // here that the bot does not exist, instead of refusing the session).
    assert_ne!(
        fetch_with(
            &format!("{}/api/bots/nobot/services/s/logs", srv.url),
            Some(&cookie)
        )
        .0,
        401
    );
    // Other reads too (the portal), but not message content.
    assert_eq!(
        fetch_with(&format!("{}/api/tasks", srv.url), Some(&cookie)).0,
        200
    );
    // Not uploads, other private reads or writes.
    let up = ureq::post(&format!("{}/api/blobs", srv.url))
        .set("Cookie", &cookie)
        .send_bytes(b"x");
    assert!(matches!(up, Err(ureq::Error::Status(401, _))));
    assert_eq!(
        fetch_with(&format!("{}/api/forms", srv.url), Some(&cookie)).0,
        401
    );
    let add = ureq::post(&format!("{}/api/tasks", srv.url))
        .set("Cookie", &cookie)
        .send_json(serde_json::json!({"project": "p", "title": "t"}));
    assert!(matches!(add, Err(ureq::Error::Status(401, _))));
    // A forged or ended session reads nothing.
    let forged = cookie.replace('.', ".0");
    assert_eq!(
        fetch_with(&format!("{}/api/blobs", srv.url), Some(&forged)).0,
        401
    );
    let end = ureq::post(&format!("{}/api/session-end", srv.url))
        .call()
        .unwrap();
    assert!(end.header("set-cookie").unwrap().contains("Max-Age=0"));
}

/// A raw GET to the test server with any `Host` (and cookie, bearer):
/// (status, lowercased headers, body).
fn raw_get(
    srv: &Server,
    host: &str,
    target: &str,
    extra: &str,
) -> (u16, std::collections::HashMap<String, String>, String) {
    raw(
        srv,
        &format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n"),
    )
}

fn raw(srv: &Server, head: &str) -> (u16, std::collections::HashMap<String, String>, String) {
    use std::io::{Read, Write};
    let addr = srv.url.strip_prefix("http://").unwrap();
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, body.to_string())
}

/// `[blobs] link_url`: links are issued for and served on a separate blob
/// site, where HTML may run inline scripts in a no-network sandbox; the
/// server's own host redirects old links there, and the blob site answers
/// nothing but links, so it never accepts or sets the session cookie.
#[test]
fn blob_site_serves_links_with_scripts_redirects_and_takes_no_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[blobs]\npublic_url = \"http://axe.test/\"\nlink_url = \"http://blobs.test/\"\n",
    )
    .unwrap();
    let srv = Server::start_with(dir.path(), &config, Some(TOKEN), &[], &[]);
    let page = "<p id=o>no</p><script>o.textContent='js ran'</script>";
    let f = dir.path().join("page.html");
    std::fs::write(&f, page).unwrap();
    let sha = srv
        .ok(&["blob", "put", f.to_str().unwrap()])
        .trim()
        .to_string();

    // Links name the blob site.
    let link = json(&srv.ok(&["blob", "link", &sha, "--name", "page.html", "--json"]));
    let url = link["url"].as_str().unwrap();
    let target = url
        .strip_prefix("http://blobs.test")
        .unwrap_or_else(|| panic!("{url}"));
    assert_eq!(target, link["path"].as_str().unwrap());
    assert!(target.starts_with(&format!("/b/{sha}/page.html?exp=")));

    // Served on the blob site: scripts and https subresources, no requests
    // of its own, no sniffing, no cookie.
    for host in ["blobs.test", "BLOBS.test:80"] {
        let (status, h, body) = raw_get(&srv, host, target, "");
        assert_eq!(status, 200, "{host}");
        assert_eq!(body, page);
        assert_eq!(
            h["content-security-policy"],
            "sandbox allow-scripts; default-src 'none'; \
             script-src 'unsafe-inline' https:; style-src 'unsafe-inline' https:; \
             font-src https: data:; img-src https: data:; connect-src 'none'; \
             form-action 'none'"
        );
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["access-control-allow-origin"], "*");
        assert!(h["content-type"].starts_with("text/html"));
        assert!(!h.contains_key("set-cookie"));
    }
    let bad = target.replace("sig=", "sig=0");
    assert_eq!(raw_get(&srv, "blobs.test", &bad, "").0, 403);

    // The server's host (by name or address) redirects to the blob site.
    for host in ["axe.test", srv.url.strip_prefix("http://").unwrap()] {
        let (status, h, _) = raw_get(&srv, host, target, "");
        assert_eq!(status, 302, "{host}");
        assert_eq!(h["location"], format!("http://blobs.test{target}"));
    }
    // The API stays on the server's host.
    assert_eq!(fetch(&format!("{}/api/blobs", srv.url)).0, 401);

    // A read session from the server's host...
    let (status, h, _) = raw(
        &srv,
        &format!(
            "POST /api/session HTTP/1.1\r\nHost: axe.test\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\n\r\n"
        ),
    );
    assert_eq!(status, 200);
    let cookie = h["set-cookie"].split(';').next().unwrap().to_string();
    assert!(!h["set-cookie"].contains("Domain"), "host-only cookie");
    assert_eq!(
        raw_get(
            &srv,
            "axe.test",
            "/api/blobs",
            &format!("Cookie: {cookie}\r\n")
        )
        .0,
        200
    );
    // ...is not accepted on the blob site, which serves no API, sign-in or
    // session, even with the token.
    let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
    let with_cookie = format!("Cookie: {cookie}\r\n");
    for (path, extra) in [
        ("/api/blobs", with_cookie.as_str()),
        (&*format!("/api/blobs/{sha}"), with_cookie.as_str()),
        ("/api/session", with_cookie.as_str()),
        ("/api/tasks", bearer.as_str()),
        ("/api/blobs", bearer.as_str()),
        ("/login", ""),
        ("/", with_cookie.as_str()),
        ("/healthz", ""),
    ] {
        let (status, h, _) = raw_get(&srv, "blobs.test", path, extra);
        assert_eq!(status, 404, "{path}");
        assert!(!h.contains_key("set-cookie"), "{path}");
    }
    let (status, h, _) = raw(
        &srv,
        &format!(
            "POST /api/session HTTP/1.1\r\nHost: blobs.test\r\n{bearer}Content-Length: 0\r\n\r\n"
        ),
    );
    assert_eq!(status, 404);
    assert!(!h.contains_key("set-cookie"));
}

/// `?refs=1` names blobs and their tasks from signed links and link-map docs;
/// `resolve` maps a recorded URL to its blob.
#[test]
fn refs_and_resolve_use_links_and_link_maps() {
    let dir = tempfile::tempdir().unwrap();
    let srv = Server::start(dir.path());
    let put = |name: &str, body: &str| {
        let f = dir.path().join(name);
        std::fs::write(&f, body).unwrap();
        srv.ok(&["blob", "put", f.to_str().unwrap()])
            .trim()
            .to_string()
    };
    let new = put("new.png", "png");
    let old = put("old.json", "{}");
    let lone = put("lone.txt", "x");
    let link = srv.ok(&["blob", "link", &new, "--name", "shot one.png"]);
    let link = link.trim();
    let t1 = srv
        .ok(&["add", "--project", "p", "--title", "a"])
        .trim()
        .to_string();
    let t2 = srv
        .ok(&["add", "--project", "p", "--title", "b"])
        .trim()
        .to_string();
    srv.ok(&["artifact", &t1, link]);
    srv.ok(&["note", &t2, &format!("evidence {link} ok")]);
    let old_url = "http://files.lan/run/dispatch%3Aab/manifest.json";
    srv.ok(&["artifact", &t2, old_url]);
    let map = dir.path().join("map.tsv");
    std::fs::write(
        &map,
        format!("url\tsha256\nhttp://files.lan/run/dispatch:ab/manifest.json\t{old}\n"),
    )
    .unwrap();
    srv.ok(&["doc", "put", "ref/old.links.tsv", map.to_str().unwrap()]);

    let auth = format!("Bearer {TOKEN}");
    let get = |path: &str| -> Value {
        json(
            &ureq::get(&format!("{}{path}", srv.url))
                .set("Authorization", &auth)
                .call()
                .unwrap()
                .into_string()
                .unwrap(),
        )
    };
    let all = get("/api/blobs?refs=1");
    let by = |sha: &str| {
        all.as_array()
            .unwrap()
            .iter()
            .find(|b| b["sha256"] == sha)
            .unwrap()
            .clone()
    };
    let (t1, t2): (i64, i64) = (t1.parse().unwrap(), t2.parse().unwrap());
    assert_eq!(by(&new)["names"], serde_json::json!(["shot one.png"]));
    assert_eq!(by(&new)["tasks"], serde_json::json!([t1, t2]));
    assert_eq!(by(&old)["names"], serde_json::json!(["manifest.json"]));
    assert_eq!(by(&old)["tasks"], serde_json::json!([t2]));
    assert_eq!(by(&lone)["tasks"], serde_json::json!([]));
    // Without the flag the listing is unchanged.
    assert!(get("/api/blobs")[0].get("names").is_none());

    let q = |u: &str| format!("/api/blobs/resolve?url={}", axe::channel::pct_encode(u));
    let hit = get(&q(old_url));
    assert_eq!(hit["sha256"], old);
    assert_eq!(hit["name"], "manifest.json");
    assert_eq!(hit["content_type"], "application/json");
    assert_eq!(get(&q(link))["sha256"], new);
    let miss = ureq::get(&format!("{}{}", srv.url, q("http://files.lan/other")))
        .set("Authorization", &auth)
        .call();
    assert!(matches!(miss, Err(ureq::Error::Status(404, _))));
    assert_eq!(fetch(&format!("{}{}", srv.url, q(old_url))).0, 401);

    // Stats: every blob is new, `lone` is the one no task links.
    let st = get("/api/store/stats");
    let tally = |count: u64, bytes: u64| serde_json::json!({"count": count, "bytes": bytes});
    assert_eq!(st["total"], tally(3, 6));
    assert_eq!(st["recent"], tally(3, 6));
    assert_eq!(st["recent_days"], 7);
    assert_eq!(st["unreferenced"], tally(1, 1));
    assert_eq!(
        st["types"],
        serde_json::json!([
            {"content_type": "image/png", "count": 1, "bytes": 3},
            {"content_type": "application/json", "count": 1, "bytes": 2},
            {"content_type": "text/plain", "count": 1, "bytes": 1},
        ])
    );
    assert_eq!(st["other_types"], tally(0, 0));
    let map_len = std::fs::metadata(&map).unwrap().len();
    assert_eq!(st["docs"], tally(1, map_len));
    assert_eq!(fetch(&format!("{}/api/store/stats", srv.url)).0, 401);
}
