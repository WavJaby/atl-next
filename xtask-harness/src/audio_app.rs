// SPDX-License-Identifier: MIT
//! `xtask test audio-app`: the audio gate of A1a (AudioFlinger and AudioPolicyService served) and A1b (createTrack and IAudioTrack). The test APK (xtask-harness/testdata/audio) is started by atld as the app gate's is; its process asks the audio services for the session ids and the minimum buffer, builds a track and a low-latency track, and logs their state, piid and frame count.
//! Expected values are AOSP's formulas worked here over the served output, not the code under test: AudioSystem::calculateMinFrameCount (AudioSystem.cpp:486) with sourceFramesNeededWithTimestretch (AudioResamplerPublic.h:84-110) at speed 1.0, and AudioFlinger's streaming-track roundup to a power of two (Tracks.cpp:162).
//! The served profile is the PCM output's granted rate and quantum (game audio A2), which on this host are PipeWire's defaults: [`SPEC_RATE_HZ`] and the others copy them, so a different graph must change these too (a mismatch is a red gate).
//! PCM (A2): the fixture plays a static track, then a streaming track, each a tone (Main.java `pcm`). The output's sink is a null sink the gate makes the default for the run, and its monitor is recorded with `pw-record` (audio_pcm.rs). The recording must hold two runs of tone with the frequency, length and level the fixture's input gives, and no sample beyond full scale. Expected values come from the constants below, never from the server.
//! Checks: session ids nonzero and distinct; getMinBufferSize equals the expected bytes (stereo, 16-bit); AudioManager's output rate and frames-per-buffer properties are the served profile's; a track reaches STATE_INITIALIZED with a nonzero piid (AudioTrack calls baseRegisterPlayer only after native_setup succeeds, AudioTrack.java:912) and the expected frame count; the low-latency track initialises although the server refuses FAST (counted); AudioFlinger's and AudioPolicyService's calls were answered in-process.
//! `--break-serve` starts the app with `--no-audio-service` (atld, then the launcher): it serves neither name, so the session line must go red. `--break-cblk` starts it with `--break-cblk`: each streaming track's region is one buffer short, createTrack refuses it, and both streaming tracks fail to initialise (a static track has no buffer of its own to be short of). `--break-mix` starts it with `--break-mix`: the mixer plays silence, so the PCM checks find no tone and must go red. `--unreachable` makes the desktop's server a stale socket file (`PIPEWIRE_REMOTE` for atld): the app must still reach `done` (the launch is not fatal), its track must fail with STATE_UNINITIALIZED and a counted refusal, and no tone may reach the sink.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use std::os::unix::net::UnixListener;

use anyhow::{Context, Result};

use crate::atld_run::{Package, Run, Runtime, counter_lines};
use crate::audio_pcm::{
    FULL_SCALE, Recorder, Recording, Routing, frequency_hz, out_of_range, read_wav, rms,
};
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
    /// The second deliberate break: the app starts with `--break-cblk`, so each streaming track's control block region is one buffer short; createTrack refuses it and both streaming tracks must fail to initialise.
    #[arg(long)]
    break_cblk: bool,
    /// The third deliberate break: the app starts with `--break-mix`, so the mixer plays silence; the recording holds no tone and the PCM checks must go red.
    #[arg(long)]
    break_mix: bool,
    /// The unreachable server: atld's desktop names a socket file that nothing listens on. The app must still reach `done`, its track must fail with AOSP's error path (STATE_UNINITIALIZED, counted), and no tone may reach the sink.
    #[arg(long)]
    unreachable: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask-harness/testdata/audio";
/// Main.onCreate's log tag and the line that ends the run.
const TAG: &str = "AtlAudio: ";
const DONE: &str = "AtlAudio: done";
/// The run's time limit: the APK's start, the two binder lookups' waits (5 s each when a service is missing) and the two tracks.
const LIMIT: Duration = Duration::from_secs(180);

/// The served profile: the PCM output's granted rate and quantum, which on this host are PipeWire's defaults (`default.clock.rate`, `default.clock.quantum`). The expectation is worked from these, not read back from the service.
const SPEC_RATE_HZ: u64 = 48_000;
const SPEC_FRAMES: u64 = 1024;
const SPEC_LATENCY_MS: u64 = 42;
/// The request Main.java makes: 44100 Hz, stereo (2 channels), 16-bit (2 bytes per sample).
const REQ_RATE_HZ: u64 = 44_100;
const REQ_CHANNELS: u64 = 2;
const REQ_BYTES_PER_SAMPLE: u64 = 2;
/// The static track's buffer in Main.java (`STATIC_BYTES`): 4096 bytes, one frame per 4 bytes of the request. Not rounded: a shared-buffer track keeps its frame count (Tracks.cpp:162 rounds only a streaming track).
const STATIC_BYTES: u64 = 4096;

/// The counter lines the in-process check needs: AudioFlinger's ids, rate and track, AudioPolicyService's output. getOutputForAttr is not served (policy.rs): no client on the game path calls it (AudioTrack asks getOutput(stream)), so it is not required here.
const IN_PROCESS: [&str; 4] = [
    "answered IAudioFlingerService.newAudioUniqueId",
    "answered IAudioFlingerService.sampleRate",
    "answered IAudioPolicyService.getOutput",
    "answered IAudioFlingerService.createTrack",
];
/// The counter line of a FAST request the server refused (flinger.rs createTrack).
const FAST_REFUSED: &str = "refused IAudioFlingerService.createTrack.FAST";
/// The counter line of a createTrack the server refused for want of an output (flinger.rs createTrack, AOSP AudioFlinger.cpp:1155).
const OUTPUT_REFUSED: &str = "refused IAudioFlingerService.createTrack.output";
/// AudioTrack.STATE_UNINITIALIZED: a track whose native setup failed.
const STATE_UNINITIALIZED: i64 = 0;
/// The constructor's message when the minimum buffer is refused (AudioTrack.java:2024, from audioBuffSizeCheck at :863).
const MIN_BUFFER_REFUSED: &str = "Invalid audio buffer size";
/// AudioTrack.STATE_INITIALIZED.
const STATE_INITIALIZED: i64 = 1;
/// AudioTrack.STATE_NO_STATIC_DATA (AudioTrack.java:170): a static track whose native setup succeeded and that has no data yet (set at :907).
const STATE_NO_STATIC_DATA: i64 = 2;
/// AudioPlaybackConfiguration.PLAYER_PIID_INVALID: a player that has no piid.
const PIID_INVALID: i64 = -1;

/// The PCM tones (game audio A2). Main.java plays them in this order: a static track of STATIC_TONE_HZ (TONE_FRAMES frames at TONE_RATE_HZ), then a streaming track of STREAM_TONE_HZ in FMOD's Java output shape (STREAM_FRAMES at STREAM_RATE_HZ: Temple Run's FMOD asks 24000 Hz, stream-type constructor, byte writes), both TONE_DBFS peak, with a gap of about 0.5 s between. The fixture's samples come from the same values.
const TONE_RATE_HZ: u64 = 44_100;
const TONE_FRAMES: u64 = 22_050;
const STREAM_RATE_HZ: u64 = 24_000;
const STREAM_FRAMES: u64 = 12_000;
const STATIC_TONE_HZ: f64 = 1_000.0;
const STREAM_TONE_HZ: f64 = 500.0;
const TONE_DBFS: f64 = -12.0;
/// Silence that separates two tone runs in the recording (the gap is about 0.5 s).
const GAP_SECONDS: f64 = 0.2;
/// As audio_pcm.rs: a sample at or above -60 dB of the peak counts as tone.
const PRESENCE_DB: f64 = -60.0;
const FREQ_TOLERANCE_HZ: f64 = 1.0;
const LEVEL_TOLERANCE_DB: f64 = 1.0;
/// One quantum of PipeWire's default graph (1024 frames, `default.clock.quantum`): the length tolerance, and the trim at each edge of a run before frequency and level are measured.
const QUANTUM_FRAMES: usize = 1_024;

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

/// Runs of samples whose magnitude reaches `threshold`; two runs are one when at most `gap` samples lie between them.
fn segments(x: &[f32], threshold: f32, gap: usize) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, v) in x.iter().enumerate() {
        if v.abs() < threshold {
            continue;
        }
        match runs.last_mut() {
            Some(run) if i - run.1 <= gap => run.1 = i,
            _ => runs.push((i, i)),
        }
    }
    runs
}

/// The presence level: a sample at or above PRESENCE_DB of the tone's peak counts as tone.
fn presence() -> f32 {
    let amplitude = 10f64.powf(TONE_DBFS / 20.0);
    (amplitude * 10f64.powf(PRESENCE_DB / 20.0)) as f32
}

/// The PCM checks (game audio A2): the recording holds the static tone, then the streaming tone, each a run of samples above the presence level. Length, frequency and level come from the fixture's input (TONE_*), never from the server. A sample beyond full scale fails the run before any tone is measured.
fn pcm_verdicts(rec: &Recording) -> bool {
    let bad = out_of_range(&rec.samples);
    if !verdict(
        "pcm range",
        bad == 0,
        &format!(
            "{bad} of {} samples NaN or beyond ±{FULL_SCALE} (full scale)",
            rec.samples.len()
        ),
    ) {
        return false;
    }
    let rate = u64::from(rec.rate);
    let amplitude = 10f64.powf(TONE_DBFS / 20.0);
    let expected_rms = amplitude / std::f64::consts::SQRT_2;
    let threshold = presence();
    let gap = (GAP_SECONDS * f64::from(rec.rate)) as usize;
    let runs = segments(&rec.channel(0), threshold, gap);
    let mut ok = verdict(
        "pcm tones",
        runs.len() == 2,
        &format!(
            "{} run(s) of tone, want 2 (static, then streaming)",
            runs.len()
        ),
    );
    for (i, (name, hz, want)) in [
        ("static", STATIC_TONE_HZ, TONE_FRAMES * rate / TONE_RATE_HZ),
        (
            "stream",
            STREAM_TONE_HZ,
            STREAM_FRAMES * rate / STREAM_RATE_HZ,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(&(first, last)) = runs.get(i) else {
            ok &= verdict(&format!("pcm {name} tone"), false, "not in the recording");
            continue;
        };
        let span = last - first + 1;
        ok &= verdict(
            &format!("pcm {name} length"),
            (span as u64).abs_diff(want) <= QUANTUM_FRAMES as u64,
            &format!("{span} frames at {rate} Hz, want {want} ±{QUANTUM_FRAMES} (one quantum)"),
        );
        if span <= 2 * QUANTUM_FRAMES {
            ok &= verdict(
                &format!("pcm {name} frequency"),
                false,
                "run shorter than two quanta",
            );
            continue;
        }
        // the central part: one quantum off each edge, so edge timing does not reach the estimates
        for ch in 0..usize::from(rec.channels) {
            let x = rec.channel(ch);
            let central = &x[first + QUANTUM_FRAMES..=last - QUANTUM_FRAMES];
            let got = frequency_hz(central, rate as f64);
            ok &= verdict(
                &format!("pcm {name} frequency"),
                got.is_some_and(|f| (f - hz).abs() <= FREQ_TOLERANCE_HZ),
                &format!("ch{ch} {got:?} Hz, want {hz} ±{FREQ_TOLERANCE_HZ}"),
            );
            let level_db = 20.0 * (rms(central) / expected_rms).log10();
            ok &= verdict(
                &format!("pcm {name} level"),
                level_db.abs() <= LEVEL_TOLERANCE_DB,
                &format!("ch{ch} {level_db:+.2} dB re A/√2, want ±{LEVEL_TOLERANCE_DB}"),
            );
        }
    }
    ok
}

/// A socket file nothing listens on: the unreachable server `--unreachable` names to atld. Dropping a std listener leaves the file, so it stays for the run.
fn stale_socket(work: &Path) -> Result<PathBuf> {
    fs::create_dir_all(work)?;
    let socket = std::path::absolute(work.join("pipewire-stale"))?;
    let _ = fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    drop(listener);
    Ok(socket)
}

/// The unreachable-server checks: the launch survives (the app reaches `done`), the track fails with AOSP's error path (STATE_UNINITIALIZED, the refusal counted), and the sink holds no tone.
fn unreachable_verdicts(app: &str, services: &str, wav: &Path) -> bool {
    let mut ok = verdict(
        "launch",
        app.contains(DONE),
        "the app reached `done` with no audio server",
    );
    // the streaming track asks getMinBufferSize first; with no output that is an error, so its constructor throws (AudioTrack.java:863, :2024)
    let track = line_value(app, "track: ");
    ok &= verdict(
        "streaming track",
        track.is_some_and(|l| l.contains(MIN_BUFFER_REFUSED)),
        &format!(
            "refused before createTrack, line: {}",
            track.unwrap_or("no line")
        ),
    );
    // the static track needs no minimum buffer: createTrack itself is refused (AudioFlinger.cpp:1155-1158), so the track is STATE_UNINITIALIZED
    let stat = line_value(app, "static: ");
    let stat_state = field(stat, "state");
    ok &= verdict(
        "static track",
        stat_state == Some(STATE_UNINITIALIZED),
        &format!(
            "state {stat_state:?} (STATE_UNINITIALIZED), line: {}",
            stat.unwrap_or("no line")
        ),
    );
    let refused = app.contains(OUTPUT_REFUSED) || services.contains(OUTPUT_REFUSED);
    ok &= verdict(
        "refusal counted",
        refused,
        "createTrack refused for want of an output",
    );
    let recording = fs::read(wav)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| read_wav(&bytes));
    ok &= match recording {
        Ok(rec) => {
            let gap = (GAP_SECONDS * f64::from(rec.rate)) as usize;
            let runs = segments(&rec.channel(0), presence(), gap).len();
            verdict(
                "no sound",
                runs == 0,
                &format!("{runs} run(s) of tone in the recording, want 0"),
            )
        }
        Err(e) => verdict("no sound", false, &format!("{e:#}")),
    };
    ok
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
    .with_gl_driver(root)?
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
    let rt = if args.break_mix {
        rt.with_broken_mix()
    } else {
        rt
    };
    let work = rt.out.join("audio");
    let rt = if args.unreachable {
        rt.with_pipewire_remote(stale_socket(&work)?)
    } else {
        rt
    };
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    println!("audio-app: built {} ({})", apk.display(), m.name);
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    // the output plays into a null sink the gate makes the default for the run; its monitor is recorded (audio_pcm.rs)
    let sink = format!("atl-audio-app-{}", std::process::id());
    let wav = work.join("recording.wav");
    let routing = Routing::setup(&sink)?;
    let recorder = Recorder::start(&sink, &wav, u32::try_from(SPEC_RATE_HZ)?)?;
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
    recorder.stop()?;
    drop(routing);
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
    if args.unreachable {
        let ok = unreachable_verdicts(&app, &services, &wav);
        return Ok(if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        });
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

    // AudioManager.getProperty: Integer.toString of the primary output's rate and HAL frame count, null when AudioFlinger answers 0 (AudioManager.java getProperty)
    let props = line_value(&app, "props: ");
    let want_props = format!("rate={SPEC_RATE_HZ} frames={SPEC_FRAMES}");
    ok &= verdict(
        "output properties",
        props == Some(want_props.as_str()),
        &format!("got {props:?}, expected {want_props} (the served profile)"),
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

    let stat = line_value(&app, "static: ");
    let stat_state = field(stat, "state");
    let stat_frames = field(stat, "frames");
    let want_static = i64::try_from(STATIC_BYTES / (REQ_CHANNELS * REQ_BYTES_PER_SAMPLE))?;
    ok &= verdict(
        "static track",
        stat_state == Some(STATE_NO_STATIC_DATA) && stat_frames == Some(want_static),
        &format!(
            "state {stat_state:?} (STATE_NO_STATIC_DATA), frames {stat_frames:?}, expected {want_static} (the shared buffer's frames, not rounded)"
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
    let pcm = fs::read(&wav)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| read_wav(&bytes));
    ok &= match pcm {
        Ok(recording) => pcm_verdicts(&recording),
        Err(e) => verdict("pcm recording", false, &format!("{e:#}")),
    };
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
