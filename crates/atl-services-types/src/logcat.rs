// SPDX-License-Identifier: MIT
//! The C6 log record of an Android log message, its one-line text form, and logd's write socket: the app launcher writes its own liblog's and the native-bridge guest's records with it, atld a
//! daemon's kernel log lines (servicemanager's), so atl-diag reads one line shape. Never filters: liblog applied `__android_log_is_loggable` before sending (`logger_write.cpp`
//! `__android_log_buf_write`), so every record handed over is written.

use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, sockopt};

/// `android_LogPriority` values (android/log.h); atl-app-launcher's test checks them against its bindgen of the header.
pub mod priority {
    pub const VERBOSE: i32 = 2;
    pub const DEBUG: i32 = 3;
    pub const INFO: i32 = 4;
    pub const WARN: i32 = 5;
    pub const ERROR: i32 = 6;
    pub const FATAL: i32 = 7;
    pub const SILENT: i32 = 8;
}

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
        match p {
            priority::VERBOSE => Level::Verbose,
            priority::DEBUG => Level::Debug,
            priority::INFO => Level::Info,
            priority::WARN => Level::Warn,
            priority::ERROR => Level::Error,
            priority::FATAL => Level::Fatal,
            priority::SILENT => Level::Silent,
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

/// One log record (plan C6): both clocks, the writer's pid and tid, the source, tag, level and text; `location` is libbase's `file:line` where the message has one.
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

/// The monotonic clock now, as [`Record::monotonic`].
pub fn monotonic_now() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}

/// liblog's logd socket path inside a sandbox (logd_writer.cpp `LogdSocket::GetSocket`).
pub const LOGDW: &str = "/dev/socket/logdw";
/// `sizeof(android_log_header_t)` (android_logger.h, packed: id u8, tid u16, realtime u32 s + u32 ns).
const HEADER: usize = 11;
/// `LOGGER_ENTRY_MAX_PAYLOAD` (log_read.h): liblog never sends more after the header.
const MAX_PAYLOAD: usize = 4068;
/// log_id_t (android/log.h) values this receiver tells apart.
const LOG_ID_EVENTS: u8 = 2;
const LOG_ID_STATS: u8 = 5;
const LOG_ID_SECURITY: u8 = 6;
const LOG_ID_KERNEL: u8 = 7;

/// Binds a logd write socket at `path` and serves it on a thread for the process's lifetime: each entry becomes a [`format`] text handed to `sink`.
/// What logd's LogListener::HandleData does, minus the buffers: one datagram per entry, `android_log_header_t` then the payload; sender credentials via SO_PASSCRED, an entry without them dropped
/// as logd drops it. Text buffers' payload is `priority u8, tag NUL, message NUL`; binary buffers (events, stats, security) are recorded as their size, not decoded; LOG_ID_KERNEL is dropped.
pub fn serve_logdw(path: &Path, sink: impl Fn(&str) + Send + 'static) -> Result<()> {
    let sock = UnixDatagram::bind(path).with_context(|| format!("binding {}", path.display()))?;
    sockopt::set_socket_passcred(&sock, true).context("SO_PASSCRED")?;
    std::thread::Builder::new()
        .name("logdw".into())
        .spawn(move || receive(&sock, &sink))
        .context("logdw thread")?;
    Ok(())
}

fn receive(sock: &UnixDatagram, sink: &impl Fn(&str)) {
    let mut buf = vec![0u8; HEADER + MAX_PAYLOAD];
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmCredentials(1))];
    loop {
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let got = rustix::net::recvmsg(
            sock,
            &mut [IoSliceMut::new(&mut buf)],
            &mut control,
            RecvFlags::empty(),
        );
        let Ok(msg) = got else {
            continue;
        };
        let pid = control.drain().find_map(|m| match m {
            RecvAncillaryMessage::ScmCredentials(c) => Some(c.pid.as_raw_nonzero().get()),
            _ => None,
        });
        let (Some(pid), Some(e)) = (pid, parse(&buf[..msg.bytes])) else {
            continue;
        };
        sink(&format(&Record {
            monotonic: monotonic_now(),
            realtime: e.realtime,
            pid: u32::try_from(pid).unwrap_or(0),
            tid: i32::from(e.tid),
            source: LIBLOG,
            tag: &e.tag,
            level: e.level,
            location: None,
            message: &e.message,
        }));
    }
}

/// One decoded entry.
#[derive(Debug, PartialEq)]
struct Entry {
    tid: u16,
    realtime: Duration,
    level: Level,
    tag: String,
    message: String,
}

/// One datagram; None for malformed or kernel entries (LogListener's checks).
fn parse(d: &[u8]) -> Option<Entry> {
    if d.len() <= HEADER || d[0] >= LOG_ID_KERNEL {
        return None;
    }
    let id = d[0];
    let tid = u16::from_le_bytes([d[1], d[2]]);
    let sec = u32::from_le_bytes([d[3], d[4], d[5], d[6]]);
    let nsec = u32::from_le_bytes([d[7], d[8], d[9], d[10]]);
    let payload = &d[HEADER..];
    let (level, tag, message) = if matches!(id, LOG_ID_EVENTS | LOG_ID_STATS | LOG_ID_SECURITY) {
        (
            Level::Info,
            String::new(),
            format!("binary entry of log id {id}, {} bytes", payload.len()),
        )
    } else {
        let mut parts = payload[1..].splitn(3, |&b| b == 0);
        let mut text = || String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
        let tag = text();
        (Level::from_priority(i32::from(payload[0])), tag, text())
    };
    Some(Entry {
        tid,
        realtime: Duration::new(u64::from(sec), nsec),
        level,
        tag,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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

    /// A datagram as logd_writer.cpp builds it: header, then priority + tag NUL + message NUL.
    fn text_entry(id: u8, prio: u8, tag: &str, msg: &str) -> Vec<u8> {
        let mut d = vec![id];
        d.extend_from_slice(&7u16.to_le_bytes());
        d.extend_from_slice(&1_762_000_000u32.to_le_bytes());
        d.extend_from_slice(&5_000_000u32.to_le_bytes());
        assert_eq!(d.len(), HEADER);
        d.push(prio);
        d.extend_from_slice(tag.as_bytes());
        d.push(0);
        d.extend_from_slice(msg.as_bytes());
        d.push(0);
        d
    }

    #[test]
    fn text_entries_decode() {
        // ANDROID_LOG_WARN = 5 (android/log.h), LOG_ID_MAIN = 0
        assert_eq!(
            parse(&text_entry(0, 5, "AtlJniGuest", "guest liblog 42")),
            Some(Entry {
                tid: 7,
                realtime: Duration::new(1_762_000_000, 5_000_000),
                level: Level::Warn,
                tag: "AtlJniGuest".into(),
                message: "guest liblog 42".into(),
            })
        );
    }

    #[test]
    fn kernel_short_and_binary_entries() {
        assert_eq!(parse(&text_entry(LOG_ID_KERNEL, 5, "t", "m")), None);
        assert_eq!(parse(&text_entry(0, 5, "t", "m")[..HEADER]), None);
        // events payload: int32 tag + typed data; 6 bytes here
        let mut ev = text_entry(LOG_ID_EVENTS, 0, "", "")[..HEADER].to_vec();
        ev.extend_from_slice(&[1, 0, 0, 0, 0, 9]);
        let e = parse(&ev).unwrap();
        assert_eq!(e.message, "binary entry of log id 2, 6 bytes");
    }

    #[test]
    fn served_socket_hands_liblog_datagrams_to_the_sink() {
        let dir = std::env::temp_dir().join(format!("atl-logdw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logdw");
        let _ = std::fs::remove_file(&path);
        let (tx, rx) = mpsc::channel::<String>();
        let tx = std::sync::Mutex::new(tx);
        serve_logdw(&path, move |line| {
            let _ = tx.lock().unwrap().send(line.to_owned());
        })
        .unwrap();
        let sender = UnixDatagram::unbound().unwrap();
        let entry = text_entry(0, 5, "t", "m");
        assert_eq!(sender.send_to(&entry, &path).unwrap(), entry.len());
        let line = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(line.starts_with("1762000000.005 "), "{line}");
        assert!(line.ends_with(" W liblog/t: m\n"), "{line}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_socket_dir_is_an_error() {
        assert!(serve_logdw(Path::new("/nonexistent-atl-dir/logdw"), |_| {}).is_err());
    }
}
