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

/// The login page a browser without a session gets, with `note` on top.
pub fn page(note: &str) -> String {
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
<p>This portal needs a sign-in. Get a one-time link with <code>axe web login-link</code>
(or <code>axe web login-link --tg</code> to receive it sealed on Telegram) and open it in this browser.</p>
<form method="post" action="/login">
<label for="token">Or sign in with the server token</label>
<input id="token" name="token" type="password" autocomplete="off" required>
<button type="submit">Sign in</button>
</form>
</body></html>
"#,
        note = escape(note)
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
    fn the_page_escapes_its_note() {
        assert!(page("<b>").contains("&lt;b&gt;"));
    }
}
