// SPDX-License-Identifier: MIT
//! `xtask test audio-pcm` (plan A0): the PCM output of atl-platform-ui-linux plays a 1 kHz tone through PipeWire; this side records the sink it plays into and checks the recording.
//! The client is `crates/atl-platform-ui/examples/audio_pcm.rs`: this side builds and runs it, then checks the recording against values derived here from the arguments it was given, never from the client's own report. Gates:
//! 1. **frequency**: rising zero crossings, linearly interpolated over the tone's central part, give 1000 Hz ±1 Hz on both channels.
//! 2. **level**: the RMS of that part equals A/√2 within 1 dB on both channels, A = 10^(dBFS/20) of the requested peak.
//! 3. **duration**: from its first to its last sample above A·10^(-60/20), the tone spans the requested frames ±1 quantum (the client reports the server's quantum).
//! 4. **refusal**: a stream the server refuses to run ends the client with an `error` line, which fails the run; it is counted, never taken for silence.
//! 5. **rt**: counted, not judged: 1 when the stream's first callback had no SCHED_FIFO/SCHED_RR (rtkit absent on WSL, so 1 is expected there).
//! 6. **underruns**: counted, not judged: server cycles that found no buffer queued, where the sound was cut (0 expected).
//!
//! Breaks (each must turn the run red): `--break zeros` (the client writes silence: gates 1-3 fail), `--break no-target` (a PipeWire server with no session manager, so nothing links the stream: gate 4 fails).
//!
//! Environment: the normal run makes a null sink the server's default sink for its duration (`pw-cli create-node`, `pw-metadata`; both restored and removed on every exit path) and records its monitor with `pw-record`. Needs pipewire, pw-cli, pw-metadata and pw-record on PATH. `no-target` runs its own server in a temporary runtime dir (`PIPEWIRE_RUNTIME_DIR`), so no real sink is reached in either mode.
//!
//! Evidence tier: isolated runtime against the real PipeWire graph; the Arch laptop runs the same gate (it needs no WSLg path).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::verdict::{Failure, Verdict};

/// The tone's frequency in Hz, the client's `--freq` default.
const FREQ_HZ: f64 = 1000.0;
/// Frequency tolerance in Hz.
const FREQ_TOLERANCE_HZ: f64 = 1.0;
/// Level tolerance in dB.
const LEVEL_TOLERANCE_DB: f64 = 1.0;
/// Presence threshold relative to the tone's amplitude: samples below it count as silence. -60 dB is far under the tone and far above the 24-bit noise floor of a recorded null sink (which is exact zero).
const PRESENCE_DB: f64 = -60.0;
/// Channels the client plays: the tone on each, checked separately.
const CHANNELS: u16 = 2;
/// Seconds of recording before the client starts, so the tone is never cut at its onset.
const RECORD_LEAD_SECONDS: f64 = 0.1;
/// Bytes `pw-record` writes before the samples: its RIFF header with fmt and fact chunks is 80 bytes; the slack covers a larger one.
const WAV_HEADER_SLACK: u64 = 256;
/// How long the recorder may take to produce its lead, and the server its first socket.
const READY_LIMIT: Duration = Duration::from_secs(5);
/// How long the default-sink change may take to show in the metadata.
const ROUTE_LIMIT: Duration = Duration::from_secs(2);
/// The sink's object name; the process id keeps two runs apart.
const SINK_PREFIX: &str = "atl-audio-pcm";
/// The default-sink metadata key (WirePlumber's default.audio.sink).
const DEFAULT_SINK_KEY: &str = "default.audio.sink";

#[derive(clap::Args)]
pub struct Args {
    /// Frames per second: the client's request, and the recording's rate.
    #[arg(long, default_value_t = 48000)]
    rate: u32,
    /// Peak amplitude of the tone in dBFS.
    #[arg(long, default_value_t = -12.0, allow_hyphen_values = true)]
    dbfs: f64,
    /// Tone length in milliseconds.
    #[arg(long, default_value_t = 1000)]
    duration_ms: u64,
    /// Deliberate fault that must turn the run red.
    #[arg(long = "break", value_enum)]
    break_: Option<Break>,
    /// Keep the recording and the logs in this directory instead of a temporary one.
    #[arg(long)]
    keep: Option<PathBuf>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Break {
    /// The client writes silence where the tone is.
    Zeros,
    /// No session manager links the client's stream: a private server, not the desktop's.
    NoTarget,
}

/// The client's report: the lines it printed and its exit status.
struct ClientRun {
    success: bool,
    error: Option<String>,
    quantum_frames: Option<u64>,
    rt_fallbacks: Option<u64>,
    underruns: Option<u64>,
    frames: Option<u64>,
}

/// Everything the run checked, and what failed.
#[derive(Default)]
struct Report {
    checks: usize,
    failures: Vec<Failure>,
}

impl Report {
    fn check(&mut self, step: &str, ok: bool, text: impl FnOnce() -> String) {
        self.checks += 1;
        if !ok {
            self.failures.push(Failure::step(step, text()));
        }
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let client = build_client(root)?;
    let dir = match &args.keep {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join(format!("{SINK_PREFIX}-{}", std::process::id())),
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut report = Report::default();
    let outcome = match args.break_ {
        Some(Break::NoTarget) => no_target(&client, args, &dir, &mut report),
        _ => normal(&client, args, &dir, &mut report),
    };
    if let Err(err) = outcome {
        report.failures.push(Failure::gate(&format!("{err:#}")));
    }
    if args.keep.is_none() {
        let _ = fs::remove_dir_all(&dir);
    }
    let verdict = Verdict {
        cmd: "audio-pcm",
        total: (report.checks > 0).then_some((report.checks, "checks")),
        skipped: Vec::new(),
        failures: report.failures,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Records the default sink's monitor while the client plays into the null sink the gate made the default.
fn normal(client: &Path, args: &Args, dir: &Path, report: &mut Report) -> Result<()> {
    let name = format!("{SINK_PREFIX}-{}", std::process::id());
    let wav = dir.join("recording.wav");
    let routing = Routing::setup(&name)?;
    let recorder = Recorder::start(&routing.name, &wav, args.rate)?;
    let remote = atl_platform_host::app_access::pipewire_socket().context(
        "no PipeWire socket on this desktop: the normal run plays into the desktop's server",
    )?;
    let run = run_client(client, args, &[], &remote)?;
    recorder.stop()?;
    drop(routing);
    if let Some(error) = &run.error {
        report.check("refusal", false, || {
            format!("the output refused to run: {error}")
        });
        return Ok(());
    }
    report.check("client", run.success, || {
        "the client exited with failure".to_owned()
    });
    if let Some(rt) = run.rt_fallbacks {
        println!(
            "rt: {rt} stream(s) ran without SCHED_FIFO/SCHED_RR (counted; no realtime grant from the session: rtkit absent, or the client not in an active local session)"
        );
    }
    if let Some(underruns) = run.underruns {
        println!(
            "underruns: {underruns} server cycle(s) without a buffer (counted; sound cut there)"
        );
    }
    let quantum = run
        .quantum_frames
        .context("the client reported no quantum")?;
    let frames = run.frames.context("the client reported no frame count")?;
    ensure!(frames > 0, "the client produced no frames");
    let recording =
        read_wav(&fs::read(&wav).with_context(|| format!("reading {}", wav.display()))?)?;
    check_recording(&recording, args, quantum, report)
}

/// Runs the client against a private server with no session manager: nothing links its stream, so the output must refuse its start.
fn no_target(client: &Path, args: &Args, dir: &Path, report: &mut Report) -> Result<()> {
    let runtime = dir.join("runtime");
    fs::create_dir_all(&runtime).with_context(|| format!("creating {}", runtime.display()))?;
    let env = [("PIPEWIRE_RUNTIME_DIR", runtime.as_path())];
    let mut server = PrivateServer::start(&runtime, dir)?;
    let run = run_client(client, args, &env, &runtime.join("pipewire-0"));
    server.stop();
    let run = run?;
    match run.error {
        Some(error) => report.check("refusal", false, || {
            format!("the output refused to run: {error}")
        }),
        None => report.check("refusal", false, || {
            "the output ran with no sink linked, and reported nothing".to_owned()
        }),
    }
    Ok(())
}

/// Checks the recording against the tone the client was asked for: values from `args` and the client's quantum, not from the client's output.
fn check_recording(
    recording: &Recording,
    args: &Args,
    quantum: u64,
    report: &mut Report,
) -> Result<()> {
    ensure!(
        recording.rate == args.rate && recording.channels == CHANNELS,
        "recording is {} Hz x {} channels, want {} Hz x {CHANNELS}",
        recording.rate,
        recording.channels,
        args.rate
    );
    let amplitude = 10f64.powf(args.dbfs / 20.0);
    let expected_rms = amplitude / std::f64::consts::SQRT_2;
    let threshold = (amplitude * 10f64.powf(PRESENCE_DB / 20.0)) as f32;
    let expected_frames = args.duration_ms * u64::from(args.rate) / 1000;
    let quantum = usize::try_from(quantum).context("quantum does not fit")?;
    for channel in 0..usize::from(CHANNELS) {
        let x = recording.channel(channel);
        let Some((first, last)) = active_span(&x, threshold) else {
            for gate in ["frequency", "level", "duration"] {
                report.check(gate, false, || {
                    format!("ch{channel}: no tone above the presence threshold (silence)")
                });
            }
            continue;
        };
        let span = last - first + 1;
        let span_frames = u64::try_from(span).context("span does not fit")?;
        report.check("duration", span_frames.abs_diff(expected_frames) <= quantum as u64, || {
            format!("ch{channel}: tone spans {span_frames} frames, want {expected_frames} ±{quantum} (one quantum)")
        });
        // the central part: one quantum off each edge, so edge timing does not reach the estimates
        if span <= 2 * quantum {
            for gate in ["frequency", "level"] {
                report.check(gate, false, || {
                    format!("ch{channel}: tone shorter than two quanta, nothing to measure")
                });
            }
            continue;
        }
        let central = &x[first + quantum..=last - quantum];
        let hz = frequency_hz(central, f64::from(recording.rate));
        match hz {
            Some(hz) => report.check(
                "frequency",
                (hz - FREQ_HZ).abs() <= FREQ_TOLERANCE_HZ,
                || format!("ch{channel}: {hz:.3} Hz, want {FREQ_HZ} ±{FREQ_TOLERANCE_HZ}"),
            ),
            None => report.check("frequency", false, || {
                format!("ch{channel}: fewer than two zero crossings")
            }),
        }
        let level_db = 20.0 * (rms(central) / expected_rms).log10();
        report.check("level", level_db.abs() <= LEVEL_TOLERANCE_DB, || {
            format!(
                "ch{channel}: RMS {level_db:+.2} dB against A/√2 ({:+.2} dBFS), want ±{LEVEL_TOLERANCE_DB} dB",
                args.dbfs - 3.0103
            )
        });
        println!(
            "ch{channel}: span {span_frames} frames (want {expected_frames} ±{quantum}), {} Hz, level {level_db:+.3} dB re A/√2",
            hz.map_or("n/a".to_owned(), |hz| format!("{hz:.3}"))
        );
    }
    Ok(())
}

/// Builds the client example and returns its path.
fn build_client(root: &Path) -> Result<PathBuf> {
    let out = crate::cargo_command()
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "-p",
            "atl-platform-ui",
            "--example",
            "audio_pcm",
            "--message-format=json-render-diagnostics",
        ])
        .stderr(Stdio::inherit())
        .output()
        .context("running cargo build")?;
    ensure!(
        out.status.success(),
        "cargo build of the audio_pcm client failed"
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "audio_pcm")
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .context("cargo reported no audio_pcm executable")
}

/// Runs the client with the tone parameters of `args` (and a break, if any), connected to the server socket `remote`; returns what it printed.
fn run_client(
    client: &Path,
    args: &Args,
    env: &[(&str, &Path)],
    remote: &Path,
) -> Result<ClientRun> {
    let mut command = Command::new(client);
    command
        .arg("--rate")
        .arg(args.rate.to_string())
        .arg("--channels")
        .arg(CHANNELS.to_string())
        .arg("--freq")
        .arg(FREQ_HZ.to_string())
        .arg("--dbfs")
        .arg(args.dbfs.to_string())
        .arg("--duration-ms")
        .arg(args.duration_ms.to_string())
        .arg("--remote")
        .arg(remote);
    if matches!(args.break_, Some(Break::Zeros)) {
        command.args(["--break", "zeros"]);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().context("running the audio_pcm client")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut run = ClientRun {
        success: output.status.success(),
        error: None,
        quantum_frames: None,
        rt_fallbacks: None,
        underruns: None,
        frames: None,
    };
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "error" => run.error = Some(value.to_owned()),
            "quantum_frames" => run.quantum_frames = value.parse().ok(),
            "rt_fallbacks" => run.rt_fallbacks = value.parse().ok(),
            "underruns" => run.underruns = value.parse().ok(),
            "frames" => run.frames = value.parse().ok(),
            _ => {}
        }
    }
    Ok(run)
}

/// The environment the normal run changes: a null sink and the default sink naming it. Dropping it restores the default and removes the sink, on every exit path.
pub(crate) struct Routing {
    name: String,
    sink: Option<String>,
    previous: Option<String>,
}

impl Routing {
    pub(crate) fn setup(name: &str) -> Result<Self> {
        let mut routing = Routing {
            name: name.to_owned(),
            sink: None,
            previous: default_sink()?,
        };
        let props = format!(
            "{{ factory.name=support.null-audio-sink node.name={name} media.class=Audio/Sink \
             audio.rate=48000 audio.channels={CHANNELS} audio.position=[ FL FR ] object.linger=true }}"
        );
        let created = pw_command(&["create-node", "adapter", &props])?;
        ensure!(created.status.success(), "pw-cli create-node failed");
        let id = node_id(name)?.context("the null sink does not show in the graph")?;
        routing.sink = Some(id);
        let json = sink_json(name);
        set_default_sink(&json)?;
        let deadline = Instant::now() + ROUTE_LIMIT;
        while default_sink()?.as_deref() != Some(json.as_str()) {
            ensure!(
                Instant::now() < deadline,
                "the default sink did not change to {name}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Ok(routing)
    }
}

impl Drop for Routing {
    fn drop(&mut self) {
        let restored = match &self.previous {
            Some(value) => set_default_sink(value),
            None => delete_default_sink(),
        };
        if let Err(err) = restored {
            report_cleanup(&format!("restoring the default sink: {err:#}"));
        }
        if let Some(id) = &self.sink
            && !pw_command(&["destroy", id]).is_ok_and(|o| o.status.success())
        {
            report_cleanup(&format!("destroying null sink {} (object {id})", self.name));
        }
    }
}

/// `pw-record` of the null sink's monitor, stopped with SIGINT so the WAV header is finalised.
pub(crate) struct Recorder {
    child: Child,
}

impl Recorder {
    pub(crate) fn start(sink: &str, wav: &Path, rate: u32) -> Result<Self> {
        let log = wav.with_extension("log");
        let out = fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
        let err = out.try_clone().context("duplicating the pw-record log")?;
        let child = Command::new("pw-record")
            .args(["-P", "stream.capture.sink=true", "--target", sink])
            .args([
                "--rate",
                &rate.to_string(),
                "--channels",
                &CHANNELS.to_string(),
            ])
            .args(["--format", "f32"])
            .arg(wav)
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .context("starting pw-record")?;
        let mut recorder = Recorder { child };
        let lead_bytes = (RECORD_LEAD_SECONDS * f64::from(rate) * f64::from(CHANNELS) * 4.0) as u64;
        let ready = lead_bytes + WAV_HEADER_SLACK;
        let deadline = Instant::now() + READY_LIMIT;
        loop {
            if fs::metadata(wav).map(|m| m.len() >= ready).unwrap_or(false) {
                return Ok(recorder);
            }
            if let Some(status) = recorder.child.try_wait().context("polling pw-record")? {
                bail!(
                    "pw-record exited before recording ({status}); its log: {}",
                    log.display()
                );
            }
            ensure!(
                Instant::now() < deadline,
                "pw-record recorded nothing within {READY_LIMIT:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ends the recording. pw-record exits with status 1 on SIGINT (its normal exit for that signal), so the exit status is not checked: a truncated file fails the duration gate instead.
    pub(crate) fn stop(mut self) -> Result<()> {
        let status = Command::new("kill")
            .arg("-INT")
            .arg(self.child.id().to_string())
            .status()
            .context("signalling pw-record")?;
        ensure!(status.success(), "kill -INT pw-record failed");
        self.child.wait().context("waiting for pw-record")?;
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A PipeWire server of the gate's own, with no session manager, in `runtime`.
struct PrivateServer {
    child: Child,
}

impl PrivateServer {
    fn start(runtime: &Path, dir: &Path) -> Result<Self> {
        let log = fs::File::create(dir.join("server.log")).context("creating the server log")?;
        let child = Command::new("pipewire")
            .env("PIPEWIRE_RUNTIME_DIR", runtime)
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .context("starting pipewire (the private server)")?;
        let mut server = PrivateServer { child };
        let socket = runtime.join("pipewire-0");
        let deadline = Instant::now() + READY_LIMIT;
        while !socket.exists() {
            ensure!(
                Instant::now() < deadline,
                "the private server made no socket within {READY_LIMIT:?}"
            );
            if let Some(status) = server
                .child
                .try_wait()
                .context("polling the private server")?
            {
                bail!("the private server exited ({status})");
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(server)
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Reports a failed cleanup step on stderr; the run's verdict is already decided by then.
fn report_cleanup(text: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "audio-pcm: cleanup failed: {text}");
}

fn pw_command(args: &[&str]) -> Result<std::process::Output> {
    Command::new("pw-cli")
        .args(args)
        .output()
        .with_context(|| format!("running pw-cli {}", args.first().copied().unwrap_or("")))
}

/// The object id of the node named `name` in `pw-cli ls Node`: the last `id` line above its `node.name`.
fn node_id(name: &str) -> Result<Option<String>> {
    let out = Command::new("pw-cli")
        .args(["ls", "Node"])
        .output()
        .context("listing nodes")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let wanted = format!("node.name = \"{name}\"");
    let mut id = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("id ") {
            id = rest.split(',').next().map(str::to_owned);
        } else if trimmed == wanted {
            return Ok(id);
        }
    }
    Ok(None)
}

fn sink_json(name: &str) -> String {
    format!("{{\"name\":\"{name}\"}}")
}

/// The default sink's current metadata value (the JSON `pw-metadata` prints), if the key is set.
fn default_sink() -> Result<Option<String>> {
    let out = Command::new("pw-metadata")
        .args(["-n", "default"])
        .output()
        .context("reading the default metadata")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let key = format!("key:'{DEFAULT_SINK_KEY}'");
    let value = text
        .lines()
        .rev()
        .find(|l| l.contains(&key))
        .and_then(|l| l.split_once("value:'"))
        .and_then(|(_, rest)| rest.split_once("' type:"))
        .map(|(value, _)| value.to_owned());
    Ok(value)
}

fn set_default_sink(json: &str) -> Result<()> {
    let status = Command::new("pw-metadata")
        .args([
            "-n",
            "default",
            "0",
            DEFAULT_SINK_KEY,
            json,
            "Spa:String:JSON",
        ])
        .status()
        .context("setting the default sink")?;
    ensure!(
        status.success(),
        "pw-metadata could not set the default sink"
    );
    Ok(())
}

fn delete_default_sink() -> Result<()> {
    let status = Command::new("pw-metadata")
        .args(["-n", "default", "-d", "0", DEFAULT_SINK_KEY])
        .status()
        .context("clearing the default sink")?;
    ensure!(
        status.success(),
        "pw-metadata could not clear the default sink"
    );
    Ok(())
}

/// A recording: interleaved samples and their format.
pub(crate) struct Recording {
    pub(crate) rate: u32,
    pub(crate) channels: u16,
    pub(crate) samples: Vec<f32>,
}

impl Recording {
    pub(crate) fn channel(&self, channel: usize) -> Vec<f32> {
        self.samples
            .iter()
            .skip(channel)
            .step_by(usize::from(self.channels))
            .copied()
            .collect()
    }
}

/// Reads a RIFF/WAVE file of 32-bit IEEE float samples, as `pw-record --format f32` writes it.
pub(crate) fn read_wav(bytes: &[u8]) -> Result<Recording> {
    ensure!(
        bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "the recording is not a RIFF/WAVE file"
    );
    let mut format = None;
    let mut data = None;
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = &bytes[at + 8..(at + 8 + size).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let tag = u16::from_le_bytes([body[0], body[1]]);
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                format = Some((tag, channels, rate, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    let (tag, channels, rate, bits) = format.context("the recording has no fmt chunk")?;
    ensure!(
        tag == 3 && bits == 32,
        "the recording is format {tag} at {bits} bits, want 32-bit float"
    );
    ensure!(channels > 0, "the recording has no channels");
    let body = data.context("the recording has no data chunk")?;
    let samples = body
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    Ok(Recording {
        rate,
        channels,
        samples,
    })
}

/// First and last index whose magnitude reaches `threshold`; None for silence.
pub(crate) fn active_span(x: &[f32], threshold: f32) -> Option<(usize, usize)> {
    let first = x.iter().position(|v| v.abs() >= threshold)?;
    let last = x.iter().rposition(|v| v.abs() >= threshold)?;
    Some((first, last))
}

/// Frequency from rising zero crossings with linear interpolation: (crossings - 1) periods over their span, in Hz.
pub(crate) fn frequency_hz(x: &[f32], rate: f64) -> Option<f64> {
    let mut first = None;
    let mut last = 0.0;
    let mut count = 0usize;
    for (i, pair) in x.windows(2).enumerate() {
        let (a, b) = (f64::from(pair[0]), f64::from(pair[1]));
        if a < 0.0 && b >= 0.0 {
            let t = i as f64 + (-a) / (b - a);
            first.get_or_insert(t);
            last = t;
            count += 1;
        }
    }
    let first = first?;
    (count >= 2).then(|| (count - 1) as f64 * rate / (last - first))
}

pub(crate) fn rms(x: &[f32]) -> f64 {
    let sum: f64 = x.iter().map(|v| f64::from(*v).powi(2)).sum();
    (sum / x.len() as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine of `freq` Hz, `amp` peak, `frames` frames at `rate`: the generator the gate's expectations are checked against.
    fn tone(rate: f64, freq: f64, amp: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|k| (amp * (std::f64::consts::TAU * freq * k as f64 / rate).sin()) as f32)
            .collect()
    }

    #[test]
    fn frequency_of_a_known_tone() {
        let x = tone(48000.0, 1000.0, 0.25, 48000);
        let hz = frequency_hz(&x, 48000.0).expect("a second of tone crosses zero many times");
        assert!((hz - 1000.0).abs() < 0.01, "{hz}");
    }

    #[test]
    fn rms_of_a_sine_is_peak_over_root_two() {
        let amp = 0.25;
        let x = tone(48000.0, 1000.0, amp, 48000);
        let want = amp / std::f64::consts::SQRT_2;
        assert!((rms(&x) - want).abs() < 1e-4, "{} vs {want}", rms(&x));
    }

    #[test]
    fn silence_has_no_span_and_no_frequency() {
        let x = vec![0.0f32; 4800];
        assert_eq!(active_span(&x, 1e-4), None);
        assert_eq!(frequency_hz(&x, 48000.0), None);
    }

    #[test]
    fn wav_reader_takes_float_samples_back() {
        let samples = [0.5f32, -0.25, 0.125, 0.0];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(4 + 24 + 8 + 16u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&48000u32.to_le_bytes());
        bytes.extend_from_slice(&(48000u32 * 8).to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let wav = read_wav(&bytes).expect("a well-formed float WAV reads");
        assert_eq!((wav.rate, wav.channels), (48000, 2));
        assert_eq!(wav.samples, samples);
        assert_eq!(wav.channel(1), vec![-0.25, 0.0]);
    }
}
