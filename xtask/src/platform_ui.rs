// SPDX-License-Identifier: MIT
//! `xtask test platform-ui` (M3 slice 6): the GTK platform layer on a real Wayland compositor (the gate's headless labwc, nested_wayland.rs; with --hold-blocked the desktop's $WAYLAND_DISPLAY), through the `atl-platform-ui` facade.
//! The client is `crates/atl-platform-ui/examples/gate.rs`; this side builds and starts it, reads its stdout and captures, and checks them against its own implementation of the spec below. Gates 1-4 run in any compositor, gate 5 only in the gate's labwc. Gates:
//! 1. **maps + frames**: the window maps (`is_mapped` at the first frame-clock tick) and `--frames` ticks of the window's frame clock each present one scene; ticks are strictly increasing in time. The presentation samples (`PRESENT`: the compositor's feedback for each presented frame, which the composer's clock grid is put on) are monotonic, on CLOCK_MONOTONIC, and have the monitor's period +-2%; no sample at all fails these.
//! 2. **readback**: after each present the client renders the window content with GSK (`Window::capture`); every pixel must equal the frame's pattern, R = (x + 7f) mod 256, G = (3y + f) mod 256, B = ((x xor y) + 31f) mod 256, A = 255, computed here from that formula, not by the code under test. The buffers have padded rows and alternate the opaque (`X8`) texture path.
//!    A composite scene (crop + FLIP_H, ROT_90 clockwise, scissor, alpha 0.5, an opaque and a half-transparent colour layer) is compared with a CPU composition worked out here from the HAL transform definitions; alpha pixels within `ALPHA_TOLERANCE`, the rest exact.
//! 3. **refusals + release + loop**: a handle with a short pitch or an unknown fourcc is refused; a replaced buffer goes back at the paint after its replacement, with a read fence, and nothing before that paint; GTK's log shows every buffer imported as a dmabuf texture and none downloaded (`gdk_dmabuf`); a watched fd's callback ran on the UI thread inside the Looper's poll.
//! 4. **ping/pong**: the client runs behind a Wayland proxy (`wayland_proxy.rs`) that injects `xdg_wm_base.ping` into the real connection. Probe proven responsive: an idle UI thread answers within `RESPONSIVE_LIMIT`, and again after the block. Then the UI thread is blocked (`block`) for the ping timeout plus a margin: no pong may arrive before the timeout, and the late pong must arrive once the thread runs again.
//!    Evidence tier: isolated runtime, a raw-protocol probe on the real connection. It shows the client side (one UI thread, no GTK thread, answers pings only from its Looper poll); the compositor's reaction to a missing pong (the native "not responding" dialog) is not observable on WSLg and stays [argued] until the Arch-laptop run: start `xtask test platform-ui --hold-blocked` there with mutter and watch for the dialog.
//! 5. **text input**: a fake input method (`nested_wayland::Ime`, input-method v2 on labwc's seat) is bound, then the client focuses its window's `TextInput` with a content type and surrounding text. Checks: the IME is activated; it receives the content type and surrounding text as text-input v3 numbers them; its preedit, commit and delete_surrounding_text (protocol units: UTF-8 bytes) reach the client as `TextInputEvent`s (GTK units: chars). Expected values are worked out from the encodings in the constants below. `--break-ime-commit` skips the commit_string, so the commit check must fail.
//!    Evidence tier: isolated runtime, real compositor relay (wlroots) and GTK's own Wayland IM module; a desktop IME (fcitx5) and the candidate popup at the cursor rect stay laptop checks.
//!
//! The client runs with GSK_RENDERER=ngl: it imports the gralloc dma-bufs (vgem on WSL) through EGL; its own window buffers still reach the compositor through wl_shm (Mesa's software EGL on a display with no zwp_linux_dmabuf), which the proxy confirms.
//! Needs a DRM node gralloc allocates on, `labwc` (or, with --hold-blocked, a Wayland display: WAYLAND_DISPLAY, XDG_RUNTIME_DIR) and GTK 4.14+.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::gdk_dmabuf;
use crate::nested_wayland::{Arg, GATE_OUTPUT, Ime, NestedCompositor, Output, im_op};
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};
use crate::wayland_proxy::{Event, Proxy};

#[derive(clap::Args)]
pub struct Args {
    /// Frames the client presents, one per frame-clock tick.
    #[arg(long, default_value_t = 8)]
    frames: u32,
    /// GSK renderer the client runs with (GSK_RENDERER). `ngl` imports the dma-bufs through EGL; `cairo` would download them through mmap, which the dmabuf gate fails.
    #[arg(long, default_value = "ngl")]
    gsk_renderer: String,
    /// How long a compositor waits for a pong before it calls a client unresponsive, in ms (mutter's check-alive timeout: 5 s).
    #[arg(long, default_value_t = 5000)]
    ping_timeout_ms: u64,
    /// Keep the client blocked this many extra seconds after the probe, so a person can watch the compositor's own "not responding" reaction (the Arch-laptop check).
    #[arg(long, conflicts_with = "maximized")]
    hold_blocked: Option<u64>,
    /// The gate's labwc maximizes the window when it maps (a binding configure size before READY, as a tiling compositor gives): the content must follow the granted size, set_size must not be waited for.
    #[arg(long)]
    maximized: bool,
    /// The gate output's scale (fractional ones reach the client as `wp_fractional_scale_v1`): the content must still be the requested physical size (readback size checks).
    /// Default fractional: the window stages run at 1 (GATE_OUTPUT), and a 1.6 output once gave 293 x 192 content for 320 x 240 (logical size fixed at GTK's integer scale 2 before the fractional one arrived, 2026-11-06).
    #[arg(long, default_value_t = 1.6)]
    scale: f64,
    /// Keep the captures in this directory instead of a temporary one.
    #[arg(long)]
    keep: Option<PathBuf>,
    /// Print the client's stderr and every proxy event.
    #[arg(long, short)]
    verbose: bool,
    /// The fake input method skips its commit_string (deliberate break): the ime commit check must fail.
    #[arg(long)]
    break_ime_commit: bool,
}

/// The ime gate's texts: multi-byte UTF-8, so a byte/char mix-up shows. Surrounding "xかy" with the cursor after か: x is 1 byte, か 3 (U+304B, UTF-8 E3 81 8B), so the cursor is at byte 4.
const IME_SURROUNDING: &str = "xかy";
const IME_CURSOR_BYTE: u32 = 4;
const IME_PREEDIT: &str = "かな";
/// か and な are 3 UTF-8 bytes each: the IME puts its cursor at the preedit's end, byte 6; GTK reports it in chars, 2.
const IME_PREEDIT_END_BYTE: i32 = 6;
const IME_PREEDIT_CURSOR_CHARS: usize = 2;
const IME_COMMIT: &str = "仮名";
/// The IME deletes か before the cursor: 3 bytes (protocol unit); GTK reports chars from the cursor: offset -1, 1 char.
const IME_DELETE_BEFORE_BYTES: u32 = 3;
/// What gate.rs declares (Email, LOWERCASE) as text-input-unstable-v3.xml numbers them: content_purpose `email` = 6, content_hint `lowercase` = 0x8.
const IME_WIRE_PURPOSE: u32 = 6;
const IME_WIRE_HINT: u32 = 0x8;
/// How long one ime check waits: GTK answers within a main-loop pass; this bounds a hang.
const IME_WAIT: Duration = Duration::from_secs(10);

/// Gate 5: binds a fake input method on the gate's labwc, has the client focus its `TextInput`, and checks both directions (spec in the module doc).
fn ime_gate(
    client: &mut Client,
    nested: &NestedCompositor,
    report: &mut Report,
    break_commit: bool,
) -> Result<()> {
    let mut ime = Ime::bind(nested)?;
    client.command(&format!("ime-focus {IME_SURROUNDING} {IME_CURSOR_BYTE}"))?;
    client.expect_line("IME-FOCUSED", IME_WAIT)?;
    let deadline = Instant::now() + IME_WAIT;
    while !(ime.active && ime.dones > 0) && Instant::now() < deadline {
        ime.pump()?;
        thread::sleep(Duration::from_millis(20));
    }
    report.check(
        ime.active,
        "ime: focus_in activates the desktop input method",
        &format!("done #{}", ime.dones),
    );
    if !ime.active {
        return Ok(());
    }
    ime.settle()?;
    report.check(
        ime.content_type == Some((IME_WIRE_HINT, IME_WIRE_PURPOSE)),
        "ime: set_content reaches the input method as content_type",
        &format!("(hint, purpose) {:?}", ime.content_type),
    );
    let want = (IME_SURROUNDING.to_owned(), IME_CURSOR_BYTE, IME_CURSOR_BYTE);
    report.check(
        ime.surrounding.as_ref() == Some(&want),
        "ime: set_surrounding reaches the input method as surrounding_text",
        &format!("{:?}", ime.surrounding),
    );

    ime.send(
        im_op::SET_PREEDIT_STRING,
        &[
            Arg::S(IME_PREEDIT),
            Arg::I(IME_PREEDIT_END_BYTE),
            Arg::I(IME_PREEDIT_END_BYTE),
        ],
    );
    ime.commit()?;
    let want = format!("IME-PREEDIT cursor={IME_PREEDIT_CURSOR_CHARS} text={IME_PREEDIT}");
    let got = wait_ime_line(client, &mut ime, "IME-PREEDIT ")?;
    report.check(
        got.as_deref() == Some(want.as_str()),
        "ime: preedit arrives with its cursor in chars",
        &format!("{got:?}"),
    );

    ime.settle()?;
    if !break_commit {
        ime.send(im_op::COMMIT_STRING, &[Arg::S(IME_COMMIT)]);
    }
    ime.send(
        im_op::SET_PREEDIT_STRING,
        &[Arg::S(""), Arg::I(0), Arg::I(0)],
    );
    ime.commit()?;
    let got = wait_ime_line(client, &mut ime, "IME-COMMIT ")?;
    report.check(
        got.as_deref() == Some(format!("IME-COMMIT text={IME_COMMIT}").as_str()),
        "ime: commit arrives",
        &format!("{got:?}"),
    );

    ime.settle()?;
    ime.send(
        im_op::DELETE_SURROUNDING_TEXT,
        &[Arg::U(IME_DELETE_BEFORE_BYTES), Arg::U(0)],
    );
    ime.commit()?;
    let got = wait_ime_line(client, &mut ime, "IME-DELETE ")?;
    report.check(
        got.as_deref() == Some("IME-DELETE offset=-1 n=1"),
        "ime: delete_surrounding_text arrives in chars from the cursor",
        &format!("{got:?}"),
    );
    Ok(())
}

/// The next client line with `prefix` within [`IME_WAIT`], pumping the IME meanwhile; `None` on timeout.
fn wait_ime_line(client: &mut Client, ime: &mut Ime, prefix: &str) -> Result<Option<String>> {
    let deadline = Instant::now() + IME_WAIT;
    while Instant::now() < deadline {
        if let Ok(line) = client.expect_line(prefix, Duration::from_millis(20)) {
            return Ok(Some(line));
        }
        ime.pump()?;
    }
    Ok(None)
}

/// The window content size asked for, physical pixels. Wide enough that the title bar's minimum width does not enlarge the content (WSLg: ~280 px), and at least the composite scene's 256 x 192.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
/// GTK's integer scale on a fractional output (wl_output scale 2 for 1.6): the scale the gate's window maps at, before the target one arrives (`late` in `run`).
const START_SCALE: f64 = 2.0;
/// Scales compare within this: the gate's scales are read back as text (Rust's float Display: 1.6 -> "1.6").
const SCALE_TOLERANCE: f64 = 1e-3;
/// The resize probe's size step, physical pixels per axis: the larger and the smaller size are WIDTH/HEIGHT +- this. The smaller content stays above the title bar's minimum width (WSLg: ~280 px).
const RESIZE_STEP: u32 = 32;
/// The presentation samples' period against the monitor's rate: the sample comes from the compositor's wp_presentation refresh (ns), the rate from GDK's milli-Hz.
const REFRESH_TOLERANCE: f64 = 0.02;
/// How long after its frame a presentation sample may be reported: the report waits for the window's next paint, so a quiet window delays it. 10 s is far above that and far below a unit or clock-base mismatch.
const PRESENT_LAG_LIMIT_NS: i64 = 10_000_000_000;
/// How long the client may take to reach READY (build excluded): mapping, the frames, the fd test.
const STARTUP_LIMIT: Duration = Duration::from_secs(60);
/// How long an idle UI thread may take to answer a ping. The client answers within one main-loop pass (20 ms); real compositors wait seconds, so 1 s is a generous bound for a loaded machine.
const RESPONSIVE_LIMIT: Duration = Duration::from_secs(1);
/// Extra time the UI thread stays blocked beyond the ping timeout, so a pong "after the timeout" is distinguishable from one at it.
const BLOCK_MARGIN: Duration = Duration::from_secs(1);
/// First serial of the probe's pings. Real compositors count serials up from small numbers, so a pong with one of these is ours.
const PROBE_SERIAL_BASE: u32 = 0xA7C0_0000;
/// Per-channel tolerance where a layer is blended at alpha 0.5: GSK blends in floats and rounds, so +-2 of 255 covers it; everything else is exact.
const ALPHA_TOLERANCE: i32 = 2;
/// How long to wait for the client to exit after `quit`.
const EXIT_LIMIT: Duration = Duration::from_secs(10);

/// The frame series' pattern (gate 2): pixel (`x`, `y`) of frame `f`.
fn pattern(x: u32, y: u32, f: u32) -> [u8; 4] {
    [
        ((x + 7 * f) % 256) as u8,
        ((3 * y + f) % 256) as u8,
        (((x ^ y) + 31 * f) % 256) as u8,
        255,
    ]
}

/// Frame `f` as the window shows it: the pattern over the whole content area.
fn expected_frame(f: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            out.extend_from_slice(&pattern(x, y, f));
        }
    }
    out
}

/// A pixel region of the composite scene: layer 0 is frame 100 everywhere; later layers replace or blend as listed in gate.rs `composite_scene`.
/// Returns (expected pixel, blended with tolerance).
fn composite_pixel(x: u32, y: u32) -> ([u8; 4], bool) {
    let under = pattern(x, y, 100);
    // layer 1: 32 x 24 at (20,30), the crop (8,4)-(40,28) of frame 101 mirrored left-right
    if (20..52).contains(&x) && (30..54).contains(&y) {
        let (u, v) = (x - 20, y - 30);
        return (pattern(8 + (31 - u), 4 + v, 101), false);
    }
    // layer 2: 24 x 40 at (100,10), a 40 x 24 buffer (frame 102) turned 90 degrees clockwise: destination (u, v) shows source (v, 23 - u)
    if (100..124).contains(&x) && (10..50).contains(&y) {
        let (u, v) = (x - 100, y - 10);
        return (pattern(v, 23 - u, 102), false);
    }
    // layer 3: frame 103 at (150,60), visible only inside the scissor (160,70)-(190,85)
    if (160..190).contains(&x) && (70..85).contains(&y) {
        return (pattern(x - 150, y - 60, 103), false);
    }
    // layer 4: frame 104 at (30,100), 40 x 40, alpha 0.5 over layer 0
    if (30..70).contains(&x) && (100..140).contains(&y) {
        let over = pattern(x - 30, y - 100, 104);
        let blend = |o: u8, u: u8| ((u16::from(o) + u16::from(u)) / 2) as u8;
        return (
            [
                blend(over[0], under[0]),
                blend(over[1], under[1]),
                blend(over[2], under[2]),
                255,
            ],
            true,
        );
    }
    // layer 5: colour (0.2, 0.4, 0.6) at (200,100)-(240,140), opaque: k/255 components, so exact bytes
    if (200..240).contains(&x) && (100..140).contains(&y) {
        return ([51, 102, 153, 255], false);
    }
    // layer 6: colour (1, 0, 0) at (80,150)-(120,180), alpha 0.5 over layer 0
    if (80..120).contains(&x) && (150..180).contains(&y) {
        let half = |o: u8, u: u8| ((u16::from(o) + u16::from(u)) / 2) as u8;
        return (
            [
                half(255, under[0]),
                half(0, under[1]),
                half(0, under[2]),
                255,
            ],
            true,
        );
    }
    (under, false)
}

/// Compares `got` with `expected(x, y) -> (pixel, tolerant)`; the mismatch count and the first one.
fn compare(got: &[u8], expected: impl Fn(u32, u32) -> ([u8; 4], bool)) -> (usize, Option<String>) {
    let mut bad = 0;
    let mut first = None;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let i = ((y * WIDTH + x) * 4) as usize;
            let (want, tolerant) = expected(x, y);
            let have = &got[i..i + 4];
            let tolerance = if tolerant { ALPHA_TOLERANCE } else { 0 };
            if (0..4).any(|c| (i32::from(have[c]) - i32::from(want[c])).abs() > tolerance) {
                bad += 1;
                first.get_or_insert_with(|| format!("({x},{y}) got {have:?} want {want:?}"));
            }
        }
    }
    (bad, first)
}

/// The client process; killed on drop so a failed gate never leaves a window behind.
struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    /// The next stdout line matching `prefix`, taken from lines already read first.
    fn expect_line(&mut self, prefix: &str, limit: Duration) -> Result<String> {
        if let Some(i) = self.seen.iter().position(|l| l.starts_with(prefix)) {
            return Ok(self.seen.remove(i));
        }
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.starts_with(prefix) => return Ok(line),
                Ok(line) => self.seen.push(line),
                Err(RecvTimeoutError::Timeout) => {
                    bail!(
                        "no `{prefix}` line from the client within {limit:?}; saw {:?}",
                        self.seen
                    )
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("the client exited before `{prefix}`; saw {:?}", self.seen)
                }
            }
        }
    }

    fn lines_with(&self, prefix: &str) -> Vec<&String> {
        self.seen.iter().filter(|l| l.starts_with(prefix)).collect()
    }

    fn command(&mut self, text: &str) -> Result<()> {
        writeln!(self.stdin, "{text}")?;
        self.stdin.flush()?;
        Ok(())
    }
}

/// The proxy's events, kept so a wait finds events that arrived during an earlier one.
struct Probe {
    proxy: Proxy,
    log: Vec<(Instant, Event)>,
    verbose: bool,
}

impl Probe {
    fn drain(&mut self, wait: Duration) {
        if let Ok(item) = self.proxy.events.recv_timeout(wait) {
            if self.verbose {
                eprintln!("proxy: {:?}", item.1);
            }
            self.log.push(item);
        }
        while let Ok(item) = self.proxy.events.try_recv() {
            self.log.push(item);
        }
    }

    fn find(&self, pred: impl Fn(&Event) -> bool) -> Option<Instant> {
        self.log.iter().find(|(_, e)| pred(e)).map(|(at, _)| *at)
    }

    /// When the client's pong for `serial` arrived, waiting up to `until`.
    fn pong(&mut self, serial: u32, until: Instant) -> Option<Instant> {
        loop {
            if let Some(at) = self.find(|e| *e == Event::Pong { serial }) {
                return Some(at);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.drain(left.min(Duration::from_millis(50)));
        }
    }

    fn ping(&mut self, serial: u32) -> Result<Instant> {
        self.proxy.inject_ping(serial)?;
        Ok(Instant::now())
    }
}

/// Build `gate` and return its path.
fn build_client(root: &Path) -> Result<PathBuf> {
    let mut cmd = crate::cargo_command();
    cmd.current_dir(root).args([
        "build",
        "--locked",
        "-p",
        "atl-platform-ui",
        "--example",
        "gate",
    ]);
    let run = unit_use::run(&mut cmd, Units::Recorded, Echo::Both)?;
    ensure!(
        run.status.success(),
        "cargo build of the gate client failed"
    );
    run.messages
        .iter()
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "gate")
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .context("cargo reported no gate executable")
}

/// The desktop's compositor socket, from WAYLAND_DISPLAY (absolute, or under XDG_RUNTIME_DIR).
fn desktop_socket() -> Result<PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY").context("WAYLAND_DISPLAY is not set")?;
    let display = PathBuf::from(display);
    if display.is_absolute() {
        return Ok(display);
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(runtime).join(display))
}

/// The tally of checks and the failing ones: the verdict names and quotes them; the exit status is nonzero if any failed.
#[derive(Default)]
struct Report {
    total: usize,
    failures: Vec<Failure>,
}

impl Report {
    fn check(&mut self, ok: bool, what: &str, detail: &str) {
        println!(
            "{} {what}{}",
            if ok { "pass" } else { "FAIL" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
        self.total += 1;
        if !ok {
            self.failures.push(Failure::step(what, detail));
        }
    }
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let exe = build_client(root)?;
    let out_dir = match &args.keep {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            std::path::absolute(dir)?
        }
        None => {
            let dir = std::env::temp_dir().join(format!("atl-platform-ui-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            dir
        }
    };
    // --hold-blocked is watched on the desktop's compositor; every other run maps in the gate's own (outlives the client: declared before it)
    // the gate's output starts at START_SCALE and takes args.scale once the window has mapped (the client's --await-scale): the fractional scale arrives after the first layout, as on Hyprland
    let late = args.hold_blocked.is_none() && !args.maximized;
    let output = Output {
        scale: if late { START_SCALE } else { args.scale },
        ..GATE_OUTPUT
    };
    let nested = match args.hold_blocked {
        Some(_) => None,
        None if args.maximized => Some(NestedCompositor::start_maximizing(
            &out_dir.join("nested"),
            output,
        )?),
        None => Some(NestedCompositor::start_with(
            &out_dir.join("nested"),
            &[output],
        )?),
    };
    let upstream = match &nested {
        Some(n) => n.socket.clone(),
        None => desktop_socket()?,
    };
    ensure!(
        upstream.exists(),
        "no Wayland socket at {}",
        upstream.display()
    );
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
    let proxy = Proxy::start(
        &upstream,
        &runtime.join(format!("atl-platform-ui-gate-{}.sock", std::process::id())),
    )?;
    let socket = proxy.listen.clone();
    let mut probe = Probe {
        proxy,
        log: Vec::new(),
        verbose: args.verbose,
    };

    let stderr_file = out_dir.join("client.stderr");
    let await_args: Vec<String> = if late {
        vec!["--await-scale".into(), args.scale.to_string()]
    } else {
        Vec::new()
    };
    let mut child = Command::new(&exe)
        .args(["--out"])
        .arg(&out_dir)
        .args(["--frames", &args.frames.to_string()])
        .args(["--width", &WIDTH.to_string(), "--height", &HEIGHT.to_string()])
        .args(["--resize-step", &RESIZE_STEP.to_string()])
        .args(["--timeout-secs", &STARTUP_LIMIT.as_secs().to_string()])
        .args(&await_args)
        .env("WAYLAND_DISPLAY", &socket)
        .env("GDK_BACKEND", "wayland")
        .env("GSK_RENDERER", &args.gsk_renderer)
        .env("GDK_DEBUG", gdk_dmabuf::GDK_DEBUG)
        // the sandbox has no accessibility bus; GTK warns about it on every start
        .env("GTK_A11Y", "none")
        // GTK picks its IM module from the display (Wayland: text-input v3, which reaches gate 5's IME); a desktop GTK_IM_MODULE (ibus) would bypass it
        .env_remove("GTK_IM_MODULE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // always a file: the dmabuf gate reads GTK's debug lines from it (--verbose prints it at the end)
        .stderr(Stdio::from(std::fs::File::create(&stderr_file)?))
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    let stdout = child.stdout.take().context("client stdout")?;
    let stdin = child.stdin.take().context("client stdin")?;
    let (tx, lines) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut client = Client {
        child,
        stdin,
        lines,
        seen: Vec::new(),
    };

    let mut report = Report::default();
    if let (true, Some(n)) = (late, &nested) {
        // mapped at START_SCALE; the output takes the target scale now, which the client's frames wait for
        let mapped_line = client.expect_line("MAPPED", STARTUP_LIMIT)?;
        client.seen.push(mapped_line);
        n.control().set_scale(0, args.scale)?;
    }
    let ready = client.expect_line("READY", STARTUP_LIMIT);
    if let Err(e) = &ready {
        let tail = std::fs::read_to_string(&stderr_file).unwrap_or_default();
        bail!("client did not reach READY: {e:#}\nclient stderr:\n{tail}");
    }
    probe.drain(Duration::from_millis(1));

    // gate 1: mapped, frames
    let mapped = client
        .lines_with("MAPPED")
        .first()
        .map(|l| l.to_string())
        .unwrap_or_default();
    report.check(
        field(&mapped, "mapped") == Some("true"),
        "window mapped at its first frame-clock tick",
        &mapped,
    );
    if late {
        let scale_of = |line: &str| field(line, "scale").and_then(|s| s.parse::<f64>().ok());
        let mapped_scale = scale_of(&mapped);
        let last_frame_scale = client
            .lines_with("FRAME ")
            .last()
            .and_then(|l| scale_of(l.as_str()));
        report.check(
            mapped_scale.is_some_and(|s| (s - START_SCALE).abs() < SCALE_TOLERANCE)
                && last_frame_scale.is_some_and(|s| (s - args.scale).abs() < SCALE_TOLERANCE),
            "the fractional scale arrives after the window mapped: mapped at the integer scale, the frames at the target",
            &format!("mapped at {mapped_scale:?}, last frame at {last_frame_scale:?}"),
        );
    }
    let frames: Vec<String> = client.lines_with("FRAME ").into_iter().cloned().collect();
    let times: Vec<u64> = frames
        .iter()
        .filter_map(|l| field(l, "time_ns")?.parse().ok())
        .collect();
    report.check(
        frames.len() == args.frames as usize && times.windows(2).all(|w| w[0] < w[1]),
        "frame-clock ticks: one frame per tick, times strictly increasing",
        &format!("{} frames, {}", frames.len(), tick_summary(&times)),
    );
    report.check(
        probe
            .find(|e| *e == Event::Bound("wl_shm".into()))
            .is_some()
            && probe.find(|e| *e == Event::ShmPool).is_some(),
        "presented through wl_shm (pool created)",
        &format!("renderer {}", args.gsk_renderer),
    );
    report.check(
        probe
            .find(|e| matches!(e, Event::Bound(n) if n.starts_with("zwp_linux_dmabuf")))
            .is_none(),
        "no zwp_linux_dmabuf global bound (the window's own buffers go through wl_shm; scene dma-bufs are imported client-side)",
        "",
    );
    // the content follows the window: the larger, then the smaller probe size is reported, the smaller last
    let sizes: Vec<(u32, u32)> = client
        .lines_with("RESIZED ")
        .into_iter()
        .filter_map(|l| {
            let (w, h) = l.strip_prefix("RESIZED ")?.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .collect();
    let size_of = |l: &str, key: &str| {
        let (w, h) = l.strip_prefix(key)?.split_once('x')?;
        Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
    };
    let fixed = client
        .lines_with("SIZE-FIXED ")
        .first()
        .and_then(|l| size_of(l, "SIZE-FIXED "));
    // each capture's size, from its FRAME / COMPOSITE line (`<file> WxH`, last word)
    let captured = |prefix: &str| -> Vec<Option<(u32, u32)>> {
        client
            .lines_with(prefix)
            .into_iter()
            .map(|l| size_of(l.rsplit(' ').next()?, ""))
            .collect()
    };
    let frame_sizes = captured("FRAME ");
    let composite_size = captured("COMPOSITE ").first().copied().flatten();
    if args.maximized {
        report.check(
            frame_sizes.last().copied().flatten() == fixed && composite_size == fixed,
            "maximized: the content follows the configure (last frame and composite at the fixed size)",
            &format!("frames {frame_sizes:?}, composite {composite_size:?}"),
        );
        let after: Vec<_> = client
            .lines_with("FIXED-AFTER ")
            .into_iter()
            .filter_map(|l| size_of(l, "FIXED-AFTER "))
            .collect();
        report.check(
            fixed.is_some_and(|g| g != (WIDTH, HEIGHT) && g.0 >= WIDTH && g.1 >= HEIGHT),
            "maximized: the window reports a fixed size, the configured one, larger than requested",
            &format!("fixed {fixed:?}"),
        );
        report.check(
            after.len() == 2 && after.iter().all(|s| Some(*s) == fixed),
            "maximized: size requests leave the content at the configured size",
            &format!("after requests {after:?}, resized {sizes:?}"),
        );
    } else {
        let larger = (WIDTH + RESIZE_STEP, HEIGHT + RESIZE_STEP);
        let smaller = (WIDTH - RESIZE_STEP, HEIGHT - RESIZE_STEP);
        let in_order = match (
            sizes.iter().position(|s| *s == larger),
            sizes.iter().position(|s| *s == smaller),
        ) {
            (Some(a), Some(b)) => a < b,
            _ => false,
        };
        report.check(
            fixed.is_none() && in_order && sizes.last() == Some(&smaller),
            "content follows the window: larger, then smaller probe size, the smaller last",
            &format!("fixed {fixed:?}, sizes {sizes:?}"),
        );
    }
    // gate 2: readback. The scene is WIDTH x HEIGHT; on a maximized window the capture is the granted size and the scene its top-left corner
    let scene_of = |got: Vec<u8>, size: Option<(u32, u32)>| -> Option<Vec<u8>> {
        let (w, h) = if args.maximized {
            size?
        } else {
            (WIDTH, HEIGHT)
        };
        (w >= WIDTH && h >= HEIGHT && got.len() == (w * h * 4) as usize).then(|| {
            got.chunks((w * 4) as usize)
                .take(HEIGHT as usize)
                .flat_map(|row| row[..(WIDTH * 4) as usize].iter().copied())
                .collect()
        })
    };
    for f in 0..args.frames {
        let path = out_dir.join(format!("frame-{f}.rgba"));
        let got = std::fs::read(&path).with_context(|| path.display().to_string())?;
        let len = got.len();
        let size = frame_sizes.get(f as usize).copied().flatten();
        let Some(got) = scene_of(got, size) else {
            report.check(
                false,
                &format!("frame {f} readback size"),
                &format!("{len} bytes ({size:?}), want {WIDTH}x{HEIGHT}x4"),
            );
            continue;
        };
        let want = expected_frame(f);
        let bad = got.iter().zip(&want).filter(|(a, b)| a != b).count();
        report.check(
            bad == 0,
            &format!("frame {f} readback equals the pattern"),
            &format!("{bad} differing bytes of {}", want.len()),
        );
    }
    let path = out_dir.join("composite.rgba");
    let got = std::fs::read(&path).with_context(|| path.display().to_string())?;
    let len = got.len();
    if let Some(got) = scene_of(got, composite_size) {
        let (bad, first) = compare(&got, composite_pixel);
        report.check(
            bad == 0,
            "composite readback (crop, FLIP_H, ROT_90, scissor, alpha, colour layers)",
            &format!("{bad} pixels differ {}", first.unwrap_or_default()),
        );
    } else {
        report.check(
            false,
            "composite readback size",
            &format!("{len} bytes ({composite_size:?})"),
        );
    }

    // gate 3: refusals, loop
    let refusals = client
        .lines_with("REFUSALS")
        .first()
        .map(|l| l.to_string())
        .unwrap_or_default();
    report.check(
        ["short-pitch", "bad-fourcc"]
            .iter()
            .all(|k| field(&refusals, k) == Some("true")),
        "a handle with a short pitch or an unknown fourcc is refused",
        &refusals,
    );
    // the client shows buffer RELEASE_FRAMES[0] (200), then replaces it with 201: exactly 200 goes back, at the paint after the replacement, with a read fence
    let releases = client
        .lines_with("RELEASES")
        .first()
        .map(|l| l.to_string())
        .unwrap_or_default();
    report.check(
        field(&releases, "before-paint") == Some("0")
            && field(&releases, "released") == Some("1")
            && field(&releases, "fenced") == Some("1")
            && field(&releases, "keys") == Some("200"),
        "the replaced buffer is released at the next paint, with a read fence, and only it",
        &releases,
    );
    let gtk_log = std::fs::read_to_string(&stderr_file).unwrap_or_default();
    if args.verbose {
        eprint!("{gtk_log}");
    }
    let dmabuf = gdk_dmabuf::Verdict::of(&gtk_log);
    report.check(
        dmabuf.ok(),
        "GTK imported every buffer as a dmabuf texture (GDK_DEBUG=dmabuf), never an mmap download",
        &dmabuf.to_string(),
    );
    let fdwatch = client
        .lines_with("FDWATCH")
        .first()
        .map(|l| l.to_string())
        .unwrap_or_default();
    report.check(
        field(&fdwatch, "on-ui-thread") == Some("true")
            && field(&fdwatch, "fired").is_some_and(|n| n != "0"),
        "watched fd callback ran on the UI thread inside the Looper poll",
        &fdwatch,
    );

    // gate 4: ping / pong
    let ping_timeout = Duration::from_millis(args.ping_timeout_ms);
    // the client stays blocked through the hold too: a block that ended during the harness's sleep left nothing for the compositor's ANR check to see (11-04 defect 1)
    let hold = Duration::from_secs(args.hold_blocked.unwrap_or(0));
    let block = ping_timeout + hold + BLOCK_MARGIN;
    let mut serial = PROBE_SERIAL_BASE;
    let mut next_serial = || {
        serial += 1;
        serial
    };
    let s1 = next_serial();
    let sent = probe.ping(s1)?;
    let answered = probe.pong(s1, sent + RESPONSIVE_LIMIT);
    report.check(
        answered.is_some(),
        "probe responds: an idle UI thread answers a ping",
        &answered.map_or("no pong".to_owned(), |at| {
            format!("pong after {:?}", at - sent)
        }),
    );

    client.command(&format!("block {}", block.as_millis()))?;
    client.expect_line("BLOCKING", RESPONSIVE_LIMIT * 5)?;
    let s2 = next_serial();
    let sent = probe.ping(s2)?;
    let early = probe.pong(s2, sent + ping_timeout);
    report.check(
        early.is_none(),
        "blocked UI thread: no pong within the ping timeout",
        &format!("waited {ping_timeout:?} after the ping"),
    );
    if !hold.is_zero() {
        println!("holding: the UI thread is blocked, watch the compositor for {hold:?} more");
        thread::sleep(hold);
    }
    client.expect_line("UNBLOCKED", block + RESPONSIVE_LIMIT * 5)?;
    let late = probe.pong(s2, Instant::now() + RESPONSIVE_LIMIT);
    report.check(
        late.is_some_and(|at| at - sent >= ping_timeout + hold),
        "the ping sent while blocked is answered once the UI thread runs again, after the timeout and the hold",
        &late.map_or("no pong".to_owned(), |at| {
            format!("pong after {:?} (block {block:?})", at - sent)
        }),
    );
    let s3 = next_serial();
    let sent = probe.ping(s3)?;
    let again = probe.pong(s3, sent + RESPONSIVE_LIMIT);
    report.check(
        again.is_some(),
        "UI thread responsive again after the block",
        "",
    );
    probe.drain(Duration::from_millis(1));
    let real: Vec<u32> = probe
        .log
        .iter()
        .filter_map(|(_, e)| match e {
            Event::Ping {
                serial,
                injected: false,
            } => Some(*serial),
            _ => None,
        })
        .collect();
    println!(
        "info: compositor-originated pings seen on the connection: {} ({})",
        real.len(),
        if real
            .iter()
            .all(|s| probe.find(|e| *e == Event::Pong { serial: *s }).is_some())
        {
            "all answered"
        } else {
            "some unanswered"
        }
    );

    // gate 5: the text input against the gate compositor's input method (the desktop's seat already has one with --hold-blocked)
    match &nested {
        Some(nested) => ime_gate(&mut client, nested, &mut report, args.break_ime_commit)?,
        None => println!("skip ime: --hold-blocked runs on the desktop's compositor"),
    }

    // shutdown
    client.command("quit")?;
    client.expect_line("DONE", EXIT_LIMIT)?;
    // presentation samples, one per presented frame the display showed (gate.rs), printed before DONE: in order, each period the monitor's rate. The composer's clock grid is put on them.
    // Zero samples fails every check here, the period one included: an empty set must not pass.
    let monitor_hz = client
        .lines_with("MONITOR ")
        .last()
        .and_then(|l| field(l, "refresh_hz")?.parse::<f32>().ok())
        .filter(|hz| *hz > 0.0);
    // (presentation time, period, monotonic time its report arrived), all ns
    let samples: Vec<(i64, i64, i64)> = client
        .lines_with("PRESENT ")
        .into_iter()
        .filter_map(|l| {
            let time = field(l, "time_ns")?.parse().ok()?;
            let period = field(l, "refresh_ns")?.parse().ok()?;
            let seen = field(l, "seen_ns")?.parse().ok()?;
            Some((time, period, seen))
        })
        .collect();
    report.check(
        !samples.is_empty(),
        "presentation samples come: the display reports each presented frame (wp_presentation feedback)",
        &format!(
            "{} samples{}",
            samples.len(),
            if samples.is_empty() { ": no samples" } else { "" }
        ),
    );
    report.check(
        samples.len() >= 2 && samples.windows(2).all(|w| w[0].0 < w[1].0),
        "presentation samples are monotonic",
        &format!("{} samples", samples.len()),
    );
    // the times are on the monotonic clock the composer's grid uses: each report arrives after its frame was shown, and within the bound (a unit or clock mismatch is far outside it)
    report.check(
        !samples.is_empty()
            && samples
                .iter()
                .all(|(time, _, seen)| (0..=PRESENT_LAG_LIMIT_NS).contains(&(seen - time))),
        "presentation times are on CLOCK_MONOTONIC: each report arrives at most 10 s after its frame was shown",
        &format!("{} samples", samples.len()),
    );
    let want_ns = monitor_hz.map(|hz| 1e9 / f64::from(hz));
    let periods = samples.iter().map(|s| s.1);
    report.check(
        !samples.is_empty()
            && want_ns.is_some_and(|want| {
                samples
                    .iter()
                    .all(|(_, period, _)| (*period as f64 - want).abs() <= want * REFRESH_TOLERANCE)
            }),
        "presentation sample periods equal the monitor rate (+-2%)",
        &format!(
            "monitor {monitor_hz:?} Hz, sample periods {:?}..{:?} ns",
            periods.clone().min(),
            periods.max()
        ),
    );
    let deadline = Instant::now() + EXIT_LIMIT;
    let status = loop {
        if let Some(s) = client.child.try_wait()? {
            break s;
        }
        ensure!(
            Instant::now() < deadline,
            "the client did not exit after quit"
        );
        thread::sleep(Duration::from_millis(50));
    };
    report.check(
        status.success(),
        "client exited cleanly",
        &format!("{status}"),
    );
    if args.keep.is_none() {
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    if !report.failures.is_empty() {
        let verdict = Verdict {
            cmd: "platform-ui",
            total: Some((report.total, "checks")),
            skipped: Vec::new(),
            failures: report.failures,
        };
        println!("{}", verdict.line());
        return Ok(ExitCode::FAILURE);
    }
    println!("platform-ui: all checks passed");
    Ok(ExitCode::SUCCESS)
}

fn tick_summary(times_ns: &[u64]) -> String {
    if times_ns.len() < 2 {
        return "fewer than two ticks".to_owned();
    }
    let span = times_ns[times_ns.len() - 1] - times_ns[0];
    format!(
        "mean interval {:.1} ms",
        span as f64 / (times_ns.len() - 1) as f64 / 1e6
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Worked by hand from the formula: pattern(3, 5, 2) = R 3 + 14 = 17, G 15 + 2 = 17, B (3 xor 5 = 6) + 62 = 68.
    #[test]
    fn pattern_by_hand() {
        assert_eq!(pattern(3, 5, 2), [17, 17, 68, 255]);
        // wraps mod 256: R = 250 + 7 * 3 = 271 -> 15
        assert_eq!(pattern(250, 0, 3)[0], 15);
    }

    /// The mirrored layer 1: its top-left destination pixel (20, 30) shows the crop's top-right source pixel (8 + 31, 4) = (39, 4) of frame 101.
    /// R = 39 + 707 = 746 -> 234; G = 12 + 101 = 113; B = (39 xor 4 = 35) + 3131 = 3166 -> 3166 - 12 * 256 = 94.
    #[test]
    fn composite_flip_corner_by_hand() {
        assert_eq!(composite_pixel(20, 30), ([234, 113, 94, 255], false));
    }

    /// The rotated layer 2: destination (u = 0, v = 0) of a clockwise turn shows the source's bottom-left pixel (0, 23) of frame 102 (clockwise: the bottom-left corner goes to the top-left).
    /// R = 0 + 714 -> 714 - 512 = 202; G = 69 + 102 = 171; B = (0 xor 23 = 23) + 3162 = 3185 -> 3185 - 12 * 256 = 113.
    #[test]
    fn composite_rotated_corner_by_hand() {
        assert_eq!(composite_pixel(100, 10), ([202, 171, 113, 255], false));
    }

    /// Outside every upper layer the composite is layer 0 (frame 100), and the scissor cuts layer 3: (155, 65) is inside its destination but outside the scissor.
    #[test]
    fn composite_background_and_scissor() {
        assert_eq!(composite_pixel(155, 65), (pattern(155, 65, 100), false));
        assert_eq!(composite_pixel(160, 70), (pattern(10, 10, 103), false));
    }

    #[test]
    fn compare_counts_and_tolerates() {
        let mut got = expected_frame(0);
        got[0] = got[0].wrapping_add(1);
        let (bad, first) = compare(&got, |x, y| (pattern(x, y, 0), false));
        assert_eq!(bad, 1);
        assert!(first.is_some_and(|s| s.starts_with("(0,0)")));
        let (bad, _) = compare(&got, |x, y| (pattern(x, y, 0), true));
        assert_eq!(bad, 0);
    }

    #[test]
    fn field_lookup() {
        assert_eq!(field("FRAME 3 time_ns=77 a", "time_ns"), Some("77"));
        assert_eq!(field("FRAME 3", "time_ns"), None);
    }
}
