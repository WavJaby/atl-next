// SPDX-License-Identifier: MIT
//! `xtask test window`: the window path, one stage at a time. Stage `resume` (M3 plan step 1): the window fixture (xtask/testdata/window: a software activity painting four solid colours)
//! is started the Android way by atld, as `xtask test app` starts its fixture; the services send ResumeActivityItem after LaunchActivityItem (activity.rs), so ActivityThread runs onResume.
//! Pass: the stage's marker is logged, and the app neither logs FATAL EXCEPTION, nor dies, nor waits on a service nothing serves ([`UNSERVED_WAIT`]: a hang shows only as that line) within [`WINDOW_MS`] after it. The app's log lines and the services' counter lines carry the same
//! wall clock (logcat's first field; `at=` on a counter line, atl_services::counters), so every counted call is printed as before or after the marker, with its offset.
//! Always printed: the counters before the marker, the calls after it, the services' crash reports and the app's log after it; on failure also the atld, app and services log tails.
//! `--self-test` checks the verdict on synthetic logs: a crash or a death inside the window fails, one just past it does not, a missing marker and an atld exit fail.
//! Stage `frames` (M3 slice 5) runs the same verdict over its longer window, then two sub-gates on the fixture's frames: (a) the first frame reaches the compositor and its pixels are the
//! quadrants ([`picture_check`]); (b) the Choreographer callbacks come once per display period ([`cadence_check`]).
//! Stage `input` (M3 slice 5 sub-gate c): the resume verdict over the frames window, then the touches the fixture logs are the ones the injected [`POINTER_SCRIPT`] makes ([`input_check`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use atl_layout::Layout;
use atl_services::display_model::DisplayModel;

use crate::atld_run::{Counter, Event, MS_PER_S, Run, Runtime, events, parse_stamp, totals};
use crate::composer::{fnv1a64, word};
use crate::manifest;
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// The stage to run. `resume`: onResume is logged, and the app survives the window after it. `frames`: resume, and the first frame's pixels and the frame cadence are right. `input`: resume, and the fixture logs the injected pointer script's touches, in order.
    #[arg(long, value_enum, default_value = "resume")]
    stage: Stage,
    /// Instead of the stage: the verdict on synthetic logs (what the stage must fail and must pass). Needs no device.
    #[arg(long)]
    self_test: bool,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Stage {
    /// The launch is followed by the resume (ResumeActivityItem): onResume is logged.
    Resume,
    /// Resume, then the sub-gates on the first frame and the frame cadence.
    Frames,
    /// Resume, then the touches the injected pointer script makes are the ones the fixture logs.
    Input,
    /// Resume in UI mode (`ro.atl.ui=gtk`): the app's window is a GTK window of its process. Spike stage: the verdict is the resume one, the mirror's counters are in the app log.
    Gtk,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Resume => "resume",
            Stage::Frames => "frames",
            Stage::Input => "input",
            Stage::Gtk => "gtk",
        }
    }

    /// How long the app must survive after the marker: the resume window, or the longer watch of the frames and input stages.
    fn window_ms(self) -> u64 {
        match self {
            Stage::Resume => WINDOW_MS,
            Stage::Frames | Stage::Input => FRAMES_WINDOW_MS,
            Stage::Gtk => GTK_WINDOW_MS,
        }
    }

    /// The sub-gates run once the marker is logged, by name (each prints `window: <name>: pass|FAIL`).
    fn sub_names(self) -> &'static [&'static str] {
        match self {
            Stage::Resume => &[],
            Stage::Gtk => &["mapped", "readback", "touch", "pong"],
            Stage::Frames => &["picture", "cadence"],
            Stage::Input => &["touch"],
        }
    }
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/window";
/// Scenes the compositor must present into the window before its readback (the fixture's first frame and a repaint).
const GTK_PRESENTED: u32 = 2;
/// The test app's uid: AID_APP_START + 123, any app id (as `xtask test app`).
const APP_UID: u32 = 10_123;
/// How long a cold start may take to reach the marker (as `xtask test app`).
const LIMIT: Duration = Duration::from_secs(180);
/// How long the app must survive after the marker. The resume path (WindowManagerGlobal.addView, the window of the activity) runs right after onResume and fails within
/// milliseconds when it fails, so two seconds leaves the failure time to show.
const WINDOW_MS: u64 = 2_000;
/// The frames and input stages' watch after the marker: the first draw, then FRAME_CALLBACKS vsyncs (0.5 s at 60 Hz), with slack for a slow start; the input stage's touches come after the first draw, inside it.
const FRAMES_WINDOW_MS: u64 = 5_000;
/// The gtk stage's watch after the marker: the first draw, the pointer script, the probe's block and the pong after it.
const GTK_WINDOW_MS: u64 = 10_000;
/// The fixture's blocking touch: an UP past this view x sleeps the UI thread (Main.java BLOCK_MIN_X). The probe's click lands past it, under the window's decorations.
const BLOCK_MIN_X: f32 = 500.0;
const BTN_LEFT: u32 = 0x110;
/// The probe's waits: the window's first configure, a pong, silence while the UI thread sleeps (the fixture sleeps 2 s), the pong after it.
const ACK_WAIT: Duration = Duration::from_secs(60);
const PONG_WAIT: Duration = Duration::from_secs(2);
const SILENT_WAIT: Duration = Duration::from_millis(1000);
const RESUME_WAIT: Duration = Duration::from_secs(4);
/// The app's launcher binary (its command line names it); atld's apps are its descendants.
const APP_LAUNCHER: &str = "atl-app-launcher";
/// How long the apps get to exit after SIGTERM, before atld is killed.
const STOP_WAIT: Duration = Duration::from_secs(3);
/// Between the block's UP and the probe's ping, so the fixture is asleep when the ping is sent.
const BLOCK_SETTLE: Duration = Duration::from_millis(500);
/// The fixture's line once its view has drawn (Main.java Quadrants.onDraw), and the settle after it before the pointer script.
const DRAW_LINE: &str = "onDraw reached";
const INPUT_SETTLE: Duration = Duration::from_millis(500);
/// The fixture's frame callbacks (Main.java FRAME_CALLBACKS): FRAME_CALLBACKS - 1 intervals, each one display period.
const FRAME_CALLBACKS: usize = 31;
/// The layer name of the fixture's window (Main.java WINDOW_TITLE, set by `Window.setTitle`): the compositor's report names the layer by it.
const WINDOW_TITLE: &str = "AtlWindowFixture";
/// The mean interval may differ from the display period by this share: the callbacks run on the app's main thread, which the vsync timer does not wait for.
const CADENCE_TOLERANCE: f64 = 0.10;
/// No single interval may exceed this many periods: a missed vsync shows as two.
const CADENCE_MAX_PERIODS: f64 = 1.5;
/// The input stage's pointer script (atld `--headless-pointer`): a down at (200, 300), then a move and an up at (260, 340), one pointer, identity transform.
const POINTER_SCRIPT: &str = "down:200,300 move:260,340 up:260,340";
/// MotionEvent.ACTION_DOWN, ACTION_UP and ACTION_MOVE (AOSP MotionEvent.java): the action of each touch the fixture logs.
const ACTION_DOWN: u32 = 0;
const ACTION_UP: u32 = 1;
const ACTION_MOVE: u32 = 2;
/// The pointer count of each touch: the script has one pointer.
const POINTERS: u32 = 1;
/// How often the logs are read while waiting.
const POLL: Duration = Duration::from_millis(50);
/// Lines shown of a log on failure.
const TAIL: usize = 40;

/// What the stage waits for: the fixture's Log.i line (tag `AtlWindow`, Main.java), as the launcher's logger writes it (`<tag>: <message>`).
fn marker(stage: Stage) -> &'static str {
    match stage {
        Stage::Resume | Stage::Frames | Stage::Input | Stage::Gtk => "AtlWindow: onResume reached",
    }
}

/// The wall clock, ms since the epoch: the clock of the logcat stamps and of the services' `at=` stamps.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `secs.mmm` for `ms` since the epoch: the form of a stamp (parse_stamp reads it back).
fn stamp_text(ms: u64) -> String {
    format!("{}.{:03}", ms / MS_PER_S, ms % MS_PER_S)
}

/// A logcat line's time: its first field.
fn line_stamp(line: &str) -> Option<u64> {
    parse_stamp(line.split_whitespace().next()?)
}

/// onResume's time: the first stamped app-log line that contains the marker.
fn marker_stamp(app_log: &str, marker: &str) -> Option<u64> {
    app_log
        .lines()
        .filter(|l| l.contains(marker))
        .find_map(line_stamp)
}

/// The stage's verdict: `marker_ms` is when onResume was logged (None: never); `fault` is the first thing inside the window after it that fails the stage (None: pass).
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub marker_ms: Option<u64>,
    pub fault: Option<String>,
}

/// libbinder's line when a service nothing serves holds a call up (IServiceManager.cpp:727, once a second): the app is hung, not crashed, so only this line tells.
const UNSERVED_WAIT: &str = "Waited one second for";

/// The verdict on an app log. Inside [marker, marker + window_ms]: a FATAL EXCEPTION, the process killed (`Sending signal`, Process.killProcess after a crash), a wait on an unserved service ([`UNSERVED_WAIT`]),
/// and `atld_exited` (the services went away before the window ended). A line outside the window, or without a stamp, does not count.
pub fn verdict(app_log: &str, marker: &str, atld_exited: bool, window_ms: u64) -> Verdict {
    let Some(marker_ms) = marker_stamp(app_log, marker) else {
        return Verdict {
            marker_ms: None,
            fault: None,
        };
    };
    let end = marker_ms + window_ms;
    let mut faults: Vec<(u64, String)> = app_log
        .lines()
        .filter_map(|l| Some((line_stamp(l)?, l)))
        .filter(|(t, _)| (marker_ms..=end).contains(t))
        .filter_map(|(t, l)| {
            let what = if l.contains("FATAL EXCEPTION") {
                "FATAL EXCEPTION"
            } else if l.contains("Sending signal") {
                "the process was killed"
            } else if l.contains(UNSERVED_WAIT) {
                "the app waits on a service nothing serves"
            } else {
                return None;
            };
            Some((t, format!("{what} at +{} ms after onResume", t - marker_ms)))
        })
        .collect();
    if atld_exited {
        faults.push((end, "atld exited inside the window".to_owned()));
    }
    faults.sort_by_key(|(t, _)| *t);
    Verdict {
        marker_ms: Some(marker_ms),
        fault: faults.into_iter().next().map(|(_, f)| f),
    }
}

/// One synthetic app log and what the verdict must say about it.
struct SelfCase {
    name: &'static str,
    log: String,
    atld_exited: bool,
    marked: bool,
    fault: bool,
}

/// A logcat line of the app at `at_ms`, with the fields logcat puts between the stamp and the tag.
fn app_line(at_ms: u64, tag: &str, text: &str) -> String {
    format!("{} 1 2 I {tag}: {text}\n", stamp_text(at_ms))
}

/// The self-test's cases. `T` is onResume's time, so the window is [T, T + WINDOW_MS].
fn self_cases() -> Vec<SelfCase> {
    const T: u64 = 1_000_000;
    let mark = app_line(T, "liblog/AtlWindow", "onResume reached");
    let crash_at = |at: u64| app_line(at, "liblog/AndroidRuntime", "FATAL EXCEPTION: main");
    let case =
        |name: &'static str, log: String, atld_exited: bool, marked: bool, fault: bool| SelfCase {
            name,
            log,
            atld_exited,
            marked,
            fault,
        };
    vec![
        case("marked, nothing after", mark.clone(), false, true, false),
        case(
            "crash inside the window",
            mark.clone() + &crash_at(T + 500),
            false,
            true,
            true,
        ),
        case(
            "process killed inside the window",
            mark.clone() + &app_line(T + 100, "liblog/Process", "Sending signal. PID: 2 SIG: 9"),
            false,
            true,
            true,
        ),
        case(
            "unserved-service wait inside the window",
            mark.clone()
                + &app_line(
                    T + 1_000,
                    "libbinder",
                    "Waited one second for SurfaceFlingerAIDL-10123-x (is service started? are binder threads started and available?)",
                ),
            false,
            true,
            true,
        ),
        case(
            "crash just past the window",
            mark.clone() + &crash_at(T + WINDOW_MS + 1),
            false,
            true,
            false,
        ),
        case("crash with no marker", crash_at(T), false, false, false),
        case(
            "atld exits inside the window",
            mark.clone(),
            true,
            true,
            true,
        ),
    ]
}

/// Whether the verdict on a case is the one it expects, and the verdict itself.
fn case_holds(c: &SelfCase) -> (bool, Verdict) {
    let v = verdict(&c.log, marker(Stage::Resume), c.atld_exited, WINDOW_MS);
    let holds = v.marker_ms.is_some() == c.marked && v.fault.is_some() == c.fault;
    (holds, v)
}

/// `--self-test`: every case's verdict is checked; the exit status is nonzero when one differs.
fn self_test() -> ExitCode {
    let mut all = true;
    let mut failures = Vec::new();
    for c in self_cases() {
        let (holds, v) = case_holds(&c);
        all &= holds;
        let said = v.fault.unwrap_or_else(|| {
            if v.marker_ms.is_some() {
                "pass".into()
            } else {
                "no marker".into()
            }
        });
        println!(
            "window: self-test {}: {} ({said})",
            c.name,
            if holds { "pass" } else { "FAIL" }
        );
        if !holds {
            failures.push(crate::verdict::Failure::step(
                c.name,
                format!("the verdict was {said}, which the case does not expect"),
            ));
        }
    }
    if all {
        ExitCode::SUCCESS
    } else {
        let verdict = crate::verdict::Verdict {
            cmd: "window",
            total: None,
            skipped: Vec::new(),
            failures,
        };
        println!("{}", verdict.line());
        ExitCode::FAILURE
    }
}

/// The counters' totals as they stood before `at_ms`.
fn before(all: &[Event], at_ms: u64) -> Vec<Counter> {
    totals(all.iter().filter(|e| e.at_ms < at_ms))
}

fn show(c: &Counter) -> String {
    format!("{} {} x{}", c.kind, c.name, c.count)
}

/// The counters before the marker (their totals), then each counted call from the marker on, with its offset from it.
fn print_calls(all: &[Event], marked: u64) {
    for c in before(all, marked) {
        println!("window: before onResume: {}", show(&c));
    }
    for e in all.iter().filter(|e| e.at_ms >= marked) {
        println!(
            "window: +{} ms after onResume: {} {} (total {})",
            e.at_ms - marked,
            e.counter.kind,
            e.counter.name,
            e.counter.count
        );
    }
}

/// The app's waits on a service nothing serves (libbinder's IServiceManager.cpp:727, "Waited one second for <service>"), from the marker on: the calls those waits hold up.
fn print_waits(app_after: &[&str]) {
    for l in app_after
        .iter()
        .filter(|l| l.contains("Waited one second for"))
    {
        println!("window: app waits, service not served: {l}");
    }
}

/// The services' crash reports (ActivityManager's handleApplicationCrash, activity.rs): logged, not counted.
fn print_crash_reports(services: &str) {
    for l in services.lines().filter(|l| l.contains("crash report from")) {
        println!("window: {l}");
    }
}

/// The app's lines after the first one that contains `marker`.
fn after_marker<'a>(app_log: &'a str, marker: &str) -> Vec<&'a str> {
    app_log
        .lines()
        .skip_while(|l| !l.contains(marker))
        .skip(1)
        .collect()
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The quadrants the fixture paints (Main.java `Quadrants.onDraw`): top-left red, top-right green, bottom-left blue, bottom-right white, as RGBA_8888 bytes
/// (Color.RED and the others). Each boundary is half the view's size with integer division (`getWidth() / 2`). The picture is the buffer's rows without stride padding,
/// the bytes the compositor's sink digests.
fn quadrant_pixels(width: u32, height: u32) -> Vec<u8> {
    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    let (half_width, half_height) = (width / 2, height / 2);
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let colour = match (x < half_width, y < half_height) {
                (true, true) => RED,
                (false, true) => GREEN,
                (true, false) => BLUE,
                (false, false) => WHITE,
            };
            pixels.extend_from_slice(&colour);
        }
    }
    pixels
}

/// Sub-gate (a): the first `composer: quad` line of the fixture's layer (the compositor's report of the first frame it got) is the display's size, and its digest is
/// the digest of the quadrants (FNV-1a over [`quadrant_pixels`]).
fn picture_check(app: &str, display: DisplayModel) -> Result<String, String> {
    let (width, height) = (display.width as u32, display.height as u32);
    let size = format!("{width}x{height}");
    let quad = app
        .lines()
        .filter(|l| l.starts_with("composer: quad "))
        .find(|l| word(l, "name") == Some(WINDOW_TITLE))
        .ok_or_else(|| format!("no `composer: quad` line for layer {WINDOW_TITLE}"))?;
    let buffer = word(quad, "buffer").unwrap_or("none");
    if buffer != size {
        return Err(format!("first frame is {buffer}, the display is {size}"));
    }
    let digest = word(quad, "digest").ok_or_else(|| format!("first frame unreadable: {quad}"))?;
    let got = u64::from_str_radix(digest, 16).map_err(|e| format!("digest {digest:?}: {e}"))?;
    let want = fnv1a64(&quadrant_pixels(width, height));
    if got != want {
        return Err(format!(
            "first frame digest {got:016x}, the quadrants {want:016x}"
        ));
    }
    Ok(format!(
        "first frame {size}, digest {want:016x} = the quadrants"
    ))
}

/// Stage `gtk` sub-gate (a): the window mapped at its first frame-clock tick, and the compositor presented at least [`GTK_PRESENTED`] scenes into it before the readback.
fn gtk_window_check(app: &str) -> Result<String, String> {
    if !app
        .lines()
        .any(|l| l.contains("atl-composer: ui: first frame mapped=true"))
    {
        return Err("the window never mapped at its first frame-clock tick".to_owned());
    }
    let line = capture_line(app)?;
    let presented = word(line, "presented")
        .and_then(|v| v.parse::<u32>().ok())
        .ok_or_else(|| format!("no presented count: {line}"))?;
    if presented < GTK_PRESENTED {
        return Err(format!(
            "{presented} scenes presented before the readback, {GTK_PRESENTED} wanted"
        ));
    }
    Ok(format!(
        "mapped, {presented} scenes presented before the readback"
    ))
}

/// Stage `gtk` sub-gate (b): the window's readback at the capture frame has the display's size and the quadrants' digest (the expectation of `picture`).
fn gtk_capture_check(app: &str, display: DisplayModel) -> Result<String, String> {
    let (width, height) = (display.width as u32, display.height as u32);
    let line = capture_line(app)?;
    let size = word(line, "size").unwrap_or("none");
    let want_size = format!("{width}x{height}");
    if size != want_size {
        return Err(format!("readback is {size}, the display is {want_size}"));
    }
    let digest = word(line, "fnv1a64").ok_or_else(|| format!("no digest: {line}"))?;
    let got = u64::from_str_radix(digest, 16).map_err(|e| format!("digest {digest:?}: {e}"))?;
    let want = fnv1a64(&quadrant_pixels(width, height));
    if got != want {
        return Err(format!(
            "readback digest {got:016x}, the quadrants {want:016x}"
        ));
    }
    Ok(format!(
        "readback {want_size} digest {want:016x} = the quadrants"
    ))
}

/// Stops the app processes atld started, before atld is killed: an app whose atld dies under it aborts on its binder calls (DEAD_OBJECT, ~5 s after onResume). SIGTERM, then up to STOP_WAIT for them to exit.
fn stop_apps(atld: u32) {
    let apps: Vec<u32> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                c.windows(APP_LAUNCHER.len())
                    .any(|w| w == APP_LAUNCHER.as_bytes())
            })
        })
        .filter(|&pid| descends_from(pid, atld))
        .collect();
    for pid in &apps {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let deadline = Instant::now() + STOP_WAIT;
    while apps
        .iter()
        .any(|pid| Path::new(&format!("/proc/{pid}")).exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(POLL);
    }
}

/// Whether `pid` is a descendant of `ancestor`, by the parent links in /proc.
fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    while let Some(parent) = ppid(pid) {
        if parent == ancestor {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        pid = parent;
    }
    false
}

/// The parent of `pid`: the field after the command name's closing parenthesis in /proc/<pid>/stat.
fn ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The gtk stage's proxy: the app's Wayland connection goes through it, so the probe sees the pongs and injects the pointer.
fn start_proxy() -> Result<Proxy> {
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
    Proxy::start(
        &crate::platform_ui::upstream_socket()?,
        &runtime.join(format!("atl-window-gate-{}.sock", std::process::id())),
    )
}

/// What the probe saw of the client's liveness.
#[derive(Debug)]
struct Liveness {
    /// The pong to the first ping came within [`PONG_WAIT`].
    answered: bool,
    /// No pong to the second ping within [`SILENT_WAIT`], while the fixture's UI thread slept.
    silent: bool,
    /// Ms from the second ping to its pong, after the sleep (None: no pong within [`RESUME_WAIT`]).
    resumed_ms: Option<u128>,
}

/// The gtk stage's probe, from the proxy's side: the window's configure is acked; a ping is answered; the pointer script goes in; then a click whose UP sleeps the UI thread (past BLOCK_MIN_X). A ping sent meanwhile gets no pong until the sleep ends.
fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    // the fixture's first draw: its window takes input from then on (the headless stage injects after the first frame too)
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    let press = |pressed| PointerAction::Button {
        button: BTN_LEFT,
        pressed,
    };
    // chrome first: surface (100, 20) is in the header bar (below the shadow's 12 px, above the content's origin), so GTK routes the press to the header and Android gets no touch for it
    for action in [
        PointerAction::Enter { x: 100.0, y: 20.0 },
        press(true),
        press(false),
        PointerAction::Enter { x: 200.0, y: 300.0 },
        press(true),
        PointerAction::Motion { x: 260.0, y: 340.0 },
        press(false),
        PointerAction::Motion { x: 600.0, y: 100.0 },
        press(true),
        press(false),
    ] {
        proxy.inject_pointer(action).map_err(|e| e.to_string())?;
    }
    std::thread::sleep(BLOCK_SETTLE);
    let sent = Instant::now();
    proxy.inject_ping(2).map_err(|e| e.to_string())?;
    let silent = await_event(&proxy.events, SILENT_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 2 }
    })
    .is_none();
    let resumed_ms = await_event(&proxy.events, RESUME_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 2 }
    })
    .map(|at| at.duration_since(sent).as_millis());
    Ok(Liveness {
        answered,
        silent,
        resumed_ms,
    })
}

/// Whether `needle` shows up in the file within `within` (polled: the app log is written by another process).
fn wait_for_line(file: &Path, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if std::fs::read_to_string(file).is_ok_and(|text| text.contains(needle)) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// The arrival time of the first event `want` accepts within `within`; the events before it are dropped.
fn await_event(
    events: &Receiver<(Instant, ProxyEvent)>,
    within: Duration,
    want: impl Fn(&ProxyEvent) -> bool,
) -> Option<Instant> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match events.recv_timeout(left) {
            Ok((at, event)) if want(&event) => return Some(at),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Stage `gtk` sub-gate (c): the probe's verdict. The ping is answered, no pong comes while the UI thread sleeps, and the pong follows the sleep.
fn pong_check(liveness: &Result<Liveness, String>) -> Result<String, String> {
    let l = liveness.as_ref().map_err(Clone::clone)?;
    if !l.answered {
        return Err(format!("no pong to the first ping within {PONG_WAIT:?}"));
    }
    if !l.silent {
        return Err(format!(
            "a pong came within {SILENT_WAIT:?} of a ping sent while the UI thread slept"
        ));
    }
    let resumed = l
        .resumed_ms
        .ok_or_else(|| format!("no pong within {RESUME_WAIT:?} after the sleep"))?;
    Ok(format!(
        "ping answered; no pong while the UI thread slept; pong {resumed} ms after its ping"
    ))
}

/// The composer's readback line of the gtk stage (`atl-composer: ui: capture presented=N size=WxH fnv1a64=H`).
fn capture_line(app: &str) -> Result<&str, String> {
    app.lines()
        .find(|l| l.contains("atl-composer: ui: capture "))
        .ok_or_else(|| "no readback line: the window never reached its capture frame".to_owned())
}

/// The frame time (ns) of an `AtlWindow: frame N time=T` line: the fixture's Choreographer callback.
fn frame_time(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once("AtlWindow: frame ")?;
    word(rest, "time")?.parse().ok()
}

/// Sub-gate (b): the first FRAME_CALLBACKS callbacks after the marker come once per display period. The mean interval is within CADENCE_TOLERANCE of the period, and no
/// interval is longer than CADENCE_MAX_PERIODS periods. Times are the vsync's, so the mean does not depend on when the app ran its callbacks.
fn cadence_check(after: &[&str], refresh_hz: f32) -> Result<String, String> {
    let times: Vec<u64> = after
        .iter()
        .filter_map(|l| frame_time(l))
        .take(FRAME_CALLBACKS)
        .collect();
    if times.len() < FRAME_CALLBACKS {
        return Err(format!(
            "{} frame callbacks after the marker, want {FRAME_CALLBACKS}",
            times.len()
        ));
    }
    let period = 1e9 / f64::from(refresh_hz);
    let mut longest = 0.0_f64;
    for pair in times.windows(2) {
        let gap = pair[1]
            .checked_sub(pair[0])
            .ok_or_else(|| format!("frame time goes back: {} then {}", pair[0], pair[1]))?;
        longest = longest.max(gap as f64);
    }
    let span = times[times.len() - 1] - times[0];
    let mean = span as f64 / (times.len() - 1) as f64;
    if (mean - period).abs() > CADENCE_TOLERANCE * period {
        return Err(format!(
            "mean interval {:.3} ms, display period {:.3} ms",
            mean / 1e6,
            period / 1e6
        ));
    }
    if longest > CADENCE_MAX_PERIODS * period {
        return Err(format!(
            "longest interval {:.3} ms, over {CADENCE_MAX_PERIODS} periods of {:.3} ms",
            longest / 1e6,
            period / 1e6
        ));
    }
    Ok(format!(
        "{FRAME_CALLBACKS} callbacks, mean interval {:.3} ms, longest {:.3} ms, display period {:.3} ms",
        mean / 1e6,
        longest / 1e6,
        period / 1e6
    ))
}

/// One touch as the fixture logs it (Main.java `Quadrants.onTouchEvent`): the action, the x and y of the pointer, the pointer count.
#[derive(Debug, Clone, PartialEq)]
struct Touch {
    action: u32,
    x: f32,
    y: f32,
    pointers: u32,
}

/// The touches the pointer script makes, in order: `kind:x,y` items, kind `down`, `move` or `up`, one pointer each.
fn script_touches(script: &str) -> Result<Vec<Touch>, String> {
    script
        .split_whitespace()
        .map(|item| {
            let (kind, at) = item
                .split_once(':')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let action = match kind {
                "down" => ACTION_DOWN,
                "move" => ACTION_MOVE,
                "up" => ACTION_UP,
                _ => return Err(format!("script item {item:?}: kind is down, move or up")),
            };
            let (x, y) = at
                .split_once(',')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let coord = |v: &str| {
                v.parse::<f32>()
                    .map_err(|e| format!("script item {item:?}: {e}"))
            };
            Ok(Touch {
                action,
                x: coord(x)?,
                y: coord(y)?,
                pointers: POINTERS,
            })
        })
        .collect()
}

/// The fixture's `AtlWindow: touch action=A x=X y=Y pointers=N` line as a [`Touch`]; any other line is no touch.
fn touch_line(line: &str) -> Option<Touch> {
    let (_, rest) = line.split_once("AtlWindow: touch ")?;
    Some(Touch {
        action: word(rest, "action")?.parse().ok()?,
        x: word(rest, "x")?.parse().ok()?,
        y: word(rest, "y")?.parse().ok()?,
        pointers: word(rest, "pointers")?.parse().ok()?,
    })
}

/// Sub-gate (c): after the marker the fixture logs one touch per script item, in order, each with the item's action, x and y and one pointer; no other touch.
/// The headless pointer has no decorations: its content origin is (0, 0).
fn input_check(after: &[&str], script: &str) -> Result<String, String> {
    track_check(after, script, (0.0, 0.0))
}

/// The platform's log line of the content origin (atl-platform-ui `GtkWindow::present`), read by [`content_origin`].
const CONTENT_ORIGIN_LINE: &str = "atl-platform-ui: content origin ";

/// The content's origin in the surface, as the platform logs it once the window is mapped: an injected surface point lands in the content at `point - origin`.
fn content_origin(app: &str) -> Result<(f32, f32), String> {
    let line = app
        .lines()
        .find(|l| l.contains(CONTENT_ORIGIN_LINE))
        .ok_or_else(|| "no content origin line: the window never presented".to_owned())?;
    let coord = |key: &str| {
        word(line, key)
            .and_then(|v| v.parse::<f32>().ok())
            .ok_or_else(|| format!("no {key} in {line:?}"))
    };
    Ok((coord("x")?, coord("y")?))
}

/// The pointer script's touches against the fixture's log: the action and pointer count of each, in order, and each position at the script's point minus `origin`
/// (the content's origin in the surface, where the injected points land; (0, 0) for the headless pointer). The offset must round to (0, 0): no measured allowance. Touches past BLOCK_MIN_X are the probe's block, not the script.
fn track_check(after: &[&str], script: &str, origin: (f32, f32)) -> Result<String, String> {
    let want = script_touches(script)?;
    let got: Vec<Touch> = after
        .iter()
        .filter_map(|l| touch_line(l))
        .filter(|t| t.x <= BLOCK_MIN_X)
        .collect();
    if got.len() != want.len() {
        return Err(format!(
            "{} touch lines after the marker, the script has {}",
            got.len(),
            want.len()
        ));
    }
    let (ox, oy) = origin;
    let off = |g: &Touch, w: &Touch| {
        (
            (g.x - (w.x - ox)).round() as i64,
            (g.y - (w.y - oy)).round() as i64,
        )
    };
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        if g.action != w.action || g.pointers != w.pointers || off(g, w) != (0, 0) {
            return Err(format!(
                "touch {i}: got {g:?}, want {w:?} at content origin {origin:?}"
            ));
        }
    }
    Ok(format!(
        "{} touches as `{script}` at content origin {origin:?}",
        want.len()
    ))
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.self_test {
        return Ok(self_test());
    }
    let _ = Layout::of(root)?;
    // hwui's GraphicBufferMapper aborts the app without the mapper ("gralloc-mapper is missing"); the window path always has it, as the app and composer gates do
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "window-art",
        args.verbose,
    )?
    .with_gralloc(root)?;
    let mut proxy = None;
    let rt = match args.stage {
        Stage::Input => rt.with_headless_pointer(POINTER_SCRIPT),
        Stage::Gtk => {
            let p = start_proxy()?;
            let rt = rt.with_ui("gtk").with_wayland(&p.listen);
            proxy = Some(p);
            rt
        }
        Stage::Resume | Stage::Frames => rt,
    };
    let work = rt.out.join("window");
    let apk = rt.build_apk(&root.join(FIXTURE), &work.join("apk"), &[])?;
    let m = manifest::read(&rt.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the fixture has no launcher activity")?;
    println!(
        "window: built {} ({} versionCode {}, sdk {}..{}, launcher activity {activity})",
        apk.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk
    );
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
    let start_ms = now_ms();
    let started = Instant::now();
    let mut atld_child = rt.start_atld(&run)?;
    // the probe drives the client through its proxy from here on
    let probe = proxy.map(|p| {
        let log = run.log(&m.name);
        std::thread::spawn(move || drive(p, &log))
    });
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let marker = marker(args.stage);
    let window_ms = args.stage.window_ms();
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mut ended = None;
    loop {
        // the window runs from the marker's own stamp, not from when this loop saw the marker
        if marker_stamp(&read(&app_log), marker).is_some_and(|t| now_ms() >= t + window_ms) {
            break;
        }
        if let Some(s) = atld_child.try_wait()? {
            ended = Some(s);
            break;
        }
        if started.elapsed() > LIMIT {
            break;
        }
        std::thread::sleep(POLL);
    }
    if ended.is_none() {
        stop_apps(atld_child.id());
        let _ = atld_child.kill();
        atld_child.wait()?;
    }
    let liveness = match probe {
        Some(handle) => handle
            .join()
            .unwrap_or_else(|_| Err("the probe thread panicked".to_owned())),
        None => Err("no probe: only the gtk stage runs one".to_owned()),
    };
    let app = read(&app_log);
    let services = read(&services_log);
    let all = events(&services);
    let v = verdict(&app, marker, ended.is_some(), window_ms);
    if args.verbose {
        println!("---- app log\n{app}");
    }
    // the sub-gates, once the marker is logged (they read the frames or the touches after it)
    let sub: Vec<Result<String, String>> = match (args.stage, v.marker_ms) {
        (Stage::Frames, Some(_)) => {
            let display = DisplayModel::default();
            vec![
                picture_check(&app, display),
                cadence_check(&after_marker(&app, marker), display.refresh_hz),
            ]
        }
        (Stage::Input, Some(_)) => vec![input_check(&after_marker(&app, marker), POINTER_SCRIPT)],
        (Stage::Gtk, Some(_)) => vec![
            gtk_window_check(&app),
            gtk_capture_check(&app, DisplayModel::default()),
            content_origin(&app).and_then(|origin| {
                track_check(&after_marker(&app, marker), POINTER_SCRIPT, origin)
            }),
            pong_check(&liveness),
        ],
        _ => Vec::new(),
    };
    let sub_fault = sub.iter().find_map(|r| r.as_ref().err().cloned());
    for (name, result) in args.stage.sub_names().iter().zip(&sub) {
        match result {
            Ok(s) => println!("window: {name}: pass {s}"),
            Err(s) => println!("window: {name}: FAIL {s}"),
        }
    }
    if let (Some(marked), None, None) = (v.marker_ms, &v.fault, &sub_fault) {
        println!(
            "window: pass {} (`{marker}` logged {} ms after the start; no FATAL EXCEPTION, death or unserved-service wait in the {window_ms} ms after it)",
            args.stage.name(),
            marked.saturating_sub(start_ms)
        );
        print_calls(&all, marked);
        print_crash_reports(&services);
        let rest = after_marker(&app, marker);
        print_waits(&rest);
        println!(
            "---- app log after the marker (last {TAIL} of {} lines)\n{}",
            rest.len(),
            rest[rest.len().saturating_sub(TAIL)..].join("\n")
        );
        return Ok(ExitCode::SUCCESS);
    }
    let why = match (&v.fault, ended, &sub_fault) {
        (Some(fault), _, _) => fault.clone(),
        (None, Some(s), _) => format!("atld exited {s} before onResume"),
        (None, None, Some(fault)) => fault.clone(),
        (None, None, None) => format!("`{marker}` never logged within {LIMIT:?}"),
    };
    println!("window: FAIL {}: {why}", args.stage.name());
    if let Some(marked) = v.marker_ms {
        print_calls(&all, marked);
        print_waits(&after_marker(&app, marker));
    }
    print_crash_reports(&services);
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services, TAIL)
    );
    let verdict = crate::verdict::Verdict {
        cmd: "window",
        total: None,
        skipped: Vec::new(),
        failures: vec![crate::verdict::Failure::step(args.stage.name(), why)],
    };
    println!("{}", verdict.line());
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every self-test case holds: a crash or a death inside the window fails, one just past it passes, a missing marker and an atld exit are as stated.
    #[test]
    fn self_cases_hold() {
        for c in self_cases() {
            let (holds, v) = case_holds(&c);
            assert!(holds, "{}: {v:?}", c.name);
        }
    }

    /// The marker's time is the first stamped line that contains it; an unstamped line is no stamp.
    #[test]
    fn marker_stamp_reads_the_first_stamped_line() {
        let log = "boot line\n1000.001 x marker\n1000.500 x marker\n";
        assert_eq!(marker_stamp(log, "marker"), Some(1_000_001));
        assert_eq!(marker_stamp("no marker here\n", "marker"), None);
    }

    /// The counters before the marker are totals; the calls from it on keep their offsets.
    #[test]
    fn calls_split_at_the_marker() {
        let ev = |at_ms, count| Event {
            at_ms,
            counter: Counter {
                kind: "unimplemented".into(),
                name: "IA.m".into(),
                count,
            },
        };
        let all = [ev(900, 1), ev(1_000, 2), ev(1_004, 3)];
        assert_eq!(
            before(&all, 1_000),
            [Counter {
                kind: "unimplemented".into(),
                name: "IA.m".into(),
                count: 1,
            }]
        );
        let offsets: Vec<u64> = all
            .iter()
            .filter(|e| e.at_ms >= 1_000)
            .map(|e| e.at_ms - 1_000)
            .collect();
        assert_eq!(offsets, [0, 4]);
    }

    /// The fixture's quadrants on a 4x2 buffer, written out pixel by pixel (red and green above the line y = 1, blue and white below; the vertical boundary at x = 2).
    #[test]
    fn quadrants_are_the_fixture_colours() {
        let (r, g, b, w): ([u8; 4], [u8; 4], [u8; 4], [u8; 4]) = (
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        );
        let want: Vec<u8> = [r, r, g, g, b, b, w, w].concat();
        assert_eq!(quadrant_pixels(4, 2), want);
        // an odd size: the boundary is still width / 2 = 2 (getWidth() / 2), so x = 1 is red and x = 2 green
        let odd = quadrant_pixels(5, 3);
        assert_eq!(odd.len(), 5 * 3 * 4);
        assert_eq!(&odd[4..8], &r, "x = 1, y = 0");
        assert_eq!(&odd[8..12], &g, "x = 2, y = 0");
    }

    /// The first frame of the fixture's layer must be the quadrants at the display's size. The expected digest is FNV-1a over the bytes written out here, not over quadrant_pixels.
    #[test]
    fn picture_check_reads_the_first_frame_of_the_layer() {
        let display = DisplayModel {
            width: 4,
            height: 2,
            ..DisplayModel::default()
        };
        let (r, g, b, w): ([u8; 4], [u8; 4], [u8; 4], [u8; 4]) = (
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        );
        let picture: Vec<u8> = [r, r, g, g, b, b, w, w].concat();
        let quad = |name: &str, buffer: &str, digest: String| {
            format!(
                "composer: quad frame=1 layer=2 name={name} buffer={buffer} frame_number=1 dst=0,0,4,2 digest={digest}\n"
            )
        };
        let good = format!("{:016x}", fnv1a64(&picture));
        assert!(picture_check(&quad(WINDOW_TITLE, "4x2", good.clone()), display).is_ok());
        assert!(
            picture_check(&quad("other", "4x2", good.clone()), display).is_err(),
            "another layer's frame is not the fixture's"
        );
        assert!(
            picture_check(&quad(WINDOW_TITLE, "4x3", good.clone()), display).is_err(),
            "a buffer of the wrong size"
        );
        let red = format!("{:016x}", fnv1a64(&r.repeat(8)));
        assert!(
            picture_check(&quad(WINDOW_TITLE, "4x2", red), display).is_err(),
            "all red is not the quadrants"
        );
        let unreadable = format!(
            "composer: quad frame=1 layer=2 name={WINDOW_TITLE} buffer=4x2 unreadable=no dma-buf\n"
        );
        assert!(picture_check(&unreadable, display).is_err());
    }

    /// Callbacks one display period apart pass. A mean off by a fifth, too few callbacks, and one missed vsync (one interval of two periods, the mean still within tolerance) fail.
    #[test]
    fn cadence_check_reads_the_callbacks() {
        const STEP: u64 = 16_666_667;
        let lines = |times: &[u64]| -> Vec<String> {
            times
                .iter()
                .enumerate()
                .map(|(i, t)| format!("1000.000 1 2 I AtlWindow: frame {} time={t}", i + 1))
                .collect()
        };
        let check = |times: &[u64], hz: f32| {
            let owned = lines(times);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            cadence_check(&refs, hz)
        };
        let steady: Vec<u64> = (0..31).map(|i| 5_000_000_000 + i * STEP).collect();
        let ok = check(&steady, 60.0);
        assert!(ok.is_ok(), "{ok:?}");
        let short: Vec<u64> = (0..30).map(|i| 5_000_000_000 + i * STEP).collect();
        assert!(check(&short, 60.0).is_err());
        let fast: Vec<u64> = (0..31).map(|i| 5_000_000_000 + i * 20_000_000).collect();
        assert!(
            check(&fast, 60.0).is_err(),
            "20 ms intervals on a 60 Hz display"
        );
        let mut missed = Vec::new();
        let mut t = 5_000_000_000u64;
        for i in 0..31 {
            missed.push(t);
            t += if i == 15 { 2 * STEP } else { STEP };
        }
        assert!(check(&missed, 60.0).is_err(), "one interval of two periods");
        assert!(frame_time("1000.000 1 2 I AtlWindow: onDraw reached").is_none());
    }

    /// The fixture's touch line reads as its action, x, y and pointer count; any other line is no touch.
    #[test]
    fn touch_line_reads_the_fixture_log() {
        assert_eq!(
            touch_line("1000.000 1 2 I AtlWindow: touch action=2 x=260.0 y=340.0 pointers=1"),
            Some(Touch {
                action: 2,
                x: 260.0,
                y: 340.0,
                pointers: 1,
            })
        );
        assert!(touch_line("1000.000 1 2 I AtlWindow: onDraw reached").is_none());
    }

    /// The script's items written out by hand: a down at (200, 300), then a move and an up at (260, 340), one pointer each. The actions are MotionEvent's ACTION_DOWN 0, ACTION_MOVE 2 and ACTION_UP 1.
    #[test]
    fn script_touches_follow_the_script_text() {
        let want = [
            Touch {
                action: 0,
                x: 200.0,
                y: 300.0,
                pointers: 1,
            },
            Touch {
                action: 2,
                x: 260.0,
                y: 340.0,
                pointers: 1,
            },
            Touch {
                action: 1,
                x: 260.0,
                y: 340.0,
                pointers: 1,
            },
        ];
        assert_eq!(script_touches(POINTER_SCRIPT), Ok(want.to_vec()));
        assert!(script_touches("tap:1,2").is_err(), "no such kind");
        assert!(script_touches("down:1").is_err(), "no y");
        assert!(script_touches("down:a,2").is_err(), "x is not a number");
    }

    /// The touches after the marker must be the script's, in order: one line each, with the script's action, x and y and one pointer. The expected lines are written out here, not made by the code under test.
    #[test]
    fn input_check_matches_the_touch_lines() {
        fn refs(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        let line = |action: u32, x: &str, y: &str, pointers: u32| {
            format!(
                "1000.000 1 2 I AtlWindow: touch action={action} x={x} y={y} pointers={pointers}"
            )
        };
        let ok = [
            line(0, "200.0", "300.0", 1),
            line(2, "260.0", "340.0", 1),
            line(1, "260.0", "340.0", 1),
        ];
        assert!(input_check(&refs(&ok), POINTER_SCRIPT).is_ok());
        let wrong_x = [line(0, "201.0", "300.0", 1), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&wrong_x), POINTER_SCRIPT).is_err(),
            "x off by one"
        );
        assert!(
            input_check(&refs(&ok[..2]), POINTER_SCRIPT).is_err(),
            "the up is missing"
        );
        let extra = [ok[0].clone(), ok[0].clone(), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&extra), POINTER_SCRIPT).is_err(),
            "one touch too many"
        );
        let reordered = [ok[0].clone(), ok[2].clone(), ok[1].clone()];
        assert!(
            input_check(&refs(&reordered), POINTER_SCRIPT).is_err(),
            "up before move"
        );
        let two = [line(0, "200.0", "300.0", 2), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&two), POINTER_SCRIPT).is_err(),
            "two pointers"
        );
        let up_first = [line(1, "200.0", "300.0", 1), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&up_first), POINTER_SCRIPT).is_err(),
            "an up where the script has a down"
        );
        assert!(
            input_check(&[], POINTER_SCRIPT).is_err(),
            "no touches at all"
        );
    }

    /// A stamp is printed in the form parse_stamp reads.
    #[test]
    fn stamp_text_round_trips() {
        assert_eq!(stamp_text(1_793_747_156_733), "1793747156.733");
        assert_eq!(
            parse_stamp(&stamp_text(1_793_747_156_733)),
            Some(1_793_747_156_733)
        );
    }
}
