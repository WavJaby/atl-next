// SPDX-License-Identifier: MIT
//! logd's write socket for this process: `/dev/socket/logdw`, where a liblog without our logger sends (`__android_log_logd_logger`, the `__ANDROID__` default, logger_write.cpp; logd_writer.cpp connects that path).
//! Such a liblog is the native-bridge guest's: AOSP liblog built for the guest (NBS `NATIVE_BRIDGE_ORIG_GUEST_LIBS`), whose logger we cannot set from the host. Its records go to the same sink as the host liblog's ([`log::write`]), so one app log holds both.
//! What logd's LogListener::HandleData does, minus the buffers: one datagram per entry, `android_log_header_t` (packed: id u8, tid u16, realtime u32 s + u32 ns) then the payload; sender credentials via SO_PASSCRED, an entry without them dropped as logd drops it.
//! Text buffers' payload is `priority u8, tag NUL, message NUL` (`__android_log_logd_logger_with_timestamp`'s three iovecs); binary buffers (events, stats, security) are recorded as their size, not decoded. LOG_ID_KERNEL is dropped, as logd drops it.
//! The broker makes `/dev/socket` (lib.rs SOCKET_DIR); a launcher started without it says so once and runs without the socket: guest log lines are then lost, as on Android without logd.

use std::io::IoSliceMut;
use std::mem::MaybeUninit;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, sockopt};

use crate::log::{self, Level, Record};

/// liblog's socket path (logd_writer.cpp `LogdSocket::GetSocket`).
pub const PATH: &str = "/dev/socket/logdw";
/// `sizeof(android_log_header_t)` (android_logger.h, packed).
const HEADER: usize = 11;
/// `LOGGER_ENTRY_MAX_PAYLOAD` (log_read.h): liblog never sends more after the header.
const MAX_PAYLOAD: usize = 4068;
/// log_id_t (android/log.h) values this receiver tells apart.
const LOG_ID_EVENTS: u8 = 2;
const LOG_ID_STATS: u8 = 5;
const LOG_ID_SECURITY: u8 = 6;
const LOG_ID_KERNEL: u8 = 7;

/// Binds [`PATH`] and serves it on a thread for the process's lifetime.
pub fn start() -> Result<()> {
    serve(Path::new(PATH))
}

fn serve(path: &Path) -> Result<()> {
    let sock = UnixDatagram::bind(path).with_context(|| format!("binding {}", path.display()))?;
    sockopt::set_socket_passcred(&sock, true).context("SO_PASSCRED")?;
    std::thread::Builder::new()
        .name("logdw".into())
        .spawn(move || receive(&sock))
        .context("logdw thread")?;
    Ok(())
}

fn receive(sock: &UnixDatagram) {
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
        if let Some(pid) = pid {
            record(&buf[..msg.bytes], pid);
        }
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

fn record(d: &[u8], pid: i32) {
    let Some(e) = parse(d) else {
        return;
    };
    let monotonic = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    log::write(&Record {
        monotonic: Duration::new(
            u64::try_from(monotonic.tv_sec).unwrap_or(0),
            u32::try_from(monotonic.tv_nsec).unwrap_or(0),
        ),
        realtime: e.realtime,
        pid: u32::try_from(pid).unwrap_or(0),
        tid: i32::from(e.tid),
        source: log::LIBLOG,
        tag: &e.tag,
        level: e.level,
        location: None,
        message: &e.message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

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
    fn served_socket_takes_liblog_datagrams() {
        // the receiver responds: a datagram sent to the served path is read without error (output goes to stderr, the sink)
        let dir = std::env::temp_dir().join(format!("atl-logdw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("logdw");
        let _ = std::fs::remove_file(&path);
        serve(&path).unwrap();
        let tx = UnixDatagram::unbound().unwrap();
        let entry = text_entry(0, 5, "t", "m");
        assert_eq!(tx.send_to(&entry, &path).unwrap(), entry.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_socket_dir_is_an_error() {
        assert!(serve(Path::new("/nonexistent-atl-dir/logdw")).is_err());
    }
}
