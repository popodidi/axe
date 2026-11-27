//! Accuracy of the real speech engine (`transcribe.rs`) on two public FLEURS
//! clips (`tests/fixtures/voice/`), as Ogg/Opus voice notes. Ignored by
//! default: the first run downloads the pinned engine and model (about 190
//! MB) into the cache (`$AXE_STT_CACHE`, else `~/.cache/axe/stt`). Run it with
//! `cargo test --test transcribe_fleurs -- --ignored --nocapture`.
use std::path::{Path, PathBuf};

use axe::transcribe::{self, SenseVoice};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/voice")
        .join(name)
}

fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, y) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(x != y);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Character error rate over letters and digits only (Chinese).
fn cer(hyp: &str, reference: &str) -> f64 {
    let norm = |s: &str| -> Vec<char> { s.chars().filter(|c| c.is_alphanumeric()).collect() };
    let (h, r) = (norm(hyp), norm(reference));
    levenshtein(&h, &r) as f64 / r.len() as f64
}

/// Word error rate, case and punctuation (apostrophes too) ignored.
fn wer(hyp: &str, reference: &str) -> f64 {
    let norm = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split_whitespace()
            .map(|w| {
                w.chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect::<String>()
            })
            .filter(|w| !w.is_empty())
            .collect()
    };
    let (h, r) = (norm(hyp), norm(reference));
    levenshtein(&h, &r) as f64 / r.len() as f64
}

#[test]
fn error_rates_are_within_bounds() {
    assert!((cer("报告，警告", "报告警告。") - 0.0).abs() < 1e-9);
    assert!((wer("When you", "when you, now") - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
#[ignore = "downloads the speech engine and model on first run"]
fn sensevoice_transcribes_the_fleurs_clips() {
    let cache = std::env::var_os("AXE_STT_CACHE")
        .map(PathBuf::from)
        .or_else(transcribe::default_dir)
        .expect("a cache folder");
    let engine = SenseVoice::new(cache);
    // Installs on first use; not part of the measured transcriptions.
    engine.ensure().expect("install the engine and model");
    for (clip, max, rate) in [
        ("zh-fleurs", 0.10, cer as fn(&str, &str) -> f64),
        ("en-fleurs", 0.20, wer),
    ] {
        let audio = std::fs::read(fixture(&format!("{clip}.ogg"))).unwrap();
        let reference = std::fs::read_to_string(fixture(&format!("{clip}.txt"))).unwrap();
        let t = transcribe::transcribe(&engine, &audio);
        let text = t.text.clone().unwrap_or_else(|| panic!("{clip}: {t:?}"));
        let err = rate(&text, reference.trim());
        eprintln!(
            "{clip}: {:.1}% error, {:.1}s audio, {} ms, peak {:?} MB, lang {:?}\n  ref: {}\n  got: {text}",
            err * 100.0,
            t.audio_secs.unwrap_or_default(),
            t.ms,
            t.peak_rss_mb,
            t.lang,
            reference.trim()
        );
        assert!(err <= max, "{clip}: error {err:.3} over {max}: {text}");
    }
}
