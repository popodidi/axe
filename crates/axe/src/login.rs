//! Web sign-in: one-time login codes and the login page.
//!
//! `POST /api/login-links` (token) mints a code; `GET /login?code=` redeems
//! it once for the read-session cookie (`server.rs`, `session_route`). The
//! server keeps only each code's SHA-256 and expiry, in memory: nothing goes
//! to the database and a restart drops every open code.
use std::collections::HashMap;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// A code's life when the request names none.
pub const DEFAULT_TTL: i64 = 10 * 60;
pub const MIN_TTL: i64 = 60;
pub const MAX_TTL: i64 = 60 * 60;
/// Most codes open at once; minting more drops the oldest.
const MAX_OPEN: usize = 64;

#[derive(Default)]
pub struct Codes(Mutex<HashMap<[u8; 32], i64>>);

fn digest(code: &str) -> [u8; 32] {
    Sha256::digest(code.as_bytes()).into()
}

impl Codes {
    /// A new code valid for `ttl` seconds from `now`.
    pub fn mint(&self, ttl: i64, now: i64) -> String {
        let mut raw = [0u8; 32];
        rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut raw);
        let code = B64.encode(raw);
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, exp| *exp > now);
        while m.len() >= MAX_OPEN {
            let oldest = *m.iter().min_by_key(|(_, exp)| **exp).expect("not empty").0;
            m.remove(&oldest);
        }
        m.insert(digest(&code), now + ttl);
        code
    }

    /// Takes `code` if it is open at `now`; a code works once.
    pub fn redeem(&self, code: &str, now: i64) -> bool {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, exp| *exp > now);
        m.remove(&digest(code)).is_some()
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Where a sign-in returns to: a path on this server (`/` then not `/` or
/// `\\`, so never another host), with no control characters, else `/`.
pub fn next_or_root(next: Option<&str>) -> &str {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.starts_with("/\\")
                && n.len() <= 2048
                && !n.chars().any(char::is_control) =>
        {
            n
        }
        _ => "/",
    }
}

/// The login page a browser without a session gets, with `note` on top;
/// signing in returns to `next` (a page of this server, see
/// [`next_or_root`]).
pub fn page(note: &str, next: &str) -> String {
    let next = next_or_root(Some(next));
    let back = if next == "/" {
        String::new()
    } else {
        " Signing in brings you back to this page.".to_string()
    };
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>axe: sign in</title>
<style>body{{font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 1rem;color:#222}}
code{{background:#f3f3f3;padding:0 .25rem}}input{{width:100%;font:inherit;padding:.4rem;box-sizing:border-box}}
button{{font:inherit;margin-top:.5rem;padding:.4rem 1rem}}.note{{color:#a33}}</style>
</head><body>
<h1>axe</h1>
<p class="note">{note}</p>
<p>This portal needs a sign-in.{back} On Telegram, ask your bot for a sign-in link
(it runs <code>axe web login-link</code>) and open it in this browser.</p>
<form method="post" action="/login">
<input type="hidden" name="next" value="{next}">
<label for="token">Or sign in with the server token</label>
<input id="token" name="token" type="password" autocomplete="off" required>
<button type="submit">Sign in</button>
</form>
</body></html>
"#,
        note = escape(note),
        next = escape(next),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_works_once_and_expires() {
        let c = Codes::default();
        let a = c.mint(60, 1000);
        assert_eq!(a.len(), 43);
        assert!(!c.redeem("nope", 1000));
        assert!(c.redeem(&a, 1059));
        assert!(!c.redeem(&a, 1059), "used");
        let b = c.mint(60, 1000);
        assert!(!c.redeem(&b, 1060), "expired");
        let first = c.mint(600, 2000);
        for _ in 0..MAX_OPEN {
            c.mint(600, 2001);
        }
        assert!(!c.redeem(&first, 2002), "the oldest goes past the cap");
    }

    #[test]
    fn the_page_escapes_its_note_and_keeps_where_to_return() {
        assert!(page("<b>", "/").contains("&lt;b&gt;"));
        let p = page("", "/?bot=a&tab=board\"><x");
        assert!(
            p.contains(r#"name="next" value="/?bot=a&amp;tab=board&quot;&gt;&lt;x""#),
            "{p}"
        );
        assert!(p.contains("brings you back to this page"));
        assert!(!page("", "/").contains("brings you back"));
    }

    #[test]
    fn a_sign_in_returns_only_to_this_server() {
        assert_eq!(next_or_root(Some("/?bot=a&tab=board")), "/?bot=a&tab=board");
        for bad in [
            None,
            Some(""),
            Some("http://evil.test/"),
            Some("//evil.test/"),
            Some("/\\evil.test"),
            Some("/a\r\nSet-Cookie: x"),
        ] {
            assert_eq!(next_or_root(bad), "/", "{bad:?}");
        }
    }
}
