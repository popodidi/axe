//! `web/lib/api.ts` holds the TypeScript form of the API types the dashboard
//! reads. This test fails when it drifts from the Rust types; regenerate with
//! `AXE_BLESS=1 cargo test -p axe-os --features ts --test ts_bindings`.
#![cfg(feature = "ts")]

use std::path::PathBuf;

use axe::{bot, db, schedule};
use ts_rs::{Config, TS};

fn bindings() -> String {
    // The API serialises integers as JSON numbers, and ids stay far below 2^53.
    let cfg = Config::new().with_large_int("number");
    let decls = [
        db::Task::decl(&cfg),
        db::Deps::decl(&cfg),
        db::DepRef::decl(&cfg),
        db::Event::decl(&cfg),
        db::RunRecord::decl(&cfg),
        bot::Bot::decl(&cfg),
        bot::Message::decl(&cfg),
        bot::Wake::decl(&cfg),
        bot::BotRun::decl(&cfg),
        bot::Event::decl(&cfg),
        bot::MessageMeta::decl(&cfg),
        bot::TaskAction::decl(&cfg),
        bot::Activity::decl(&cfg),
        schedule::Schedule::decl(&cfg),
        serde_json::Value::decl(&cfg),
    ];
    let mut out = String::from(
        "// Generated from the Rust API types by crates/axe/tests/ts_bindings.rs.\n\
         // Do not edit; regenerate with\n\
         // AXE_BLESS=1 cargo test -p axe-os --features ts --test ts_bindings\n",
    );
    for decl in decls {
        out.push_str("\nexport ");
        out.push_str(&decl);
        out.push('\n');
    }
    out
}

#[test]
fn web_api_types_match_rust() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/lib/api.ts");
    let want = bindings();
    if std::env::var_os("AXE_BLESS").is_some() {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        have == want,
        "web/lib/api.ts is stale; run AXE_BLESS=1 cargo test -p axe-os --features ts --test ts_bindings"
    );
}
