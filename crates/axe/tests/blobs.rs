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
