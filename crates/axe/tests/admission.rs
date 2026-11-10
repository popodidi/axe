//! Resource admission gate: Linux counter parsing, thresholds, hysteresis, the
//! 60-second CPU window, unknown/stale readings and quiet logging. All pressure
//! is synthetic: a fake probe and a fake monotonic clock.

mod common;

use axe::admission::{self, cpu_busy_percent, parse_meminfo, parse_proc_stat, CpuTimes, Gate};
use axe::config::{Config, Resources};
use common::gate::{defaults, Rig, GIB};

fn levels(g: &Gate) -> Vec<(String, String)> {
    let v = admission::view(&g.handle());
    v["signals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["level"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn level(g: &Gate, name: &str) -> String {
    levels(g).into_iter().find(|(n, _)| n == name).unwrap().1
}

/// A gate past its 60 s CPU warm-up with healthy readings.
fn warm(rig: &Rig, cfg: Resources) -> Gate {
    let mut g = rig.gate(cfg);
    assert!(!g.check(), "startup is unknown, never healthy");
    rig.run(&mut g, 60, 10);
    assert!(g.admitting(), "{:?}", admission::view(&g.handle()));
    g
}

// ---- Linux counter semantics ------------------------------------------------

#[test]
fn meminfo_uses_mem_available_over_mem_total() {
    let text = "MemTotal:       16318416 kB\nMemFree:          512000 kB\nMemAvailable:    4079604 kB\nBuffers: 1 kB\n";
    let m = parse_meminfo(text).unwrap();
    assert_eq!((m.available_kib, m.total_kib), (4079604, 16318416));
    // MemFree is not a substitute: a kernel without MemAvailable is unknown.
    let err = parse_meminfo("MemTotal: 10 kB\nMemFree: 5 kB\n").unwrap_err();
    assert!(err.to_string().contains("MemAvailable"), "{err}");
    assert!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n").is_err());
    assert!(parse_meminfo("MemTotal: 10 MB\nMemAvailable: 1 kB\n").is_err());
}

#[test]
fn proc_stat_busy_excludes_idle_and_iowait_and_counts_guest_once() {
    // user nice system idle iowait irq softirq steal guest guest_nice
    let a = parse_proc_stat("cpu  100 0 100 700 100 0 0 0 50 0\ncpu0 1 2 3 4\n").unwrap();
    let b = parse_proc_stat("cpu  400 0 200 800 200 50 50 100 300 0\ncpu0 1 2 3 4\n").unwrap();
    // deltas: user 300, system 100, idle 100, iowait 100, irq 50, softirq 50, steal 100
    // total 800 (guest already inside user), busy 600 -> 75%.
    assert_eq!(cpu_busy_percent(&a, &b), Some(75.0));
    // No elapsed time is unknown, not 0% or 100%.
    assert_eq!(cpu_busy_percent(&b, &b), None);
    // iowait can move backwards (proc(5)); it never yields a negative delta.
    let c = parse_proc_stat("cpu  500 0 200 900 150 50 50 100 300 0\n").unwrap();
    assert_eq!(cpu_busy_percent(&b, &c), Some(50.0));
    // Per-CPU lines and short lines are rejected rather than misread.
    assert!(parse_proc_stat("cpu0 1 2 3 4\n").is_err());
    assert!(parse_proc_stat("cpu 1 2 3\n").is_err());
}

#[test]
fn linux_probe_reads_this_guest() {
    use axe::admission::{LinuxProbe, Probe};
    let mut p = LinuxProbe;
    let m = p.memory().unwrap();
    assert!(m.available_kib <= m.total_kib && m.total_kib > 0);
    let disk = p.disk_available(std::path::Path::new("/")).unwrap();
    assert!(disk > 0);
    assert!(p
        .disk_available(std::path::Path::new("/no/such/dir"))
        .is_err());
    let t: CpuTimes = p.cpu().unwrap();
    assert!(t.fields[3] > 0);
}

// ---- configuration ----------------------------------------------------------

#[test]
fn resources_section_is_optional_and_defaults_to_approved_thresholds() {
    assert!(Config::parse("").unwrap().resources.is_none());
    let cfg = Config::parse("workspace_root = \"/ws\"\n[resources]\n").unwrap();
    let r = cfg.resources.clone().unwrap();
    assert_eq!(r, defaults());
    assert_eq!(
        (r.sample_seconds, r.cpu_window_seconds, r.stale_seconds),
        (10, 60, 30)
    );
    assert_eq!(
        (r.disk_pause_below_gib, r.disk_resume_above_gib),
        (15.0, 20.0)
    );
    assert_eq!(
        (r.memory_pause_below_percent, r.memory_resume_above_percent),
        (15.0, 20.0)
    );
    assert_eq!(
        (r.cpu_pause_above_percent, r.cpu_resume_below_percent),
        (90.0, 80.0)
    );
    assert_eq!(cfg.resources_path(), std::path::PathBuf::from("/ws"));
    let custom = Config::parse(
        "[resources]\npath = \"/data\"\ndisk_pause_below_gib = 5\ndisk_resume_above_gib = 7.5\ncpu_pause_above_percent = 95\n",
    )
    .unwrap();
    let r = custom.resources.clone().unwrap();
    assert_eq!(
        (r.disk_pause_below_gib, r.disk_resume_above_gib),
        (5.0, 7.5)
    );
    assert_eq!(r.cpu_pause_above_percent, 95.0);
    assert_eq!(custom.resources_path(), std::path::PathBuf::from("/data"));
    // Round-trips through `axe config` output.
    let again = Config::parse(&toml::to_string_pretty(&custom).unwrap()).unwrap();
    assert_eq!(again.resources, custom.resources);
}

#[test]
fn invalid_resource_thresholds_are_rejected() {
    for (body, msg) in [
        (
            "disk_pause_below_gib = 20",
            "disk_resume_above_gib must be greater",
        ),
        (
            "disk_resume_above_gib = 15",
            "disk_resume_above_gib must be greater",
        ),
        (
            "memory_pause_below_percent = 25",
            "memory_resume_above_percent must be greater",
        ),
        ("memory_resume_above_percent = 101", "<= 100"),
        (
            "cpu_resume_below_percent = 90",
            "cpu_resume_below_percent must be less",
        ),
        ("cpu_pause_above_percent = -1", ">= 0"),
        ("sample_seconds = 0", "sample_seconds must be >= 1"),
        (
            "cpu_window_seconds = 5",
            "cpu_window_seconds must be >= sample_seconds",
        ),
        (
            "stale_seconds = 5",
            "stale_seconds must be >= sample_seconds",
        ),
        ("disk_pause_below_gib = nan", "finite"),
        ("unknown_key = 1", "unknown field"),
    ] {
        let err = Config::parse(&format!("[resources]\n{body}\n")).unwrap_err();
        assert!(format!("{err:#}").contains(msg), "{body}: {err:#}");
    }
}

// ---- thresholds and hysteresis ----------------------------------------------

#[test]
fn disk_pauses_strictly_below_15_gib_and_recovers_strictly_above_20() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    for (bytes, admitting) in [
        (15 * GIB, true), // equal to pause: not below
        (15 * GIB - 1, false),
        (19 * GIB, false), // hysteresis band
        (20 * GIB, false), // equal to resume: not above
        (20 * GIB + 1, true),
        (16 * GIB, true), // band while clear stays clear
        (15 * GIB, true),
        (14 * GIB, false),
    ] {
        rig.set_disk(Ok(bytes));
        rig.advance(10);
        assert_eq!(g.check(), admitting, "{bytes} bytes");
        assert_eq!(level(&g, "memory"), "clear");
        assert_eq!(level(&g, "cpu"), "clear");
    }
    let v = admission::view(&g.handle());
    assert_eq!(v["signals"][0]["value"], (14 * GIB) as f64);
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains(
            "disk /fake/workspace 14.0 GiB free (pause < 15 GiB free, resume > 20 GiB free)"
        ),
        "{reason}"
    );
}

#[test]
fn memory_pauses_strictly_below_15_percent_and_recovers_strictly_above_20() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    for (pct, admitting) in [
        (15.0, true),
        (14.9, false),
        (18.0, false),
        (20.0, false),
        (20.1, true),
        (17.0, true),
        (10.0, false),
    ] {
        rig.set_memory(Ok(pct));
        rig.advance(10);
        assert_eq!(g.check(), admitting, "{pct}%");
        assert_eq!(level(&g, "disk"), "clear");
    }
    assert!(admission::view(&g.handle())["reason"]
        .as_str()
        .unwrap()
        .starts_with("memory 10.0% available"));
}

#[test]
fn cpu_uses_a_60_second_average_with_hysteresis() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    // A 20 s burst at 100% inside an otherwise idle minute averages below 90%.
    rig.set_busy(100.0);
    rig.run(&mut g, 20, 10);
    assert!(g.admitting());
    // Sustained 95%: pauses only once the 60 s average exceeds 90%.
    rig.set_busy(95.0);
    let mut paused_after = None;
    for i in 1..=8 {
        rig.advance(10);
        if !g.check() && paused_after.is_none() {
            paused_after = Some(i * 10);
        }
    }
    // 60 s window: 20 s at 100% + 10*k s at 95% + rest at 10% exceeds 90% only
    // when the idle time has left the window.
    assert_eq!(paused_after, Some(40), "{:?}", admission::view(&g.handle()));
    assert_eq!(level(&g, "cpu"), "constrained");
    // 85% for a full window: inside the band, stays paused.
    rig.set_busy(85.0);
    rig.run(&mut g, 120, 10);
    assert!(!g.admitting());
    // Exactly 80% over the window is not below the resume threshold.
    rig.set_busy(80.0);
    rig.run(&mut g, 60, 10);
    assert!(!g.admitting());
    // The 60 s average (50 s at 80%, 10 s at 70%) is 78.3%: below 80% recovers.
    rig.set_busy(70.0);
    rig.run(&mut g, 10, 10);
    assert!(g.admitting());
    rig.run(&mut g, 60, 10);
    let v = admission::view(&g.handle());
    assert_eq!(v["signals"][2]["value"], 70.0);
    assert_eq!(v["signals"][2]["reading"], "70.0% busy over 60 s");
    // Exactly 90% does not pause.
    rig.set_busy(90.0);
    rig.run(&mut g, 120, 10);
    assert!(g.admitting());
}

#[test]
fn mixed_pressure_resumes_only_after_every_signal_recovers() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    rig.set_disk(Ok(10 * GIB));
    rig.set_memory(Ok(5.0));
    rig.set_busy(99.0);
    rig.run(&mut g, 60, 10);
    assert!(!g.admitting());
    let reason = admission::view(&g.handle())["reason"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(reason.contains("disk") && reason.contains("memory") && reason.contains("cpu"));
    rig.set_disk(Ok(30 * GIB));
    rig.run(&mut g, 10, 10);
    assert!(!g.admitting());
    rig.set_memory(Ok(50.0));
    rig.run(&mut g, 10, 10);
    assert!(!g.admitting(), "cpu still constrained");
    assert_eq!(
        levels(&g),
        [
            ("disk".into(), "clear".into()),
            ("memory".into(), "clear".into()),
            ("cpu".into(), "constrained".into())
        ]
    );
    rig.set_busy(10.0);
    rig.run(&mut g, 60, 10);
    assert!(g.admitting());
    assert!(admission::view(&g.handle())["reason"].is_null());
}

#[test]
fn startup_is_unknown_until_a_full_cpu_window_and_band_values_must_recover() {
    let rig = Rig::default();
    rig.set_memory(Ok(17.0)); // inside the band at startup
    let mut g = rig.gate(defaults());
    assert!(!g.check());
    assert_eq!(level(&g, "cpu"), "unknown");
    assert_eq!(level(&g, "memory"), "constrained");
    assert!(admission::view(&g.handle())["reason"]
        .as_str()
        .unwrap()
        .contains("warming up (0 of 60 s window)"));
    rig.run(&mut g, 50, 10);
    assert_eq!(level(&g, "cpu"), "unknown");
    rig.run(&mut g, 10, 10);
    assert_eq!(level(&g, "cpu"), "clear");
    // Unknown never claims healthy: memory must cross its resume threshold.
    assert!(!g.admitting());
    rig.set_memory(Ok(21.0));
    rig.run(&mut g, 10, 10);
    assert!(g.admitting());
}

#[test]
fn unavailable_readings_pause_and_must_recover_past_resume() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    rig.set_disk(Err("statvfs: permission denied"));
    rig.advance(10);
    assert!(!g.check());
    assert_eq!(level(&g, "disk"), "unknown");
    let v = admission::view(&g.handle());
    assert!(v["signals"][0]["value"].is_null());
    assert!(v["reason"]
        .as_str()
        .unwrap()
        .contains("unknown: unavailable: statvfs: permission denied"));
    // Back but inside the band: still paused.
    rig.set_disk(Ok(18 * GIB));
    rig.advance(10);
    assert!(!g.check());
    rig.set_disk(Ok(21 * GIB));
    rig.advance(10);
    assert!(g.check());

    rig.set_memory(Err("no MemAvailable"));
    rig.advance(10);
    assert!(!g.check());
    rig.set_memory(Ok(60.0));
    rig.advance(10);
    assert!(g.check());

    // A CPU read failure is unknown; the next good sample uses retained samples.
    rig.set_cpu_error(Some("/proc/stat unreadable"));
    rig.advance(10);
    assert!(!g.check());
    assert_eq!(level(&g, "cpu"), "unknown");
    rig.set_cpu_error(None);
    rig.advance(10);
    assert!(g.check(), "earlier in-window samples still span 60 s");
}

#[test]
fn stalled_sampling_makes_the_cpu_window_stale_until_refilled() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    // No samples for 2 minutes (> window + stale_seconds).
    rig.advance(120);
    assert!(!g.check());
    let v = admission::view(&g.handle());
    assert!(
        v["reason"]
            .as_str()
            .unwrap()
            .contains("stale window (120 s"),
        "{v}"
    );
    rig.run(&mut g, 50, 10);
    assert!(!g.admitting());
    assert!(admission::view(&g.handle())["reason"]
        .as_str()
        .unwrap()
        .contains("no sample spanning the 60 s window"));
    rig.run(&mut g, 10, 10);
    assert!(g.admitting());
}

#[test]
fn configured_thresholds_and_windows_are_used() {
    let rig = Rig::default();
    let cfg = Config::parse(
        "[resources]\nsample_seconds = 5\ncpu_window_seconds = 20\nstale_seconds = 5\ndisk_pause_below_gib = 50\ndisk_resume_above_gib = 60\nmemory_pause_below_percent = 40\nmemory_resume_above_percent = 45\ncpu_pause_above_percent = 50\ncpu_resume_below_percent = 30\n",
    )
    .unwrap()
    .resources
    .unwrap();
    let mut g = rig.gate(cfg);
    g.check();
    rig.run(&mut g, 20, 5);
    assert!(g.admitting());
    rig.set_disk(Ok(49 * GIB));
    rig.run(&mut g, 5, 5);
    assert_eq!(level(&g, "disk"), "constrained");
    rig.set_disk(Ok(61 * GIB));
    rig.set_memory(Ok(39.0));
    rig.run(&mut g, 5, 5);
    assert_eq!(level(&g, "memory"), "constrained");
    rig.set_memory(Ok(46.0));
    rig.set_busy(60.0);
    rig.run(&mut g, 20, 5);
    assert_eq!(level(&g, "cpu"), "constrained");
    rig.set_busy(20.0);
    rig.run(&mut g, 20, 5);
    assert!(g.admitting());
    let v = admission::view(&g.handle());
    assert_eq!(v["signals"][2]["pause"], "> 50% busy over 20 s");
    assert_eq!(v["thresholds"]["disk_resume_above_gib"], 60.0);
}

#[test]
fn ticks_sample_only_when_due() {
    let rig = Rig::default();
    let mut g = rig.gate(defaults());
    assert!(!g.tick(), "first tick samples but does not open");
    assert_eq!(g.samples(), 1);
    for _ in 0..9 {
        rig.advance(1);
        g.tick();
    }
    assert_eq!(g.samples(), 1);
    assert_eq!(g.until_due(), std::time::Duration::from_secs(1));
    rig.advance(1);
    g.tick();
    assert_eq!(g.samples(), 2);
}

#[test]
fn repeated_samples_log_transitions_only() {
    let rig = Rig::default();
    let mut g = warm(&rig, defaults());
    let after_warmup = rig.lines();
    assert_eq!(
        after_warmup,
        [
            "axe admission: disk unknown -> clear: disk /fake/workspace 100.0 GiB free (pause < 15 GiB free)",
            "axe admission: memory unknown -> clear: memory 50.0% available (pause < 15% available)",
            "axe admission: paused new sessions: cpu unknown: warming up (0 of 60 s window) (resume < 80% busy over 60 s)",
            // CPU warming up never holds bot runs.
            "axe admission: admitting bot runs",
            "axe admission: cpu unknown -> clear: cpu 10.0% busy over 60 s (pause > 90% busy over 60 s)",
            "axe admission: admitting new sessions",
        ]
    );
    // An hour of sustained pressure, sampled every 10 s and before 3 spawn
    // attempts per sample, logs one transition, one pause line and, below the
    // bot floor, one bot hold line.
    rig.set_disk(Ok(GIB));
    for _ in 0..360 {
        rig.advance(10);
        g.tick();
        for _ in 0..3 {
            g.check();
        }
    }
    assert_eq!(
        rig.lines().len(),
        after_warmup.len() + 3,
        "{:?}",
        rig.lines()
    );
    assert!(rig.lines().last().unwrap().starts_with(
        "axe admission: bot runs held at the floor: disk 1.0 GiB free (floor < 7.5 GiB free)"
    ));
    rig.set_disk(Ok(100 * GIB));
    rig.run(&mut g, 10, 10);
    let lines = rig.lines();
    assert_eq!(lines.len(), after_warmup.len() + 6);
    assert_eq!(
        lines[lines.len() - 2..],
        [
            "axe admission: admitting new sessions",
            "axe admission: admitting bot runs"
        ]
    );
}

#[test]
fn published_status_goes_stale_when_the_dispatcher_stops_sampling() {
    let rig = Rig::default();
    let cfg = Config::parse(
        "[resources]\nsample_seconds = 1\ncpu_window_seconds = 1\nstale_seconds = 1\n",
    )
    .unwrap()
    .resources
    .unwrap();
    let mut g = rig.gate(cfg);
    g.check();
    rig.run(&mut g, 1, 1);
    assert!(g.admitting());
    assert_eq!(admission::view(&g.handle())["admitting"], true);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let v = admission::view(&g.handle());
    assert_eq!(v["admitting"], false);
    assert!(
        v["reason"].as_str().unwrap().starts_with("status stale"),
        "{v}"
    );
    assert_eq!(v["bots_admitting"], false);
    assert_eq!(v["bots_reason"], v["reason"]);
}

#[test]
fn bot_floor_is_half_the_disk_and_memory_pause_and_ignores_cpu() {
    let rig = Rig::default();
    let mut g = rig.gate(defaults());
    // Startup: CPU is warming up, so tasks wait, but bots may start.
    assert!(!g.check());
    assert!(g.check_bots());
    let v = admission::view(&g.handle());
    assert_eq!(
        (v["bots_admitting"].clone(), v["bots_reason"].clone()),
        (true.into(), serde_json::Value::Null)
    );
    // Exactly at the floor does not cross it; strictly below does.
    rig.set_disk(Ok(15 * GIB / 2));
    assert!(g.check_bots());
    rig.set_disk(Ok(15 * GIB / 2 - 1));
    assert!(!g.check_bots());
    rig.set_disk(Ok(100 * GIB));
    rig.set_memory(Ok(7.5));
    assert!(g.check_bots());
    rig.set_memory(Ok(7.4));
    assert!(!g.check_bots());
    assert_eq!(
        admission::view(&g.handle())["bots_reason"],
        "memory 7.4% available (floor < 7.5% available)"
    );
    // No hysteresis: back above the floor admits bots at once.
    rig.set_memory(Ok(7.6));
    assert!(g.check_bots());
    // Unreadable disk or memory holds bots; an unreadable CPU does not.
    rig.set_cpu_error(Some("no /proc/stat"));
    assert!(g.check_bots());
    rig.set_cpu_error(None);
    rig.set_disk(Err("statvfs failed"));
    assert!(!g.check_bots());
    assert!(admission::view(&g.handle())["bots_reason"]
        .as_str()
        .unwrap()
        .starts_with("disk unknown: unavailable: statvfs failed"));
    rig.set_disk(Ok(100 * GIB));
    rig.set_memory(Err("no meminfo"));
    assert!(!g.check_bots());
}

#[test]
fn disabled_admission_reports_not_gated() {
    let v = admission::view(&admission::handle(None));
    assert_eq!(
        (v["enabled"].clone(), v["admitting"].clone()),
        (false.into(), true.into())
    );
    let v = admission::view(&admission::handle(Some(&defaults())));
    assert_eq!(v["admitting"], false);
    assert_eq!(v["reason"], "not sampled yet");
}

// ---- server copy of the dispatcher's report ----------------------------------

#[test]
fn server_report_is_unknown_when_missing_or_expired_and_ages_into_stale() {
    use std::time::{Duration, Instant};
    let missing = admission::reported(None);
    assert_eq!(missing["admitting"], serde_json::Value::Null);
    assert_eq!(missing["enabled"], serde_json::Value::Null);

    let enabled = serde_json::json!({
        "enabled": true, "admitting": true, "reason": null,
        "thresholds": {"stale_seconds": 30}, "age_seconds": 5, "signals": [],
    });
    let at = |secs| {
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .unwrap()
    };
    let fresh = admission::reported(Some(&(enabled.clone(), at(2))));
    assert_eq!(
        (fresh["admitting"].clone(), fresh["age_seconds"].clone()),
        (true.into(), 7.into())
    );
    assert_eq!(fresh["report_age_seconds"], 2);
    // Readings older than stale_seconds pause, as on the dispatcher.
    let stale = admission::reported(Some(&(enabled.clone(), at(26))));
    assert_eq!(stale["admitting"], false);
    assert!(
        stale["reason"]
            .as_str()
            .unwrap()
            .starts_with("status stale: "),
        "{stale}"
    );
    // No report within the dispatcher lease: unknown, even for an ungated dispatcher.
    let ungated = serde_json::json!({"enabled": false, "admitting": true, "age_seconds": null});
    assert_eq!(
        admission::reported(Some(&(ungated.clone(), at(30))))["admitting"],
        true
    );
    let gone = admission::reported(Some(&(ungated, at(61))));
    assert_eq!(gone["admitting"], serde_json::Value::Null);
    assert!(
        gone["reason"]
            .as_str()
            .unwrap()
            .starts_with("unknown: dispatcher last reported 61s ago"),
        "{gone}"
    );
}

#[test]
fn hold_file_states_and_fail_closed_parsing() {
    use axe::admission::{apply_hold, read_hold, remove_hold, write_hold, Hold, HoldState};
    use time::macros::datetime;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/hold.json");
    let now = datetime!(2026-11-10 12:00 UTC);
    assert_eq!(read_hold(&path, now), HoldState::None);
    let hold = Hold {
        until: "2026-11-10T23:00:00Z".into(),
        reason: Some("cutover".into()),
        set_at: None,
        set_by: Some("agent".into()),
    };
    write_hold(&path, &hold).unwrap();
    assert_eq!(read_hold(&path, now), HoldState::Active(hold.clone()));
    // The until instant itself is no longer held.
    let end = datetime!(2026-11-10 23:00 UTC);
    assert_eq!(read_hold(&path, end), HoldState::Expired(hold.clone()));
    assert_eq!(HoldState::Expired(hold.clone()).reason(), None);

    // An admitting view gains the hold; bot fields are untouched.
    let mut v = serde_json::json!({"admitting": true, "reason": null, "bots_admitting": true});
    apply_hold(&mut v, &read_hold(&path, now));
    assert_eq!(v["admitting"], false);
    assert_eq!(v["bots_admitting"], true);
    assert_eq!(
        v["reason"],
        "admission hold until 2026-11-10T23:00:00Z (cutover)"
    );
    assert_eq!(v["hold"]["set_by"], "agent");
    // A resource pause keeps its reason after the hold's.
    let mut v = serde_json::json!({"admitting": false, "reason": "disk low"});
    apply_hold(&mut v, &read_hold(&path, now));
    assert_eq!(
        v["reason"],
        "admission hold until 2026-11-10T23:00:00Z (cutover); disk low"
    );
    // No hold: the view is unchanged apart from `hold: null`.
    let mut v = serde_json::json!({"admitting": true, "reason": null});
    apply_hold(&mut v, &HoldState::None);
    assert_eq!(
        v,
        serde_json::json!({"admitting": true, "reason": null, "hold": null})
    );

    // Unreadable files and bad times hold admission until fixed or released.
    for text in ["not json", r#"{"until": "tomorrow"}"#, "{}"] {
        std::fs::write(&path, text).unwrap();
        let state = read_hold(&path, now);
        assert!(matches!(state, HoldState::Invalid(_)), "{text}: {state:?}");
        assert!(state
            .reason()
            .unwrap()
            .starts_with("admission hold file unreadable: "));
        assert!(state.json()["error"].is_string());
    }
    assert!(remove_hold(&path).unwrap());
    assert!(!remove_hold(&path).unwrap());
    assert!(write_hold(
        &path,
        &Hold {
            until: "soon".into(),
            ..hold
        }
    )
    .is_err());
}
