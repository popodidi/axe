//! Voice transcription, the core action behind a bot's voice messages: the
//! node hands it a voice message's audio (Telegram Ogg/Opus, fetched through
//! `GET /api/bots/<bot>/tg/file/<msg>`) and it returns a [`Transcript`]; the
//! node stores it on the message (`bot::set_transcript`) and the prompt line
//! carries it with [`MARK`] (`bot::wake_line`). It never fails: an error is
//! a transcript with `error` set, so the bot still gets the message and its
//! attachment.
//!
//! The engine runs locally, with no API or key: SenseVoice-Small (int8)
//! through the sherpa-onnx `sherpa-onnx-offline` binary in a child process,
//! so the model is loaded for one message and its memory is gone when the
//! process exits. Engine and model archives are pinned by URL and sha256
//! ([`ENGINES`], [`MODEL`]) and fetched once, on first use, into
//! `~/.cache/axe/stt`. Another engine is another [`Engine`].
//!
//! One switch turns it off: `transcribe_voice = false` under `[defaults]` in
//! the node config, resolved with the rest of a run's config
//! (`runtime_config::RuntimeConfig::transcribe_voice`).
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Leads the text of a transcribed voice message in a bot's prompt.
pub const MARK: &str = "[voice message, auto-transcribed, may contain errors]";
/// Leads a voice message whose transcription failed; the reason follows.
pub const FAILED: &str = "[voice message, transcription failed";

/// Longest voice message transcribed; a longer one gets a failure marker.
pub const MAX_AUDIO_SECS: f64 = 300.0;
/// Largest voice file read (Telegram bots download at most 20 MB).
pub const MAX_AUDIO_BYTES: usize = 20 << 20;
/// How long one engine run may take before it is killed.
pub const ENGINE_TIMEOUT: Duration = Duration::from_secs(120);
/// The rate the engine reads.
pub const RATE: u32 = 16_000;

/// A voice message's transcript, stored on the message under
/// `flags.transcript`: `text`, or `error` when it failed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Detected language (`zh`, `en`, ...), when the engine reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// The engine and model, e.g. `sensevoice-small-int8-2024-07-17`.
    #[serde(default)]
    pub engine: String,
    /// Seconds of audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_secs: Option<f64>,
    /// Wall time of the whole action (decode, model load, inference), ms.
    #[serde(default)]
    pub ms: u64,
    /// Peak resident memory of the engine process, MB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_mb: Option<u64>,
}

impl Transcript {
    fn failed(engine: &str, e: &anyhow::Error, started: Instant) -> Transcript {
        Transcript {
            error: Some(one_line(&format!("{e:#}"))),
            engine: engine.into(),
            ms: started.elapsed().as_millis() as u64,
            ..Default::default()
        }
    }
}

fn one_line(s: &str) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match s.char_indices().nth(300) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

/// Whether stored message `kind` with `flags` is a voice message still
/// without a transcript.
pub fn needs_transcript(kind: &str, flags: &Value) -> bool {
    kind == "voice" && flags.get("transcript").is_none_or(Value::is_null)
}

/// The prompt text of a voice message with `flags` and `caption`: the
/// marker and the transcript (or the failure), then the caption. `None` when
/// it has no transcript (transcription off or not run).
pub fn prompt_text(flags: &Value, caption: &str) -> Option<String> {
    let t: Transcript = serde_json::from_value(flags.get("transcript")?.clone()).ok()?;
    let mut s = match (&t.text, &t.error) {
        (Some(text), _) if !text.trim().is_empty() => format!("{MARK} {}", text.trim()),
        (Some(_), _) => format!("{FAILED}: no speech recognized]"),
        (None, Some(e)) => format!("{FAILED}: {e}]"),
        (None, None) => return None,
    };
    if !caption.trim().is_empty() {
        s.push_str(&format!(" (caption: {})", caption.trim()));
    }
    Some(s)
}

/// The server's reply to a voice message while it waits for the transcript
/// (`telegram.rs`); edited into [`ack_text`] once it is stored.
pub const ACK: &str = "Transcribing…";

/// The text the voice ack is edited to, from the voice message's `flags`:
/// the transcript, or a short failure line. `None` while it has none.
pub fn ack_text(flags: &Value) -> Option<String> {
    let t: Transcript = serde_json::from_value(flags.get("transcript")?.clone()).ok()?;
    Some(match (&t.text, &t.error) {
        (Some(text), _) if !text.trim().is_empty() => {
            // Telegram's text limit is 4096 UTF-16 units; audio is at most
            // 300 s, so this only guards a runaway engine.
            let text = text.trim();
            match text.char_indices().nth(3500) {
                Some((i, _)) => format!("Transcript: {}…", &text[..i]),
                None => format!("Transcript: {text}"),
            }
        }
        (Some(_), _) => "Transcription failed: no speech recognized".into(),
        (None, Some(e)) => format!("Transcription failed: {}", one_line(e)),
        (None, None) => return None,
    })
}

/// The ack's text when the bot's run started without a transcript
/// (transcription off on the node, or storing it failed).
pub const NOT_TRANSCRIBED: &str = "Not transcribed.";

/// A speech-to-text engine: 16 kHz mono samples in, text and language out.
pub trait Engine {
    /// The engine and model, recorded on each transcript.
    fn name(&self) -> String;
    fn transcribe(&self, pcm: &[f32]) -> Result<Heard>;
}

/// What an engine heard.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Heard {
    pub text: String,
    pub lang: Option<String>,
    /// Peak resident memory of the engine, MB, when it ran in a child.
    pub peak_rss_mb: Option<u64>,
}

/// The action: decodes `audio` (Ogg/Opus) to 16 kHz mono, bounds it and
/// runs `engine`. Every failure is a transcript with `error`.
pub fn transcribe(engine: &dyn Engine, audio: &[u8]) -> Transcript {
    transcribe_within(engine, audio, MAX_AUDIO_SECS)
}

/// [`transcribe`] with audio longer than `max_secs` refused.
pub fn transcribe_within(engine: &dyn Engine, audio: &[u8], max_secs: f64) -> Transcript {
    let started = Instant::now();
    let name = engine.name();
    let pcm = match decode_ogg_opus(audio, max_secs) {
        Ok(p) => p,
        Err(e) => return Transcript::failed(&name, &e, started),
    };
    let secs = pcm.len() as f64 / f64::from(RATE);
    if secs > max_secs {
        let e = anyhow::anyhow!("too long ({secs:.0}s or more; the limit is {max_secs:.0}s)");
        return Transcript {
            audio_secs: Some(secs),
            ..Transcript::failed(&name, &e, started)
        };
    }
    match engine.transcribe(&pcm) {
        Ok(h) => Transcript {
            text: Some(h.text.trim().to_string()),
            lang: h.lang,
            engine: name,
            audio_secs: Some(secs),
            ms: started.elapsed().as_millis() as u64,
            peak_rss_mb: h.peak_rss_mb,
            ..Default::default()
        },
        Err(e) => Transcript {
            audio_secs: Some(secs),
            ..Transcript::failed(&name, &e, started)
        },
    }
}

/// Decodes an Ogg/Opus file (a Telegram voice note) to 16 kHz mono f32,
/// stopping a little past `max_secs`.
pub fn decode_ogg_opus(bytes: &[u8], max_secs: f64) -> Result<Vec<f32>> {
    use opus_pure::{OggOpusReader, Trim, MAX_PACKET_SAMPLES};
    if bytes.len() > MAX_AUDIO_BYTES {
        bail!(
            "the voice file is {} bytes; the limit is {MAX_AUDIO_BYTES}",
            bytes.len()
        );
    }
    let mut reader = OggOpusReader::new(std::io::Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("not an Ogg/Opus voice file: {e}"))?;
    let head = reader.head().clone();
    let channels = usize::from(head.channel_count.max(1));
    let rate = RATE as i32;
    let mut decoder = head
        .decoder(rate)
        .map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
    let mut trim = Trim::new(&head, rate, channels).map_err(|e| anyhow::anyhow!("opus: {e}"))?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut out = Vec::new();
    let limit = ((max_secs + 1.0) * f64::from(RATE)) as usize;
    for packet in reader.packets() {
        let packet = packet.map_err(|e| anyhow::anyhow!("ogg: {e}"))?;
        let n = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(|e| anyhow::anyhow!("opus: {e}"))?;
        let kept = trim.keep(&packet, &block[..n * channels]);
        out.extend(
            kept.chunks_exact(channels)
                .map(|f| (f.iter().sum::<f32>() / channels as f32).clamp(-1.0, 1.0)),
        );
        // Past the limit the length is all that matters.
        if out.len() > limit {
            break;
        }
    }
    if out.is_empty() {
        bail!("the voice file has no audio");
    }
    Ok(out)
}

/// Writes 16-bit PCM mono WAV.
pub fn write_wav(path: &Path, pcm: &[f32], rate: u32) -> Result<()> {
    let data = (pcm.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + pcm.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, b).with_context(|| format!("write {}", path.display()))
}

/// A pinned download: where, its sha256, and the archive members kept
/// (relative to the archive's top folder) under the install folder.
#[derive(Debug, Clone, Copy)]
pub struct Pin {
    pub url: &'static str,
    pub sha256: &'static str,
    pub keep: &'static [&'static str],
    /// The install folder's name under the cache folder.
    pub dir: &'static str,
}

/// The sherpa-onnx v2.0.3 CPU build for each supported platform
/// (`(os, arch)`): the offline recognizer and the ONNX runtime it loads
/// from `../lib`.
pub const ENGINES: [((&str, &str), Pin); 3] = [
    (
        ("linux", "x86_64"),
        Pin {
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.3/sherpa-onnx-v2.0.3-linux-x64-shared-no-tts.tar.bz2",
            sha256: "c16987540bc90cd10bab271023b2dcfb0864892b5af74cb1c9a9c7f1c4efe3af",
            keep: &["bin/sherpa-onnx-offline", "lib/libonnxruntime.so"],
            dir: "sherpa-onnx-v2.0.3",
        },
    ),
    (
        ("linux", "aarch64"),
        Pin {
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.3/sherpa-onnx-v2.0.3-linux-aarch64-shared-cpu.tar.bz2",
            sha256: "c42748225180235bdd4ba10f8c4575b54beb8d5b26a5303bb3051026a0fe4e53",
            keep: &["bin/sherpa-onnx-offline", "lib/libonnxruntime.so"],
            dir: "sherpa-onnx-v2.0.3",
        },
    ),
    (
        ("macos", "aarch64"),
        Pin {
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.3/sherpa-onnx-v2.0.3-osx-arm64-shared-no-tts.tar.bz2",
            sha256: "611aee84500fbc8a1295c05c15afeefa74380602ddc1a212e8a703cfe76981fd",
            keep: &["bin/sherpa-onnx-offline", "lib/libonnxruntime.dylib"],
            dir: "sherpa-onnx-v2.0.3",
        },
    ),
];

/// SenseVoice-Small int8 (2024-07-17), the model task 2000 picked.
pub const MODEL: Pin = Pin {
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17.tar.bz2",
    sha256: "7d1efa2138a65b0b488df37f8b89e3d91a60676e416f515b952358d83dfd347e",
    keep: &["model.int8.onnx", "tokens.txt"],
    dir: "sense-voice-int8-2024-07-17",
};

/// The largest archive accepted (the model is 163 MB).
const MAX_ARCHIVE_BYTES: u64 = 400 << 20;

/// The engine pin for this platform, if there is one.
pub fn engine_pin() -> Option<Pin> {
    ENGINES
        .iter()
        .find(|((os, arch), _)| *os == std::env::consts::OS && *arch == std::env::consts::ARCH)
        .map(|(_, p)| *p)
}

/// The default cache folder: `~/.cache/axe/stt`.
pub fn default_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("axe").join("stt"))
}

/// Unpacks the members `pin.keep` of tar.bz2 `archive` into `dest`,
/// dropping the archive's top folder. Fails when a member is missing.
pub fn unpack(archive: &Path, pin: &Pin, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut tar = tar::Archive::new(bzip2::read::BzDecoder::new(std::io::BufReader::new(f)));
    let mut found = vec![false; pin.keep.len()];
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        // `<top>/<member>`: compare the member part.
        let rel: PathBuf = path.components().skip(1).collect();
        let Some(i) = pin.keep.iter().position(|k| Path::new(k) == rel) else {
            continue;
        };
        let to = dest.join(&rel);
        std::fs::create_dir_all(to.parent().expect("member has a parent"))?;
        let mut out =
            std::fs::File::create(&to).with_context(|| format!("create {}", to.display()))?;
        std::io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if rel.starts_with("bin") { 0o755 } else { 0o644 };
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))?;
        }
        found[i] = true;
    }
    if let Some(i) = found.iter().position(|f| !f) {
        bail!("{} has no {}", pin.url, pin.keep[i]);
    }
    Ok(())
}

/// Streams `url` into `to`, failing unless its sha256 is `sha256`.
pub fn download(url: &str, sha256: &str, to: &Path) -> Result<()> {
    let resp = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(60))
        .build()
        .get(url)
        .call()
        .with_context(|| format!("download {url}"))?;
    let mut reader = resp.into_reader().take(MAX_ARCHIVE_BYTES + 1);
    let mut out = std::fs::File::create(to).with_context(|| format!("create {}", to.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("download {url}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_ARCHIVE_BYTES {
            bail!("{url} is larger than {MAX_ARCHIVE_BYTES} bytes");
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    out.sync_all()?;
    verify(url, &hex(&hash.finalize()), sha256)
}

fn verify(what: &str, got: &str, want: &str) -> Result<()> {
    if got != want {
        bail!("{what}: sha256 {got} does not match the pinned {want}");
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Installs `pin` under `cache` once: download, verify, unpack into a
/// temporary folder, then rename it into place, so a present folder is a
/// complete, verified install. Callers hold the cache lock.
pub fn install(
    cache: &Path,
    pin: &Pin,
    fetch: &dyn Fn(&str, &str, &Path) -> Result<()>,
) -> Result<PathBuf> {
    let dir = cache.join(pin.dir);
    if dir.is_dir() {
        return Ok(dir);
    }
    std::fs::create_dir_all(cache).with_context(|| format!("create {}", cache.display()))?;
    let tmp = cache.join(format!(".{}.partial", pin.dir));
    let archive = cache.join(format!(".{}.archive", pin.dir));
    let _ = std::fs::remove_dir_all(&tmp);
    let r = (|| {
        fetch(pin.url, pin.sha256, &archive)?;
        std::fs::create_dir_all(&tmp)?;
        unpack(&archive, pin, &tmp)?;
        std::fs::rename(&tmp, &dir).with_context(|| format!("install {}", dir.display()))
    })();
    let _ = std::fs::remove_file(&archive);
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    r.map(|()| dir)
}

/// SenseVoice-Small via `sherpa-onnx-offline`, installed into `cache` on
/// first use.
pub struct SenseVoice {
    pub cache: PathBuf,
    pub threads: usize,
    pub timeout: Duration,
}

impl SenseVoice {
    pub fn new(cache: PathBuf) -> SenseVoice {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
        SenseVoice {
            cache,
            threads,
            timeout: ENGINE_TIMEOUT,
        }
    }

    /// The engine binary and the model folder, installing them first if
    /// needed (one install at a time, across processes).
    pub fn ensure(&self) -> Result<(PathBuf, PathBuf)> {
        let pin = engine_pin().with_context(|| {
            format!(
                "no speech engine for {}/{}",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        std::fs::create_dir_all(&self.cache)
            .with_context(|| format!("create {}", self.cache.display()))?;
        let lock = std::fs::File::create(self.cache.join(".lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        let engine = install(&self.cache, &pin, &download)?;
        let model = install(&self.cache, &MODEL, &download)?;
        Ok((engine.join("bin").join("sherpa-onnx-offline"), model))
    }
}

impl Engine for SenseVoice {
    fn name(&self) -> String {
        "sensevoice-small-int8-2024-07-17".into()
    }

    fn transcribe(&self, pcm: &[f32]) -> Result<Heard> {
        let (bin, model) = self.ensure()?;
        let wav = self
            .cache
            .join(format!(".voice-{}.wav", uuid::Uuid::new_v4()));
        write_wav(&wav, pcm, RATE)?;
        let r = run_engine(&bin, &model, &wav, self.threads, self.timeout);
        let _ = std::fs::remove_file(&wav);
        r
    }
}

/// Runs `sherpa-onnx-offline` on `wav` and reads its JSON result line.
fn run_engine(
    bin: &Path,
    model: &Path,
    wav: &Path,
    threads: usize,
    timeout: Duration,
) -> Result<Heard> {
    let lib = bin
        .parent()
        .and_then(Path::parent)
        .map(|p| p.join("lib"))
        .unwrap_or_default();
    let mut child = Command::new(bin)
        .arg(format!(
            "--sense-voice-model={}",
            model.join("model.int8.onnx").display()
        ))
        .arg(format!("--tokens={}", model.join("tokens.txt").display()))
        .arg("--sense-voice-use-itn=1")
        .arg(format!("--num-threads={threads}"))
        .arg(wav)
        .env("LD_LIBRARY_PATH", &lib)
        .env("DYLD_LIBRARY_PATH", &lib)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;
    // Drain both pipes so a chatty engine cannot block on a full pipe.
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let (code, peak) = loop {
        if let Some(done) = reap(&child, false)? {
            break done;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = reap(&child, true);
            bail!("the speech engine took longer than {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if code != Some(0) {
        let tail: Vec<&str> = err
            .lines()
            .rev()
            .filter(|l| !l.trim().is_empty())
            .take(3)
            .collect();
        bail!(
            "the speech engine exited with {}: {}",
            code.map_or("a signal".to_string(), |c| format!("code {c}")),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    let (text, lang) = parse_result(&out)?;
    Ok(Heard {
        text,
        lang,
        peak_rss_mb: peak,
    })
}

/// Waits for (or, unless `block`, polls) `child` with `wait4`, which also
/// reports the child's own peak memory: (exit code, peak MB). `None` while
/// it runs.
fn reap(child: &std::process::Child, block: bool) -> Result<Option<(Option<i32>, Option<u64>)>> {
    let pid = child.id() as libc::pid_t;
    let mut status = 0;
    // SAFETY: rusage is plain data, written by the call.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let flags = if block { 0 } else { libc::WNOHANG };
    // SAFETY: pid is our own unreaped child; the pointers are valid.
    let r = unsafe { libc::wait4(pid, &mut status, flags, &mut usage) };
    if r == 0 {
        return Ok(None);
    }
    if r < 0 {
        bail!(
            "wait for the speech engine: {}",
            std::io::Error::last_os_error()
        );
    }
    let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
    // ru_maxrss is KiB on Linux, bytes on macOS.
    let maxrss = usage.ru_maxrss as u64;
    let mb = if cfg!(target_os = "macos") {
        maxrss >> 20
    } else {
        maxrss >> 10
    };
    Ok(Some((code, Some(mb))))
}

/// The text and language of `sherpa-onnx-offline`'s JSON result line.
pub fn parse_result(stdout: &str) -> Result<(String, Option<String>)> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .context("the speech engine printed no result")?;
    let v: Value = serde_json::from_str(line.trim()).context("the speech engine's result")?;
    let text = v["text"]
        .as_str()
        .context("the speech engine's result has no text")?
        .to_string();
    let lang = v["lang"]
        .as_str()
        .map(|l| {
            l.trim_start_matches("<|")
                .trim_end_matches("|>")
                .to_string()
        })
        .filter(|l| !l.is_empty() && l != "nospeech");
    Ok((text, lang))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) struct Fixed(pub Result<(String, Option<String>), String>);
    impl Engine for Fixed {
        fn name(&self) -> String {
            "fixed".into()
        }
        fn transcribe(&self, pcm: &[f32]) -> Result<Heard> {
            assert!(!pcm.is_empty());
            self.0
                .clone()
                .map(|(text, lang)| Heard {
                    text,
                    lang,
                    peak_rss_mb: None,
                })
                .map_err(|e| anyhow::anyhow!(e))
        }
    }

    /// A `secs` long, 16 kHz mono Ogg/Opus voice note (a 440 Hz tone), made
    /// with the encoder of the same crate.
    pub(crate) fn tone_ogg(secs: f64) -> Vec<u8> {
        use opus_pure::{Application, OggOpusWriter, OpusEncoder, OpusHead, MAX_PACKET_BYTES};
        let rate = 16_000;
        let frame = 320;
        let mut enc = OpusEncoder::new(rate, 1, Application::Voip).unwrap();
        let head = OpusHead::for_encoder(&enc, rate as u32);
        let mut out = Vec::new();
        let mut w = OggOpusWriter::new(&mut out, head).unwrap();
        let mut packet = vec![0u8; MAX_PACKET_BYTES];
        let n = (secs * f64::from(rate)) as usize / frame;
        for i in 0..n {
            let block: Vec<f32> = (0..frame)
                .map(|j| {
                    let t = (i * frame + j) as f32 / rate as f32;
                    0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                })
                .collect();
            let len = enc.encode(&block, frame, &mut packet).unwrap();
            w.write_packet(&packet[..len]).unwrap();
        }
        w.finish().unwrap();
        out
    }

    #[test]
    fn ogg_opus_decodes_to_16k_mono() {
        let pcm = decode_ogg_opus(&tone_ogg(1.5), MAX_AUDIO_SECS).unwrap();
        let secs = pcm.len() as f64 / 16_000.0;
        assert!((secs - 1.5).abs() < 0.05, "{secs}");
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1, "decoded silence: {peak}");
    }

    #[test]
    fn the_action_returns_the_engines_text_or_a_failure() {
        let ok = transcribe(
            &Fixed(Ok((" hello ".into(), Some("en".into())))),
            &tone_ogg(1.0),
        );
        assert_eq!(ok.text.as_deref(), Some("hello"));
        assert_eq!(ok.lang.as_deref(), Some("en"));
        assert_eq!(ok.engine, "fixed");
        assert!(ok.error.is_none());
        assert!((ok.audio_secs.unwrap() - 1.0).abs() < 0.05);

        let failed = transcribe(&Fixed(Err("model gone".into())), &tone_ogg(1.0));
        assert_eq!(failed.text, None);
        assert_eq!(failed.error.as_deref(), Some("model gone"));

        // Not audio: a failure, not a panic or an error.
        let junk = transcribe(&Fixed(Ok(("x".into(), None))), b"not ogg");
        assert!(junk.error.unwrap().contains("not an Ogg/Opus"));
    }

    #[test]
    fn too_long_voice_is_refused_before_the_engine() {
        struct Never;
        impl Engine for Never {
            fn name(&self) -> String {
                "never".into()
            }
            fn transcribe(&self, _: &[f32]) -> Result<Heard> {
                panic!("must not run")
            }
        }
        let t = transcribe_within(&Never, &tone_ogg(3.0), 1.0);
        let e = t.error.unwrap();
        assert!(
            e.contains("too long") && e.contains("the limit is 1s"),
            "{e}"
        );
    }

    #[test]
    fn prompt_text_marks_transcripts_and_failures() {
        let ok = json!({"transcript": {"text": "明天开会", "engine": "e", "ms": 1}});
        assert_eq!(
            prompt_text(&ok, "").unwrap(),
            "[voice message, auto-transcribed, may contain errors] 明天开会"
        );
        assert_eq!(
            prompt_text(&ok, "fyi").unwrap(),
            "[voice message, auto-transcribed, may contain errors] 明天开会 (caption: fyi)"
        );
        let failed = json!({"transcript": {"error": "too long (301s or more; the limit is 300s)"}});
        assert_eq!(
            prompt_text(&failed, "").unwrap(),
            "[voice message, transcription failed: too long (301s or more; the limit is 300s)]"
        );
        let silent = json!({"transcript": {"text": " "}});
        assert_eq!(
            prompt_text(&silent, "").unwrap(),
            "[voice message, transcription failed: no speech recognized]"
        );
        assert_eq!(prompt_text(&json!({}), "x"), None);
        assert_eq!(ack_text(&ok).unwrap(), "Transcript: 明天开会");
        assert_eq!(
            ack_text(&failed).unwrap(),
            "Transcription failed: too long (301s or more; the limit is 300s)"
        );
        assert_eq!(
            ack_text(&silent).unwrap(),
            "Transcription failed: no speech recognized"
        );
        assert_eq!(ack_text(&json!({})), None);
        assert!(needs_transcript("voice", &json!({})));
        assert!(!needs_transcript("voice", &ok));
        assert!(!needs_transcript("photo", &json!({})));
    }

    #[test]
    fn parses_the_engine_result_line() {
        let out = "{\"lang\": \"<|zh|>\", \"emotion\": \"<|NEUTRAL|>\", \"text\": \"报告。\", \"tokens\":[]}\n";
        assert_eq!(
            parse_result(out).unwrap(),
            ("报告。".to_string(), Some("zh".to_string()))
        );
        assert!(parse_result("no json\n").is_err());
    }

    #[test]
    fn wav_header_is_16bit_mono() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.wav");
        write_wav(&p, &[0.0, 0.5, -0.5, 1.5], 16_000).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(b.len(), 44 + 8);
        assert_eq!(u16::from_le_bytes([b[22], b[23]]), 1);
        assert_eq!(u32::from_le_bytes([b[24], b[25], b[26], b[27]]), 16_000);
        assert_eq!(i16::from_le_bytes([b[50], b[51]]), 32767, "clamped");
    }

    /// A tar.bz2 with `<top>/<name>` members.
    fn archive(dir: &Path, members: &[(&str, &[u8])]) -> PathBuf {
        let p = dir.join("a.tar.bz2");
        let f = std::fs::File::create(&p).unwrap();
        let mut t = tar::Builder::new(bzip2::write::BzEncoder::new(f, bzip2::Compression::fast()));
        for (name, data) in members {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            t.append_data(&mut h, format!("top/{name}"), *data).unwrap();
        }
        t.into_inner().unwrap().finish().unwrap();
        p
    }

    const PIN: Pin = Pin {
        url: "https://example.invalid/a.tar.bz2",
        sha256: "",
        keep: &["bin/tool", "lib/x.so"],
        dir: "tool-1",
    };

    #[test]
    fn install_keeps_only_pinned_members_and_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let src = archive(
            dir.path(),
            &[
                ("bin/tool", b"#!"),
                ("bin/other", b"x"),
                ("lib/x.so", b"so"),
            ],
        );
        let cache = dir.path().join("cache");
        let copy = |_: &str, _: &str, to: &Path| -> Result<()> {
            std::fs::copy(&src, to)?;
            Ok(())
        };
        let got = install(&cache, &PIN, &copy).unwrap();
        assert!(got.join("bin/tool").is_file());
        assert!(got.join("lib/x.so").is_file());
        assert!(!got.join("bin/other").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(got.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // Installed: no second fetch.
        let never = |_: &str, _: &str, _: &Path| -> Result<()> { panic!("fetched twice") };
        assert_eq!(install(&cache, &PIN, &never).unwrap(), got);
        // Only the install folder is left.
        let names: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["tool-1"]);
    }

    #[test]
    fn a_failed_fetch_or_missing_member_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let bad = |u: &str, _: &str, _: &Path| -> Result<()> { verify(u, "00", "ff") };
        let e = install(&cache, &PIN, &bad).unwrap_err();
        assert!(format!("{e:#}").contains("does not match the pinned"));
        assert!(!cache.join("tool-1").exists());

        let src = archive(dir.path(), &[("bin/tool", b"#!")]);
        let partial = |_: &str, _: &str, to: &Path| -> Result<()> {
            std::fs::copy(&src, to)?;
            Ok(())
        };
        let e = install(&cache, &PIN, &partial).unwrap_err();
        assert!(format!("{e:#}").contains("has no lib/x.so"));
        assert!(!cache.join("tool-1").exists());
        assert!(
            std::fs::read_dir(&cache).unwrap().next().is_none(),
            "no leftovers"
        );
    }

    #[test]
    fn every_platform_pin_keeps_the_offline_binary() {
        for (_, p) in ENGINES {
            assert!(p.keep.contains(&"bin/sherpa-onnx-offline"));
            assert_eq!(p.sha256.len(), 64);
        }
        assert_eq!(MODEL.sha256.len(), 64);
    }
}
