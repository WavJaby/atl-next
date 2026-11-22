// SPDX-License-Identifier: MIT
//! Bounded capture of an app process's stdout + stderr (liblog's stderr sink among them) into its `log/<name>.log`. An app's output is not ours to trust: a warning stack logged in a loop
//! (Doodle Jump's BillingClient) wrote 9.5 GB in 90 s and filled the disk. Mirrors systemd-journald (journald.conf(5)), the Linux convention (maintainer decision 3):
//! - `RateLimitIntervalSec=` / `RateLimitBurst=`: at most `burst` lines per `interval` per process; the rest of the window is dropped, and when the window ends one line records it,
//!   journald's `Suppressed N messages from <unit>`;
//! - `SystemMaxUse=` / `SystemMaxFileSize=`: a per-process byte budget, split into a head (the start of the output: verdicts read start and resume lines) and a tail ring of segments, rotated and
//!   vacuumed oldest-first as journald does, so a long run keeps its last minutes (a report of a bug seen after 80 min of play must find them). File layout: atl_diag::logfiles.
//!
//! Every drop is counted in the log itself (`atld-log: ...`) and reported by the caller, so a flood never reads as silence: a rate window's note, one line when the head is full, one line at the start of
//! each new tail segment counting what the retired segment held (and the running total), and the totals at EOF.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atl_diag::logfiles::tail_segment;

/// journald.conf(5)'s `RateLimitIntervalSec=` default.
pub const RATE_INTERVAL_MS_DEFAULT: u64 = 30_000;
/// Lines per window, 10x journald.conf(5)'s `RateLimitBurst=` default (10000, sized for a service): Temple Run's start logged 17,940 lines in its first 30 s and lost 7940 at 10000; 100000 is >5x
/// that peak, and a loop like Doodle Jump's (~474k lines/s) still loses >99 %.
pub const RATE_BURST_DEFAULT: u64 = 100_000;
/// Per-process byte budget default [est]: 256 MiB, the bound on a sustained flood (the burst at ~190 B/line passes ~630 KB/s, so the budget is spent after ~7 min of flood) and on long lines.
/// journald's own `SystemMaxFileSize=` default is 1/8 of `SystemMaxUse=` (<= 4 GiB): far above what a per-run log dir on a dev disk can spend per process.
pub const MAX_BYTES_DEFAULT: u64 = 256 << 20;
/// The head is 1/HEAD_SHARE of the budget: 64 MiB at the default. A start costs a few MB (Temple Run: 17,940 lines ~ 3.4 MB in 30 s); Angry Birds 2 played at ~3.2 MiB/min (916k lines, 256 MiB in
/// ~80 min), so the head holds its first ~20 min and the tail the last ~60.
pub const HEAD_SHARE: u64 = 4;
/// Tail segments: a rotation retires 1/4 of the tail, so the tail always keeps 3/4 to all of its share (journald vacuums whole files the same way).
pub const TAIL_SEGMENTS: usize = 4;

/// The tag of the lines this capture writes into a log.
pub const TAG: &str = "atld-log:";

/// How a byte budget is spent: the head, then `segments` tail segments of `segment` bytes each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub head: u64,
    pub segment: u64,
    pub segments: usize,
}

impl Size {
    /// The split of `budget` bytes: [`HEAD_SHARE`], the rest over [`TAIL_SEGMENTS`].
    pub fn of_budget(budget: u64) -> Self {
        let head = budget / HEAD_SHARE;
        Size {
            head,
            segment: (budget - head) / TAIL_SEGMENTS as u64,
            segments: TAIL_SEGMENTS,
        }
    }
}

/// What a capture keeps. `None` disables that bound (the gate's break).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// `(interval, burst)`: at most `burst` lines per `interval`.
    pub rate: Option<(Duration, u64)>,
    /// Head + tail ring; `None`: the head grows without bound.
    pub size: Option<Size>,
}

/// What a capture dropped (app lines only: the capture's own `atld-log:` lines are not counted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub lines: u64,
    pub bytes: u64,
}

impl std::ops::AddAssign for Dropped {
    fn add_assign(&mut self, o: Self) {
        self.lines += o.lines;
        self.bytes += o.bytes;
    }
}

/// Where a capture writes: the head and the tail ring.
pub trait Sink {
    /// Bytes already in the head, and in the newest tail segment (`None`: no tail yet). A restarted process of the same name appends where the last one stopped.
    fn lens(&self) -> io::Result<(u64, Option<u64>)>;
    fn head(&mut self, b: &[u8]) -> io::Result<()>;
    /// Appends to the newest tail segment; only after a [`Sink::rotate`] or when `lens` reported one.
    fn tail(&mut self, b: &[u8]) -> io::Result<()>;
    /// Starts a new newest tail segment. When `keep` segments exist the oldest is deleted first; returns the app lines it held.
    fn rotate(&mut self, keep: usize) -> io::Result<Dropped>;
}

/// The app lines of retired output: lines not written by this capture.
pub fn app_lines(bytes: &[u8]) -> Dropped {
    let mut d = Dropped::default();
    for l in bytes.split_inclusive(|b| *b == b'\n') {
        if !l.starts_with(TAG.as_bytes()) {
            d.lines += 1;
            d.bytes += l.len() as u64;
        }
    }
    d
}

/// The files of `log/<name>.log` (atl_diag::logfiles).
pub struct Files {
    path: PathBuf,
    head: File,
    tail: Option<File>,
}

impl Files {
    pub fn open(path: &Path) -> io::Result<Self> {
        let append = |p: &Path| OpenOptions::new().create(true).append(true).open(p);
        let first = tail_segment(path, 1);
        Ok(Files {
            path: path.to_owned(),
            head: append(path)?,
            tail: if first.exists() {
                Some(append(&first)?)
            } else {
                None
            },
        })
    }
}

impl Sink for Files {
    fn lens(&self) -> io::Result<(u64, Option<u64>)> {
        let tail = match &self.tail {
            Some(f) => Some(f.metadata()?.len()),
            None => None,
        };
        Ok((self.head.metadata()?.len(), tail))
    }
    fn head(&mut self, b: &[u8]) -> io::Result<()> {
        self.head.write_all(b)
    }
    fn tail(&mut self, b: &[u8]) -> io::Result<()> {
        match &mut self.tail {
            Some(f) => f.write_all(b),
            None => Err(io::Error::other("no tail segment")),
        }
    }
    fn rotate(&mut self, keep: usize) -> io::Result<Dropped> {
        let mut retired = Dropped::default();
        let oldest = tail_segment(&self.path, keep);
        if oldest.exists() {
            retired = app_lines(&std::fs::read(&oldest)?);
            std::fs::remove_file(&oldest)?;
        }
        for n in (1..keep).rev() {
            let p = tail_segment(&self.path, n);
            if p.exists() {
                std::fs::rename(&p, tail_segment(&self.path, n + 1))?;
            }
        }
        self.tail = Some(File::create(tail_segment(&self.path, 1))?);
        Ok(retired)
    }
}

/// The write side of one capture: head, then the tail ring.
struct Writer<'a, S: Sink> {
    sink: &'a mut S,
    size: Option<Size>,
    head: u64,
    tail: Option<u64>,
    retired: Dropped,
}

impl<S: Sink> Writer<'_, S> {
    /// A capture line, wherever output currently goes; not bounded (a few per drop).
    fn note(&mut self, msg: &str) -> io::Result<()> {
        match &mut self.tail {
            Some(t) => {
                *t += msg.len() as u64;
                self.sink.tail(msg.as_bytes())
            }
            None => {
                self.head += msg.len() as u64;
                self.sink.head(msg.as_bytes())
            }
        }
    }

    /// One app line; `false` when it was dropped (longer than a whole segment).
    fn line(&mut self, line: &[u8]) -> io::Result<bool> {
        let len = line.len() as u64;
        let Some(size) = self.size else {
            self.head += len;
            self.sink.head(line)?;
            return Ok(true);
        };
        if self.tail.is_none() {
            if self.head + len <= size.head {
                self.head += len;
                self.sink.head(line)?;
                return Ok(true);
            }
            self.note(&format!(
                "{TAG} head full at {} bytes: output continues in the tail ring (<log>.1 newest .. .{} oldest, {} bytes each)\n",
                self.head, size.segments, size.segment
            ))?;
            self.retired += self.sink.rotate(size.segments)?;
            self.tail = Some(0);
        }
        if len > size.segment {
            return Ok(false);
        }
        if self.tail.is_some_and(|t| t + len > size.segment) {
            let r = self.sink.rotate(size.segments)?;
            self.retired += r;
            self.tail = Some(0);
            if r.lines > 0 {
                self.note(&format!(
                    "{TAG} tail ring retired its oldest segment: {} lines ({} bytes); {} lines ({} bytes) retired in total\n",
                    r.lines, r.bytes, self.retired.lines, self.retired.bytes
                ))?;
            }
        }
        if let Some(t) = &mut self.tail {
            *t += len;
        }
        self.sink.tail(line)?;
        Ok(true)
    }
}

/// Copies `input` (the process's pipe) line by line into `sink` under `limits` until EOF; `now` is the clock (a fake one in tests). Returns what was dropped (rate limit, overlong lines, retired
/// segments). Write errors end the writing (a full disk must not spin); the pipe is drained so the process never blocks on it.
pub fn copy_bounded(
    mut input: impl BufRead,
    sink: &mut impl Sink,
    limits: Limits,
    mut now: impl FnMut() -> Instant,
) -> io::Result<Dropped> {
    let (head, tail) = sink.lens()?;
    let mut w = Writer {
        sink,
        size: limits.size,
        head,
        tail,
        retired: Dropped::default(),
    };
    let mut total = Dropped::default();
    let mut window = Dropped::default();
    let mut window_start = now();
    let mut window_lines = 0u64;
    let mut out_ok = true;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let len = line.len() as u64;
        if let Some((interval, burst)) = limits.rate {
            let t = now();
            if t.duration_since(window_start) >= interval {
                if window.lines > 0 && out_ok {
                    out_ok = w
                        .note(&format!(
                            "{TAG} suppressed {} lines ({} bytes): rate limit {burst} lines per {} ms\n",
                            window.lines,
                            window.bytes,
                            interval.as_millis()
                        ))
                        .is_ok();
                }
                window = Dropped::default();
                window_start = t;
                window_lines = 0;
            }
            if window_lines >= burst {
                window.lines += 1;
                window.bytes += len;
                total.lines += 1;
                total.bytes += len;
                continue;
            }
            window_lines += 1;
        }
        if out_ok {
            match w.line(&line) {
                Ok(true) => {}
                Ok(false) => {
                    total.lines += 1;
                    total.bytes += len;
                }
                Err(_) => out_ok = false,
            }
        }
    }
    if out_ok && total.lines > 0 {
        let _ = w.note(&format!(
            "{TAG} suppressed {} lines ({} bytes) in total\n",
            total.lines, total.bytes
        ));
    }
    if out_ok && w.retired.lines > 0 {
        let _ = w.note(&format!(
            "{TAG} tail ring retired {} lines ({} bytes) in total\n",
            w.retired.lines, w.retired.bytes
        ));
    }
    total += w.retired;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Cursor;

    /// An in-memory [`Sink`]: `tail[0]` newest, as `<log>.1`.
    #[derive(Default)]
    struct Mem {
        head: Vec<u8>,
        tail: Vec<Vec<u8>>,
    }

    impl Sink for Mem {
        fn lens(&self) -> io::Result<(u64, Option<u64>)> {
            Ok((
                self.head.len() as u64,
                self.tail.first().map(|t| t.len() as u64),
            ))
        }
        fn head(&mut self, b: &[u8]) -> io::Result<()> {
            self.head.extend_from_slice(b);
            Ok(())
        }
        fn tail(&mut self, b: &[u8]) -> io::Result<()> {
            self.tail[0].extend_from_slice(b);
            Ok(())
        }
        fn rotate(&mut self, keep: usize) -> io::Result<Dropped> {
            let r = if self.tail.len() >= keep {
                app_lines(&self.tail.pop().expect("oldest"))
            } else {
                Dropped::default()
            };
            self.tail.insert(0, Vec::new());
            Ok(r)
        }
    }

    impl Mem {
        fn text(&self) -> String {
            let mut all = self.head.clone();
            for t in self.tail.iter().rev() {
                all.extend_from_slice(t);
            }
            String::from_utf8(all).expect("utf8")
        }
    }

    /// A fake child's flood: `n` lines of `len` bytes each (newline included).
    fn flood(n: usize, len: usize) -> Vec<u8> {
        let mut line = vec![b'w'; len - 1];
        line.push(b'\n');
        line.repeat(n)
    }

    /// `n` numbered lines `w<i>` padded to `LINE` bytes, so a kept line names its place.
    fn numbered(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("w{i:0>width$}\n", width = LINE - 2).into_bytes())
            .collect()
    }

    /// A clock advancing `step` per read.
    fn ticking(step: Duration) -> impl FnMut() -> Instant {
        let t0 = Instant::now();
        let k = Cell::new(0u32);
        move || {
            k.set(k.get() + 1);
            t0 + step * k.get()
        }
    }

    const LINE: usize = 100;

    /// 100k lines in 1 s (the Doodle loop's shape, 10 us/line): the burst of the one window passes, the rest is dropped and counted; the file stays near burst x line.
    #[test]
    fn rate_limit_holds_and_records_the_drop() {
        let lines = 100_000;
        let burst = 1_000;
        let limits = Limits {
            rate: Some((Duration::from_secs(30), burst)),
            size: None,
        };
        let mut out = Mem::default();
        let d = copy_bounded(
            Cursor::new(flood(lines, LINE)),
            &mut out,
            limits,
            ticking(Duration::from_micros(10)),
        )
        .expect("copy");
        // independent count: everything past the burst
        assert_eq!(
            d,
            Dropped {
                lines: lines as u64 - burst,
                bytes: (lines as u64 - burst) * LINE as u64
            }
        );
        let text = out.text();
        assert_eq!(
            text.lines().filter(|l| l.starts_with('w')).count() as u64,
            burst
        );
        assert!(
            text.ends_with(&format!(
                "{TAG} suppressed 99000 lines (9900000 bytes) in total\n"
            )),
            "{}",
            &text[text.len() - 200..]
        );
    }

    /// A window end writes the window's suppressed line, journald's `Suppressed N messages`.
    #[test]
    fn a_window_end_writes_its_suppressed_line() {
        // 30 lines, 1 s apart, burst 5 per 10 s
        let limits = Limits {
            rate: Some((Duration::from_secs(10), 5)),
            size: None,
        };
        let mut out = Mem::default();
        let d = copy_bounded(
            Cursor::new(flood(30, LINE)),
            &mut out,
            limits,
            ticking(Duration::from_secs(1)),
        )
        .expect("copy");
        let text = out.text();
        let notes: Vec<&str> = text.lines().filter(|l| l.contains("rate limit")).collect();
        // the window opens at t=1 s (the first clock read); line i reads t=i+2 s, and a window ends at the first read 10 s after its start: lines 0-8 (9), 9-18 (10), 19-28 (10), 29 (1, open at EOF, no note): drops 4, 5, 5, 0
        assert_eq!(notes.len(), 3, "{text}");
        assert!(
            notes[0].starts_with(&format!("{TAG} suppressed 4 lines (400 bytes)")),
            "{}",
            notes[0]
        );
        assert!(
            notes[1].starts_with(&format!("{TAG} suppressed 5 lines (500 bytes)")),
            "{}",
            notes[1]
        );
        assert_eq!(d.lines, 4 + 5 + 5);
    }

    /// 10 MB offered to a 1 MiB budget: the head keeps the first lines, the tail ring the last ones, the retired middle is counted line-exact; break: no size -> the whole flood lands in the head.
    #[test]
    fn head_and_tail_ring_hold_and_count_the_middle() {
        let lines = 100_000;
        let budget = 1 << 20;
        let size = Size::of_budget(budget);
        // the split itself, from the constants: 1/4 head, the rest over 4 segments
        assert_eq!(size.head, 262_144);
        assert_eq!(size.segment, 196_608);
        let mut out = Mem::default();
        let d = copy_bounded(
            Cursor::new(numbered(lines)),
            &mut out,
            Limits {
                rate: None,
                size: Some(size),
            },
            Instant::now,
        )
        .expect("copy");
        let text = out.text();
        let kept: Vec<&str> = text.lines().filter(|l| l.starts_with('w')).collect();
        let index = |l: &str| l[1..].parse::<usize>().expect("index");
        // head intact: whole lines that fit in it, from line 0 on
        let head_lines = size.head as usize / LINE;
        assert!(
            kept[..head_lines]
                .iter()
                .enumerate()
                .all(|(i, l)| index(l) == i)
        );
        // last line present
        assert_eq!(index(kept[kept.len() - 1]), lines - 1);
        // counts: every line is kept once or retired, the retired total equals the missing ones
        let missing = (lines - kept.len()) as u64;
        assert_eq!(
            d,
            Dropped {
                lines: missing,
                bytes: missing * LINE as u64
            }
        );
        assert!(text.contains(&format!("{TAG} head full at")));
        assert!(text.ends_with(&format!(
            "{TAG} tail ring retired {missing} lines ({} bytes) in total\n",
            missing * LINE as u64
        )));
        // the per-rotation lines' running total ends at the same count
        let last_rotation = text
            .lines()
            .rfind(|l| l.contains("retired its oldest segment"))
            .expect("a rotation line");
        assert!(
            last_rotation.ends_with(&format!(
                "{missing} lines ({} bytes) retired in total",
                missing * LINE as u64
            )),
            "{last_rotation}"
        );
        // bounded: head + segments, plus the capture's own lines (head full, one per live segment, EOF total: < 200 bytes each)
        let bytes: usize = out.head.len() + out.tail.iter().map(Vec::len).sum::<usize>();
        assert!(
            (bytes as u64) < budget + (size.segments as u64 + 2) * 200,
            "{bytes}"
        );
        assert_eq!(out.tail.len(), size.segments);

        // break: no size -> the head takes everything, nothing recorded
        let mut out = Mem::default();
        let d = copy_bounded(
            Cursor::new(flood(lines, LINE)),
            &mut out,
            Limits {
                rate: None,
                size: None,
            },
            Instant::now,
        )
        .expect("copy");
        assert_eq!(d, Dropped::default());
        assert_eq!(out.head.len(), lines * LINE);
        assert!(out.tail.is_empty());
    }

    /// The file sink rotates `.1` (newest) .. `.k` and atl_diag::logfiles::read gives the whole log in order.
    #[test]
    fn files_rotate_and_read_back_in_order() {
        let dir = std::env::temp_dir().join(format!("atld-applog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("p.log");
        let mut files = Files::open(&path).expect("open");
        let size = Size::of_budget(64 * LINE as u64);
        copy_bounded(
            Cursor::new(numbered(1000)),
            &mut files,
            Limits {
                rate: None,
                size: Some(size),
            },
            Instant::now,
        )
        .expect("copy");
        assert!(tail_segment(&path, size.segments).exists());
        assert!(!tail_segment(&path, size.segments + 1).exists());
        let text = atl_diag::logfiles::read(&path);
        let kept: Vec<usize> = text
            .lines()
            .filter(|l| l.starts_with('w'))
            .map(|l| l[1..].parse().expect("index"))
            .collect();
        assert_eq!(kept[0], 0);
        assert_eq!(kept[kept.len() - 1], 999);
        assert!(kept.windows(2).all(|w| w[0] < w[1]), "out of order");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line with no newline at EOF is kept.
    #[test]
    fn last_partial_line_is_kept() {
        let mut out = Mem::default();
        copy_bounded(
            Cursor::new(b"a\nb".to_vec()),
            &mut out,
            Limits {
                rate: None,
                size: None,
            },
            Instant::now,
        )
        .expect("copy");
        assert_eq!(out.head, b"a\nb");
    }
}
