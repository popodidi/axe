//! The repo registry: the server's map from a repo name (as in a task's,
//! bot's or routine's `repos`) to the Git URL its canonical clone is cloned
//! from. It is the only source of clone URLs: any machine running
//! `axe workspace` or the node clones a repo it has never seen from here
//! (`worktree::clone_url`); a machine config keeps only `[repos.<name>] path`.
//! The server refuses to store a repo name that is not registered
//! ([`require_registered`]), so a missing registration surfaces when the repo
//! is named, not on the first machine that needs to clone it.
//!
//! URLs are checked by [`crate::config::validate_repo_url`]: never a leading
//! `-`, never embedded credentials. Clones use each host's own Git
//! credentials.
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db;

/// v21: additive, so a database lowered to `user_version` 20 still opens with
/// that binary and upgrades again in place.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repos (
 name TEXT PRIMARY KEY, url TEXT NOT NULL, default_branch TEXT,
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);";

/// One registered repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub name: String,
    pub url: String,
    /// For reference; task worktrees start from the clone's `origin/HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// What a write sets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRepo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

impl NewRepo {
    pub fn validate(&self) -> Result<()> {
        crate::config::validate_repo_url(&self.url)?;
        if let Some(b) = &self.default_branch {
            if b.is_empty()
                || b.starts_with('-')
                || b.contains("..")
                || b.chars()
                    .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
            {
                bail!("default_branch {b:?} is not a branch name");
            }
        }
        Ok(())
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    Ok(Repo {
        name: r.get(0)?,
        url: r.get(1)?,
        default_branch: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

const COLUMNS: &str = "name, url, default_branch, created_at, updated_at";

/// Every registered repo, by name.
pub fn list(conn: &Connection) -> Result<Vec<Repo>> {
    let mut st = conn.prepare(&format!("SELECT {COLUMNS} FROM repos ORDER BY name"))?;
    let rows = st.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<Repo>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM repos WHERE name=?1"),
            [name],
            row,
        )
        .optional()?)
}

/// Refuses `names` that are not registered, except those in `known` (what
/// the record already held, so an edit never fails over a repo it keeps).
pub fn require_registered(conn: &Connection, names: &[String], known: &[String]) -> Result<()> {
    let names = db::names("repo", names)?;
    let mut missing = Vec::new();
    for n in &names {
        if !known.contains(n) && get(conn, n)?.is_none() {
            missing.push(n.clone());
        }
    }
    match missing.as_slice() {
        [] => Ok(()),
        [one] => bail!(
            "repo {one} is not registered: register its clone URL first with \
             `axe repo add {one} <url>` (`axe repo ls` lists registered repos)"
        ),
        many => bail!(
            "repos {} are not registered: register each clone URL first with \
             `axe repo add <name> <url>` (`axe repo ls` lists registered repos)",
            many.join(", ")
        ),
    }
}

/// Registers `name`, or replaces its URL and default branch.
pub fn put(conn: &Connection, name: &str, new: &NewRepo) -> Result<Repo> {
    db::validate_name("repo", name)?;
    new.validate()?;
    let now = db::now();
    conn.execute(
        "INSERT INTO repos (name, url, default_branch, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(name) DO UPDATE SET url=excluded.url,
           default_branch=excluded.default_branch, updated_at=excluded.updated_at",
        params![name, new.url, new.default_branch, now],
    )?;
    Ok(get(conn, name)?.expect("repo just written"))
}

/// Removes `name`; returns what it held.
pub fn remove(conn: &Connection, name: &str) -> Result<Repo> {
    let Some(r) = get(conn, name)? else {
        bail!("repo {name} not found");
    };
    conn.execute("DELETE FROM repos WHERE name=?1", [name])?;
    Ok(r)
}
