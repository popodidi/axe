//! The dashboard, embedded with the `embed-ui` feature and served by
//! `axe server` at `/`. Build it first: `npm ci && npm run build` in `web/`
//! writes the static export to `web/out`, which this module embeds.

use std::borrow::Cow;

#[derive(rust_embed::Embed)]
#[folder = "../../web/out"]
struct Assets;

/// The embedded file for a request path: the file itself, then `<path>.html`
/// and `<path>/index.html` (the static export's routes); `/` is `index.html`.
pub fn asset(path: &[String]) -> Option<(&'static str, Cow<'static, [u8]>)> {
    let joined = path.join("/");
    let candidates = if joined.is_empty() {
        vec!["index.html".to_string()]
    } else {
        vec![
            joined.clone(),
            format!("{joined}.html"),
            format!("{joined}/index.html"),
        ]
    };
    candidates
        .into_iter()
        .find_map(|name| Assets::get(&name).map(|f| (mime(&name), f.data)))
}

/// Whether `path` may be a dashboard file rather than a server route.
pub fn is_ui_path(path: &[String]) -> bool {
    !matches!(
        path.first().map(String::as_str),
        Some("api" | "healthz" | "metrics")
    )
}

fn mime(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
