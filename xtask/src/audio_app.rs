// SPDX-License-Identifier: MIT
//! `xtask test audio-app`: the audio gate of A1a (AudioFlinger and AudioPolicyService served) and A1b (createTrack and IAudioTrack). The test APK (xtask/testdata/audio) is started by atld as the app gate's is; its process asks the audio services for the session ids and the minimum buffer, builds a track and a low-latency track, and logs their state, piid and frame count.
//! Expected values are AOSP's formulas worked here over the served output, not the code under test: AudioSystem::calculateMinFrameCount (AudioSystem.cpp:486) with sourceFramesNeededWithTimestretch (AudioResamplerPublic.h:84-110) at speed 1.0, and AudioFlinger's streaming-track roundup to a power of two (Tracks.cpp:162).
//! The served output is atl-audioflinger's OUTPUT_* constants; [`SPEC_RATE_HZ`] and the others copy them, so a change there must change these too (a mismatch is a red gate).
//! Checks: session ids nonzero and distinct; getMinBufferSize equals the expected bytes (stereo, 16-bit); a track reaches STATE_INITIALIZED with a nonzero piid (AudioTrack calls baseRegisterPlayer only after native_setup succeeds, AudioTrack.java:912) and the expected frame count; the low-latency track initialises although the server refuses FAST (counted); AudioFlinger's and AudioPolicyService's calls were answered in-process.
//! `--break-serve` starts the app with `--no-audio-service` (atld, then the launcher): it serves neither name, so the session line must go red. `--break-cblk` starts it with `--break-cblk`: each track's region is one buffer short, createTrack refuses it, and both tracks fail to initialise.

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
    /// The second deliberate break: the app starts with `--break-cblk`, so each track's control block region is one buffer short; createTrack refuses it and both tracks must fail to initialise.
    #[arg(long)]
    break_cblk: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/audio";
/// Main.onCreate's log tag and the line that ends the run.
const TAG: &str = "AtlAudio: ";
const DONE: &str = "AtlAudio: done";
/// The app's uid under atld (as the app gate's).
const APP_UID: u32 = 10_123;
/// The run's time limit: the APK's start, the two binder lookups' waits (5 s each when a service is missing) and the two tracks.
const LIMIT: Duration = Duration::from_secs(180);

/// The served output (atl-audioflinger lib.rs OUTPUT_RATE_HZ, OUTPUT_FRAMES, OUTPUT_LATENCY_MS), copied: the expectation is worked from these, not read back from the service.
const SPEC_RATE_HZ: u64 = 48_000;
const SPEC_FRAMES: u64 = 1024;
const SPEC_LATENCY_MS: u64 = 42;
/// The request Main.java makes: 44100 Hz, stereo (2 channels), 16-bit (2 bytes per sample).
const REQ_RATE_HZ: u64 = 44_100;
const REQ_CHANNELS: u64 = 2;
const REQ_BYTES_PER_SAMPLE: u64 = 2;

/// The counter lines the in-process check needs: AudioFlinger's ids, rate and track, AudioPolicyService's output. getOutputForAttr is served but no client on the game path calls it (AudioTrack asks getOutput(stream)), so it is not required here.
const IN_PROCESS: [&str; 4] = [
    "answered IAudioFlingerService.newAudioUniqueId",
    "answered IAudioFlingerService.sampleRate",
    "answered IAudioPolicyService.getOutput",
    "answered IAudioFlingerService.createTrack",
];
/// The counter line of a FAST request the server refused (flinger.rs createTrack).
const FAST_REFUSED: &str = "refused IAudioFlingerService.createTrack.FAST";
/// AudioTrack.STATE_INITIALIZED; STATE_UNINITIALIZED is 0.
const STATE_INITIALIZED: i64 = 1;
/// AudioPlaybackConfiguration.PLAYER_PIID_INVALID: a player that has no piid.
const PIID_INVALID: i64 = -1;

/// AudioSystem::calculateMinFrameCount at speed 1.0, in frames: `minBufCount` quanta (at least 2) of `sourceFramesNeeded + 2` source frames each, the stretch's `+1 +1` (AudioResamplerPublic.h:109).
/// The source frames are `dst * src / dstRate + 1 + 1` (uint64 math), since the rates differ.
fn expected_min_frames() -> u64 {
    let period_ms = 1000 * SPEC_FRAMES / SPEC_RATE_HZ;
    let min_buf = (SPEC_LATENCY_MS / period_ms).max(2);
    let needed = SPEC_FRAMES * REQ_RATE_HZ / SPEC_RATE_HZ + 2;
    min_buf * (needed + 2)
}

/// The frame count of a track requested at the minimum buffer: AudioFlinger rounds a streaming track's frame count up to a power of two (Tracks.cpp:162, `roundup`).
fn expected_track_frames() -> u64 {
    expected_min_frames().next_power_of_two()
}

/// The value of the app's `AtlAudio: <key><value>` line, the last one if the app logged it twice.
fn line_value<'a>(app: &'a str, key: &str) -> Option<&'a str> {
    app.lines()
        .filter_map(|l| l.find(TAG).map(|i| &l[i + TAG.len()..]))
        .filter_map(|m| m.strip_prefix(key))
        .next_back()
}

/// The integer of `key=<n>` among a log line's fields (`state=1 piid=7 frames=2048`).
fn field(text: Option<&str>, key: &str) -> Option<i64> {
    text?
        .split_whitespace()
        .find_map(|f| f.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
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
    let rt = if args.break_cblk {
        rt.with_broken_cblk()
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

    let track = line_value(&app, "track: ");
    let state = field(track, "state");
    let piid = field(track, "piid");
    let frames = field(track, "frames");
    let want_frames = i64::try_from(expected_track_frames())?;
    ok &= verdict(
        "track init",
        state == Some(STATE_INITIALIZED),
        &format!(
            "state {state:?} (STATE_INITIALIZED), line: {}",
            track.unwrap_or("no line")
        ),
    );
    ok &= verdict(
        "track piid",
        piid.is_some_and(|p| p != 0 && p != PIID_INVALID),
        &format!("piid {piid:?} (nonzero)"),
    );
    ok &= verdict(
        "track frames",
        frames == Some(want_frames),
        &format!(
            "got {frames:?}, expected {want_frames} (the min buffer rounded up to a power of two)"
        ),
    );

    let fast = line_value(&app, "fast: ");
    let fast_state = field(fast, "state");
    let fast_refused = app.contains(FAST_REFUSED) || services.contains(FAST_REFUSED);
    ok &= verdict(
        "fast track",
        fast_state == Some(STATE_INITIALIZED) && fast_refused,
        &format!(
            "state {fast_state:?} (STATE_INITIALIZED); server refused FAST (counted): {fast_refused}"
        ),
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
