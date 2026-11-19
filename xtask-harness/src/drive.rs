// SPDX-License-Identifier: MIT
//! `xtask run-apk --drive <script>`: an agent's hands and eyes on a real APK, headless. A script runs against the app's window once run-apk's own wait is over (resumed, settled, a frame presented, the watch done).
//! Input goes through the window proxy (wayland_proxy.rs), captures through the compositor (screencopy.rs), compared in memory. The compositor holds the window at output (0, 0) ([`NestedCompositor::start_placed`]) at output scale 1.
//! The grammar is [`GRAMMAR`], also `run-apk --help`'s text for `--drive`: one source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::nested_wayland::NestedCompositor;
use crate::screencopy::{self, Diff, Image};
use crate::wayland_proxy::{Event, PointerAction, Proxy, TouchAction};
use crate::window;

/// The script grammar (`run-apk --drive`'s help text).
pub const GRAMMAR: &str = "A script file run against the window on the headless compositor, one step a line; blank lines and `#` comments are skipped. Every wait names its timeout: nothing waits by default.
Header (before the first step): `size W H` = the content size the script's coordinates were read at; points and regions scale per axis to the live content. Without it coordinates are live content pixels.
Scenario headers (a tracked scenario, `run-apk --scenario`; a plain run ignores them): `control BREAK` (a deliberate break the scenario must fail under, at a checkpoint) | `unreachable STATE: REASON` (declared out of reach, printed in the verdict) | `needs ARCH` (the guest arch of the row's bridge) | `network offline|online` (the network the scenario is written for: offline = loopback only, the default regression form; `--network` overrides it).
A region is `X Y W H` in the same pixels. Text arguments are one word or \"quoted\"; a source is `app`, `services` or `atld` (that process's log).
Input: `tap X Y` / `swipe X1 Y1 X2 Y2 MS` (the mouse) | `touch X Y MS` / `touch-swipe X1 Y1 X2 Y2 MS` (one finger, wl_touch; Unity reads only this) | `leave` (mouse off the window) | `key NAME` / `key-down NAME` / `key-up NAME` (a letter, a digit, escape, backspace, tab, enter, space, home, end, up, down, left, right, pageup, pagedown, back) | `resize W H` (content size).
Time: `wait MS` | `wait-frames N` (N presents from here, 120 s bound; only after something that redraws).
Condition waits (checkpoints): `wait-log SOURCE TEXT MS` (a line holding TEXT since the last checkpoint) | `wait-changed REGION MS` (the region changes from its picture at this step) | `wait-stable REGION MS HOLD_MS` (no change for HOLD_MS).
Marks (no checkpoint: they judge nothing): `mark NAME [REGION]` (keeps the content's picture).
Checkpoints: `expect-changed NAME [REGION]` / `expect-same NAME [REGION]` (now vs the mark; region default: the mark's, else the content) | `expect-varied [REGION]` (not one colour)
 | `expect-log SOURCE TEXT` / `expect-no-log SOURCE TEXT` (since the last checkpoint) | `expect-frames N MS` (N presents within MS) | `expect-fps FLOOR tier=TIER over MS` (presents per second over MS >= FLOOR; judged only when the app logged buffer tier TIER, else skipped: counted apart in the verdict, never as passed; write one line per tier the scenario runs on)
 | `expect-audio MS` (an `audio-mix` line with audible > 0 in the app log within MS) | `expect-counter KEY OP N` (KEY = `<outcome> <api>` of the diag counters summed over the run's logs, quoted; OP one of >= <= == > <).
Branch: `if-log SOURCE TEXT MS` ... `end` = the steps inside run only when the line comes within MS (an optional first-run dialog) | `if-same NAME [REGION]` ... `end` = only when the region is `same` as the mark now (an optional dialog that logs nothing); one level, no other control flow.
`changed` = more than CHANGED_FRACTION of the region's pixels differ by more than CHANGE_DELTA in a channel; `same` = no pixel differs by more than CHANGE_DELTA.
`shot FILE.png` writes the content (relative to the current directory; a one-colour image fails). The app is stopped after the last step; a failed step exits non-zero, naming it.";

const BTN_LEFT: u32 = 0x110;
/// One softpipe frame of a large window takes seconds under load, so a frame wait bounds a hang, not a pace.
const FRAME_WAIT: Duration = Duration::from_secs(120);
/// A swipe's motion events come at about the display's 60 Hz.
const SWIPE_STEP: Duration = Duration::from_millis(16);
const POLL: Duration = Duration::from_millis(20);
/// A visual wait's capture interval: ~6 frames at 60 Hz, so a capture (a shm copy of the output) stays a small share of the time.
const VISUAL_POLL: Duration = Duration::from_millis(100);
/// Per-channel difference above which a pixel counts as changed. Derivation (widgets fixture, `xtask test drive`, WSL readback 2026-11-20): two still captures 3 s apart differ by 0 in every channel,
/// the Button's press by up to 145; 16 (1/16 of the range) sits above any rounding a scaled or dithered path adds (a few units) and far below the press.
const CHANGE_DELTA: u8 = 16;
/// Share of a region's pixels that must change for `changed`. Derivation: the widgets Button's tap once changed 3972 of the 1440x900 = 1296000 content px (0.306 %), half that = 0.15 %, which leaves
/// room for a smaller widget while a few stray pixels (a caret, a cursor) stay below it. That 3972 counted the window leaving touch mode (windows were added out of touch mode then); in touch mode
/// the tap changes only the label "Clicked 1": 1448 px (0.112 % of the content, 3.1 % of the label's 1408x33), so a check of a small change names its region.
const CHANGED_FRACTION: f64 = 0.0015;
/// Injected configure serials: far above what the compositor sends one window in a run.
const FIRST_SERIAL: u32 = 0x10_0000;
/// The mixer's counter line (atl-audioflinger output.rs) and its field of frames above silence.
const MIX_LINE: &str = "audio-mix: ";
const MIX_AUDIBLE: &str = "audible=";
/// Key rows of linux/input-event-codes.h: `1`..`0` are KEY_1 = 2.., the letter rows start at KEY_Q = 16, KEY_A = 30, KEY_Z = 44.
const DIGITS: &str = "1234567890";
const FIRST_DIGIT: u32 = 2;
const LETTER_ROWS: [(&str, u32); 3] = [("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
const NAMED_KEYS: [(&str, u32); 14] = [
    ("escape", 1),
    ("backspace", 14),
    ("tab", 15),
    ("enter", 28),
    ("space", 57),
    ("home", 102),
    ("up", 103),
    ("pageup", 104),
    ("left", 105),
    ("right", 106),
    ("end", 107),
    ("down", 108),
    ("pagedown", 109),
    ("back", 158),
];

/// A log a step reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    App,
    Services,
    Atld,
}

const SOURCES: [Source; 3] = [Source::App, Source::Services, Source::Atld];

impl Source {
    fn index(self) -> usize {
        self as usize
    }
}

/// `X Y W H` in script pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Ge,
    Le,
    Eq,
    Gt,
    Lt,
}

impl Op {
    fn holds(self, got: u64, want: u64) -> bool {
        match self {
            Op::Ge => got >= want,
            Op::Le => got <= want,
            Op::Eq => got == want,
            Op::Gt => got > want,
            Op::Lt => got < want,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Wait(u64),
    WaitFrames(usize),
    Tap(f64, f64),
    /// One finger at (x, y), held for the ms.
    Touch(f64, f64, u64),
    /// One finger down at `from`, moved to `to` over the ms, then up.
    TouchSwipe {
        from: (f64, f64),
        to: (f64, f64),
        ms: u64,
    },
    /// The mouse leaves the window (`wl_pointer.leave` -> HOVER_EXIT): no pointer hovers the content.
    Leave,
    Swipe {
        from: (f64, f64),
        to: (f64, f64),
        ms: u64,
    },
    Key(u32),
    /// `key-down` / `key-up`: one half of a key, so a key can be held across waits and shots.
    KeyHalf(u32, bool),
    Resize(i32, i32),
    Shot(PathBuf),
    WaitLog {
        source: Source,
        text: String,
        ms: u64,
    },
    WaitChanged {
        region: Region,
        ms: u64,
    },
    WaitStable {
        region: Region,
        ms: u64,
        hold: u64,
    },
    Mark {
        name: String,
        region: Option<Region>,
    },
    ExpectChanged {
        name: String,
        region: Option<Region>,
    },
    ExpectSame {
        name: String,
        region: Option<Region>,
    },
    ExpectVaried(Option<Region>),
    ExpectLog {
        source: Source,
        text: String,
        present: bool,
    },
    ExpectFrames {
        n: u64,
        ms: u64,
    },
    ExpectFps {
        floor: f64,
        tier: String,
        ms: u64,
    },
    ExpectAudio(u64),
    ExpectCounter {
        key: String,
        op: Op,
        n: u64,
    },
    /// `if-log` / `if-same` ... `end`: `body` runs only when `cond` holds.
    If {
        cond: Cond,
        body: Vec<Step>,
    },
}

/// A branch's condition.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// `if-log SOURCE TEXT MS`: the line comes within the ms.
    Log {
        source: Source,
        text: String,
        ms: u64,
    },
    /// `if-same NAME [REGION]`: the picture now is `same` as the mark (region default as for `expect-same`); judged at once.
    Same {
        name: String,
        region: Option<Region>,
    },
}

impl Cond {
    /// Whether judging it waits (a `wait` may then open the body).
    pub fn waits(&self) -> bool {
        matches!(self, Cond::Log { .. })
    }
}

/// The network a scenario run has: `offline` = atld and everything it starts in a network namespace of the harness with loopback only (atld_run.rs `Runtime::offline`); `online` = the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Network {
    Offline,
    Online,
}

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Network::Offline => "offline",
            Network::Online => "online",
        }
    }
}

/// A parsed script: its headers and the steps. `controls`, `unreachable`, `needs` and `network` are a tracked scenario's (scenarios.rs); a plain `--drive` run ignores them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Script {
    pub size: Option<(f64, f64)>,
    /// `network offline|online`: the network the scenario is written for (`--network` overrides it for a run).
    pub network: Option<Network>,
    /// `control NAME`: a deliberate break under which the scenario must fail at a checkpoint.
    pub controls: Vec<String>,
    /// `unreachable STATE: REASON`: a state the scenario declares out of reach, printed in its verdict.
    pub unreachable: Vec<(String, String)>,
    /// `needs ARCH`: the guest arch the row runs under (its bridge).
    pub needs: Option<String>,
    pub steps: Vec<Step>,
}

impl Script {
    /// The top-level checkpoints, the `n` of a scenario's `<n> checkpoints` (a branch counts as one: its body is optional).
    pub fn checkpoints(&self) -> usize {
        self.steps.iter().filter(|s| is_checkpoint(s)).count()
    }

    /// Every `expect-fps` step, branch bodies included: a run that judged none of them says so in its verdict.
    pub fn expect_fps(&self) -> usize {
        fn count(steps: &[Step]) -> usize {
            steps
                .iter()
                .map(|s| match s {
                    Step::ExpectFps { .. } => 1,
                    Step::If { body, .. } => count(body),
                    _ => 0,
                })
                .sum()
        }
        count(&self.steps)
    }
}

/// How a run's `shot` steps keep their pictures.
#[derive(Debug, Clone)]
pub enum Shots {
    /// At the path the step names (`--drive`).
    AsNamed,
    /// Judged (one colour fails) and dropped: a scenario's default.
    Memory,
    /// Under this dir, by the step's file name (`--keep-shots`).
    Into(PathBuf),
}

/// What a run got to: top-level checkpoints judged and passed, those that judged nothing (`skipped`, each with its reason: an `expect-fps` whose tier the run did not have, a branch not
/// taken), and the failure if any (`at_checkpoint`: it failed in a checkpoint step, not in input, a wait or a shot). A skipped checkpoint never counts as passed.
#[derive(Debug)]
pub struct Outcome {
    pub passed: usize,
    pub skipped: Vec<String>,
    /// `expect-fps` steps that measured (any tier mismatch is in `skipped`).
    pub fps_judged: usize,
    pub error: Option<anyhow::Error>,
    pub at_checkpoint: bool,
}

/// The linux input code of a script key name.
fn key_code(name: &str) -> Option<u32> {
    if let Some(&(_, code)) = NAMED_KEYS.iter().find(|(n, _)| *n == name) {
        return Some(code);
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if let Some(at) = DIGITS.find(c) {
        return Some(FIRST_DIGIT + at as u32);
    }
    LETTER_ROWS
        .iter()
        .find_map(|(row, first)| Some(first + row.find(c)? as u32))
}

/// Words of a line; `"..."` is one word without its quotes (no escapes).
fn words(line: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() {
        if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').context("unclosed quote")?;
            out.push(quoted[..end].to_owned());
            rest = quoted[end + 1..].trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push(rest[..end].to_owned());
            rest = rest[end..].trim_start();
        }
    }
    Ok(out)
}

/// The script; an error names the line.
pub fn parse(text: &str) -> Result<Script> {
    let mut script = Script::default();
    let mut size = None;
    let mut steps = Vec::new();
    // an open branch: its line and its step; the body collects until `end`
    let mut open: Option<(usize, Step)> = None;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = || format!("script line {}", index + 1);
        let header = |name: &str| -> Result<()> {
            ensure!(
                steps.is_empty() && open.is_none(),
                "{}: `{name}` is a header: before the first step",
                at()
            );
            Ok(())
        };
        // free text after the state's colon: not split into words
        if let Some(rest) = line.strip_prefix("unreachable ") {
            header("unreachable")?;
            let (state, reason) = rest
                .split_once(':')
                .map(|(s, r)| (s.trim(), r.trim()))
                .filter(|(s, r)| !s.is_empty() && !r.is_empty())
                .with_context(|| format!("{}: unreachable needs `STATE: REASON`", at()))?;
            script
                .unreachable
                .push((state.to_owned(), reason.to_owned()));
            continue;
        }
        let w = words(line).with_context(at)?;
        let w: Vec<&str> = w.iter().map(String::as_str).collect();
        match w.as_slice() {
            ["size", sw, sh] => {
                header("size")?;
                ensure!(size.is_none(), "{}: `size` given twice", at());
                let (sw, sh): (f64, f64) = (num(sw).with_context(at)?, num(sh).with_context(at)?);
                ensure!(sw > 0.0 && sh > 0.0, "{}: size needs a positive size", at());
                size = Some((sw, sh));
            }
            ["control", name] => {
                header("control")?;
                script.controls.push((*name).to_owned());
            }
            ["needs", arch] => {
                header("needs")?;
                ensure!(script.needs.is_none(), "{}: `needs` given twice", at());
                script.needs = Some((*arch).to_owned());
            }
            ["network", mode] => {
                header("network")?;
                ensure!(script.network.is_none(), "{}: `network` given twice", at());
                script.network = Some(match *mode {
                    "offline" => Network::Offline,
                    "online" => Network::Online,
                    other => bail!("{}: network {other:?} is neither offline nor online", at()),
                });
            }
            ["end"] => {
                let (_, step) = open
                    .take()
                    .with_context(|| format!("{}: `end` with no open branch", at()))?;
                steps.push(step);
            }
            _ => {
                let step = parse_step(&w).with_context(at)?;
                match (&mut open, step) {
                    (Some(_), Step::If { .. }) => {
                        bail!("{}: branch inside a branch: one level only", at())
                    }
                    (None, step @ Step::If { .. }) => open = Some((index + 1, step)),
                    (Some((_, Step::If { body, .. })), step) => body.push(step),
                    (Some(_), _) => unreachable!("only a branch is ever open"),
                    (None, step) => steps.push(step),
                }
            }
        }
    }
    if let Some((line, _)) = open {
        bail!("script line {line}: branch has no `end`");
    }
    script.size = size;
    script.steps = steps;
    Ok(script)
}

fn num<T: std::str::FromStr>(word: &str) -> Result<T> {
    word.parse()
        .map_err(|_| anyhow::anyhow!("{word:?} is not a valid number"))
}

fn source(word: &str) -> Result<Source> {
    Ok(match word {
        "app" => Source::App,
        "services" => Source::Services,
        "atld" => Source::Atld,
        _ => bail!("{word:?} is no source (app, services, atld)"),
    })
}

fn region(w: &[&str]) -> Result<Region> {
    let [x, y, rw, rh] = w else {
        bail!("a region is X Y W H");
    };
    let r = Region {
        x: num(x)?,
        y: num(y)?,
        w: num(rw)?,
        h: num(rh)?,
    };
    ensure!(
        r.x >= 0.0 && r.y >= 0.0 && r.w > 0.0 && r.h > 0.0,
        "a region needs x, y >= 0 and a positive size"
    );
    Ok(r)
}

fn opt_region(w: &[&str]) -> Result<Option<Region>> {
    if w.is_empty() {
        Ok(None)
    } else {
        region(w).map(Some)
    }
}

fn op(word: &str) -> Result<Op> {
    Ok(match word {
        ">=" => Op::Ge,
        "<=" => Op::Le,
        "==" => Op::Eq,
        ">" => Op::Gt,
        "<" => Op::Lt,
        _ => bail!("{word:?} is no comparison (>= <= == > <)"),
    })
}

fn parse_step(words: &[&str]) -> Result<Step> {
    let key = |name: &str| key_code(name).with_context(|| format!("no key named {name:?}"));
    Ok(match words {
        ["wait", ms] => Step::Wait(num(ms)?),
        ["wait-frames", n] => Step::WaitFrames(num(n)?),
        ["tap", x, y] => Step::Tap(num(x)?, num(y)?),
        ["touch", x, y, ms] => Step::Touch(num(x)?, num(y)?, num(ms)?),
        ["touch-swipe", x1, y1, x2, y2, ms] => Step::TouchSwipe {
            from: (num(x1)?, num(y1)?),
            to: (num(x2)?, num(y2)?),
            ms: num(ms)?,
        },
        ["leave"] => Step::Leave,
        ["swipe", x1, y1, x2, y2, ms] => Step::Swipe {
            from: (num(x1)?, num(y1)?),
            to: (num(x2)?, num(y2)?),
            ms: num(ms)?,
        },
        ["key", name] => Step::Key(key(name)?),
        [half @ ("key-down" | "key-up"), name] => Step::KeyHalf(key(name)?, *half == "key-down"),
        ["resize", w, h] => {
            let (w, h) = (num(w)?, num(h)?);
            ensure!(w > 0 && h > 0, "resize needs a positive size");
            Step::Resize(w, h)
        }
        ["shot", file] => Step::Shot(PathBuf::from(file)),
        ["wait-log", s, text, ms] => Step::WaitLog {
            source: source(s)?,
            text: (*text).to_owned(),
            ms: num(ms)?,
        },
        ["wait-changed", r @ .., ms] if r.len() == 4 => Step::WaitChanged {
            region: region(r)?,
            ms: num(ms)?,
        },
        ["wait-stable", r @ .., ms, hold] if r.len() == 4 => {
            let (ms, hold) = (num(ms)?, num(hold)?);
            ensure!(
                hold < ms,
                "wait-stable: the hold must be shorter than the timeout"
            );
            Step::WaitStable {
                region: region(r)?,
                ms,
                hold,
            }
        }
        ["mark", name, r @ ..] => Step::Mark {
            name: (*name).to_owned(),
            region: opt_region(r)?,
        },
        ["expect-changed", name, r @ ..] => Step::ExpectChanged {
            name: (*name).to_owned(),
            region: opt_region(r)?,
        },
        ["expect-same", name, r @ ..] => Step::ExpectSame {
            name: (*name).to_owned(),
            region: opt_region(r)?,
        },
        ["expect-varied", r @ ..] => Step::ExpectVaried(opt_region(r)?),
        [which @ ("expect-log" | "expect-no-log"), s, text] => Step::ExpectLog {
            source: source(s)?,
            text: (*text).to_owned(),
            present: *which == "expect-log",
        },
        ["expect-frames", n, ms] => Step::ExpectFrames {
            n: num(n)?,
            ms: num(ms)?,
        },
        ["expect-fps", floor, tier, "over", ms] => Step::ExpectFps {
            floor: num(floor)?,
            tier: tier
                .strip_prefix("tier=")
                .filter(|t| !t.is_empty())
                .context("expect-fps needs tier=<tier>")?
                .to_owned(),
            ms: num(ms)?,
        },
        ["expect-audio", ms] => Step::ExpectAudio(num(ms)?),
        ["expect-counter", k, o, n] => Step::ExpectCounter {
            key: (*k).to_owned(),
            op: op(o)?,
            n: num(n)?,
        },
        ["if-log", s, text, ms] => Step::If {
            cond: Cond::Log {
                source: source(s)?,
                text: (*text).to_owned(),
                ms: num(ms)?,
            },
            body: Vec::new(),
        },
        ["if-same", name, r @ ..] => Step::If {
            cond: Cond::Same {
                name: (*name).to_owned(),
                region: opt_region(r)?,
            },
            body: Vec::new(),
        },
        _ => bail!("not a step: {:?}", words.join(" ")),
    })
}

/// Window geometry in surface coordinates (`xdg_surface.set_window_geometry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// The content's rectangle on the output as (x, y, width, height): the window's geometry sits at (0, 0), the content at `origin` in the surface, reaching the geometry's right and bottom edges (GTK's chrome is the header bar alone).
fn content_rect(geometry: Geometry, origin: (i32, i32)) -> (i32, i32, i32, i32) {
    let (x, y) = (origin.0 - geometry.x, origin.1 - geometry.y);
    (x, y, geometry.width - x, geometry.height - y)
}

/// A script point in live content pixels: scaled per axis from the reference `size` to the live `content` size (pixels); without `size` the point itself.
fn scale_point(p: (f64, f64), size: Option<(f64, f64)>, content: (f64, f64)) -> (f64, f64) {
    match size {
        Some((rw, rh)) => (p.0 * content.0 / rw, p.1 * content.1 / rh),
        None => p,
    }
}

pub struct Target<'a> {
    pub proxy: &'a Proxy,
    pub nested: &'a NestedCompositor,
    pub app_log: &'a Path,
    pub services_log: &'a Path,
    pub atld_log: &'a Path,
    /// The run's log dir: every process's log, which `expect-counter` sums.
    pub log_dir: &'a Path,
    /// False once the app died: a wait ends with it.
    pub alive: &'a dyn Fn() -> bool,
    /// Deliberate failure: each shot is blanked before its check, which must then fail the run.
    pub break_blank: bool,
    pub shots: Shots,
}

struct Driver<'a> {
    /// Top-level checkpoints passed so far, each having judged something.
    passed: usize,
    /// [`Outcome::skipped`] so far.
    skipped: Vec<String>,
    fps_judged: usize,
    /// Whether the step that failed is a checkpoint.
    failed_checkpoint: bool,
    t: Target<'a>,
    size: Option<(f64, f64)>,
    /// The content's origin in the surface, from the platform's log line.
    origin: (i32, i32),
    /// The app log as it was at the start: holds the origin line [`window::content_to_surface`] reads.
    origin_log: String,
    /// The window's scale (content pixels per surface unit), from the same line.
    scale: f64,
    geometry: Option<Geometry>,
    serial: u32,
    marks: HashMap<String, (Image, Option<Region>)>,
    /// Per source: the log's length at the last checkpoint; `expect-log` and `wait-log` read from there.
    cursors: [usize; 3],
}

/// Runs `script`, stopping at the first failure.
pub fn run(script: &Script, target: Target<'_>) -> Outcome {
    let log = atl_diag::logfiles::read(target.app_log);
    let placed = window::content_origin(&log).and_then(|(x, y)| {
        // the scale: one content pixel past the origin, in surface units
        let one = window::content_to_surface(&log, (1.0, 0.0))?;
        Ok((x, y, 1.0 / (one.0 - f64::from(x))))
    });
    let (x, y, scale) = match placed {
        Ok(p) => p,
        Err(e) => {
            return Outcome {
                passed: 0,
                skipped: Vec::new(),
                fps_judged: 0,
                error: Some(anyhow::Error::msg(e)),
                at_checkpoint: false,
            };
        }
    };
    let mut d = Driver {
        passed: 0,
        skipped: Vec::new(),
        fps_judged: 0,
        failed_checkpoint: false,
        t: target,
        size: script.size,
        origin: (x.round() as i32, y.round() as i32),
        origin_log: log,
        scale,
        geometry: None,
        serial: FIRST_SERIAL,
        marks: HashMap::new(),
        cursors: [0; 3],
    };
    let error = d.steps(&script.steps, "").err();
    Outcome {
        passed: d.passed,
        skipped: d.skipped,
        fps_judged: d.fps_judged,
        at_checkpoint: error.is_some() && d.failed_checkpoint,
        error,
    }
}

/// The phase a script step's time counts in: waits and shots on the app's own clock, input, resizes, checkpoints.
fn step_phase(step: &Step) -> &'static str {
    match step {
        Step::Wait(_) => "script: wait",
        Step::WaitFrames(_) => "script: wait frames",
        Step::Shot(_) => "script: shot",
        Step::Resize(..) => "script: resize",
        Step::Tap(..)
        | Step::Touch(..)
        | Step::TouchSwipe { .. }
        | Step::Leave
        | Step::Swipe { .. }
        | Step::Key(_)
        | Step::KeyHalf(..) => "script: input",
        Step::If { cond, .. } if cond.waits() => "script: condition wait",
        Step::WaitLog { .. } | Step::WaitChanged { .. } | Step::WaitStable { .. } => {
            "script: condition wait"
        }
        Step::If { .. }
        | Step::Mark { .. }
        | Step::ExpectChanged { .. }
        | Step::ExpectSame { .. }
        | Step::ExpectVaried(_)
        | Step::ExpectLog { .. }
        | Step::ExpectFrames { .. }
        | Step::ExpectFps { .. }
        | Step::ExpectAudio(_)
        | Step::ExpectCounter { .. } => "script: checkpoint",
    }
}

/// Whether a step sends input to the app (tap, touch, swipe, key, leave).
pub fn is_input(step: &Step) -> bool {
    step_phase(step) == "script: input"
}

/// Whether a step is an `expect-*` (judges the run at that point).
pub fn is_expect(step: &Step) -> bool {
    matches!(
        step,
        Step::ExpectChanged { .. }
            | Step::ExpectSame { .. }
            | Step::ExpectVaried(_)
            | Step::ExpectLog { .. }
            | Step::ExpectFrames { .. }
            | Step::ExpectFps { .. }
            | Step::ExpectAudio(_)
            | Step::ExpectCounter { .. }
    )
}

/// Whether a step is a checkpoint: once it passes, `expect-log` reads only what comes after it. A `mark` judges nothing (it keeps a picture), so it is none.
fn is_checkpoint(step: &Step) -> bool {
    !matches!(step, Step::Mark { .. })
        && matches!(
            step_phase(step),
            "script: condition wait" | "script: checkpoint"
        )
}

/// A short form of a step for the progress and error lines (a branch's body left out).
fn show(step: &Step) -> String {
    match step {
        Step::If { cond, .. } => format!("If({cond:?})"),
        other => format!("{other:?}"),
    }
}

/// The text of `log` from byte `from`; a log shorter than that (its tail segment rotated) is read whole.
fn since(log: &str, from: usize) -> &str {
    log.get(from..).unwrap_or(log)
}

/// The `audible=` count of an `audio-mix` line, if it is one.
fn audible(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once(MIX_LINE)?;
    rest.split_whitespace()
        .find_map(|w| w.strip_prefix(MIX_AUDIBLE))?
        .parse()
        .ok()
}

/// The diag counter `key` (`<outcome> <api>`) summed over `logs` (atl_diag summary totals; one per process). None: no process counted it.
fn counter(logs: &[String], key: &str) -> Option<u64> {
    let mut sum = None;
    for log in logs {
        for c in atl_diag::summary::counters(log) {
            if format!("{} {}", c.outcome, c.api) == key {
                *sum.get_or_insert(0) += c.count;
            }
        }
    }
    sum
}

/// `changed` per [`CHANGE_DELTA`] / [`CHANGED_FRACTION`].
fn is_changed(d: &Diff) -> bool {
    d.fraction() > CHANGED_FRACTION
}

/// `same`: no pixel apart by more than [`CHANGE_DELTA`].
fn is_same(d: &Diff) -> bool {
    d.changed == 0
}

fn show_diff(d: &Diff) -> String {
    format!(
        "{} of {} px changed ({:.3} %), max channel delta {}",
        d.changed,
        d.total,
        d.fraction() * 100.0,
        d.max_delta
    )
}

impl Driver<'_> {
    fn steps(&mut self, steps: &[Step], indent: &str) -> Result<()> {
        for (index, step) in steps.iter().enumerate() {
            let name = format!("{indent}{} {}", index + 1, show(step));
            println!("drive: step {name}");
            // a failure inside a branch's body leaves the body's own flag; the branch itself is the top-level checkpoint
            self.failed_checkpoint = is_checkpoint(step);
            let skipped = self.skipped.len();
            xtask::timing::phase(step_phase(step), || self.step(step, indent))
                .with_context(|| format!("step {name}"))?;
            self.failed_checkpoint = false;
            ensure!((self.t.alive)(), "the app ended during step {name}");
            if is_checkpoint(step) {
                self.checkpoint();
                // a checkpoint that skipped (itself, or a branch whose body skipped) judged nothing: it is in `skipped`, not `passed`
                if indent.is_empty() && self.skipped.len() == skipped {
                    self.passed += 1;
                }
            }
        }
        Ok(())
    }

    fn log_path(&self, source: Source) -> &Path {
        match source {
            Source::App => self.t.app_log,
            Source::Services => self.t.services_log,
            Source::Atld => self.t.atld_log,
        }
    }

    fn read(&self, source: Source) -> String {
        atl_diag::logfiles::read(self.log_path(source))
    }

    /// Whether `source` holds a line with `text` since the last checkpoint.
    fn logged(&self, source: Source, text: &str) -> bool {
        since(&self.read(source), self.cursors[source.index()]).contains(text)
    }

    fn checkpoint(&mut self) {
        for s in SOURCES {
            self.cursors[s.index()] = self.read(s).len();
        }
    }

    /// Takes the proxy's events: the geometry is kept; returns whether `serial`'s ack was among them.
    fn pump(&mut self, serial: Option<u32>) -> bool {
        let mut acked = false;
        while let Ok((_, event)) = self.t.proxy.events.try_recv() {
            match event {
                Event::Geometry {
                    x,
                    y,
                    width,
                    height,
                } => {
                    self.geometry = Some(Geometry {
                        x,
                        y,
                        width,
                        height,
                    });
                }
                Event::Acked { serial: s } => acked |= Some(s) == serial,
                _ => {}
            }
        }
        acked
    }

    fn geometry(&mut self) -> Result<Geometry> {
        self.pump(None);
        self.geometry
            .context("the window declared no geometry (xdg_surface.set_window_geometry)")
    }

    /// The live content size in content pixels: the content rect (surface units) times the window's scale.
    fn content_px(&mut self) -> Result<(f64, f64)> {
        let (_, _, w, h) = content_rect(self.geometry()?, self.origin);
        Ok((f64::from(w) * self.scale, f64::from(h) * self.scale))
    }

    /// A script point as a surface point (scaled from `size`, then [`window::content_to_surface`]); a point outside the content is a script error (it would land on the chrome, or nowhere).
    fn surface_point(&mut self, p: (f64, f64)) -> Result<(f64, f64)> {
        let (w, h) = self.content_px()?;
        let (x, y) = scale_point(p, self.size, (w, h));
        ensure!(
            x >= 0.0 && y >= 0.0 && x < w && y < h,
            "({}, {}) is outside the {w}x{h} content",
            p.0,
            p.1
        );
        window::content_to_surface(&self.origin_log, (x, y)).map_err(anyhow::Error::msg)
    }

    /// The content's picture now (output pixels: the nested output has scale 1, so one per surface unit).
    fn content(&mut self) -> Result<Image> {
        let (x, y, w, h) = content_rect(self.geometry()?, self.origin);
        let u = |v: i32| u32::try_from(v).context("the content rectangle is off the output");
        screencopy::capture(self.t.nested)?.crop(u(x)?, u(y)?, u(w)?, u(h)?)
    }

    /// `image` (a content picture) cut to the script region `r`, scaled from `size` to the live content and to output pixels; the region must lie inside.
    fn cut(&mut self, image: &Image, r: Option<Region>) -> Result<Image> {
        let Some(r) = r else {
            return Ok(image.clone());
        };
        let content = self.content_px()?;
        let (x0, y0) = scale_point((r.x, r.y), self.size, content);
        let (x1, y1) = scale_point((r.x + r.w, r.y + r.h), self.size, content);
        let px = |v: f64| (v / self.scale).round().max(0.0) as u32;
        let (x, y) = (px(x0), px(y0));
        let (w, h) = (px(x1).saturating_sub(x), px(y1).saturating_sub(y));
        image
            .crop(x, y, w, h)
            .with_context(|| format!("region {r:?} is not inside the content"))
    }

    fn region_now(&mut self, r: Option<Region>) -> Result<Image> {
        let image = self.content()?;
        self.cut(&image, r)
    }

    fn press(&self, pressed: bool) -> Result<()> {
        self.t.proxy.inject_pointer(PointerAction::Button {
            button: BTN_LEFT,
            pressed,
        })
    }

    /// Polls `done` every `poll` until it holds; the app's end or `limit` fails.
    fn wait_until(
        &mut self,
        what: &str,
        limit: Duration,
        poll: Duration,
        mut done: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        let deadline = Instant::now() + limit;
        while !done(self)? {
            ensure!((self.t.alive)(), "the app ended waiting for {what}");
            ensure!(Instant::now() < deadline, "no {what} within {limit:?}");
            std::thread::sleep(poll);
        }
        Ok(())
    }

    /// Frames presented so far (a lower bound: run_apk::presented_total), so a frame count may lag up to the fps line's second.
    fn presents(&self) -> u64 {
        crate::run_apk::presented_total(&atl_diag::logfiles::read(self.t.app_log))
    }

    fn mark(&self, name: &str) -> Result<&(Image, Option<Region>)> {
        self.marks
            .get(name)
            .with_context(|| format!("no mark named {name:?} before this step"))
    }

    /// The mark `name` and now, both cut to `r` (default: the mark's region).
    fn compare(&mut self, name: &str, r: Option<Region>) -> Result<Diff> {
        let (then, marked) = self.mark(name)?.clone();
        let r = r.or(marked);
        let now = self.region_now(r)?;
        let then = self.cut(&then, r)?;
        then.diff(&now, CHANGE_DELTA)
    }

    fn step(&mut self, step: &Step, indent: &str) -> Result<()> {
        match step {
            Step::Wait(ms) => std::thread::sleep(Duration::from_millis(*ms)),
            Step::WaitFrames(n) => {
                let want = self.presents() + *n as u64;
                self.wait_until("frame", FRAME_WAIT, POLL, |d| Ok(d.presents() >= want))?;
            }
            Step::Tap(x, y) => {
                let (x, y) = self.surface_point((*x, *y))?;
                self.t.proxy.inject_pointer(PointerAction::Enter { x, y })?;
                self.press(true)?;
                self.press(false)?;
            }
            Step::Touch(x, y, ms) => {
                let (x, y) = self.surface_point((*x, *y))?;
                self.t
                    .proxy
                    .inject_touch(TouchAction::Down { id: 0, x, y })?;
                std::thread::sleep(Duration::from_millis(*ms));
                self.t.proxy.inject_touch(TouchAction::Up { id: 0 })?;
            }
            Step::TouchSwipe { from, to, ms } => self.touch_swipe(*from, *to, *ms)?,
            Step::Leave => self.t.proxy.inject_pointer(PointerAction::Leave)?,
            Step::Swipe { from, to, ms } => self.swipe(*from, *to, *ms)?,
            Step::Key(code) => {
                self.t.proxy.inject_key(*code, true)?;
                self.t.proxy.inject_key(*code, false)?;
            }
            Step::KeyHalf(code, down) => self.t.proxy.inject_key(*code, *down)?,
            Step::Resize(w, h) => self.resize(*w, *h)?,
            Step::Shot(file) => self.shot(file)?,
            Step::WaitLog { source, text, ms } => {
                let what = format!("{source:?} log line {text:?}");
                self.wait_until(&what, Duration::from_millis(*ms), POLL, |d| {
                    Ok(d.logged(*source, text))
                })?;
            }
            Step::WaitChanged { region, ms } => {
                let start = self.region_now(Some(*region))?;
                let mut last = None;
                let result =
                    self.wait_until("change", Duration::from_millis(*ms), VISUAL_POLL, |d| {
                        let diff = start.diff(&d.region_now(Some(*region))?, CHANGE_DELTA)?;
                        last = Some(diff);
                        Ok(is_changed(&diff))
                    });
                if let Some(diff) = last {
                    println!("drive: wait-changed: {}", show_diff(&diff));
                }
                result?;
            }
            Step::WaitStable { region, ms, hold } => {
                let hold = Duration::from_millis(*hold);
                let mut prev = self.region_now(Some(*region))?;
                let mut still_since = Instant::now();
                self.wait_until(
                    "stable picture",
                    Duration::from_millis(*ms),
                    VISUAL_POLL,
                    |d| {
                        let now = d.region_now(Some(*region))?;
                        if !is_same(&prev.diff(&now, CHANGE_DELTA)?) {
                            still_since = Instant::now();
                        }
                        prev = now;
                        Ok(still_since.elapsed() >= hold)
                    },
                )?;
            }
            Step::Mark { name, region } => {
                let image = self.content()?;
                // a region outside the content fails here, not at the expect
                self.cut(&image, *region)?;
                self.marks.insert(name.clone(), (image, *region));
            }
            Step::ExpectChanged { name, region } => {
                let diff = self.compare(name, *region)?;
                println!("drive: expect-changed {name}: {}", show_diff(&diff));
                ensure!(
                    is_changed(&diff),
                    "not changed since mark {name:?}: {} (changed needs more than {:.3} % apart by more than {CHANGE_DELTA})",
                    show_diff(&diff),
                    CHANGED_FRACTION * 100.0
                );
            }
            Step::ExpectSame { name, region } => {
                let diff = self.compare(name, *region)?;
                println!("drive: expect-same {name}: {}", show_diff(&diff));
                ensure!(
                    is_same(&diff),
                    "moved since mark {name:?}: {}",
                    show_diff(&diff)
                );
            }
            Step::ExpectVaried(region) => {
                let image = self.region_now(*region)?;
                ensure!(
                    !image.is_blank(),
                    "the {}x{} region is one colour",
                    image.width,
                    image.height
                );
            }
            Step::ExpectLog {
                source,
                text,
                present,
            } => {
                let found = self.logged(*source, text);
                ensure!(
                    found == *present,
                    "{source:?} log {} {text:?} since the last checkpoint",
                    if *present {
                        "has no line with"
                    } else {
                        "has a line with"
                    }
                );
            }
            Step::ExpectFrames { n, ms } => {
                let want = self.presents() + n;
                self.wait_until(
                    &format!("{n} frames"),
                    Duration::from_millis(*ms),
                    POLL,
                    |d| Ok(d.presents() >= want),
                )?;
            }
            Step::ExpectFps { floor, tier, ms } => self.fps(*floor, tier, *ms)?,
            Step::ExpectAudio(ms) => {
                self.wait_until(
                    "audible audio-mix line",
                    Duration::from_millis(*ms),
                    POLL,
                    |d| {
                        let log = d.read(Source::App);
                        Ok(since(&log, d.cursors[Source::App.index()])
                            .lines()
                            .filter_map(audible)
                            .any(|n| n > 0))
                    },
                )?;
            }
            Step::ExpectCounter { key, op, n } => {
                let logs = self.logs()?;
                let got = counter(&logs, key).unwrap_or(0);
                println!("drive: counter {key:?} = {got}");
                ensure!(
                    op.holds(got, *n),
                    "counter {key:?} is {got}, wanted {op:?} {n}"
                );
            }
            Step::If { cond, body } => {
                let holds = match cond {
                    Cond::Log { source, text, ms } => {
                        let deadline = Instant::now() + Duration::from_millis(*ms);
                        let mut found = self.logged(*source, text);
                        while !found && Instant::now() < deadline && (self.t.alive)() {
                            std::thread::sleep(POLL);
                            found = self.logged(*source, text);
                        }
                        println!(
                            "drive: if-log: {}",
                            if found {
                                format!("{text:?} came")
                            } else {
                                format!("no {text:?} within {ms} ms")
                            }
                        );
                        found
                    }
                    Cond::Same { name, region } => {
                        let diff = self.compare(name, *region)?;
                        println!("drive: if-same {name}: {}", show_diff(&diff));
                        is_same(&diff)
                    }
                };
                if holds {
                    println!("drive: branch: its {} steps run", body.len());
                    self.checkpoint();
                    self.steps(body, &format!("{indent}  "))?;
                } else {
                    println!("drive: branch: skipped");
                    self.skipped.push(match cond {
                        Cond::Log { text, .. } => format!("if-log {text:?}: not taken"),
                        Cond::Same { name, .. } => format!("if-same {name}: not taken"),
                    });
                }
            }
        }
        Ok(())
    }

    /// Every process's log in the run's log dir, as atld's summary reads them.
    fn logs(&self) -> Result<Vec<String>> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(self.t.log_dir)
            .with_context(|| format!("reading {}", self.t.log_dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .collect();
        paths.sort();
        Ok(paths.iter().map(|p| atl_diag::logfiles::read(p)).collect())
    }

    /// `expect-fps`: judged only when the app's buffer tier is `tier` (a probed fact of the run), else printed as skipped.
    fn fps(&mut self, floor: f64, tier: &str, ms: u64) -> Result<()> {
        let app = self.read(Source::App);
        let got = crate::run_apk::buffer_tier(&app);
        if got != Some(tier) {
            let got = got.unwrap_or("not logged");
            println!("drive: expect-fps skipped: buffer tier {got}, the floor is for {tier}");
            self.skipped
                .push(format!("expect-fps: tier {got} != {tier}"));
            return Ok(());
        }
        self.fps_judged += 1;
        let first = self.presents();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            ensure!((self.t.alive)(), "the app ended measuring fps");
            std::thread::sleep(POLL);
        }
        let fps = (self.presents() - first) as f64 / start.elapsed().as_secs_f64();
        println!("drive: fps {fps:.1} over {ms} ms (tier {tier}, floor {floor})");
        ensure!(
            fps >= floor,
            "fps {fps:.1} below the floor {floor} (tier {tier})"
        );
        Ok(())
    }

    fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let (x1, y1) = self.surface_point(from)?;
        let (x2, y2) = self.surface_point(to)?;
        let steps = (ms / SWIPE_STEP.as_millis() as u64).max(1);
        self.t
            .proxy
            .inject_pointer(PointerAction::Enter { x: x1, y: y1 })?;
        self.press(true)?;
        for i in 1..=steps {
            let f = i as f64 / steps as f64;
            std::thread::sleep(SWIPE_STEP);
            self.t.proxy.inject_pointer(PointerAction::Motion {
                x: x1 + (x2 - x1) * f,
                y: y1 + (y2 - y1) * f,
            })?;
        }
        self.press(false)
    }

    /// [`Driver::swipe`] with one finger: down, a move each [`SWIPE_STEP`], up at `to`.
    fn touch_swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let (x1, y1) = self.surface_point(from)?;
        let (x2, y2) = self.surface_point(to)?;
        let steps = (ms / SWIPE_STEP.as_millis() as u64).max(1);
        self.t.proxy.inject_touch(TouchAction::Down {
            id: 0,
            x: x1,
            y: y1,
        })?;
        for i in 1..=steps {
            let f = i as f64 / steps as f64;
            std::thread::sleep(SWIPE_STEP);
            self.t.proxy.inject_touch(TouchAction::Motion {
                id: 0,
                x: x1 + (x2 - x1) * f,
                y: y1 + (y2 - y1) * f,
            })?;
        }
        self.t.proxy.inject_touch(TouchAction::Up { id: 0 })
    }

    /// Configures the window so its content is `w` x `h`, then waits for the ack and the geometry of that size.
    fn resize(&mut self, w: i32, h: i32) -> Result<()> {
        let geometry = self.geometry()?;
        let (_, _, cw, ch) = content_rect(geometry, self.origin);
        let want = (w + geometry.width - cw, h + geometry.height - ch);
        let serial = self.serial;
        self.serial += 1;
        self.t.proxy.inject_configure(want.0, want.1, serial, &[])?;
        let mut acked = false;
        self.wait_until("configure ack", FRAME_WAIT, POLL, |d| {
            acked |= d.pump(Some(serial));
            Ok(acked)
        })?;
        self.wait_until("window at the new size", FRAME_WAIT, POLL, |d| {
            d.pump(None);
            Ok(d.geometry.is_some_and(|g| (g.width, g.height) == want))
        })
    }

    fn shot(&mut self, file: &Path) -> Result<()> {
        let mut image = self.content()?;
        if self.t.break_blank {
            image.rgba.fill(0);
        }
        ensure!(
            !image.is_blank(),
            "blank capture: all {}x{} pixels are one colour",
            image.width,
            image.height
        );
        let path = match &self.t.shots {
            Shots::AsNamed => Some(file.to_path_buf()),
            Shots::Memory => None,
            Shots::Into(dir) => Some(dir.join(file.file_name().context("shot names no file")?)),
        };
        if let Some(path) = &path {
            image.write_png(path)?;
        }
        println!(
            "drive: shot {} {}x{}",
            path.as_deref().unwrap_or(file).display(),
            image.width,
            image.height
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(script: &str) -> Vec<Step> {
        parse(script).unwrap().steps
    }

    #[test]
    fn every_step_parses() {
        let script = "# comment\n\nsize 1440 900\nwait 10\nwait-frames 2\ntap 1 2.5\ntouch 3 4 80\ntouch-swipe 1 2 3 4 100\nleave\nswipe 1 2 3 4 100\nkey a\nkey 0\nkey enter\nresize 640 400\nshot out/a.png\n\
wait-log app \"Scene loaded: (Main)\" 60000\nwait-changed 0 64 1440 296 5000\nwait-stable 0 0 10 10 9000 1500\nmark m\nmark d 0 64 1440 296\nexpect-changed d\nexpect-same m 1 2 3 4\nexpect-varied\nexpect-varied 1 2 3 4\n\
expect-log services resumed\nexpect-no-log app \"diag: refused\"\nexpect-frames 5 2000\nexpect-fps 20 tier=readback over 5000\nexpect-audio 3000\nexpect-counter \"refused open\" <= 2\nif-log app \"FIRST THINGS\" 1000\ntouch 720 748 150\nend\nif-same m 1 2 3 4\ntouch 1 2 150\nend\n";
        // KEY_A = 30, KEY_0 = 11, KEY_ENTER = 28 (linux/input-event-codes.h)
        let display = Region {
            x: 0.0,
            y: 64.0,
            w: 1440.0,
            h: 296.0,
        };
        let small = Region {
            x: 1.0,
            y: 2.0,
            w: 3.0,
            h: 4.0,
        };
        let got = parse(script).unwrap();
        assert_eq!(got.size, Some((1440.0, 900.0)));
        assert_eq!(
            got.steps,
            vec![
                Step::Wait(10),
                Step::WaitFrames(2),
                Step::Tap(1.0, 2.5),
                Step::Touch(3.0, 4.0, 80),
                Step::TouchSwipe {
                    from: (1.0, 2.0),
                    to: (3.0, 4.0),
                    ms: 100
                },
                Step::Leave,
                Step::Swipe {
                    from: (1.0, 2.0),
                    to: (3.0, 4.0),
                    ms: 100
                },
                Step::Key(30),
                Step::Key(11),
                Step::Key(28),
                Step::Resize(640, 400),
                Step::Shot(PathBuf::from("out/a.png")),
                Step::WaitLog {
                    source: Source::App,
                    text: "Scene loaded: (Main)".into(),
                    ms: 60000
                },
                Step::WaitChanged {
                    region: display,
                    ms: 5000
                },
                Step::WaitStable {
                    region: Region {
                        x: 0.0,
                        y: 0.0,
                        w: 10.0,
                        h: 10.0
                    },
                    ms: 9000,
                    hold: 1500
                },
                Step::Mark {
                    name: "m".into(),
                    region: None
                },
                Step::Mark {
                    name: "d".into(),
                    region: Some(display)
                },
                Step::ExpectChanged {
                    name: "d".into(),
                    region: None
                },
                Step::ExpectSame {
                    name: "m".into(),
                    region: Some(small)
                },
                Step::ExpectVaried(None),
                Step::ExpectVaried(Some(small)),
                Step::ExpectLog {
                    source: Source::Services,
                    text: "resumed".into(),
                    present: true
                },
                Step::ExpectLog {
                    source: Source::App,
                    text: "diag: refused".into(),
                    present: false
                },
                Step::ExpectFrames { n: 5, ms: 2000 },
                Step::ExpectFps {
                    floor: 20.0,
                    tier: "readback".into(),
                    ms: 5000
                },
                Step::ExpectAudio(3000),
                Step::ExpectCounter {
                    key: "refused open".into(),
                    op: Op::Le,
                    n: 2
                },
                Step::If {
                    cond: Cond::Log {
                        source: Source::App,
                        text: "FIRST THINGS".into(),
                        ms: 1000
                    },
                    body: vec![Step::Touch(720.0, 748.0, 150)]
                },
                Step::If {
                    cond: Cond::Same {
                        name: "m".into(),
                        region: Some(small)
                    },
                    body: vec![Step::Touch(1.0, 2.0, 150)]
                },
            ]
        );
    }

    #[test]
    fn letter_rows_follow_the_header() {
        // KEY_P = 25, KEY_L = 38, KEY_M = 50
        assert_eq!(
            [key_code("p"), key_code("l"), key_code("m")],
            [Some(25), Some(38), Some(50)]
        );
        assert_eq!(key_code("f13"), None);
    }

    #[test]
    fn bad_lines_are_named() {
        for bad in [
            "tap 1",
            "touch 1 2",
            "wait x",
            "fly 1",
            "key nosuch",
            "resize 0 1",
            "size 10 10",
            "wait-log nowhere x 10",
            "wait-log app \"open 10",
            "wait-changed 0 0 10 10",
            "wait-stable 0 0 10 10 100 100",
            "mark m 1 2 3",
            "expect-changed m 0 0 0 5",
            "expect-fps 20 readback over 100",
            "expect-counter k ~ 1",
            "end",
        ] {
            let err = format!("{:#}", parse(&format!("wait 1\n{bad}\nend\n")).unwrap_err());
            assert!(err.contains("script line 2"), "{bad}: {err}");
        }
        let err = format!(
            "{:#}",
            parse("wait 1\nif-log app x 10\ntap 1 1\n").unwrap_err()
        );
        assert!(
            err.contains("script line 2") && err.contains("no `end`"),
            "{err}"
        );
        let err = format!(
            "{:#}",
            parse("wait 1\nif-log app x 10\nif-same m\nend\n").unwrap_err()
        );
        assert!(
            err.contains("script line 3") && err.contains("one level"),
            "{err}"
        );
    }

    #[test]
    fn size_is_a_header() {
        assert_eq!(
            parse("size 10 20\ntap 1 1\n").unwrap().size,
            Some((10.0, 20.0))
        );
        assert!(parse("size 10 20\nsize 10 20\n").is_err());
        assert_eq!(steps("tap 1 1\n").len(), 1);
    }

    /// GTK's shape: a 12 px shadow around the geometry, a 37 px header bar inside it.
    #[test]
    fn content_is_the_geometry_below_the_header() {
        let geometry = Geometry {
            x: 12,
            y: 12,
            width: 800,
            height: 637,
        };
        assert_eq!(content_rect(geometry, (12, 49)), (0, 37, 800, 600));
    }

    /// Hand-computed: a point read at 1440x900 on a 720x450 live content halves on both axes; at 562x900 -> 1124x900 only x doubles; no `size` keeps the point.
    #[test]
    fn points_scale_per_axis() {
        assert_eq!(
            scale_point((720.0, 748.0), Some((1440.0, 900.0)), (720.0, 450.0)),
            (360.0, 374.0)
        );
        assert_eq!(
            scale_point((280.0, 625.0), Some((562.0, 900.0)), (1124.0, 900.0)),
            (560.0, 625.0)
        );
        assert_eq!(scale_point((3.0, 4.0), None, (99.0, 99.0)), (3.0, 4.0));
    }

    /// The origin line's mapping, as [`run`] reads the scale: origin (14, 51) at scale 2 puts content pixel (100, 40) at surface (14 + 50, 51 + 20).
    #[test]
    fn surface_mapping_reads_the_origin_line() {
        let log = "1.000 INFO atl-platform-ui: content origin x=14 y=51 scale=2\n";
        assert_eq!(
            window::content_to_surface(log, (100.0, 40.0)).unwrap(),
            (64.0, 71.0)
        );
        let one = window::content_to_surface(log, (1.0, 0.0)).unwrap();
        assert_eq!(1.0 / (one.0 - 14.0), 2.0);
    }

    fn image(w: u32, h: u32, rgb: [u8; 3]) -> Image {
        let mut rgba = Vec::new();
        for _ in 0..w * h {
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        Image {
            width: w,
            height: h,
            rgba,
        }
    }

    /// Set `n` pixels of `image` (from the first) to a colour `by` above the grey 100.
    fn touch_pixels(image: &mut Image, n: usize, by: u8) {
        for p in image.rgba.chunks_mut(4).take(n) {
            p[1] = 100 + by;
        }
    }

    /// 100x100 = 10000 px: the fraction threshold 0.15 % is 15 px. 16 px past the delta = changed; 15 = neither changed nor same; deltas at the threshold count as unchanged.
    #[test]
    fn change_thresholds_on_synthetic_images() {
        let base = image(100, 100, [100, 100, 100]);
        let mut over = base.clone();
        touch_pixels(&mut over, 16, CHANGE_DELTA + 1);
        let d = base.diff(&over, CHANGE_DELTA).unwrap();
        assert_eq!(
            (d.changed, d.total, d.max_delta),
            (16, 10_000, CHANGE_DELTA + 1)
        );
        assert!(is_changed(&d) && !is_same(&d));
        let mut at = base.clone();
        touch_pixels(&mut at, 15, CHANGE_DELTA + 1);
        let d = base.diff(&at, CHANGE_DELTA).unwrap();
        assert!(!is_changed(&d) && !is_same(&d));
        let mut faint = base.clone();
        touch_pixels(&mut faint, 10_000, CHANGE_DELTA);
        let d = base.diff(&faint, CHANGE_DELTA).unwrap();
        assert!(is_same(&d) && !is_changed(&d));
        assert!(base.diff(&image(10, 10, [0, 0, 0]), CHANGE_DELTA).is_err());
    }

    #[test]
    fn audible_reads_the_mix_line() {
        let line = "1.0 I atl-audioflinger: audio-mix: requested=960 mixed=960 from_tracks=1 audible=480 peak=0.2500 active=1 muted=0 dropped=0";
        assert_eq!(audible(line), Some(480));
        assert_eq!(audible("audio-mix: requested=1 audible=0"), Some(0));
        assert_eq!(audible("nothing"), None);
    }

    #[test]
    fn since_reads_from_the_cursor() {
        assert_eq!(since("abcdef", 2), "cdef");
        assert_eq!(since("abc", 10), "abc");
    }
}
