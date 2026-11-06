// SPDX-License-Identifier: MIT
//! The client of `xtask test audio-pcm` (plan A0): a sine tone through the `atl-platform-ui` facade's PCM output to the server's default sink.
//! Prints one line per fact the harness reads (`spec`, `callbacks`, `quantum_frames`, `frames`, `rt_fallbacks`, `underruns`, `done`); a refused call prints `error <reason>` and exits 1.
//! The harness checks what reaches the sink against values derived from these arguments, not against this program.
#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use atl_platform_ui::PlatformAudioHost;
use atl_platform_ui::audio::{AudioHost, PcmOutput, PcmSpec};
use clap::{Parser, ValueEnum};

/// Deliberate faults the harness must see go red.
#[derive(Clone, Copy, ValueEnum)]
enum Break {
    /// `fill` writes silence instead of the tone.
    Zeros,
}

#[derive(Parser)]
struct Args {
    /// Frames per second requested.
    #[arg(long, default_value_t = 48000)]
    rate: u32,
    /// Interleaved channels requested; every channel carries the same tone.
    #[arg(long, default_value_t = 2)]
    channels: u16,
    /// Tone frequency in Hz.
    #[arg(long, default_value_t = 1000.0)]
    freq: f64,
    /// Peak amplitude of the tone in dBFS (amplitude = 10^(dbfs / 20)).
    #[arg(long, default_value_t = -12.0, allow_hyphen_values = true)]
    dbfs: f64,
    /// Tone length in milliseconds; the output runs `duration + 200 ms` of silence after it.
    #[arg(long)]
    duration_ms: u64,
    #[arg(long = "break", value_enum)]
    break_: Option<Break>,
}

/// Silence the output keeps after the tone, so the harness's capture sees where the tone ends.
const TAIL_MS: u64 = 200;

/// The tone generator. `fill` runs on the server's realtime thread: no allocation, no lock.
struct Tone {
    /// Next frame index; the tone is `amp * sin(2 pi freq frame / rate)` for `frame < total`.
    frame: u64,
    total: u64,
    rate: f64,
    freq: f64,
    amp: f64,
    channels: usize,
    silent: bool,
    callbacks: Arc<AtomicU64>,
    produced: Arc<AtomicU64>,
    /// Frames in the first call: the server's buffer size, one quantum.
    quantum: Arc<AtomicU64>,
}

impl Tone {
    fn fill(&mut self, samples: &mut [f32]) {
        if self.callbacks.load(Ordering::Relaxed) == 0 {
            self.quantum
                .store((samples.len() / self.channels) as u64, Ordering::Relaxed);
        }
        for frame in samples.chunks_exact_mut(self.channels) {
            let value = if self.silent || self.frame >= self.total {
                0.0
            } else {
                let phase = std::f64::consts::TAU * self.freq * self.frame as f64 / self.rate;
                (self.amp * phase.sin()) as f32
            };
            frame.fill(value);
            self.frame += 1;
        }
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.produced.store(self.frame, Ordering::Release);
    }
}

fn say(text: &str) {
    let mut out = std::io::stdout().lock();
    // a closed pipe ends the harness's read; nothing else to do with the error
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn run(args: &Args) -> Result<()> {
    if args.channels == 0 {
        bail!("channels must be at least 1");
    }
    let total = args.rate as u64 * args.duration_ms / 1000;
    let tail = args.rate as u64 * TAIL_MS / 1000;
    let callbacks = Arc::new(AtomicU64::new(0));
    let produced = Arc::new(AtomicU64::new(0));
    let quantum = Arc::new(AtomicU64::new(0));
    let mut tone = Tone {
        frame: 0,
        total,
        rate: f64::from(args.rate),
        freq: args.freq,
        amp: 10f64.powf(args.dbfs / 20.0),
        channels: usize::from(args.channels),
        silent: matches!(args.break_, Some(Break::Zeros)),
        callbacks: Arc::clone(&callbacks),
        produced: Arc::clone(&produced),
        quantum: Arc::clone(&quantum),
    };

    let host = PlatformAudioHost::connect()?;
    say("connected");
    let output = host.open_output(
        PcmSpec {
            rate_hz: args.rate,
            channels: args.channels,
        },
        move |samples| tone.fill(samples),
    )?;
    let spec = output.spec();
    say(&format!(
        "spec rate={} channels={}",
        spec.rate_hz, spec.channels
    ));
    output.set_active(true)?;
    // The server runs in real time, so the generator's position is the playback position. Wait for the tone and its tail.
    let wanted = total + tail;
    let deadline = Instant::now() + Duration::from_millis(args.duration_ms + TAIL_MS + 10_000);
    while produced.load(Ordering::Acquire) < wanted {
        if Instant::now() > deadline {
            bail!(
                "generator stalled at {} of {wanted} frames",
                produced.load(Ordering::Acquire)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    output.set_active(false)?;
    say(&format!("callbacks {}", callbacks.load(Ordering::Relaxed)));
    say(&format!(
        "quantum_frames {}",
        quantum.load(Ordering::Relaxed)
    ));
    say(&format!("frames {}", produced.load(Ordering::Acquire)));
    say(&format!("rt_fallbacks {}", output.rt_fallbacks()));
    say(&format!("underruns {}", output.underruns()));
    say("done");
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            say(&format!("error {e:#}"));
            ExitCode::FAILURE
        }
    }
}
