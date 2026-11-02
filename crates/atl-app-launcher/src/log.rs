// SPDX-License-Identifier: MIT
//! The C6 log record of a liblog message and its sink: stderr for now (journald later, only through the platform layer).
//! Never filters: liblog applied `__android_log_is_loggable` (per-tag `log.tag.*` levels) before calling the logger (`logger_write.cpp` `__android_log_buf_write`), so every record it hands over is written.

use std::io::Write;
use std::time::Duration;

use crate::generated::log as liblog;

/// Android log priority (`android_LogPriority`), one letter each as logcat prints them (`filterPriToChar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Verbose,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    Silent,
    /// Outside the enum, kept as given.
    Other(i32),
}

impl Level {
    pub fn from_priority(p: i32) -> Self {
        match u32::try_from(p) {
            Ok(liblog::ANDROID_LOG_VERBOSE) => Level::Verbose,
            Ok(liblog::ANDROID_LOG_DEBUG) => Level::Debug,
            Ok(liblog::ANDROID_LOG_INFO) => Level::Info,
            Ok(liblog::ANDROID_LOG_WARN) => Level::Warn,
            Ok(liblog::ANDROID_LOG_ERROR) => Level::Error,
            Ok(liblog::ANDROID_LOG_FATAL) => Level::Fatal,
            Ok(liblog::ANDROID_LOG_SILENT) => Level::Silent,
            _ => Level::Other(p),
        }
    }

    pub fn letter(self) -> char {
        match self {
            Level::Verbose => 'V',
            Level::Debug => 'D',
            Level::Info => 'I',
            Level::Warn => 'W',
            Level::Error => 'E',
            Level::Fatal => 'F',
            Level::Silent => 'S',
            Level::Other(_) => '?',
        }
    }
}

/// One log record (plan C6): both clocks, the writer's pid and tid, the source, tag, level and text; `location` is libbase's `file:line` where the message has one. Package: once the launcher starts APKs.
#[derive(Debug)]
pub struct Record<'a> {
    pub monotonic: Duration,
    pub realtime: Duration,
    pub pid: u32,
    pub tid: i32,
    pub source: &'static str,
    pub tag: &'a str,
    pub level: Level,
    pub location: Option<(&'a str, u32)>,
    pub message: &'a str,
}

/// The source of records from liblog.
pub const LIBLOG: &str = "liblog";

/// One line per message line: `<realtime s.ms> <monotonic s.ms> <pid> <tid> <L> <source>/<tag>: <text>`, `<file>:<line>] ` before the text where known (libbase's form).
pub fn format(r: &Record) -> String {
    let stamp = |d: Duration| format!("{}.{:03}", d.as_secs(), d.subsec_millis());
    let head = format!(
        "{} {} {:5} {:5} {} {}/{}: ",
        stamp(r.realtime),
        stamp(r.monotonic),
        r.pid,
        r.tid,
        r.level.letter(),
        r.source,
        r.tag
    );
    let location = r
        .location
        .map(|(file, line)| format!("{file}:{line}] "))
        .unwrap_or_default();
    let mut out = String::new();
    for line in r.message.split('\n') {
        out.push_str(&head);
        out.push_str(&location);
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Writes `r` to stderr in one write. A failed write is dropped: a logger has nowhere to report it.
pub fn write(r: &Record) {
    let _ = std::io::stderr().lock().write_all(format(r).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(level: Level, message: &str) -> Record<'_> {
        Record {
            monotonic: Duration::from_millis(5_250),
            realtime: Duration::from_millis(1_762_000_000_007),
            pid: 42,
            tid: 43,
            source: LIBLOG,
            tag: "t",
            level,
            location: None,
            message,
        }
    }

    #[test]
    fn levels_from_android_priorities() {
        // android/log.h: VERBOSE 2 .. FATAL 7, SILENT 8
        let letters: String = (0..=9).map(|p| Level::from_priority(p).letter()).collect();
        assert_eq!(letters, "??VDIWEFS?");
    }

    #[test]
    fn every_record_is_written() {
        // no level is dropped here: liblog filtered already
        for p in 0..=9 {
            assert!(!format(&record(Level::from_priority(p), "m")).is_empty());
        }
        assert_eq!(
            format(&record(Level::Verbose, "a\nb")),
            "1762000000.007 5.250    42    43 V liblog/t: a\n1762000000.007 5.250    42    43 V liblog/t: b\n"
        );
        let mut r = record(Level::Fatal, "x");
        r.location = Some(("runtime.cc", 7));
        assert!(format(&r).ends_with(" F liblog/t: runtime.cc:7] x\n"));
    }
}
