// SPDX-License-Identifier: MIT
//! `xtask test audio-app`: the A1a gate. The test APK (xtask/testdata/audio) is started by atld as the app gate's is; its process asks the audio services for the session ids, the minimum buffer and a track's player id, which AudioSystem, AudioTrack and PlayerBase need.
//! Expected values are AOSP's formulas worked here over the served output, not the code under test: AudioSystem::calculateMinFrameCount (AudioSystem.cpp:486) with sourceFramesNeededWithTimestretch (AudioResamplerPublic.h:84-110) at speed 1.0.
//! The served output is atl-audioflinger's OUTPUT_* constants; [`SPEC_RATE_HZ`] and the others copy them, so a change there must change these too (a mismatch is a red gate).
//! Checks: session ids nonzero and distinct; getMinBufferSize equals the expected bytes (stereo, 16-bit); the track's player id is deferred to A1b (trackPlayer runs only after native_setup succeeds, AudioTrack.java:912); AudioFlinger's and AudioPolicyService's calls were answered in-process (their counter lines).
//! `--break-serve` starts the app with `--no-audio-service` (atld, then the launcher): it serves neither name, so the session line must go red.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::atld_run::{Run, Runtime, counter_lines};
use crate::manifest;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's slot, see `xtask slot`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's slot).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
    /// The deliberate break: the app starts with `--no-audio-service`, so it serves neither AudioFlinger nor AudioPolicyService, and the session line must go red.
    #[arg(long)]
    break_serve: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/audio";
/// Main.onCreate's log tag and the line that ends the run.
const TAG: &str = "AtlAudio: ";
const DONE: &str = "AtlAudio: done";
/// The app's uid under atld (as the app gate's).
const APP_UID: u32 = 10_123;
/// The run's time limit: the APK's start, the two binder lookups' waits (5 s each when a service is missing) and the track.
const LIMIT: Duration = Duration::from_secs(180);

/// The served output (atl-audioflinger lib.rs OUTPUT_RATE_HZ, OUTPUT_FRAMES, OUTPUT_LATENCY_MS), copied: the expectation is worked from these, not read back from the service.
const SPEC_RATE_HZ: u64 = 48_000;
const SPEC_FRAMES: u64 = 1024;
const SPEC_LATENCY_MS: u64 = 42;
/// The request Main.java makes: 44100 Hz, stereo (2 channels), 16-bit (2 bytes per sample).
const REQ_RATE_HZ: u64 = 44_100;
const REQ_CHANNELS: u64 = 2;
const REQ_BYTES_PER_SAMPLE: u64 = 2;

/// The counter lines the in-process check needs: AudioFlinger's ids and rate, AudioPolicyService's output.
const IN_PROCESS: [&str; 3] = [
    "answered IAudioFlingerService.newAudioUniqueId",
    "answered IAudioFlingerService.sampleRate",
    "answered IAudioPolicyService.getOutput",
];

/// AudioSystem::calculateMinFrameCount at speed 1.0, in frames: `minBufCount` quanta (at least 2) of `sourceFramesNeeded + 2` source frames each, the stretch's `+1 +1` (AudioResamplerPublic.h:109).
/// The source frames are `dst * src / dstRate + 1 + 1` (uint64 math), since the rates differ.
fn expected_min_frames() -> u64 {
    let period_ms = 1000 * SPEC_FRAMES / SPEC_RATE_HZ;
    let min_buf = (SPEC_LATENCY_MS / period_ms).max(2);
    let needed = SPEC_FRAMES * REQ_RATE_HZ / SPEC_RATE_HZ + 2;
    min_buf * (needed + 2)
}

/// The value of the app's `AtlAudio: <key><value>` line, the last one if the app logged it twice.
fn line_value<'a>(app: &'a str, key: &str) -> Option<&'a str> {
    app.lines()
        .filter_map(|l| l.find(TAG).map(|i| &l[i + TAG.len()..]))
        .filter_map(|m| m.strip_prefix(key))
        .next_back()
}

fn verdict(name: &str, pass: bool, detail: &str) -> bool {
    println!(
        "audio-app: {name}: {} {detail}",
        if pass { "PASS" } else { "FAIL" }
    );
    pass
}

/// The native goals the gate reads: the app gate's (same runtime and APK tooling, native-bridge guest included).
pub fn goals() -> crate::gate_goals::Goals {
    crate::app::goals()
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        "audio-app",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "audio-art",
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver()?
    .with_native_bridge_guest(&args.host)?;
    let rt = if args.break_serve {
        rt.without_audio_service()
    } else {
        rt
    };
    let work = rt.out.join("audio");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    println!("audio-app: built {} ({})", apk.display(), m.name);
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![(apk, m.clone())],
        first_uid: APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    let mut atld_child = rt.start_atld(&run)?;
    let app_log = run.log(&m.name);
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let start = Instant::now();
    loop {
        if read(&app_log).lines().any(|l| l.contains(DONE)) {
            break;
        }
        if atld_child.try_wait()?.is_some() || run.app_ended() || start.elapsed() > LIMIT {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if atld_child.try_wait()?.is_none() {
        run.stop(&mut atld_child)?;
    }
    let app = read(&app_log);
    let services = read(&run.log("services"));
    if args.verbose {
        println!("---- app log\n{app}");
    }
    for l in counter_lines(&services) {
        println!("audio-app: counter {l}");
    }

    let sessions: Vec<i64> = line_value(&app, "session: ")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    let session_ok = matches!(sessions.as_slice(), [a, b] if *a > 0 && *b > 0 && a != b);
    let mut ok = verdict(
        "session ids",
        session_ok,
        &format!("{sessions:?} (nonzero, distinct)"),
    );

    let want = expected_min_frames() * REQ_CHANNELS * REQ_BYTES_PER_SAMPLE;
    let got: Option<i64> = line_value(&app, "minbuf: ").and_then(|v| v.trim().parse().ok());
    let frame_bytes = REQ_CHANNELS * REQ_BYTES_PER_SAMPLE;
    ok &= verdict(
        "min buffer",
        got == Some(i64::try_from(want)?),
        &format!(
            "got {got:?}, expected {want} bytes ({} frames)",
            want / frame_bytes
        ),
    );

    // Deferred, not judged: trackPlayer runs only after native_setup succeeds (AudioTrack.java:912), and createTrack is A1b.
    let track = line_value(&app, "track: ").unwrap_or("no line");
    let track_served = counter_lines(&services)
        .iter()
        .any(|l| l.starts_with("answered IAudioService.trackPlayer"));
    println!(
        "audio-app: track player id: DEFERRED to A1b (trackPlayer answered: {track_served}; track: {track})"
    );

    let in_process = IN_PROCESS
        .iter()
        .all(|n| app.contains(n) || services.contains(n));
    ok &= verdict(
        "in-process ids",
        in_process,
        "AudioFlinger and AudioPolicyService answered in the app process",
    );
    if !ok {
        println!("---- app log tail");
        for l in app
            .lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            println!("{l}");
        }
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
