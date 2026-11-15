// SPDX-License-Identifier: MIT
//! Bounded capture of an app process's stdout + stderr (liblog's stderr sink among them) into its `log/<name>.log`. An app's output is not ours to trust: a warning stack logged in a loop
//! (Doodle Jump's BillingClient) wrote 9.5 GB in 90 s and filled the disk. Mirrors systemd-journald (journald.conf(5)), the Linux convention (owner decision 3):
//! - `RateLimitIntervalSec=` / `RateLimitBurst=`: at most `burst` lines per `interval` per process; the rest of the window is dropped, and when the window ends one line records it,
//!   journald's `Suppressed N messages from <unit>`;
//! - `SystemMaxFileSize=`: a per-process byte cap. journald then rotates and vacuums the oldest file; here the head is kept and the rest dropped, because a run's verdicts read its early lines
//!   (start, resume) and the rate limit already keeps a flood far below the cap. At the cap one line records it, and EOF writes the totals.
//!
//! Every drop is counted in the file itself (`atld-log: suppressed ...`) and reported by the caller, so a flood never reads as silence.

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

/// journald.conf(5)'s `RateLimitIntervalSec=` default.
pub const RATE_INTERVAL_MS_DEFAULT: u64 = 30_000;
/// Lines per window, 10x journald.conf(5)'s `RateLimitBurst=` default (10000, sized for a service): Temple Run's start logged 17,940 lines in its first 30 s and lost 7940 at 10000; 100000 is >5x
/// that peak, and a loop like Doodle Jump's (~474k lines/s) still loses >99 %.
pub const RATE_BURST_DEFAULT: u64 = 100_000;
/// Per-process byte cap default [est]: 256 MiB, the bound on a sustained flood (the burst at ~190 B/line passes ~630 KB/s, so the cap is reached after ~7 min of flood) and on long lines.
/// journald's own `SystemMaxFileSize=` default is 1/8 of `SystemMaxUse=` (<= 4 GiB): far above what a per-run log dir on a dev disk can spend per process.
pub const MAX_BYTES_DEFAULT: u64 = 256 << 20;

/// The tag of the lines this capture writes into a log.
pub const TAG: &str = "atld-log:";

/// What a capture keeps. `None` disables that bound (the gate's break).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// `(interval, burst)`: at most `burst` lines per `interval`.
    pub rate: Option<(Duration, u64)>,
    /// Bytes a log may grow by.
    pub max_bytes: Option<u64>,
}

/// What a capture dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub lines: u64,
    pub bytes: u64,
}

/// Copies `input` (the process's pipe) line by line into `out` under `limits` until EOF; `now` is the clock (a fake one in tests). Returns what was dropped. Write errors end the copy
/// (a full disk must not spin), the pipe is drained so the process never blocks on it.
pub fn copy_bounded(
    mut input: impl BufRead,
    out: &mut impl Write,
    limits: Limits,
    mut now: impl FnMut() -> Instant,
) -> io::Result<Dropped> {
    let mut total = Dropped::default();
    let mut window = Dropped::default();
    let mut window_start = now();
    let mut window_lines = 0u64;
    let mut written = 0u64;
    let mut capped = false;
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
                if window.lines > 0 && out_ok && !capped {
                    out_ok = emit(
                        out,
                        &mut written,
                        &note(window, "rate limit", interval, burst),
                    )
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
        if let Some(max) = limits.max_bytes
            && written + len > max
        {
            if !capped && out_ok {
                let msg = format!(
                    "{TAG} size cap {max} bytes reached: the rest of this process's output is dropped\n"
                );
                out_ok = out.write_all(msg.as_bytes()).is_ok();
            }
            capped = true;
            total.lines += 1;
            total.bytes += len;
            continue;
        }
        if out_ok {
            out_ok = emit(out, &mut written, &line).is_ok();
        }
    }
    if out_ok && (total.lines > 0) {
        let msg = format!(
            "{TAG} suppressed {} lines ({} bytes) in total\n",
            total.lines, total.bytes
        );
        let _ = out.write_all(msg.as_bytes());
    }
    Ok(total)
}

fn emit(out: &mut impl Write, written: &mut u64, bytes: &[u8]) -> io::Result<()> {
    out.write_all(bytes)?;
    *written += bytes.len() as u64;
    Ok(())
}

fn note(d: Dropped, why: &str, interval: Duration, burst: u64) -> Vec<u8> {
    format!(
        "{TAG} suppressed {} lines ({} bytes): {why} {burst} lines per {} ms\n",
        d.lines,
        d.bytes,
        interval.as_millis()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Cursor;

    /// A fake child's flood: `n` lines of `len` bytes each (newline included).
    fn flood(n: usize, len: usize) -> Vec<u8> {
        let mut line = vec![b'w'; len - 1];
        line.push(b'\n');
        line.repeat(n)
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
            max_bytes: None,
        };
        let mut out = Vec::new();
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
        let text = String::from_utf8(out).expect("utf8");
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
            max_bytes: None,
        };
        let mut out = Vec::new();
        let d = copy_bounded(
            Cursor::new(flood(30, LINE)),
            &mut out,
            limits,
            ticking(Duration::from_secs(1)),
        )
        .expect("copy");
        let text = String::from_utf8(out).expect("utf8");
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

    /// The size cap holds with no rate limit: 10 MB offered, 1 MB cap -> the file stays under cap + two record lines; break: no cap -> the whole flood lands.
    #[test]
    fn size_cap_holds_and_records_the_drop() {
        let lines = 100_000;
        let cap = 1 << 20;
        let mut out = Vec::new();
        let d = copy_bounded(
            Cursor::new(flood(lines, LINE)),
            &mut out,
            Limits {
                rate: None,
                max_bytes: Some(cap),
            },
            Instant::now,
        )
        .expect("copy");
        let kept = cap / LINE as u64; // whole lines that fit
        assert_eq!(d.lines, lines as u64 - kept);
        assert!((out.len() as u64) < cap + 200, "{}", out.len());
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(&format!("{TAG} size cap {cap} bytes reached")));
        assert!(text.ends_with(&format!(
            "{TAG} suppressed {} lines ({} bytes) in total\n",
            d.lines, d.bytes
        )));

        // break: both bounds off -> the file grows past the cap, nothing recorded
        let mut out = Vec::new();
        let d = copy_bounded(
            Cursor::new(flood(lines, LINE)),
            &mut out,
            Limits {
                rate: None,
                max_bytes: None,
            },
            Instant::now,
        )
        .expect("copy");
        assert_eq!(d, Dropped::default());
        assert_eq!(out.len(), lines * LINE);
        assert!(out.len() as u64 > cap);
    }

    /// A line with no newline at EOF is kept.
    #[test]
    fn last_partial_line_is_kept() {
        let mut out = Vec::new();
        copy_bounded(
            Cursor::new(b"a\nb".to_vec()),
            &mut out,
            Limits {
                rate: None,
                max_bytes: None,
            },
            Instant::now,
        )
        .expect("copy");
        assert_eq!(out, b"a\nb");
    }
}
