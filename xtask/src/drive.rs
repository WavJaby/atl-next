// SPDX-License-Identifier: MIT
//! `xtask run-apk --drive <script>`: an agent's hands and eyes on a real APK, headless. A script runs against the app's window once run-apk's own wait is over (resumed, settled, a frame presented, the watch done).
//! Input goes through the window proxy (wayland_proxy.rs), shots through the compositor (screencopy.rs). The compositor holds the window at output (0, 0) ([`NestedCompositor::start_placed`]), so content pixels map to the output by the window geometry the client declared.
//! Script, one step a line; blank lines and `#` comments are skipped. Coordinates are window content pixels (the output has scale 1).
//!   wait <ms> | wait-frames <n> | tap <x> <y> | swipe <x1> <y1> <x2> <y2> <ms> | key <name> | resize <w> <h> | shot <file.png>
//! `wait-frames` counts the app's presents from that step on and fails if they do not come: an idle app presents nothing, so use it only after something that redraws. A present is logged before the compositor shows it: a shot straight after may be one frame old.
//! `shot` paths are relative to the current directory; the image is the window's content (a dialog reaching past it is cut), and a one-colour image is an error, never a pass.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::nested_wayland::NestedCompositor;
use crate::screencopy;
use crate::wayland_proxy::{Event, PointerAction, Proxy};
use crate::window;

const BTN_LEFT: u32 = 0x110;
/// One softpipe frame of a large window takes seconds under load, so a frame wait bounds a hang, not a pace.
const FRAME_WAIT: Duration = Duration::from_secs(120);
/// A swipe's motion events come at about the display's 60 Hz.
const SWIPE_STEP: Duration = Duration::from_millis(16);
const POLL: Duration = Duration::from_millis(20);
/// Injected configure serials: far above what the compositor sends one window in a run.
const FIRST_SERIAL: u32 = 0x10_0000;
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

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Wait(u64),
    WaitFrames(usize),
    Tap(f64, f64),
    Swipe {
        from: (f64, f64),
        to: (f64, f64),
        ms: u64,
    },
    Key(u32),
    Resize(i32, i32),
    Shot(PathBuf),
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

/// The script's steps; an error names the line.
pub fn parse(script: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for (index, line) in script.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        steps.push(parse_step(line).with_context(|| format!("script line {}", index + 1))?);
    }
    Ok(steps)
}

fn num<T: std::str::FromStr>(word: &str) -> Result<T> {
    word.parse()
        .map_err(|_| anyhow::anyhow!("{word:?} is not a valid number"))
}

fn parse_step(line: &str) -> Result<Step> {
    let words: Vec<&str> = line.split_whitespace().collect();
    Ok(match words.as_slice() {
        ["wait", ms] => Step::Wait(num(ms)?),
        ["wait-frames", n] => Step::WaitFrames(num(n)?),
        ["tap", x, y] => Step::Tap(num(x)?, num(y)?),
        ["swipe", x1, y1, x2, y2, ms] => Step::Swipe {
            from: (num(x1)?, num(y1)?),
            to: (num(x2)?, num(y2)?),
            ms: num(ms)?,
        },
        ["key", name] => {
            Step::Key(key_code(name).with_context(|| format!("no key named {name:?}"))?)
        }
        ["resize", w, h] => {
            let (w, h) = (num(w)?, num(h)?);
            ensure!(w > 0 && h > 0, "resize needs a positive size");
            Step::Resize(w, h)
        }
        ["shot", file] => Step::Shot(PathBuf::from(file)),
        _ => bail!("not a step: {line:?}"),
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

pub struct Target<'a> {
    pub proxy: &'a Proxy,
    pub nested: &'a NestedCompositor,
    pub app_log: &'a Path,
    /// The line of a presented frame in the app log.
    pub present_line: &'a str,
    /// False once the app died: a wait ends with it.
    pub alive: &'a dyn Fn() -> bool,
    /// Deliberate failure: each shot is blanked before its check, which must then fail the run.
    pub break_blank: bool,
}

struct Driver<'a> {
    t: Target<'a>,
    /// The content's origin in the surface, from the platform's log line.
    origin: (i32, i32),
    geometry: Option<Geometry>,
    serial: u32,
}

/// Runs `steps`, stopping at the first failure.
pub fn run(steps: &[Step], target: Target<'_>) -> Result<()> {
    let log = std::fs::read_to_string(target.app_log).unwrap_or_default();
    let (x, y) = window::content_origin(&log).map_err(anyhow::Error::msg)?;
    let mut d = Driver {
        t: target,
        origin: (x.round() as i32, y.round() as i32),
        geometry: None,
        serial: FIRST_SERIAL,
    };
    for (index, step) in steps.iter().enumerate() {
        println!("drive: step {}: {step:?}", index + 1);
        d.step(step)
            .with_context(|| format!("step {} {step:?}", index + 1))?;
        ensure!(
            (d.t.alive)(),
            "the app ended during step {} {step:?}",
            index + 1
        );
    }
    Ok(())
}

impl Driver<'_> {
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

    /// A content point as a surface point; a point outside the content is a script error (it would land on the chrome, or nowhere).
    fn surface_point(&mut self, (x, y): (f64, f64)) -> Result<(f64, f64)> {
        let (_, _, w, h) = content_rect(self.geometry()?, self.origin);
        ensure!(
            x >= 0.0 && y >= 0.0 && x < f64::from(w) && y < f64::from(h),
            "({x}, {y}) is outside the {w}x{h} content"
        );
        Ok((x + f64::from(self.origin.0), y + f64::from(self.origin.1)))
    }

    fn press(&self, pressed: bool) -> Result<()> {
        self.t.proxy.inject_pointer(PointerAction::Button {
            button: BTN_LEFT,
            pressed,
        })
    }

    /// Polls `done` until it holds; the app's end or [`FRAME_WAIT`] fails.
    fn wait_until(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) -> Result<()> {
        let deadline = Instant::now() + FRAME_WAIT;
        while !done(self) {
            ensure!((self.t.alive)(), "the app ended waiting for {what}");
            ensure!(Instant::now() < deadline, "no {what} within {FRAME_WAIT:?}");
            std::thread::sleep(POLL);
        }
        Ok(())
    }

    fn presents(&self) -> usize {
        std::fs::read_to_string(self.t.app_log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(self.t.present_line))
            .count()
    }

    fn step(&mut self, step: &Step) -> Result<()> {
        match step {
            Step::Wait(ms) => std::thread::sleep(Duration::from_millis(*ms)),
            Step::WaitFrames(n) => {
                let want = self.presents() + n;
                self.wait_until("frame", |d| d.presents() >= want)?;
            }
            Step::Tap(x, y) => {
                let (x, y) = self.surface_point((*x, *y))?;
                self.t.proxy.inject_pointer(PointerAction::Enter { x, y })?;
                self.press(true)?;
                self.press(false)?;
            }
            Step::Swipe { from, to, ms } => self.swipe(*from, *to, *ms)?,
            Step::Key(code) => {
                self.t.proxy.inject_key(*code, true)?;
                self.t.proxy.inject_key(*code, false)?;
            }
            Step::Resize(w, h) => self.resize(*w, *h)?,
            Step::Shot(file) => self.shot(file)?,
        }
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

    /// Configures the window so its content is `w` x `h`, then waits for the ack and the geometry of that size.
    fn resize(&mut self, w: i32, h: i32) -> Result<()> {
        let geometry = self.geometry()?;
        let (_, _, cw, ch) = content_rect(geometry, self.origin);
        let want = (w + geometry.width - cw, h + geometry.height - ch);
        let serial = self.serial;
        self.serial += 1;
        self.t.proxy.inject_configure(want.0, want.1, serial, &[])?;
        let mut acked = false;
        self.wait_until("configure ack", |d| {
            acked |= d.pump(Some(serial));
            acked
        })?;
        self.wait_until("window at the new size", |d| {
            d.pump(None);
            d.geometry.is_some_and(|g| (g.width, g.height) == want)
        })
    }

    fn shot(&mut self, file: &Path) -> Result<()> {
        let (x, y, w, h) = content_rect(self.geometry()?, self.origin);
        let u = |v: i32| u32::try_from(v).context("the content rectangle is off the output");
        let mut image = screencopy::capture(self.t.nested)?.crop(u(x)?, u(y)?, u(w)?, u(h)?)?;
        if self.t.break_blank {
            image.rgba.fill(0);
        }
        ensure!(
            !image.is_blank(),
            "blank capture: all {w}x{h} pixels are one colour"
        );
        image.write_png(file)?;
        println!("drive: shot {} {w}x{h}", file.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_parses() {
        let script = "# comment\n\nwait 10\nwait-frames 2\ntap 1 2.5\nswipe 1 2 3 4 100\nkey a\nkey 0\nkey enter\nresize 640 400\nshot out/a.png\n";
        // KEY_A = 30, KEY_0 = 11, KEY_ENTER = 28 (linux/input-event-codes.h)
        assert_eq!(
            parse(script).unwrap(),
            vec![
                Step::Wait(10),
                Step::WaitFrames(2),
                Step::Tap(1.0, 2.5),
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
        for bad in ["tap 1", "wait x", "fly 1", "key nosuch", "resize 0 1"] {
            let err = format!("{:#}", parse(&format!("wait 1\n{bad}\n")).unwrap_err());
            assert!(err.contains("script line 2"), "{err}");
        }
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
}
