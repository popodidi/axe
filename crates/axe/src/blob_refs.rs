//! Where blobs are referenced, for the dashboard's store views: file names
//! and tasks taken from task events that carry a signed link
//! (`/b/<sha256>[/<name>]`), and older artifact URLs that link-map docs tie
//! to a blob.
//!
//! A link-map doc is any doc whose path ends in `.links.tsv`: tab-separated,
//! a header row with `url` and `sha256` columns (an optional `name` column
//! names the file). Its URLs resolve to their blobs, so evidence recorded
//! before it moved into the store stays viewable.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::blob;
use crate::doc;

/// Doc path suffix of a link map.
pub const LINK_MAP_SUFFIX: &str = ".links.tsv";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Refs {
    /// File names the blob was linked or mapped under.
    pub names: BTreeSet<String>,
    /// Tasks whose events link the blob.
    pub tasks: BTreeSet<i64>,
}

/// One URL a link map ties to a blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mapped {
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// URLs from every link-map doc, keyed by [`url_key`].
pub fn link_maps(docs: Option<&doc::Store>) -> Result<HashMap<String, Mapped>> {
    let mut out = HashMap::new();
    let Some(docs) = docs else {
        return Ok(out);
    };
    for entry in docs.list(None)? {
        if !entry.path.ends_with(LINK_MAP_SUFFIX) {
            continue;
        }
        if let Some(content) = docs.get(&entry.path, None)?.content {
            parse_link_map(&content, &mut out);
        }
    }
    Ok(out)
}

/// Adds one link map's rows to `out`; rows without a URL or a valid sha256
/// are skipped, and the first mapping of a URL wins.
pub fn parse_link_map(content: &str, out: &mut HashMap<String, Mapped>) {
    let mut lines = content.lines();
    let Some(header) = lines.next() else { return };
    let cols: Vec<&str> = header.split('\t').map(str::trim).collect();
    let find = |n: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(n));
    let (Some(url), Some(sha)) = (find("url"), find("sha256")) else {
        return;
    };
    let name = find("name");
    for line in lines {
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        let (Some(u), Some(s)) = (f.get(url), f.get(sha)) else {
            continue;
        };
        if u.is_empty() || blob::check_sha(s).is_err() {
            continue;
        }
        out.entry(url_key(u)).or_insert_with(|| Mapped {
            sha256: s.to_string(),
            name: name
                .and_then(|i| f.get(i))
                .filter(|n| !n.is_empty())
                .map(|n| n.to_string()),
        });
    }
}

/// A URL compared by its decoded scheme, host and path: the query, fragment
/// and a trailing slash are dropped, so `a%3Ab` and `a:b` match.
pub fn url_key(url: &str) -> String {
    let url = url.trim();
    let url = url.split(['?', '#']).next().unwrap_or("");
    crate::server::pct_decode(url, false)
        .trim_end_matches('/')
        .to_string()
}

/// The signed-link references in `text`: (sha256, file name if any).
pub fn signed_links(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("/b/") {
        rest = &rest[i + 3..];
        let Some(sha) = rest.get(..64) else { break };
        if blob::check_sha(sha).is_err() {
            continue;
        }
        let after = &rest[64..];
        let name = after.strip_prefix('/').and_then(|n| {
            let end = n
                .find(|c: char| c == '?' || c == '#' || c.is_whitespace() || "\"'<>()".contains(c))
                .unwrap_or(n.len());
            let n = crate::server::pct_decode(&n[..end], false);
            (!n.is_empty() && !n.contains('/')).then_some(n)
        });
        out.push((sha.to_string(), name));
        rest = after;
    }
    out
}

/// References by sha256: signed links in any event, and artifacts whose URL
/// a link map resolves (their mapped name too).
pub fn index(conn: &Connection, maps: &HashMap<String, Mapped>) -> Result<BTreeMap<String, Refs>> {
    let mut out: BTreeMap<String, Refs> = BTreeMap::new();
    let mut q = conn.prepare(
        "SELECT task_id, kind, COALESCE(body, to_value, '') FROM events
         WHERE body LIKE '%/b/%' OR to_value LIKE '%/b/%' OR kind = 'artifact'",
    )?;
    let rows = q.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (task, kind, text) = row?;
        for (sha, name) in signed_links(&text) {
            let e = out.entry(sha).or_default();
            e.tasks.insert(task);
            e.names.extend(name);
        }
        if kind == "artifact" {
            if let Some(m) = maps.get(&url_key(&text)) {
                let e = out.entry(m.sha256.clone()).or_default();
                e.tasks.insert(task);
                e.names
                    .extend(m.name.clone().or_else(|| last_segment(&text)));
            }
        }
    }
    Ok(out)
}

/// The decoded last path segment of a URL, if any.
pub fn last_segment(url: &str) -> Option<String> {
    let key = url_key(url);
    let path = key.split_once("://").map_or(key.as_str(), |(_, p)| p);
    let (_, last) = path.rsplit_once('/')?;
    (!last.is_empty()).then(|| last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "60114b3eef416f5af3679f3bfe8d5337b1e6bee80a4138c4db15f302eaeb7747";

    #[test]
    fn signed_links_are_found_in_free_text() {
        let text = format!(
            "see http://axe.hlab/b/{SHA}/shot%20one.png?exp=1&sig=2 and (/b/{SHA}) or /b/nothex"
        );
        assert_eq!(
            signed_links(&text),
            vec![
                (SHA.to_string(), Some("shot one.png".to_string())),
                (SHA.to_string(), None)
            ]
        );
    }

    #[test]
    fn link_maps_match_decoded_urls() {
        let mut m = HashMap::new();
        parse_link_map(
            &format!("sha256\turl\tname\n{SHA}\thttp://files.lan/a/dispatch:x/m.json\t\nbad\thttp://x/y\t\n"),
            &mut m,
        );
        assert_eq!(m.len(), 1);
        let hit = &m[&url_key("http://files.lan/a/dispatch%3Ax/m.json?raw=1")];
        assert_eq!(hit.sha256, SHA);
        assert_eq!(hit.name, None);
        assert_eq!(
            last_segment("http://files.lan/a/dispatch%3Ax/m.json"),
            Some("m.json".into())
        );
        // No url column: not a link map.
        let mut none = HashMap::new();
        parse_link_map(&format!("path\tsha256\na\t{SHA}\n"), &mut none);
        assert!(none.is_empty());
    }
}
